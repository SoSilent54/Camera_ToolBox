//! Still Dump 的 typed service、请求、取消、超时与终态错误。

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use camera_toolbox_core::{AssetId, EphemeralAsset};
use thiserror::Error;

use crate::{CaptureStore, CaptureStoreError};

/// 仅允许已由抓包和真机闭环验证的请求组合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifiedDumpKind {
    Raw10,
    Raw12,
    Jpeg,
    Nv21,
}

impl VerifiedDumpKind {
    #[must_use]
    pub const fn raw_bit_depth(self) -> Option<u8> {
        match self {
            Self::Raw10 => Some(10),
            Self::Raw12 => Some(12),
            Self::Jpeg | Self::Nv21 => None,
        }
    }
}

/// 单帧 still Dump 命令；资产标识由调用方显式提供。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDumpRequest {
    pub kind: VerifiedDumpKind,
    pub asset_id: AssetId,
    pub source_name: String,
}

impl VerifiedDumpRequest {
    /// # Errors
    ///
    /// source name 为空时拒绝创建请求。
    pub fn new(
        kind: VerifiedDumpKind,
        asset_id: AssetId,
        source_name: impl Into<String>,
    ) -> Result<Self, DumpServiceError> {
        let source_name = source_name.into();
        if source_name.trim().is_empty() {
            return Err(DumpServiceError::InvalidRequest(
                "dump source name must not be empty".to_owned(),
            ));
        }
        Ok(Self {
            kind,
            asset_id,
            source_name,
        })
    }
}

/// 已验证响应携带的类型化 source 描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DumpSourceDescriptor {
    Raw {
        width: u32,
        height: u32,
        stride: usize,
        bit_depth: u8,
        metadata_words: [u32; 38],
    },
    Jpeg {
        width: u16,
        height: u16,
        payload_len: usize,
        reserved: [u8; 32],
    },
    Nv21 {
        width: u32,
        height: u32,
        y_stride: usize,
        chroma_stride: usize,
        metadata_words: [u32; 8],
    },
}

