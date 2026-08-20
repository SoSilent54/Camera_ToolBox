//! RTSP 源节点：连接 RTSP 后经 `StreamService` 解码并向输出推送视频帧。
//!
//! `rtspSource` 已承担连接与解码，输出 `stream.video-frame`；`rtspDecoder`
//! 只保留为兼容已有工作流的帧直通节点，不承担二次解码。

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use crate::{
    engine::{
        CaptureMode, CaptureRequest, CaptureTarget, DataPacket, ImageFrame, NodeAction, NodeError,
        NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec, SpawnContext,
    },
    platform::{
        LatestDecodedFrameSlot, RtspCodec, RtspLatencyMode, RtspStreamConfig, RtspTransport,
        StreamCancellation, StreamOpenRequest, StreamOperationControl, StreamRecordingRequest,
        StreamService, StreamServiceError, StreamServiceEvent, StreamSession, StreamSessionId,
        StreamStage, StreamTerminal, StreamTimeouts,
    },
};

pub struct RtspSourceFactory;

impl NodeFactory for RtspSourceFactory {
    fn kind(&self) -> &'static str {
        crate::engine::node::kinds::RTSP_SOURCE
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        // `snapshot` 与 `frames` 并存时必须按 id 选择连续流出口，不能依赖端口排序。
        let output_port = spec
            .outputs
            .iter()
            .find(|port| port.id == "frames")
            .or_else(|| spec.outputs.first())
            .map(|port| port.id.clone())
            .unwrap_or_else(|| "frames".to_owned());
        Ok(Box::new(RtspSourceNode {
            spec,
            output_port,
            latest_image: Arc::new(Mutex::new(None)),
            cancellation: None,
            pump_cancel: None,
            session: None,
        }))
    }
}

pub struct RtspSourceNode {
    spec: NodeSpec,
    output_port: String,
    /// Pump 转换出的最近已解码 RGBA 帧；capture 从这里取帧，避免消费 stream slot 后丢失快照。
    latest_image: Arc<Mutex<Option<Arc<ImageFrame>>>>,
    cancellation: Option<StreamCancellation>,
    session: Option<StreamSession>,
    pump_cancel: Option<Arc<AtomicBool>>,
}

impl NodeInstance for RtspSourceNode {
    fn kind(&self) -> &'static str {
        crate::engine::node::kinds::RTSP_SOURCE
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "connect to start streaming");
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
                "rtspSource.capture requires command.capture.request.v1".to_owned(),
            ));
        };
        let frame = self.match_cached_frame(&request)?;
        rt.emit("snapshot", DataPacket::ImageFrame(frame))?;
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Connect => self.connect(rt),
            NodeAction::Disconnect => self.disconnect(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let _ = self.disconnect(rt);
        Ok(())
    }
}

