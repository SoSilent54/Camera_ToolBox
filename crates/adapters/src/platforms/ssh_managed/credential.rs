//! 生产 credential resolver：显式 key-file 引用与进程内 session secret。

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use directories::BaseDirs;

use secrecy::SecretString;

use super::connection::{CredentialResolver, SshCredential};

const MAX_PRIVATE_KEY_BYTES: u64 = 1024 * 1024;

/// Secret material 只存在于进程内 map 或按 operation 读取的私钥文件。
#[derive(Default)]
pub struct ProductionCredentialResolver {
    sessions: Mutex<BTreeMap<String, SshCredential>>,
}

impl ProductionCredentialResolver {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sessions: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn register_session_password(
        &self,
        id: &str,
        password: SecretString,
    ) -> Result<String, String> {
        let reference = session_reference(id)?;
        self.lock()?
            .insert(reference.clone(), SshCredential::Password(password));
        Ok(reference)
    }

    pub fn register_session_private_key(
        &self,
        id: &str,
        key: SecretString,
        passphrase: Option<SecretString>,
    ) -> Result<String, String> {
        let reference = session_reference(id)?;
        self.lock()?.insert(
            reference.clone(),
            SshCredential::PrivateKeyOpenSsh { key, passphrase },
        );
        Ok(reference)
    }

    /// 只查询引用是否存在，不克隆 secret。
    pub fn has_session(&self, reference: &str) -> Result<bool, String> {
        Ok(self.lock()?.contains_key(reference))
    }

    pub fn remove_session(&self, reference: &str) -> Result<bool, String> {
        Ok(self.lock()?.remove(reference).is_some())
    }

    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<String, SshCredential>>, String> {
        self.sessions
            .lock()
            .map_err(|_| "session credential registry is unavailable".to_owned())
    }
}

impl CredentialResolver for ProductionCredentialResolver {
    fn resolve(&self, credential_ref: &str) -> Result<SshCredential, String> {
        if credential_ref.starts_with("session:") {
            return self.lock()?.get(credential_ref).cloned().ok_or_else(|| {
                format!(
                    "session credential {credential_ref} is not registered; re-enter it in Device Manager"
                )
            });
        }
        let Some(path) = credential_ref.strip_prefix("key-file:") else {
            return Err(
                "unsupported credential reference; use key-file:/absolute/path or session:<id>"
                    .to_owned(),
            );
        };
        let validated = validate_private_key_file(Path::new(path))?;
        Ok(SshCredential::PrivateKeyOpenSsh {
            key: SecretString::from(validated.contents),
            passphrase: None,
        })
    }
}

struct ValidatedPrivateKeyFile {
    canonical_path: PathBuf,
    contents: String,
}

fn validate_private_key_file(path: &Path) -> Result<ValidatedPrivateKeyFile, String> {
    if !path.is_absolute() {
        return Err("private-key file reference must use an absolute path".to_owned());
    }
    let metadata = fs::metadata(path).map_err(|error| {
        format!(
            "cannot read private-key metadata {}: {error}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "private-key path is not a file: {}",
            path.display()
        ));
    }
    if metadata.len() == 0 || metadata.len() > MAX_PRIVATE_KEY_BYTES {
        return Err(format!(
            "private-key file size must be 1..={MAX_PRIVATE_KEY_BYTES} bytes: {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "private-key file must not be group/world accessible (chmod 600): {}",
                path.display()
            ));
        }
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("cannot read private-key file {}: {error}", path.display()))?;
    if !contents.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----")
        || !contents
            .trim_end()
            .ends_with("-----END OPENSSH PRIVATE KEY-----")
    {
        return Err(format!(
            "private-key file is not an OpenSSH private key: {}",
            path.display()
        ));
    }
    let canonical_path = fs::canonicalize(path).map_err(|error| {
        format!(
            "cannot normalize private-key path {}: {error}",
            path.display()
        )
    })?;
    Ok(ValidatedPrivateKeyFile {
        canonical_path,
        contents,
    })
}

/// 从默认 `~/.ssh` 发现可选择的 OpenSSH 私钥文件。
///
/// # Errors
///
/// home 目录不可用或目录读取失败时返回错误。
pub fn discover_private_key_files() -> Result<Vec<PathBuf>, PrivateKeyDiscoveryError> {
    let base = BaseDirs::new().ok_or(PrivateKeyDiscoveryError::HomeDirectoryUnavailable)?;
    discover_private_key_files_in(&base.home_dir().join(".ssh"))
}

