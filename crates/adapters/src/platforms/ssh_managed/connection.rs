//! SSH/SFTP transport abstraction 与 production russh 实现。

use std::{
    collections::{BTreeMap, VecDeque},
    io,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use camera_toolbox_app::{RemoteFileStat, RemoteOperationControl};
use russh::{ChannelMsg, client};
use russh_sftp::{
    client::{RawSftpSession, SftpSession, error::Error as SftpError, fs::File as SftpFile},
    protocol::{OpenFlags, StatusCode},
};
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;

const ARGV_MAGIC: &[u8; 8] = b"CTARGV1\0";
const WATCH_MAGIC: &[u8; 8] = b"CTWATCH1";
const READ_CHUNK_BYTES: usize = 256 * 1024;
const WRITE_CHUNK_BYTES: usize = 64 * 1024;
const SFTP_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_EVENT_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_DIRECTORY_CANDIDATES: usize = 4096;
const MAX_DIRECTORY_METADATA_BYTES: usize = 1024 * 1024;
const MAX_ACTIVE_DIRECTORY_CURSORS: usize = 16;
const MAX_DIRECTORY_PAGE_ENTRIES: usize = 1024;
const DIRECTORY_CURSOR_TTL: Duration = Duration::from_secs(30);

/// resolver 返回 operation-owned secret；profile 永远只持有引用字符串。
#[derive(Clone)]
pub enum SshCredential {
    Password(SecretString),
    PrivateKeyOpenSsh {
        key: SecretString,
        passphrase: Option<SecretString>,
    },
}

#[allow(clippy::missing_errors_doc)]
pub trait CredentialResolver: Send + Sync {
    fn resolve(&self, credential_ref: &str) -> Result<SshCredential, String>;
}

#[derive(Debug, Clone)]
pub struct SshConnectionTarget {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// `None` 明确表示接受服务端提供的任意主机密钥。
    pub expected_host_key: Option<String>,
    pub command_subsystem: Option<String>,
    pub remote_event_subsystem: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportCommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_status: Option<u32>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportFileKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportDirEntry {
    pub path: String,
    pub size: u64,
    pub modified_seconds: u64,
    pub kind: TransportFileKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportListPage {
    pub entries: Vec<TransportDirEntry>,
    pub continuation: Option<String>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SshTransportError {
    #[error("SSH host key mismatch")]
    HostKeyMismatch,
    #[error("SSH authentication failed")]
    AuthenticationFailed,
    #[error("operation cancelled")]
    Cancelled,
    #[error("operation timed out")]
    TimedOut,
    #[error("remote path not found: {0}")]
    NotFound(String),
    #[error("remote permission denied: {0}")]
    PermissionDenied(String),
    #[error("remote file source disconnected: {0}")]
    Disconnected(String),
    #[error("remote destination already exists: {0}")]
    AlreadyExists(String),
    #[error("remote read exceeds bound: requested {requested}, limit {limit}")]
    ReadLimitExceeded { requested: u64, limit: u64 },
    #[error("remote source changed during read: {0}")]
    ChangedDuringRead(String),
    #[error("invalid or expired remote directory continuation")]
    InvalidContinuation,
    #[error("remote operation is unsupported")]
    Unsupported,
    #[error("transport failure: {0}")]
    Transport(String),
    #[error("helper protocol failure: {0}")]
    HelperProtocol(String),
}

#[allow(clippy::missing_errors_doc)]
pub trait SshTransportFactory: Send + Sync {
    fn connect(
        &self,
        target: &SshConnectionTarget,
        credential: SshCredential,
        control: &RemoteOperationControl,
    ) -> Result<Box<dyn SshTransportSession>, SshTransportError>;
}

/// 所有 command API 保持 argv 边界；trait 中不存在 command string。
#[allow(clippy::missing_errors_doc)]
pub trait SshTransportSession: Send {
    /// 为可复用 session 绑定当前 operation 的取消令牌。
    fn prepare_operation(&mut self, _control: &RemoteOperationControl) {}
    fn execute_argv(
        &mut self,
        argv: &[String],
        output_limit: usize,
        control: &RemoteOperationControl,
    ) -> Result<TransportCommandOutput, SshTransportError>;

    /// 执行固定 argv 并通过 stdin 发送 typed payload；默认 transport 不隐式降级到 argv。
    fn execute_argv_with_stdin(
        &mut self,
        _argv: &[String],
        _stdin: &[u8],
        _output_limit: usize,
        _control: &RemoteOperationControl,
    ) -> Result<TransportCommandOutput, SshTransportError> {
        Err(SshTransportError::Unsupported)
    }

    fn canonicalize(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<String, SshTransportError>;

    fn stat(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<RemoteFileStat, SshTransportError>;

    fn read_dir(
        &mut self,
        root: &str,
        control: &RemoteOperationControl,
    ) -> Result<Vec<TransportDirEntry>, SshTransportError>;

    fn read_dir_page(
        &mut self,
        root: &str,
        continuation: Option<&str>,
        limit: usize,
        control: &RemoteOperationControl,
    ) -> Result<TransportListPage, SshTransportError>;

    fn metadata(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<TransportDirEntry, SshTransportError>;

    fn event_paths(
        &mut self,
        root: &str,
        glob: &str,
        control: &RemoteOperationControl,
    ) -> Result<Vec<String>, SshTransportError>;

    /// 数据以固定 chunk 交给 caller；caller 在同一 owned buffer 中增量 hash。
    fn read_file(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
        consume: &mut dyn FnMut(&[u8]) -> Result<(), SshTransportError>,
    ) -> Result<u64, SshTransportError>;

    /// 使用 SFTP 的 CREATE|EXCL 新建受控 staging 文件；调用者负责最终发布与清理。
    fn write_file_new(
        &mut self,
        _path: &str,
        _control: &RemoteOperationControl,
        _produce: &mut dyn FnMut(&mut dyn io::Write) -> Result<(), SshTransportError>,
    ) -> Result<(), SshTransportError> {
        Err(SshTransportError::Unsupported)
    }

    fn mkdir(
        &mut self,
        _path: &str,
        _control: &RemoteOperationControl,
    ) -> Result<(), SshTransportError> {
        Err(SshTransportError::Unsupported)
    }

    fn rename(
        &mut self,
        _source: &str,
        _destination: &str,
        _control: &RemoteOperationControl,
    ) -> Result<(), SshTransportError> {
        Err(SshTransportError::Unsupported)
    }

    fn remove_file(
        &mut self,
        _path: &str,
        _control: &RemoteOperationControl,
    ) -> Result<(), SshTransportError> {
        Err(SshTransportError::Unsupported)
    }

    fn remove_dir(
        &mut self,
        _path: &str,
        _control: &RemoteOperationControl,
    ) -> Result<(), SshTransportError> {
        Err(SshTransportError::Unsupported)
    }
}

#[derive(Debug, Default)]
pub struct RusshTransportFactory;

struct HostKeyVerifier {
    expected: Option<russh::keys::ssh_key::PublicKey>,
}

impl client::Handler for HostKeyVerifier {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(self
            .expected
            .as_ref()
            .is_none_or(|expected| server_public_key == expected))
    }
}

struct RusshTransportSession {
    runtime: tokio::runtime::Runtime,
    session: client::Handle<HostKeyVerifier>,
    sftp: Option<Arc<SftpSession>>,
    command_subsystem: Option<String>,
    remote_event_subsystem: Option<String>,
    cancellation: CancellationToken,
    next_directory_cursor_id: u64,
    directory_cursors: BTreeMap<String, TransportDirectoryCursor>,
    directory_cursor_order: VecDeque<String>,
}

struct TransportDirectoryCursor {
    root: String,
    raw: RawSftpSession,
    handle: String,
    pending: VecDeque<TransportDirEntry>,
    last_used: TokioInstant,
}
fn operation_cancellation(control: &RemoteOperationControl) -> CancellationToken {
    let cancellation = CancellationToken::new();
    let bridge = cancellation.clone();
    control
        .cancellation
        .register_interrupt(Arc::new(move || bridge.cancel()));
    cancellation
}

/// 仅缓存成功初始化的共享对象；失败保持空槽，允许后续 operation 重试。
fn get_or_try_init_shared<T, E>(
    slot: &mut Option<Arc<T>>,
    initialize: impl FnOnce() -> Result<T, E>,
) -> Result<Arc<T>, E> {
    if let Some(value) = slot {
        return Ok(Arc::clone(value));
    }
    let value = Arc::new(initialize()?);
    *slot = Some(Arc::clone(&value));
    Ok(value)
}

/// 在 operation 剩余预算内关闭文件句柄；取消时立即交给 transport 废弃路径回收。
async fn close_sftp_file(
    file: &mut SftpFile,
    cancellation: &CancellationToken,
    timeout: Duration,
) -> Result<(), SshTransportError> {
    if timeout.is_zero() {
        return Err(SshTransportError::TimedOut);
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(SshTransportError::Cancelled),
        result = tokio::time::timeout(timeout, file.shutdown()) => match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(SshTransportError::Transport(error.to_string())),
            Err(_) => Err(SshTransportError::TimedOut),
        }
    }
}

fn read_error_discards_sftp(error: &SshTransportError) -> bool {
    matches!(
        error,
        SshTransportError::Cancelled
            | SshTransportError::TimedOut
            | SshTransportError::Disconnected(_)
            | SshTransportError::Transport(_)
    )
}

fn finish_sftp_read<T>(
    read_result: Result<T, SshTransportError>,
    close_result: Result<(), SshTransportError>,
) -> Result<T, SshTransportError> {
    match read_result {
        Err(error) => Err(error),
        Ok(value) => close_result.map(|()| value),
    }
}

struct BlockingSftpWriter<'a> {
    runtime: &'a tokio::runtime::Runtime,
    file: SftpFile,
    cancellation: CancellationToken,
    deadline: TokioInstant,
    idle: Duration,
    failure: Option<SshTransportError>,
}

impl BlockingSftpWriter<'_> {
    fn record_failure(&mut self, error: SshTransportError) -> io::Error {
        if self.failure.is_none() {
            self.failure = Some(error.clone());
        }
        io::Error::other(error.to_string())
    }

    fn finish(mut self) -> Result<(), SshTransportError> {
        if let Some(error) = self.failure.take() {
            let _ = self.runtime.block_on(async {
                cancellable_until(
                    &self.cancellation,
                    self.deadline,
                    self.idle,
                    self.file.shutdown(),
                )
                .await
            });
            return Err(error);
        }
        self.runtime.block_on(async {
            cancellable_until(
                &self.cancellation,
                self.deadline,
                self.idle,
                self.file.flush(),
            )
            .await?;
            cancellable_until(
                &self.cancellation,
                self.deadline,
                self.idle,
                self.file.shutdown(),
            )
            .await
        })
    }
}

impl io::Write for BlockingSftpWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let limit = bytes.len().min(WRITE_CHUNK_BYTES);
        let result = self.runtime.block_on(async {
            cancellable_until(
                &self.cancellation,
                self.deadline,
                self.idle,
                self.file.write(&bytes[..limit]),
            )
            .await
        });
        match result {
            Ok(0) => Err(self.record_failure(SshTransportError::Transport(
                "remote write returned zero bytes".to_owned(),
            ))),
            Ok(written) => Ok(written),
            Err(error) => Err(self.record_failure(error)),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.runtime.block_on(async {
            cancellable_until(
                &self.cancellation,
                self.deadline,
                self.idle,
                self.file.flush(),
            )
            .await
        });
        result.map_err(|error| self.record_failure(error))
    }
}

impl RusshTransportSession {
    fn sftp_session(
        &mut self,
        cancellation: &CancellationToken,
        deadline: TokioInstant,
        idle: Duration,
    ) -> Result<Arc<SftpSession>, SshTransportError> {
        let runtime = &self.runtime;
        let session = &self.session;
        get_or_try_init_shared(&mut self.sftp, || {
            runtime.block_on(open_sftp(session, cancellation, deadline, idle))
        })
    }

    fn close_sftp_session(&mut self) {
        let Some(sftp) = self.sftp.take() else {
            return;
        };
        self.runtime.block_on(async {
            let _ = tokio::time::timeout(SFTP_CLOSE_TIMEOUT, sftp.close()).await;
        });
    }
    fn discard_sftp_session(&mut self) {
        self.sftp.take();
    }

    fn execute_argv_input(
        &mut self,
        argv: &[String],
        stdin: Option<&[u8]>,
        output_limit: usize,
        control: &RemoteOperationControl,
    ) -> Result<TransportCommandOutput, SshTransportError> {
        let dispatch = command_dispatch(argv, self.command_subsystem.as_deref())?;
        let cancellation = self.cancellation.clone();
        let idle = control.timeouts.idle;
        let overall = control.remaining_overall();
        if overall.is_zero() {
            return Err(SshTransportError::TimedOut);
        }
        self.runtime.block_on(async {
            let deadline = TokioInstant::now() + overall;
            let mut channel = cancellable_until(
                &cancellation,
                deadline,
                idle,
                self.session.channel_open_session(),
            )
            .await?;
            if !dispatch_channel_request(&channel, dispatch, &cancellation, deadline, idle).await? {
                return Ok(empty_command_output(output_limit));
            }
            if let Some(stdin) = stdin {
                cancellable_until(
                    &cancellation,
                    deadline,
                    idle,
                    channel.data_bytes(stdin.to_vec()),
                )
                .await?;
                cancellable_until(&cancellation, deadline, idle, channel.eof()).await?;
            }

            let mut output = empty_command_output(output_limit);
            loop {
                let message = cancellable_until(&cancellation, deadline, idle, async {
                    Ok::<_, io::Error>(channel.wait().await)
                })
                .await?;
                let Some(message) = message else {
                    break;
                };
                match message {
                    ChannelMsg::Data { data } => append_bounded(
                        &mut output.stdout,
                        &data,
                        output_limit,
                        &mut output.stdout_truncated,
                    ),
                    ChannelMsg::ExtendedData { data, .. } => append_bounded(
                        &mut output.stderr,
                        &data,
                        output_limit,
                        &mut output.stderr_truncated,
                    ),
                    ChannelMsg::ExitStatus { exit_status } => {
                        output.exit_status = Some(exit_status);
                    }
                    _ => {}
                }
            }
            Ok(output)
        })
    }

    fn close_directory_cursor(&self, cursor: TransportDirectoryCursor) {
        self.runtime.block_on(async {
            let _ =
                tokio::time::timeout(Duration::from_secs(1), cursor.raw.close(cursor.handle)).await;
        });
    }

    fn prune_directory_cursors(&mut self) {
        let now = TokioInstant::now();
        let expired: Vec<_> = self
            .directory_cursors
            .iter()
            .filter(|(_, cursor)| now.duration_since(cursor.last_used) >= DIRECTORY_CURSOR_TTL)
            .map(|(token, _)| token.clone())
            .collect();
        for token in expired {
            self.directory_cursor_order
                .retain(|candidate| candidate != &token);
            if let Some(cursor) = self.directory_cursors.remove(&token) {
                self.close_directory_cursor(cursor);
            }
        }
    }
}

impl Drop for RusshTransportSession {
    fn drop(&mut self) {
        let cursors = std::mem::take(&mut self.directory_cursors);
        self.directory_cursor_order.clear();
        for cursor in cursors.into_values() {
            self.close_directory_cursor(cursor);
        }
        self.close_sftp_session();
    }
}

impl SshTransportFactory for RusshTransportFactory {
    fn connect(
        &self,
        target: &SshConnectionTarget,
        credential: SshCredential,
        control: &RemoteOperationControl,
    ) -> Result<Box<dyn SshTransportSession>, SshTransportError> {
        if control.cancellation.is_cancelled() {
            return Err(SshTransportError::Cancelled);
        }
        let expected = target
            .expected_host_key
            .as_deref()
            .map(russh::keys::ssh_key::PublicKey::from_openssh)
            .transpose()
            .map_err(|error| {
                SshTransportError::HelperProtocol(format!("invalid expected host key: {error}"))
            })?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| SshTransportError::Transport(error.to_string()))?;
        let cancellation = CancellationToken::new();
        let cancel_interrupt = cancellation.clone();
        control
            .cancellation
            .register_interrupt(Arc::new(move || cancel_interrupt.cancel()));
        let address = (target.host.as_str(), target.port);
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(control.timeouts.idle),
            ..Default::default()
        });
        let connect_timeout = control.timeouts.connect.min(control.remaining_overall());
        if connect_timeout.is_zero() {
            return Err(SshTransportError::TimedOut);
        }
        let mut session = runtime.block_on(async {
            tokio::select! {
                () = cancellation.cancelled() => Err(SshTransportError::Cancelled),
                result = tokio::time::timeout(
                    connect_timeout,
                    client::connect(config, address, HostKeyVerifier { expected }),
                ) => match result {
                    Ok(Ok(session)) => Ok(session),
                    Ok(Err(russh::Error::UnknownKey)) => {
                        Err(SshTransportError::HostKeyMismatch)
                    }
                    Ok(Err(error)) => Err(SshTransportError::Transport(error.to_string())),
                    Err(_) => Err(SshTransportError::TimedOut),
                }
            }
        })?;
        let username = target.username.clone();
        let auth_remaining = control.remaining_overall();
        if auth_remaining.is_zero() {
            return Err(SshTransportError::TimedOut);
        }
        let auth_deadline = TokioInstant::now() + auth_remaining;
        let authenticated = runtime.block_on(cancellable_until(
            &cancellation,
            auth_deadline,
            control.timeouts.idle,
            authenticate(&mut session, username, credential),
        ))?;
        if !authenticated {
            return Err(SshTransportError::AuthenticationFailed);
        }
        Ok(Box::new(RusshTransportSession {
            runtime,
            session,
            sftp: None,
            command_subsystem: target.command_subsystem.clone(),
            remote_event_subsystem: target.remote_event_subsystem.clone(),
            cancellation,
            next_directory_cursor_id: 0,
            directory_cursors: BTreeMap::new(),
            directory_cursor_order: VecDeque::new(),
        }))
    }
}

