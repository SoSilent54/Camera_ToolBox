//! Profile commit 规范化与候选 ProfileStore 持久化。

#[cfg(feature = "platform-ssh")]
use camera_toolbox_app::PlatformConfig;
use camera_toolbox_app::{PlatformProfile, PlatformProfileId, ProfileStore};
#[cfg(feature = "platform-ssh")]
use secrecy::SecretString;

use super::device_manager::ProfileDraft;
#[cfg(feature = "platform-ssh")]
use super::ssh_profile::SshCommitAuth;

pub(super) struct NormalizedProfileCommit {
    pub(super) profile: PlatformProfile,
    pub(super) original_id: Option<PlatformProfileId>,
    #[cfg(feature = "platform-ssh")]
    pub(super) session_password: Option<(String, SecretString)>,
}

#[cfg(feature = "platform-ssh")]
pub(super) fn normalize_profile_commit(
    store: &ProfileStore,
    mut draft: ProfileDraft,
    ssh_auth: Option<SshCommitAuth>,
) -> Result<NormalizedProfileCommit, String> {
    let original_id = draft.original_id.clone();
    match &mut draft.config {
        PlatformConfig::SshManaged(config) => {
            let identity = format!("{}@{}", config.username.trim(), config.host.trim());
            if draft.display_name.trim().is_empty() {
                draft.display_name.clone_from(&identity);
            }
            if draft.id.trim().is_empty() {
                draft.id = unique_profile_id(store, &identity);
            }
            let profile_id =
                PlatformProfileId::new(draft.id.clone()).map_err(|error| error.to_string())?;
            let auth = ssh_auth.ok_or_else(|| "SSH authentication mode is missing".to_owned())?;
            let session_password = match auth {
                SshCommitAuth::Password { password } => {
                    let session_id = stable_session_id(&profile_id);
                    config.credential_ref = format!("session:{session_id}");
                    password.map(|password| (session_id, password))
                }
                SshCommitAuth::PrivateKeyFile { path } => {
                    if !path.is_absolute() {
                        return Err("SSH client private key path must be absolute".to_owned());
                    }
                    let path = path
                        .to_str()
                        .ok_or_else(|| "SSH client private key path must be UTF-8".to_owned())?;
                    config.credential_ref = format!("key-file:{path}");
                    None
                }
            };
            let profile = draft.build()?;
            Ok(NormalizedProfileCommit {
                profile,
                original_id,
                session_password,
            })
        }
        _ => {
            if ssh_auth.is_some() {
                return Err("SSH authentication was supplied for a non-SSH profile".to_owned());
            }
            Ok(NormalizedProfileCommit {
                profile: draft.build()?,
                original_id,
                session_password: None,
            })
        }
    }
}

#[cfg(not(feature = "platform-ssh"))]
pub(super) fn normalize_profile_commit(
    _store: &ProfileStore,
    draft: ProfileDraft,
) -> Result<NormalizedProfileCommit, String> {
    let original_id = draft.original_id.clone();
    Ok(NormalizedProfileCommit {
        profile: draft.build()?,
        original_id,
    })
}

/// 先在 clone 中校验 mutation，再持久化；调用者只安装成功返回的 candidate。
pub(super) fn persist_profile_candidate(
    current: &ProfileStore,
    profile: PlatformProfile,
    original_id: Option<&PlatformProfileId>,
    persist: impl FnOnce(&ProfileStore) -> Result<(), String>,
) -> Result<ProfileStore, String> {
    let mut candidate = current.clone();
    if let Some(original_id) = original_id {
        if original_id != &profile.id {
            return Err("profile ID is immutable while editing; duplicate it instead".to_owned());
        }
        candidate
            .replace_platform(profile)
            .map_err(|error| error.to_string())?;
    } else {
        candidate
            .insert_platform(profile)
            .map_err(|error| error.to_string())?;
    }
    persist(&candidate)?;
    Ok(candidate)
}

#[cfg(feature = "platform-ssh")]
pub(super) fn stable_session_id(profile_id: &PlatformProfileId) -> String {
    let mut id = String::with_capacity(8 + profile_id.as_str().len() * 2);
    id.push_str("profile-");
    for byte in profile_id.as_str().as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(id, "{byte:02x}");
    }
    id
}

#[cfg(feature = "platform-ssh")]
fn unique_profile_id(store: &ProfileStore, identity: &str) -> String {
    let mut base = String::new();
    let mut separator = false;
    for character in identity.chars() {
        if character.is_ascii_alphanumeric() {
            base.push(character.to_ascii_lowercase());
            separator = false;
        } else if !separator && !base.is_empty() {
            base.push('-');
            separator = true;
        }
    }
    while base.ends_with('-') {
        base.pop();
    }
    if base.is_empty() {
        base.push_str("ssh-profile");
    }
    let mut candidate = base.clone();
    let mut suffix = 2_u64;
    loop {
        let id =
            PlatformProfileId::new(candidate.clone()).expect("generated profile id is nonempty");
        if store.platform(&id).is_none() {
            return candidate;
        }
        candidate = format!("{base}-{suffix}");
        suffix = suffix.saturating_add(1);
    }
}

#[cfg(all(test, feature = "platform-ssh"))]
mod tests {
    use std::sync::{Arc, Mutex};

    use camera_toolbox_app::LocalConfig;

    use super::*;
    use crate::platform_ui::device_manager::incomplete_ssh_template;
    use crate::platform_ui::ssh_profile::SshCommitAuth;

    fn local_profile(id: &str) -> PlatformProfile {
        PlatformProfile {
            id: PlatformProfileId::new(id).unwrap(),
            display_name: id.to_owned(),
            config: PlatformConfig::Local(LocalConfig::default()),
        }
    }

    #[test]
    fn generated_id_name_and_session_reference_are_stable() {
        let mut draft = ProfileDraft {
            original_id: None,
            id: String::new(),
            display_name: String::new(),
            config: PlatformConfig::SshManaged(incomplete_ssh_template()),
        };
        let PlatformConfig::SshManaged(config) = &mut draft.config else {
            panic!()
        };
        config.host = "camera.example".to_owned();
        config.username = "root".to_owned();
        config.expected_host_key = "ssh-ed25519 test".to_owned();
        config.remote_artifact_dir = "/data".to_owned();
        config.remote_artifact_glob = "frame.raw".to_owned();
        let normalized = normalize_profile_commit(
            &ProfileStore::new(),
            draft,
            Some(SshCommitAuth::Password { password: None }),
        )
        .unwrap();
        assert_eq!(normalized.profile.id.as_str(), "root-camera-example");
        assert_eq!(normalized.profile.display_name, "root@camera.example");
    }

    #[test]
    fn candidate_is_persisted_before_install_and_failure_leaves_current_unchanged() {
        let mut current = ProfileStore::new();
        current.insert_platform(local_profile("old")).unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_in_persist = Arc::clone(&observed);
        let result =
            persist_profile_candidate(&current, local_profile("new"), None, move |candidate| {
                observed_in_persist
                    .lock()
                    .unwrap()
                    .push(candidate.platforms().count());
                Err("disk full".to_owned())
            });
        assert_eq!(result.unwrap_err(), "disk full");
        assert_eq!(*observed.lock().unwrap(), vec![2]);
        assert_eq!(current.platforms().count(), 1);
    }
}