/// 只扫描指定目录直属文件；返回绝对、规范化且不含控制字符的路径。
///
/// # Errors
///
/// 目录路径不是绝对路径或无法读取时返回错误。
pub fn discover_private_key_files_in(
    directory: &Path,
) -> Result<Vec<PathBuf>, PrivateKeyDiscoveryError> {
    if !directory.is_absolute() {
        return Err(PrivateKeyDiscoveryError::DirectoryNotAbsolute);
    }
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut discovered = Vec::new();
    let entries =
        fs::read_dir(directory).map_err(|source| PrivateKeyDiscoveryError::ReadDirectory {
            path: directory.to_path_buf(),
            source,
        })?;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() || excluded_key_filename(&entry.file_name()) {
            continue;
        }
        let Ok(validated) = validate_private_key_file(&entry.path()) else {
            continue;
        };
        let Some(path) = validated.canonical_path.to_str() else {
            continue;
        };
        if path.chars().any(char::is_control) {
            continue;
        }
        discovered.push(validated.canonical_path);
    }
    discovered.sort();
    discovered.dedup();
    Ok(discovered)
}

fn excluded_key_filename(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return true;
    };
    name.ends_with(".pub")
        || matches!(
            name,
            "config" | "known_hosts" | "known_hosts.old" | "authorized_keys" | "authorized_keys2"
        )
}

#[derive(Debug, thiserror::Error)]
pub enum PrivateKeyDiscoveryError {
    #[error("home directory is unavailable")]
    HomeDirectoryUnavailable,
    #[error("private-key discovery directory must be absolute")]
    DirectoryNotAbsolute,
    #[error("cannot read private-key discovery directory {}: {source}", path.display())]
    ReadDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn session_reference(id: &str) -> Result<String, String> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(
            "session credential id must contain only ASCII letters, digits, '-' or '_'".to_owned(),
        );
    }
    Ok(format!("session:{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_session_is_actionable_and_plaintext_reference_is_rejected() {
        let resolver = ProductionCredentialResolver::new();
        let missing = match resolver.resolve("session:lab") {
            Err(error) => error,
            Ok(_) => panic!("missing session credential unexpectedly resolved"),
        };
        assert!(missing.contains("re-enter"));
        let plaintext = match resolver.resolve("password:secret") {
            Err(error) => error,
            Ok(_) => panic!("plaintext credential reference unexpectedly resolved"),
        };
        assert!(plaintext.contains("unsupported"));
    }

    #[test]
    fn session_secret_can_be_registered_and_removed_without_persistence() {
        let resolver = ProductionCredentialResolver::new();
        let reference = resolver
            .register_session_password("lab", SecretString::from("secret"))
            .unwrap();
        assert!(resolver.has_session(&reference).unwrap());
        assert!(matches!(
            resolver.resolve(&reference),
            Ok(SshCredential::Password(_))
        ));
        assert!(resolver.remove_session(&reference).unwrap());
        assert!(!resolver.has_session(&reference).unwrap());
        assert!(resolver.resolve(&reference).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn key_discovery_returns_only_direct_valid_private_key_paths() {
        use std::os::unix::fs::PermissionsExt;

        let directory = test_directory("discovery");
        fs::create_dir_all(directory.join("nested")).unwrap();
        write_key(&directory.join("id_ed25519"), 0o600);
        write_key(&directory.join("id_open.pub"), 0o600);
        write_key(&directory.join("config"), 0o600);
        write_key(&directory.join("too-open"), 0o644);
        write_key(&directory.join("nested/id_nested"), 0o600);
        fs::write(directory.join("not-a-key"), "ordinary text").unwrap();

        let discovered = discover_private_key_files_in(&directory).unwrap();
        assert_eq!(discovered, vec![directory.join("id_ed25519")]);

        let resolver = ProductionCredentialResolver::new();
        assert!(matches!(
            resolver.resolve(&format!("key-file:{}", discovered[0].display())),
            Ok(SshCredential::PrivateKeyOpenSsh { .. })
        ));
        let too_open = match resolver.resolve(&format!(
            "key-file:{}",
            directory.join("too-open").display()
        )) {
            Err(error) => error,
            Ok(_) => panic!("group-readable key unexpectedly resolved"),
        };
        assert!(too_open.contains("chmod 600"));
        let _ = fs::remove_dir_all(directory);

        fn write_key(path: &Path, mode: u32) {
            fs::write(
                path,
                "-----BEGIN OPENSSH PRIVATE KEY-----\ntest\n-----END OPENSSH PRIVATE KEY-----\n",
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    fn test_directory(label: &str) -> PathBuf {
        let unique = format!(
            "camera-toolbox-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }
}
