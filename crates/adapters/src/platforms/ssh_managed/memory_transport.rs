//! Deterministic、无网络/文件系统副作用的 argv-level SSH test transport。

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use camera_toolbox_app::{RemoteFileStat, RemoteOperationControl};
use secrecy::SecretString;

use super::connection::{
    CredentialResolver, SshConnectionTarget, SshCredential, SshTransportError, SshTransportFactory,
    SshTransportSession, TransportCommandOutput, TransportDirEntry, TransportFileKind,
    TransportListPage,
};

#[derive(Debug, Clone)]
pub struct MemoryRemoteFile {
    pub bytes: Vec<u8>,
    pub stats: VecDeque<RemoteFileStat>,
}

#[derive(Debug)]
struct MemoryTransportState {
    actual_host_key: String,
    credentials: BTreeSet<String>,
    files: BTreeMap<String, MemoryRemoteFile>,
    directories: BTreeSet<String>,
    canonical_paths: BTreeMap<String, String>,
    event_paths: Vec<String>,
    event_error: Option<SshTransportError>,
    command_output: TransportCommandOutput,
    captured_argv: Vec<Vec<String>>,
    captured_stdin: Vec<Vec<u8>>,
    read_calls: usize,
    read_chunk_bytes: usize,
    read_delay: Duration,
    cancel_after_bytes: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct MemorySshTransport {
    state: Arc<Mutex<MemoryTransportState>>,
}

impl MemorySshTransport {
    #[must_use]
    pub fn new(actual_host_key: impl Into<String>) -> Self {
        Self {
            state: Arc::new(Mutex::new(MemoryTransportState {
                actual_host_key: actual_host_key.into(),
                credentials: BTreeSet::new(),
                files: BTreeMap::new(),
                directories: BTreeSet::from(["/".to_owned()]),
                canonical_paths: BTreeMap::new(),
                event_paths: Vec::new(),
                event_error: None,
                command_output: TransportCommandOutput {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    exit_status: Some(0),
                    stdout_truncated: false,
                    stderr_truncated: false,
                },
                captured_argv: Vec::new(),
                captured_stdin: Vec::new(),
                read_calls: 0,
                read_chunk_bytes: 64 * 1024,
                read_delay: Duration::ZERO,
                cancel_after_bytes: None,
            })),
        }
    }

    pub fn allow_credential(&self, credential_ref: impl Into<String>) {
        self.lock().credentials.insert(credential_ref.into());
    }

    pub fn insert_file(&self, path: impl Into<String>, file: MemoryRemoteFile) {
        let path = path.into();
        let mut state = self.lock();
        insert_parent_directories(&mut state.directories, &path);
        state.files.insert(path, file);
    }

    pub fn insert_directory(&self, path: impl Into<String>) {
        let path = trim_memory_path(path.into());
        let mut state = self.lock();
        insert_parent_directories(&mut state.directories, &path);
        state.directories.insert(path);
    }

    #[must_use]
    pub fn contains_path(&self, path: &str) -> bool {
        let state = self.lock();
        state.files.contains_key(path) || state.directories.contains(path)
    }

    #[must_use]
    pub fn file_bytes(&self, path: &str) -> Option<Vec<u8>> {
        self.lock().files.get(path).map(|file| file.bytes.clone())
    }

    pub fn set_canonical_path(&self, path: impl Into<String>, canonical: impl Into<String>) {
        self.lock()
            .canonical_paths
            .insert(path.into(), canonical.into());
    }

    pub fn set_event_paths(&self, paths: Vec<String>) {
        self.lock().event_paths = paths;
    }

    pub fn set_event_error(&self, error: Option<SshTransportError>) {
        self.lock().event_error = error;
    }

    pub fn set_command_output(&self, output: TransportCommandOutput) {
        self.lock().command_output = output;
    }

    pub fn set_read_behavior(
        &self,
        chunk_bytes: usize,
        delay: Duration,
        cancel_after_bytes: Option<usize>,
    ) {
        let mut state = self.lock();
        state.read_chunk_bytes = chunk_bytes.max(1);
        state.read_delay = delay;
        state.cancel_after_bytes = cancel_after_bytes;
    }

    #[must_use]
    pub fn captured_argv(&self) -> Vec<Vec<String>> {
        self.lock().captured_argv.clone()
    }

    #[must_use]
    pub fn captured_stdin(&self) -> Vec<Vec<u8>> {
        self.lock().captured_stdin.clone()
    }

