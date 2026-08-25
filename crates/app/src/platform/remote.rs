//! SSH-managed command 与 remote-file 的同步 app port；async runtime 由 adapter 隔离。

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use camera_toolbox_core::{AssetId, EphemeralAsset, MediaFormat};
use thiserror::Error;

use crate::{CaptureStore, CaptureStoreError};

use super::{DumpCancellation, OperationId};

/// recipe 参数只允许显式 typed 值；不存在任意 command-line 或 shell 文本入口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandParameter {
    Signed(i64),
    Unsigned(u64),
    Bool(bool),
    Choice(String),
    RemotePath(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedCommandRequest {
    pub recipe_id: String,
    pub parameters: BTreeMap<String, CommandParameter>,
}

impl TypedCommandRequest {
    /// # Errors
    ///
    /// recipe id 为空时拒绝请求。
    pub fn new(recipe_id: impl Into<String>) -> Result<Self, CommandServiceError> {
        let recipe_id = recipe_id.into();
        if recipe_id.trim().is_empty() {
            return Err(CommandServiceError::InvalidRequest(
                "command recipe id must not be empty".to_owned(),
            ));
        }
        Ok(Self {
            recipe_id,
            parameters: BTreeMap::new(),
        })
    }

    #[must_use]
    pub fn with_parameter(mut self, name: impl Into<String>, value: CommandParameter) -> Self {
        self.parameters.insert(name.into(), value);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandTerminal {
    Succeeded,
    ExitFailure { status: u32 },
    OutputTruncated { status: Option<u32> },
    RemoteStateUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub terminal: CommandTerminal,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub artifact_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteStage {
    ResolvingCredential,
    Connecting,
    Executing,
    Discovering,
    WaitingStable,
    Reserving,
    Fetching,
    Verifying,
    Publishing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteTimeouts {
    pub connect: Duration,
    pub idle: Duration,
    pub overall: Duration,
}

impl Default for RemoteTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            idle: Duration::from_secs(10),
            overall: Duration::from_secs(60),
        }
    }
}

impl RemoteTimeouts {
    /// # Errors
    ///
    /// 任一 timeout 为零或阶段 timeout 超过 overall 时拒绝。
    pub fn validate(self) -> Result<Self, RemoteServiceError> {
        if self.connect.is_zero() || self.idle.is_zero() || self.overall.is_zero() {
            return Err(RemoteServiceError::InvalidRequest(
                "remote timeouts must be non-zero".to_owned(),
            ));
        }
        if self.connect > self.overall || self.idle > self.overall {
            return Err(RemoteServiceError::InvalidRequest(
                "connect and idle timeouts must not exceed overall".to_owned(),
            ));
        }
        Ok(self)
    }
}

type RemoteStageReporter = Arc<dyn Fn(RemoteStage) + Send + Sync>;

/// Command/RemoteFile operation 的取消、deadline 与阶段上报。
#[derive(Clone)]
pub struct RemoteOperationControl {
    pub timeouts: RemoteTimeouts,
    pub cancellation: DumpCancellation,
    started: Instant,
    reporter: Option<RemoteStageReporter>,
}

impl RemoteOperationControl {
    /// # Errors
    ///
    /// timeout 组合无效时返回错误。
    pub fn new(
        timeouts: RemoteTimeouts,
        cancellation: DumpCancellation,
    ) -> Result<Self, RemoteServiceError> {
        Ok(Self {
            timeouts: timeouts.validate()?,
            cancellation,
            started: Instant::now(),
            reporter: None,
        })
    }

    #[must_use]
    pub fn with_reporter(mut self, reporter: RemoteStageReporter) -> Self {
        self.reporter = Some(reporter);
        self
    }

    pub fn report(&self, stage: RemoteStage) {
        if let Some(reporter) = &self.reporter {
            reporter(stage);
        }
    }

    #[must_use]
    pub fn remaining_overall(&self) -> Duration {
        self.timeouts.overall.saturating_sub(self.started.elapsed())
    }

    #[must_use]
    pub fn deadline_expired(&self) -> bool {
        self.started.elapsed() >= self.timeouts.overall
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CommandServiceError {
    #[error("invalid command request: {0}")]
    InvalidRequest(String),
    #[error("command recipe is not allowlisted: {recipe_id}")]
    RecipeNotAllowed { recipe_id: String },
    #[error("invalid parameter {parameter}: {reason}")]
    InvalidParameter { parameter: String, reason: String },
    #[error("credential reference could not be resolved: {0}")]
    CredentialResolution(String),
    #[error("SSH host key mismatch")]
    HostKeyMismatch,
    #[error("SSH authentication failed")]
    AuthenticationFailed,
    #[error("remote command cancelled")]
    Cancelled,
    #[error("remote command deadline exceeded")]
    DeadlineExceeded,
    #[error("remote command transport failure: {0}")]
    Transport(String),
    #[error("remote argv helper protocol failure: {0}")]
    HelperProtocol(String),
}

pub trait CommandService: Send + Sync {
    fn service_id(&self) -> &str;

    /// 执行 allowlisted recipe。实现只能向 transport 传 argv，不接受 shell command string。
    fn execute(
        &self,
        request: TypedCommandRequest,
        control: RemoteOperationControl,
    ) -> Result<CommandResult, CommandServiceError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFileStat {
    pub path: String,
    pub size: u64,
    pub modified_seconds: u64,
    /// 已验证 producer helper 的完成标记；存在时无需 size/mtime 多点推断。
    pub producer_marker: Option<String>,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFileRequest {
    pub asset_id: AssetId,
    pub remote_path: String,
    pub expected_sha256: Option<String>,
    pub format: MediaFormat,
    pub source_name: String,
}

impl RemoteFileRequest {
    /// # Errors
    ///
    /// path 或 source name 为空时拒绝请求。
    pub fn validate(&self) -> Result<(), RemoteServiceError> {
        if self.remote_path.is_empty() || self.remote_path.contains('\0') {
            return Err(RemoteServiceError::InvalidRequest(
                "remote path must be non-empty and contain no NUL".to_owned(),
            ));
        }
        if self.source_name.trim().is_empty() {
            return Err(RemoteServiceError::InvalidRequest(
                "source name must not be empty".to_owned(),
            ));
        }
        if self
            .expected_sha256
            .as_ref()
            .is_some_and(|hash| !is_sha256_hex(hash))
        {
            return Err(RemoteServiceError::InvalidRequest(
                "expected SHA-256 must be exactly 64 hexadecimal digits".to_owned(),
            ));
        }
        Ok(())
    }
}

fn is_sha256_hex(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteDiscoverySource {
    CommandResultPath,
    RemoteEventWatch,
    DirectoryPolling,
    ExplicitPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDiscoveryRequest {
    /// command 输出的精确 path；存在时必须优先，且不再调用 watcher/polling。
    pub command_result_path: Option<String>,
    pub asset_id_prefix: String,
    pub format: MediaFormat,
}

#[derive(Debug, Clone)]
pub struct RemoteFileOperationResult {
    pub asset: Arc<EphemeralAsset>,
    pub remote_stat: RemoteFileStat,
    pub payload_sha256: String,
    pub discovery_source: RemoteDiscoverySource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteWatchRequest {
    pub asset_id_prefix: String,
    pub format: MediaFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOpenDisposition {
    None,
    Background,
}

#[derive(Debug, Clone)]
pub enum RemoteWatchEvent {
    AssetReady {
        result: RemoteFileOperationResult,
        open: RemoteOpenDisposition,
    },
    CandidateFailed {
        path: String,
        error: RemoteServiceError,
    },
    Terminal(RemoteWatchTerminal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteWatchTerminal {
    Cancelled,
    DeadlineReached,
    Completed,
}

pub type RemoteWatchReporter = Arc<dyn Fn(RemoteWatchEvent) + Send + Sync>;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RemoteServiceError {
    #[error("invalid remote-file request: {0}")]
    InvalidRequest(String),
    #[error("remote path is outside the configured root or glob: {path}")]
    PathNotAllowed { path: String },
    #[error("no matching remote artifact candidate")]
    NoCandidate,
    #[error("remote file {path} shrank from {previous} to {current} bytes")]
    FileTruncatedBeforeStable {
        path: String,
        previous: u64,
        current: u64,
    },
    #[error("remote file {path} did not become stable")]
    FileNotStable { path: String },
    #[error("remote payload size {declared} exceeds hard limit {limit}")]
    PayloadTooLarge { declared: u64, limit: u64 },
    #[error("remote payload length {declared} cannot fit this host")]
    PayloadLengthUnsupported { declared: u64 },
    #[error("remote payload truncated: expected {expected}, received {received}")]
    Truncated { expected: u64, received: u64 },
    #[error("remote payload grew past preflight size {expected}: received at least {received}")]
    GrewDuringFetch { expected: u64, received: u64 },
    #[error("remote file changed during fetch")]
    ChangedDuringFetch,
    #[error("remote SHA-256 mismatch: expected {expected}, calculated {calculated}")]
    HashMismatch {
        expected: String,
        calculated: String,
    },
    #[error("failed to allocate remote payload buffer of {bytes} bytes")]
    AllocationFailed { bytes: usize },
    #[error("credential reference could not be resolved: {0}")]
    CredentialResolution(String),
    #[error("SSH host key mismatch")]
    HostKeyMismatch,
    #[error("SSH authentication failed")]
    AuthenticationFailed,
    #[error("remote operation cancelled while in {stage:?}")]
    Cancelled { stage: RemoteStage },
    #[error("remote operation deadline exceeded while in {stage:?}")]
    DeadlineExceeded { stage: RemoteStage },
    #[error("remote state is unknown after transport loss while in {stage:?}")]
    RemoteStateUnknown { stage: RemoteStage },
    #[error("remote transport failure while in {stage:?}: {reason}")]
    Transport { stage: RemoteStage, reason: String },
    #[error("remote helper protocol failure: {0}")]
    HelperProtocol(String),
    #[error(transparent)]
    Store(#[from] CaptureStoreError),
}

pub trait RemoteFileService: Send + Sync {
    fn service_id(&self) -> &str;

    fn stat(
        &self,
        remote_path: &str,
        control: RemoteOperationControl,
    ) -> Result<RemoteFileStat, RemoteServiceError>;

    fn fetch(
        &self,
        operation_id: OperationId,
        request: RemoteFileRequest,
        control: RemoteOperationControl,
        store: &CaptureStore,
    ) -> Result<RemoteFileOperationResult, RemoteServiceError>;

    /// CommandResultPath → validated event helper → root/glob directory polling。
    fn discover_and_fetch(
        &self,
        operation_id: OperationId,
        request: RemoteDiscoveryRequest,
        control: RemoteOperationControl,
        store: &CaptureStore,
    ) -> Result<Vec<RemoteFileOperationResult>, RemoteServiceError>;

    /// 运行 cancellable passive watcher；实现必须去重，且只能发出 None/Background 打开语义。
    fn watch(
        &self,
        operation_id: OperationId,
        request: RemoteWatchRequest,
        control: RemoteOperationControl,
        store: &CaptureStore,
        reporter: RemoteWatchReporter,
    ) -> Result<RemoteWatchTerminal, RemoteServiceError>;
}
