//! SFTP 文件源实现；连接不持久化也不校验 host-key，只依赖 SSH 认证与远端权限。

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use camera_toolbox_app::{
    DirectoryRef, DumpCancellation, EntryName, FileEntry, FileKind, FileRef, FileSourceId,
    FileSystem, FileSystemCapabilities, FileSystemError, FileVersion, FsCancellation, FsControl,
    ListContinuation, ListPage, ListPageRequest, ReadOutcome, ReadRequest, RemoteOperationControl,
    RemoteTimeouts, SourcePath,
};

use crate::platforms::ssh_managed::{
    CredentialResolver, SshConnectionTarget, SshTransportError, SshTransportFactory,
    SshTransportSession, TransportDirEntry, TransportFileKind,
};

const MAX_LIST_PAGE_ENTRIES: usize = 1024;
const MAX_RECURSIVE_DELETE_ENTRIES: usize = 16_384;
const MAX_STAGING_NAME_ATTEMPTS: usize = 32;
const EXPORT_PRODUCER_ABORT: &str = "export producer rejected the staging write sink";

static NEXT_STAGING_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct SftpConnectionState {
    session: Option<Box<dyn SshTransportSession>>,
    canonical_root: Option<String>,
}

struct FsInterruptRegistration<'a>(&'a FsCancellation);

impl Drop for FsInterruptRegistration<'_> {
    fn drop(&mut self) {
        self.0.clear_interrupt();
    }
}

pub struct SftpFileSystem {
    source_id: FileSourceId,
    configured_root: String,
    target: SshConnectionTarget,
    credential_ref: String,
    credentials: Arc<dyn CredentialResolver>,
    transport: Arc<dyn SshTransportFactory>,
    state: Mutex<SftpConnectionState>,
}

