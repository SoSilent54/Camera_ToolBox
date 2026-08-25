//! 本地目录挂载的受限文件系统实现。

use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::UNIX_EPOCH,
};

use camera_toolbox_app::{
    DirectoryRef, EntryName, FileEntry, FileKind, FileRef, FileSourceId, FileSystem,
    FileSystemCapabilities, FileSystemError, FileVersion, FsControl, ListContinuation, ListPage,
    ListPageRequest, ReadOutcome, ReadRequest, SourcePath,
};

const READ_CHUNK_BYTES: usize = 64 * 1024;
const MAX_LIST_PAGE_ENTRIES: usize = 1024;
const MAX_ACTIVE_LIST_CURSORS: usize = 16;

static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

struct ControlledWriter<'a, W> {
    inner: &'a mut W,
    control: &'a FsControl,
    control_error: Option<FileSystemError>,
}

impl<W: Write> ControlledWriter<'_, W> {
    fn checkpoint(&mut self) -> std::io::Result<()> {
        match self.control.checkpoint() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.control_error = Some(error.clone());
                Err(std::io::Error::other(error.to_string()))
            }
        }
    }
}

impl<W: Write> Write for ControlledWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.checkpoint()?;
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.checkpoint()?;
        self.inner.flush()
    }
}

struct LocalListCursor {
    directory: DirectoryRef,
    iterator: fs::ReadDir,
    pending: Option<std::io::Result<fs::DirEntry>>,
}

#[derive(Default)]
struct LocalCursorStore {
    next_id: u64,
    cursors: BTreeMap<String, LocalListCursor>,
    order: VecDeque<String>,
}

pub struct LocalFileSystem {
    source_id: FileSourceId,
    root: PathBuf,
    cursors: Mutex<LocalCursorStore>,
}

impl LocalFileSystem {
    /// 创建本地挂载并固定规范根目录。
    ///
    /// # Errors
    ///
    /// 根目录不存在、不是目录或无法规范化时返回错误。
    pub fn new(source_id: FileSourceId, root: impl AsRef<Path>) -> Result<Self, FileSystemError> {
        let root = fs::canonicalize(root).map_err(map_io)?;
        if !root.is_dir() {
            return Err(FileSystemError::NotDirectory(root.display().to_string()));
        }
        Ok(Self {
            source_id,
            root,
            cursors: Mutex::new(LocalCursorStore::default()),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn ensure_source(&self, actual: &FileSourceId) -> Result<(), FileSystemError> {
        if actual == &self.source_id {
            Ok(())
        } else {
            Err(FileSystemError::SourceMismatch {
                expected: self.source_id.clone(),
                actual: actual.clone(),
            })
        }
    }

    fn lexical_path(&self, path: &SourcePath) -> PathBuf {
        let mut output = self.root.clone();
        for component in path.as_str().split('/').filter(|part| !part.is_empty()) {
            output.push(component);
        }
        output
    }

    fn resolve_existing(&self, path: &SourcePath) -> Result<PathBuf, FileSystemError> {
        let lexical = self.lexical_path(path);
        let canonical = fs::canonicalize(&lexical).map_err(map_io)?;
        if !canonical.starts_with(&self.root) {
            return Err(FileSystemError::PathEscapesRoot(
                lexical.display().to_string(),
            ));
        }
        Ok(canonical)
    }

    /// 变更叶节点时只跟随并校验父目录，绝不把 symlink 替换成其目标。
    fn resolve_leaf(&self, path: &SourcePath) -> Result<PathBuf, FileSystemError> {
        let name = path
            .file_name()
            .ok_or_else(|| FileSystemError::PathEscapesRoot(path.as_str().to_owned()))?;
        let parent = path.parent().unwrap_or_else(SourcePath::root);
        let canonical_parent = self.resolve_existing(&parent)?;
        let leaf = canonical_parent.join(name);
        fs::symlink_metadata(&leaf).map_err(map_io)?;
        Ok(leaf)
    }

    fn resolve_destination(
        &self,
        directory: &DirectoryRef,
        name: &EntryName,
    ) -> Result<PathBuf, FileSystemError> {
        self.ensure_source(&directory.source_id)?;
        let parent = self.resolve_existing(&directory.path)?;
        if !parent.is_dir() {
            return Err(FileSystemError::NotDirectory(
                directory.path.as_str().to_owned(),
            ));
        }
        Ok(parent.join(name.as_str()))
    }

    fn create_temporary_file(parent: &Path) -> Result<(PathBuf, File), FileSystemError> {
        for _ in 0..32 {
            let id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".camera-toolbox-export-{}-{id}.tmp",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(map_io(error)),
            }
        }
        Err(FileSystemError::Io(
            "could not allocate a unique export staging filename".to_owned(),
        ))
    }

    fn entry_from_path(
        &self,
        reference: FileRef,
        name: EntryName,
        path: &Path,
    ) -> Result<FileEntry, FileSystemError> {
        let metadata = fs::symlink_metadata(path).map_err(map_io)?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            FileKind::Symlink
        } else if file_type.is_file() {
            FileKind::File
        } else if file_type.is_dir() {
            FileKind::Directory
        } else {
            FileKind::Other
        };
        Ok(FileEntry {
            reference,
            name,
            kind,
            version: version_from_metadata(&metadata),
        })
    }
}

impl LocalFileSystem {
    fn take_list_cursor(
        &self,
        directory: &DirectoryRef,
        continuation: Option<&ListContinuation>,
    ) -> Result<LocalListCursor, FileSystemError> {
        if let Some(continuation) = continuation {
            let mut store = self
                .cursors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let cursor = store
                .cursors
                .remove(continuation.as_str())
                .ok_or(FileSystemError::InvalidContinuation)?;
            store.order.retain(|token| token != continuation.as_str());
            if &cursor.directory != directory {
                return Err(FileSystemError::InvalidContinuation);
            }
            return Ok(cursor);
        }
        let path = self.resolve_existing(&directory.path)?;
        if !path.is_dir() {
            return Err(FileSystemError::NotDirectory(
                directory.path.as_str().to_owned(),
            ));
        }
        Ok(LocalListCursor {
            directory: directory.clone(),
            iterator: fs::read_dir(path).map_err(map_io)?,
            pending: None,
        })
    }