async fn authenticate(
    session: &mut client::Handle<HostKeyVerifier>,
    username: String,
    credential: SshCredential,
) -> Result<bool, SshTransportError> {
    let result = match credential {
        SshCredential::Password(password) => session
            .authenticate_password(username, password.expose_secret())
            .await
            .map_err(|error| SshTransportError::Transport(error.to_string()))?,
        SshCredential::PrivateKeyOpenSsh { key, passphrase } => {
            let private_key = russh::keys::decode_secret_key(
                key.expose_secret(),
                passphrase.as_ref().map(ExposeSecret::expose_secret),
            )
            .map_err(|error| {
                SshTransportError::Transport(format!("invalid resolved private key: {error}"))
            })?;
            let hash = session
                .best_supported_rsa_hash()
                .await
                .map_err(|error| SshTransportError::Transport(error.to_string()))?
                .flatten();
            session
                .authenticate_publickey(
                    username,
                    russh::keys::PrivateKeyWithHashAlg::new(Arc::new(private_key), hash),
                )
                .await
                .map_err(|error| SshTransportError::Transport(error.to_string()))?
        }
    };
    Ok(result.success())
}

impl SshTransportSession for RusshTransportSession {
    fn prepare_operation(&mut self, control: &RemoteOperationControl) {
        self.cancellation = operation_cancellation(control);
    }
    fn execute_argv(
        &mut self,
        argv: &[String],
        output_limit: usize,
        control: &RemoteOperationControl,
    ) -> Result<TransportCommandOutput, SshTransportError> {
        self.execute_argv_input(argv, None, output_limit, control)
    }

