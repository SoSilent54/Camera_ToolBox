//! Deterministic、无网络/文件系统副作用的 argv-level SSH test transport。

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use camera_toolbox_app::{RemoteFileStat, RemoteOperationControl};
use secrecy::SecretString;

use super::connection::{
    CredentialResolver, SshConnectionTarget, SshCredential, SshTransportError, SshTransportFactory,
    SshTransportSession, TransportCommandOutput, TransportDirEntry,
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
    canonical_paths: BTreeMap<String, String>,
    event_paths: Vec<String>,
    event_error: Option<SshTransportError>,
    command_output: TransportCommandOutput,
    captured_argv: Vec<Vec<String>>,
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
        self.lock().files.insert(path.into(), file);
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
        if target.expected_host_key != self.lock().actual_host_key {
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

    fn canonicalize(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<String, SshTransportError> {
        check_control(control)?;
        Ok(self
            .lock()
            .canonical_paths
            .get(path)
            .cloned()
            .unwrap_or_else(|| path.to_owned()))
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

    fn read_dir(
        &mut self,
        root: &str,
        control: &RemoteOperationControl,
    ) -> Result<Vec<TransportDirEntry>, SshTransportError> {
        check_control(control)?;
        let state = self.lock();
        Ok(state
            .files
            .iter()
            .filter(|(path, _)| path.starts_with(root))
            .filter_map(|(path, file)| {
                file.stats.front().map(|stat| TransportDirEntry {
                    path: path.clone(),
                    size: stat.size,
                    modified_seconds: stat.modified_seconds,
                })
            })
            .collect())
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