    fn store_list_cursor(&self, cursor: LocalListCursor) -> ListContinuation {
        let mut store = self
            .cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while store.cursors.len() >= MAX_ACTIVE_LIST_CURSORS {
            let Some(oldest) = store.order.pop_front() else {
                break;
            };
            store.cursors.remove(&oldest);
        }
        let token = format!("local-list-{}", store.next_id);
        store.next_id = store.next_id.wrapping_add(1);
        store.order.push_back(token.clone());
        store.cursors.insert(token.clone(), cursor);
        ListContinuation::new(token)
    }
}

impl FileSystem for LocalFileSystem {
    fn source_id(&self) -> &FileSourceId {
        &self.source_id
    }

    fn capabilities(&self) -> FileSystemCapabilities {
        FileSystemCapabilities::READ_WRITE
    }

    fn list(
        &self,
        directory: &DirectoryRef,
        page: ListPageRequest,
        control: &FsControl,
    ) -> Result<ListPage, FileSystemError> {
        control.checkpoint()?;
        self.ensure_source(&directory.source_id)?;
        let limit = page.limit.min(MAX_LIST_PAGE_ENTRIES);
        if limit == 0 {
            return Ok(ListPage {
                entries: Vec::new(),
                next: None,
            });
        }
        let mut cursor = self.take_list_cursor(directory, page.continuation.as_ref())?;
        let mut entries = Vec::with_capacity(limit);
        let mut exhausted = false;
        while entries.len() < limit {
            control.checkpoint()?;
            let item = cursor.pending.take().or_else(|| cursor.iterator.next());
            let Some(item) = item else {
                exhausted = true;
                break;
            };
            let item = item.map_err(map_io)?;
            let name = item.file_name().into_string().map_err(|_| {
                FileSystemError::Io("non-UTF-8 local filename is unsupported".into())
            })?;
            let name =
                EntryName::new(name).map_err(|error| FileSystemError::Io(error.to_string()))?;
            let source_path = directory.path.join(&name);
            let reference = FileRef::new(self.source_id.clone(), source_path);
            entries.push(self.entry_from_path(reference, name, &item.path())?);
        }
        if !exhausted {
            cursor.pending = cursor.iterator.next();
            exhausted = cursor.pending.is_none();
        }
        entries.sort_by(|left, right| {
            let left_dir = left.kind == FileKind::Directory;
            let right_dir = right.kind == FileKind::Directory;
            right_dir
                .cmp(&left_dir)
                .then_with(|| left.name.as_str().cmp(right.name.as_str()))
        });
        Ok(ListPage {
            entries,
            next: (!exhausted).then(|| self.store_list_cursor(cursor)),
        })
    }

