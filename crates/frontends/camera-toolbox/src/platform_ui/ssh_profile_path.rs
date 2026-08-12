//! SSH Profile 远程文件路径与客户端私钥文件校验。

use std::path::{Path, PathBuf};

use camera_toolbox_adapters::platforms::ssh_managed::discover_private_key_files_in;

pub(in crate::platform_ui) fn is_literal_remote_glob(glob: &str) -> bool {
    !glob.is_empty() && !glob.bytes().any(|byte| matches!(byte, b'*' | b'?'))
}

pub(in crate::platform_ui) fn combine_remote_file(root: &str, basename: &str) -> String {
    if root == "/" {
        format!("/{basename}")
    } else {
        format!("{}/{basename}", root.trim_end_matches('/'))
    }
}

pub(super) fn split_exact_remote_file(path: &str) -> Result<(String, String), String> {
    if !path.starts_with('/') || path.ends_with('/') || path.contains('\0') {
        return Err("remote file must be an exact absolute file path".to_owned());
    }
    let (root, basename) = path
        .rsplit_once('/')
        .ok_or_else(|| "remote file must include a literal basename".to_owned())?;
    if basename.is_empty() || basename.bytes().any(|byte| matches!(byte, b'*' | b'?')) {
        return Err("remote file basename must be literal (no '*' or '?')".to_owned());
    }
    if path.split('/').any(|component| component == "..") {
        return Err("remote file must not contain '..' path segments".to_owned());
    }
    Ok((
        if root.is_empty() { "/" } else { root }.to_owned(),
        basename.to_owned(),
    ))
}

pub(super) fn validate_client_private_key(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("SSH client private key path must be absolute".to_owned());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "private-key file has no parent directory".to_owned())?;
    let candidates = discover_private_key_files_in(parent).map_err(|error| error.to_string())?;
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("cannot normalize private-key path: {error}"))?;
    candidates
        .contains(&canonical)
        .then_some(canonical)
        .ok_or_else(|| "Selected file is not a valid protected OpenSSH private key".to_owned())
}