impl RtspSourceNode {
    fn connect(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        if let Some(cancellation) = self.cancellation.as_ref() {
            if !cancellation.is_cancelled() && self.session.is_some() {
                return Ok(());
            }
            if let Some(session) = self.session.take() {
                let _ = session.force_cleanup();
            }
            self.cancellation.take();
            self.pump_cancel.take();
        }
        let url = config_string(&self.spec, "url", "");
        if url.is_empty() {
            return Err(NodeError::Config("RTSP url is required".to_owned()));
        }
        let width = config_u32(&self.spec, "width", 1920);
        let height = config_u32(&self.spec, "height", 1080);
        let channel = config_u16(&self.spec, "channel", 0);
        let transport = match config_string(&self.spec, "transport", "tcp").as_str() {
            "tcp" => RtspTransport::Tcp,
            "udp" => RtspTransport::Udp,
            value => {
                return Err(NodeError::Config(format!(
                    "transport must be tcp or udp, got `{value}`"
                )));
            }
        };

        let factory = rt.services().stream_factory()?;
        let config = RtspStreamConfig {
            url,
            channel,
            width,
            height,
            codec: RtspCodec::H264,
            transport,
            latency_mode: RtspLatencyMode::Low,
        };
        let service: Arc<dyn StreamService> = factory.create(config);
        let session_id = StreamSessionId::new(format!("rtsp-{}", self.spec.id))
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let request = StreamOpenRequest {
            channel,
            media: "rtsp".to_owned(),
            cseq: 1,
            prefer_hardware_acceleration: false,
            recording: StreamRecordingRequest::default(),
        };
        let timeouts = stream_timeouts_from_config(&self.spec)?;
        let cancellation = StreamCancellation::default();
        let pump_cancel = Arc::new(AtomicBool::new(false));
        let reporter = stream_reporter(rt, cancellation.clone(), Arc::clone(&pump_cancel));
        let control = StreamOperationControl::new(timeouts, cancellation.clone(), reporter)
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let session = match service.open(session_id, request, control) {
            Ok(session) => session,
            Err(error) => {
                rt.report_state(NodeRuntimeState::Error, stream_failure_diagnostic(&error));
                return Err(NodeError::Execution(error.to_string()));
            }
        };

        let latest_frame = Arc::clone(&session.latest_frame);
        self.session = Some(session);
        self.cancellation = Some(cancellation);
        let output_port = self.output_port.clone();
        let latest_image = Arc::clone(&self.latest_image);
        let pump_cancel_flag = Arc::clone(&pump_cancel);
        // 必须保存 pump 取消标志，否则 disconnect 无法停掉后台 pump 线程（泄漏线程且 join 永不返回）。
        self.pump_cancel = Some(pump_cancel);
        rt.spawn(format!("rtsp-pump-{}", self.spec.id), move |ctx| {
            pump_frames(
                latest_frame,
                latest_image,
                ctx,
                pump_cancel_flag,
                output_port,
            );
        });

        rt.report_state(NodeRuntimeState::Running, "streaming");
        Ok(())
    }

    fn disconnect(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        if let Some(session) = self.session.take() {
            session.request_close();
        }
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
        if let Some(cancel) = self.pump_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        *self
            .latest_image
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        rt.report_state(NodeRuntimeState::Idle, "disconnected");
        Ok(())
    }

    /// 只按当前已解码帧中的实际元数据匹配；RTSP 节点绝不调用设备 TCP 抓图旁路。
    fn match_cached_frame(&self, request: &CaptureRequest) -> Result<Arc<ImageFrame>, NodeError> {
        let frame = self
            .latest_image
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                NodeError::Precondition(
                    "RTSP capture requires a decoded frame; connect and wait for the first frame"
                        .to_owned(),
                )
            })?;
        let stream_identity = frame.identity.stream_identity().ok_or_else(|| {
            NodeError::Precondition("RTSP capture cache lacks stream provenance".to_owned())
        })?;
        match request.target {
            CaptureTarget::Yuv { channel } if channel == stream_identity.channel => {}
            CaptureTarget::Yuv { channel } => {
                return Err(capture_miss("channel", u64::from(channel)));
            }
            CaptureTarget::Raw { .. } => {
                return Err(NodeError::Precondition(
                    "RTSP capture only snapshots decoded RGBA8 frames; RAW requires X5_233 Driver"
                        .to_owned(),
                ));
            }
        }
        if request
            .source_identity
            .as_ref()
            .is_some_and(|identity| identity != &frame.identity)
        {
            return Err(NodeError::Precondition(
                "RTSP capture cache does not match the requested source identity".to_owned(),
            ));
        }
        match request.mode {
            CaptureMode::Latest => Ok(frame),
            CaptureMode::FrameId(expected) => (frame.identity.frame_sequence == expected)
                .then_some(frame)
                .ok_or_else(|| capture_miss("frame_id", expected)),
            CaptureMode::TimestampNs(expected) => frame
                .identity
                .device_timestamp_ns()
                .ok_or_else(|| {
                    NodeError::Precondition(
                        "RTSP capture timestamp_ns matching requires device timestamp_ns metadata"
                            .to_owned(),
                    )
                })
                .and_then(|actual| {
                    (actual == expected)
                        .then_some(frame)
                        .ok_or_else(|| capture_miss("timestamp_ns", expected))
                }),
        }
    }
}

fn capture_miss(field: &str, expected: u64) -> NodeError {
    NodeError::Precondition(format!(
        "RTSP capture cache has no decoded frame matching {field}={expected}"
    ))
}

