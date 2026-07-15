//! 版本化平台配置存储；仅持久化静态配置，不包含 UI、连接或 session 状态。

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    CapabilityResolutionError, CapabilityResolutionKey, DefaultCapabilityResolver, LocalConfig,
    PlatformBindings, PlatformConfig, PlatformProfile, PlatformProfileId, ProfileError,
    SensorDescriptor, SensorId, SensorModeKey, SensorPlatformCapabilityCell,
    SensorPlatformCapabilityMatrix, SensorSelection, TargetResolutionSnapshot,
};

pub const PROFILE_STORE_SCHEMA_VERSION: u32 = 1;
const PROFILE_FILE_NAME: &str = "platform-profiles.json";

#[derive(Debug, Clone, Default)]
pub struct ProfileStore {
    platforms: BTreeMap<PlatformProfileId, PlatformProfile>,
    sensors: BTreeMap<SensorId, SensorDescriptor>,
    matrix: SensorPlatformCapabilityMatrix,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileStoreDocument {
    schema_version: u32,
    #[serde(default)]
    platforms: Vec<PlatformProfile>,
    #[serde(default)]
    sensors: Vec<SensorDescriptor>,
    #[serde(default)]
    capability_bindings: Vec<SensorPlatformCapabilityCell>,
}

#[derive(Debug, Deserialize)]
struct SchemaVersionEnvelope {
    schema_version: u32,
}

impl ProfileStore {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            platforms: BTreeMap::new(),
            sensors: BTreeMap::new(),
            matrix: SensorPlatformCapabilityMatrix::new(),
        }
    }

    /// 首次运行时唯一内建目标；网络目标必须由用户显式配置。
    pub fn with_builtin_local() -> Result<Self, ProfileStoreError> {
        let mut store = Self::new();
        store.insert_platform(PlatformProfile {
            id: PlatformProfileId::new("local")?,
            display_name: "Local files".to_owned(),
            config: PlatformConfig::Local(LocalConfig::default()),
        })?;
        Ok(store)
    }

    #[must_use]
    pub fn platform(&self, id: &PlatformProfileId) -> Option<&PlatformProfile> {
        self.platforms.get(id)
    }

    pub fn platforms(&self) -> impl Iterator<Item = &PlatformProfile> {
        self.platforms.values()
    }

    #[must_use]
    pub fn sensor(&self, id: &SensorId) -> Option<&SensorDescriptor> {
        self.sensors.get(id)
    }

    pub fn sensors(&self) -> impl Iterator<Item = &SensorDescriptor> {
        self.sensors.values()
    }

    #[must_use]
    pub const fn matrix(&self) -> &SensorPlatformCapabilityMatrix {
        &self.matrix
    }

    pub fn insert_platform(&mut self, profile: PlatformProfile) -> Result<(), ProfileStoreError> {
        validate_platform_for_store(&profile)?;
        if self.platforms.contains_key(&profile.id) {
            return Err(ProfileStoreError::DuplicatePlatform(profile.id));
        }
        self.platforms.insert(profile.id.clone(), profile);
        Ok(())
    }

    pub fn replace_platform(&mut self, profile: PlatformProfile) -> Result<(), ProfileStoreError> {
        validate_platform_for_store(&profile)?;
        if !self.platforms.contains_key(&profile.id) {
            return Err(ProfileStoreError::UnknownPlatform(profile.id));
        }
        self.platforms.insert(profile.id.clone(), profile);
        Ok(())
    }

    pub fn remove_platform(
        &mut self,
        id: &PlatformProfileId,
    ) -> Result<PlatformProfile, ProfileStoreError> {
        if self.matrix.iter().any(|cell| cell.platform_id == *id) {
            return Err(ProfileStoreError::PlatformReferencedByCapabilityCell(
                id.clone(),
            ));
        }
        self.platforms
            .remove(id)
            .ok_or_else(|| ProfileStoreError::UnknownPlatform(id.clone()))
    }

    pub fn insert_sensor(&mut self, sensor: SensorDescriptor) -> Result<(), ProfileStoreError> {
        sensor.validate()?;
        if self.sensors.contains_key(&sensor.id) {
            return Err(ProfileStoreError::DuplicateSensor(sensor.id));
        }
        self.sensors.insert(sensor.id.clone(), sensor);
        Ok(())
    }

    pub fn insert_capability_cell(
        &mut self,
        cell: SensorPlatformCapabilityCell,
    ) -> Result<(), ProfileStoreError> {
        self.validate_cell_references(&cell)?;
        let sensor_mode = cell.sensor_mode.clone();
        let platform_id = cell.platform_id.clone();
        self.matrix.insert(cell).map_err(|error| match error {
            CapabilityResolutionError::DuplicateCapabilityCell => {
                ProfileStoreError::DuplicateCapabilityCell {
                    sensor_mode,
                    platform_id,
                }
            }
            other => ProfileStoreError::Capability(other),
        })
    }

    /// 从 store 强制查找精确 sparse cell，调用方无法遗漏已配置限制。
    pub fn resolve_target(
        &self,
        selection: SensorSelection,
        bindings: &PlatformBindings,
    ) -> Result<TargetResolutionSnapshot, ProfileStoreError> {
        let platform_id = bindings.platform_id().clone();
        if !self.platforms.contains_key(&platform_id) {
            return Err(ProfileStoreError::UnknownPlatform(platform_id));
        }
        let key = CapabilityResolutionKey {
            platform_id: bindings.platform_id().clone(),
            sensor: selection.clone(),
        };
        let sensor = match &selection {
            SensorSelection::Unbound => None,
            SensorSelection::Mode(mode_key) => {
                let descriptor = self
                    .sensors
                    .get(&mode_key.sensor_id)
                    .ok_or_else(|| ProfileStoreError::UnknownSensor(mode_key.sensor_id.clone()))?;
                Some(descriptor.mode_snapshot(&mode_key.mode_id)?)
            }
        };
        let cell = match &selection {
            SensorSelection::Unbound => None,
            SensorSelection::Mode(mode_key) => self.matrix.get(mode_key, bindings.platform_id()),
        };
        DefaultCapabilityResolver
            .resolve(&key, bindings, sensor.as_ref(), cell)
            .map_err(ProfileStoreError::Capability)
    }

    /// JSON 输出按主键顺序固定，末尾换行也固定。
    pub fn export_json(&self) -> Result<Vec<u8>, ProfileStoreError> {
        let document = ProfileStoreDocument {
            schema_version: PROFILE_STORE_SCHEMA_VERSION,
            platforms: self.platforms.values().cloned().collect(),
            sensors: self.sensors.values().cloned().collect(),
            capability_bindings: self.matrix.iter().cloned().collect(),
        };
        let mut bytes = serde_json::to_vec_pretty(&document)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn import_json(bytes: &[u8]) -> Result<Self, ProfileStoreError> {
        let envelope: SchemaVersionEnvelope = serde_json::from_slice(bytes)?;
        if envelope.schema_version != PROFILE_STORE_SCHEMA_VERSION {
            return Err(ProfileStoreError::UnsupportedSchemaVersion {
                found: envelope.schema_version,
                supported: PROFILE_STORE_SCHEMA_VERSION,
            });
        }
        let document: ProfileStoreDocument = serde_json::from_slice(bytes)?;
        let mut store = Self::new();
        for profile in document.platforms {
            store.insert_platform(profile)?;
        }
        for sensor in document.sensors {
            store.insert_sensor(sensor)?;
        }
        for cell in document.capability_bindings {
            store.insert_capability_cell(cell)?;
        }
        Ok(store)
    }

    pub fn load_from_path(path: &Path) -> Result<Self, ProfileStoreError> {
        Self::import_json(&fs::read(path)?)
    }

    pub fn save_to_path(&self, path: &Path) -> Result<(), ProfileStoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, self.export_json()?)?;
        Ok(())
    }

    pub fn load_project() -> Result<Self, ProfileStoreError> {
        Self::load_from_path(&Self::project_file_path()?)
    }

    pub fn save_project(&self) -> Result<(), ProfileStoreError> {
        self.save_to_path(&Self::project_file_path()?)
    }

    pub fn project_config_dir() -> Result<PathBuf, ProfileStoreError> {
        ProjectDirs::from("io", "sosilent", "camera-toolbox")
            .map(|dirs| dirs.config_dir().to_path_buf())
            .ok_or(ProfileStoreError::ProjectDirectoryUnavailable)
    }

    pub fn project_file_path() -> Result<PathBuf, ProfileStoreError> {
        Ok(Self::project_config_dir()?.join(PROFILE_FILE_NAME))
    }

    fn validate_cell_references(
        &self,
        cell: &SensorPlatformCapabilityCell,
    ) -> Result<(), ProfileStoreError> {
        if !self.platforms.contains_key(&cell.platform_id) {
            return Err(ProfileStoreError::UnknownPlatform(cell.platform_id.clone()));
        }
        let sensor = self
            .sensors
            .get(&cell.sensor_mode.sensor_id)
            .ok_or_else(|| ProfileStoreError::UnknownSensor(cell.sensor_mode.sensor_id.clone()))?;
        if sensor.mode(&cell.sensor_mode.mode_id).is_none() {
            return Err(ProfileStoreError::UnknownSensorMode(
                cell.sensor_mode.clone(),
            ));
        }
        Ok(())
    }
}

