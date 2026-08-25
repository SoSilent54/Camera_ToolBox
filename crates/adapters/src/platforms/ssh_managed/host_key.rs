//! SSH server host-key 发现、known_hosts 评估与显式信任。

use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use camera_toolbox_app::validate_ssh_host;
use directories::BaseDirs;
use russh::{
    client,
    keys::{
        known_hosts::{known_host_keys_path, learn_known_hosts_path},
        ssh_key,
    },
};
use thiserror::Error;

const MAX_SCAN_TIMEOUT: Duration = Duration::from_secs(30);

type PathLock = Arc<Mutex<()>>;
type PathLockRegistry = Mutex<BTreeMap<PathBuf, PathLock>>;

static PATH_LOCKS: OnceLock<PathLockRegistry> = OnceLock::new();

/// 已规范化的 server public key；不保留不可信 comment。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerHostKey {
    public_key: ssh_key::PublicKey,
    openssh: String,
    algorithm: String,
    fingerprint: String,
}

impl ServerHostKey {
    /// 解析并移除 OpenSSH public-key comment。
    ///
    /// # Errors
    ///
    /// 文本不是有效 OpenSSH public key 时返回错误。
    pub fn from_openssh(value: &str) -> Result<Self, HostKeyError> {
        let parsed = ssh_key::PublicKey::from_openssh(value)
            .map_err(|error| HostKeyError::InvalidServerKey(error.to_string()))?;
        Self::from_public_key(&parsed)
    }

    fn from_public_key(value: &ssh_key::PublicKey) -> Result<Self, HostKeyError> {
        let bytes = value
            .to_bytes()
            .map_err(|error| HostKeyError::InvalidServerKey(error.to_string()))?;
        let public_key = ssh_key::PublicKey::from_bytes(&bytes)
            .map_err(|error| HostKeyError::InvalidServerKey(error.to_string()))?;
        let openssh = public_key
            .to_openssh()
            .map_err(|error| HostKeyError::InvalidServerKey(error.to_string()))?;
        let algorithm = public_key.algorithm().as_str().to_owned();
        let fingerprint = public_key.fingerprint(ssh_key::HashAlg::Sha256).to_string();
        Ok(Self {
            public_key,
            openssh,
            algorithm,
            fingerprint,
        })
    }

    #[must_use]
    pub fn openssh(&self) -> &str {
        &self.openssh
    }

    #[must_use]
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

/// 已校验、可安全交给 socket 与 known_hosts API 的目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyTarget {
    host: String,
    port: u16,
}

impl HostKeyTarget {
    /// # Errors
    ///
    /// host 含注入字符或 port 为零时返回错误。
    pub fn new(host: impl Into<String>, port: u16) -> Result<Self, HostKeyError> {
        let host = host.into();
        validate_ssh_host(&host).map_err(|error| HostKeyError::InvalidTarget(error.to_string()))?;
        if port == 0 {
            return Err(HostKeyError::InvalidTarget(
                "SSH port must be in 1..=65535".to_owned(),
            ));
        }
        Ok(Self { host, port })
    }

    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

/// 扫描到的 key 相对当前 target 的 known_hosts 精确状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyAssessment {
    Trusted(ServerHostKey),
    Unknown(ServerHostKey),
    Changed(ServerHostKey),
}