    fn execute_argv_with_stdin(
        &mut self,
        argv: &[String],
        stdin: &[u8],
        output_limit: usize,
        control: &RemoteOperationControl,
    ) -> Result<TransportCommandOutput, SshTransportError> {
        self.execute_argv_input(argv, Some(stdin), output_limit, control)
    }

    fn canonicalize(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<String, SshTransportError> {
        let cancellation = self.cancellation.clone();
        let overall = control.remaining_overall();
        let idle = control.timeouts.idle;
        let deadline = TokioInstant::now() + overall;
        let path = path.to_owned();
        let sftp = self.sftp_session(&cancellation, deadline, idle)?;
        self.runtime.block_on(async {
            cancellable_until(&cancellation, deadline, idle, sftp.canonicalize(path)).await
        })
    }

    fn stat(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<RemoteFileStat, SshTransportError> {
        let cancellation = self.cancellation.clone();
        let overall = control.remaining_overall();
        let idle = control.timeouts.idle;
        let deadline = TokioInstant::now() + overall;
        let path = path.to_owned();
        let sftp = self.sftp_session(&cancellation, deadline, idle)?;
        self.runtime.block_on(async {
            let metadata =
                cancellable_until(&cancellation, deadline, idle, sftp.metadata(path.clone()))
                    .await?;
            Ok(RemoteFileStat {
                path,
                size: metadata.len(),
                modified_seconds: modified_seconds(&metadata),
                producer_marker: None,
                sha256: None,
            })
        })
    }

    fn metadata(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<TransportDirEntry, SshTransportError> {
        let cancellation = self.cancellation.clone();
        let overall = control.remaining_overall();
        let idle = control.timeouts.idle;
        let deadline = TokioInstant::now() + overall;
        let path = path.to_owned();
        let sftp = self.sftp_session(&cancellation, deadline, idle)?;
        self.runtime.block_on(async {
            let metadata = cancellable_sftp_until(
                &cancellation,
                deadline,
                idle,
                sftp.symlink_metadata(path.clone()),
            )
            .await?;
            Ok(TransportDirEntry {
                path,
                size: metadata.len(),
                modified_seconds: modified_seconds(&metadata),
                kind: transport_file_kind(&metadata),
            })
        })
    }

    fn read_dir(
        &mut self,
        root: &str,
        control: &RemoteOperationControl,
    ) -> Result<Vec<TransportDirEntry>, SshTransportError> {
        let mut entries = Vec::new();
        let mut continuation = None;
        loop {
            let page = self.read_dir_page(
                root,
                continuation.as_deref(),
                MAX_DIRECTORY_PAGE_ENTRIES,
                control,
            )?;
            entries.extend(page.entries);
            if entries.len() > MAX_DIRECTORY_CANDIDATES {
                return Err(SshTransportError::HelperProtocol(
                    "directory candidate count exceeds legacy hard limit".to_owned(),
                ));
            }
            let Some(next) = page.continuation else {
                return Ok(entries);
            };
            continuation = Some(next);
        }
    }

    fn read_dir_page(
        &mut self,
        root: &str,
        continuation: Option<&str>,
        limit: usize,
        control: &RemoteOperationControl,
    ) -> Result<TransportListPage, SshTransportError> {
        self.prune_directory_cursors();
        let limit = limit.clamp(1, MAX_DIRECTORY_PAGE_ENTRIES);
        let cancellation = self.cancellation.clone();
        let deadline = TokioInstant::now() + control.remaining_overall();
        let idle = control.timeouts.idle;
        let mut cursor = if let Some(token) = continuation {
            let cursor = self
                .directory_cursors
                .remove(token)
                .ok_or(SshTransportError::InvalidContinuation)?;
            self.directory_cursor_order
                .retain(|candidate| candidate != token);
            if cursor.root != root {
                self.close_directory_cursor(cursor);
                return Err(SshTransportError::InvalidContinuation);
            }
            cursor
        } else {
            let root = root.to_owned();
            self.runtime.block_on(async {
                let raw = open_raw_sftp(&self.session, &cancellation, deadline, idle).await?;
                let handle = cancellable_sftp_until(
                    &cancellation,
                    deadline,
                    idle,
                    raw.opendir(root.clone()),
                )
                .await?
                .handle;
                Ok::<_, SshTransportError>(TransportDirectoryCursor {
                    root,
                    raw,
                    handle,
                    pending: VecDeque::new(),
                    last_used: TokioInstant::now(),
                })
            })?
        };
        let page_result = self.runtime.block_on(read_directory_cursor_page(
            &mut cursor,
            limit,
            &cancellation,
            deadline,
            idle,
        ));
        let (entries, exhausted) = match page_result {
            Ok(page) => page,
            Err(error) => {
                self.close_directory_cursor(cursor);
                return Err(error);
            }
        };
        let continuation = if exhausted {
            None
        } else {
            while self.directory_cursors.len() >= MAX_ACTIVE_DIRECTORY_CURSORS {
                let Some(oldest) = self.directory_cursor_order.pop_front() else {
                    break;
                };
                if let Some(cursor) = self.directory_cursors.remove(&oldest) {
                    self.close_directory_cursor(cursor);
                }
            }
            let token = format!("remote-dir-{}", self.next_directory_cursor_id);
            self.next_directory_cursor_id = self.next_directory_cursor_id.wrapping_add(1);
            self.directory_cursor_order.push_back(token.clone());
            cursor.last_used = TokioInstant::now();
            self.directory_cursors.insert(token.clone(), cursor);
            Some(token)
        };
        Ok(TransportListPage {
            entries,
            continuation,
        })
    }

    fn event_paths(
        &mut self,
        root: &str,
        glob: &str,
        control: &RemoteOperationControl,
    ) -> Result<Vec<String>, SshTransportError> {
        let subsystem = self.remote_event_subsystem.clone().ok_or_else(|| {
            SshTransportError::HelperProtocol("remote event subsystem is not configured".to_owned())
        })?;
        let request = encode_watch_request(root, glob)?;
        let cancellation = self.cancellation.clone();
        let overall = control.remaining_overall();
        let idle = control.timeouts.idle;
        let deadline = TokioInstant::now() + overall;
        self.runtime.block_on(async {
            let mut channel = cancellable_until(
                &cancellation,
                deadline,
                idle,
                self.session.channel_open_session(),
            )
            .await?;
            cancellable_until(
                &cancellation,
                deadline,
                idle,
                channel.request_subsystem(true, subsystem),
            )
            .await?;
            cancellable_until(&cancellation, deadline, idle, channel.data_bytes(request)).await?;
            cancellable_until(&cancellation, deadline, idle, channel.eof()).await?;
            let mut response = Vec::new();
            let mut exit_status = None;
            loop {
                let Some(message) = cancellable_until(&cancellation, deadline, idle, async {
                    Ok::<_, io::Error>(channel.wait().await)
                })
                .await?
                else {
                    break;
                };
                match message {
                    ChannelMsg::Data { data } => {
                        let new_len = response.len().checked_add(data.len()).ok_or_else(|| {
                            SshTransportError::HelperProtocol(
                                "event response length overflow".to_owned(),
                            )
                        })?;
                        if new_len > MAX_EVENT_RESPONSE_BYTES {
                            return Err(SshTransportError::HelperProtocol(
                                "event response exceeds hard limit".to_owned(),
                            ));
                        }
                        response.extend_from_slice(&data);
                    }
                    ChannelMsg::ExitStatus {
                        exit_status: status,
                    } => {
                        exit_status = Some(status);
                    }
                    _ => {}
                }
            }
            if exit_status != Some(0) {
                return Err(SshTransportError::HelperProtocol(format!(
                    "event helper did not complete successfully: {exit_status:?}"
                )));
            }
            decode_path_frames(&response)
        })
    }

    fn read_file(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
        consume: &mut dyn FnMut(&[u8]) -> Result<(), SshTransportError>,
    ) -> Result<u64, SshTransportError> {
        let cancellation = self.cancellation.clone();
        let overall = control.remaining_overall();
        let idle = control.timeouts.idle;
        let deadline = TokioInstant::now() + overall;
        let path = path.to_owned();
        let sftp = self.sftp_session(&cancellation, deadline, idle)?;
        let mut file = self.runtime.block_on(async {
            cancellable_until(&cancellation, deadline, idle, sftp.open(path)).await
        })?;
        let read_result: Result<u64, SshTransportError> = self.runtime.block_on(async {
            let mut total = 0_u64;
            let mut chunk = vec![0_u8; READ_CHUNK_BYTES];
            loop {
                let read =
                    cancellable_until(&cancellation, deadline, idle, file.read(&mut chunk)).await?;
                if read == 0 {
                    return Ok(total);
                }
                total = total.checked_add(read as u64).ok_or_else(|| {
                    SshTransportError::Transport("remote read byte count overflow".to_owned())
                })?;
                consume(&chunk[..read])?;
            }
        });
        let discard_without_close = read_result.as_ref().is_err_and(read_error_discards_sftp);
        let close_result = if discard_without_close {
            Ok(())
        } else {
            let close_budget = control.remaining_overall().min(SFTP_CLOSE_TIMEOUT);
            self.runtime
                .block_on(close_sftp_file(&mut file, &cancellation, close_budget))
        };
        drop(file);
        drop(sftp);
        if discard_without_close || close_result.is_err() {
            self.discard_sftp_session();
        }
        finish_sftp_read(read_result, close_result)
    }

    fn write_file_new(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
        produce: &mut dyn FnMut(&mut dyn io::Write) -> Result<(), SshTransportError>,
    ) -> Result<(), SshTransportError> {
        let cancellation = self.cancellation.clone();
        let overall = control.remaining_overall();
        let idle = control.timeouts.idle;
        let deadline = TokioInstant::now() + overall;
        let path = path.to_owned();
        let sftp = self.sftp_session(&cancellation, deadline, idle)?;
        let file = self.runtime.block_on(async {
            cancellable_sftp_until(
                &cancellation,
                deadline,
                idle,
                sftp.open_with_flags(
                    path,
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                ),
            )
            .await
        })?;
        drop(sftp);
        let mut writer = BlockingSftpWriter {
            runtime: &self.runtime,
            file,
            cancellation,
            deadline,
            idle,
            failure: None,
        };
        let produce_result = produce(&mut writer);
        let finish_result = writer.finish();
        if finish_result.is_err() {
            self.close_sftp_session();
        }
        finish_result?;
        produce_result
    }

    fn mkdir(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<(), SshTransportError> {
        let cancellation = self.cancellation.clone();
        let deadline = TokioInstant::now() + control.remaining_overall();
        let idle = control.timeouts.idle;
        let path = path.to_owned();
        let sftp = self.sftp_session(&cancellation, deadline, idle)?;
        self.runtime.block_on(async {
            cancellable_sftp_until(&cancellation, deadline, idle, sftp.create_dir(path)).await
        })
    }

    fn rename(
        &mut self,
        source: &str,
        destination: &str,
        control: &RemoteOperationControl,
    ) -> Result<(), SshTransportError> {
        let cancellation = self.cancellation.clone();
        let deadline = TokioInstant::now() + control.remaining_overall();
        let idle = control.timeouts.idle;
        let source = source.to_owned();
        let destination = destination.to_owned();
        let sftp = self.sftp_session(&cancellation, deadline, idle)?;
        self.runtime.block_on(async {
            cancellable_sftp_until(
                &cancellation,
                deadline,
                idle,
                sftp.rename(source, destination),
            )
            .await
        })
    }

    fn remove_file(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<(), SshTransportError> {
        let cancellation = self.cancellation.clone();
        let deadline = TokioInstant::now() + control.remaining_overall();
        let idle = control.timeouts.idle;
        let path = path.to_owned();
        let sftp = self.sftp_session(&cancellation, deadline, idle)?;
        self.runtime.block_on(async {
            cancellable_sftp_until(&cancellation, deadline, idle, sftp.remove_file(path)).await
        })
    }

    fn remove_dir(
        &mut self,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<(), SshTransportError> {
        let cancellation = self.cancellation.clone();
        let deadline = TokioInstant::now() + control.remaining_overall();
        let idle = control.timeouts.idle;
        let path = path.to_owned();
        let sftp = self.sftp_session(&cancellation, deadline, idle)?;
        self.runtime.block_on(async {
            cancellable_sftp_until(&cancellation, deadline, idle, sftp.remove_dir(path)).await
        })
    }
}

async fn read_directory_cursor_page(
    cursor: &mut TransportDirectoryCursor,
    limit: usize,
    cancellation: &CancellationToken,
    deadline: TokioInstant,
    idle: Duration,
) -> Result<(Vec<TransportDirEntry>, bool), SshTransportError> {
    let mut entries = Vec::with_capacity(limit);
    loop {
        while entries.len() < limit {
            let Some(entry) = cursor.pending.pop_front() else {
                break;
            };
            entries.push(entry);
        }
        if entries.len() == limit {
            return Ok((entries, false));
        }
        let Some(page) =
            next_sftp_directory_response(&cursor.raw, &cursor.handle, cancellation, deadline, idle)
                .await?
        else {
            cancellable_sftp_until(
                cancellation,
                deadline,
                idle,
                cursor.raw.close(cursor.handle.clone()),
            )
            .await?;
            return Ok((entries, true));
        };
        if page.files.len() > MAX_DIRECTORY_CANDIDATES {
            return Err(SshTransportError::HelperProtocol(
                "single SFTP directory response exceeds hard entry limit".to_owned(),
            ));
        }
        let mut metadata_bytes = 0_usize;
        for file in page.files {
            if matches!(file.filename.as_str(), "." | "..") {
                continue;
            }
            metadata_bytes = metadata_bytes
                .checked_add(file.filename.len())
                .and_then(|bytes| bytes.checked_add(file.longname.len()))
                .and_then(|bytes| {
                    bytes.checked_add(file.attrs.user.as_ref().map_or(0, String::len))
                })
                .and_then(|bytes| {
                    bytes.checked_add(file.attrs.group.as_ref().map_or(0, String::len))
                })
                .and_then(|bytes| bytes.checked_add(64))
                .ok_or_else(|| {
                    SshTransportError::HelperProtocol(
                        "directory metadata accounting overflow".to_owned(),
                    )
                })?;
            if metadata_bytes > MAX_DIRECTORY_METADATA_BYTES {
                return Err(SshTransportError::HelperProtocol(
                    "single SFTP directory response exceeds metadata limit".to_owned(),
                ));
            }
            let path = if cursor.root.ends_with('/') {
                format!("{}{}", cursor.root, file.filename)
            } else {
                format!("{}/{}", cursor.root, file.filename)
            };
            cursor.pending.push_back(TransportDirEntry {
                path,
                size: file.attrs.len(),
                modified_seconds: modified_seconds(&file.attrs),
                kind: transport_file_kind(&file.attrs),
            });
        }
    }
}

async fn next_sftp_directory_response(
    raw: &RawSftpSession,
    handle: &str,
    cancellation: &CancellationToken,
    deadline: TokioInstant,
    idle: Duration,
) -> Result<Option<russh_sftp::protocol::Name>, SshTransportError> {
    let timeout = deadline
        .saturating_duration_since(TokioInstant::now())
        .min(idle);
    if timeout.is_zero() {
        return Err(SshTransportError::TimedOut);
    }
    tokio::select! {
        () = cancellation.cancelled() => Err(SshTransportError::Cancelled),
        result = tokio::time::timeout(timeout, raw.readdir(handle.to_owned())) => match result {
            Ok(Ok(page)) => Ok(Some(page)),
            Ok(Err(SftpError::Status(status))) if status.status_code == StatusCode::Eof => Ok(None),
            Ok(Err(error)) => Err(map_sftp_error(error)),
            Err(_) => Err(SshTransportError::TimedOut),
        }
    }
}

async fn open_raw_sftp(
    session: &client::Handle<HostKeyVerifier>,
    cancellation: &CancellationToken,
    deadline: TokioInstant,
    idle: Duration,
) -> Result<RawSftpSession, SshTransportError> {
    let channel =
        cancellable_until(cancellation, deadline, idle, session.channel_open_session()).await?;
    cancellable_until(
        cancellation,
        deadline,
        idle,
        channel.request_subsystem(true, "sftp"),
    )
    .await?;
    let raw = RawSftpSession::new(channel.into_stream());
    cancellable_until(cancellation, deadline, idle, raw.init()).await?;
    Ok(raw)
}

async fn open_sftp(
    session: &client::Handle<HostKeyVerifier>,
    cancellation: &CancellationToken,
    deadline: TokioInstant,
    idle: Duration,
) -> Result<SftpSession, SshTransportError> {
    let channel =
        cancellable_until(cancellation, deadline, idle, session.channel_open_session()).await?;
    cancellable_until(
        cancellation,
        deadline,
        idle,
        channel.request_subsystem(true, "sftp"),
    )
    .await?;
    cancellable_until(
        cancellation,
        deadline,
        idle,
        SftpSession::new(channel.into_stream()),
    )
    .await
}

async fn cancellable_until<F, T, E>(
    cancellation: &CancellationToken,
    deadline: TokioInstant,
    idle: Duration,
    future: F,
) -> Result<T, SshTransportError>
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let timeout = deadline
        .saturating_duration_since(TokioInstant::now())
        .min(idle);
    if timeout.is_zero() {
        return Err(SshTransportError::TimedOut);
    }
    tokio::select! {
        () = cancellation.cancelled() => Err(SshTransportError::Cancelled),
        result = tokio::time::timeout(timeout, future) => match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(SshTransportError::Transport(error.to_string())),
            Err(_) => Err(SshTransportError::TimedOut),
        }
    }
}

async fn cancellable_sftp_until<F, T>(
    cancellation: &CancellationToken,
    deadline: TokioInstant,
    idle: Duration,
    future: F,
) -> Result<T, SshTransportError>
where
    F: Future<Output = Result<T, SftpError>>,
{
    let timeout = deadline
        .saturating_duration_since(TokioInstant::now())
        .min(idle);
    if timeout.is_zero() {
        return Err(SshTransportError::TimedOut);
    }
    tokio::select! {
        () = cancellation.cancelled() => Err(SshTransportError::Cancelled),
        result = tokio::time::timeout(timeout, future) => match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(map_sftp_error(error)),
            Err(_) => Err(SshTransportError::TimedOut),
        }
    }
}

fn map_sftp_error(error: SftpError) -> SshTransportError {
    match error {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => SshTransportError::NotFound(status.error_message),
            StatusCode::PermissionDenied => {
                SshTransportError::PermissionDenied(status.error_message)
            }
            StatusCode::NoConnection | StatusCode::ConnectionLost => {
                SshTransportError::Disconnected(status.error_message)
            }
            StatusCode::OpUnsupported => SshTransportError::Unsupported,
            _ => SshTransportError::Transport(format!(
                "{}: {}",
                status.status_code, status.error_message
            )),
        },
        SftpError::Timeout => SshTransportError::TimedOut,
        SftpError::IO(message) => SshTransportError::Disconnected(message),
        other => SshTransportError::Transport(other.to_string()),
    }
}

fn transport_file_kind(metadata: &russh_sftp::client::fs::Metadata) -> TransportFileKind {
    let file_type = metadata.file_type();
    if file_type.is_file() {
        TransportFileKind::File
    } else if file_type.is_dir() {
        TransportFileKind::Directory
    } else if file_type.is_symlink() {
        TransportFileKind::Symlink
    } else {
        TransportFileKind::Other
    }
}

fn modified_seconds(metadata: &russh_sftp::client::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs())
}

trait CommandRequestChannel {
    async fn request_exec(&self, command: Vec<u8>) -> Result<(), String>;
    async fn request_argv_subsystem(&self, subsystem: String) -> Result<(), String>;
    async fn send_request_data(&self, data: Vec<u8>) -> Result<(), String>;
    async fn send_request_eof(&self) -> Result<(), String>;
}

impl CommandRequestChannel for russh::Channel<client::Msg> {
    async fn request_exec(&self, command: Vec<u8>) -> Result<(), String> {
        self.exec(true, command)
            .await
            .map_err(|error| error.to_string())
    }

