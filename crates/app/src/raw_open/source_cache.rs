//! 有界、会话级、纯内存 RAW 源缓存。

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use thiserror::Error;

use crate::{FileKind, FileRef, FileSystem, FileSystemError, FileVersion, FsControl, ReadRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceReadProgress {
    pub bytes_read: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    reference: FileRef,
    version: FileVersion,
}

struct CacheEntry {
    bytes: Arc<Vec<u8>>,
    last_access: u64,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    resident_bytes: u64,
    access_clock: u64,
}

struct SourceCacheInner {
    max_bytes: u64,
    max_entries: usize,
    state: Mutex<CacheState>,
    acquisition: Mutex<()>,
}

#[derive(Clone)]
pub struct SourceCache(Arc<SourceCacheInner>);

#[derive(Clone)]
pub struct RawSourceHandle {
    key: CacheKey,
    bytes: Arc<Vec<u8>>,
}

impl std::fmt::Debug for SourceCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SourceCache")
            .field("max_bytes", &self.0.max_bytes)
            .field("max_entries", &self.0.max_entries)
            .field("resident_bytes", &self.resident_bytes())
            .field("entry_count", &self.entry_count())
            .finish()
    }
}

impl std::fmt::Debug for RawSourceHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RawSourceHandle")
            .field("reference", &self.key.reference)
            .field("version", &self.key.version)
            .finish_non_exhaustive()
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

    /// 返回缓存内不可变源字节；句柄存活期间底层分配不会被回收。
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }
}

impl SourceCache {
    /// 创建纯内存会话缓存。
    ///
    /// # Errors
    ///
    /// 预算为空时返回错误。
    pub fn new(max_bytes: u64, max_entries: usize) -> Result<Self, SourceCacheError> {
        if max_bytes == 0 || max_entries == 0 {
            return Err(SourceCacheError::InvalidBudget);
        }
        Ok(Self(Arc::new(SourceCacheInner {
            max_bytes,
            max_entries,
            state: Mutex::new(CacheState::default()),
            acquisition: Mutex::new(()),
        })))
    }

    /// 获取指定版本的不可变源快照。
    ///
    /// # Errors
    ///
    /// 文件类型、版本、读取、地址空间或缓存预算不满足时返回错误。
    pub fn acquire(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        max_source_bytes: u64,
        control: &FsControl,
    ) -> Result<RawSourceHandle, SourceCacheError> {
        self.acquire_with_progress(
            file_system,
            reference,
            max_source_bytes,
            control,
            &mut |_| {},
        )
    }

    /// 获取不可变源，并报告单调递增的传输进度。
    ///
    /// # Errors
    ///
    /// 文件类型、版本、读取、地址空间或缓存预算不满足时返回错误。
    pub fn acquire_with_progress(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        max_source_bytes: u64,
        control: &FsControl,
        progress: &mut dyn FnMut(SourceReadProgress),
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
        let capacity = usize::try_from(entry.version.size).map_err(|_| {
            SourceCacheError::SourceDoesNotFitAddressSpace {
                bytes: entry.version.size,
            }
        })?;
        let key = CacheKey {
            reference: reference.clone(),
            version: entry.version,
        };
        if let Some(handle) = self.0.cached_handle(&key) {
            progress(SourceReadProgress {
                bytes_read: entry.version.size,
                total_bytes: entry.version.size,
            });
            return Ok(handle);
        }

        let _acquisition = self
            .0
            .acquisition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(handle) = self.0.cached_handle(&key) {
            progress(SourceReadProgress {
                bytes_read: entry.version.size,
                total_bytes: entry.version.size,
            });
            return Ok(handle);
        }
        self.0.reserve(entry.version.size)?;
        control.checkpoint()?;

        let mut bytes = Vec::new();
        bytes.try_reserve_exact(capacity).map_err(|_| {
            SourceCacheError::SourceAllocationFailed {
                bytes: entry.version.size,
            }
        })?;
        progress(SourceReadProgress {
            bytes_read: 0,
            total_bytes: entry.version.size,
        });
        let outcome = file_system.read(
            reference,
            ReadRequest {
                offset: 0,
                max_bytes: entry.version.size,
            },
            control,
            &mut |chunk| {
                let next = bytes.len().checked_add(chunk.len()).ok_or(
                    FileSystemError::ReadLimitExceeded {
                        requested: u64::MAX,
                        limit: entry.version.size,
                    },
                )?;
                if next > capacity {
                    return Err(FileSystemError::ReadLimitExceeded {
                        requested: u64::try_from(next).unwrap_or(u64::MAX),
                        limit: entry.version.size,
                    });
                }
                bytes.extend_from_slice(chunk);
                progress(SourceReadProgress {
                    bytes_read: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                    total_bytes: entry.version.size,
                });
                Ok(())
            },
        )?;
        if outcome.bytes_read != entry.version.size
            || outcome.source_version != entry.version
            || bytes.len() != capacity
        {
            return Err(SourceCacheError::ChangedDuringAcquire(reference.clone()));
        }
        Ok(self.0.publish(key, bytes))
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
    fn cached_handle(&self, key: &CacheKey) -> Option<RawSourceHandle> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.access_clock = state.access_clock.saturating_add(1);
        let access = state.access_clock;
        let entry = state.entries.get_mut(key)?;
        entry.last_access = access;
        Some(RawSourceHandle {
            key: key.clone(),
            bytes: Arc::clone(&entry.bytes),
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
                .filter(|(_, entry)| Arc::strong_count(&entry.bytes) == 1)
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| key.clone());
            let Some(candidate) = candidate else {
                return Err(SourceCacheError::CacheCapacity {
                    required: incoming,
                    available: self.max_bytes.saturating_sub(state.resident_bytes),
                });
            };
            let entry = state.entries.remove(&candidate).expect("candidate exists");
            state.resident_bytes = state
                .resident_bytes
                .saturating_sub(u64::try_from(entry.bytes.len()).unwrap_or(u64::MAX));
        }
        Ok(())
    }

