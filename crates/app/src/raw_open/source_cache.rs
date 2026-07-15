//! 有界、会话级 RAW 源缓存。

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use thiserror::Error;

use crate::{FileKind, FileRef, FileSystem, FileSystemError, FileVersion, FsControl, ReadRequest};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    reference: FileRef,
    version: FileVersion,
}

#[derive(Debug)]
struct CacheEntry {
    path: PathBuf,
    bytes: u64,
    last_access: u64,
    pins: usize,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    resident_bytes: u64,
    access_clock: u64,
    next_file_id: u64,
}

#[derive(Debug)]
struct SourceCacheInner {
    root: PathBuf,
    max_bytes: u64,
    max_entries: usize,
    state: Mutex<CacheState>,
    acquisition: Mutex<()>,
}

impl Drop for SourceCacheInner {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone, Debug)]
pub struct SourceCache(Arc<SourceCacheInner>);

#[derive(Debug)]
pub struct RawSourceHandle {
    cache: Arc<SourceCacheInner>,
    key: CacheKey,
    path: PathBuf,
}

impl Clone for RawSourceHandle {
    fn clone(&self) -> Self {
        self.cache.pin(&self.key);
        Self {
            cache: Arc::clone(&self.cache),
            key: self.key.clone(),
            path: self.path.clone(),
        }
    }
}

impl Drop for RawSourceHandle {
    fn drop(&mut self) {
        self.cache.unpin(&self.key);
    }
}

impl RawSourceHandle {
    #[must_use]
    pub fn reference(&self) -> &FileRef {
        &self.key.reference
    }

    #[must_use]
    pub const fn version(&self) -> FileVersion {
        self.key.version
    }

    #[must_use]
    pub fn cached_path(&self) -> &Path {
        &self.path
    }
}

impl SourceCache {
    /// 创建会话缓存目录；最后一个缓存/句柄释放后自动清理。
    ///
    /// # Errors
    ///
    /// 预算为空、目录创建失败或目录权限设置失败时返回错误。
    pub fn new(
        root: impl Into<PathBuf>,
        max_bytes: u64,
        max_entries: usize,
    ) -> Result<Self, SourceCacheError> {
        if max_bytes == 0 || max_entries == 0 {
            return Err(SourceCacheError::InvalidBudget);
        }
        let root = root.into();
        fs::create_dir_all(&root)?;
        restrict_directory(&root)?;
        Ok(Self(Arc::new(SourceCacheInner {
            root,
            max_bytes,
            max_entries,
            state: Mutex::new(CacheState::default()),
            acquisition: Mutex::new(()),
        })))
    }

    /// 获取指定版本的不可变源快照；未命中时从传输无关文件系统完整读取后原子发布。
    ///
    /// # Errors
    ///
    /// 文件类型、版本、读取、磁盘写入或缓存预算不满足时返回错误。
    pub fn acquire(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        max_source_bytes: u64,
        control: &FsControl,
    ) -> Result<RawSourceHandle, SourceCacheError> {
        control.checkpoint()?;
        let entry = file_system.stat(reference, control)?;
        if entry.kind != FileKind::File {
            return Err(SourceCacheError::NotFile(reference.clone()));
        }
        if entry.version.size > max_source_bytes {
            return Err(SourceCacheError::SourceTooLarge {
                bytes: entry.version.size,
                limit: max_source_bytes,
            });
        }
        let key = CacheKey {
            reference: reference.clone(),
            version: entry.version,
        };
        if let Some(handle) = self.0.cached_handle(&key) {
            return Ok(handle);
        }

        let _acquisition = self
            .0
            .acquisition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(handle) = self.0.cached_handle(&key) {
            return Ok(handle);
        }
        self.0.reserve(entry.version.size)?;
        control.checkpoint()?;

        let (temporary, published) = self.0.allocate_paths();
        let mut output = create_private_file(&temporary)?;
        let read_result = file_system.read(
            reference,
            ReadRequest {
                offset: 0,
                max_bytes: entry.version.size,
            },
            control,
            &mut |chunk| {
                output
                    .write_all(chunk)
                    .map_err(|error| FileSystemError::Io(error.to_string()))
            },
        );
        let outcome = match read_result {
            Ok(outcome) => outcome,
            Err(error) => {
                drop(output);
                let _ = fs::remove_file(&temporary);
                return Err(error.into());
            }
        };
        output.flush()?;
        output.sync_all()?;
        drop(output);
        if outcome.bytes_read != entry.version.size || outcome.source_version != entry.version {
            let _ = fs::remove_file(&temporary);
            return Err(SourceCacheError::ChangedDuringAcquire(reference.clone()));
        }
        fs::rename(&temporary, &published)?;
        self.0.publish(key, published)
    }

    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resident_bytes
    }

    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .len()
    }
}