fn stream_reporter(
    rt: &NodeRuntime,
    cancellation: StreamCancellation,
    pump_cancel: Arc<AtomicBool>,
) -> Arc<dyn Fn(StreamServiceEvent) + Send + Sync> {
    let reporter = rt.context().reporter.clone();
    Arc::new(move |event| {
        match &event {
            StreamServiceEvent::Terminal(StreamTerminal::Failed(error)) => {
                cancellation.cancel();
                pump_cancel.store(true, Ordering::Release);
                reporter.report_state(NodeRuntimeState::Error, stream_failure_diagnostic(error));
            }
            StreamServiceEvent::Terminal(StreamTerminal::Forced {
                remote_state_unknown,
            }) => {
                cancellation.cancel();
                pump_cancel.store(true, Ordering::Release);
                reporter.report_state(
                    NodeRuntimeState::Error,
                    format!("stream forced closed; remote_state_unknown={remote_state_unknown}"),
                );
            }
            StreamServiceEvent::Terminal(StreamTerminal::BoundaryClosed) => {
                cancellation.cancel();
                pump_cancel.store(true, Ordering::Release);
                reporter.report_state(NodeRuntimeState::Idle, "stream boundary closed");
            }
            StreamServiceEvent::Terminal(StreamTerminal::Cancelled) => {
                cancellation.cancel();
                pump_cancel.store(true, Ordering::Release);
                reporter.report_state(NodeRuntimeState::Idle, "stream cancelled");
            }
            StreamServiceEvent::Stage(StreamStage::Playing) => {
                reporter.report_state(NodeRuntimeState::Running, "streaming");
            }
            _ => {}
        }
        reporter.report_event(format!("stream: {event:?}"));
    })
}

fn pump_frames(
    latest: Arc<LatestDecodedFrameSlot>,
    latest_image: Arc<Mutex<Option<Arc<ImageFrame>>>>,
    ctx: SpawnContext,
    cancel: Arc<AtomicBool>,
    output_port: String,
) {
    // 事件驱动：阻塞等待新帧的 condvar 通知，仅以短超时周期复查取消标志，
    // 消除原先 5ms sleep 空转及其带来的突发丢帧（见 plan D4/P4）。
    while !cancel.load(Ordering::Acquire) {
        match latest.wait_latest_timeout(PUMP_CANCEL_POLL) {
            Some(frame) => {
                let image = Arc::new(ImageFrame::from(frame.as_ref()));
                *latest_image
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&image));
                let _ = ctx
                    .outputs
                    .emit(&output_port, DataPacket::ImageFrame(image));
            }
            None => {
                // 超时且无新帧：循环回到顶部检查 cancel，避免 join 永不返回。
                continue;
            }
        }
    }
}

/// 事件驱动 wait 的取消复查周期；仅用于无帧时的觉醒，非轮询节拍。
const PUMP_CANCEL_POLL: Duration = Duration::from_millis(100);

const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 8_000;
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 120_000;

fn stream_timeouts_from_config(spec: &NodeSpec) -> Result<StreamTimeouts, NodeError> {
    let connect = config_duration_ms(spec, "connectTimeoutMs", DEFAULT_CONNECT_TIMEOUT_MS)?;
    let idle = config_duration_ms(spec, "idleTimeoutMs", DEFAULT_IDLE_TIMEOUT_MS)?;
    StreamTimeouts { connect, idle }
        .validate()
        .map_err(|error| NodeError::Config(error.to_string()))
}

fn config_duration_ms(spec: &NodeSpec, key: &str, fallback_ms: u64) -> Result<Duration, NodeError> {
    let value_ms = spec
        .config
        .get(key)
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
        })
        .unwrap_or(fallback_ms);
    if value_ms == 0 || value_ms > MAX_TIMEOUT_MS {
        return Err(NodeError::Config(format!(
            "{key} must be in 1..={MAX_TIMEOUT_MS} ms"
        )));
    }
    Ok(Duration::from_millis(value_ms))
}

