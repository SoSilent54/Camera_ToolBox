//! 设备控制节点实现。
//!
//! I²C/EEPROM 的旧自由配置控制路径已移除；新的 map/inspect/approval/executor
//! 链位于 `i2c_plan_nodes`，本模块只保留其他设备控制节点。

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use camera_toolbox_core::Rgba8Frame;

use crate::engine::{
    BayerPattern, CaptureMode, CaptureRequest, CaptureTarget, DataPacket, FrameProvenance,
    ImageFrame, ImageFrameFormat, ImageFrameIdentity, ImagePlane, NodeAction, NodeError,
    NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec, RawMetadata,
};
use crate::platform::{
    CommandResult, ControlTargetSpec, DumpCancellation, HexArmJointPositionsRequest,
    HexArmTargetConfig, HexArmTransport, LatestDecodedFrameSlot, RemoteOperationControl,
    RemoteTimeouts, RtspCodec, RtspLatencyMode, RtspStreamConfig, RtspTransport, SourcePts,
    StreamCancellation, StreamFrameIdentity, StreamOpenRequest, StreamOperationControl,
    StreamRecordingRequest, StreamService, StreamServiceError, StreamServiceEvent, StreamSession,
    StreamSessionId, StreamStage, StreamTerminal, StreamTimeouts, TypedCommandRequest,
    X5233CapturePayload, host_monotonic_time_ns,
};
use crate::ports::RasterFormat;

/// 清理 config 读取用的字符串辅助。
fn config_string(spec: &NodeSpec, key: &str) -> String {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn non_empty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn password_session_credential_ref(spec: &NodeSpec, label: &str) -> Result<String, NodeError> {
    let credential_ref = config_string(spec, "credentialRef");
    let credential_ref = credential_ref.trim();
    if credential_ref.is_empty() {
        return Err(NodeError::Precondition(format!("{label} is required")));
    }
    let Some(session_id) = credential_ref.strip_prefix("session:") else {
        return Err(NodeError::Precondition(format!(
            "{label} must be a password session:<node-id> reference"
        )));
    };
    if session_id.is_empty()
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(NodeError::Precondition(format!(
            "{label} must be a password session:<node-id> reference"
        )));
    }
    Ok(credential_ref.to_owned())
}

// ---------------------------------------------------------------------------
// X5_233 Driver 节点
// ---------------------------------------------------------------------------

/// X5_233 专用驱动适配器；TCP 用于状态/抓帧，RTSP video 输出由本节点显式连接并解码。
pub struct X5233DriverFactory;

impl NodeFactory for X5233DriverFactory {
    fn kind(&self) -> &'static str {
        "x5233Driver"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(X5233DriverNode::new(spec)))
    }
}

pub struct X5233DriverNode {
    spec: NodeSpec,
    video_ch0: Option<X5233VideoStream>,
    video_ch3: Option<X5233VideoStream>,
}

struct X5233VideoStream {
    cancellation: StreamCancellation,
    session: StreamSession,
    pump_cancel: Arc<AtomicBool>,
}

impl X5233DriverNode {
    fn new(spec: NodeSpec) -> Self {
        Self {
            spec,
            video_ch0: None,
            video_ch3: None,
        }
    }

    fn host(&self) -> Result<String, NodeError> {
        non_empty(config_string(&self.spec, "host")).ok_or_else(|| {
            NodeError::Precondition("x5233Driver host must be configured".to_owned())
        })
    }

    fn port(&self) -> u16 {
        self.config_u16("tcpPort", 9073)
    }

    fn yuv_capture_request(&self) -> Result<CaptureRequest, NodeError> {
        Ok(CaptureRequest {
            target: CaptureTarget::Yuv {
                channel: self.config_u16("snapshotChannel", 0),
            },
            mode: self.capture_mode()?,
            source_identity: None,
        })
    }

    fn raw_capture_request(&self) -> CaptureRequest {
        CaptureRequest {
            target: CaptureTarget::Raw {
                camera: self.config_u16("rawCamera", 0),
            },
            mode: CaptureMode::Latest,
            source_identity: None,
        }
    }

    fn capture_mode(&self) -> Result<CaptureMode, NodeError> {
        match config_string(&self.spec, "snapshotMode").as_str() {
            "" | "latest" => Ok(CaptureMode::Latest),
            "frame_id" => self
                .required_config_u64("snapshotFrameId", "snapshotFrameId")
                .map(CaptureMode::FrameId),
            "timestamp_ns" => self
                .required_config_u64("snapshotTimestampNs", "snapshotTimestampNs")
                .map(CaptureMode::TimestampNs),
            value => Err(NodeError::Config(format!(
                "x5233Driver snapshotMode `{value}` is unsupported"
            ))),
        }
    }

    fn config_u16(&self, key: &str, fallback: u16) -> u16 {
        self.config_u64(key)
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(fallback)
    }

    fn config_u64(&self, key: &str) -> Option<u64> {
        let value = self.spec.config.get(key)?;
        if let Some(number) = value.as_u64() {
            return Some(number);
        }
        value.as_str()?.trim().parse::<u64>().ok()
    }

    fn required_config_u64(&self, key: &str, label: &str) -> Result<u64, NodeError> {
        self.config_u64(key).ok_or_else(|| {
            NodeError::Config(format!(
                "x5233Driver {label} must be a non-negative integer"
            ))
        })
    }

    fn raw_metadata(&self) -> RawMetadata {
        RawMetadata {
            bayer_pattern: BayerPattern::Rggb,
            bits_per_sample: 12,
            black_level: None,
            white_level: None,
        }
    }

    fn config_u32(&self, key: &str, fallback: u32) -> u32 {
        self.config_u64(key)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(fallback)
    }

    fn rtsp_channel(&self) -> u16 {
        self.config_u16("rtspChannel", 0)
    }

