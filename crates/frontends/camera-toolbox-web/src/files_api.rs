//! 文件浏览器 API：本地目录列表。

use std::path::{Component, Path, PathBuf};

use axum::http::StatusCode;
use serde::Serialize;

/// 目录条目。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
    pub is_directory: bool,
    pub size: u64,
}

/// 目录列表响应。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileListResponse {
    pub path: String,
    pub entries: Vec<DirectoryEntry>,
}


/// 列目录核心逻辑（ws_router 的 `file.local.list` 复用；错误归一为字符串）。
pub(crate) fn list_local_files_inner(
    root: &str,
    path: &str,
) -> std::result::Result<FileListResponse, String> {
    let root = resolve_root(root).map_err(|(_, msg)| msg)?;
    let relative = resolve_relative(path).map_err(|(_, msg)| msg)?;
    let dir = root.join(relative);
    let dir = std::fs::canonicalize(&dir)
        .map_err(|error| format!("directory not found: {error}"))?;
    if !dir.starts_with(&root) {
        return Err("path escapes workspace root".to_owned());
    }

    let mut entries = Vec::new();
    let read = std::fs::read_dir(&dir).map_err(|error| error.to_string())?;
    for entry in read {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = entry.metadata().ok();
        let is_directory = metadata.as_ref().is_some_and(std::fs::Metadata::is_dir);
        let size = if is_directory {
            0
        } else {
            metadata.as_ref().map_or(0, std::fs::Metadata::len)
        };
        entries.push(DirectoryEntry {
            name,
            path: path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned(),
            is_directory,
            size,
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_directory
            .cmp(&left.is_directory)
            .then_with(|| left.name.cmp(&right.name))
    });

    Ok(FileListResponse {
        path: path.trim_start_matches('/').to_owned(),
        entries,
    })
}

fn resolve_root(root: &str) -> Result<PathBuf, (StatusCode, String)> {
    let trimmed = root.trim();
    if trimmed.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "root must not be empty".to_owned()));
    }
    let path = Path::new(trimmed);
    if !path.is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            "root must be an absolute path".to_owned(),
        ));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| (StatusCode::NOT_FOUND, format!("root not found: {error}")))?;
    if !canonical.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "root must be a directory".to_owned()));
    }
    Ok(canonical)
}

fn resolve_relative(relative: &str) -> Result<PathBuf, (StatusCode, String)> {
    let trimmed = relative.trim();
    if trimmed.is_empty() {
        return Ok(PathBuf::new());
    }
    let mut normalized = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => continue,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "path must stay inside the workspace root".to_owned(),
                ));
            }
        }
    }
    Ok(normalized)
}