impl DumpSourceDescriptor {
    /// 用 checked arithmetic 推导 source 长度。
    ///
    /// # Errors
    ///
    /// metadata 尺寸无法表示于当前 host 或乘加溢出时返回协议错误。
    pub fn checked_payload_len(&self) -> Result<usize, DumpServiceError> {
        let overflow = || DumpServiceError::ProtocolViolation {
            stage: DumpStage::ReceivingMetadata,
            reason: "metadata-derived payload length overflows host address space".to_owned(),
        };
        match self {
            Self::Raw { stride, height, .. } => {
                let height = usize::try_from(*height).map_err(|_| overflow())?;
                stride.checked_mul(height).ok_or_else(overflow)
            }
            Self::Jpeg { payload_len, .. } => Ok(*payload_len),
            Self::Nv21 {
                height,
                y_stride,
                chroma_stride,
                ..
            } => {
                let height = usize::try_from(*height).map_err(|_| overflow())?;
                let y_bytes = y_stride.checked_mul(height).ok_or_else(&overflow)?;
                let chroma_bytes = chroma_stride
                    .checked_mul(height / 2)
                    .ok_or_else(&overflow)?;
                y_bytes.checked_add(chroma_bytes).ok_or_else(overflow)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DumpEnvelope {
    pub progress_tokens: u8,
    pub frame_count: u32,
    pub block_length: usize,
}

/// 服务完成后返回 store 中的同一份 source ownership 与协议摘要。
#[derive(Debug, Clone)]
pub struct DumpOperationResult {
    pub asset: Arc<EphemeralAsset>,
    pub descriptor: DumpSourceDescriptor,
    pub envelope: DumpEnvelope,
    pub payload_sha256: String,
    pub response_checksum: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpStage {
    Initializing,
    Connecting,
    Requesting,
    ReceivingMetadata,
    ReceivingPayload,
    Verifying,
    Publishing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpTruncationStage {
    Marker,
    Envelope,
    Metadata,
    Payload,
    Checksum,
    ErrorText,
}

/// 每个读取阶段的明确终态；协议 EOF 不折叠为普通 IO 错误。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DumpServiceError {
    #[error("invalid dump request: {0}")]
    InvalidRequest(String),
    #[error("validated initialization recipe is unavailable: {recipe_id}")]
    InitializationRecipeUnavailable { recipe_id: String },
    #[error("validated initialization recipe {recipe_id} failed: {reason}")]
    InitializationFailed { recipe_id: String, reason: String },
    #[error("failed to resolve dump endpoint {endpoint}: {reason}")]
    AddressResolution { endpoint: String, reason: String },
    #[error("dump connect timed out after {timeout_ms} ms")]
    ConnectTimeout { timeout_ms: u64 },
    #[error("dump idle timeout while in {stage:?} after {timeout_ms} ms")]
    IdleTimeout { stage: DumpStage, timeout_ms: u64 },
    #[error("dump overall deadline exceeded while in {stage:?}")]
    DeadlineExceeded { stage: DumpStage },
    #[error("dump cancelled while in {stage:?}")]
    Cancelled { stage: DumpStage },
    #[error("dump response truncated in {stage:?}: expected {expected}, received {received}")]
    Truncated {
        stage: DumpTruncationStage,
        expected: usize,
        received: usize,
    },
    #[error("peer closed before checksum completed: received {received} of 4 bytes")]
    PeerClosedBeforeChecksum { received: usize },
    #[error("server rejected dump: {message}")]
    ServerRejected { message: String },
    #[error("dump protocol violation in {stage:?}: {reason}")]
    ProtocolViolation { stage: DumpStage, reason: String },
    #[error("declared response length {declared} exceeds hard limit {limit}")]
    ResponseTooLarge { declared: usize, limit: usize },
    #[error("response checksum mismatch: received 0x{received:08x}, calculated 0x{calculated:08x}")]
    ChecksumMismatch { received: u32, calculated: u32 },
    #[error("failed to allocate final payload buffer of {bytes} bytes")]
    AllocationFailed { bytes: usize },
    #[error("dump transport error while in {stage:?}: {reason}")]
    Transport { stage: DumpStage, reason: String },
    #[error(transparent)]
    Store(#[from] CaptureStoreError),
}

/// 单次 operation 的三个 deadline；overall 覆盖初始化、连接、请求与接收。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DumpTimeouts {
    pub connect: Duration,
    pub idle: Duration,
    pub overall: Duration,
}

impl Default for DumpTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            idle: Duration::from_secs(10),
            overall: Duration::from_secs(30),
        }
    }
}

impl DumpTimeouts {
    /// # Errors
    ///
    /// 任一 deadline 为零，或 connect/idle 超过 overall 时返回错误。
    pub fn validate(self) -> Result<Self, DumpServiceError> {
        if self.connect.is_zero() || self.idle.is_zero() || self.overall.is_zero() {
            return Err(DumpServiceError::InvalidRequest(
                "dump timeouts must be non-zero".to_owned(),
            ));
        }
        if self.connect > self.overall || self.idle > self.overall {
            return Err(DumpServiceError::InvalidRequest(
                "connect and idle timeouts must not exceed the overall deadline".to_owned(),
            ));
        }
        Ok(self)
    }
}

type Interrupt = Arc<dyn Fn() + Send + Sync>;
type StageReporter = Arc<dyn Fn(DumpStage) + Send + Sync>;

#[derive(Default)]
struct CancellationInner {
    requested: AtomicBool,
    interrupt: Mutex<Option<Interrupt>>,
}

/// 可克隆的 operation cancellation；adapter 注册的 interrupt 必须关闭其自有 socket。
#[derive(Clone, Default)]
pub struct DumpCancellation {
    inner: Arc<CancellationInner>,
}

impl DumpCancellation {
    pub fn cancel(&self) {
        self.inner.requested.store(true, Ordering::Release);
        let interrupt = self
            .inner
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
        self.inner.requested.load(Ordering::Acquire)
    }

    /// 安装当前 transport 的关闭函数；取消与安装并发时也保证至少执行一次。
    pub fn register_interrupt(&self, interrupt: Interrupt) {
        let mut slot = self
            .inner
            .interrupt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(Arc::clone(&interrupt));
        if self.is_cancelled() {
            interrupt();
        }
    }

    pub fn clear_interrupt(&self) {
        *self
            .inner
            .interrupt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// 由 controller 创建并传入 service 的 operation control。
#[derive(Clone)]
pub struct DumpOperationControl {
    pub timeouts: DumpTimeouts,
    pub cancellation: DumpCancellation,
    reporter: Option<StageReporter>,
}

impl DumpOperationControl {
    /// # Errors
    ///
    /// timeout 组合无效时返回错误。
    pub fn new(
        timeouts: DumpTimeouts,
        cancellation: DumpCancellation,
    ) -> Result<Self, DumpServiceError> {
        Ok(Self {
            timeouts: timeouts.validate()?,
            cancellation,
            reporter: None,
        })
    }

    #[must_use]
    pub fn with_reporter(mut self, reporter: StageReporter) -> Self {
        self.reporter = Some(reporter);
        self
    }

    pub fn report(&self, stage: DumpStage) {
        if let Some(reporter) = &self.reporter {
            reporter(stage);
        }
    }
}

/// 单次调用拥有一条 one-shot TCP 连接，并将已验证 source 直接发布到 `CaptureStore`。
pub trait DumpService: Send + Sync {
    fn service_id(&self) -> &str;

    /// # Errors
    ///
    /// 连接、deadline、取消、协议校验、内存预算或发布失败时返回 typed error。
    fn capture(
        &self,
        operation_id: crate::platform::OperationId,
        request: VerifiedDumpRequest,
        control: DumpOperationControl,
        store: &CaptureStore,
    ) -> Result<DumpOperationResult, DumpServiceError>;
}

/// Stream 只接受经协议验证的 H.26x 自动协商请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamOpenRequest {
    pub channel: u16,
    pub media: String,
    pub cseq: u32,
    pub recording: StreamRecordingRequest,
}

impl StreamOpenRequest {
    /// # Errors
    ///
    /// channel 超过协议字节范围、media 非安全 token、录制目标冲突或 quota 无效时返回错误。
    pub fn validate(&self) -> Result<(), StreamServiceError> {
        if self.channel > u16::from(u8::MAX) {
            return Err(StreamServiceError::InvalidRequest(
                "stream channel must fit one interleaved byte".to_owned(),
            ));
        }
        if self.media.trim().is_empty()
            || self.media.contains(['\r', '\n'])
            || !self
                .media
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(StreamServiceError::InvalidRequest(
                "stream media must be a non-empty URI-safe token".to_owned(),
            ));
        }
        self.recording.validate()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamRecordingRequest {
    /// 显式用户目标；`None` 时绝不写 transport evidence。
    pub transport_destination: Option<std::path::PathBuf>,
    /// 显式用户目标；`None` 时绝不写 Annex-B。
    pub annexb_destination: Option<std::path::PathBuf>,
    /// Annex-B 启用时必须同时由用户显式确认的 RTP timestamp sidecar 目标。
    pub timestamp_sidecar_destination: Option<std::path::PathBuf>,
    pub quota_bytes: u64,
}

impl StreamRecordingRequest {
    fn validate(&self) -> Result<(), StreamServiceError> {
        if self.annexb_destination.is_some() != self.timestamp_sidecar_destination.is_some() {
            return Err(StreamServiceError::InvalidRequest(
                "Annex-B recording and RTP timestamp sidecar destinations must be supplied together"
                    .to_owned(),
            ));
        }
        let enabled = self.transport_destination.is_some() || self.annexb_destination.is_some();
        if enabled && self.quota_bytes == 0 {
            return Err(StreamServiceError::InvalidRequest(
                "recording quota must be non-zero".to_owned(),
            ));
        }
        let mut destinations = Vec::new();
        for destination in [
            self.transport_destination.as_ref(),
            self.annexb_destination.as_ref(),
            self.timestamp_sidecar_destination.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if destination.as_os_str().is_empty() {
                return Err(StreamServiceError::InvalidRequest(
                    "recording destination must not be empty".to_owned(),
                ));
            }
            let normalized = std::path::absolute(destination).map_err(|error| {
                StreamServiceError::InvalidRequest(format!(
                    "cannot normalize recording destination {}: {error}",
                    destination.display()
                ))
            })?;
            if destinations.contains(&normalized) {
                return Err(StreamServiceError::InvalidRequest(
                    "recording destinations must be distinct".to_owned(),
                ));
            }
            destinations.push(normalized);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamTimeouts {
    pub connect: Duration,
    pub idle: Duration,
}

impl Default for StreamTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(3),
            idle: Duration::from_secs(10),
        }
    }
}

impl StreamTimeouts {
    pub fn validate(self) -> Result<Self, StreamServiceError> {
        if self.connect.is_zero() || self.idle.is_zero() {
            return Err(StreamServiceError::InvalidRequest(
                "stream timeouts must be non-zero".to_owned(),
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamStage {
    Connecting,
    Negotiating,
    ConfirmingRtp,
    Playing,
    Closing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamCodec {
    H264,
    H265,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMediaInfo {
    pub session: Option<String>,
    pub codec: StreamCodec,
    pub payload_type: u8,
    pub ssrc: u32,
    pub width: u32,
    pub height: u32,
    pub clock_rate: u32,
    pub frame_rate: u32,
    pub bitrate_value: u32,
    pub rtp_channel: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamMetrics {
    pub network_bytes: u64,
    pub rtp_packets: u64,
    pub rtp_gaps: u64,
    pub preview_dropped: u64,
    pub decoder_input_dropped: u64,
    pub decoder_resyncs: u64,
    pub decoded_frames: u64,
    pub presented_frames: u64,
    pub transport_queue_bytes: usize,
    pub recorder_queue_bytes: usize,
    pub preview_queue_depth: usize,
    pub decoder_queue_depth: usize,
    pub record_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingBranch {
    TransportEvidence,
    AnnexB,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingState {
    Started,
    FailedIncomplete { reason: String, bytes_written: u64 },
    Completed { bytes_written: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamTerminal {
    BoundaryClosed,
    Cancelled,
    Failed(StreamServiceError),
    Forced { remote_state_unknown: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamServiceEvent {
    Stage(StreamStage),
    Negotiated(StreamMediaInfo),
    DecoderUnavailable {
        reason: String,
    },
    Recording {
        branch: RecordingBranch,
        state: RecordingState,
    },
    Metrics(StreamMetrics),
    Terminal(StreamTerminal),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StreamServiceError {
    #[error("invalid stream request: {0}")]
    InvalidRequest(String),
    #[error("failed to resolve stream endpoint {endpoint}: {reason}")]
    AddressResolution { endpoint: String, reason: String },
    #[error("stream connect timed out after {timeout_ms} ms")]
    ConnectTimeout { timeout_ms: u64 },
    #[error("stream idle timeout in {stage:?} after {timeout_ms} ms")]
    IdleTimeout { stage: StreamStage, timeout_ms: u64 },
    #[error("stream cancelled in {stage:?}")]
    Cancelled { stage: StreamStage },
    #[error("stream negotiation failed: {0}")]
    Negotiation(String),
    #[error("stream protocol violation: {0}")]
    ProtocolViolation(String),
    #[error("stream record truncated in {stage}: expected {expected}, received {received}")]
    TruncatedRecord {
        stage: String,
        expected: usize,
        received: usize,
    },
    #[error("unsupported stream framing: {0}")]
    UnsupportedFraming(String),
    #[error("stream transport failed in {stage:?}: {reason}")]
    Transport { stage: StreamStage, reason: String },
    #[error("stream worker failed to start: {0}")]
    WorkerStart(String),
}

/// FFmpeg rawvideo 的 host-output 时间语义；rawvideo 不伪造源 RTP PTS。
#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
    pub host_output_time: std::time::SystemTime,
    pub source_rtp_pts: Option<u32>,
    pub revision: u64,
}

#[derive(Default)]
pub struct LatestDecodedFrameSlot {
    frame: Mutex<Option<Arc<DecodedVideoFrame>>>,
    revision: std::sync::atomic::AtomicU64,
}

impl LatestDecodedFrameSlot {
    pub fn publish(&self, width: u32, height: u32, rgba: Arc<[u8]>) {
        let revision = self
            .revision
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            .saturating_add(1);
        let frame = Arc::new(DecodedVideoFrame {
            width,
            height,
            rgba,
            host_output_time: std::time::SystemTime::now(),
            source_rtp_pts: None,
            revision,
        });
        *self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(frame);
    }

    #[must_use]
    pub fn latest(&self) -> Option<Arc<DecodedVideoFrame>> {
        self.frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn clear(&self) {
        *self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

type StreamInterrupt = Arc<dyn Fn() + Send + Sync>;
type StreamReporter = Arc<dyn Fn(StreamServiceEvent) + Send + Sync>;

#[derive(Default)]
struct StreamCancellationInner {
    requested: AtomicBool,
    force_requested: AtomicBool,
    interrupts: Mutex<Vec<StreamInterrupt>>,
    force_cleanup: Mutex<Vec<StreamInterrupt>>,
}

#[derive(Clone, Default)]
pub struct StreamCancellation {
    inner: Arc<StreamCancellationInner>,
}

impl StreamCancellation {
    pub fn cancel(&self) {
        self.inner.requested.store(true, Ordering::Release);
        let interrupts = self
            .inner
            .interrupts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for interrupt in interrupts {
            interrupt();
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.requested.load(Ordering::Acquire)
    }

    pub fn register_interrupt(&self, interrupt: StreamInterrupt) {
        self.inner
            .interrupts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::clone(&interrupt));
        if self.is_cancelled() {
            interrupt();
        }
    }

    pub fn register_force_cleanup(&self, cleanup: StreamInterrupt) {
        let invoke_now = {
            let mut callbacks = self
                .inner
                .force_cleanup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.inner.force_requested.load(Ordering::Acquire) {
                true
            } else {
                callbacks.push(Arc::clone(&cleanup));
                false
            }
        };
        if invoke_now {
            cleanup();
        }
    }

    pub fn force_cleanup(&self) {
        self.cancel();
        let callbacks = {
            let mut callbacks = self
                .inner
                .force_cleanup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.inner.force_requested.swap(true, Ordering::AcqRel) {
                return;
            }
            std::mem::take(&mut *callbacks)
        };
        for callback in callbacks {
            callback();
        }
    }
}

#[derive(Clone)]
pub struct StreamOperationControl {
    pub timeouts: StreamTimeouts,
    pub cancellation: StreamCancellation,
    reporter: StreamReporter,
    terminal_reported: Arc<AtomicBool>,
}

impl StreamOperationControl {
    pub fn new(
        timeouts: StreamTimeouts,
        cancellation: StreamCancellation,
        reporter: StreamReporter,
    ) -> Result<Self, StreamServiceError> {
        Ok(Self {
            timeouts: timeouts.validate()?,
            cancellation,
            reporter,
            terminal_reported: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn report(&self, event: StreamServiceEvent) {
        if !self.terminal_reported.load(Ordering::Acquire)
            && !matches!(event, StreamServiceEvent::Terminal(_))
        {
            (self.reporter)(event);
        }
    }

    pub fn finish(&self, terminal: StreamTerminal) -> bool {
        if self
            .terminal_reported
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            (self.reporter)(StreamServiceEvent::Terminal(terminal));
            true
        } else {
            false
        }
    }
}

pub struct StreamSession {
    pub session_id: crate::platform::StreamSessionId,
    pub latest_frame: Arc<LatestDecodedFrameSlot>,
    pub cancellation: StreamCancellation,
    control: StreamOperationControl,
}

impl StreamSession {
    #[must_use]
    pub fn new(
        session_id: crate::platform::StreamSessionId,
        latest_frame: Arc<LatestDecodedFrameSlot>,
        control: StreamOperationControl,
    ) -> Self {
        Self {
            session_id,
            latest_frame,
            cancellation: control.cancellation.clone(),
            control,
        }
    }

    pub fn request_close(&self) {
        self.cancellation.cancel();
    }

    pub fn force_cleanup(&self) -> bool {
        let claimed = self.control.finish(StreamTerminal::Forced {
            remote_state_unknown: true,
        });
        if claimed {
            self.cancellation.force_cleanup();
        }
        claimed
    }
}

pub trait StreamService: Send + Sync {
    fn service_id(&self) -> &str;

    /// 创建一条独立持久 TCP 会话并立即返回；网络与解码均不得阻塞调用线程。
    fn open(
        &self,
        session_id: crate::platform::StreamSessionId,
        request: StreamOpenRequest,
        control: StreamOperationControl,
    ) -> Result<StreamSession, StreamServiceError>;
}

#[cfg(test)]
mod stream_cancellation_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::StreamCancellation;

    #[test]
    fn force_cleanup_registered_after_force_is_invoked_immediately() {
        let cancellation = StreamCancellation::default();
        cancellation.force_cleanup();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);

        cancellation.register_force_cleanup(Arc::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        }));

        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