    fn publish(&self, key: CacheKey, bytes: Vec<u8>) -> RawSourceHandle {
        let bytes = Arc::new(bytes);
        let handle = RawSourceHandle {
            key: key.clone(),
            bytes: Arc::clone(&bytes),
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.access_clock = state.access_clock.saturating_add(1);
        let access = state.access_clock;
        state.resident_bytes = state.resident_bytes.saturating_add(key.version.size);
        state.entries.insert(
            key,
            CacheEntry {
                bytes,
                last_access: access,
            },
        );
        handle
    }
}

#[derive(Debug, Error)]
pub enum SourceCacheError {
    #[error("source cache budgets must be non-zero")]
    InvalidBudget,
    #[error("source is not a file: {0:?}")]
    NotFile(FileRef),
    #[error("source is {bytes} bytes, exceeding the {limit} byte acquisition limit")]
    SourceTooLarge { bytes: u64, limit: u64 },
    #[error("source is {bytes} bytes and does not fit this platform address space")]
    SourceDoesNotFitAddressSpace { bytes: u64 },
    #[error("unable to allocate {bytes} bytes for the immutable source")]
    SourceAllocationFailed { bytes: u64 },
    #[error("source cache cannot reserve {required} bytes; {available} bytes available")]
    CacheCapacity { required: u64, available: u64 },
    #[error("source changed during cache acquisition: {0:?}")]
    ChangedDuringAcquire(FileRef),
    #[error(transparent)]
    FileSystem(#[from] FileSystemError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{
        DirectoryRef, EntryName, FileEntry, FileSourceId, FileSystemCapabilities, ListPage,
        ListPageRequest, ReadOutcome, SourcePath,
    };
    use camera_toolbox_core::{BayerPattern, RawEncoding, RawSpec};

    #[derive(Clone, Copy)]
    enum ReadFault {
        None,
        Short,
        ChangedVersion,
    }

    struct MemoryFileSystem {
        source_id: FileSourceId,
        files: HashMap<String, Vec<u8>>,
        read_fault: ReadFault,
        reads: Arc<AtomicUsize>,
    }