    async fn request_argv_subsystem(&self, subsystem: String) -> Result<(), String> {
        self.request_subsystem(true, subsystem)
            .await
            .map_err(|error| error.to_string())
    }

    async fn send_request_data(&self, data: Vec<u8>) -> Result<(), String> {
        self.data_bytes(data)
            .await
            .map_err(|error| error.to_string())
    }

    async fn send_request_eof(&self) -> Result<(), String> {
        self.eof().await.map_err(|error| error.to_string())
    }
}

async fn dispatch_channel_request<C: CommandRequestChannel>(
    channel: &C,
    dispatch: CommandDispatch,
    cancellation: &CancellationToken,
    deadline: TokioInstant,
    idle: Duration,
) -> Result<bool, SshTransportError> {
    match dispatch {
        CommandDispatch::StandardExec(command) => {
            cancellable_until(
                cancellation,
                deadline,
                idle,
                channel.request_exec(command.into_bytes()),
            )
            .await?;
            Ok(true)
        }
        CommandDispatch::ArgvSubsystem { subsystem, request } => {
            cancellable_until(
                cancellation,
                deadline,
                idle,
                channel.request_argv_subsystem(subsystem),
            )
            .await?;
            // subsystem 接受后，中断无法证明远端进程状态；保留有界 evidence。
            let sent = cancellable_until(
                cancellation,
                deadline,
                idle,
                channel.send_request_data(request),
            )
            .await;
            let closed =
                cancellable_until(cancellation, deadline, idle, channel.send_request_eof()).await;
            Ok(sent.is_ok() && closed.is_ok())
        }
    }
}

enum CommandDispatch {
    StandardExec(String),
    ArgvSubsystem { subsystem: String, request: Vec<u8> },
}

fn command_dispatch(
    argv: &[String],
    command_subsystem: Option<&str>,
) -> Result<CommandDispatch, SshTransportError> {
    if let Some(subsystem) = command_subsystem {
        return Ok(CommandDispatch::ArgvSubsystem {
            subsystem: subsystem.to_owned(),
            request: encode_argv_request(argv)?,
        });
    }
    Ok(CommandDispatch::StandardExec(encode_posix_command(argv)?))
}

/// 将每个 argv 元素独立单引号编码；远端 shell 只能恢复原始 argv，不能解释参数内容。
fn encode_posix_command(argv: &[String]) -> Result<String, SshTransportError> {
    if argv.is_empty() {
        return Err(SshTransportError::HelperProtocol(
            "argv must contain a program".to_owned(),
        ));
    }
    let mut command = String::new();
    for (index, argument) in argv.iter().enumerate() {
        if argument.as_bytes().contains(&0) {
            return Err(SshTransportError::HelperProtocol(
                "argv contains NUL".to_owned(),
            ));
        }
        if index != 0 {
            command.push(' ');
        }
        command.push('\'');
        let mut remainder = argument.as_str();
        while let Some(quote) = remainder.find('\'') {
            command.push_str(&remainder[..quote]);
            command.push_str("'\\''");
            remainder = &remainder[quote + 1..];
        }
        command.push_str(remainder);
        command.push('\'');
    }
    Ok(command)
}

fn empty_command_output(output_limit: usize) -> TransportCommandOutput {
    TransportCommandOutput {
        stdout: Vec::with_capacity(output_limit.min(4096)),
        stderr: Vec::with_capacity(output_limit.min(4096)),
        exit_status: None,
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

fn append_bounded(target: &mut Vec<u8>, data: &[u8], limit: usize, truncated: &mut bool) {
    let available = limit.saturating_sub(target.len());
    let retained = available.min(data.len());
    target.extend_from_slice(&data[..retained]);
    *truncated |= retained != data.len();
}

fn encode_argv_request(argv: &[String]) -> Result<Vec<u8>, SshTransportError> {
    if argv.is_empty() {
        return Err(SshTransportError::HelperProtocol(
            "argv must contain a program".to_owned(),
        ));
    }
    let argument_count = u32::try_from(argv.len())
        .map_err(|_| SshTransportError::HelperProtocol("argv count exceeds u32".to_owned()))?;
    let mut request = Vec::with_capacity(64);
    request.extend_from_slice(ARGV_MAGIC);
    request.extend_from_slice(&argument_count.to_be_bytes());
    for argument in argv {
        if argument.as_bytes().contains(&0) {
            return Err(SshTransportError::HelperProtocol(
                "argv contains NUL".to_owned(),
            ));
        }
        let len = u32::try_from(argument.len()).map_err(|_| {
            SshTransportError::HelperProtocol("argv element exceeds u32".to_owned())
        })?;
        request.extend_from_slice(&len.to_be_bytes());
        request.extend_from_slice(argument.as_bytes());
    }
    Ok(request)
}

fn encode_watch_request(root: &str, glob: &str) -> Result<Vec<u8>, SshTransportError> {
    let mut request = Vec::with_capacity(root.len().saturating_add(glob.len()).saturating_add(16));
    request.extend_from_slice(WATCH_MAGIC);
    for value in [root, glob] {
        let len = u32::try_from(value.len()).map_err(|_| {
            SshTransportError::HelperProtocol("watch request field exceeds u32".to_owned())
        })?;
        request.extend_from_slice(&len.to_be_bytes());
        request.extend_from_slice(value.as_bytes());
    }
    Ok(request)
}

fn decode_path_frames(bytes: &[u8]) -> Result<Vec<String>, SshTransportError> {
    let mut cursor = 0_usize;
    let mut paths = Vec::new();
    while cursor < bytes.len() {
        let length_end = cursor.checked_add(4).ok_or_else(|| {
            SshTransportError::HelperProtocol("event frame offset overflow".to_owned())
        })?;
        let length_bytes: [u8; 4] = bytes
            .get(cursor..length_end)
            .ok_or_else(|| {
                SshTransportError::HelperProtocol("truncated event frame length".to_owned())
            })?
            .try_into()
            .map_err(|_| {
                SshTransportError::HelperProtocol("invalid event frame length".to_owned())
            })?;
        let length = usize::try_from(u32::from_be_bytes(length_bytes)).map_err(|_| {
            SshTransportError::HelperProtocol("event path length unsupported".to_owned())
        })?;
        cursor = length_end;
        let path_end = cursor.checked_add(length).ok_or_else(|| {
            SshTransportError::HelperProtocol("event path offset overflow".to_owned())
        })?;
        let path =
            std::str::from_utf8(bytes.get(cursor..path_end).ok_or_else(|| {
                SshTransportError::HelperProtocol("truncated event path".to_owned())
            })?)
            .map_err(|error| SshTransportError::HelperProtocol(error.to_string()))?;
        if path.is_empty() || path.contains('\0') {
            return Err(SshTransportError::HelperProtocol(
                "event path is empty or contains NUL".to_owned(),
            ));
        }
        paths.push(path.to_owned());
        cursor = path_end;
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Debug, PartialEq, Eq)]
    enum RecordedRequest {
        Exec(Vec<u8>),
        Subsystem(String),
        Data(Vec<u8>),
        Eof,
    }

    #[derive(Default)]
    struct RecordingChannel {
        requests: RefCell<Vec<RecordedRequest>>,
    }

    impl CommandRequestChannel for RecordingChannel {
        async fn request_exec(&self, command: Vec<u8>) -> Result<(), String> {
            self.requests
                .borrow_mut()
                .push(RecordedRequest::Exec(command));
            Ok(())
        }

        async fn request_argv_subsystem(&self, subsystem: String) -> Result<(), String> {
            self.requests
                .borrow_mut()
                .push(RecordedRequest::Subsystem(subsystem));
            Ok(())
        }

        async fn send_request_data(&self, data: Vec<u8>) -> Result<(), String> {
            self.requests.borrow_mut().push(RecordedRequest::Data(data));
            Ok(())
        }

        async fn send_request_eof(&self) -> Result<(), String> {
            self.requests.borrow_mut().push(RecordedRequest::Eof);
            Ok(())
        }
    }

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";

    #[test]
    fn absent_host_key_pin_accepts_any_server_key() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let key_a = russh::keys::ssh_key::PublicKey::from_openssh(KEY_A).unwrap();
        let key_b = russh::keys::ssh_key::PublicKey::from_openssh(KEY_B).unwrap();
        let mut verifier = HostKeyVerifier { expected: None };

        runtime.block_on(async {
            assert!(
                client::Handler::check_server_key(&mut verifier, &key_a)
                    .await
                    .unwrap()
            );
            assert!(
                client::Handler::check_server_key(&mut verifier, &key_b)
                    .await
                    .unwrap()
            );
        });
    }

    #[test]
    fn configured_host_key_pin_remains_strict() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let key_a = russh::keys::ssh_key::PublicKey::from_openssh(KEY_A).unwrap();
        let key_b = russh::keys::ssh_key::PublicKey::from_openssh(KEY_B).unwrap();
        let mut verifier = HostKeyVerifier {
            expected: Some(key_a.clone()),
        };

        runtime.block_on(async {
            assert!(
                client::Handler::check_server_key(&mut verifier, &key_a)
                    .await
                    .unwrap()
            );
            assert!(
                !client::Handler::check_server_key(&mut verifier, &key_b)
                    .await
                    .unwrap()
            );
        });
    }