    fn rtsp_output_port(channel: u16) -> Result<&'static str, NodeError> {
        match channel {
            0 => Ok("videoCh0"),
            3 => Ok("videoCh3"),
            _ => Err(NodeError::Precondition(format!(
                "X5_233 video only exposes RTSP channels 0 and 3, got {channel}"
            ))),
        }
    }

    fn rtsp_url(&self, channel: u16) -> Result<String, NodeError> {
        let explicit_url = config_string(&self.spec, "rtspUrl");
        if !explicit_url.trim().is_empty() {
            return Ok(explicit_url);
        }
        let port = match channel {
            0 => 554,
            3 => 557,
            _ => {
                return Err(NodeError::Precondition(format!(
                    "X5_233 video only exposes RTSP channels 0 and 3, got {channel}"
                )));
            }
        };
        Ok(format!("rtsp://{}:{port}/PRR", self.host()?))
    }

    fn connect_video(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.connect_video_channel(self.rtsp_channel(), rt)
    }

    fn connect_all_video(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.connect_video_channel(0, rt)?;
        self.connect_video_channel(3, rt)
    }

    fn connect_video_channel(
        &mut self,
        channel: u16,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let output_port = Self::rtsp_output_port(channel)?.to_owned();
        if let Some(stream) = self.video_stream_ref(channel)? {
            if !stream.cancellation.is_cancelled() {
                rt.report_state(
                    NodeRuntimeState::Running,
                    format!("x5_233 video CH{channel} streaming"),
                );
                return Ok(());
            }
        }
        self.disconnect_video_channel(channel, rt)?;

        let factory = rt.services().stream_factory()?;
        let config = RtspStreamConfig {
            url: self.rtsp_url(channel)?,
            channel,
            width: self.config_u32("rtspWidth", 1920),
            height: self.config_u32("rtspHeight", 1080),
            codec: RtspCodec::H264,
            transport: RtspTransport::Tcp,
            latency_mode: RtspLatencyMode::Low,
        };
        let service: Arc<dyn StreamService> = factory.create(config);
        let session_id = StreamSessionId::new(format!("x5-{}-ch{channel}", self.spec.id))
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let request = StreamOpenRequest {
            channel,
            media: "rtsp".to_owned(),
            cseq: 1,
            prefer_hardware_acceleration: false,
            recording: StreamRecordingRequest::default(),
        };
        let cancellation = StreamCancellation::default();
        let pump_cancel = Arc::new(AtomicBool::new(false));
        let reporter =
            x5233_stream_reporter(rt, channel, cancellation.clone(), Arc::clone(&pump_cancel));
        let control = StreamOperationControl::new(
            x5233_stream_timeouts_from_config(&self.spec)?,
            cancellation.clone(),
            reporter,
        )
        .map_err(|error| NodeError::Execution(error.to_string()))?;
        let session = match service.open(session_id, request, control) {
            Ok(session) => session,
            Err(error) => {
                rt.report_state(
                    NodeRuntimeState::Error,
                    x5233_stream_failure_diagnostic(channel, &error),
                );
                return Err(NodeError::Execution(error.to_string()));
            }
        };

        let latest_frame = Arc::clone(&session.latest_frame);
        rt.spawn(format!("x5-video-pump-{}-ch{channel}", self.spec.id), {
            let pump_cancel = Arc::clone(&pump_cancel);
            move |ctx| pump_x5233_video_frames(latest_frame, ctx, pump_cancel, output_port)
        });
        *self.video_stream_slot(channel)? = Some(X5233VideoStream {
            cancellation,
            session,
            pump_cancel,
        });
        rt.report_state(
            NodeRuntimeState::Running,
            format!("x5_233 video CH{channel} streaming"),
        );
        Ok(())
    }

    fn disconnect_video_channel(
        &mut self,
        channel: u16,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let Some(stream) = self.video_stream_slot(channel)?.take() else {
            return Ok(());
        };
        close_x5233_video_stream(stream);
        if self.has_active_video_stream() {
            rt.report_state(
                NodeRuntimeState::Running,
                format!("x5_233 video CH{channel} disconnected; other channel still streaming"),
            );
        } else {
            rt.report_state(
                NodeRuntimeState::Idle,
                format!("x5_233 video CH{channel} disconnected"),
            );
        }
        Ok(())
    }

    fn disconnect_video(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        if let Some(stream) = self.video_ch0.take() {
            close_x5233_video_stream(stream);
        }
        if let Some(stream) = self.video_ch3.take() {
            close_x5233_video_stream(stream);
        }
        rt.report_state(NodeRuntimeState::Idle, "x5_233 video disconnected");
        Ok(())
    }

    fn video_stream_ref(&self, channel: u16) -> Result<&Option<X5233VideoStream>, NodeError> {
        match channel {
            0 => Ok(&self.video_ch0),
            3 => Ok(&self.video_ch3),
            _ => Err(NodeError::Precondition(format!(
                "X5_233 video only exposes RTSP channels 0 and 3, got {channel}"
            ))),
        }
    }

    fn video_stream_slot(
        &mut self,
        channel: u16,
    ) -> Result<&mut Option<X5233VideoStream>, NodeError> {
        match channel {
            0 => Ok(&mut self.video_ch0),
            3 => Ok(&mut self.video_ch3),
            _ => Err(NodeError::Precondition(format!(
                "X5_233 video only exposes RTSP channels 0 and 3, got {channel}"
            ))),
        }
    }

    fn has_active_video_stream(&self) -> bool {
        self.video_ch0
            .as_ref()
            .is_some_and(|stream| !stream.cancellation.is_cancelled())
            || self
                .video_ch3
                .as_ref()
                .is_some_and(|stream| !stream.cancellation.is_cancelled())
    }

    fn capture(&self, request: &CaptureRequest, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        if matches!(request.target, CaptureTarget::Raw { .. })
            && !matches!(request.mode, CaptureMode::Latest)
        {
            return Err(NodeError::Precondition(
                "X5_233 RAW capture only supports mode=latest; the device exposes no RAW frame ring"
                    .to_owned(),
            ));
        }
        let host = self.host()?;
        let payload = rt
            .services()
            .x5_client()?
            .capture(&host, self.port(), request)
            .map_err(NodeError::Execution)?;
        let (output, frame) = match payload {
            X5233CapturePayload::Nv12 {
                channel,
                width,
                height,
                y_len,
                uv_len,
                frame_id,
                timestamp_ns,
                payload,
            } => {
                if request.target != (CaptureTarget::Yuv { channel }) {
                    return Err(NodeError::Execution(
                        "X5_233 YUV response target does not match capture request".to_owned(),
                    ));
                }
                let output = match channel {
                    0 => "yuvCh0",
                    3 => "yuvCh3",
                    _ => {
                        return Err(NodeError::Precondition(format!(
                            "X5_233 only exposes YUV channels 0 and 3, got {channel}"
                        )));
                    }
                };
                let y_stride = plane_stride(y_len, height, "Y")?;
                let uv_stride = plane_stride(uv_len, height / 2, "UV")?;
                let y_end = y_len;
                let uv_end = y_len.checked_add(uv_len).ok_or_else(|| {
                    NodeError::Execution("X5_233 NV12 payload length overflow".to_owned())
                })?;
                if payload.len() != uv_end {
                    return Err(NodeError::Execution(format!(
                        "X5_233 NV12 payload has {} bytes, expected {uv_end}",
                        payload.len()
                    )));
                }
                let identity = x5233_identity(channel, None, frame_id, timestamp_ns);
                let frame = ImageFrame::new(
                    width,
                    height,
                    ImageFrameFormat::Nv12,
                    vec![
                        ImagePlane::new(Arc::from(&payload[..y_end]), y_stride),
                        ImagePlane::new(Arc::from(&payload[y_end..uv_end]), uv_stride),
                    ],
                    identity,
                    None,
                    None,
                )
                .map_err(|error| {
                    NodeError::Execution(format!("invalid X5_233 NV12 frame: {error}"))
                })?;
                (output, frame)
            }
            X5233CapturePayload::BayerRaw {
                camera,
                width,
                height,
                stride_bytes,
                frame_id,
                timestamp_ns,
                payload,
                ..
            } => {
                if request.target != (CaptureTarget::Raw { camera }) {
                    return Err(NodeError::Execution(
                        "X5_233 RAW response target does not match capture request".to_owned(),
                    ));
                }
                let output = match camera {
                    0 => "rawCam0",
                    1 => "rawCam1",
                    _ => {
                        return Err(NodeError::Precondition(format!(
                            "X5_233 only exposes RAW cameras 0 and 1, got {camera}"
                        )));
                    }
                };
                let raw = self.raw_metadata();

                let identity = x5233_identity(camera, Some(camera), frame_id, timestamp_ns);
                let frame = ImageFrame::new(
                    width,
                    height,
                    ImageFrameFormat::BayerRaw,
                    vec![ImagePlane::new(payload, stride_bytes)],
                    identity,
                    None,
                    Some(raw),
                )
                .map_err(|error| {
                    NodeError::Execution(format!("invalid X5_233 RAW frame: {error}"))
                })?;
                (output, frame)
            }
        };
        rt.emit(output, DataPacket::ImageFrame(Arc::new(frame)))?;
        rt.report_event(format!("x5_233 capture published on {output}"));
        rt.report_state(NodeRuntimeState::Idle, "x5_233 capture ready");
        Ok(())
    }
}
fn close_x5233_video_stream(stream: X5233VideoStream) {
    stream.session.request_close();
    stream.cancellation.cancel();
    stream.pump_cancel.store(true, Ordering::Release);
}