    impl MemoryFileSystem {
        fn new(files: impl IntoIterator<Item = (&'static str, &'static [u8])>) -> Self {
            Self {
                source_id: FileSourceId::new("memory").unwrap(),
                files: files
                    .into_iter()
                    .map(|(name, bytes)| (name.to_owned(), bytes.to_vec()))
                    .collect(),
                read_fault: ReadFault::None,
                reads: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_read_fault(mut self, fault: ReadFault) -> Self {
            self.read_fault = fault;
            self
        }

        fn reference(&self, name: &str) -> FileRef {
            FileRef::new(self.source_id.clone(), SourcePath::new(name).unwrap())
        }

        fn entry(&self, reference: &FileRef) -> Result<FileEntry, FileSystemError> {
            let bytes = self
                .files
                .get(reference.path.as_str())
                .ok_or_else(|| FileSystemError::NotFound(reference.path.as_str().to_owned()))?;
            Ok(FileEntry {
                reference: reference.clone(),
                name: EntryName::new(reference.path.file_name().unwrap()).unwrap(),
                kind: FileKind::File,
                version: FileVersion {
                    size: u64::try_from(bytes.len()).unwrap(),
                    modified_millis: Some(1),
                },
            })
        }
    }

    impl FileSystem for MemoryFileSystem {
        fn source_id(&self) -> &FileSourceId {
            &self.source_id
        }

        fn capabilities(&self) -> FileSystemCapabilities {
            FileSystemCapabilities::READ_ONLY
        }

        fn list(
            &self,
            _directory: &DirectoryRef,
            _page: ListPageRequest,
            _control: &FsControl,
        ) -> Result<ListPage, FileSystemError> {
            Err(FileSystemError::Unsupported)
        }

        fn stat(
            &self,
            reference: &FileRef,
            _control: &FsControl,
        ) -> Result<FileEntry, FileSystemError> {
            self.entry(reference)
        }

        fn read(
            &self,
            reference: &FileRef,
            request: ReadRequest,
            _control: &FsControl,
            consume: &mut dyn FnMut(&[u8]) -> Result<(), FileSystemError>,
        ) -> Result<ReadOutcome, FileSystemError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let entry = self.entry(reference)?;
            let bytes = self.files.get(reference.path.as_str()).unwrap();
            if request.offset != 0 || request.max_bytes < entry.version.size {
                return Err(FileSystemError::Unsupported);
            }
            let delivered = match self.read_fault {
                ReadFault::None | ReadFault::ChangedVersion => bytes.as_slice(),
                ReadFault::Short => &bytes[..bytes.len().saturating_sub(1)],
            };
            for chunk in delivered.chunks(2) {
                consume(chunk)?;
            }
            let mut source_version = entry.version;
            if matches!(self.read_fault, ReadFault::ChangedVersion) {
                source_version.modified_millis = Some(2);
            }
            Ok(ReadOutcome {
                bytes_read: u64::try_from(delivered.len()).unwrap(),
                source_version,
            })
        }

        fn mkdir(
            &self,
            _parent: &DirectoryRef,
            _name: &EntryName,
            _control: &FsControl,
        ) -> Result<DirectoryRef, FileSystemError> {
            Err(FileSystemError::Unsupported)
        }

        fn rename(
            &self,
            _reference: &FileRef,
            _new_name: &EntryName,
            _control: &FsControl,
        ) -> Result<FileRef, FileSystemError> {
            Err(FileSystemError::Unsupported)
        }

        fn move_entry(
            &self,
            _reference: &FileRef,
            _destination: &DirectoryRef,
            _control: &FsControl,
        ) -> Result<FileRef, FileSystemError> {
            Err(FileSystemError::Unsupported)
        }

        fn delete(
            &self,
            _reference: &FileRef,
            _recursive: bool,
            _control: &FsControl,
        ) -> Result<(), FileSystemError> {
            Err(FileSystemError::Unsupported)
        }
    }