impl SftpFileSystem {
    /// 创建 SFTP 挂载；未 pin 主机密钥的挂载仅允许浏览和读取。
    ///
    /// # Errors
    ///
    /// 远端根不是安全绝对路径或凭据引用为空时返回错误。
    pub fn new(
        source_id: FileSourceId,
        configured_root: impl Into<String>,
        mut target: SshConnectionTarget,
        credential_ref: impl Into<String>,
        credentials: Arc<dyn CredentialResolver>,
        transport: Arc<dyn SshTransportFactory>,
    ) -> Result<Self, FileSystemError> {
        let configured_root = configured_root.into();
        if !configured_root.starts_with('/')
            || configured_root.contains('\0')
            || configured_root
                .split('/')
                .any(|component| component == "..")
        {
            return Err(FileSystemError::PathEscapesRoot(configured_root));
        }
        let credential_ref = credential_ref.into();
        if credential_ref.trim().is_empty() {
            return Err(FileSystemError::Remote(
                "credential reference must not be empty".to_owned(),
            ));
        }
        target.expected_host_key = None;
        Ok(Self {
            source_id,
            configured_root,
            target,
            credential_ref,
            credentials,
            transport,
            state: Mutex::new(SftpConnectionState::default()),
        })
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

    fn with_session<T>(
        &self,
        control: &FsControl,
        operation: impl FnOnce(
            &mut dyn SshTransportSession,
            &str,
            &RemoteOperationControl,
        ) -> Result<T, SshTransportError>,
    ) -> Result<T, FileSystemError> {
        control.checkpoint()?;
        let remaining = control.remaining();
        if remaining.is_zero() {
            return Err(FileSystemError::TimedOut);
        }
        let remote_cancel = DumpCancellation::default();
        let cancel_bridge = remote_cancel.clone();
        control
            .cancellation
            .register_interrupt(Arc::new(move || cancel_bridge.cancel()));
        let _interrupt_registration = FsInterruptRegistration(&control.cancellation);
        let remote_control = RemoteOperationControl::new(
            RemoteTimeouts {
                connect: remaining.min(Duration::from_secs(10)),
                idle: remaining.min(Duration::from_secs(10)),
                overall: remaining,
            },
            remote_cancel,
        )
        .map_err(|error| FileSystemError::Remote(error.to_string()))?;

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.session.is_none() {
            let credential = self
                .credentials
                .resolve(&self.credential_ref)
                .map_err(FileSystemError::Remote)?;
            state.session = Some(
                self.transport
                    .connect(&self.target, credential, &remote_control)
                    .map_err(map_transport)?,
            );
            state.canonical_root = None;
        }
        if state.canonical_root.is_none() {
            let root = state
                .session
                .as_mut()
                .expect("session was installed")
                .canonicalize(&self.configured_root, &remote_control)
                .map_err(map_transport)?;
            if !root.starts_with('/') {
                state.session = None;
                return Err(FileSystemError::PathEscapesRoot(root));
            }
            state.canonical_root = Some(trim_remote_root(root));
        }
        let root = state
            .canonical_root
            .as_ref()
            .expect("canonical root was installed")
            .clone();
        let result = operation(
            state
                .session
                .as_mut()
                .expect("session was installed")
                .as_mut(),
            &root,
            &remote_control,
        );
        if result.as_ref().is_err_and(transport_requires_reconnect) {
            state.session = None;
            state.canonical_root = None;
        }
        result.map_err(map_transport)
    }

    fn canonical_directory(
        session: &mut dyn SshTransportSession,
        root: &str,
        directory: &DirectoryRef,
        control: &RemoteOperationControl,
    ) -> Result<String, SshTransportError> {
        let candidate = join_remote(root, &directory.path);
        let canonical = session.canonicalize(&candidate, control)?;
        if is_within_remote_root(root, &canonical) {
            Ok(trim_remote_root(canonical))
        } else {
            Err(SshTransportError::PermissionDenied(
                "path escapes mounted SFTP root".to_owned(),
            ))
        }
    }

    /// 变更叶节点只规范化父目录；LSTAT 决定叶类型，绝不跟随 symlink 变更目标。
    fn leaf_path(
        session: &mut dyn SshTransportSession,
        root: &str,
        reference: &FileRef,
        control: &RemoteOperationControl,
    ) -> Result<String, SshTransportError> {
        let canonical_parent =
            Self::canonical_directory(session, root, &reference.parent(), control)?;
        let name = reference.path.file_name().ok_or_else(|| {
            SshTransportError::PermissionDenied("mount root is not an entry".into())
        })?;
        Ok(format!("{canonical_parent}/{name}"))
    }

    fn destination_path(
        session: &mut dyn SshTransportSession,
        root: &str,
        directory: &DirectoryRef,
        name: &EntryName,
        control: &RemoteOperationControl,
    ) -> Result<String, SshTransportError> {
        let parent = Self::canonical_directory(session, root, directory, control)?;
        Ok(format!("{parent}/{name}"))
    }

    fn staging_path(destination: &str) -> String {
        let parent =
            destination.rsplit_once('/').map_or(
                "/",
                |(parent, _)| if parent.is_empty() { "/" } else { parent },
            );
        let id = NEXT_STAGING_FILE_ID.fetch_add(1, Ordering::Relaxed);
        if parent == "/" {
            format!("/.camera-toolbox-export-{}-{id}.tmp", std::process::id())
        } else {
            format!(
                "{parent}/.camera-toolbox-export-{}-{id}.tmp",
                std::process::id()
            )
        }
    }

    fn remote_entry(
        &self,
        directory: &DirectoryRef,
        entry: TransportDirEntry,
    ) -> Result<FileEntry, FileSystemError> {
        let name = entry
            .path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .ok_or_else(|| FileSystemError::Remote("remote entry has no basename".to_owned()))?;
        let name =
            EntryName::new(name).map_err(|error| FileSystemError::Remote(error.to_string()))?;
        Ok(FileEntry {
            reference: FileRef::new(self.source_id.clone(), directory.path.join(&name)),
            name,
            kind: map_kind(entry.kind),
            version: FileVersion {
                size: entry.size,
                modified_millis: Some(entry.modified_seconds.saturating_mul(1000)),
            },
        })
    }
}

impl FileSystem for SftpFileSystem {
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
        self.ensure_source(&directory.source_id)?;
        let limit = page.limit.min(MAX_LIST_PAGE_ENTRIES);
        if limit == 0 {
            return Ok(ListPage {
                entries: Vec::new(),
                next: None,
            });
        }
        let continuation = page
            .continuation
            .as_ref()
            .map(|token| {
                token
                    .as_str()
                    .strip_prefix("sftp:")
                    .ok_or(FileSystemError::InvalidContinuation)
            })
            .transpose()?;
        let transport_page = self.with_session(control, |session, root, remote_control| {
            let path = Self::canonical_directory(session, root, directory, remote_control)?;
            session.read_dir_page(&path, continuation, limit, remote_control)
        })?;
        let mut entries: Vec<_> = transport_page
            .entries
            .into_iter()
            .map(|entry| self.remote_entry(directory, entry))
            .collect::<Result<_, _>>()?;
        entries.sort_by(|left, right| {
            let left_dir = left.kind == FileKind::Directory;
            let right_dir = right.kind == FileKind::Directory;
            right_dir
                .cmp(&left_dir)
                .then_with(|| left.name.as_str().cmp(right.name.as_str()))
        });
        Ok(ListPage {
            entries,
            next: transport_page
                .continuation
                .map(|token| ListContinuation::new(format!("sftp:{token}"))),
        })
    }