    fn stat(&self, reference: &FileRef, control: &FsControl) -> Result<FileEntry, FileSystemError> {
        control.checkpoint()?;
        self.ensure_source(&reference.source_id)?;
        let path = self.resolve_leaf(&reference.path)?;
        let name = EntryName::new(reference.path.file_name().unwrap_or("/"))
            .map_err(|error| FileSystemError::Io(error.to_string()))?;
        self.entry_from_path(reference.clone(), name, &path)
    }

    fn read(
        &self,
        reference: &FileRef,
        request: ReadRequest,
        control: &FsControl,
        consume: &mut dyn FnMut(&[u8]) -> Result<(), FileSystemError>,
    ) -> Result<ReadOutcome, FileSystemError> {
        control.checkpoint()?;
        self.ensure_source(&reference.source_id)?;
        let path = self.resolve_existing(&reference.path)?;
        let before = fs::metadata(&path).map_err(map_io)?;
        if !before.is_file() {
            return Err(FileSystemError::NotFile(reference.path.as_str().to_owned()));
        }
        let version = version_from_metadata(&before);
        let remaining = before.len().saturating_sub(request.offset);
        if remaining > request.max_bytes {
            return Err(FileSystemError::ReadLimitExceeded {
                requested: remaining,
                limit: request.max_bytes,
            });
        }
        let mut file = File::open(&path).map_err(map_io)?;
        file.seek(SeekFrom::Start(request.offset)).map_err(map_io)?;
        let mut buffer = [0_u8; READ_CHUNK_BYTES];
        let mut total = 0_u64;
        while total < remaining {
            control.checkpoint()?;
            let capacity = usize::try_from((remaining - total).min(READ_CHUNK_BYTES as u64))
                .unwrap_or(READ_CHUNK_BYTES);
            let count = file.read(&mut buffer[..capacity]).map_err(map_io)?;
            if count == 0 {
                return Err(FileSystemError::ChangedDuringRead(
                    reference.path.as_str().to_owned(),
                ));
            }
            consume(&buffer[..count])?;
            total += count as u64;
        }
        let after = fs::metadata(&path).map_err(map_io)?;
        if version_from_metadata(&after) != version {
            return Err(FileSystemError::ChangedDuringRead(
                reference.path.as_str().to_owned(),
            ));
        }
        Ok(ReadOutcome {
            bytes_read: total,
            source_version: version,
        })
    }

    fn write_new_atomic(
        &self,
        parent: &DirectoryRef,
        name: &EntryName,
        control: &FsControl,
        produce: &mut dyn FnMut(&mut dyn Write) -> Result<(), FileSystemError>,
    ) -> Result<FileRef, FileSystemError> {
        control.checkpoint()?;
        let destination = self.resolve_destination(parent, name)?;
        let staging_parent = destination
            .parent()
            .ok_or_else(|| FileSystemError::PathEscapesRoot(destination.display().to_string()))?;
        let (staging, mut file) = Self::create_temporary_file(staging_parent)?;

        let produce_result = {
            let mut writer = ControlledWriter {
                inner: &mut file,
                control,
                control_error: None,
            };
            let result = produce(&mut writer);
            match writer.control_error.take() {
                Some(error) => Err(error),
                None => result,
            }
        };
        let write_result = produce_result.and_then(|()| {
            control.checkpoint()?;
            file.sync_all().map_err(map_io)
        });
        drop(file);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&staging);
            return Err(error);
        }
        if let Err(error) = control.checkpoint() {
            let _ = fs::remove_file(&staging);
            return Err(error);
        }

        // hard_link 的“目标必须不存在”是内核原子条件，避免 rename 覆盖已有导出文件。
        if let Err(error) = fs::hard_link(&staging, &destination).map_err(map_io) {
            let _ = fs::remove_file(&staging);
            return Err(error);
        }
        let _ = fs::remove_file(staging);

