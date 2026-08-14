//! 文件浏览器 API：本地目录列表。

use std::path::{Component, Path, PathBuf};

use axum::{Json, extract::Query, http::StatusCode};
use serde::{Deserialize, Serialize};

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

/// 目录列表查询参数。
#[derive(Debug, Deserialize)]
pub struct FileListQuery {
    /// 工作区根目录（绝对路径）。
    pub root: String,
    /// 相对根目录的子路径；空表示根目录。
    #[serde(default)]
    pub path: String,
}

/// 列出本地目录。
pub async fn list_local_files(
    Query(query): Query<FileListQuery>,
) -> Result<Json<FileListResponse>, (StatusCode, String)> {
    let root = resolve_root(&query.root)?;
    let relative = resolve_relative(&query.path)?;
    let dir = root.join(relative);
    let dir = std::fs::canonicalize(&dir)
        .map_err(|error| (StatusCode::NOT_FOUND, format!("directory not found: {error}")))?;
    if !dir.starts_with(&root) {
        return Err((StatusCode::BAD_REQUEST, "path escapes workspace root".to_owned()));
    }

    let mut entries = Vec::new();
    let read = std::fs::read_dir(&dir)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    for entry in read {
        let entry = entry.map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
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

    Ok(Json(FileListResponse {
        path: query.path.trim_start_matches('/').to_owned(),
        entries,
    }))
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
