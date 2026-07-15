//! 文件系统能力端口；调用者只依赖相对路径和显式边界。

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use thiserror::Error;

use super::{DirectoryRef, EntryName, FileEntry, FileRef, FileSourceId, FileVersion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSystemCapabilities {
    pub create_directory: bool,
    pub rename: bool,
    pub move_entry: bool,
    pub delete: bool,
}

impl FileSystemCapabilities {
    pub const READ_ONLY: Self = Self {
        create_directory: false,
        rename: false,
        move_entry: false,
        delete: false,
    };

    pub const READ_WRITE: Self = Self {
        create_directory: true,
        rename: true,
        move_entry: true,
        delete: true,
    };
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListContinuation(String);

impl ListContinuation {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPageRequest {
    pub continuation: Option<ListContinuation>,
    pub limit: usize,
}

impl Default for ListPageRequest {
    fn default() -> Self {
        Self {
            continuation: None,
            limit: 256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPage {
    pub entries: Vec<FileEntry>,
    pub next: Option<ListContinuation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRequest {
    pub offset: u64,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOutcome {
    pub bytes_read: u64,
    pub source_version: FileVersion,
}

type FsInterrupt = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct FsCancellationInner {
    requested: AtomicBool,
    interrupt: Mutex<Option<FsInterrupt>>,
}

#[derive(Clone, Default)]
pub struct FsCancellation(Arc<FsCancellationInner>);

impl std::fmt::Debug for FsCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FsCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl FsCancellation {
    pub fn cancel(&self) {
        self.0.requested.store(true, Ordering::Release);
        let interrupt = self
            .0
            .interrupt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(interrupt) = interrupt {
            interrupt();
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.requested.load(Ordering::Acquire)
    }

    pub fn register_interrupt(&self, interrupt: FsInterrupt) {
        *self
            .0
            .interrupt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&interrupt));
        if self.is_cancelled() {
            interrupt();
        }
    }

    pub fn clear_interrupt(&self) {
        *self
            .0
            .interrupt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

#[derive(Debug, Clone)]
pub struct FsControl {
    pub cancellation: FsCancellation,
    deadline: Instant,
}

impl FsControl {
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            cancellation: FsCancellation::default(),
            deadline: Instant::now() + timeout,
        }
    }

    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// # Errors
    ///
    /// 操作已取消或超过 deadline 时返回相应错误。
    pub fn checkpoint(&self) -> Result<(), FileSystemError> {
        if self.cancellation.is_cancelled() {
            Err(FileSystemError::Cancelled)
        } else if self.remaining().is_zero() {
            Err(FileSystemError::TimedOut)
        } else {
            Ok(())
        }
    }
}

/// Local 与 SFTP 必须共同满足的同步、可取消文件能力。
///
/// `read` 以固定块回调，避免要求实现端一次性分配整个文件。
pub trait FileSystem: Send + Sync {
    fn source_id(&self) -> &FileSourceId;
    fn capabilities(&self) -> FileSystemCapabilities;

    fn list(
        &self,
        directory: &DirectoryRef,
        page: ListPageRequest,
        control: &FsControl,
    ) -> Result<ListPage, FileSystemError>;

    fn stat(&self, reference: &FileRef, control: &FsControl) -> Result<FileEntry, FileSystemError>;

    fn read(
        &self,
        reference: &FileRef,
        request: ReadRequest,
        control: &FsControl,
        consume: &mut dyn FnMut(&[u8]) -> Result<(), FileSystemError>,
    ) -> Result<ReadOutcome, FileSystemError>;

    fn mkdir(
        &self,
        parent: &DirectoryRef,
        name: &EntryName,
        control: &FsControl,
    ) -> Result<DirectoryRef, FileSystemError>;

    fn rename(
        &self,
        reference: &FileRef,
        new_name: &EntryName,
        control: &FsControl,
    ) -> Result<FileRef, FileSystemError>;

    fn move_entry(
        &self,
        reference: &FileRef,
        destination: &DirectoryRef,
        control: &FsControl,
    ) -> Result<FileRef, FileSystemError>;

    fn delete(
        &self,
        reference: &FileRef,
        recursive: bool,
        control: &FsControl,
    ) -> Result<(), FileSystemError>;
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FileSystemError {
    #[error("file source mismatch: expected {expected}, got {actual}")]
    SourceMismatch {
        expected: FileSourceId,
        actual: FileSourceId,
    },
    #[error("entry not found: {0}")]
    NotFound(String),
    #[error("entry already exists: {0}")]
    AlreadyExists(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("file source disconnected: {0}")]
    Disconnected(String),
    #[error("not a directory: {0}")]
    NotDirectory(String),
    #[error("not a regular file: {0}")]
    NotFile(String),
    #[error("operation is not supported by this source")]
    Unsupported,
    #[error("operation cancelled")]
    Cancelled,
    #[error("operation timed out")]
    TimedOut,
    #[error("read exceeds configured bound: requested {requested} bytes, limit {limit} bytes")]
    ReadLimitExceeded { requested: u64, limit: u64 },
    #[error("directory scan exceeds configured bound: observed {observed} entries, limit {limit}")]
    DirectoryLimitExceeded { observed: usize, limit: usize },
    #[error("invalid or expired directory continuation")]
    InvalidContinuation,
    #[error("source changed during read: {0}")]
    ChangedDuringRead(String),
    #[error("path escapes mounted root: {0}")]
    PathEscapesRoot(String),
    #[error("filesystem I/O failed: {0}")]
    Io(String),
    #[error("remote filesystem failed: {0}")]
    Remote(String),
}

impl FileSystemError {
    #[must_use]
    pub fn io(error: impl std::fmt::Display) -> Self {
        Self::Io(error.to_string())
    }
}