    #[test]
    fn acquisition_is_memory_backed_and_reuses_the_same_allocation() {
        let file_system = MemoryFileSystem::new([("frame.raw", &[1, 2, 3, 4][..])]);
        let reference = file_system.reference("frame.raw");
        let cache = SourceCache::new(16, 4).unwrap();
        let control = FsControl::with_timeout(std::time::Duration::from_secs(1));

        let first = cache
            .acquire(&file_system, &reference, 16, &control)
            .unwrap();
        let second = cache
            .acquire(&file_system, &reference, 16, &control)
            .unwrap();

        assert_eq!(first.bytes(), &[1, 2, 3, 4]);
        assert!(Arc::ptr_eq(&first.bytes, &second.bytes));
        assert_eq!(cache.resident_bytes(), 4);
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn progress_is_monotonic_and_finishes_at_total_size() {
        let file_system = MemoryFileSystem::new([("frame.raw", &[1, 2, 3, 4, 5][..])]);
        let reference = file_system.reference("frame.raw");
        let cache = SourceCache::new(16, 4).unwrap();
        let control = FsControl::with_timeout(std::time::Duration::from_secs(1));
        let mut observed = Vec::new();

        cache
            .acquire_with_progress(&file_system, &reference, 16, &control, &mut |progress| {
                observed.push(progress);
            })
            .unwrap();

        assert_eq!(observed.first().unwrap().bytes_read, 0);
        assert_eq!(observed.last().unwrap().bytes_read, 5);
        assert!(
            observed
                .windows(2)
                .all(|pair| pair[0].bytes_read <= pair[1].bytes_read)
        );
        assert!(observed.iter().all(|progress| progress.total_bytes == 5));
    }

    #[test]
    fn pinned_entry_prevents_budget_overcommit() {
        let file_system = MemoryFileSystem::new([
            ("first.raw", &[1, 2, 3, 4][..]),
            ("second.raw", &[5, 6, 7, 8][..]),
        ]);
        let first = file_system.reference("first.raw");
        let second = file_system.reference("second.raw");
        let cache = SourceCache::new(4, 1).unwrap();
        let control = FsControl::with_timeout(std::time::Duration::from_secs(1));

        let pinned = cache.acquire(&file_system, &first, 4, &control).unwrap();
        assert!(matches!(
            cache.acquire(&file_system, &second, 4, &control),
            Err(SourceCacheError::CacheCapacity { .. })
        ));

        drop(pinned);
        let replacement = cache.acquire(&file_system, &second, 4, &control).unwrap();
        assert_eq!(replacement.bytes(), &[5, 6, 7, 8]);
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn short_or_changed_reads_are_not_published() {
        for fault in [ReadFault::Short, ReadFault::ChangedVersion] {
            let file_system =
                MemoryFileSystem::new([("frame.raw", &[1, 2, 3, 4][..])]).with_read_fault(fault);
            let reference = file_system.reference("frame.raw");
            let cache = SourceCache::new(16, 4).unwrap();
            let control = FsControl::with_timeout(std::time::Duration::from_secs(1));

            assert!(matches!(
                cache.acquire(&file_system, &reference, 16, &control),
                Err(SourceCacheError::ChangedDuringAcquire(changed)) if changed == reference
            ));
            assert_eq!(cache.entry_count(), 0);
            assert_eq!(cache.resident_bytes(), 0);
        }
    }

    #[test]
    fn cancelled_acquisition_is_not_published() {
        let file_system = MemoryFileSystem::new([("frame.raw", &[1, 2, 3, 4][..])]);
        let reference = file_system.reference("frame.raw");
        let cache = SourceCache::new(16, 4).unwrap();
        let control = FsControl::with_timeout(std::time::Duration::from_secs(1));
        control.cancellation.cancel();

        assert!(matches!(
            cache.acquire(&file_system, &reference, 16, &control),
            Err(SourceCacheError::FileSystem(FileSystemError::Cancelled))
        ));
        assert_eq!(cache.entry_count(), 0);
        assert_eq!(cache.resident_bytes(), 0);
    }
    #[test]
    fn decode_and_reinterpret_reuse_memory_after_source_is_dropped() {
        let file_system = MemoryFileSystem::new([("frame.raw", &[1, 0, 2, 0, 3, 0, 4, 0][..])]);
        let reads = Arc::clone(&file_system.reads);
        let reference = file_system.reference("frame.raw");
        let pipeline =
            crate::RawOpenPipeline::new(SourceCache::new(16, 4).unwrap(), Vec::new(), 16);
        let control = FsControl::with_timeout(std::time::Duration::from_secs(1));
        let params = crate::RawDecodeParams {
            spec: RawSpec {
                width: 2,
                height: 2,
                bit_depth: 12,
                bayer: BayerPattern::Rggb,
            },
            encoding: RawEncoding::U16Le,
        };

        let session = pipeline
            .inspect(&file_system, &reference, &control)
            .unwrap();
        let opened = pipeline
            .decode(
                session,
                crate::RawOpenMode::WithOptions(params.clone()),
                &control,
            )
            .unwrap();
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        drop(file_system);

        let reinterpreted = pipeline
            .reinterpret(opened.source, params, 7, &control)
            .unwrap();
        assert_eq!(reinterpreted.generation, Some(7));
        assert_eq!(reinterpreted.frame.pixels(), &[1, 2, 3, 4]);
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }
}