    #[must_use]
    pub fn read_calls(&self) -> usize {
        self.lock().read_calls
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryTransportState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl CredentialResolver for MemorySshTransport {
    fn resolve(&self, credential_ref: &str) -> Result<SshCredential, String> {
        if self.lock().credentials.contains(credential_ref) {
            Ok(SshCredential::Password(SecretString::from(
                "memory-test-secret",
            )))
        } else {
            Err(format!("unknown credential reference {credential_ref}"))
        }
    }
}

impl SshTransportFactory for MemorySshTransport {
    fn connect(
        &self,
        target: &SshConnectionTarget,
        _credential: SshCredential,
        control: &RemoteOperationControl,
    ) -> Result<Box<dyn SshTransportSession>, SshTransportError> {
        if control.cancellation.is_cancelled() {
            return Err(SshTransportError::Cancelled);
        }
        if control.deadline_expired() {
            return Err(SshTransportError::TimedOut);
        }
        if let Some(expected) = &target.expected_host_key
            && expected != &self.lock().actual_host_key
        {
            return Err(SshTransportError::HostKeyMismatch);
        }
        Ok(Box::new(MemorySshSession {
            state: Arc::clone(&self.state),
        }))
    }
}

struct MemorySshSession {
    state: Arc<Mutex<MemoryTransportState>>,
}

impl MemorySshSession {
    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryTransportState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct MemoryWriteSink<'a> {
    bytes: Vec<u8>,
    control: &'a RemoteOperationControl,
    failure: Option<SshTransportError>,
}

impl MemoryWriteSink<'_> {
    fn checkpoint(&mut self) -> io::Result<()> {
        match check_control(self.control) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failure = Some(error.clone());
                Err(io::Error::other(error.to_string()))
            }
        }
    }
}

impl io::Write for MemoryWriteSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.checkpoint()?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.checkpoint()
    }
}

impl SshTransportSession for MemorySshSession {
    fn execute_argv(
        &mut self,
        argv: &[String],
        output_limit: usize,
        control: &RemoteOperationControl,
    ) -> Result<TransportCommandOutput, SshTransportError> {
        check_control(control)?;
        let mut state = self.lock();
        state.captured_argv.push(argv.to_vec());
        let mut output = state.command_output.clone();
        bound_output(
            &mut output.stdout,
            output_limit,
            &mut output.stdout_truncated,
        );
        bound_output(
            &mut output.stderr,
            output_limit,
            &mut output.stderr_truncated,
        );
        Ok(output)
    }

    fn execute_argv_with_stdin(
        &mut self,
        argv: &[String],
        stdin: &[u8],
        output_limit: usize,
        control: &RemoteOperationControl,
    ) -> Result<TransportCommandOutput, SshTransportError> {
        check_control(control)?;
        let mut state = self.lock();
        state.captured_argv.push(argv.to_vec());
        state.captured_stdin.push(stdin.to_vec());
        let mut output = state.command_output.clone();
        bound_output(
            &mut output.stdout,
            output_limit,
            &mut output.stdout_truncated,
        );
        bound_output(
            &mut output.stderr,
            output_limit,
            &mut output.stderr_truncated,
        );
        Ok(output)
    }

