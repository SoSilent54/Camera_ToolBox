//! 平台 tagged profile 与现有 SensorProfile/Mode 的选择模型。

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use camera_toolbox_core::{
    BayerPattern,
    sensor::{SensorMode, SensorModeId, SensorProfile},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::snapshot::{SnapshotBuilder, SnapshotHash};

macro_rules! string_id {
    ($name:ident, $error:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// 创建非空标识。
            ///
            /// # Errors
            ///
            /// 标识为空或只包含空白时返回错误。
            pub fn new(value: impl Into<String>) -> Result<Self, ProfileError> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(ProfileError::$error);
                }
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

string_id!(PlatformProfileId, EmptyPlatformId);
string_id!(SensorId, EmptySensorId);

/// 单个平台实例；config tagged enum 保证 variant 间不混入其他 transport 字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformProfile {
    pub id: PlatformProfileId,
    pub display_name: String,
    #[serde(flatten)]
    pub config: PlatformConfig,
}

impl PlatformProfile {
    /// 校验连接字段与端口。
    ///
    /// # Errors
    ///
    /// 名称、host、credential reference 无效或端口为零时返回错误。
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.display_name.trim().is_empty() {
            return Err(ProfileError::EmptyDisplayName);
        }
        self.config.validate()
    }

    /// 对完整 tagged profile 计算字段无歧义的 SHA-256 快照。
    ///
    /// # Errors
    ///
    /// profile 字段无效时返回错误。
    pub fn snapshot_hash(&self) -> Result<SnapshotHash, ProfileError> {
        self.validate()?;
        let mut hash = SnapshotHash::builder("camera-toolbox/platform-profile/v1");
        hash.string(self.id.as_str());
        hash.string(&self.display_name);
        self.config.hash_into(&mut hash);
        Ok(hash.finish())
    }
}

/// 每份 profile 恰好选择一个 provider variant。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider")]
pub enum PlatformConfig {
    #[serde(rename = "local")]
    Local(LocalConfig),
    #[serde(rename = "hisilicon-cv610-pq")]
    HisiliconCv610(Cv610Config),
    #[serde(rename = "ssh-managed")]
    SshManaged(SshManagedConfig),
}

impl PlatformConfig {
    fn validate(&self) -> Result<(), ProfileError> {
        match self {
            Self::Local(_) => Ok(()),
            Self::HisiliconCv610(config) => config.validate(),
            Self::SshManaged(config) => config.validate(),
        }
    }

