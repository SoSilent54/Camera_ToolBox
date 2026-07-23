//! 连接端点与文件工作区设置；密码永不进入可序列化模型。

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    AutoOpenActivation, AutoOpenRule, AutoOpenRuleError, AutoOpenRuleId, CompletionRequirement,
    DirectoryRef, FileMatcher, FileSourceId,
};
use crate::{PlatformConfig, ProfileStore};

pub const CONNECTION_SETTINGS_SCHEMA_VERSION: u32 = 1;
pub const WORKSPACE_SETTINGS_SCHEMA_VERSION: u32 = 1;
const CONNECTION_FILE_NAME: &str = "connections.json";
const WORKSPACE_FILE_NAME: &str = "workspace-settings.json";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemoteConnectionId(String);

impl RemoteConnectionId {
    /// # Errors
    ///
    /// 空白或控制字符标识会被拒绝。
    pub fn new(value: impl Into<String>) -> Result<Self, WorkspaceSettingsError> {
        let value = value.into();
        if value.trim().is_empty() || value.chars().any(char::is_control) {
            return Err(WorkspaceSettingsError::InvalidConnectionId);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteAuthentication {
    /// 只持久化非秘密 slot ID；密码由当前进程会话按该 ID 注入。
    Password {
        slot_id: String,
    },
    PrivateKeyFile {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConnectionConfig {
    pub id: RemoteConnectionId,
    pub display_name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// 仅用于读取旧版配置；导入时清空，且永不重新序列化。
    #[serde(default, skip_serializing)]
    pub expected_host_key: Option<String>,
    pub authentication: RemoteAuthentication,
}

impl RemoteConnectionConfig {
    /// # Errors
    ///
    /// 端点、用户名或认证配置无效时返回错误。
    pub fn validate(&self) -> Result<(), WorkspaceSettingsError> {
        if self.display_name.trim().is_empty() {
            return Err(WorkspaceSettingsError::InvalidDisplayName);
        }
        if self.host.trim().is_empty()
            || self.host.contains(char::is_whitespace)
            || self.host.contains(['/', '\\', '\0'])
        {
            return Err(WorkspaceSettingsError::InvalidHost);
        }
        if self.port == 0 {
            return Err(WorkspaceSettingsError::InvalidPort);
        }
        if self.username.trim().is_empty() || self.username.contains('\0') {
            return Err(WorkspaceSettingsError::InvalidUsername);
        }
        match &self.authentication {
            RemoteAuthentication::Password { slot_id }
                if slot_id.trim().is_empty()
                    || slot_id.contains(['/', '\\', '\0'])
                    || slot_id.chars().any(char::is_control) =>
            {
                return Err(WorkspaceSettingsError::InvalidPasswordSlot);
            }
            RemoteAuthentication::PrivateKeyFile { path } if path.as_os_str().is_empty() => {
                return Err(WorkspaceSettingsError::InvalidPrivateKeyPath);
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConnectionSettings {
    connections: BTreeMap<RemoteConnectionId, RemoteConnectionConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectionSettingsDocument {
    schema_version: u32,
    #[serde(default)]
    connections: Vec<RemoteConnectionConfig>,
}

impl ConnectionSettings {
    #[must_use]
    pub fn iter(&self) -> impl Iterator<Item = &RemoteConnectionConfig> {
        self.connections.values()
    }

    #[must_use]
    pub fn get(&self, id: &RemoteConnectionId) -> Option<&RemoteConnectionConfig> {
        self.connections.get(id)
    }

    /// # Errors
    ///
    /// 配置无效或标识冲突时返回错误。
    pub fn insert(
        &mut self,
        mut connection: RemoteConnectionConfig,
    ) -> Result<(), WorkspaceSettingsError> {
        connection.expected_host_key = None;
        connection.validate()?;
        if self.connections.contains_key(&connection.id) {
            return Err(WorkspaceSettingsError::DuplicateConnection(
                connection.id.as_str().to_owned(),
            ));
        }
        self.connections.insert(connection.id.clone(), connection);
        Ok(())
    }

    pub fn upsert(
        &mut self,
        mut connection: RemoteConnectionConfig,
    ) -> Result<(), WorkspaceSettingsError> {
        connection.expected_host_key = None;
        connection.validate()?;
        self.connections.insert(connection.id.clone(), connection);
        Ok(())
    }

    #[must_use]
    pub fn export_json(&self) -> Result<Vec<u8>, WorkspaceSettingsError> {
        let document = ConnectionSettingsDocument {
            schema_version: CONNECTION_SETTINGS_SCHEMA_VERSION,
            connections: self.connections.values().cloned().collect(),
        };
        json_bytes(&document)
    }

    pub fn import_json(bytes: &[u8]) -> Result<Self, WorkspaceSettingsError> {
        let document: ConnectionSettingsDocument = serde_json::from_slice(bytes)?;
        if document.schema_version != CONNECTION_SETTINGS_SCHEMA_VERSION {
            return Err(WorkspaceSettingsError::UnsupportedConnectionSchema(
                document.schema_version,
            ));
        }
        let mut settings = Self::default();
        for connection in document.connections {
            settings.insert(connection)?;
        }
        Ok(settings)
    }

    pub fn load_from_path(path: &Path) -> Result<Self, WorkspaceSettingsError> {
        Self::import_json(&fs::read(path)?)
    }

    pub fn save_to_path(&self, path: &Path) -> Result<(), WorkspaceSettingsError> {
        save_bytes(path, &self.export_json()?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MountedSourceConfig {
    Local {
        source_id: FileSourceId,
        root: PathBuf,
    },
    Sftp {
        source_id: FileSourceId,
        connection_id: RemoteConnectionId,
        remote_root: String,
    },
}

impl MountedSourceConfig {
    #[must_use]
    pub const fn source_id(&self) -> &FileSourceId {
        match self {
            Self::Local { source_id, .. } | Self::Sftp { source_id, .. } => source_id,
        }
    }

    /// # Errors
    ///
    /// 本地根为空或远端根不是安全绝对路径时返回错误。
    pub fn validate(&self) -> Result<(), WorkspaceSettingsError> {
        match self {
            Self::Local { root, .. } if root.as_os_str().is_empty() => {
                Err(WorkspaceSettingsError::InvalidLocalRoot)
            }
            Self::Sftp { remote_root, .. }
                if !remote_root.starts_with('/')
                    || remote_root.contains('\0')
                    || remote_root.split('/').any(|component| component == "..") =>
            {
                Err(WorkspaceSettingsError::InvalidRemoteRoot)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceSettings {
    mounts: BTreeMap<FileSourceId, MountedSourceConfig>,
    auto_open_rules: Vec<AutoOpenRule>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceSettingsDocument {
    schema_version: u32,
    #[serde(default)]
    mounts: Vec<MountedSourceConfig>,
    #[serde(default)]
    auto_open_rules: Vec<AutoOpenRule>,
}

impl WorkspaceSettings {
    #[must_use]
    pub fn mounts(&self) -> impl Iterator<Item = &MountedSourceConfig> {
        self.mounts.values()
    }

    #[must_use]
    pub fn auto_open_rules(&self) -> &[AutoOpenRule] {
        &self.auto_open_rules
    }

    pub fn insert_mount(
        &mut self,
        mount: MountedSourceConfig,
    ) -> Result<(), WorkspaceSettingsError> {
        mount.validate()?;
        if self.mounts.contains_key(mount.source_id()) {
            return Err(WorkspaceSettingsError::DuplicateSource(
                mount.source_id().as_str().to_owned(),
            ));
        }
        self.mounts.insert(mount.source_id().clone(), mount);
        Ok(())
    }

    pub fn push_auto_open_rule(
        &mut self,
        rule: AutoOpenRule,
    ) -> Result<(), WorkspaceSettingsError> {
        rule.validate()?;
        if self
            .auto_open_rules
            .iter()
            .any(|existing| existing.id == rule.id)
        {
            return Err(WorkspaceSettingsError::DuplicateAutoOpenRule(
                rule.id.as_str().to_owned(),
            ));
        }
        self.auto_open_rules.push(rule);
        Ok(())
    }

    #[must_use]
    pub fn export_json(&self) -> Result<Vec<u8>, WorkspaceSettingsError> {
        let document = WorkspaceSettingsDocument {
            schema_version: WORKSPACE_SETTINGS_SCHEMA_VERSION,
            mounts: self.mounts.values().cloned().collect(),
            auto_open_rules: self.auto_open_rules.clone(),
        };
        json_bytes(&document)
    }

    pub fn import_json(bytes: &[u8]) -> Result<Self, WorkspaceSettingsError> {
        let document: WorkspaceSettingsDocument = serde_json::from_slice(bytes)?;
        if document.schema_version != WORKSPACE_SETTINGS_SCHEMA_VERSION {
            return Err(WorkspaceSettingsError::UnsupportedWorkspaceSchema(
                document.schema_version,
            ));
        }
        let mut settings = Self::default();
        for mount in document.mounts {
            settings.insert_mount(mount)?;
        }
        for rule in document.auto_open_rules {
            settings.push_auto_open_rule(rule)?;
        }
        Ok(settings)
    }

    pub fn load_from_path(path: &Path) -> Result<Self, WorkspaceSettingsError> {
        Self::import_json(&fs::read(path)?)
    }

    pub fn save_to_path(&self, path: &Path) -> Result<(), WorkspaceSettingsError> {
        save_bytes(path, &self.export_json()?)
    }
}

#[derive(Debug, Clone, Default)]
pub struct LegacyWorkspaceImport {
    pub connections: ConnectionSettings,
    pub workspace: WorkspaceSettings,
}

impl LegacyWorkspaceImport {
    /// 仅迁移 SSH 端点、认证偏好、artifact 根和旧自动打开开关。
    /// 命令、capture recipe、helper、主机密钥与密码均不会进入新模型。
    pub fn from_profile_store(store: &ProfileStore) -> Result<Self, WorkspaceSettingsError> {
        let mut import = Self::default();
        for profile in store.platforms() {
            let PlatformConfig::SshManaged(config) = &profile.config else {
                continue;
            };
            let connection_id = RemoteConnectionId::new(profile.id.as_str())?;
            let authentication = if let Some(path) = config.credential_ref.strip_prefix("key-file:")
            {
                RemoteAuthentication::PrivateKeyFile {
                    path: PathBuf::from(path),
                }
            } else if let Some(slot_id) = config.credential_ref.strip_prefix("session:") {
                RemoteAuthentication::Password {
                    slot_id: slot_id.to_owned(),
                }
            } else {
                return Err(WorkspaceSettingsError::UnsupportedLegacyCredentialReference);
            };
            import.connections.insert(RemoteConnectionConfig {
                id: connection_id.clone(),
                display_name: profile.display_name.clone(),
                host: config.host.clone(),
                port: config.port,
                username: config.username.clone(),
                expected_host_key: None,
                authentication,
            })?;
            let source_id = FileSourceId::new(format!("sftp-{}", profile.id.as_str()))?;
            import.workspace.insert_mount(MountedSourceConfig::Sftp {
                source_id: source_id.clone(),
                connection_id,
                remote_root: config.remote_artifact_dir.clone(),
            })?;
            if config.passive_watch_auto_open {
                let matcher = legacy_file_matcher(&config.remote_artifact_glob)?;
                import.workspace.push_auto_open_rule(AutoOpenRule {
                    id: AutoOpenRuleId::new(format!("legacy-{}", profile.id.as_str()))?,
                    directory: DirectoryRef::root(source_id),
                    matcher,
                    recursive: false,
                    enabled: true,
                    activation: AutoOpenActivation::FollowLatest,
                    minimum_completion: CompletionRequirement::AllowSettledHeuristic,
                    completion_contract: None,
                })?;
            }
        }
        Ok(import)
    }
}

fn legacy_file_matcher(glob: &str) -> Result<FileMatcher, WorkspaceSettingsError> {
    if glob == "*" {
        return Ok(FileMatcher {
            exact_names: Vec::new(),
            suffixes: Vec::new(),
        });
    }
    if let Some(suffix) = glob.strip_prefix('*') {
        if !suffix.contains(['*', '?', '[', ']']) {
            return Ok(FileMatcher {
                exact_names: Vec::new(),
                suffixes: vec![suffix.to_owned()],
            });
        }
    } else if !glob.contains(['*', '?', '[', ']']) {
        return Ok(FileMatcher {
            exact_names: vec![glob.to_owned()],
            suffixes: Vec::new(),
        });
    }
    Err(WorkspaceSettingsError::UnsupportedLegacyArtifactGlob(
        glob.to_owned(),
    ))
}

pub fn project_config_dir() -> Result<PathBuf, WorkspaceSettingsError> {
    ProjectDirs::from("io", "sosilent", "camera-toolbox")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .ok_or(WorkspaceSettingsError::ProjectDirectoryUnavailable)
}

pub fn connection_settings_path() -> Result<PathBuf, WorkspaceSettingsError> {
    Ok(project_config_dir()?.join(CONNECTION_FILE_NAME))
}

pub fn workspace_settings_path() -> Result<PathBuf, WorkspaceSettingsError> {
    Ok(project_config_dir()?.join(WORKSPACE_FILE_NAME))
}

fn json_bytes(document: &impl Serialize) -> Result<Vec<u8>, WorkspaceSettingsError> {
    let mut bytes = serde_json::to_vec_pretty(document)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn save_bytes(path: &Path, bytes: &[u8]) -> Result<(), WorkspaceSettingsError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum WorkspaceSettingsError {
    #[error("invalid remote connection id")]
    InvalidConnectionId,
    #[error("connection display name must not be empty")]
    InvalidDisplayName,
    #[error("invalid SSH host")]
    InvalidHost,
    #[error("SSH port must be non-zero")]
    InvalidPort,
    #[error("SSH username must not be empty")]
    InvalidUsername,
    #[error("private key path must not be empty")]
    InvalidPrivateKeyPath,
    #[error("password slot id must be non-empty and contain no path syntax")]
    InvalidPasswordSlot,
    #[error("local mount root must not be empty")]
    InvalidLocalRoot,
    #[error("SFTP mount root must be an absolute non-escaping path")]
    InvalidRemoteRoot,
    #[error("duplicate connection id: {0}")]
    DuplicateConnection(String),
    #[error("duplicate file source id: {0}")]
    DuplicateSource(String),
    #[error("duplicate auto-open rule id: {0}")]
    DuplicateAutoOpenRule(String),
    #[error("unsupported connections schema version {0}")]
    UnsupportedConnectionSchema(u32),
    #[error("unsupported workspace settings schema version {0}")]
    UnsupportedWorkspaceSchema(u32),
    #[error("unsupported legacy credential reference")]
    UnsupportedLegacyCredentialReference,
    #[error("legacy artifact glob cannot be represented safely: {0}")]
    UnsupportedLegacyArtifactGlob(String),
    #[error("project config directory is unavailable")]
    ProjectDirectoryUnavailable,
    #[error(transparent)]
    Rule(#[from] AutoOpenRuleError),
    #[error(transparent)]
    FileModel(#[from] super::FileModelError),
    #[error("settings I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("settings JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlatformProfile, PlatformProfileId, SshManagedConfig};

    fn ssh_profile() -> PlatformProfile {
        PlatformProfile {
            id: PlatformProfileId::new("camera").unwrap(),
            display_name: "Camera".to_owned(),
            config: PlatformConfig::SshManaged(SshManagedConfig {
                host: "camera.lan".to_owned(),
                port: 22,
                username: "root".to_owned(),
                expected_host_key: "ssh-ed25519 AAAA".to_owned(),
                credential_ref: "session:camera".to_owned(),
                command_subsystem: Some("ctargv".to_owned()),
                capture_recipe: "capture".to_owned(),
                remote_artifact_dir: "/opt/raw".to_owned(),
                remote_artifact_glob: "*.raw".to_owned(),
                remote_event_subsystem: Some("events".to_owned()),
                passive_watch_auto_open: true,
                stable_samples: 3,
                stability_interval_ms: 250,
                max_fetch_bytes: 1024,
                command_output_bytes: 1024,
                rtsp: None,
            }),
        }
    }

    #[test]
    fn connection_json_never_contains_password_or_credential_reference() {
        let mut settings = ConnectionSettings::default();
        settings
            .insert(RemoteConnectionConfig {
                id: RemoteConnectionId::new("camera").unwrap(),
                display_name: "Camera".to_owned(),
                host: "camera.lan".to_owned(),
                port: 22,
                username: "root".to_owned(),
                expected_host_key: None,
                authentication: RemoteAuthentication::Password {
                    slot_id: "camera".to_owned(),
                },
            })
            .unwrap();
        let json = String::from_utf8(settings.export_json().unwrap()).unwrap();
        assert!(!json.contains("credential_ref"));
        assert!(!json.contains("session:"));
        assert!(!json.contains("expected_host_key"));
    }

    #[test]
    fn legacy_connection_host_key_is_discarded_on_import() {
        let json = br#"{
            "schema_version": 1,
            "connections": [{
                "id": "camera",
                "display_name": "Camera",
                "host": "camera.lan",
                "port": 22,
                "username": "root",
                "expected_host_key": "stale-host-key",
                "authentication": {"kind": "password", "slot_id": "camera"}
            }]
        }"#;

        let settings = ConnectionSettings::import_json(json).unwrap();
        let id = RemoteConnectionId::new("camera").unwrap();
        assert_eq!(settings.get(&id).unwrap().expected_host_key, None);

        let exported = String::from_utf8(settings.export_json().unwrap()).unwrap();
        assert!(!exported.contains("expected_host_key"));
        assert!(!exported.contains("stale-host-key"));
    }

    #[test]
    fn legacy_import_omits_command_capture_and_password_reference() {
        let mut store = ProfileStore::new();
        store.insert_platform(ssh_profile()).unwrap();
        let import = LegacyWorkspaceImport::from_profile_store(&store).unwrap();
        let connections = String::from_utf8(import.connections.export_json().unwrap()).unwrap();
        let workspace = String::from_utf8(import.workspace.export_json().unwrap()).unwrap();
        assert!(connections.contains("camera.lan"));
        assert!(connections.contains("\"slot_id\": \"camera\""));
        assert!(!connections.contains("session:camera"));
        assert!(!connections.contains("ctargv"));
        assert!(!connections.contains("capture"));
        assert!(workspace.contains("/opt/raw"));
        assert!(workspace.contains(".raw"));
        assert!(workspace.contains("legacy-camera"));
    }

    #[test]
    fn workspace_round_trip_preserves_mount_root() {
        let mut settings = WorkspaceSettings::default();
        settings
            .insert_mount(MountedSourceConfig::Local {
                source_id: FileSourceId::new("local").unwrap(),
                root: PathBuf::from("/tmp/raw"),
            })
            .unwrap();
        let decoded = WorkspaceSettings::import_json(&settings.export_json().unwrap()).unwrap();
        assert_eq!(decoded.mounts().count(), 1);
    }
}