impl NodeInstance for X5233DriverNode {
    fn kind(&self) -> &'static str {
        "x5233Driver"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(
            NodeRuntimeState::Ready,
            "trigger to query X5_233 driver status",
        );
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if port != "capture" {
            return Ok(());
        }
        let DataPacket::CaptureRequest(request) = packet else {
            return Err(NodeError::Precondition(
                "x5233Driver.capture requires command.capture.request.v1".to_owned(),
            ));
        };
        self.capture(&request, rt)
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Connect => self.connect_video(rt),
            NodeAction::Disconnect => self.disconnect_video(rt),
            NodeAction::Custom { name, .. } if name == "open_rtsp_ch0" => {
                self.connect_video_channel(0, rt)
            }
            NodeAction::Custom { name, .. } if name == "open_rtsp_ch3" => {
                self.connect_video_channel(3, rt)
            }
            NodeAction::Custom { name, .. } if name == "open_rtsp_all" => {
                self.connect_all_video(rt)
            }
            NodeAction::Custom { name, .. } if name == "close_rtsp" => self.disconnect_video(rt),
            NodeAction::Trigger => {
                let status = rt
                    .services()
                    .x5_client()?
                    .status(&self.host()?, self.port())
                    .map_err(NodeError::Execution)?;
                rt.emit("status", DataPacket::Json(Arc::new(status)))?;
                rt.report_state(NodeRuntimeState::Idle, "x5_233 status ready");
                Ok(())
            }
            NodeAction::Custom { name, .. } if name == "status" => {
                let status = rt
                    .services()
                    .x5_client()?
                    .status(&self.host()?, self.port())
                    .map_err(NodeError::Execution)?;
                rt.emit("status", DataPacket::Json(Arc::new(status)))?;
                rt.report_state(NodeRuntimeState::Idle, "x5_233 status ready");
                Ok(())
            }
            NodeAction::Custom { name, .. } if name == "probe" => {
                let summary = rt
                    .services()
                    .x5_client()?
                    .probe(&self.host()?, self.port())
                    .map_err(NodeError::Execution)?;
                rt.emit("status", DataPacket::Json(Arc::new(summary)))?;
                rt.report_event("x5_233 probe ready".to_owned());
                rt.report_state(NodeRuntimeState::Idle, "x5_233 probe ready");
                Ok(())
            }
            NodeAction::Custom { name, .. } if name == "snapshot" || name == "capture_yuv" => {
                let request = self.yuv_capture_request()?;
                self.capture(&request, rt)
            }
            NodeAction::Custom { name, .. } if name == "capture_raw" => {
                self.capture(&self.raw_capture_request(), rt)
            }
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.disconnect_video(rt)
    }
}

fn x5233_stream_reporter(
    rt: &NodeRuntime,
    channel: u16,
    cancellation: StreamCancellation,
    pump_cancel: Arc<AtomicBool>,
) -> Arc<dyn Fn(StreamServiceEvent) + Send + Sync> {
    let reporter = rt.context().reporter.clone();
    Arc::new(move |event| {
        match &event {
            StreamServiceEvent::Terminal(StreamTerminal::Failed(error)) => {
                cancellation.cancel();
                pump_cancel.store(true, Ordering::Release);
                reporter.report_state(
                    NodeRuntimeState::Error,
                    x5233_stream_failure_diagnostic(channel, error),
                );
            }
            StreamServiceEvent::Terminal(StreamTerminal::Forced {
                remote_state_unknown,
            }) => {
                cancellation.cancel();
                pump_cancel.store(true, Ordering::Release);
                reporter.report_state(
                    NodeRuntimeState::Error,
                    format!(
                        "x5_233 video CH{channel} forced closed; remote_state_unknown={remote_state_unknown}"
                    ),
                );
            }
            StreamServiceEvent::Terminal(StreamTerminal::BoundaryClosed) => {
                cancellation.cancel();
                pump_cancel.store(true, Ordering::Release);
                reporter.report_state(
                    NodeRuntimeState::Idle,
                    format!("x5_233 video CH{channel} boundary closed"),
                );
            }
            StreamServiceEvent::Terminal(StreamTerminal::Cancelled) => {
                cancellation.cancel();
                pump_cancel.store(true, Ordering::Release);
                reporter.report_state(
                    NodeRuntimeState::Idle,
                    format!("x5_233 video CH{channel} cancelled"),
                );
            }
            StreamServiceEvent::Stage(StreamStage::Playing) => {
                reporter.report_state(
                    NodeRuntimeState::Running,
                    format!("x5_233 video CH{channel} streaming"),
                );
            }
            _ => {}
        }
        reporter.report_event(format!("x5_233 video CH{channel} stream: {event:?}"));
    })
}

fn pump_x5233_video_frames(
    latest: Arc<LatestDecodedFrameSlot>,
    ctx: crate::engine::SpawnContext,
    cancel: Arc<AtomicBool>,
    output_port: String,
) {
    while !cancel.load(Ordering::Acquire) {
        let Some(frame) = latest.wait_latest_timeout(X5233_PUMP_CANCEL_POLL) else {
            continue;
        };
        let _ = ctx
            .outputs
            .emit(&output_port, DataPacket::VideoFrame(frame));
    }
}

const X5233_PUMP_CANCEL_POLL: Duration = Duration::from_millis(100);
const X5233_DEFAULT_CONNECT_TIMEOUT_MS: u64 = 8_000;
const X5233_DEFAULT_IDLE_TIMEOUT_MS: u64 = 10_000;
const X5233_MAX_TIMEOUT_MS: u64 = 120_000;

fn x5233_stream_timeouts_from_config(spec: &NodeSpec) -> Result<StreamTimeouts, NodeError> {
    let connect = x5233_config_duration_ms(
        spec,
        "rtspConnectTimeoutMs",
        X5233_DEFAULT_CONNECT_TIMEOUT_MS,
    )?;
    let idle = x5233_config_duration_ms(spec, "rtspIdleTimeoutMs", X5233_DEFAULT_IDLE_TIMEOUT_MS)?;
    StreamTimeouts { connect, idle }
        .validate()
        .map_err(|error| NodeError::Config(error.to_string()))
}

fn x5233_config_duration_ms(
    spec: &NodeSpec,
    key: &str,
    fallback_ms: u64,
) -> Result<Duration, NodeError> {
    let value_ms = spec
        .config
        .get(key)
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
        })
        .unwrap_or(fallback_ms);
    if value_ms == 0 || value_ms > X5233_MAX_TIMEOUT_MS {
        return Err(NodeError::Config(format!(
            "x5233Driver {key} must be in 1..={X5233_MAX_TIMEOUT_MS} ms"
        )));
    }
    Ok(Duration::from_millis(value_ms))
}

fn x5233_stream_failure_diagnostic(channel: u16, error: &StreamServiceError) -> String {
    match error {
        StreamServiceError::ConnectTimeout { timeout_ms } => format!(
            "x5_233 video CH{channel} failed before first decoded frame: connect timeout after {timeout_ms} ms; check RTSP channel/server/network or increase rtspConnectTimeoutMs"
        ),
        StreamServiceError::IdleTimeout { timeout_ms, .. } => format!(
            "x5_233 video CH{channel} failed after frames stopped: idle timeout after {timeout_ms} ms; check encoder/network stability"
        ),
        _ => format!("x5_233 video CH{channel} stream failed: {error}"),
    }
}

fn plane_stride(length: usize, rows: u32, plane: &str) -> Result<u32, NodeError> {
    let rows = usize::try_from(rows)
        .map_err(|_| NodeError::Execution(format!("X5_233 {plane} plane rows do not fit usize")))?;
    if rows == 0 || length % rows != 0 {
        return Err(NodeError::Execution(format!(
            "X5_233 {plane} plane length {length} is not an exact row extent"
        )));
    }
    u32::try_from(length / rows)
        .map_err(|_| NodeError::Execution(format!("X5_233 {plane} stride does not fit u32")))
}

fn x5233_identity(
    channel: u16,
    camera: Option<u16>,
    frame_id: u64,
    timestamp_ns: u64,
) -> ImageFrameIdentity {
    ImageFrameIdentity {
        provenance: FrameProvenance::Device {
            driver: "x5_233".to_owned(),
            channel,
            camera,
            timestamp_ns,
        },
        frame_sequence: frame_id,
        source_pts: SourcePts::Unavailable {
            reason: "X5_233 device timestamp is not a decoded RTSP frame identity".to_owned(),
        },
        host_monotonic_time_ns: host_monotonic_time_ns(),
        device_timestamp_ns: Some(timestamp_ns),
    }
}

