//! 生产 credential resolver：显式 key-file 引用与进程内 session secret。

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Mutex, MutexGuard},
};

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
        let path = Path::new(path);
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
        let key = fs::read_to_string(path)
            .map_err(|error| format!("cannot read private-key file {}: {error}", path.display()))?;
        if !key.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----") {
            return Err(format!(
                "private-key file is not an OpenSSH private key: {}",
                path.display()
            ));
        }
        Ok(SshCredential::PrivateKeyOpenSsh {
            key: SecretString::from(key),
            passphrase: None,
        })
    }
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
        assert!(matches!(
            resolver.resolve(&reference),
            Ok(SshCredential::Password(_))
        ));
        assert!(resolver.remove_session(&reference).unwrap());
        assert!(resolver.resolve(&reference).is_err());
    }
}