    #[test]
    fn argv_wire_is_length_delimited_and_preserves_injection_text_as_one_argument() {
        let argv = vec![
            "/usr/libexec/camera-toolbox-capture".to_owned(),
            "$(touch /tmp/pwned);'\"".to_owned(),
        ];
        let encoded = encode_argv_request(&argv).unwrap();
        assert_eq!(&encoded[..8], ARGV_MAGIC);
        assert_eq!(u32::from_be_bytes(encoded[8..12].try_into().unwrap()), 2);
        assert!(
            encoded
                .windows(argv[1].len())
                .any(|window| window == argv[1].as_bytes())
        );
    }

    #[test]
    fn posix_single_quote_encoder_covers_shell_metacharacters() {
        for (argument, expected) in [
            ("", "''"),
            ("plain", "'plain'"),
            ("white space\tand\nnewline", "'white space\tand\nnewline'"),
            ("semi;colon", "'semi;colon'"),
            ("$(touch /tmp/substitution)", "'$(touch /tmp/substitution)'"),
            ("`touch /tmp/backtick`", "'`touch /tmp/backtick`'"),
            ("single'quote", "'single'\\''quote'"),
        ] {
            assert_eq!(
                encode_posix_command(&[argument.to_owned()]).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn absent_subsystem_selects_standard_exec_and_configured_subsystem_keeps_ctargv1() {
        let argv = vec![
            "/usr/libexec/camera-toolbox-capture".to_owned(),
            "$(touch /tmp/not-executed); value".to_owned(),
        ];
        let CommandDispatch::StandardExec(command) = command_dispatch(&argv, None).unwrap() else {
            panic!("ordinary SSH must default to standard exec")
        };
        assert_eq!(
            command,
            "'/usr/libexec/camera-toolbox-capture' '$(touch /tmp/not-executed); value'"
        );
        assert!(!command.as_bytes().starts_with(ARGV_MAGIC));

        let CommandDispatch::ArgvSubsystem { subsystem, request } =
            command_dispatch(&argv, Some("camera-toolbox-argv-v1")).unwrap()
        else {
            panic!("configured argv subsystem must retain CTARGV1")
        };
        assert_eq!(subsystem, "camera-toolbox-argv-v1");
        assert!(request.starts_with(ARGV_MAGIC));
    }

    #[test]
    fn no_subsystem_production_channel_dispatch_uses_exec_only() {
        let argv = vec!["/bin/echo".to_owned(), "safe; $(false)".to_owned()];
        let expected = encode_posix_command(&argv).unwrap().into_bytes();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let channel = RecordingChannel::default();
        runtime.block_on(async {
            let cancellation = CancellationToken::new();
            let deadline = TokioInstant::now() + Duration::from_secs(1);
            assert!(
                dispatch_channel_request(
                    &channel,
                    command_dispatch(&argv, None).unwrap(),
                    &cancellation,
                    deadline,
                    Duration::from_secs(1),
                )
                .await
                .unwrap()
            );
        });
        assert_eq!(
            channel.requests.into_inner(),
            vec![RecordedRequest::Exec(expected)]
        );
    }

    #[test]
    fn configured_subsystem_channel_dispatch_sends_ctargv1_frames() {
        let argv = vec!["/bin/echo".to_owned(), "safe".to_owned()];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let channel = RecordingChannel::default();
        runtime.block_on(async {
            let cancellation = CancellationToken::new();
            let deadline = TokioInstant::now() + Duration::from_secs(1);
            assert!(
                dispatch_channel_request(
                    &channel,
                    command_dispatch(&argv, Some("argv-v1")).unwrap(),
                    &cancellation,
                    deadline,
                    Duration::from_secs(1),
                )
                .await
                .unwrap()
            );
        });
        let requests = channel.requests.into_inner();
        assert_eq!(
            requests[0],
            RecordedRequest::Subsystem("argv-v1".to_owned())
        );
        assert!(
            matches!(&requests[1], RecordedRequest::Data(data) if data.starts_with(ARGV_MAGIC))
        );
        assert_eq!(requests[2], RecordedRequest::Eof);
    }

    #[cfg(unix)]
    #[test]
    fn standard_exec_round_trips_inert_arguments_through_posix_shell() {
        let marker = std::env::temp_dir().join(format!(
            "camera-toolbox-ssh-injection-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let values = vec![
            String::new(),
            "white space".to_owned(),
            "single'quote".to_owned(),
            "line one\nline two".to_owned(),
            format!("; touch {}", marker.display()),
            format!("$(touch {})", marker.display()),
            format!("`touch {}`", marker.display()),
        ];
        let mut argv = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "printf '[%s]\\n' \"$@\"".to_owned(),
            "camera-toolbox-test".to_owned(),
        ];
        argv.extend(values.iter().cloned());
        let command = encode_posix_command(&argv).unwrap();
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .output()
            .unwrap();
        assert!(output.status.success());
        let expected: String = values.iter().map(|value| format!("[{value}]\n")).collect();
        assert_eq!(output.stdout, expected.as_bytes());
        assert!(
            !marker.exists(),
            "shell metacharacters escaped argv isolation"
        );
    }

    #[test]
    fn both_command_modes_reject_nul() {
        let argv = vec!["/bin/echo".to_owned(), "bad\0argument".to_owned()];
        assert!(command_dispatch(&argv, None).is_err());
        assert!(command_dispatch(&argv, Some("argv-v1")).is_err());
    }

    #[test]
    fn event_frames_require_complete_utf8_lengths() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&5_u32.to_be_bytes());
        bytes.extend_from_slice(b"/a.raw");
        assert!(matches!(
            decode_path_frames(&bytes),
            Err(SshTransportError::HelperProtocol(_))
        ));
    }

    #[test]
    fn shared_session_cache_initializes_once_and_retries_after_failure() {
        let opens = RefCell::new(0_usize);
        let mut cached = None;
        let first = get_or_try_init_shared(&mut cached, || {
            *opens.borrow_mut() += 1;
            Ok::<_, &'static str>(7_usize)
        })
        .unwrap();
        let second = get_or_try_init_shared(&mut cached, || -> Result<usize, &'static str> {
            panic!("cached session must not be initialized twice")
        })
        .unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(*opens.borrow(), 1);

        let mut retry = None;
        let error = get_or_try_init_shared(&mut retry, || Err::<usize, _>("injected")).unwrap_err();
        assert_eq!(error, "injected");
        assert!(retry.is_none());
        assert_eq!(
            *get_or_try_init_shared(&mut retry, || Ok::<_, &'static str>(9_usize)).unwrap(),
            9
        );
    }

    #[test]
    fn read_cleanup_prioritizes_error_and_discards_fatal_transport() {
        let read_error = SshTransportError::PermissionDenied("consumer rejected chunk".to_owned());
        let close_error = SshTransportError::TimedOut;

        assert_eq!(
            finish_sftp_read::<u64>(Err(read_error.clone()), Err(close_error.clone())),
            Err(read_error.clone())
        );
        assert_eq!(
            finish_sftp_read(Ok(17_u64), Err(close_error.clone())),
            Err(close_error)
        );
        assert_eq!(finish_sftp_read(Ok(17_u64), Ok(())), Ok(17));
        assert!(!read_error_discards_sftp(&read_error));
        assert!(read_error_discards_sftp(&SshTransportError::Cancelled));
        assert!(read_error_discards_sftp(&SshTransportError::TimedOut));
        assert!(read_error_discards_sftp(&SshTransportError::Disconnected(
            "connection closed".to_owned()
        )));
        assert!(read_error_discards_sftp(&SshTransportError::Transport(
            "broken subsystem".to_owned()
        )));
    }

    #[test]
    fn reused_session_binds_cancellation_to_second_operation() {
        let timeouts = camera_toolbox_app::RemoteTimeouts {
            connect: Duration::from_secs(1),
            idle: Duration::from_secs(1),
            overall: Duration::from_secs(1),
        };
        let first_control =
            RemoteOperationControl::new(timeouts, camera_toolbox_app::DumpCancellation::default())
                .unwrap();
        let first_operation = operation_cancellation(&first_control);
        let second_control =
            RemoteOperationControl::new(timeouts, camera_toolbox_app::DumpCancellation::default())
                .unwrap();
        let second_operation = operation_cancellation(&second_control);

        second_control.cancellation.cancel();
        assert!(!first_operation.is_cancelled());
        assert!(second_operation.is_cancelled());
    }

    #[test]
    fn absolute_deadline_bounds_multiple_chunks_cumulatively() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let cancellation = CancellationToken::new();
            let deadline = TokioInstant::now() + Duration::from_millis(50);
            cancellable_until(&cancellation, deadline, Duration::from_secs(1), async {
                Ok::<_, io::Error>(())
            })
            .await
            .unwrap();
            tokio::time::advance(Duration::from_millis(40)).await;
            let error = cancellable_until(&cancellation, deadline, Duration::from_secs(1), async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok::<_, io::Error>(())
            })
            .await
            .unwrap_err();
            assert_eq!(error, SshTransportError::TimedOut);
        });
    }
}