// ---------------------------------------------------------------------------
// Hex Arm Device 节点
// ---------------------------------------------------------------------------

/// Hex Arm 控制节点：显式维护本节点建立的会话与 API 控制初始化状态。
///
/// probe/status 不需要运动许可；其他控制动作必须经过 connect → initialize_api_control，
/// 且 `controlEnabled` 为真。这样工作流恢复和未知会话状态都不能直接驱动机械臂。
pub struct HexArmDeviceFactory;

impl NodeFactory for HexArmDeviceFactory {
    fn kind(&self) -> &'static str {
        "hexArmDevice"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(HexArmDeviceNode {
            spec,
            connected: false,
            api_control_initialized: false,
        }))
    }
}

pub struct HexArmDeviceNode {
    spec: NodeSpec,
    connected: bool,
    api_control_initialized: bool,
}

impl HexArmDeviceNode {
    fn target(&self) -> Result<HexArmTargetConfig, NodeError> {
        let mut target = HexArmTargetConfig {
            host: non_empty(config_string(&self.spec, "host")).ok_or_else(|| {
                NodeError::Precondition("hexArmDevice host must be configured".to_owned())
            })?,
            ..HexArmTargetConfig::default()
        };
        target.port = config_u16(&self.spec, "port", target.port)?;
        target.command_timeout_ms =
            config_positive_u64(&self.spec, "commandTimeoutMs", target.command_timeout_ms)?;
        target.connect_timeout_ms =
            config_positive_u64(&self.spec, "connectTimeoutMs", target.connect_timeout_ms)?;
        target.control_enabled = config_bool(&self.spec, "controlEnabled", false)?;
        target.transport = match config_string(&self.spec, "transport").trim() {
            "" | "websocket" => HexArmTransport::WebSocket,
            "kcp" => HexArmTransport::Kcp,
            value => {
                return Err(NodeError::Config(format!(
                    "config `transport` must be `websocket` or `kcp`, got `{value}`"
                )));
            }
        };
        Ok(target)
    }

    /// 校验本节点创建的控制会话，避免未初始化或恢复出的节点执行运动相关命令。
    fn require_control_session(&self, target: &HexArmTargetConfig) -> Result<(), NodeError> {
        if !target.control_enabled {
            return Err(NodeError::Precondition(
                "hex arm control is disabled; set controlEnabled=true before control actions"
                    .to_owned(),
            ));
        }
        if !self.connected {
            return Err(NodeError::Precondition(
                "hex arm session is not connected; connect first".to_owned(),
            ));
        }
        if !self.api_control_initialized {
            return Err(NodeError::Precondition(
                "hex arm API control is not initialized; initialize_api_control first".to_owned(),
            ));
        }
        Ok(())
    }