    fn hash_into(&self, hash: &mut SnapshotBuilder) {
        match self {
            Self::Local(_) => hash.u8(0),
            Self::HisiliconCv610(config) => {
                hash.u8(1);
                hash.string(&config.host);
                hash.u16(config.dump.port);
                match &config.dump.initialization {
                    DumpInitializationPolicy::Auto => hash.u8(0),
                    DumpInitializationPolicy::DirectOnly => hash.u8(1),
                    DumpInitializationPolicy::ValidatedRecipe { recipe_id } => {
                        hash.u8(2);
                        hash.string(recipe_id);
                    }
                }
                hash.u8(config.dump.preferred_raw_bits);
                hash.u16(config.stream.port);
                hash.u16(config.stream.channel);
                hash.string(&config.stream.media);
                hash.boolean(config.stream.auto_reconnect);
            }
            Self::SshManaged(config) => {
                hash.u8(2);
                hash.string(&config.host);
                hash.u16(config.port);
                hash.string(&config.username);
                hash.string(&config.expected_host_key);
                hash.string(&config.credential_ref);
                hash.boolean(config.command_subsystem.is_some());
                if let Some(subsystem) = &config.command_subsystem {
                    hash.string(subsystem);
                }
                hash.string(&config.capture_recipe);
                hash.string(&config.remote_artifact_dir);
                hash.string(&config.remote_artifact_glob);
                hash.boolean(config.remote_event_subsystem.is_some());
                if let Some(subsystem) = &config.remote_event_subsystem {
                    hash.string(subsystem);
                }
                hash.boolean(config.passive_watch_auto_open);
                hash.u8(config.stable_samples);
                hash.u64(config.stability_interval_ms);
                hash.u64(config.max_fetch_bytes);
                hash.u64(config.command_output_bytes);
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalConfig {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cv610Config {
    pub host: String,
    pub dump: Cv610DumpConfig,
    pub stream: Cv610StreamConfig,
}

impl Cv610Config {
    fn validate(&self) -> Result<(), ProfileError> {
        validate_host(&self.host)?;
        validate_port(self.dump.port, "dump")?;
        validate_port(self.stream.port, "stream")?;
        if !matches!(self.dump.preferred_raw_bits, 10 | 12) {
            return Err(ProfileError::InvalidPreferredRawBits(
                self.dump.preferred_raw_bits,
            ));
        }
        if let DumpInitializationPolicy::ValidatedRecipe { recipe_id } = &self.dump.initialization
            && recipe_id.trim().is_empty()
        {
            return Err(ProfileError::EmptyInitializationRecipe);
        }
        if self.stream.media.trim().is_empty() {
            return Err(ProfileError::EmptyStreamMedia);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cv610DumpConfig {
    pub port: u16,
    pub initialization: DumpInitializationPolicy,
    pub preferred_raw_bits: u8,
}

impl Default for Cv610DumpConfig {
    fn default() -> Self {
        Self {
            port: 4321,
            initialization: DumpInitializationPolicy::Auto,
            preferred_raw_bits: 12,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DumpInitializationPolicy {
    Auto,
    DirectOnly,
    ValidatedRecipe { recipe_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cv610StreamConfig {
    pub port: u16,
    pub channel: u16,
    pub media: String,
    pub auto_reconnect: bool,
}

impl Default for Cv610StreamConfig {
    fn default() -> Self {
        Self {
            port: 80,
            channel: 0,
            media: "video_data".to_owned(),
            auto_reconnect: false,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshManagedConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// OpenSSH 公钥文本；provider 必须逐 key 严格比较，禁止 trust-on-first-use。
    pub expected_host_key: String,
    /// 仅保存凭据引用；密钥或密码由注入的 resolver 在 operation 时解析。
    pub credential_ref: String,
    /// 可选 CTARGV1 argv subsystem；`None` 使用普通 SSH exec。
    pub command_subsystem: Option<String>,
    pub capture_recipe: String,
    pub remote_artifact_dir: String,
    /// 只匹配 artifact 根目录直属文件名的 glob。
    pub remote_artifact_glob: String,
    pub remote_event_subsystem: Option<String>,
    /// 默认 false；启用后也只能请求后台打开。
    pub passive_watch_auto_open: bool,
    pub stable_samples: u8,
    pub stability_interval_ms: u64,
    pub max_fetch_bytes: u64,
    pub command_output_bytes: u64,
}

impl fmt::Debug for SshManagedConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshManagedConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("expected_host_key", &self.expected_host_key)
            .field("credential_ref", &"[REDACTED]")
            .field("command_subsystem", &self.command_subsystem)
            .field("capture_recipe", &self.capture_recipe)
            .field("remote_artifact_dir", &self.remote_artifact_dir)
            .field("remote_artifact_glob", &self.remote_artifact_glob)
            .field("remote_event_subsystem", &self.remote_event_subsystem)
            .field("passive_watch_auto_open", &self.passive_watch_auto_open)
            .field("stable_samples", &self.stable_samples)
            .field("stability_interval_ms", &self.stability_interval_ms)
            .field("max_fetch_bytes", &self.max_fetch_bytes)
            .field("command_output_bytes", &self.command_output_bytes)
            .finish()
    }
}

impl SshManagedConfig {
    pub const DEFAULT_PASSIVE_WATCH_AUTO_OPEN: bool = false;

    fn validate(&self) -> Result<(), ProfileError> {
        validate_host(&self.host)?;
        validate_port(self.port, "ssh")?;
        if self.username.trim().is_empty() {
            return Err(ProfileError::EmptySshUsername);
        }
        if self.expected_host_key.trim().is_empty() {
            return Err(ProfileError::EmptyExpectedHostKey);
        }
        if self.credential_ref.trim().is_empty() {
            return Err(ProfileError::EmptyCredentialReference);
        }
        if let Some(subsystem) = &self.command_subsystem {
            validate_subsystem(subsystem)?;
            if self.capture_recipe.is_empty() {
                return Err(ProfileError::CommandSubsystemWithoutRecipe);
            }
        }
        if let Some(subsystem) = &self.remote_event_subsystem {
            validate_subsystem(subsystem)?;
        }
        if !self.capture_recipe.is_empty() && self.capture_recipe.trim().is_empty() {
            return Err(ProfileError::InvalidCaptureRecipe);
        }
        if !self.remote_artifact_dir.starts_with('/')
            || self.remote_artifact_dir.contains('\0')
            || self.remote_artifact_dir.split('/').any(|part| part == "..")
        {
            return Err(ProfileError::InvalidRemoteArtifactDirectory);
        }
        if self.remote_artifact_glob.trim().is_empty()
            || self.remote_artifact_glob.contains(['/', '\0'])
            || self.remote_artifact_glob.contains("..")
        {
            return Err(ProfileError::InvalidRemoteArtifactGlob);
        }
        if !(2..=10).contains(&self.stable_samples) {
            return Err(ProfileError::InvalidStableSamples(self.stable_samples));
        }
        if !(100..=5_000).contains(&self.stability_interval_ms) {
            return Err(ProfileError::InvalidStabilityInterval(
                self.stability_interval_ms,
            ));
        }
        if self.max_fetch_bytes == 0 {
            return Err(ProfileError::ZeroSshFetchLimit);
        }
        if self.command_output_bytes == 0 {
            return Err(ProfileError::ZeroCommandOutputLimit);
        }
        Ok(())
    }
}

fn validate_subsystem(subsystem: &str) -> Result<(), ProfileError> {
    if subsystem.is_empty()
        || !subsystem
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ProfileError::InvalidSshSubsystem);
    }
    Ok(())
}

/// 校验可安全传给 socket 解析与 russh known_hosts API 的裸 host。
///
/// IPv6 使用未加方括号的形式；非默认端口的 known_hosts 标记由 russh 生成。
///
/// # Errors
///
/// host 为空、含首尾空白、ASCII 空白/控制字符或 known_hosts 分隔符时返回错误。
pub fn validate_ssh_host(host: &str) -> Result<(), ProfileError> {
    if host.is_empty() {
        return Err(ProfileError::EmptyHost);
    }
    if host.trim() != host
        || host
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        || host.contains([',', '[', ']', '|', '#'])
    {
        return Err(ProfileError::UnsafeHost);
    }
    Ok(())
}

fn validate_host(host: &str) -> Result<(), ProfileError> {
    validate_ssh_host(host)
}

fn validate_port(port: u16, service: &'static str) -> Result<(), ProfileError> {
    if port == 0 {
        Err(ProfileError::InvalidPort { service })
    } else {
        Ok(())
    }
}

/// 复用 core SensorProfile；不引入 Sensor provider 或寄存器权限推断。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorDescriptor {
    pub id: SensorId,
    pub display_name: String,
    pub profile: SensorProfile,
}

impl SensorDescriptor {
    #[must_use]
    pub fn mode(&self, mode_id: &str) -> Option<&SensorMode> {
        self.profile.modes.iter().find(|mode| mode.id == mode_id)
    }

    /// 固化某一现有 `SensorMode` 及其主键。
    ///
    /// # Errors
    ///
    /// mode 不存在时返回错误。
    pub fn mode_snapshot(&self, mode_id: &str) -> Result<SensorModeSnapshot, ProfileError> {
        let mode = self
            .mode(mode_id)
            .ok_or_else(|| ProfileError::UnknownSensorMode(mode_id.to_owned()))?
            .clone();
        SensorModeSnapshot::new(
            SensorModeKey {
                sensor_id: self.id.clone(),
                mode_id: mode.id.clone(),
            },
            mode,
        )
    }
    /// 校验可持久化 Sensor 描述与 mode 主键。
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.display_name.trim().is_empty() {
            return Err(ProfileError::EmptySensorDisplayName);
        }
        if self.profile.model.trim().is_empty() {
            return Err(ProfileError::EmptySensorModel);
        }
        let mut modes = BTreeSet::new();
        for mode in &self.profile.modes {
            if mode.id.trim().is_empty() {
                return Err(ProfileError::EmptySensorModeId);
            }
            if !modes.insert(mode.id.clone()) {
                return Err(ProfileError::DuplicateSensorMode {
                    sensor_id: self.id.clone(),
                    mode_id: mode.id.clone(),
                });
            }
            mode.raw
                .validate()
                .map_err(|error| ProfileError::InvalidSensorRaw(error.to_string()))?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorModeKey {
    pub sensor_id: SensorId,
    pub mode_id: SensorModeId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "selection", rename_all = "snake_case")]
pub enum SensorSelection {
    Unbound,
    Mode(SensorModeKey),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityResolutionKey {
    pub platform_id: PlatformProfileId,
    pub sensor: SensorSelection,
}

/// job 提交时固化的 SensorMode；mode 直接复用 core 类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensorModeSnapshot {
    pub key: SensorModeKey,
    pub mode: SensorMode,
    pub snapshot_hash: SnapshotHash,
}

impl SensorModeSnapshot {
    /// 创建并 hash 完整的 `(SensorModeKey, SensorMode)`。
    ///
    /// # Errors
    ///
    /// key 与 mode id 不匹配时返回错误。
    pub fn new(key: SensorModeKey, mode: SensorMode) -> Result<Self, ProfileError> {
        if key.mode_id != mode.id {
            return Err(ProfileError::SensorModeKeyMismatch {
                key_mode_id: key.mode_id,
                mode_id: mode.id,
            });
        }
        let mut hash = SnapshotHash::builder("camera-toolbox/sensor-mode/v1");
        hash.string(key.sensor_id.as_str());
        hash.string(&key.mode_id);
        hash.u32(mode.raw.width);
        hash.u32(mode.raw.height);
        hash.u8(mode.raw.bit_depth);
        hash.u8(match mode.raw.bayer {
            BayerPattern::Rggb => 0,
            BayerPattern::Grbg => 1,
            BayerPattern::Gbrg => 2,
            BayerPattern::Bggr => 3,
        });
        hash.boolean(mode.is_wdr);
        Ok(Self {
            key,
            mode,
            snapshot_hash: hash.finish(),
        })
    }
}

/// 轻量 Sensor catalog；只负责确定性查找，不负责硬件连接。
#[derive(Debug, Clone, Default)]
pub struct SensorCatalog {
    sensors: BTreeMap<SensorId, SensorDescriptor>,
}

impl SensorCatalog {
    /// 插入 Sensor，拒绝重复主键。
    ///
    /// # Errors
    ///
    /// 已存在相同 `SensorId` 时返回错误。
    pub fn insert(&mut self, sensor: SensorDescriptor) -> Result<(), ProfileError> {
        if self.sensors.contains_key(&sensor.id) {
            return Err(ProfileError::DuplicateSensor(sensor.id));
        }
        self.sensors.insert(sensor.id.clone(), sensor);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, id: &SensorId) -> Option<&SensorDescriptor> {
        self.sensors.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &SensorDescriptor> {
        self.sensors.values()
    }
}

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("sensor display name must not be empty")]
    EmptySensorDisplayName,
    #[error("sensor model must not be empty")]
    EmptySensorModel,
    #[error("sensor mode id must not be empty")]
    EmptySensorModeId,
    #[error("duplicate sensor mode {mode_id} for sensor {sensor_id}")]
    DuplicateSensorMode {
        sensor_id: SensorId,
        mode_id: SensorModeId,
    },
    #[error("invalid sensor RAW specification: {0}")]
    InvalidSensorRaw(String),
    #[error("platform id must not be empty")]
    EmptyPlatformId,
    #[error("sensor id must not be empty")]
    EmptySensorId,
    #[error("profile display name must not be empty")]
    EmptyDisplayName,
    #[error("platform host must not be empty")]
    EmptyHost,
    #[error("{service} port must be in 1..=65535")]
    InvalidPort { service: &'static str },
    #[error("preferred RAW bits must be 10 or 12, got {0}")]
    InvalidPreferredRawBits(u8),
    #[error("validated initialization recipe id must not be empty")]
    EmptyInitializationRecipe,
    #[error("stream media must not be empty")]
    EmptyStreamMedia,
    #[error("SSH username must not be empty")]
    EmptySshUsername,
    #[error("expected SSH host key must not be empty")]
    EmptyExpectedHostKey,
    #[error("credential reference must not be empty")]
    EmptyCredentialReference,
    #[error("SSH subsystem must contain only ASCII letters, digits, '-', '_' or '.'")]
    InvalidSshSubsystem,
    #[error("an SSH command subsystem requires a non-empty capture recipe")]
    CommandSubsystemWithoutRecipe,
    #[error("capture recipe must be empty or contain non-whitespace characters")]
    InvalidCaptureRecipe,
    #[error("platform host contains whitespace, control characters or known_hosts delimiters")]
    UnsafeHost,
    #[error("remote artifact directory must be an absolute normalized path")]
    InvalidRemoteArtifactDirectory,
    #[error("remote artifact glob must be a non-empty basename pattern")]
    InvalidRemoteArtifactGlob,
    #[error("stable samples must be in 2..=10, got {0}")]
    InvalidStableSamples(u8),
    #[error("stability interval must be in 100..=5000 ms, got {0}")]
    InvalidStabilityInterval(u64),
    #[error("SSH fetch hard limit must be non-zero")]
    ZeroSshFetchLimit,
    #[error("command output limit must be non-zero")]
    ZeroCommandOutputLimit,
    #[error("sensor mode not found: {0}")]
    UnknownSensorMode(String),
    #[error("sensor mode key {key_mode_id} does not match mode {mode_id}")]
    SensorModeKeyMismatch {
        key_mode_id: SensorModeId,
        mode_id: SensorModeId,
    },
    #[error("duplicate sensor id: {0}")]
    DuplicateSensor(SensorId),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_config_is_a_single_tagged_variant() {
        let profile = PlatformProfile {
            id: PlatformProfileId::new("cv610-lab").unwrap(),
            display_name: "CV610 Lab".to_owned(),
            config: PlatformConfig::HisiliconCv610(Cv610Config {
                host: "10.21.12.102".to_owned(),
                dump: Cv610DumpConfig::default(),
                stream: Cv610StreamConfig::default(),
            }),
        };
        let PlatformConfig::HisiliconCv610(config) = &profile.config else {
            panic!("expected CV610 tagged config");
        };
        assert_eq!(config.host, "10.21.12.102");
        assert_eq!(config.dump.port, 4321);
        assert!(profile.snapshot_hash().is_ok());
    }

    fn ssh_config(host: &str) -> SshManagedConfig {
        SshManagedConfig {
            host: host.to_owned(),
            port: 22,
            username: "camera".to_owned(),
            expected_host_key: "ssh-ed25519 AAAA".to_owned(),
            credential_ref: "session:camera".to_owned(),
            command_subsystem: None,
            capture_recipe: String::new(),
            remote_artifact_dir: "/data".to_owned(),
            remote_artifact_glob: "*.raw".to_owned(),
            remote_event_subsystem: None,
            passive_watch_auto_open: false,
            stable_samples: 2,
            stability_interval_ms: 500,
            max_fetch_bytes: 1024,
            command_output_bytes: 1024,
        }
    }

    #[test]
    fn empty_recipe_is_valid_only_without_command_subsystem() {
        assert!(ssh_config("camera.local").validate().is_ok());
        let mut inconsistent = ssh_config("camera.local");
        inconsistent.command_subsystem = Some("camera-toolbox-argv-v1".to_owned());
        assert!(matches!(
            inconsistent.validate(),
            Err(ProfileError::CommandSubsystemWithoutRecipe)
        ));
    }

    #[test]
    fn ssh_host_rejects_known_hosts_injection_and_accepts_normal_addresses() {
        for host in ["camera.local", "192.0.2.10", "2001:db8::10", "::1"] {
            assert!(validate_ssh_host(host).is_ok(), "rejected {host:?}");
        }
        for host in [
            " camera.local",
            "camera.local ",
            "camera\t.local",
            "camera\nattacker",
            "camera\0attacker",
            "camera,attacker",
            "[2001:db8::10]",
            "|1|salt|hash",
            "camera#comment",
        ] {
            assert!(
                matches!(validate_ssh_host(host), Err(ProfileError::UnsafeHost)),
                "accepted {host:?}"
            );
        }
    }
}
