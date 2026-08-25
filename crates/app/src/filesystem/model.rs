//! 传输无关的文件源、路径、目录项与版本标识。

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 工作区内稳定的文件源标识；本地挂载与远端 SFTP 使用同一命名空间。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileSourceId(String);

impl FileSourceId {
    /// 创建非空、可持久化的源标识。
    ///
    /// # Errors
    ///
    /// 空字符串、控制字符或路径分隔符会被拒绝。
    pub fn new(value: impl Into<String>) -> Result<Self, FileModelError> {
        let value = value.into();
        if value.is_empty()
            || value.chars().any(char::is_control)
            || value.contains('/')
            || value.contains('\\')
        {
            return Err(FileModelError::InvalidSourceId(value));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FileSourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 文件源根目录之下的规范相对路径，统一使用 `/` 分隔。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourcePath(String);

impl SourcePath {
    /// 根目录路径。
    #[must_use]
    pub const fn root() -> Self {
        Self(String::new())
    }

    /// 规范化源内相对路径。
    ///
    /// # Errors
    ///
    /// 绝对路径、父目录跳转、NUL、反斜杠或空文件名会被拒绝。
    pub fn new(value: impl AsRef<str>) -> Result<Self, FileModelError> {
        normalize_path(value.as_ref(), false).map(Self)
    }

    /// 规范化目录路径；空字符串表示源根目录。
    ///
    /// # Errors
    ///
    /// 绝对路径、父目录跳转、NUL 或反斜杠会被拒绝。
    pub fn directory(value: impl AsRef<str>) -> Result<Self, FileModelError> {
        normalize_path(value.as_ref(), true).map(Self)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// 在当前目录下追加一个已校验名称。
    #[must_use]
    pub fn join(&self, name: &EntryName) -> Self {
        if self.is_root() {
            Self(name.0.clone())
        } else {
            Self(format!("{}/{}", self.0, name.0))
        }
    }

    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        Some(
            self.0
                .rsplit_once('/')
                .map_or_else(Self::root, |(parent, _)| Self(parent.to_owned())),
        )
    }

    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        (!self.is_root())
            .then(|| self.0.rsplit('/').next())
            .flatten()
    }
}

fn normalize_path(value: &str, allow_root: bool) -> Result<String, FileModelError> {
    if value.contains('\0') || value.contains('\\') || value.starts_with('/') {
        return Err(FileModelError::InvalidSourcePath(value.to_owned()));
    }
    let mut components = Vec::new();
    for component in value.split('/') {
        match component {
            "" | "." => {}
            ".." => return Err(FileModelError::PathTraversal(value.to_owned())),
            component if component.chars().any(char::is_control) => {
                return Err(FileModelError::InvalidSourcePath(value.to_owned()));
            }
            component => components.push(component),
        }
    }
    if components.is_empty() && !allow_root {
        return Err(FileModelError::InvalidSourcePath(value.to_owned()));
    }
    Ok(components.join("/"))
}

/// 单个目录项名称；不允许携带路径语义。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryName(String);

impl EntryName {
    /// # Errors
    ///
    /// 空名称、`.`、`..`、分隔符、NUL 或控制字符会被拒绝。
    pub fn new(value: impl Into<String>) -> Result<Self, FileModelError> {
        let value = value.into();
        if value.is_empty()
            || value == "."
            || value == ".."
            || value.contains('/')
            || value.contains('\\')
            || value.chars().any(char::is_control)
        {
            return Err(FileModelError::InvalidEntryName(value));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EntryName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DirectoryRef {
    pub source_id: FileSourceId,
    pub path: SourcePath,
}

impl DirectoryRef {
    #[must_use]
    pub const fn new(source_id: FileSourceId, path: SourcePath) -> Self {
        Self { source_id, path }
    }

    #[must_use]
    pub fn root(source_id: FileSourceId) -> Self {
        Self::new(source_id, SourcePath::root())
    }

    #[must_use]
    pub fn child(&self, name: &EntryName) -> Self {
        Self::new(self.source_id.clone(), self.path.join(name))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileRef {
    pub source_id: FileSourceId,
    pub path: SourcePath,
}

impl FileRef {
    #[must_use]
    pub const fn new(source_id: FileSourceId, path: SourcePath) -> Self {
        Self { source_id, path }
    }

    #[must_use]
    pub fn parent(&self) -> DirectoryRef {
        DirectoryRef::new(
            self.source_id.clone(),
            self.path.parent().unwrap_or_else(SourcePath::root),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// 远端与本地都可稳定比较的轻量版本快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileVersion {
    pub size: u64,
    pub modified_millis: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub reference: FileRef,
    pub name: EntryName,
    pub kind: FileKind,
    pub version: FileVersion,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FileModelError {
    #[error("invalid file source id: {0}")]
    InvalidSourceId(String),
    #[error("invalid source-relative path: {0}")]
    InvalidSourcePath(String),
    #[error("source-relative path escapes its mount: {0}")]
    PathTraversal(String),
    #[error("invalid directory entry name: {0}")]
    InvalidEntryName(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_relative_source_path() {
        assert_eq!(SourcePath::directory("a/./b//").unwrap().as_str(), "a/b");
        assert!(matches!(
            SourcePath::directory("a/../b"),
            Err(FileModelError::PathTraversal(_))
        ));
        assert!(SourcePath::new("").is_err());
        assert!(SourcePath::directory("").unwrap().is_root());
    }

    #[test]
    fn joins_only_valid_entry_names() {
        let root = SourcePath::root();
        assert_eq!(
            root.join(&EntryName::new("raw.bin").unwrap()).as_str(),
            "raw.bin"
        );
        assert!(EntryName::new("../raw.bin").is_err());
    }
}