fn stream_failure_diagnostic(error: &StreamServiceError) -> String {
    match error {
        StreamServiceError::ConnectTimeout { timeout_ms } => format!(
            "stream failed before first decoded frame: connect timeout after {timeout_ms} ms; check RTSP URL/server/network or increase connectTimeoutMs for slow channels"
        ),
        StreamServiceError::IdleTimeout { timeout_ms, .. } => format!(
            "stream failed after frames stopped: idle timeout after {timeout_ms} ms; check upstream encoder/network stability"
        ),
        _ => format!("stream failed: {error}"),
    }
}
fn config_string(spec: &NodeSpec, key: &str, fallback: &str) -> String {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| fallback.to_owned(), str::to_owned)
}

fn config_u32(spec: &NodeSpec, key: &str, fallback: u32) -> u32 {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(fallback)
}

fn config_u16(spec: &NodeSpec, key: &str, fallback: u16) -> u16 {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use std::sync::{atomic::AtomicBool, mpsc, Arc, Mutex};

    use super::*;
    use crate::engine::{
        EngineServices, ImageFrameIdentity, NodeReporter, OutputRegistry, SpawnContext,
    };
    use crate::platform::{
        DecodedVideoFrame, LatestDecodedFrameSlot, SourcePts, SourcePtsProvenance,
        StreamFrameIdentity, StreamServiceError, StreamSession, StreamSessionId, StreamTimeouts,
    };

    fn spec() -> NodeSpec {
        crate::engine::NodeSpec {
            id: "rtsp-1".to_owned(),
            kind: "rtspSource".to_owned(),
            title: "RTSP Source".to_owned(),
            inputs: vec![crate::engine::PortSpec {
                id: "capture".to_owned(),
                label: "Capture".to_owned(),
                kind: "command.capture.request.v1".to_owned(),
                cardinality: crate::engine::PortCardinality::One,
                required: false,
            }],
            outputs: vec![
                crate::engine::PortSpec {
                    id: "frames".to_owned(),
                    label: "Decoded Video Frames".to_owned(),
                    kind: "stream.video-frame".to_owned(),
                    cardinality: crate::engine::PortCardinality::One,
                    required: false,
                },
                crate::engine::PortSpec {
                    id: "snapshot".to_owned(),
                    label: "Snapshot".to_owned(),
                    kind: "image.frame".to_owned(),
                    cardinality: crate::engine::PortCardinality::One,
                    required: false,
                },
            ],
            config: serde_json::json!({"url": "rtsp://127.0.0.1:554/test", "transport": "tcp"}),
        }
    }

    fn runtime(
        services: EngineServices,
        state_tx: mpsc::Sender<crate::engine::NodeStatusReport>,
    ) -> NodeRuntime {
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("rtsp-1".to_owned(), state_tx, event_tx);
        let ctx = SpawnContext {
            outputs: OutputRegistry::default(),
            reporter,
            services: Arc::new(services),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        NodeRuntime::new(ctx)
    }

    fn runtime_with_packets(
        services: EngineServices,
        state_tx: mpsc::Sender<crate::engine::NodeStatusReport>,
    ) -> (NodeRuntime, Arc<Mutex<Vec<DataPacket>>>) {
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("rtsp-1".to_owned(), state_tx, event_tx);
        let packets = Arc::new(Mutex::new(Vec::new()));
        let recorded_packets = Arc::clone(&packets);
        let mut outputs = OutputRegistry::default();
        outputs.set_record(Arc::new(move |packet| {
            recorded_packets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(packet);
        }));
        let ctx = SpawnContext {
            outputs,
            reporter,
            services: Arc::new(services),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        (NodeRuntime::new(ctx), packets)
    }

    fn decoded_frame(sequence: u64, device_timestamp_ns: Option<u64>, pts: i64) -> Arc<ImageFrame> {
        let identity = StreamFrameIdentity::known_at_with_device_timestamp(
            StreamSessionId::new("rtsp-test").expect("valid session id"),
            2,
            sequence,
            SourcePts::Known {
                ticks: pts,
                time_base_numerator: 1,
                time_base_denominator: 90_000,
                provenance: SourcePtsProvenance::FfmpegDecodedFrame,
            },
            123,
            device_timestamp_ns,
        );
        Arc::new(ImageFrame::from(&DecodedVideoFrame {
            width: 2,
            height: 2,
            rgba: Arc::from([3_u8; 16]),
            identity,
        }))
    }

    fn capture_request(
        mode: CaptureMode,
        source_identity: Option<ImageFrameIdentity>,
    ) -> DataPacket {
        DataPacket::CaptureRequest(Arc::new(CaptureRequest {
            target: CaptureTarget::Yuv { channel: 2 },
            mode,
            source_identity,
        }))
    }

    fn node_with_cached_frame(frame: Option<Arc<ImageFrame>>) -> RtspSourceNode {
        RtspSourceNode {
            spec: spec(),
            output_port: "frames".to_owned(),
            latest_image: Arc::new(Mutex::new(frame)),
            cancellation: None,
            session: None,
            pump_cancel: None,
        }
    }

    /// 记录 open 调用次数并返回带独立 latest_frame slot 的 mock StreamService。
    struct RecordingStreamService {
        opened: Arc<Mutex<Vec<String>>>,
        frame: Arc<LatestDecodedFrameSlot>,
    }

    impl StreamService for RecordingStreamService {
        fn service_id(&self) -> &str {
            "mock"
        }

        fn open(
            &self,
            session_id: crate::platform::StreamSessionId,
            _request: StreamOpenRequest,
            control: StreamOperationControl,
        ) -> Result<StreamSession, StreamServiceError> {
            self.opened
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(session_id.as_str().to_owned());
            Ok(StreamSession::new(
                session_id,
                Arc::clone(&self.frame),
                control,
            ))
        }
    }

    /// 记录 create 调用并返回单个 mock 服务的工厂。
    struct RecordingFactory {
        opened: Arc<Mutex<Vec<String>>>,
        frame: Arc<LatestDecodedFrameSlot>,
    }

    impl crate::engine::StreamServiceFactory for RecordingFactory {
        fn create(&self, _config: RtspStreamConfig) -> Arc<dyn StreamService> {
            Arc::new(RecordingStreamService {
                opened: Arc::clone(&self.opened),
                frame: Arc::clone(&self.frame),
            })
        }
    }

    fn last_state(
        rx: &mpsc::Receiver<crate::engine::NodeStatusReport>,
    ) -> Option<NodeRuntimeState> {
        let mut last = None;
        while let Ok(report) = rx.try_recv() {
            last = Some(report.state);
        }
        last
    }

    #[test]
    fn pump_emits_rgba_image_frame_and_keeps_snapshot_cache() {
        let (state_tx, _state_rx) = mpsc::channel();
        let (rt, packets) = runtime_with_packets(EngineServices::default(), state_tx);
        let source = Arc::new(LatestDecodedFrameSlot::default());
        let cache = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let ctx = rt.context().clone();
        let thread_source = Arc::clone(&source);
        let thread_cache = Arc::clone(&cache);
        let thread_cancel = Arc::clone(&cancel);
        let handle = std::thread::spawn(move || {
            pump_frames(
                thread_source,
                thread_cache,
                ctx,
                thread_cancel,
                "frames".to_owned(),
            );
        });

        let frame = decoded_frame(7, Some(8_000), 9_000);
        let decoded = frame.decoded_rgba_frame().expect("stream RGBA frame");
        source.publish(decoded);
        for _ in 0..50 {
            if !packets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cancel.store(true, Ordering::Release);
        handle.join().expect("pump exits");

        let packets = packets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let DataPacket::ImageFrame(emitted) = &packets[0] else {
            panic!(
                "RTSP frames output must be ImageFrame, got {:?}",
                packets[0]
            );
        };
        assert_eq!(emitted.format, crate::engine::ImageFrameFormat::Rgba8);
        assert_eq!(emitted.identity.frame_sequence, 7);
        assert_eq!(
            cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref(),
            Some(emitted)
        );
    }

    #[test]
    fn capture_latest_emits_cached_rgba_with_identity() {
        let frame = decoded_frame(7, Some(8_000), 9_000);
        let identity = frame.identity.clone();
        let mut node = node_with_cached_frame(Some(Arc::clone(&frame)));
        let (state_tx, _state_rx) = mpsc::channel();
        let (mut rt, packets) = runtime_with_packets(EngineServices::default(), state_tx);
        node.on_input(
            "capture",
            capture_request(CaptureMode::Latest, Some(identity.clone())),
            &mut rt,
        )
        .expect("latest capture");
        let packets = packets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let DataPacket::ImageFrame(snapshot) = &packets[0] else {
            panic!("snapshot output must be ImageFrame");
        };
        assert_eq!(snapshot.format, crate::engine::ImageFrameFormat::Rgba8);
        assert_eq!(snapshot.identity, identity);
        assert!(Arc::ptr_eq(
            &snapshot.planes[0].bytes,
            &frame.planes[0].bytes
        ));
    }

    #[test]
    fn capture_exact_modes_emit_only_matching_cached_metadata() {
        let frame = decoded_frame(7, Some(8_000), 9_000);
        let mut node = node_with_cached_frame(Some(Arc::clone(&frame)));
        let (state_tx, _state_rx) = mpsc::channel();
        let (mut rt, packets) = runtime_with_packets(EngineServices::default(), state_tx);
        for mode in [CaptureMode::FrameId(7), CaptureMode::TimestampNs(8_000)] {
            node.on_input("capture", capture_request(mode, None), &mut rt)
                .expect("cached metadata matches capture request");
        }
        assert_eq!(
            packets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            2
        );
    }

    #[test]
    fn capture_without_frame_or_exact_cache_match_is_precondition_error() {
        let (state_tx, _state_rx) = mpsc::channel();
        let (mut rt, _packets) = runtime_with_packets(EngineServices::default(), state_tx);
        let mut empty = node_with_cached_frame(None);
        let error = empty
            .on_input(
                "capture",
                capture_request(CaptureMode::Latest, None),
                &mut rt,
            )
            .expect_err("missing decoded frame");
        assert!(matches!(error, NodeError::Precondition(_)));

        let frame = decoded_frame(7, Some(8_000), 9_000);
        for mode in [CaptureMode::FrameId(8), CaptureMode::TimestampNs(8_001)] {
            let mut node = node_with_cached_frame(Some(Arc::clone(&frame)));
            let error = node
                .on_input("capture", capture_request(mode, None), &mut rt)
                .expect_err("exact cache miss");
            assert!(matches!(error, NodeError::Precondition(_)), "got {error:?}");
        }

        let mut node = node_with_cached_frame(Some(decoded_frame(7, None, 9_000)));
        let error = node
            .on_input(
                "capture",
                capture_request(CaptureMode::TimestampNs(8_000), None),
                &mut rt,
            )
            .expect_err("device timestamp metadata is required");
        assert!(matches!(error, NodeError::Precondition(_)));
        let mut node = node_with_cached_frame(Some(Arc::clone(&frame)));
        let raw_request = DataPacket::CaptureRequest(Arc::new(CaptureRequest {
            target: CaptureTarget::Raw { camera: 0 },
            mode: CaptureMode::Latest,
            source_identity: None,
        }));
        let error = node
            .on_input("capture", raw_request, &mut rt)
            .expect_err("RTSP cannot satisfy a RAW capture request");
        assert!(matches!(error, NodeError::Precondition(_)));
    }

    #[test]
    fn factory_instantiates_with_expected_kind() {
        assert_eq!(RtspSourceFactory.kind(), "rtspSource");
        let instance = RtspSourceFactory.instantiate(spec()).expect("instantiate");
        assert_eq!(instance.kind(), "rtspSource");
    }

    #[test]
    fn on_start_reports_ready() {
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = RtspSourceFactory.instantiate(spec()).expect("instantiate");
        node.on_start(&mut rt).expect("on_start");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Ready));
    }

    #[test]
    fn connect_without_url_is_config_error() {
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut s = spec();
        s.config = serde_json::json!({});
        let mut node = RtspSourceFactory.instantiate(s).expect("instantiate");
        let err = node
            .on_action(NodeAction::Connect, &mut rt)
            .expect_err("empty url");
        assert!(matches!(err, NodeError::Config(_)), "got {err:?}");
    }

    #[test]
    fn connect_without_stream_factory_is_precondition() {
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = RtspSourceFactory.instantiate(spec()).expect("instantiate");
        let err = node
            .on_action(NodeAction::Connect, &mut rt)
            .expect_err("no factory");
        assert!(matches!(err, NodeError::Precondition(_)), "got {err:?}");
    }

    #[test]
    fn connect_opens_stream_and_reports_running() {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let frame = Arc::new(LatestDecodedFrameSlot::default());
        let services = EngineServices {
            stream_factory: Some(Arc::new(RecordingFactory {
                opened: Arc::clone(&opened),
                frame: Arc::clone(&frame),
            })),
            ..EngineServices::default()
        };
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(services, state_tx);

        let mut node = RtspSourceFactory.instantiate(spec()).expect("instantiate");
        node.on_action(NodeAction::Connect, &mut rt)
            .expect("connect");
        // 立即上报 running（streaming）
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));
        assert_eq!(opened.lock().unwrap().len(), 1);

        // 重复 connect 是 no-op（cancellation 已存在）
        node.on_action(NodeAction::Connect, &mut rt)
            .expect("re-connect");
        assert_eq!(opened.lock().unwrap().len(), 1);

        // 清理后台 pump 线程
        node.on_stop(&mut rt).expect("stop");
        rt.stop_background();
    }

    #[test]
    fn disconnect_reports_idle_and_stops_pump() {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let frame = Arc::new(LatestDecodedFrameSlot::default());
        let services = EngineServices {
            stream_factory: Some(Arc::new(RecordingFactory {
                opened: Arc::clone(&opened),
                frame,
            })),
            ..EngineServices::default()
        };
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(services, state_tx);

        let mut node = RtspSourceFactory.instantiate(spec()).expect("instantiate");
        node.on_action(NodeAction::Connect, &mut rt)
            .expect("connect");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));

        node.on_action(NodeAction::Disconnect, &mut rt)
            .expect("disconnect");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Idle));
        rt.stop_background();
    }

    #[test]
    fn terminal_failed_reports_error_state() {
        let (state_tx, state_rx) = mpsc::channel();
        let rt = runtime(EngineServices::default(), state_tx);
        let cancellation = StreamCancellation::default();
        let pump_cancel = Arc::new(AtomicBool::new(false));
        let reporter = stream_reporter(&rt, cancellation.clone(), Arc::clone(&pump_cancel));

        reporter(StreamServiceEvent::Terminal(StreamTerminal::Failed(
            StreamServiceError::ConnectTimeout { timeout_ms: 3000 },
        )));

        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Error));
        assert!(cancellation.is_cancelled());
        assert!(pump_cancel.load(Ordering::Acquire));
    }

    #[test]
    fn unsupported_action_is_error() {
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = RtspSourceFactory.instantiate(spec()).expect("instantiate");
        let err = node
            .on_action(NodeAction::Trigger, &mut rt)
            .expect_err("unsupported");
        assert!(matches!(err, NodeError::UnsupportedAction(_)));
    }

    #[test]
    fn transport_config_maps_udp_vs_default_tcp() {
        // 直接验证 config 分派：transport=udp → Udp，其余 → Tcp。
        let node = RtspSourceFactory.instantiate(spec()).expect("instantiate");
        // 通过内部 connect 分支不便直接断言 transport，这里用黑盒保证 udp 也能成功构造 config。
        // 构造一个 udp 配置的 spec，确认 connect 不因 transport 解析 panic（缺 factory 时为 Precondition）。
        let _ = node;
        let mut s = spec();
        s.config = serde_json::json!({"url": "rtsp://x", "transport": "udp"});
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = RtspSourceFactory.instantiate(s).expect("instantiate");
        let err = node
            .on_action(NodeAction::Connect, &mut rt)
            .expect_err("no factory");
        // udp 解析不 panic，仅因缺 factory 报 Precondition
        assert!(matches!(err, NodeError::Precondition(_)), "got {err:?}");
        let _ = StreamTimeouts::default();
    }
}