impl HostKeyAssessment {
    #[must_use]
    pub const fn server_key(&self) -> &ServerHostKey {
        match self {
            Self::Trusted(key) | Self::Unknown(key) | Self::Changed(key) => key,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKeyTrustOutcome {
    Added,
    AlreadyTrusted,
}

/// 测试可注入的无认证 probe；接口中没有 credential 或 authenticate 操作。
pub trait ServerHostKeyProbe: Send + Sync {
    /// # Errors
    ///
    /// 握手超时、连接失败或 server 未提供 key 时返回错误。
    fn probe(
        &self,
        target: &HostKeyTarget,
        timeout: Duration,
    ) -> Result<ServerHostKey, HostKeyError>;
}

#[derive(Debug, Default)]
pub struct RusshServerHostKeyProbe;

impl ServerHostKeyProbe for RusshServerHostKeyProbe {
    fn probe(
        &self,
        target: &HostKeyTarget,
        timeout: Duration,
    ) -> Result<ServerHostKey, HostKeyError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| HostKeyError::Probe(error.to_string()))?;
        let observed = Arc::new(Mutex::new(None));
        let handler = ScanHandler {
            observed: Arc::clone(&observed),
        };
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(timeout),
            ..Default::default()
        });
        let address = (target.host(), target.port());
        let result = runtime.block_on(async {
            tokio::time::timeout(timeout, client::connect(config, address, handler)).await
        });
        if result.is_err() {
            return Err(HostKeyError::ProbeTimedOut);
        }
        let key = observed
            .lock()
            .map_err(|_| HostKeyError::ProbeStateUnavailable)?
            .take();
        if let Some(key) = key {
            return ServerHostKey::from_public_key(&key);
        }
        match result.expect("timeout handled above") {
            Ok(_) => Err(HostKeyError::Probe(
                "SSH handshake completed without exposing a server key".to_owned(),
            )),
            Err(error) => Err(HostKeyError::Probe(error.to_string())),
        }
    }
}

struct ScanHandler {
    observed: Arc<Mutex<Option<ssh_key::PublicKey>>>,
}

impl client::Handler for ScanHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let mut observed = self
            .observed
            .lock()
            .map_err(|_| russh::Error::Inconsistent)?;
        *observed = Some(server_public_key.clone());
        // 主动终止于 KEX；不会进入任何认证方法。
        Ok(false)
    }
}

/// 绑定一个 known_hosts 路径与一个无认证 probe。
pub struct HostKeyManager {
    known_hosts_path: PathBuf,
    probe: Arc<dyn ServerHostKeyProbe>,
}

impl HostKeyManager {
    /// 使用默认 `~/.ssh/known_hosts` 与 russh handshake probe。
    ///
    /// # Errors
    ///
    /// home 目录不可用时返回错误。
    pub fn production() -> Result<Self, HostKeyError> {
        let base = BaseDirs::new().ok_or(HostKeyError::HomeDirectoryUnavailable)?;
        Self::with_known_hosts_path(base.home_dir().join(".ssh/known_hosts"))
    }

    /// 注入 known_hosts 路径，主要用于确定性测试与隔离配置。
    ///
    /// # Errors
    ///
    /// 路径不是绝对规范路径时返回错误。
    pub fn with_known_hosts_path(path: impl Into<PathBuf>) -> Result<Self, HostKeyError> {
        Self::with_probe(path, Arc::new(RusshServerHostKeyProbe))
    }

    /// 注入 known_hosts 路径与 probe seam。
    ///
    /// # Errors
    ///
    /// 路径不是绝对规范路径时返回错误。
    pub fn with_probe(
        path: impl Into<PathBuf>,
        probe: Arc<dyn ServerHostKeyProbe>,
    ) -> Result<Self, HostKeyError> {
        let path = path.into();
        validate_known_hosts_path(&path)?;
        Ok(Self {
            known_hosts_path: path,
            probe,
        })
    }

    #[must_use]
    pub fn known_hosts_path(&self) -> &Path {
        &self.known_hosts_path
    }

    /// 仅执行有总时限的 SSH handshake，然后精确评估 observed key。
    ///
    /// # Errors
    ///
    /// timeout 越界、probe 或 known_hosts 读取失败时返回错误。
    pub fn scan_and_assess(
        &self,
        target: &HostKeyTarget,
        timeout: Duration,
    ) -> Result<HostKeyAssessment, HostKeyError> {
        validate_scan_timeout(timeout)?;
        let key = self.probe.probe(target, timeout)?;
        self.assess(target, key)
    }