    fn joint_positions(&self) -> Result<HexArmJointPositionsRequest, NodeError> {
        let values = match self.spec.config.get("jointPositions") {
            Some(serde_json::Value::String(text)) => text
                .split(',')
                .map(str::trim)
                .map(|value| {
                    value.parse::<f64>().map_err(|_| {
                        NodeError::Config(
                            "config `jointPositions` must be a comma-separated radian list"
                                .to_owned(),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(serde_json::Value::Array(values)) => values
                .iter()
                .map(|value| {
                    value.as_f64().ok_or_else(|| {
                        NodeError::Config(
                            "config `jointPositions` must contain only numeric radians".to_owned(),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => {
                return Err(NodeError::Precondition(
                    "config `jointPositions` must be a non-empty radian list".to_owned(),
                ));
            }
        };
        if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
            return Err(NodeError::Precondition(
                "joint positions must be non-empty finite radians".to_owned(),
            ));
        }
        Ok(HexArmJointPositionsRequest {
            joint_positions_radians: values,
        })
    }
}

impl NodeInstance for HexArmDeviceNode {
    fn kind(&self) -> &'static str {
        "hexArmDevice"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(
            NodeRuntimeState::Ready,
            "connect before initializing Hex Arm API control",
        );
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        _packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let client = rt.services().hex_arm_client()?;
        let target = self.target()?;
        match action {
            NodeAction::Connect => {
                rt.report_state(NodeRuntimeState::Running, "connecting Hex Arm");
                client.connect(&target).map_err(NodeError::Execution)?;
                self.connected = true;
                self.api_control_initialized = false;
                rt.report_event("hex arm connected".to_owned());
                rt.report_state(
                    NodeRuntimeState::Idle,
                    "connected; initialize API control next",
                );
                Ok(())
            }
            NodeAction::Disconnect => {
                if !self.connected {
                    return Err(NodeError::Precondition(
                        "hex arm session is not connected; refusing untracked disconnect"
                            .to_owned(),
                    ));
                }
                client.disconnect(&target).map_err(NodeError::Execution)?;
                self.connected = false;
                self.api_control_initialized = false;
                rt.report_event("hex arm disconnected".to_owned());
                rt.report_state(NodeRuntimeState::Idle, "disconnected");
                Ok(())
            }
            NodeAction::Trigger => {
                client.status(&target).map_err(NodeError::Execution)?;
                rt.report_event("hex arm status ready".to_owned());
                Ok(())
            }
            NodeAction::Custom { name, .. } if name == "status" => {
                client.status(&target).map_err(NodeError::Execution)?;
                rt.report_event("hex arm status ready".to_owned());
                Ok(())
            }
            NodeAction::Custom { name, .. } if name == "initialize_api_control" => {
                if !target.control_enabled {
                    return Err(NodeError::Precondition(
                        "hex arm control is disabled; set controlEnabled=true before control actions"
                            .to_owned(),
                    ));
                }
                if !self.connected {
                    return Err(NodeError::Precondition(
                        "hex arm session is not connected; connect first".to_owned(),
                    ));
                }
                client
                    .initialize_api_control(&target)
                    .map_err(NodeError::Execution)?;
                self.api_control_initialized = true;
                rt.report_event("hex arm API control initialized".to_owned());
                Ok(())
            }
            NodeAction::Custom { name, .. } if name == "calibrate" => {
                self.require_control_session(&target)?;
                client.calibrate(&target).map_err(NodeError::Execution)?;
                rt.report_event("hex arm calibration requested".to_owned());
                Ok(())
            }
            NodeAction::Custom { name, .. } if name == "clear_parking_stop" => {
                self.require_control_session(&target)?;
                client
                    .clear_parking_stop(&target)
                    .map_err(NodeError::Execution)?;
                rt.report_event("hex arm parking stop cleared".to_owned());
                Ok(())
            }
            NodeAction::Custom { name, .. } if name == "zero_current" => {
                self.require_control_session(&target)?;
                client.zero_current(&target).map_err(NodeError::Execution)?;
                rt.report_event("hex arm current zeroed".to_owned());
                Ok(())
            }
            NodeAction::Custom { name, .. } if name == "send_joint_positions" => {
                self.require_control_session(&target)?;
                let request = self.joint_positions()?;
                client
                    .send_joint_positions(&target, &request)
                    .map_err(NodeError::Execution)?;
                rt.report_event("hex arm joint positions sent".to_owned());
                Ok(())
            }
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_config_update(
        &mut self,
        config: serde_json::Value,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let mut updated_spec = self.spec.clone();
        updated_spec.config = config;
        // 先解析新目标，拒绝明显无效配置而不改变当前会话。
        let candidate = HexArmDeviceNode {
            spec: updated_spec.clone(),
            connected: false,
            api_control_initialized: false,
        };
        let candidate_target = candidate.target()?;
        let (should_disconnect, old_target) = if self.connected {
            let old_target = self.target()?;
            let should_disconnect = old_target.host != candidate_target.host
                || old_target.port != candidate_target.port
                || old_target.transport != candidate_target.transport
                || (old_target.control_enabled && !candidate_target.control_enabled);
            (should_disconnect, Some(old_target))
        } else {
            (false, None)
        };
        let disconnect_result = if should_disconnect {
            match rt.services().hex_arm_client() {
                Ok(client) => client
                    .disconnect(old_target.as_ref().expect("connected target is present"))
                    .map(|_| ())
                    .map_err(NodeError::Execution),
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        };
        if should_disconnect {
            // 主机/传输/禁用控制的变更必须先安全断开；失败时清除本地会话状态。
            self.connected = false;
            self.api_control_initialized = false;
        }
        disconnect_result?;
        self.spec = updated_spec;
        rt.report_event("hex arm configuration updated".to_owned());
        Ok(())
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let disconnect_result = if self.connected {
            let client = rt.services().hex_arm_client()?;
            let target = self.target()?;
            client
                .disconnect(&target)
                .map(|_| ())
                .map_err(NodeError::Execution)
        } else {
            Ok(())
        };
        self.connected = false;
        self.api_control_initialized = false;
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        disconnect_result
    }
}

/// 读取正整数配置；允许 Web UI 写入 number 或文本字段。
fn config_positive_u64(spec: &NodeSpec, key: &str, fallback: u64) -> Result<u64, NodeError> {
    let Some(value) = spec.config.get(key) else {
        return Ok(fallback);
    };
    let parsed = match value {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(value) => value.trim().parse::<u64>().ok(),
        _ => None,
    }
    .filter(|value| *value > 0)
    .ok_or_else(|| NodeError::Config(format!("config `{key}` must be a positive integer")))?;
    Ok(parsed)
}

fn config_u16(spec: &NodeSpec, key: &str, fallback: u16) -> Result<u16, NodeError> {
    let value = config_positive_u64(spec, key, u64::from(fallback))?;
    u16::try_from(value).map_err(|_| NodeError::Config(format!("config `{key}` must fit in u16")))
}

fn config_bool(spec: &NodeSpec, key: &str, fallback: bool) -> Result<bool, NodeError> {
    let Some(value) = spec.config.get(key) else {
        return Ok(fallback);
    };
    match value {
        serde_json::Value::Bool(value) => Ok(*value),
        serde_json::Value::String(value) => match value.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(NodeError::Config(format!(
                "config `{key}` must be a boolean"
            ))),
        },
        _ => Err(NodeError::Config(format!(
            "config `{key}` must be a boolean"
        ))),
    }
}

// ---------------------------------------------------------------------------
// SFTP File Source 节点
// ---------------------------------------------------------------------------

const DECODED_IMAGE_BYTE_LIMIT: usize = 128 * 1024 * 1024;

/// SFTP 文件源：经 `SftpFileReader` 读取远程图片字节，经 `RasterImageCodec` 解码为 image.frame。
pub struct SftpFileSourceFactory;

impl NodeFactory for SftpFileSourceFactory {
    fn kind(&self) -> &'static str {
        "sftpFileSource"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(SftpFileSourceNode { spec }))
    }
}

pub struct SftpFileSourceNode {
    spec: NodeSpec,
}

impl SftpFileSourceNode {
    fn target(&self) -> Result<ControlTargetSpec, NodeError> {
        let host = non_empty(config_string(&self.spec, "host")).ok_or_else(|| {
            NodeError::Precondition("sftpFileSource host must be configured".to_owned())
        })?;
        let port = config_string(&self.spec, "port")
            .parse::<u16>()
            .map_err(|_| {
                NodeError::Config("sftpFileSource port must be in 1..=65535".to_owned())
            })?;
        if port == 0 {
            return Err(NodeError::Config(
                "sftpFileSource port must be in 1..=65535".to_owned(),
            ));
        }
        Ok(ControlTargetSpec {
            host,
            port,
            username: config_string(&self.spec, "username"),
            expected_host_key: None,
        })
    }

    fn remote_path(&self) -> Result<String, NodeError> {
        let root = config_string(&self.spec, "remoteRoot");
        let selection = config_string(&self.spec, "selection");
        if selection.trim().is_empty() {
            return Err(NodeError::Precondition(
                "sftpFileSource selection must be configured".to_owned(),
            ));
        }
        let mut path = root.trim_end_matches('/').to_owned();
        path.push('/');
        path.push_str(selection.trim_start_matches('/'));
        Ok(path)
    }
}

impl NodeInstance for SftpFileSourceNode {
    fn kind(&self) -> &'static str {
        "sftpFileSource"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "trigger to fetch remote image");
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        _packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => self.fetch(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl SftpFileSourceNode {
    fn fetch(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let target = self.target()?;
        let credential_ref =
            password_session_credential_ref(&self.spec, "sftpFileSource credentialRef")?;
        let path = self.remote_path()?;
        rt.report_state(NodeRuntimeState::Running, "fetching remote image");
        let format = match path
            .rsplit('.')
            .next()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => RasterFormat::Png,
            Some("jpg" | "jpeg") => RasterFormat::Jpeg,
            _ => {
                return Err(NodeError::Precondition(
                    "unsupported remote image extension".to_owned(),
                ));
            }
        };

        let reader = rt.services().sftp_reader()?;
        let control = control_timeout(30, 120)?;
        let bytes = reader
            .read(
                &target,
                &credential_ref,
                &path,
                DECODED_IMAGE_BYTE_LIMIT,
                control,
            )
            .map_err(NodeError::Execution)?;

        let codec = rt.services().image_codec()?;
        let rgba: Rgba8Frame = codec
            .decode_rgba8(format, &bytes, DECODED_IMAGE_BYTE_LIMIT)
            .map_err(|e| NodeError::Execution(e.to_string()))?;

        let (width, height) = (rgba.width, rgba.height);
        let compact = compact_rgba8(&rgba, width, height)?;
        let identity = StreamFrameIdentity::unavailable(
            StreamSessionId::new(format!("sftp-{}", self.spec.id))
                .map_err(|_| NodeError::Execution("invalid session id".to_owned()))?,
            0,
            0,
            "sftp file source",
        );
        let frame = ImageFrame::rgba8(width, height, compact, ImageFrameIdentity::from(&identity))
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        rt.emit("image", DataPacket::ImageFrame(Arc::new(frame)))?;
        rt.report_state(NodeRuntimeState::Idle, "remote image ready");
        Ok(())
    }
}

/// 把 `Rgba8Frame`（可能带 stride）复制为紧密排列的 RGBA 字节。
fn compact_rgba8(frame: &Rgba8Frame, width: u32, height: u32) -> Result<Arc<[u8]>, NodeError> {
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|w| w.checked_mul(4))
        .ok_or_else(|| NodeError::Execution("image width overflow".to_owned()))?;
    let total = row_bytes
        .checked_mul(height as usize)
        .ok_or_else(|| NodeError::Execution("image size overflow".to_owned()))?;
    let mut compact = Vec::with_capacity(total);
    let pixels = frame.pixels();
    for row in 0..height as usize {
        let start = row * frame.stride;
        let end = start + row_bytes;
        let Some(row_slice) = pixels.get(start..end) else {
            return Err(NodeError::Execution(
                "image stride/layout inconsistent".to_owned(),
            ));
        };
        compact.extend_from_slice(row_slice);
    }
    Ok(compact.into())
}

// ---------------------------------------------------------------------------
// SSH Session 节点
// ---------------------------------------------------------------------------

/// SSH 会话：经 `SshCommandExecutor` 执行一次 allowlisted typed 命令，输出 CommandResult。
pub struct SshSessionFactory;

impl NodeFactory for SshSessionFactory {
    fn kind(&self) -> &'static str {
        "sshSession"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(SshSessionNode { spec }))
    }
}

pub struct SshSessionNode {
    spec: NodeSpec,
}

impl NodeInstance for SshSessionNode {
    fn kind(&self) -> &'static str {
        "sshSession"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "trigger to run remote command");
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        _packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => self.run(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl SshSessionNode {
    fn run(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let host = non_empty(config_string(&self.spec, "host")).ok_or_else(|| {
            NodeError::Precondition("sshSession host must be configured".to_owned())
        })?;
        let credential_ref =
            password_session_credential_ref(&self.spec, "sshSession credentialRef")?;
        let recipe_id = non_empty(config_string(&self.spec, "recipeId")).ok_or_else(|| {
            NodeError::Precondition("sshSession recipeId must be configured".to_owned())
        })?;
        let target = ControlTargetSpec {
            host,
            port: config_string(&self.spec, "port")
                .parse::<u16>()
                .unwrap_or(22),
            username: config_string(&self.spec, "username"),
            expected_host_key: None,
        };
        let request = TypedCommandRequest::new(recipe_id)
            .map_err(|e| NodeError::Precondition(e.to_string()))?;

        let executor = rt.services().ssh_command_executor()?;
        let control = control_timeout(10, 60)?;
        let result: CommandResult = executor
            .execute(&target, &credential_ref, request, control)
            .map_err(NodeError::Execution)?;

        // CommandResult 未实现 Serialize，手动折叠为 JSON（stdout/stderr 只给长度摘要）。
        let value = serde_json::json!({
            "terminal": format!("{:?}", result.terminal),
            "stdoutLen": result.stdout.len(),
            "stderrLen": result.stderr.len(),
            "stdoutTruncated": result.stdout_truncated,
            "stderrTruncated": result.stderr_truncated,
            "artifactPath": result.artifact_path,
        });
        let _ = rt.emit("result", DataPacket::Json(Arc::new(value)));
        rt.report_state(NodeRuntimeState::Idle, "command executed");
        Ok(())
    }
}

fn control_timeout(
    connect_secs: u64,
    overall_secs: u64,
) -> Result<RemoteOperationControl, NodeError> {
    RemoteOperationControl::new(
        RemoteTimeouts {
            connect: Duration::from_secs(connect_secs),
            idle: Duration::from_secs(overall_secs),
            overall: Duration::from_secs(overall_secs),
        },
        DumpCancellation::default(),
    )
    .map_err(|e| NodeError::Precondition(e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicBool, mpsc};

    use parking_lot::Mutex;

    use super::*;
    use crate::engine::{EngineServices, NodeReporter, OutputRegistry, SpawnContext};
    use crate::platform::{
        DecodedVideoFrame, HexArmControlClient, LatestDecodedFrameSlot, SourcePtsProvenance,
        StreamServiceError, StreamSession, X5ControlClient,
    };

    fn x5_spec() -> NodeSpec {
        NodeSpec {
            id: "x5-1".to_owned(),
            kind: "x5233Driver".to_owned(),
            title: "X5_233 Driver".to_owned(),
            inputs: vec![],
            outputs: vec![],
            config: serde_json::json!({
                "host": "camera.local",
                "tcpPort": 9073,
                "snapshotChannel": 3,
            }),
        }
    }

    struct RecordingSshCommandExecutor {
        targets: Arc<Mutex<Vec<ControlTargetSpec>>>,
    }

    impl crate::platform::SshCommandExecutor for RecordingSshCommandExecutor {
        fn execute(
            &self,
            target: &ControlTargetSpec,
            _credential_ref: &str,
            _request: TypedCommandRequest,
            _control: RemoteOperationControl,
        ) -> Result<CommandResult, String> {
            self.targets.lock().push(target.clone());
            Ok(CommandResult {
                terminal: crate::platform::CommandTerminal::Succeeded,
                stdout: vec![],
                stderr: vec![],
                stdout_truncated: false,
                stderr_truncated: false,
                artifact_path: None,
            })
        }
    }

    fn ssh_session_spec() -> NodeSpec {
        NodeSpec {
            id: "ssh-session-1".to_owned(),
            kind: "sshSession".to_owned(),
            title: "SSH Session".to_owned(),
            inputs: vec![],
            outputs: vec![],
            config: serde_json::json!({
                "host": "camera.local",
                "port": "22",
                "username": "root",
                "credentialRef": "session:ssh-session-1",
                "recipeId": "capture",
            }),
        }
    }

    struct RecordingX5StreamService {
        opened: Arc<Mutex<Vec<String>>>,
        frame: Arc<LatestDecodedFrameSlot>,
    }

    impl StreamService for RecordingX5StreamService {
        fn service_id(&self) -> &str {
            "mock-x5-video"
        }

        fn open(
            &self,
            session_id: StreamSessionId,
            _request: StreamOpenRequest,
            control: StreamOperationControl,
        ) -> Result<StreamSession, StreamServiceError> {
            self.opened.lock().push(session_id.as_str().to_owned());
            Ok(StreamSession::new(
                session_id,
                Arc::clone(&self.frame),
                control,
            ))
        }
    }

    struct RecordingX5StreamFactory {
        configs: Arc<Mutex<Vec<RtspStreamConfig>>>,
        opened: Arc<Mutex<Vec<String>>>,
        frames: Arc<Mutex<Vec<(u16, Arc<LatestDecodedFrameSlot>)>>>,
    }

    impl crate::engine::StreamServiceFactory for RecordingX5StreamFactory {
        fn create(&self, config: RtspStreamConfig) -> Arc<dyn StreamService> {
            let channel = config.channel;
            let frame = Arc::new(LatestDecodedFrameSlot::default());
            self.configs.lock().push(config);
            self.frames.lock().push((channel, Arc::clone(&frame)));
            Arc::new(RecordingX5StreamService {
                opened: Arc::clone(&self.opened),
                frame,
            })
        }
    }

    fn decoded_video_frame(sequence: u64, channel: u16) -> DecodedVideoFrame {
        DecodedVideoFrame {
            width: 2,
            height: 2,
            rgba: Arc::from([7_u8; 16]),
            identity: StreamFrameIdentity::known_at(
                StreamSessionId::new("x5-video-test").expect("valid session id"),
                channel,
                sequence,
                SourcePts::Known {
                    ticks: 9_000,
                    time_base_numerator: 1,
                    time_base_denominator: 90_000,
                    provenance: SourcePtsProvenance::FfmpegDecodedFrame,
                },
                123_456,
            ),
        }
    }

    fn hex_arm_spec(control_enabled: bool, positions: &str) -> NodeSpec {
        NodeSpec {
            id: "hex-arm-1".to_owned(),
            kind: "hexArmDevice".to_owned(),
            title: "Hex Arm".to_owned(),
            inputs: vec![],
            outputs: vec![],
            config: serde_json::json!({
                "host": "hex-arm.local",
                "port": 8439,
                "transport": "websocket",
                "controlEnabled": control_enabled,
                "jointPositions": positions,
            }),
        }
    }

    struct RecordingHexArmClient {
        calls: Arc<Mutex<Vec<String>>>,
        positions: Arc<Mutex<Option<Vec<f64>>>>,
    }

    impl RecordingHexArmClient {
        fn record(&self, name: &str) {
            self.calls.lock().push(name.to_owned());
        }
    }

    impl HexArmControlClient for RecordingHexArmClient {
        fn probe(&self, _target: &HexArmTargetConfig) -> Result<serde_json::Value, String> {
            self.record("probe");
            Ok(serde_json::json!({"ok": true}))
        }
        fn status(&self, _target: &HexArmTargetConfig) -> Result<serde_json::Value, String> {
            self.record("status");
            Ok(serde_json::json!({"ok": true}))
        }
        fn connect(&self, _target: &HexArmTargetConfig) -> Result<serde_json::Value, String> {
            self.record("connect");
            Ok(serde_json::json!({"ok": true}))
        }
        fn initialize_api_control(
            &self,
            _target: &HexArmTargetConfig,
        ) -> Result<serde_json::Value, String> {
            self.record("initialize_api_control");
            Ok(serde_json::json!({"ok": true}))
        }
        fn calibrate(&self, _target: &HexArmTargetConfig) -> Result<serde_json::Value, String> {
            self.record("calibrate");
            Ok(serde_json::json!({"ok": true}))
        }
        fn clear_parking_stop(
            &self,
            _target: &HexArmTargetConfig,
        ) -> Result<serde_json::Value, String> {
            self.record("clear_parking_stop");
            Ok(serde_json::json!({"ok": true}))
        }
        fn zero_current(&self, _target: &HexArmTargetConfig) -> Result<serde_json::Value, String> {
            self.record("zero_current");
            Ok(serde_json::json!({"ok": true}))
        }
        fn send_joint_positions(
            &self,
            _target: &HexArmTargetConfig,
            request: &HexArmJointPositionsRequest,
        ) -> Result<serde_json::Value, String> {
            self.record("send_joint_positions");
            *self.positions.lock() = Some(request.joint_positions_radians.clone());
            Ok(serde_json::json!({"ok": true}))
        }
        fn disconnect(&self, _target: &HexArmTargetConfig) -> Result<serde_json::Value, String> {
            self.record("disconnect");
            Ok(serde_json::json!({"ok": true}))
        }
    }

    fn runtime(services: EngineServices) -> (NodeRuntime, OutputRegistry) {
        let (status_tx, _status_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("i2c-1".to_owned(), status_tx, event_tx);
        let outputs = OutputRegistry::default();
        let ctx = SpawnContext {
            outputs: outputs.clone(),
            reporter,
            services: Arc::new(services),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        (NodeRuntime::new(ctx), outputs)
    }

    fn runtime_with_record(
        services: EngineServices,
        recorded: Arc<Mutex<Vec<DataPacket>>>,
    ) -> NodeRuntime {
        let (status_tx, _status_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("x5233-1".to_owned(), status_tx, event_tx);
        let mut outputs = OutputRegistry::default();
        outputs.set_record(Arc::new(move |packet| recorded.lock().push(packet)));
        NodeRuntime::new(SpawnContext {
            outputs,
            reporter,
            services: Arc::new(services),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        })
    }

    struct RecordingX5Client {
        requests: Arc<Mutex<Vec<CaptureRequest>>>,
        payload: X5233CapturePayload,
    }

    impl X5ControlClient for RecordingX5Client {
        fn probe(&self, _host: &str, _port: u16) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({"ok": true}))
        }

        fn status(&self, _host: &str, _port: u16) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({"ok": true}))
        }

        fn capture(
            &self,
            _host: &str,
            _port: u16,
            request: &CaptureRequest,
        ) -> Result<X5233CapturePayload, String> {
            self.requests.lock().push(request.clone());
            Ok(self.payload.clone())
        }
    }

    #[test]
    fn rtsp_open_all_keeps_ch0_and_ch3_streams_active() {
        let configs = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(Mutex::new(Vec::new()));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let services = EngineServices {
            stream_factory: Some(Arc::new(RecordingX5StreamFactory {
                configs: Arc::clone(&configs),
                opened: Arc::clone(&opened),
                frames: Arc::clone(&frames),
            })),
            ..EngineServices::default()
        };
        let mut rt = runtime_with_record(services, Arc::clone(&emitted));
        let spec = x5_spec();
        let mut node = X5233DriverNode::new(spec);

        node.on_action(
            NodeAction::Custom {
                name: "open_rtsp_all".to_owned(),
                payload: serde_json::Value::Null,
            },
            &mut rt,
        )
        .expect("open both X5 RTSP video channels");
        let slots = frames.lock().clone();
        assert_eq!(slots.len(), 2);
        for (channel, slot) in slots {
            slot.publish(decoded_video_frame(90 + u64::from(channel), channel));
        }
        for _ in 0..50 {
            if emitted.lock().len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        node.on_action(
            NodeAction::Custom {
                name: "close_rtsp".to_owned(),
                payload: serde_json::Value::Null,
            },
            &mut rt,
        )
        .expect("close X5 RTSP video");
        rt.stop_background();

        let configs = configs.lock();
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].url, "rtsp://camera.local:554/PRR");
        assert_eq!(configs[0].channel, 0);
        assert_eq!(configs[1].url, "rtsp://camera.local:557/PRR");
        assert_eq!(configs[1].channel, 3);
        assert_eq!(opened.lock().as_slice(), ["x5-x5-1-ch0", "x5-x5-1-ch3"]);
        let packets = emitted.lock();
        assert_eq!(packets.len(), 2);
        let mut channels = packets
            .iter()
            .map(|packet| {
                let DataPacket::VideoFrame(frame) = packet else {
                    panic!("X5 video output must emit stream.video-frame, got {packet:?}");
                };
                frame.identity.channel
            })
            .collect::<Vec<_>>();
        channels.sort_unstable();
        assert_eq!(channels, [0, 3]);
    }

    fn yuv_payload(channel: u16) -> X5233CapturePayload {
        X5233CapturePayload::Nv12 {
            channel,
            width: 2,
            height: 2,
            y_len: 4,
            uv_len: 2,
            frame_id: 42,
            timestamp_ns: 123_456,
            payload: Arc::from([1_u8, 2, 3, 4, 5, 6]),
        }
    }

    #[test]
    fn capture_request_emits_nv12_with_exact_device_timestamp_identity() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let services = EngineServices {
            x5_client: Some(Arc::new(RecordingX5Client {
                requests: Arc::clone(&requests),
                payload: yuv_payload(3),
            })),
            ..EngineServices::default()
        };
        let mut rt = runtime_with_record(services, Arc::clone(&emitted));
        let mut node = X5233DriverNode::new(x5_spec());
        let request = CaptureRequest {
            target: CaptureTarget::Yuv { channel: 3 },
            mode: CaptureMode::TimestampNs(123_456),
            source_identity: None,
        };

        node.on_input(
            "capture",
            DataPacket::CaptureRequest(Arc::new(request.clone())),
            &mut rt,
        )
        .expect("X5 NV12 capture");

        assert_eq!(requests.lock().as_slice(), [request]);
        let packets = emitted.lock();
        let DataPacket::ImageFrame(frame) = &packets[0] else {
            panic!("capture must emit image.frame");
        };
        assert_eq!(frame.format, ImageFrameFormat::Nv12);
        assert_eq!(frame.identity.frame_sequence, 42);
        assert!(matches!(
            &frame.identity.provenance,
            FrameProvenance::Device {
                driver,
                channel: 3,
                camera: None,
                timestamp_ns: 123_456,
            } if driver == "x5_233"
        ));
        assert_eq!(frame.identity.device_timestamp_ns(), Some(123_456));
        assert!(matches!(
            frame.identity.source_pts,
            SourcePts::Unavailable { .. }
        ));
    }

    #[test]
    fn capture_yuv_action_uses_capture_request_path() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let services = EngineServices {
            x5_client: Some(Arc::new(RecordingX5Client {
                requests: Arc::clone(&requests),
                payload: yuv_payload(3),
            })),
            ..EngineServices::default()
        };
        let (mut rt, _outputs) = runtime(services);
        let mut node = X5233DriverNode::new(x5_spec());
        node.on_action(
            NodeAction::Custom {
                name: "capture_yuv".to_owned(),
                payload: serde_json::Value::Null,
            },
            &mut rt,
        )
        .expect("capture_yuv action");
        assert!(matches!(
            requests.lock().as_slice(),
            [CaptureRequest {
                target: CaptureTarget::Yuv { channel: 3 },
                mode: CaptureMode::Latest,
                ..
            }]
        ));
    }

    #[test]
    fn capture_raw_action_uses_raw_camera_path() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let services = EngineServices {
            x5_client: Some(Arc::new(RecordingX5Client {
                requests: Arc::clone(&requests),
                payload: X5233CapturePayload::BayerRaw {
                    camera: 1,
                    width: 2,
                    height: 1,
                    stride_bytes: 4,
                    format_code: 24,
                    frame_id: 8,
                    timestamp_ns: 456_789,
                    payload: Arc::from([0_u8, 1, 2, 3]),
                },
            })),
            ..EngineServices::default()
        };
        let (mut rt, _outputs) = runtime(services);
        let mut node = X5233DriverNode::new(NodeSpec {
            id: "x5-1".to_owned(),
            kind: "x5233Driver".to_owned(),
            title: "X5_233 Driver".to_owned(),
            inputs: vec![],
            outputs: vec![],
            config: serde_json::json!({
                "host": "10.21.12.108",
                "tcpPort": 9073,
                "rawCamera": 1,
            }),
        });
        node.on_action(
            NodeAction::Custom {
                name: "capture_raw".to_owned(),
                payload: serde_json::Value::Null,
            },
            &mut rt,
        )
        .expect("capture_raw action");
        assert!(matches!(
            requests.lock().as_slice(),
            [CaptureRequest {
                target: CaptureTarget::Raw { camera: 1 },
                mode: CaptureMode::Latest,
                ..
            }]
        ));
    }

    #[test]
    fn raw_capture_emits_bayer_frame_with_default_metadata() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let services = EngineServices {
            x5_client: Some(Arc::new(RecordingX5Client {
                requests: Arc::clone(&requests),
                payload: X5233CapturePayload::BayerRaw {
                    camera: 0,
                    width: 2,
                    height: 1,
                    stride_bytes: 4,
                    format_code: 24,
                    frame_id: 8,
                    timestamp_ns: 456_789,
                    payload: Arc::from([0_u8, 1, 2, 3]),
                },
            })),
            ..EngineServices::default()
        };
        let mut rt = runtime_with_record(services, Arc::clone(&emitted));
        let mut node = X5233DriverNode::new(x5_spec());
        node.on_input(
            "capture",
            DataPacket::CaptureRequest(Arc::new(CaptureRequest {
                target: CaptureTarget::Raw { camera: 0 },
                mode: CaptureMode::Latest,
                source_identity: None,
            })),
            &mut rt,
        )
        .expect("X5 RAW capture");

        assert!(matches!(
            requests.lock().as_slice(),
            [CaptureRequest {
                target: CaptureTarget::Raw { camera: 0 },
                mode: CaptureMode::Latest,
                ..
            }]
        ));
        let packets = emitted.lock();
        let DataPacket::ImageFrame(frame) = &packets[0] else {
            panic!("RAW capture must emit image.frame");
        };
        assert_eq!(frame.format, ImageFrameFormat::BayerRaw);
        assert!(matches!(
            frame.raw,
            Some(RawMetadata {
                bayer_pattern: BayerPattern::Rggb,
                bits_per_sample: 12,
                ..
            })
        ));
    }

    #[test]
    fn raw_exact_capture_is_explicit_precondition_without_device_support() {
        let mut node = X5233DriverNode::new(x5_spec());
        let (mut rt, _outputs) = runtime(EngineServices::default());
        let error = node
            .on_input(
                "capture",
                DataPacket::CaptureRequest(Arc::new(CaptureRequest {
                    target: CaptureTarget::Raw { camera: 0 },
                    mode: CaptureMode::FrameId(42),
                    source_identity: None,
                })),
                &mut rt,
            )
            .expect_err("RAW exact capture has no X5 executor support");
        assert!(matches!(error, NodeError::Precondition(_)), "got {error:?}");
    }

    #[test]
    fn capture_without_x5_client_is_precondition() {
        let mut node = X5233DriverNode::new(x5_spec());
        let (mut rt, _outputs) = runtime(EngineServices::default());
        let error = node
            .on_input(
                "capture",
                DataPacket::CaptureRequest(Arc::new(CaptureRequest {
                    target: CaptureTarget::Yuv { channel: 0 },
                    mode: CaptureMode::Latest,
                    source_identity: None,
                })),
                &mut rt,
            )
            .expect_err("X5 client is required");
        assert!(matches!(error, NodeError::Precondition(_)), "got {error:?}");
    }

    fn hex_services(
        calls: Arc<Mutex<Vec<String>>>,
        positions: Arc<Mutex<Option<Vec<f64>>>>,
    ) -> EngineServices {
        EngineServices {
            hex_arm_client: Some(Arc::new(RecordingHexArmClient { calls, positions })),
            ..EngineServices::default()
        }
    }

    #[test]
    fn hex_arm_missing_service_is_precondition() {
        let mut node = HexArmDeviceNode {
            spec: hex_arm_spec(true, "0.1"),
            connected: false,
            api_control_initialized: false,
        };
        let (mut rt, _outputs) = runtime(EngineServices::default());
        let error = node
            .on_action(
                NodeAction::Custom {
                    name: "status".to_owned(),
                    payload: serde_json::Value::Null,
                },
                &mut rt,
            )
            .expect_err("missing Hex Arm service must be a precondition");
        assert!(matches!(error, NodeError::Precondition(_)));
    }

    #[test]
    fn hex_arm_motion_requires_control_enabled() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let positions = Arc::new(Mutex::new(None));
        let mut node = HexArmDeviceNode {
            spec: hex_arm_spec(false, "0.1"),
            connected: true,
            api_control_initialized: true,
        };
        let (mut rt, _outputs) = runtime(hex_services(Arc::clone(&calls), positions));
        let error = node
            .on_action(
                NodeAction::Custom {
                    name: "send_joint_positions".to_owned(),
                    payload: serde_json::Value::Null,
                },
                &mut rt,
            )
            .expect_err("motion must be disabled when controlEnabled=false");
        assert!(matches!(error, NodeError::Precondition(_)));
        assert!(calls.lock().is_empty());
    }