impl SourceCacheInner {
    fn cached_handle(self: &Arc<Self>, key: &CacheKey) -> Option<RawSourceHandle> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.access_clock = state.access_clock.saturating_add(1);
        let access = state.access_clock;
        let entry = state.entries.get_mut(key)?;
        entry.last_access = access;
        entry.pins = entry.pins.saturating_add(1);
        Some(RawSourceHandle {
            cache: Arc::clone(self),
            key: key.clone(),
            path: entry.path.clone(),
        })
    }

    fn reserve(&self, incoming: u64) -> Result<(), SourceCacheError> {
        if incoming > self.max_bytes {
            return Err(SourceCacheError::CacheCapacity {
                required: incoming,
                available: self.max_bytes,
            });
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.entries.len() >= self.max_entries
            || state.resident_bytes.saturating_add(incoming) > self.max_bytes
        {
            let candidate = state
                .entries
                .iter()
                .filter(|(_, entry)| entry.pins == 0)
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| key.clone());
            let Some(candidate) = candidate else {
                return Err(SourceCacheError::CacheCapacity {
                    required: incoming,
                    available: self.max_bytes.saturating_sub(state.resident_bytes),
                });
            };
            let entry = state.entries.remove(&candidate).expect("candidate exists");
            state.resident_bytes = state.resident_bytes.saturating_sub(entry.bytes);
            let _ = fs::remove_file(entry.path);
        }
        Ok(())
    }

    fn allocate_paths(&self) -> (PathBuf, PathBuf) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = state.next_file_id;
        state.next_file_id = state.next_file_id.wrapping_add(1);
        (
            self.root.join(format!("source-{id}.partial")),
            self.root.join(format!("source-{id}.raw")),
        )
    }

    fn publish(
        self: &Arc<Self>,
        key: CacheKey,
        path: PathBuf,
    ) -> Result<RawSourceHandle, SourceCacheError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.access_clock = state.access_clock.saturating_add(1);
        let access = state.access_clock;
        let bytes = key.version.size;
        state.resident_bytes = state.resident_bytes.saturating_add(bytes);
        state.entries.insert(
            key.clone(),
            CacheEntry {
                path: path.clone(),
                bytes,
                last_access: access,
                pins: 1,
            },
        );
        Ok(RawSourceHandle {
            cache: Arc::clone(self),
            key,
            path,
        })
    }

    fn pin(&self, key: &CacheKey) {
        if let Some(entry) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .get_mut(key)
        {
            entry.pins = entry.pins.saturating_add(1);
        }
    }

    fn unpin(&self, key: &CacheKey) {
        if let Some(entry) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .get_mut(key)
        {
            entry.pins = entry.pins.saturating_sub(1);
        }
    }
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create_new(true).write(true).open(path)
}

#[derive(Debug, Error)]
pub enum SourceCacheError {
    #[error("source cache budgets must be non-zero")]
    InvalidBudget,
    #[error("source is not a file: {0:?}")]
    NotFile(FileRef),
    #[error("source is {bytes} bytes, exceeding the {limit} byte acquisition limit")]
    SourceTooLarge { bytes: u64, limit: u64 },
    #[error("source cache cannot reserve {required} bytes; {available} bytes available")]
    CacheCapacity { required: u64, available: u64 },
    #[error("source changed during cache acquisition: {0:?}")]
    ChangedDuringAcquire(FileRef),
    #[error(transparent)]
    FileSystem(#[from] FileSystemError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