    /// 使用 russh 0.62 hostname matching 读取所有 target entries 后精确分类。
    ///
    /// # Errors
    ///
    /// known_hosts 不可读或内容无效时返回错误。
    pub fn assess(
        &self,
        target: &HostKeyTarget,
        key: ServerHostKey,
    ) -> Result<HostKeyAssessment, HostKeyError> {
        let entries = read_target_entries(&self.known_hosts_path, target)?;
        Ok(classify(entries, key))
    }

    /// 显式信任 observed key；同一路径的 re-read + append 在进程内串行。
    ///
    /// 已有精确 key 时幂等成功；已有任意不同 key 时拒绝且不写文件。
    ///
    /// # Errors
    ///
    /// known_hosts 状态变化、锁或文件操作失败时返回错误。
    pub fn trust(
        &self,
        target: &HostKeyTarget,
        key: &ServerHostKey,
    ) -> Result<HostKeyTrustOutcome, HostKeyError> {
        let path_lock = path_lock(&self.known_hosts_path)?;
        let _guard = path_lock
            .lock()
            .map_err(|_| HostKeyError::KnownHostsLockUnavailable)?;
        let entries = read_target_entries(&self.known_hosts_path, target)?;
        if entries
            .iter()
            .any(|(_, recorded)| recorded == &key.public_key)
        {
            return Ok(HostKeyTrustOutcome::AlreadyTrusted);
        }
        if !entries.is_empty() {
            return Err(HostKeyError::ConcurrentHostKeyChanged);
        }
        learn_known_hosts_path(
            target.host(),
            target.port(),
            &key.public_key,
            &self.known_hosts_path,
        )
        .map_err(|error| HostKeyError::KnownHosts(error.to_string()))?;
        Ok(HostKeyTrustOutcome::Added)
    }
}

fn validate_scan_timeout(timeout: Duration) -> Result<(), HostKeyError> {
    if timeout.is_zero() || timeout > MAX_SCAN_TIMEOUT {
        return Err(HostKeyError::InvalidScanTimeout {
            maximum_seconds: MAX_SCAN_TIMEOUT.as_secs(),
        });
    }
    Ok(())
}

fn validate_known_hosts_path(path: &Path) -> Result<(), HostKeyError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || path.as_os_str().is_empty()
        || path
            .to_str()
            .is_none_or(|value| value.chars().any(char::is_control))
    {
        return Err(HostKeyError::InvalidKnownHostsPath);
    }
    Ok(())
}

fn read_target_entries(
    path: &Path,
    target: &HostKeyTarget,
) -> Result<Vec<(usize, ssh_key::PublicKey)>, HostKeyError> {
    if path.exists() {
        std::fs::File::open(path).map_err(|error| HostKeyError::KnownHosts(error.to_string()))?;
    }
    known_host_keys_path(target.host(), target.port(), path)
        .map_err(|error| HostKeyError::KnownHosts(error.to_string()))
}

fn classify(entries: Vec<(usize, ssh_key::PublicKey)>, key: ServerHostKey) -> HostKeyAssessment {
    if entries
        .iter()
        .any(|(_, recorded)| recorded == &key.public_key)
    {
        HostKeyAssessment::Trusted(key)
    } else if entries.is_empty() {
        HostKeyAssessment::Unknown(key)
    } else {
        HostKeyAssessment::Changed(key)
    }
}