fn validate_platform_for_store(profile: &PlatformProfile) -> Result<(), ProfileStoreError> {
    profile.validate()?;
    if let PlatformConfig::SshManaged(config) = &profile.config {
        validate_persisted_credential_ref(&config.credential_ref)?;
    }
    Ok(())
}

/// 只允许引用语法进入持久化 profile；PEM、密码和任意裸字符串均拒绝。
fn validate_persisted_credential_ref(reference: &str) -> Result<(), ProfileStoreError> {
    if let Some(id) = reference.strip_prefix("session:") {
        if !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Ok(());
        }
    }
    if let Some(path) = reference.strip_prefix("key-file:") {
        let path = Path::new(path);
        if path.is_absolute()
            && !path.as_os_str().is_empty()
            && !reference.contains(['\0', '\r', '\n'])
        {
            return Ok(());
        }
    }
    Err(ProfileStoreError::UnsafeCredentialReference)
}

#[derive(Debug, Error)]
pub enum ProfileStoreError {
    #[error("unsupported profile schema version {found}; this build supports {supported}")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("duplicate platform id: {0}")]
    DuplicatePlatform(PlatformProfileId),
    #[error("platform profile not found: {0}")]
    UnknownPlatform(PlatformProfileId),
    #[error("duplicate sensor id: {0}")]
    DuplicateSensor(SensorId),
    #[error("sensor not found: {0}")]
    UnknownSensor(SensorId),
    #[error("sensor mode not found: {0:?}")]
    UnknownSensorMode(SensorModeKey),
    #[error("duplicate capability cell for {sensor_mode:?} and platform {platform_id}")]
    DuplicateCapabilityCell {
        sensor_mode: SensorModeKey,
        platform_id: PlatformProfileId,
    },
    #[error("platform {0} is referenced by a capability cell")]
    PlatformReferencedByCapabilityCell(PlatformProfileId),
    #[error(
        "credential_ref must be key-file:/absolute/path or session:<opaque-id>; secret material is forbidden"
    )]
    UnsafeCredentialReference,
    #[error("the per-user Camera Toolbox config directory is unavailable")]
    ProjectDirectoryUnavailable,
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Capability(#[from] CapabilityResolutionError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use camera_toolbox_core::{
        BayerPattern, RawSpec,
        sensor::{SensorMode, SensorProfile},
    };

    use super::*;
    use crate::{
        CapabilityKind, CapabilityRestriction, CapabilityVariant, Cv610Config, Cv610DumpConfig,
        Cv610StreamConfig, DumpInitializationPolicy, EvidenceMaturity,
        EvidencedCapabilityDeclaration, SshManagedConfig,
    };

    fn local(id: &str) -> PlatformProfile {
        PlatformProfile {
            id: PlatformProfileId::new(id).unwrap(),
            display_name: id.to_owned(),
            config: PlatformConfig::Local(LocalConfig::default()),
        }
    }

    fn sensor() -> SensorDescriptor {
        SensorDescriptor {
            id: SensorId::new("sensor-a").unwrap(),
            display_name: "Sensor A".to_owned(),
            profile: SensorProfile {
                model: "sensor-a".to_owned(),
                modes: vec![SensorMode {
                    id: "mode-a".to_owned(),
                    raw: RawSpec {
                        width: 4,
                        height: 2,
                        bit_depth: 12,
                        bayer: BayerPattern::Bggr,
                    },
                    is_wdr: false,
                }],
                registers: Vec::new(),
            },
        }
    }

    #[test]
    fn round_trip_is_canonical_and_insertion_order_independent() {
        let mut first = ProfileStore::new();
        first.insert_platform(local("z")).unwrap();
        first.insert_platform(local("a")).unwrap();
        first.insert_sensor(sensor()).unwrap();
        let mode = SensorModeKey {
            sensor_id: SensorId::new("sensor-a").unwrap(),
            mode_id: "mode-a".to_owned(),
        };
        first
            .insert_capability_cell(SensorPlatformCapabilityCell {
                sensor_mode: mode,
                platform_id: PlatformProfileId::new("z").unwrap(),
                restrictions: vec![CapabilityRestriction {
                    capability: CapabilityKind::Dump,
                    allowed_variants: BTreeSet::from([CapabilityVariant::Raw12]),
                }],
                declarations: vec![EvidencedCapabilityDeclaration {
                    capability: CapabilityKind::Dump,
                    supported: true,
                    evidence: EvidenceMaturity::ProfileDeclared,
                }],
            })
            .unwrap();

        let bytes = first.export_json().unwrap();
        let second = ProfileStore::import_json(&bytes).unwrap();
        assert_eq!(second.export_json().unwrap(), bytes);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.find("\"id\": \"a\"").unwrap() < text.find("\"id\": \"z\"").unwrap());
    }

    #[test]
    fn schema_and_duplicate_ids_are_typed_errors() {
        let future = br#"{"schema_version":99,"future_field":{"new":true}}"#;
        assert!(matches!(
            ProfileStore::import_json(future),
            Err(ProfileStoreError::UnsupportedSchemaVersion {
                found: 99,
                supported: 1
            })
        ));
        let duplicate = br#"{
          "schema_version": 1,
          "platforms": [
            {"id":"same","display_name":"A","provider":"local"},
            {"id":"same","display_name":"B","provider":"local"}
          ],
          "sensors": [],
          "capability_bindings": []
        }"#;
        assert!(matches!(
            ProfileStore::import_json(duplicate),
            Err(ProfileStoreError::DuplicatePlatform(_))
        ));
    }

    #[test]
    fn credential_reference_round_trips_but_debug_is_redacted() {
        let profile = PlatformProfile {
            id: PlatformProfileId::new("ssh").unwrap(),
            display_name: "SSH".to_owned(),
            config: PlatformConfig::SshManaged(SshManagedConfig {
                host: "192.0.2.10".to_owned(),
                port: 22,
                username: "camera".to_owned(),
                expected_host_key: "ssh-ed25519 AAAA".to_owned(),
                credential_ref: "key-file:/home/user/.ssh/id_ed25519".to_owned(),
                command_subsystem: Some("camera-toolbox-argv-v1".to_owned()),
                capture_recipe: "capture".to_owned(),
                remote_artifact_dir: "/data/captures".to_owned(),
                remote_artifact_glob: "*.raw".to_owned(),
                remote_event_subsystem: None,
                passive_watch_auto_open: false,
                stable_samples: 2,
                stability_interval_ms: 500,
                max_fetch_bytes: 256 * 1024 * 1024,
                command_output_bytes: 64 * 1024,
            }),
        };
        assert!(!format!("{profile:?}").contains("id_ed25519"));
        let mut store = ProfileStore::new();
        store.insert_platform(profile).unwrap();
        let bytes = store.export_json().unwrap();
        let loaded = ProfileStore::import_json(&bytes).unwrap();
        let loaded = loaded
            .platform(&PlatformProfileId::new("ssh").unwrap())
            .unwrap();
        let PlatformConfig::SshManaged(config) = &loaded.config else {
            panic!()
        };
        assert_eq!(config.credential_ref, "key-file:/home/user/.ssh/id_ed25519");
        assert_eq!(
            config.command_subsystem.as_deref(),
            Some("camera-toolbox-argv-v1")
        );
    }

    #[test]
    fn schema_v1_round_trips_ssh_profile_without_command_recipe() {
        let json = br#"{
          "schema_version": 1,
          "platforms": [{
            "id": "ssh-files",
            "display_name": "SSH Files",
            "provider": "ssh-managed",
            "host": "2001:db8::10",
            "port": 2222,
            "username": "camera",
            "expected_host_key": "ssh-ed25519 AAAA",
            "credential_ref": "session:camera",
            "command_subsystem": null,
            "capture_recipe": "",
            "remote_artifact_dir": "/data/captures",
            "remote_artifact_glob": "*.raw",
            "remote_event_subsystem": null,
            "passive_watch_auto_open": false,
            "stable_samples": 2,
            "stability_interval_ms": 500,
            "max_fetch_bytes": 1024,
            "command_output_bytes": 1024
          }],
          "sensors": [],
          "capability_bindings": []
        }"#;
        let store = ProfileStore::import_json(json).unwrap();
        let exported = store.export_json().unwrap();
        let reloaded = ProfileStore::import_json(&exported).unwrap();
        let profile = reloaded
            .platform(&PlatformProfileId::new("ssh-files").unwrap())
            .unwrap();
        let PlatformConfig::SshManaged(config) = &profile.config else {
            panic!("expected SSH-managed profile")
        };
        assert!(config.capture_recipe.is_empty());
        assert!(config.command_subsystem.is_none());
        assert_eq!(PROFILE_STORE_SCHEMA_VERSION, 1);
    }

    #[test]
    fn plaintext_credential_is_rejected_before_persistence() {
        let profile = PlatformProfile {
            id: PlatformProfileId::new("ssh").unwrap(),
            display_name: "SSH".to_owned(),
            config: PlatformConfig::SshManaged(SshManagedConfig {
                host: "host".to_owned(),
                port: 22,
                username: "user".to_owned(),
                expected_host_key: "host-key".to_owned(),
                credential_ref: "plaintext-password".to_owned(),
                command_subsystem: None,
                capture_recipe: "capture".to_owned(),
                remote_artifact_dir: "/data".to_owned(),
                remote_artifact_glob: "*.raw".to_owned(),
                remote_event_subsystem: None,
                passive_watch_auto_open: false,
                stable_samples: 2,
                stability_interval_ms: 500,
                max_fetch_bytes: 1024,
                command_output_bytes: 1024,
            }),
        };
        assert!(matches!(
            ProfileStore::new().insert_platform(profile),
            Err(ProfileStoreError::UnsafeCredentialReference)
        ));
    }

    #[test]
    fn cv610_profile_json_uses_one_tagged_variant() {
        let profile = PlatformProfile {
            id: PlatformProfileId::new("cv").unwrap(),
            display_name: "CV".to_owned(),
            config: PlatformConfig::HisiliconCv610(Cv610Config {
                host: "10.0.0.1".to_owned(),
                dump: Cv610DumpConfig {
                    port: 4321,
                    initialization: DumpInitializationPolicy::DirectOnly,
                    preferred_raw_bits: 12,
                },
                stream: Cv610StreamConfig::default(),
            }),
        };
        let mut store = ProfileStore::new();
        store.insert_platform(profile).unwrap();
        let text = String::from_utf8(store.export_json().unwrap()).unwrap();
        assert!(text.contains("\"provider\": \"hisilicon-cv610-pq\""));
        assert!(!text.contains("ssh-managed"));
    }
}