        Ok(FileRef::new(self.source_id.clone(), parent.path.join(name)))
    }

    fn mkdir(
        &self,
        parent: &DirectoryRef,
        name: &EntryName,
        control: &FsControl,
    ) -> Result<DirectoryRef, FileSystemError> {
        control.checkpoint()?;
        let destination = self.resolve_destination(parent, name)?;
        fs::create_dir(&destination).map_err(map_io)?;
        Ok(parent.child(name))
    }

    fn rename(
        &self,
        reference: &FileRef,
        new_name: &EntryName,
        control: &FsControl,
    ) -> Result<FileRef, FileSystemError> {
        control.checkpoint()?;
        self.ensure_source(&reference.source_id)?;
        let source = self.resolve_leaf(&reference.path)?;
        let parent = reference.parent();
        let destination = self.resolve_destination(&parent, new_name)?;
        if destination.exists() {
            return Err(FileSystemError::AlreadyExists(
                destination.display().to_string(),
            ));
        }
        fs::rename(source, &destination).map_err(map_io)?;
        Ok(FileRef::new(
            self.source_id.clone(),
            parent.path.join(new_name),
        ))
    }

    fn move_entry(
        &self,
        reference: &FileRef,
        destination: &DirectoryRef,
        control: &FsControl,
    ) -> Result<FileRef, FileSystemError> {
        control.checkpoint()?;
        self.ensure_source(&reference.source_id)?;
        self.ensure_source(&destination.source_id)?;
        let source = self.resolve_leaf(&reference.path)?;
        let name = EntryName::new(reference.path.file_name().unwrap_or_default())
            .map_err(|error| FileSystemError::Io(error.to_string()))?;
        let target = self.resolve_destination(destination, &name)?;
        if target.exists() {
            return Err(FileSystemError::AlreadyExists(target.display().to_string()));
        }
        fs::rename(source, &target).map_err(map_io)?;
        Ok(FileRef::new(
            self.source_id.clone(),
            destination.path.join(&name),
        ))
    }

    fn delete(
        &self,
        reference: &FileRef,
        recursive: bool,
        control: &FsControl,
    ) -> Result<(), FileSystemError> {
        control.checkpoint()?;
        self.ensure_source(&reference.source_id)?;
        let path = self.resolve_leaf(&reference.path)?;
        let metadata = fs::symlink_metadata(&path).map_err(map_io)?;
        if metadata.is_dir() {
            if recursive {
                fs::remove_dir_all(path).map_err(map_io)
            } else {
                fs::remove_dir(path).map_err(map_io)
            }
        } else {
            fs::remove_file(path).map_err(map_io)
        }
    }
}

fn version_from_metadata(metadata: &fs::Metadata) -> FileVersion {
    let modified_millis = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    FileVersion {
        size: metadata.len(),
        modified_millis,
    }
}