    fn canonicalize(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<String, SshTransportError> {
        check_control(control)?;
        let state = self.lock();
        if let Some(canonical) = state.canonical_paths.get(path) {
            return Ok(canonical.clone());
        }
        if state.files.contains_key(path) || state.directories.contains(path) {
            Ok(trim_memory_path(path.to_owned()))
        } else {
            Err(SshTransportError::NotFound(path.to_owned()))
        }
    }

    fn stat(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<RemoteFileStat, SshTransportError> {
        check_control(control)?;
        let mut state = self.lock();
        let file = state
            .files
            .get_mut(path)
            .ok_or_else(|| SshTransportError::Transport(format!("missing remote file {path}")))?;
        if file.stats.len() > 1 {
            file.stats
                .pop_front()
                .ok_or_else(|| SshTransportError::Transport("empty stat script".to_owned()))
        } else {
            file.stats
                .front()
                .cloned()
                .ok_or_else(|| SshTransportError::Transport("empty stat script".to_owned()))
        }
    }

    fn metadata(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<TransportDirEntry, SshTransportError> {
        check_control(control)?;
        let state = self.lock();
        if state.directories.contains(path) {
            return Ok(TransportDirEntry {
                path: path.to_owned(),
                size: 0,
                modified_seconds: 0,
                kind: TransportFileKind::Directory,
            });
        }
        let stat = state
            .files
            .get(path)
            .and_then(|file| file.stats.front())
            .ok_or_else(|| SshTransportError::NotFound(path.to_owned()))?;
        Ok(TransportDirEntry {
            path: path.to_owned(),
            size: stat.size,
            modified_seconds: stat.modified_seconds,
            kind: TransportFileKind::File,
        })
    }

    fn read_dir(
        &mut self,
        root: &str,
        control: &RemoteOperationControl,
    ) -> Result<Vec<TransportDirEntry>, SshTransportError> {
        check_control(control)?;
        let state = self.lock();
        if !state.directories.contains(root) {
            return Err(SshTransportError::NotFound(root.to_owned()));
        }
        let prefix = if root == "/" {
            "/".to_owned()
        } else {
            format!("{root}/")
        };
        let mut entries = Vec::new();
        for directory in &state.directories {
            let Some(relative) = directory.strip_prefix(&prefix) else {
                continue;
            };
            if relative.is_empty() || relative.contains('/') {
                continue;
            }
            entries.push(TransportDirEntry {
                path: directory.clone(),
                size: 0,
                modified_seconds: 0,
                kind: TransportFileKind::Directory,
            });
        }
        for (path, file) in &state.files {
            let Some(relative) = path.strip_prefix(&prefix) else {
                continue;
            };
            if relative.is_empty() || relative.contains('/') {
                continue;
            }
            if let Some(stat) = file.stats.front() {
                entries.push(TransportDirEntry {
                    path: path.clone(),
                    size: stat.size,
                    modified_seconds: stat.modified_seconds,
                    kind: TransportFileKind::File,
                });
            }
        }
        Ok(entries)
    }

    fn read_dir_page(
        &mut self,
        root: &str,
        continuation: Option<&str>,
        limit: usize,
        control: &RemoteOperationControl,
    ) -> Result<TransportListPage, SshTransportError> {
        let offset = if let Some(token) = continuation {
            let (token_root, offset) = token
                .rsplit_once('|')
                .ok_or(SshTransportError::InvalidContinuation)?;
            if token_root != root {
                return Err(SshTransportError::InvalidContinuation);
            }
            offset
                .parse::<usize>()
                .map_err(|_| SshTransportError::InvalidContinuation)?
        } else {
            0
        };
        let entries = self.read_dir(root, control)?;
        let limit = limit.clamp(1, 1024);
        let end = offset.saturating_add(limit).min(entries.len());
        if offset > entries.len() {
            return Err(SshTransportError::InvalidContinuation);
        }
        Ok(TransportListPage {
            entries: entries[offset..end].to_vec(),
            continuation: (end < entries.len()).then(|| format!("{root}|{end}")),
        })
    }

    fn event_paths(
        &mut self,
        _root: &str,
        _glob: &str,
        control: &RemoteOperationControl,
    ) -> Result<Vec<String>, SshTransportError> {
        check_control(control)?;
        let state = self.lock();
        if let Some(error) = &state.event_error {
            Err(error.clone())
        } else {
            Ok(state.event_paths.clone())
        }
    }

    fn read_file(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
        consume: &mut dyn FnMut(&[u8]) -> Result<(), SshTransportError>,
    ) -> Result<u64, SshTransportError> {
        check_control(control)?;
        let (bytes, chunk_bytes, delay, cancel_after) = {
            let mut state = self.lock();
            state.read_calls = state.read_calls.saturating_add(1);
            let bytes = state
                .files
                .get(path)
                .ok_or_else(|| SshTransportError::Transport(format!("missing remote file {path}")))?
                .bytes
                .clone();
            (
                bytes,
                state.read_chunk_bytes,
                state.read_delay,
                state.cancel_after_bytes,
            )
        };
        let mut total = 0_usize;
        for chunk in bytes.chunks(chunk_bytes) {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            check_control(control)?;
            consume(chunk)?;
            total = total.saturating_add(chunk.len());
            if cancel_after.is_some_and(|limit| total >= limit) {
                control.cancellation.cancel();
            }
        }
        u64::try_from(total)
            .map_err(|_| SshTransportError::Transport("memory read length overflow".to_owned()))
    }

    fn write_file_new(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
        produce: &mut dyn FnMut(&mut dyn io::Write) -> Result<(), SshTransportError>,
    ) -> Result<(), SshTransportError> {
        check_control(control)?;
        {
            let state = self.lock();
            if state.files.contains_key(path) || state.directories.contains(path) {
                return Err(SshTransportError::AlreadyExists(path.to_owned()));
            }
            let parent = memory_parent(path);
            if !state.directories.contains(parent) {
                return Err(SshTransportError::NotFound(parent.to_owned()));
            }
        }

        let mut writer = MemoryWriteSink {
            bytes: Vec::new(),
            control,
            failure: None,
        };
        let produce_result = produce(&mut writer);
        let failure = writer.failure.take();
        if let Some(error) = failure {
            return Err(error);
        }
        produce_result?;
        check_control(control)?;
        let size = u64::try_from(writer.bytes.len())
            .map_err(|_| SshTransportError::Transport("memory write length overflow".to_owned()))?;

        let mut state = self.lock();
        if state.files.contains_key(path) || state.directories.contains(path) {
            return Err(SshTransportError::AlreadyExists(path.to_owned()));
        }
        let parent = memory_parent(path);
        if !state.directories.contains(parent) {
            return Err(SshTransportError::NotFound(parent.to_owned()));
        }
        state.files.insert(
            path.to_owned(),
            MemoryRemoteFile {
                bytes: writer.bytes,
                stats: VecDeque::from([RemoteFileStat {
                    path: path.to_owned(),
                    size,
                    modified_seconds: 0,
                    producer_marker: None,
                    sha256: None,
                }]),
            },
        );
        Ok(())
    }

    fn mkdir(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<(), SshTransportError> {
        check_control(control)?;
        let mut state = self.lock();
        if state.files.contains_key(path) || state.directories.contains(path) {
            return Err(SshTransportError::AlreadyExists(path.to_owned()));
        }
        let parent = memory_parent(path);
        if !state.directories.contains(parent) {
            return Err(SshTransportError::NotFound(parent.to_owned()));
        }
        state.directories.insert(path.to_owned());
        Ok(())
    }

    fn rename(
        &mut self,
        source: &str,
        destination: &str,
        control: &RemoteOperationControl,
    ) -> Result<(), SshTransportError> {
        check_control(control)?;
        let mut state = self.lock();
        if state.files.contains_key(destination) || state.directories.contains(destination) {
            return Err(SshTransportError::AlreadyExists(destination.to_owned()));
        }
        if let Some(mut file) = state.files.remove(source) {
            for stat in &mut file.stats {
                stat.path = destination.to_owned();
            }
            state.files.insert(destination.to_owned(), file);
            return Ok(());
        }
        if !state.directories.contains(source) {
            return Err(SshTransportError::NotFound(source.to_owned()));
        }
        let source_prefix = format!("{source}/");
        let directories: Vec<_> = state
            .directories
            .iter()
            .filter(|path| *path == source || path.starts_with(&source_prefix))
            .cloned()
            .collect();
        let files: Vec<_> = state
            .files
            .keys()
            .filter(|path| path.starts_with(&source_prefix))
            .cloned()
            .collect();
        for path in directories {
            state.directories.remove(&path);
            state
                .directories
                .insert(path.replacen(source, destination, 1));
        }
        for path in files {
            let mut file = state.files.remove(&path).expect("listed file exists");
            let renamed = path.replacen(source, destination, 1);
            for stat in &mut file.stats {
                stat.path = renamed.clone();
            }
            state.files.insert(renamed, file);
        }
        Ok(())
    }

    fn remove_file(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<(), SshTransportError> {
        check_control(control)?;
        self.lock()
            .files
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| SshTransportError::NotFound(path.to_owned()))
    }

    fn remove_dir(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<(), SshTransportError> {
        check_control(control)?;
        let mut state = self.lock();
        let prefix = format!("{path}/");
        if state.files.keys().any(|child| child.starts_with(&prefix))
            || state
                .directories
                .iter()
                .any(|child| child != path && child.starts_with(&prefix))
        {
            return Err(SshTransportError::Transport(
                "remote directory is not empty".to_owned(),
            ));
        }
        if state.directories.remove(path) {
            Ok(())
        } else {
            Err(SshTransportError::NotFound(path.to_owned()))
        }
    }
}

fn trim_memory_path(mut path: String) -> String {
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

fn memory_parent(path: &str) -> &str {
    path.rsplit_once('/').map_or(
        "/",
        |(parent, _)| {
            if parent.is_empty() { "/" } else { parent }
        },
    )
}

fn insert_parent_directories(directories: &mut BTreeSet<String>, path: &str) {
    let mut current = memory_parent(path);
    loop {
        directories.insert(current.to_owned());
        if current == "/" {
            break;
        }
        current = memory_parent(current);
    }
}

fn check_control(control: &RemoteOperationControl) -> Result<(), SshTransportError> {
    if control.cancellation.is_cancelled() {
        Err(SshTransportError::Cancelled)
    } else if control.deadline_expired() {
        Err(SshTransportError::TimedOut)
    } else {
        Ok(())
    }
}

fn bound_output(bytes: &mut Vec<u8>, limit: usize, truncated: &mut bool) {
    if bytes.len() > limit {
        bytes.truncate(limit);
        *truncated = true;
    }
}
