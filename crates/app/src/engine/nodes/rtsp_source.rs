//! RTSP 源节点：连接 RTSP 后经 `StreamService` 解码并向输出推送视频帧。
//!
//! 引擎把「RTSP 连接 + 解码」合并在本节点（`StreamService` 内部已解码为 RGBA），
//! 因此输出为 `stream.video-frame`；独立的 `rtspDecoder` 节点语义在引擎层为 no-op。

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use crate::{
    engine::{
        DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime,
        NodeRuntimeState, NodeSpec, SpawnContext,
    },
    platform::{
        LatestDecodedFrameSlot, RtspCodec, RtspLatencyMode, RtspStreamConfig, RtspTransport,
        StreamCancellation, StreamOpenRequest, StreamOperationControl, StreamRecordingRequest,
        StreamService, StreamServiceEvent, StreamSessionId, StreamTimeouts,
    },
};

pub struct RtspSourceFactory;

impl NodeFactory for RtspSourceFactory {
    fn kind(&self) -> &'static str {
        crate::engine::node::kinds::RTSP_SOURCE
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        // 输出端口 id 跟随 web 图（rtspSource 输出 "endpoint"），避免硬编码导致接线断裂。
        let output_port = spec
            .outputs
            .first()
            .map(|port| port.id.clone())
            .unwrap_or_else(|| "endpoint".to_owned());
        Ok(Box::new(RtspSourceNode {
            spec,
            output_port,
            cancellation: None,
            pump_cancel: None,
        }))
    }
}

pub struct RtspSourceNode {
    spec: NodeSpec,
    output_port: String,
    cancellation: Option<StreamCancellation>,
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
        _port: &str,
        _packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
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
        if self.cancellation.is_some() {
            return Ok(());
        }
        let url = config_string(&self.spec, "url", "");
        if url.is_empty() {
            return Err(NodeError::Config("RTSP url is required".to_owned()));
        }
        let width = config_u32(&self.spec, "width", 960);
        let height = config_u32(&self.spec, "height", 540);
        let channel = config_u16(&self.spec, "channel", 0);
        let transport = match config_string(&self.spec, "transport", "tcp").as_str() {
            "udp" => RtspTransport::Udp,
            _ => RtspTransport::Tcp,
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
        let cancellation = StreamCancellation::default();
        let reporter = stream_reporter(rt);
        let control = StreamOperationControl::new(
            StreamTimeouts::default(),
            cancellation.clone(),
            reporter,
        )
        .map_err(|error| NodeError::Execution(error.to_string()))?;
        let session = service
            .open(session_id, request, control)
            .map_err(|error| NodeError::Execution(error.to_string()))?;

        let latest_frame = Arc::clone(&session.latest_frame);
        self.cancellation = Some(cancellation);
        let output_port = self.output_port.clone();
        let pump_cancel = Arc::new(AtomicBool::new(false));
        let pump_cancel_flag = Arc::clone(&pump_cancel);
        rt.spawn(format!("rtsp-pump-{}", self.spec.id), move |ctx| {
            pump_frames(latest_frame, ctx, pump_cancel_flag, output_port);
        });

        rt.report_state(NodeRuntimeState::Running, "streaming");
        Ok(())
    }

    fn disconnect(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
        if let Some(cancel) = self.pump_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        rt.report_state(NodeRuntimeState::Idle, "disconnected");
        Ok(())
    }
}

fn stream_reporter(rt: &NodeRuntime) -> Arc<dyn Fn(StreamServiceEvent) + Send + Sync> {
    let reporter = rt.context().reporter.clone();
    Arc::new(move |event| {
        reporter.report_event(format!("stream: {event:?}"));
    })
}

fn pump_frames(
    latest: Arc<LatestDecodedFrameSlot>,
    ctx: SpawnContext,
    cancel: Arc<AtomicBool>,
    output_port: String,
) {
    let mut last_sequence: Option<u64> = None;
    while !cancel.load(Ordering::Acquire) {
        if let Some(frame) = latest.latest()
            && last_sequence != Some(frame.identity.frame_sequence)
        {
            last_sequence = Some(frame.identity.frame_sequence);
            let _ = ctx.outputs.emit(&output_port, DataPacket::VideoFrame(frame));
        }
        thread::sleep(Duration::from_millis(5));
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