fn path_lock(path: &Path) -> Result<PathLock, HostKeyError> {
    let registry = PATH_LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = registry
        .lock()
        .map_err(|_| HostKeyError::KnownHostsLockUnavailable)?;
    Ok(Arc::clone(
        locks
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    ))
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum HostKeyError {
    #[error("invalid SSH host-key target: {0}")]
    InvalidTarget(String),
    #[error("invalid SSH server public key: {0}")]
    InvalidServerKey(String),
    #[error("known_hosts path must be absolute, normalized UTF-8 without control characters")]
    InvalidKnownHostsPath,
    #[error("home directory is unavailable")]
    HomeDirectoryUnavailable,
    #[error("host-key scan timeout must be in (0, {maximum_seconds}] seconds")]
    InvalidScanTimeout { maximum_seconds: u64 },
    #[error("SSH host-key scan timed out")]
    ProbeTimedOut,
    #[error("SSH host-key scan failed: {0}")]
    Probe(String),
    #[error("SSH host-key scan state is unavailable")]
    ProbeStateUnavailable,
    #[error("known_hosts operation failed: {0}")]
    KnownHosts(String),
    #[error("known_hosts lock is unavailable")]
    KnownHostsLockUnavailable,
    #[error("host key changed concurrently; known_hosts was not modified")]
    ConcurrentHostKeyChanged,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";
    const HASHED_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF";
    const ECDSA_KEY: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBN76zuqnjypL54/w4763l7q1Sn3IBYHptJ5wcYfEWkzeNTvpexr05Z18m2yPT2SWRd1JJ8Aj5TYidG9MdSS5J78=";

    #[test]
    fn production_probe_creates_timeout_inside_runtime() {
        let target = HostKeyTarget::new("127.0.0.1", 1).unwrap();
        let _ = RusshServerHostKeyProbe.probe(&target, Duration::from_millis(50));
    }

    #[test]
    fn known_hosts_matching_covers_plain_hashed_comma_and_nondefault_port() {
        let directory = test_directory("matching");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("known_hosts");
        fs::write(
            &path,
            format!(
                "plain.example {KEY_A}\nfirst.example,comma.example {KEY_B}\n|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak= {HASHED_KEY}\n[port.example]:2222 {KEY_A}\n"
            ),
        )
        .unwrap();
        let manager = HostKeyManager::with_known_hosts_path(&path).unwrap();
        for (host, port, key) in [
            ("plain.example", 22, KEY_A),
            ("comma.example", 22, KEY_B),
            ("example.com", 22, HASHED_KEY),
            ("port.example", 2222, KEY_A),
        ] {
            let assessment = manager
                .assess(
                    &HostKeyTarget::new(host, port).unwrap(),
                    ServerHostKey::from_openssh(key).unwrap(),
                )
                .unwrap();
            assert!(matches!(assessment, HostKeyAssessment::Trusted(_)));
        }
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn matching_host_with_same_or_different_algorithm_is_changed() {
        let directory = test_directory("changed");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("known_hosts");
        let manager = HostKeyManager::with_known_hosts_path(&path).unwrap();
        let target = HostKeyTarget::new("camera.example", 22).unwrap();

        fs::write(&path, format!("camera.example {KEY_A}\n")).unwrap();
        assert!(matches!(
            manager
                .assess(&target, ServerHostKey::from_openssh(KEY_B).unwrap())
                .unwrap(),
            HostKeyAssessment::Changed(_)
        ));

        fs::write(&path, format!("camera.example {ECDSA_KEY}\n")).unwrap();
        assert!(matches!(
            manager
                .assess(&target, ServerHostKey::from_openssh(KEY_A).unwrap())
                .unwrap(),
            HostKeyAssessment::Changed(_)
        ));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn unknown_assessment_does_not_mutate_and_explicit_trust_is_idempotent() {
        let directory = test_directory("trust");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("known_hosts");
        let manager = HostKeyManager::with_known_hosts_path(&path).unwrap();
        let target = HostKeyTarget::new("camera.example", 22).unwrap();
        let key = ServerHostKey::from_openssh(KEY_A).unwrap();

        assert!(matches!(
            manager.assess(&target, key.clone()).unwrap(),
            HostKeyAssessment::Unknown(_)
        ));
        assert!(!path.exists());
        assert_eq!(
            manager.trust(&target, &key).unwrap(),
            HostKeyTrustOutcome::Added
        );
        let once = fs::read_to_string(&path).unwrap();
        assert_eq!(
            manager.trust(&target, &key).unwrap(),
            HostKeyTrustOutcome::AlreadyTrusted
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), once);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn trust_reread_rejects_concurrent_mismatch_without_mutation() {
        let directory = test_directory("concurrent");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("known_hosts");
        let manager = HostKeyManager::with_known_hosts_path(&path).unwrap();
        let target = HostKeyTarget::new("camera.example", 22).unwrap();
        let observed = ServerHostKey::from_openssh(KEY_A).unwrap();
        assert!(matches!(
            manager.assess(&target, observed.clone()).unwrap(),
            HostKeyAssessment::Unknown(_)
        ));

        let concurrent = format!("camera.example {KEY_B}\n");
        fs::write(&path, &concurrent).unwrap();
        assert_eq!(
            manager.trust(&target, &observed),
            Err(HostKeyError::ConcurrentHostKeyChanged)
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), concurrent);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn simultaneous_different_trusts_append_exactly_one_key() {
        let directory = test_directory("simultaneous");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("known_hosts");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut workers = Vec::new();
        for key in [KEY_A, KEY_B] {
            let manager = HostKeyManager::with_known_hosts_path(&path).unwrap();
            let target = HostKeyTarget::new("camera.example", 22).unwrap();
            let key = ServerHostKey::from_openssh(key).unwrap();
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                manager.trust(&target, &key)
            }));
        }
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Ok(HostKeyTrustOutcome::Added)))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(HostKeyError::ConcurrentHostKeyChanged)))
                .count(),
            1
        );
        assert_eq!(
            fs::read_to_string(&path)
                .unwrap()
                .lines()
                .filter(|line| !line.is_empty())
                .count(),
            1
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn malicious_target_and_known_hosts_path_are_rejected() {
        for host in [
            "camera.example,attacker",
            "camera.example\nattacker",
            "[2001:db8::1]",
            "|1|salt|hash",
            "camera#comment",
        ] {
            assert!(HostKeyTarget::new(host, 22).is_err(), "accepted {host:?}");
        }
        assert!(HostKeyTarget::new("2001:db8::1", 22).is_ok());
        assert!(HostKeyManager::with_known_hosts_path("relative/known_hosts").is_err());
    }

    #[derive(Debug)]
    struct FixedProbe {
        key: ServerHostKey,
        calls: Arc<Mutex<Vec<(HostKeyTarget, Duration)>>>,
    }

    impl ServerHostKeyProbe for FixedProbe {
        fn probe(
            &self,
            target: &HostKeyTarget,
            timeout: Duration,
        ) -> Result<ServerHostKey, HostKeyError> {
            self.calls.lock().unwrap().push((target.clone(), timeout));
            Ok(self.key.clone())
        }
    }

    #[test]
    fn scan_uses_injected_handshake_only_probe_and_bounded_timeout() {
        let directory = test_directory("probe");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("known_hosts");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let manager = HostKeyManager::with_probe(
            &path,
            Arc::new(FixedProbe {
                key: ServerHostKey::from_openssh(KEY_A).unwrap(),
                calls: Arc::clone(&calls),
            }),
        )
        .unwrap();
        let target = HostKeyTarget::new("camera.example", 22).unwrap();
        assert!(matches!(
            manager
                .scan_and_assess(&target, Duration::from_secs(3))
                .unwrap(),
            HostKeyAssessment::Unknown(_)
        ));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(target, Duration::from_secs(3))]
        );
        assert!(matches!(
            manager.scan_and_assess(
                &HostKeyTarget::new("camera.example", 22).unwrap(),
                Duration::from_secs(31)
            ),
            Err(HostKeyError::InvalidScanTimeout { .. })
        ));
        let _ = fs::remove_dir_all(directory);
    }

    fn test_directory(label: &str) -> PathBuf {
        let unique = format!(
            "camera-toolbox-host-key-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }
}
