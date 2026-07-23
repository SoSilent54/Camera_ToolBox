//! SFTP 文件源实现；连接不持久化也不校验 host-key，只依赖 SSH 认证与远端权限。

use std::{
    sync::{
        Arc, Condvar, Mutex,
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
const SFTP_SESSION_POOL_SIZE: usize = 8;

static NEXT_STAGING_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct SftpConnectionState {
    session: Option<Box<dyn SshTransportSession>>,
    canonical_root: Option<String>,
}

struct SftpSessionPool {
    available: Mutex<Vec<SftpConnectionState>>,
    ready: Condvar,
}

impl SftpSessionPool {
    fn new(capacity: usize) -> Self {
        let mut available = Vec::with_capacity(capacity);
        available.resize_with(capacity, SftpConnectionState::default);
        Self {
            available: Mutex::new(available),
            ready: Condvar::new(),
        }
    }

    fn checkout(
        self: &Arc<Self>,
        control: &FsControl,
    ) -> Result<SftpSessionLease, FileSystemError> {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            control.checkpoint()?;
            if let Some(state) = available.pop() {
                return Ok(SftpSessionLease {
                    pool: Arc::clone(self),
                    state: Some(state),
                });
            }
            let remaining = control.remaining();
            if remaining.is_zero() {
                return Err(FileSystemError::TimedOut);
            }
            let (next, timeout) = self
                .ready
                .wait_timeout(available, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            available = next;
            if timeout.timed_out() && available.is_empty() {
                return Err(FileSystemError::TimedOut);
            }
        }
    }

    fn wake_waiters(&self) {
        let guard = self
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(guard);
        self.ready.notify_all();
    }
}

struct SftpSessionLease {
    pool: Arc<SftpSessionPool>,
    state: Option<SftpConnectionState>,
}

impl SftpSessionLease {
    fn state_mut(&mut self) -> &mut SftpConnectionState {
        self.state.as_mut().expect("leased SFTP state is present")
    }
}

impl Drop for SftpSessionLease {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let mut available = self
            .pool
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        available.push(state);
        drop(available);
        self.pool.ready.notify_one();
    }
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
    control_sessions: Arc<SftpSessionPool>,
    read_sessions: Arc<SftpSessionPool>,
}

impl SftpFileSystem {
    /// host-key 是否校验由传入 target 决定；密码 GUI 连接不持久化 host-key。
    ///
    /// # Errors
    ///
    /// 远端根不是安全绝对路径或凭据引用为空时返回错误。
    pub fn new(
        source_id: FileSourceId,
        configured_root: impl Into<String>,
        target: SshConnectionTarget,
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
        Ok(Self {
            source_id,
            configured_root,
            target,
            credential_ref,
            credentials,
            transport,
            control_sessions: Arc::new(SftpSessionPool::new(1)),
            read_sessions: Arc::new(SftpSessionPool::new(SFTP_SESSION_POOL_SIZE)),
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

    fn remote_control<'a>(
        control: &'a FsControl,
        wake_sessions: Option<Arc<SftpSessionPool>>,
    ) -> Result<(RemoteOperationControl, FsInterruptRegistration<'a>), FileSystemError> {
        control.checkpoint()?;
        let remaining = control.remaining();
        if remaining.is_zero() {
            return Err(FileSystemError::TimedOut);
        }
        let remote_cancel = DumpCancellation::default();
        let cancel_bridge = remote_cancel.clone();
        control.cancellation.register_interrupt(Arc::new(move || {
            cancel_bridge.cancel();
            if let Some(sessions) = &wake_sessions {
                sessions.wake_waiters();
            }
        }));
        let interrupt_registration = FsInterruptRegistration(&control.cancellation);
        let remote_control = RemoteOperationControl::new(
            RemoteTimeouts {
                connect: remaining.min(Duration::from_secs(10)),
                idle: remaining.min(Duration::from_secs(10)),
                overall: remaining,
            },
            remote_cancel,
        )
        .map_err(|error| FileSystemError::Remote(error.to_string()))?;
        Ok((remote_control, interrupt_registration))
    }

    fn operate_session<T>(
        &self,
        state: &mut SftpConnectionState,
        remote_control: &RemoteOperationControl,
        operation: impl FnOnce(
            &mut dyn SshTransportSession,
            &str,
            &RemoteOperationControl,
        ) -> Result<T, SshTransportError>,
    ) -> Result<T, FileSystemError> {
        if state.session.is_none() {
            let credential = self
                .credentials
                .resolve(&self.credential_ref)
                .map_err(FileSystemError::Remote)?;
            state.session = Some(
                self.transport
                    .connect(&self.target, credential, remote_control)
                    .map_err(map_transport)?,
            );
            state.canonical_root = None;
        }
        let session = state.session.as_mut().expect("session was installed");
        session.prepare_operation(remote_control);
        if state.canonical_root.is_none() {
            let root = session.canonicalize(&self.configured_root, remote_control);
            match root {
                Ok(root) if root.starts_with('/') => {
                    state.canonical_root = Some(trim_remote_root(root));
                }
                Ok(root) => {
                    state.session = None;
                    return Err(FileSystemError::PathEscapesRoot(root));
                }
                Err(error) => {
                    if transport_requires_reconnect(&error) {
                        state.session = None;
                    }
                    return Err(map_transport(error));
                }
            }
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
            remote_control,
        );
        if result.as_ref().is_err_and(transport_requires_reconnect) {
            state.session = None;
            state.canonical_root = None;
        }
        result.map_err(map_transport)
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
        let control_sessions = Arc::clone(&self.control_sessions);
        let (remote_control, _interrupt_registration) =
            Self::remote_control(control, Some(control_sessions.clone()))?;
        let mut lease = control_sessions.checkout(control)?;
        self.operate_session(lease.state_mut(), &remote_control, operation)
    }

    fn with_read_session<T>(
        &self,
        control: &FsControl,
        operation: impl FnOnce(
            &mut dyn SshTransportSession,
            &str,
            &RemoteOperationControl,
        ) -> Result<T, SshTransportError>,
    ) -> Result<T, FileSystemError> {
        let (remote_control, _interrupt_registration) =
            Self::remote_control(control, Some(Arc::clone(&self.read_sessions)))?;
        let mut lease = self.read_sessions.checkout(control)?;
        self.operate_session(lease.state_mut(), &remote_control, operation)
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
        let result = self.with_read_session(control, |session, root, remote_control| {
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
        SshTransportError::Cancelled
            | SshTransportError::Disconnected(_)
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
    use std::{
        collections::VecDeque,
        io::Write,
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Instant,
    };

    use super::*;
    use camera_toolbox_app::RemoteFileStat;

    use crate::platforms::ssh_managed::{
        MemoryRemoteFile, MemorySshTransport, SshCredential, SshTransportFactory,
        SshTransportSession, TransportCommandOutput, TransportDirEntry, TransportListPage,
    };

    struct BlockingReadProbe {
        entered: AtomicUsize,
        list_entered: AtomicUsize,
        read_calls: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        connections: AtomicUsize,
        block_lists: AtomicBool,
        block_second_read: AtomicBool,
        second_read_entered: AtomicBool,
        released: AtomicBool,
        mutex: Mutex<()>,
        changed: Condvar,
    }

    impl BlockingReadProbe {
        fn new() -> Self {
            Self {
                entered: AtomicUsize::new(0),
                list_entered: AtomicUsize::new(0),
                read_calls: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                connections: AtomicUsize::new(0),
                block_lists: AtomicBool::new(false),
                block_second_read: AtomicBool::new(false),
                second_read_entered: AtomicBool::new(false),
                released: AtomicBool::new(false),
                mutex: Mutex::new(()),
                changed: Condvar::new(),
            }
        }

        fn wait_for_entered(&self, expected: usize) -> bool {
            self.wait_for_counter(&self.entered, expected)
        }

        fn wait_for_list_entered(&self, expected: usize) -> bool {
            self.wait_for_counter(&self.list_entered, expected)
        }

        fn wait_for_second_read(&self) -> bool {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut guard = self
                .mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !self.second_read_entered.load(Ordering::Acquire) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                let (next, timeout) = self.changed.wait_timeout(guard, remaining).unwrap();
                guard = next;
                if timeout.timed_out() {
                    return self.second_read_entered.load(Ordering::Acquire);
                }
            }
            true
        }

        fn wait_for_counter(&self, counter: &AtomicUsize, expected: usize) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut guard = self
                .mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while counter.load(Ordering::Acquire) < expected {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                let (next, timeout) = self.changed.wait_timeout(guard, remaining).unwrap();
                guard = next;
                if timeout.timed_out() {
                    return counter.load(Ordering::Acquire) >= expected;
                }
            }
            true
        }

        fn release(&self) {
            self.released.store(true, Ordering::Release);
            self.changed.notify_all();
        }
    }

    struct ActiveRead(Arc<BlockingReadProbe>);

    impl Drop for ActiveRead {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::AcqRel);
        }
    }

    struct BlockingReadTransport {
        probe: Arc<BlockingReadProbe>,
    }

    impl SshTransportFactory for BlockingReadTransport {
        fn connect(
            &self,
            _target: &SshConnectionTarget,
            _credential: SshCredential,
            _control: &RemoteOperationControl,
        ) -> Result<Box<dyn SshTransportSession>, SshTransportError> {
            let session_id = self.probe.connections.fetch_add(1, Ordering::AcqRel);
            Ok(Box::new(BlockingReadSession {
                probe: Arc::clone(&self.probe),
                session_id,
                operation_cancellation: DumpCancellation::default(),
            }))
        }
    }

    struct BlockingReadSession {
        probe: Arc<BlockingReadProbe>,
        session_id: usize,
        operation_cancellation: DumpCancellation,
    }

    impl SshTransportSession for BlockingReadSession {
        fn prepare_operation(&mut self, control: &RemoteOperationControl) {
            self.operation_cancellation = control.cancellation.clone();
        }
        fn execute_argv(
            &mut self,
            _argv: &[String],
            _output_limit: usize,
            _control: &RemoteOperationControl,
        ) -> Result<TransportCommandOutput, SshTransportError> {
            Err(SshTransportError::Unsupported)
        }

        fn canonicalize(
            &mut self,
            path: &str,
            _control: &RemoteOperationControl,
        ) -> Result<String, SshTransportError> {
            Ok(path.to_owned())
        }

        fn stat(
            &mut self,
            path: &str,
            _control: &RemoteOperationControl,
        ) -> Result<RemoteFileStat, SshTransportError> {
            Ok(RemoteFileStat {
                path: path.to_owned(),
                size: 1,
                modified_seconds: 7,
                producer_marker: None,
                sha256: None,
            })
        }

        fn read_dir(
            &mut self,
            _root: &str,
            _control: &RemoteOperationControl,
        ) -> Result<Vec<TransportDirEntry>, SshTransportError> {
            Err(SshTransportError::Unsupported)
        }
        fn read_dir_page(
            &mut self,
            root: &str,
            continuation: Option<&str>,
            _limit: usize,
            control: &RemoteOperationControl,
        ) -> Result<TransportListPage, SshTransportError> {
            if self.probe.block_lists.load(Ordering::Acquire) {
                self.probe.list_entered.fetch_add(1, Ordering::AcqRel);
                self.probe.changed.notify_all();
                let mut guard = self
                    .probe
                    .mutex
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while !self.probe.released.load(Ordering::Acquire) {
                    if control.cancellation.is_cancelled() {
                        return Err(SshTransportError::Cancelled);
                    }
                    if control.deadline_expired() {
                        return Err(SshTransportError::TimedOut);
                    }
                    let (next, _) = self
                        .probe
                        .changed
                        .wait_timeout(guard, Duration::from_millis(20))
                        .unwrap();
                    guard = next;
                }
            }
            if control.cancellation.is_cancelled() {
                return Err(SshTransportError::Cancelled);
            }
            let continuation_token = format!("blocking:{}:{root}", self.session_id);
            let entry = |name: &str| TransportDirEntry {
                path: format!("{root}/{name}"),
                size: 1,
                modified_seconds: 7,
                kind: TransportFileKind::File,
            };
            match continuation {
                None => Ok(TransportListPage {
                    entries: vec![entry("a.raw")],
                    continuation: Some(continuation_token),
                }),
                Some(token) if token == continuation_token => Ok(TransportListPage {
                    entries: vec![entry("b.raw")],
                    continuation: None,
                }),
                Some(_) => Err(SshTransportError::InvalidContinuation),
            }
        }

        fn metadata(
            &mut self,
            _path: &str,
            _control: &RemoteOperationControl,
        ) -> Result<TransportDirEntry, SshTransportError> {
            Err(SshTransportError::Unsupported)
        }

        fn event_paths(
            &mut self,
            _root: &str,
            _glob: &str,
            _control: &RemoteOperationControl,
        ) -> Result<Vec<String>, SshTransportError> {
            Err(SshTransportError::Unsupported)
        }

        fn read_file(
            &mut self,
            _path: &str,
            control: &RemoteOperationControl,
            consume: &mut dyn FnMut(&[u8]) -> Result<(), SshTransportError>,
        ) -> Result<u64, SshTransportError> {
            if control.cancellation.is_cancelled() {
                return Err(SshTransportError::Cancelled);
            }
            let call = self.probe.read_calls.fetch_add(1, Ordering::AcqRel) + 1;
            if self.probe.block_second_read.load(Ordering::Acquire) && call == 1 {
                consume(&[7])?;
                return Ok(1);
            }
            if self.probe.block_second_read.load(Ordering::Acquire) && call == 2 {
                self.probe
                    .second_read_entered
                    .store(true, Ordering::Release);
                self.probe.changed.notify_all();
                let mut guard = self
                    .probe
                    .mutex
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while !self.probe.released.load(Ordering::Acquire) {
                    if self.operation_cancellation.is_cancelled() {
                        return Err(SshTransportError::Cancelled);
                    }
                    if control.deadline_expired() {
                        return Err(SshTransportError::TimedOut);
                    }
                    let (next, _) = self
                        .probe
                        .changed
                        .wait_timeout(guard, Duration::from_millis(20))
                        .unwrap();
                    guard = next;
                }
                drop(guard);
                consume(&[7])?;
                return Ok(1);
            }
            let _active = ActiveRead(Arc::clone(&self.probe));
            let active = self.probe.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.probe.max_active.fetch_max(active, Ordering::AcqRel);
            self.probe.entered.fetch_add(1, Ordering::AcqRel);
            self.probe.changed.notify_all();
            let mut guard = self
                .probe
                .mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !self.probe.released.load(Ordering::Acquire) {
                if control.cancellation.is_cancelled() {
                    return Err(SshTransportError::Cancelled);
                }
                if control.deadline_expired() {
                    return Err(SshTransportError::TimedOut);
                }
                let (next, _) = self
                    .probe
                    .changed
                    .wait_timeout(guard, Duration::from_millis(20))
                    .unwrap();
                guard = next;
            }
            drop(guard);
            consume(&[7])?;
            Ok(1)
        }
    }

    #[test]
    fn reused_sftp_session_cancels_only_the_second_read() {
        let probe = Arc::new(BlockingReadProbe::new());
        probe.block_second_read.store(true, Ordering::Release);
        let (fs, source_id) = blocking_filesystem(Arc::clone(&probe), "reuse-cancel-test");
        let reference = FileRef::new(source_id, SourcePath::new("frame.png").unwrap());

        let mut first_bytes = Vec::new();
        assert!(
            fs.read(
                &reference,
                ReadRequest {
                    offset: 0,
                    max_bytes: 16,
                },
                &control(),
                &mut |chunk| {
                    first_bytes.extend_from_slice(chunk);
                    Ok(())
                },
            )
            .is_ok()
        );
        assert_eq!(first_bytes, [7]);

        let cancellation = FsCancellation::default();
        let mut second_control = control();
        second_control.cancellation = cancellation.clone();
        let (started_sender, started_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let second_fs = Arc::clone(&fs);
        let second_reference = reference.clone();
        let second_handle = thread::spawn(move || {
            started_sender.send(()).unwrap();
            let mut bytes = Vec::new();
            let result = second_fs.read(
                &second_reference,
                ReadRequest {
                    offset: 0,
                    max_bytes: 16,
                },
                &second_control,
                &mut |chunk| {
                    bytes.extend_from_slice(chunk);
                    Ok(())
                },
            );
            result_sender.send(result).unwrap();
        });
        started_receiver.recv().unwrap();
        assert!(probe.wait_for_second_read());

        cancellation.cancel();
        let second_result = result_receiver.recv_timeout(Duration::from_secs(1));
        probe.release();
        second_handle.join().unwrap();
        assert!(matches!(
            second_result.expect("second read did not return after cancellation"),
            Err(FileSystemError::Cancelled)
        ));
        assert_eq!(probe.connections.load(Ordering::Acquire), 1);
    }

    #[test]
    fn same_source_reads_overlap_on_independent_sessions() {
        let probe = Arc::new(BlockingReadProbe::new());
        let credentials = Arc::new(MemorySshTransport::new(HOST_KEY));
        credentials.allow_credential("session:test");
        let transport = Arc::new(BlockingReadTransport {
            probe: Arc::clone(&probe),
        });
        let source_id = FileSourceId::new("sftp-pool-test").unwrap();
        let fs = Arc::new(
            SftpFileSystem::new(
                source_id.clone(),
                "/opt",
                SshConnectionTarget {
                    host: "camera.test".to_owned(),
                    port: 22,
                    username: "root".to_owned(),
                    expected_host_key: None,
                    command_subsystem: None,
                    remote_event_subsystem: None,
                },
                "session:test",
                credentials,
                transport,
            )
            .unwrap(),
        );
        let references = [
            "a.png", "b.png", "c.png", "d.png", "e.png", "f.png", "g.png", "h.png", "i.png",
        ]
        .into_iter()
        .map(|name| FileRef::new(source_id.clone(), SourcePath::new(name).unwrap()));
        let handles: Vec<_> = references
            .map(|reference| {
                let fs = Arc::clone(&fs);
                thread::spawn(move || {
                    let mut bytes = Vec::new();
                    fs.read(
                        &reference,
                        ReadRequest {
                            offset: 0,
                            max_bytes: 16,
                        },
                        &control(),
                        &mut |chunk| {
                            bytes.extend_from_slice(chunk);
                            Ok(())
                        },
                    )
                })
            })
            .collect();

        let saturated = probe.wait_for_entered(SFTP_SESSION_POOL_SIZE);
        assert!(saturated, "same-source reads did not fill the session pool");
        assert_eq!(
            probe.entered.load(Ordering::Acquire),
            SFTP_SESSION_POOL_SIZE,
            "the ninth read entered before a session slot was released"
        );
        probe.release();
        for handle in handles {
            assert!(handle.join().unwrap().is_ok());
        }
        assert_eq!(
            probe.max_active.load(Ordering::Acquire),
            SFTP_SESSION_POOL_SIZE
        );
        assert_eq!(
            probe.connections.load(Ordering::Acquire),
            SFTP_SESSION_POOL_SIZE
        );
    }

    fn blocking_filesystem(
        probe: Arc<BlockingReadProbe>,
        source_name: &str,
    ) -> (Arc<SftpFileSystem>, FileSourceId) {
        let credentials = Arc::new(MemorySshTransport::new(HOST_KEY));
        credentials.allow_credential("session:test");
        let transport = Arc::new(BlockingReadTransport { probe });
        let source_id = FileSourceId::new(source_name).unwrap();
        let fs = Arc::new(
            SftpFileSystem::new(
                source_id.clone(),
                "/opt",
                SshConnectionTarget {
                    host: "camera.test".to_owned(),
                    port: 22,
                    username: "root".to_owned(),
                    expected_host_key: None,
                    command_subsystem: None,
                    remote_event_subsystem: None,
                },
                "session:test",
                credentials,
                transport,
            )
            .unwrap(),
        );
        (fs, source_id)
    }

    #[test]
    fn waiting_control_session_checkout_honors_cancellation() {
        let probe = Arc::new(BlockingReadProbe::new());
        probe.block_lists.store(true, Ordering::Release);
        let (fs, source_id) = blocking_filesystem(Arc::clone(&probe), "control-wait-test");
        let root = DirectoryRef::root(source_id.clone());
        let first_fs = Arc::clone(&fs);
        let first_root = root.clone();
        let first_handle = thread::spawn(move || {
            first_fs.list(&first_root, ListPageRequest::default(), &control())
        });
        assert!(probe.wait_for_list_entered(1));

        let cancellation = FsCancellation::default();
        let mut second_control = control();
        second_control.cancellation = cancellation.clone();
        let (started_sender, started_receiver) = mpsc::channel();
        let second_fs = Arc::clone(&fs);
        let second_root = root.clone();
        let (result_sender, result_receiver) = mpsc::channel();
        let second_handle = thread::spawn(move || {
            started_sender.send(()).unwrap();
            let result = second_fs.list(&second_root, ListPageRequest::default(), &second_control);
            result_sender.send(result).unwrap();
        });
        started_receiver.recv().unwrap();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(probe.list_entered.load(Ordering::Acquire), 1);

        cancellation.cancel();
        let second_result = result_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("cancelled control checkout did not return");
        probe.release();
        assert!(first_handle.join().unwrap().is_ok());
        second_handle.join().unwrap();
        assert!(matches!(second_result, Err(FileSystemError::Cancelled)));
    }

    #[test]
    fn control_session_keeps_directory_cursor_during_parallel_reads() {
        let probe = Arc::new(BlockingReadProbe::new());
        let (fs, source_id) = blocking_filesystem(Arc::clone(&probe), "cursor-affinity-test");
        let root = DirectoryRef::root(source_id.clone());
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
        let continuation = first.next.clone();
        assert!(continuation.is_some());

        let handles: Vec<_> = ["a.png", "b.png"]
            .into_iter()
            .map(|name| {
                let fs = Arc::clone(&fs);
                let reference = FileRef::new(source_id.clone(), SourcePath::new(name).unwrap());
                thread::spawn(move || {
                    let mut bytes = Vec::new();
                    fs.read(
                        &reference,
                        ReadRequest {
                            offset: 0,
                            max_bytes: 16,
                        },
                        &control(),
                        &mut |chunk| {
                            bytes.extend_from_slice(chunk);
                            Ok(())
                        },
                    )
                })
            })
            .collect();
        assert!(probe.wait_for_entered(2));

        let second = fs
            .list(
                &root,
                ListPageRequest {
                    continuation,
                    limit: 1,
                },
                &control(),
            )
            .unwrap();
        assert_eq!(second.entries.len(), 1);
        assert!(second.next.is_none());

        probe.release();
        for handle in handles {
            assert!(handle.join().unwrap().is_ok());
        }
        assert_eq!(probe.connections.load(Ordering::Acquire), 3);
    }

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
    fn missing_host_key_pin_does_not_reduce_sftp_permissions() {
        let memory = Arc::new(MemorySshTransport::new(HOST_KEY));
        memory.allow_credential("session:test");
        memory.insert_directory("/opt");
        let unpinned = filesystem(memory, None);

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
    fn explicit_host_key_pin_is_enforced() {
        let memory = Arc::new(MemorySshTransport::new(HOST_KEY));
        memory.allow_credential("session:test");
        memory.insert_directory("/opt");
        let fs = filesystem(memory, Some("ssh-ed25519 DIFFERENT"));
        let error = fs
            .list(
                &DirectoryRef::root(FileSourceId::new("remote-test").unwrap()),
                ListPageRequest::default(),
                &control(),
            )
            .unwrap_err();
        assert!(matches!(error, FileSystemError::PermissionDenied(_)));
    }
}