fn map_io(error: std::io::Error) -> FileSystemError {
    match error.kind() {
        std::io::ErrorKind::NotFound => FileSystemError::NotFound(error.to_string()),
        std::io::ErrorKind::AlreadyExists => FileSystemError::AlreadyExists(error.to_string()),
        std::io::ErrorKind::PermissionDenied => {
            FileSystemError::PermissionDenied(error.to_string())
        }
        std::io::ErrorKind::NotConnected
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionRefused
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::UnexpectedEof => FileSystemError::Disconnected(error.to_string()),
        _ => FileSystemError::Io(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("camera-toolbox-local-fs-{nonce}"));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn control() -> FsControl {
        FsControl::with_timeout(std::time::Duration::from_secs(5))
    }

    #[test]
    fn local_filesystem_supports_bounded_crud() {
        let root = TestRoot::new();
        fs::write(root.0.join("frame.raw"), [1_u8, 2, 3, 4]).unwrap();
        let source_id = FileSourceId::new("local-test").unwrap();
        let fs = LocalFileSystem::new(source_id.clone(), &root.0).unwrap();
        let root_ref = DirectoryRef::root(source_id.clone());

        let page = fs
            .list(&root_ref, ListPageRequest::default(), &control())
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        let file = page.entries[0].reference.clone();
        let mut bytes = Vec::new();
        let outcome = fs
            .read(
                &file,
                ReadRequest {
                    offset: 0,
                    max_bytes: 4,
                },
                &control(),
                &mut |chunk| {
                    bytes.extend_from_slice(chunk);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(outcome.bytes_read, 4);
        assert_eq!(bytes, [1, 2, 3, 4]);
        assert!(matches!(
            fs.read(
                &file,
                ReadRequest {
                    offset: 0,
                    max_bytes: 3,
                },
                &control(),
                &mut |_| Ok(())
            ),
            Err(FileSystemError::ReadLimitExceeded { .. })
        ));

        let created = fs
            .mkdir(&root_ref, &EntryName::new("archive").unwrap(), &control())
            .unwrap();
        let renamed = fs
            .rename(&file, &EntryName::new("renamed.raw").unwrap(), &control())
            .unwrap();
        let moved = fs.move_entry(&renamed, &created, &control()).unwrap();
        fs.delete(&moved, false, &control()).unwrap();
        fs.delete(&FileRef::new(source_id, created.path), false, &control())
            .unwrap();
        assert!(
            fs.list(&root_ref, ListPageRequest::default(), &control())
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn write_new_atomic_publishes_a_new_file_without_overwrite() {
        let root = TestRoot::new();
        let source_id = FileSourceId::new("local-export-test").unwrap();
        let fs = LocalFileSystem::new(source_id.clone(), &root.0).unwrap();
        let parent = DirectoryRef::root(source_id.clone());
        let name = EntryName::new("intrinsics.yaml").unwrap();

        let mut write_calibration = |writer: &mut dyn Write| {
            writer
                .write_all(b"calibration")
                .map_err(FileSystemError::io)
        };
        let saved = fs
            .write_new_atomic(&parent, &name, &control(), &mut write_calibration)
            .unwrap();
        assert_eq!(
            saved,
            FileRef::new(source_id, SourcePath::new("intrinsics.yaml").unwrap())
        );
        assert_eq!(
            fs::read(root.0.join("intrinsics.yaml")).unwrap(),
            b"calibration"
        );
        let mut write_replacement = |writer: &mut dyn Write| {
            writer
                .write_all(b"replacement")
                .map_err(FileSystemError::io)
        };
        assert!(matches!(
            fs.write_new_atomic(&parent, &name, &control(), &mut write_replacement),
            Err(FileSystemError::AlreadyExists(_))
        ));
        assert_eq!(
            fs::read(root.0.join("intrinsics.yaml")).unwrap(),
            b"calibration"
        );
        assert!(fs::read_dir(&root.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".camera-toolbox-export-")
        }));
    }

    #[test]
    fn directory_pages_continue_without_scanning_whole_directory() {
        let root = TestRoot::new();
        fs::write(root.0.join("a.raw"), []).unwrap();
        fs::write(root.0.join("b.raw"), []).unwrap();
        let source_id = FileSourceId::new("local-test").unwrap();
        let fs = LocalFileSystem::new(source_id.clone(), &root.0).unwrap();
        let root_ref = DirectoryRef::root(source_id);
        let first = fs
            .list(
                &root_ref,
                ListPageRequest {
                    continuation: None,
                    limit: 1,
                },
                &control(),
            )
            .unwrap();
        assert_eq!(first.entries.len(), 1);
        let second = fs
            .list(
                &root_ref,
                ListPageRequest {
                    continuation: first.next,
                    limit: 1,
                },
                &control(),
            )
            .unwrap();
        assert_eq!(second.entries.len(), 1);
        assert!(second.next.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rename_and_delete_symlink_never_mutate_its_target() {
        use std::os::unix::fs::symlink;
        let root = TestRoot::new();
        let target = root.0.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep.raw"), [1_u8, 2]).unwrap();
        symlink("target", root.0.join("link")).unwrap();
        let source_id = FileSourceId::new("local-test").unwrap();
        let fs = LocalFileSystem::new(source_id.clone(), &root.0).unwrap();
        let link = FileRef::new(source_id, SourcePath::new("link").unwrap());

        let renamed = fs
            .rename(&link, &EntryName::new("renamed-link").unwrap(), &control())
            .unwrap();
        assert!(target.join("keep.raw").is_file());
        assert!(
            fs::symlink_metadata(root.0.join("renamed-link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs.delete(&renamed, true, &control()).unwrap();
        assert!(target.join("keep.raw").is_file());
        assert!(!root.0.join("renamed-link").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_hard_blocked() {
        use std::os::unix::fs::symlink;
        let root = TestRoot::new();
        symlink("/", root.0.join("escape")).unwrap();
        let source_id = FileSourceId::new("local-test").unwrap();
        let fs = LocalFileSystem::new(source_id.clone(), &root.0).unwrap();
        let escaped = FileRef::new(source_id, SourcePath::new("escape/etc").unwrap());
        assert!(matches!(
            fs.stat(&escaped, &control()),
            Err(FileSystemError::PathEscapesRoot(_))
        ));
    }
}