    #[test]
    fn ssh_session_executes_without_host_key() {
        let targets = Arc::new(Mutex::new(Vec::new()));
        let services = EngineServices {
            ssh_command_executor: Some(Arc::new(RecordingSshCommandExecutor {
                targets: Arc::clone(&targets),
            })),
            ..EngineServices::default()
        };
        let (mut rt, _outputs) = runtime(services);
        let mut node = SshSessionNode {
            spec: ssh_session_spec(),
        };

        node.on_action(NodeAction::Trigger, &mut rt)
            .expect("SSH command executes without a host key");

        assert_eq!(
            targets.lock().as_slice(),
            [ControlTargetSpec {
                host: "camera.local".to_owned(),
                port: 22,
                username: "root".to_owned(),
                expected_host_key: None,
            }]
        );
    }

    #[test]
    fn ssh_session_discards_legacy_host_key_before_command_execution() {
        let targets = Arc::new(Mutex::new(Vec::new()));
        let services = EngineServices {
            ssh_command_executor: Some(Arc::new(RecordingSshCommandExecutor {
                targets: Arc::clone(&targets),
            })),
            ..EngineServices::default()
        };
        let (mut rt, _outputs) = runtime(services);
        let mut spec = ssh_session_spec();
        spec.config
            .as_object_mut()
            .expect("SSH session config is an object")
            .insert(
                "expectedHostKey".to_owned(),
                serde_json::Value::String("not-an-openssh-public-key".to_owned()),
            );
        let mut node = SshSessionNode { spec };

        node.on_action(NodeAction::Trigger, &mut rt)
            .expect("legacy host key does not block SSH command execution");

        assert_eq!(targets.lock()[0].expected_host_key, None);
    }

    #[test]
    fn hex_arm_config_update_disconnects_only_for_target_changes() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let positions = Arc::new(Mutex::new(None));
        let mut node = HexArmDeviceNode {
            spec: hex_arm_spec(true, "0.1"),
            connected: true,
            api_control_initialized: true,
        };
        let (mut rt, _outputs) = runtime(hex_services(Arc::clone(&calls), Arc::clone(&positions)));

        let updated = hex_arm_spec(true, "0.2,0.3").config;
        node.on_config_update(updated, &mut rt)
            .expect("joint-only update must preserve session");
        assert!(calls.lock().is_empty());
        node.on_action(
            NodeAction::Custom {
                name: "send_joint_positions".to_owned(),
                payload: serde_json::Value::Null,
            },
            &mut rt,
        )
        .unwrap();
        assert_eq!(*positions.lock(), Some(vec![0.2, 0.3]));

        let mut host_changed = hex_arm_spec(true, "0.2,0.3").config;
        host_changed["host"] = serde_json::json!("new-hex-arm.local");
        node.on_config_update(host_changed, &mut rt)
            .expect("host update must disconnect old session");
        assert_eq!(
            calls.lock().as_slice(),
            ["send_joint_positions", "disconnect"]
        );
    }
}