    fn stat(&self, reference: &FileRef, control: &FsControl) -> Result<FileEntry, FileSystemError> {
        self.ensure_source(&reference.source_id)?;
        let metadata = self.with_session(control, |session, root, remote_control| {
            let path = Self::leaf_path(session, root, reference, remote_control)?;
            session.metadata(&path, remote_control)
        })?;
        self.remote_entry(&reference.parent(), metadata)
    }

    fn read(
        &self,
        reference: &FileRef,
        request: ReadRequest,
        control: &FsControl,
        consume: &mut dyn FnMut(&[u8]) -> Result<(), FileSystemError>,
    ) -> Result<ReadOutcome, FileSystemError> {
        self.ensure_source(&reference.source_id)?;
        if request.offset != 0 {
            return Err(FileSystemError::Unsupported);
        }
        let mut callback_error = None;
        let result = self.with_session(control, |session, root, remote_control| {
            let lexical = join_remote(root, &reference.path);
            let path = session.canonicalize(&lexical, remote_control)?;
            if !is_within_remote_root(root, &path) {
                return Err(SshTransportError::PermissionDenied(
                    "read path escapes mounted SFTP root".to_owned(),
                ));
            }
            let before = session.stat(&path, remote_control)?;
            if before.size > request.max_bytes {
                return Err(SshTransportError::ReadLimitExceeded {
                    requested: before.size,
                    limit: request.max_bytes,
                });
            }
            let bytes_read = session.read_file(&path, remote_control, &mut |chunk| {
                if let Err(error) = consume(chunk) {
                    callback_error = Some(error);
                    return Err(SshTransportError::Transport(
                        "filesystem read consumer rejected chunk".to_owned(),
                    ));
                }
                Ok(())
            })?;
            let after = session.stat(&path, remote_control)?;
            if before.size != after.size || before.modified_seconds != after.modified_seconds {
                return Err(SshTransportError::ChangedDuringRead(path));
            }
            Ok((bytes_read, after))
        });
        if let Some(error) = callback_error {
            return Err(error);
        }
        let (bytes_read, stat) = result?;
        if bytes_read != stat.size {
            return Err(FileSystemError::ChangedDuringRead(
                reference.path.as_str().to_owned(),
            ));
        }
        Ok(ReadOutcome {
            bytes_read,
            source_version: FileVersion {
                size: stat.size,
                modified_millis: Some(stat.modified_seconds.saturating_mul(1000)),
            },
        })
    }

    fn write_new_atomic(
        &self,
        parent: &DirectoryRef,
        name: &EntryName,
        control: &FsControl,
        produce: &mut dyn FnMut(&mut dyn std::io::Write) -> Result<(), FileSystemError>,
    ) -> Result<FileRef, FileSystemError> {
        self.ensure_source(&parent.source_id)?;
        let mut producer_error = None;
        let result = self.with_session(control, |session, root, remote_control| {
            let destination = Self::destination_path(session, root, parent, name, remote_control)?;
            ensure_remote_destination_absent(session, &destination, remote_control)?;

            let mut staging = None;
            for _ in 0..MAX_STAGING_NAME_ATTEMPTS {
                let candidate = Self::staging_path(&destination);
                let mut transport_producer = |writer: &mut dyn std::io::Write| match produce(writer)
                {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        producer_error = Some(error);
                        Err(SshTransportError::HelperProtocol(
                            EXPORT_PRODUCER_ABORT.to_owned(),
                        ))
                    }
                };
                match session.write_file_new(&candidate, remote_control, &mut transport_producer) {
                    Ok(()) => {
                        staging = Some(candidate);
                        break;
                    }
                    Err(SshTransportError::AlreadyExists(_)) => continue,
                    Err(error) => {
                        let _ = session.remove_file(&candidate, remote_control);
                        return Err(error);
                    }
                }
            }
            let staging = staging.ok_or_else(|| {
                SshTransportError::AlreadyExists(
                    "could not allocate remote staging path".to_owned(),
                )
            })?;

            let publish_result = (|| {
                ensure_remote_destination_absent(session, &destination, remote_control)?;
                session.rename(&staging, &destination, remote_control)
            })();
            if let Err(error) = publish_result {
                let _ = session.remove_file(&staging, remote_control);
                return Err(error);
            }
            Ok(())
        });
        match result {
            Err(FileSystemError::Remote(reason)) if reason == EXPORT_PRODUCER_ABORT => {
                Err(producer_error.unwrap_or_else(|| {
                    FileSystemError::Remote("export producer aborted without an error".to_owned())
                }))
            }
            Err(error) => Err(error),
            Ok(()) => {
                if let Some(error) = producer_error {
                    return Err(error);
                }
                Ok(FileRef::new(self.source_id.clone(), parent.path.join(name)))
            }
        }
    }

    fn mkdir(
        &self,
        parent: &DirectoryRef,
        name: &EntryName,
        control: &FsControl,
    ) -> Result<DirectoryRef, FileSystemError> {
        self.ensure_source(&parent.source_id)?;
        self.with_session(control, |session, root, remote_control| {
            let path = Self::destination_path(session, root, parent, name, remote_control)?;
            ensure_remote_destination_absent(session, &path, remote_control)?;
            session.mkdir(&path, remote_control)
        })?;
        Ok(parent.child(name))
    }

    fn rename(
        &self,
        reference: &FileRef,
        new_name: &EntryName,
        control: &FsControl,
    ) -> Result<FileRef, FileSystemError> {
        self.ensure_source(&reference.source_id)?;
        let parent = reference.parent();
        self.with_session(control, |session, root, remote_control| {
            let source = Self::leaf_path(session, root, reference, remote_control)?;
            let destination =
                Self::destination_path(session, root, &parent, new_name, remote_control)?;
            ensure_remote_destination_absent(session, &destination, remote_control)?;
            session.rename(&source, &destination, remote_control)
        })?;
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
        self.ensure_source(&reference.source_id)?;
        self.ensure_source(&destination.source_id)?;
        let name = EntryName::new(reference.path.file_name().unwrap_or_default())
            .map_err(|error| FileSystemError::Remote(error.to_string()))?;
        self.with_session(control, |session, root, remote_control| {
            let source = Self::leaf_path(session, root, reference, remote_control)?;
            let target = Self::destination_path(session, root, destination, &name, remote_control)?;
            ensure_remote_destination_absent(session, &target, remote_control)?;
            session.rename(&source, &target, remote_control)
        })?;
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
        self.ensure_source(&reference.source_id)?;
        self.with_session(control, |session, root, remote_control| {
            let path = Self::leaf_path(session, root, reference, remote_control)?;
            let metadata = session.metadata(&path, remote_control)?;
            match metadata.kind {
                TransportFileKind::Directory if recursive => {
                    remove_remote_tree(session, &path, remote_control)
                }
                TransportFileKind::Directory => session.remove_dir(&path, remote_control),
                _ => session.remove_file(&path, remote_control),
            }
        })
    }
}

fn remove_remote_tree(
    session: &mut dyn SshTransportSession,
    root: &str,
    control: &RemoteOperationControl,
) -> Result<(), SshTransportError> {
    let mut stack = vec![(root.to_owned(), false)];
    let mut observed = 0_usize;
    while let Some((path, visited)) = stack.pop() {
        observed = observed.saturating_add(1);
        if observed > MAX_RECURSIVE_DELETE_ENTRIES {
            return Err(SshTransportError::HelperProtocol(
                "recursive delete exceeds hard entry limit".to_owned(),
            ));
        }
        if visited {
            session.remove_dir(&path, control)?;
            continue;
        }
        stack.push((path.clone(), true));
        for child in session.read_dir(&path, control)? {
            if child.kind == TransportFileKind::Directory {
                stack.push((child.path, false));
            } else {
                session.remove_file(&child.path, control)?;
            }
        }
    }
    Ok(())
}

fn ensure_remote_destination_absent(
    session: &mut dyn SshTransportSession,
    destination: &str,
    control: &RemoteOperationControl,
) -> Result<(), SshTransportError> {
    match session.metadata(destination, control) {
        Err(SshTransportError::NotFound(_)) => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(SshTransportError::AlreadyExists(destination.to_owned())),
    }
}

fn trim_remote_root(mut path: String) -> String {
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

fn join_remote(root: &str, path: &SourcePath) -> String {
    if path.is_root() {
        root.to_owned()
    } else if root == "/" {
        format!("/{}", path.as_str())
    } else {
        format!("{root}/{}", path.as_str())
    }
}

fn is_within_remote_root(root: &str, path: &str) -> bool {
    path == root
        || root == "/"
        || path
            .strip_prefix(root)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

fn map_kind(kind: TransportFileKind) -> FileKind {
    match kind {
        TransportFileKind::File => FileKind::File,
        TransportFileKind::Directory => FileKind::Directory,
        TransportFileKind::Symlink => FileKind::Symlink,
        TransportFileKind::Other => FileKind::Other,
    }
}

fn transport_requires_reconnect(error: &SshTransportError) -> bool {
    matches!(
        error,
        SshTransportError::Disconnected(_)
            | SshTransportError::Transport(_)
            | SshTransportError::TimedOut
    )
}

fn map_transport(error: SshTransportError) -> FileSystemError {
    match error {
        SshTransportError::HostKeyMismatch => {
            FileSystemError::PermissionDenied("SSH host key mismatch".to_owned())
        }
        SshTransportError::AuthenticationFailed => {
            FileSystemError::PermissionDenied("SSH authentication failed".to_owned())
        }
        SshTransportError::Cancelled => FileSystemError::Cancelled,
        SshTransportError::TimedOut => FileSystemError::TimedOut,
        SshTransportError::NotFound(path) => FileSystemError::NotFound(path),
        SshTransportError::PermissionDenied(reason) => FileSystemError::PermissionDenied(reason),
        SshTransportError::Disconnected(reason) => FileSystemError::Disconnected(reason),
        SshTransportError::AlreadyExists(path) => FileSystemError::AlreadyExists(path),
        SshTransportError::ReadLimitExceeded { requested, limit } => {
            FileSystemError::ReadLimitExceeded { requested, limit }
        }
        SshTransportError::ChangedDuringRead(path) => FileSystemError::ChangedDuringRead(path),
        SshTransportError::InvalidContinuation => FileSystemError::InvalidContinuation,
        SshTransportError::Unsupported => FileSystemError::Unsupported,
        SshTransportError::Transport(reason) | SshTransportError::HelperProtocol(reason) => {
            FileSystemError::Remote(reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, io::Write};

    use super::*;
    use camera_toolbox_app::RemoteFileStat;

    use crate::platforms::ssh_managed::{MemoryRemoteFile, MemorySshTransport};

    const HOST_KEY: &str = "ssh-ed25519 AAAATEST";

    fn control() -> FsControl {
        FsControl::with_timeout(Duration::from_secs(5))
    }

    fn remote_file(path: &str, bytes: &[u8]) -> MemoryRemoteFile {
        MemoryRemoteFile {
            bytes: bytes.to_vec(),
            stats: VecDeque::from([RemoteFileStat {
                path: path.to_owned(),
                size: bytes.len() as u64,
                modified_seconds: 7,
                producer_marker: None,
                sha256: None,
            }]),
        }
    }

    fn filesystem(memory: Arc<MemorySshTransport>, stored_key: Option<&str>) -> SftpFileSystem {
        SftpFileSystem::new(
            FileSourceId::new("remote-test").unwrap(),
            "/opt",
            SshConnectionTarget {
                host: "camera.test".to_owned(),
                port: 22,
                username: "root".to_owned(),
                expected_host_key: stored_key.map(str::to_owned),
                command_subsystem: None,
                remote_event_subsystem: None,
            },
            "session:test",
            memory.clone(),
            memory,
        )
        .unwrap()
    }

    #[test]
    fn sftp_filesystem_supports_paged_bounded_crud() {
        let memory = Arc::new(MemorySshTransport::new(HOST_KEY));
        memory.allow_credential("session:test");
        memory.insert_file("/opt/a.raw", remote_file("/opt/a.raw", &[1, 2, 3, 4]));
        memory.insert_file("/opt/b.raw", remote_file("/opt/b.raw", &[5, 6]));
        let fs = filesystem(memory.clone(), Some(HOST_KEY));
        let root = DirectoryRef::root(FileSourceId::new("remote-test").unwrap());

        let first = fs
            .list(
                &root,
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
                &root,
                ListPageRequest {
                    continuation: first.next,
                    limit: 1,
                },
                &control(),
            )
            .unwrap();
        assert_eq!(second.entries.len(), 1);
        assert!(second.next.is_none());

        let file = FileRef::new(
            FileSourceId::new("remote-test").unwrap(),
            SourcePath::new("a.raw").unwrap(),
        );
        let mut bytes = Vec::new();
        fs.read(
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
            Err(FileSystemError::ReadLimitExceeded {
                requested: 4,
                limit: 3
            })
        ));

        let archive = fs
            .mkdir(&root, &EntryName::new("archive").unwrap(), &control())
            .unwrap();
        let renamed = fs
            .rename(&file, &EntryName::new("renamed.raw").unwrap(), &control())
            .unwrap();
        let moved = fs.move_entry(&renamed, &archive, &control()).unwrap();
        assert!(memory.contains_path("/opt/archive/renamed.raw"));
        fs.delete(&moved, false, &control()).unwrap();
        fs.delete(
            &FileRef::new(FileSourceId::new("remote-test").unwrap(), archive.path),
            false,
            &control(),
        )
        .unwrap();
        assert!(!memory.contains_path("/opt/archive"));
    }

    #[test]
    fn unpinned_sftp_writes_new_artifacts_without_overwrite() {
        let memory = Arc::new(MemorySshTransport::new(HOST_KEY));
        memory.allow_credential("session:test");
        memory.insert_directory("/opt");
        let fs = filesystem(memory.clone(), None);
        let root = DirectoryRef::root(FileSourceId::new("remote-test").unwrap());
        let name = EntryName::new("intrinsics.yaml").unwrap();

        let mut write_calibration = |writer: &mut dyn Write| {
            writer
                .write_all(b"calibration")
                .map_err(FileSystemError::io)
        };
        let saved = fs
            .write_new_atomic(&root, &name, &control(), &mut write_calibration)
            .unwrap();
        assert_eq!(
            saved,
            FileRef::new(
                root.source_id.clone(),
                SourcePath::new("intrinsics.yaml").unwrap()
            )
        );
        assert!(memory.contains_path("/opt/intrinsics.yaml"));
        assert_eq!(
            memory.file_bytes("/opt/intrinsics.yaml"),
            Some(b"calibration".to_vec())
        );
        let mut write_replacement = |writer: &mut dyn Write| {
            writer
                .write_all(b"replacement")
                .map_err(FileSystemError::io)
        };
        assert!(matches!(
            fs.write_new_atomic(&root, &name, &control(), &mut write_replacement),
            Err(FileSystemError::AlreadyExists(_))
        ));

        let page = fs
            .list(&root, ListPageRequest::default(), &control())
            .unwrap();
        assert!(
            page.entries
                .iter()
                .all(|entry| !entry.name.as_str().starts_with(".camera-toolbox-export-"))
        );
    }

    #[test]
    fn stale_or_missing_host_key_pin_does_not_reduce_sftp_permissions() {
        let memory = Arc::new(MemorySshTransport::new(HOST_KEY));
        memory.allow_credential("session:test");
        memory.insert_directory("/opt");
        let stale = filesystem(memory.clone(), Some("stale-host-key"));
        let unpinned = filesystem(memory, None);

        assert_eq!(stale.capabilities(), FileSystemCapabilities::READ_WRITE);
        assert_eq!(unpinned.capabilities(), FileSystemCapabilities::READ_WRITE);
    }

    #[test]
    fn canonical_path_outside_mounted_root_is_permission_denied() {
        let memory = Arc::new(MemorySshTransport::new(HOST_KEY));
        memory.allow_credential("session:test");
        memory.insert_directory("/opt");
        memory.set_canonical_path("/opt/link", "/etc");
        let fs = filesystem(memory, Some(HOST_KEY));
        let error = fs
            .list(
                &DirectoryRef::new(
                    FileSourceId::new("remote-test").unwrap(),
                    SourcePath::directory("link").unwrap(),
                ),
                ListPageRequest::default(),
                &control(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            FileSystemError::PermissionDenied(reason) if reason.contains("escapes mounted SFTP root")
        ));
    }

    #[test]
    fn changed_host_key_is_ignored() {
        let memory = Arc::new(MemorySshTransport::new(HOST_KEY));
        memory.allow_credential("session:test");
        memory.insert_directory("/opt");
        let fs = filesystem(memory, Some("ssh-ed25519 DIFFERENT"));
        fs.list(
            &DirectoryRef::root(FileSourceId::new("remote-test").unwrap()),
            ListPageRequest::default(),
            &control(),
        )
        .unwrap();
    }
}
