//! 帧变换节点：由数据输入触发的转换节点（范式样板）。
//!
//! 引擎语义下 `rtspSource` 已把「连接 + 解码」合并，因此 `rtspDecoder` 是 pass-through；
//! `frameSampler` 按时间降采样；`videoLayer`/`imageLayer` 是可见性标记的 pass-through。
//!
//! 这是「转换节点」的完整样板：`on_input` 收到上游帧 → 变换 → `emit` 到输出端口。

use crate::{
    engine::{DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec},
    platform::host_monotonic_time_ns,
};

/// RTSP 解码节点：解码已在 `rtspSource` 内完成，这里原样转发视频帧。
pub struct RtspDecoderFactory;

impl NodeFactory for RtspDecoderFactory {
    fn kind(&self) -> &'static str {
        "rtspDecoder"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(PassThroughNode {
            kind: "rtspDecoder",
            output_port: output_port(&spec),
            active: false,
        }))
    }
}

/// 视频图层节点：可见性标记 + 帧转发。
pub struct VideoLayerFactory;

impl NodeFactory for VideoLayerFactory {
    fn kind(&self) -> &'static str {
        "videoLayer"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(PassThroughNode {
            kind: "videoLayer",
            output_port: output_port(&spec),
            active: false,
        }))
    }
}

/// 图像图层节点：可见性标记 + 帧转发。
pub struct ImageLayerFactory;

impl NodeFactory for ImageLayerFactory {
    fn kind(&self) -> &'static str {
        "imageLayer"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(PassThroughNode {
            kind: "imageLayer",
            output_port: output_port(&spec),
            active: false,
        }))
    }
}

/// pass-through 转换节点：原样转发视频/图像帧。
pub struct PassThroughNode {
    kind: &'static str,
    output_port: String,
    /// 是否已上报过 running；避免每帧重复上报，stop 后复位。
    active: bool,
}

/// 输出端口 id 跟随 web 图，避免硬编码导致接线断裂。
fn output_port(spec: &NodeSpec) -> String {
    spec.outputs
        .first()
        .map(|port| port.id.clone())
        .unwrap_or_else(|| "frames".to_owned())
}

impl NodeInstance for PassThroughNode {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for frames");
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        match packet {
            DataPacket::VideoFrame(_) | DataPacket::ImageFrame(_) => {
                // 收到第一帧即进入 running，让下游（viewer 等）能看到数据流已接通；
                // 后续帧不重复上报，stop 后由 on_stop 复位 active。
                if !self.active {
                    self.active = true;
                    rt.report_state(NodeRuntimeState::Running, "relaying frames");
                }
                let _ = rt.emit(&self.output_port, packet);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.active = false;
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 帧降采样节点：按目标 fps 丢弃中间帧。
pub struct FrameSamplerFactory;

impl NodeFactory for FrameSamplerFactory {
    fn kind(&self) -> &'static str {
        "frameSampler"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(FrameSamplerNode {
            spec,
            last_emit_ns: None,
            active: false,
        }))
    }
}

pub struct FrameSamplerNode {
    spec: NodeSpec,
    last_emit_ns: Option<u64>,
    /// 是否已上报过 running；避免每帧重复上报，stop 后复位。
    active: bool,
}

impl NodeInstance for FrameSamplerNode {
    fn kind(&self) -> &'static str {
        "frameSampler"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let fps_limit = config_f64(&self.spec, "fpsLimit", 30.0).max(1.0);
        rt.report_state(
            NodeRuntimeState::Ready,
            format!("downsampling to {fps_limit:.0} fps"),
        );
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let DataPacket::VideoFrame(frame) = packet else {
            return Ok(());
        };
        let now = host_monotonic_time_ns();
        let interval_ns = fps_interval_ns(&self.spec);
        if self
            .last_emit_ns
            .is_none_or(|last| now.saturating_sub(last) >= interval_ns)
        {
            self.last_emit_ns = Some(now);
            if !self.active {
                self.active = true;
                rt.report_state(NodeRuntimeState::Running, "downsampling frames");
            }
            let _ = rt.emit("frames", DataPacket::VideoFrame(frame));
        }
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.active = false;
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

fn fps_interval_ns(spec: &NodeSpec) -> u64 {
    let fps = config_f64(spec, "fpsLimit", 30.0).clamp(1.0, 240.0);
    (1_000_000_000.0 / fps) as u64
}

fn config_f64(spec: &NodeSpec, key: &str, fallback: f64) -> f64 {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};

    use super::*;
    use crate::engine::{
        NodeReporter, OutputRegistry, PortCardinality, PortSpec, SpawnContext,
    };
    use crate::platform::{DecodedVideoFrame, StreamFrameIdentity, StreamSessionId};

    fn out_port(id: &str) -> PortSpec {
        PortSpec {
            id: id.to_owned(),
            label: id.to_owned(),
            kind: "stream.video-frame".to_owned(),
            cardinality: PortCardinality::One,
            required: false,
        }
    }

    fn pt_spec(kind: &str, output_id: &str) -> NodeSpec {
        NodeSpec {
            id: "n".to_owned(),
            kind: kind.to_owned(),
            title: kind.to_owned(),
            inputs: vec![],
            outputs: vec![out_port(output_id)],
            config: serde_json::json!({}),
        }
    }

    /// 构造一个带 `record` 回调的 runtime：emit 无下游时也会把 packet 送进 record sink。
    fn runtime(outputs: OutputRegistry, state_tx: mpsc::Sender<crate::engine::NodeStatusReport>) -> NodeRuntime {
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("n".to_owned(), state_tx, event_tx);
        let ctx = SpawnContext {
            outputs,
            reporter,
            services: Arc::new(crate::engine::EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        NodeRuntime::new(ctx)
    }

    fn video_frame(seq: u64) -> DataPacket {
        let session = StreamSessionId::new("test-stream").expect("session id");
        DataPacket::VideoFrame(Arc::new(DecodedVideoFrame {
            width: 2,
            height: 2,
            rgba: Arc::from(vec![0u8; 16]),
            identity: StreamFrameIdentity::unavailable(
                session, 0, seq, "test",
            ),
        }))
    }

    fn last_state(rx: &mpsc::Receiver<crate::engine::NodeStatusReport>) -> Option<crate::engine::NodeRuntimeState> {
        let mut last = None;
        while let Ok(report) = rx.try_recv() {
            last = Some(report.state);
        }
        last
    }

    #[test]
    fn pass_through_factories_instantiate_with_expected_kinds() {
        let cases: [(&dyn NodeFactory, &str, &str); 3] = [
            (&RtspDecoderFactory, "rtspDecoder", "frames"),
            (&VideoLayerFactory, "videoLayer", "layer"),
            (&ImageLayerFactory, "imageLayer", "layer"),
        ];
        for (factory, kind, output_id) in cases {
            assert_eq!(factory.kind(), kind);
            let instance = factory.instantiate(pt_spec(kind, output_id)).expect("instantiate");
            assert_eq!(instance.kind(), kind);
        }
    }

    #[test]
    fn pass_through_reports_running_on_first_frame_and_relays() {
        let mut outputs = OutputRegistry::default();
        let relayed: Arc<Mutex<Vec<DataPacket>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |packet| sink.lock().unwrap().push(packet)));

        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let mut node = PassThroughNode {
            kind: "videoLayer",
            output_port: "layer".to_owned(),
            active: false,
        };

        // on_start → ready
        node.on_start(&mut rt).expect("on_start");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Ready));

        // 第一帧 → running + relay
        node.on_input("video", video_frame(1), &mut rt).expect("on_input");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));
        assert_eq!(relayed.lock().unwrap().len(), 1);

        // 第二帧 → 不重复上报状态（无新状态报告），但继续 relay
        node.on_input("video", video_frame(2), &mut rt).expect("on_input");
        assert_eq!(last_state(&state_rx), None);
        assert_eq!(relayed.lock().unwrap().len(), 2);
    }

    #[test]
    fn pass_through_ignores_non_frame_packets() {
        let mut outputs = OutputRegistry::default();
        let relayed: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |_| *sink.lock().unwrap() += 1));

        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let mut node = PassThroughNode {
            kind: "videoLayer",
            output_port: "layer".to_owned(),
            active: false,
        };

        node.on_input("video", DataPacket::Json(Arc::new(serde_json::json!({}))), &mut rt)
            .expect("on_input");
        assert_eq!(*relayed.lock().unwrap(), 0);
        assert_eq!(last_state(&state_rx), None); // 无帧，不进入 running
    }

    #[test]
    fn pass_through_stop_resets_to_idle_and_rearms() {
        let outputs = OutputRegistry::default();
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let mut node = PassThroughNode {
            kind: "videoLayer",
            output_port: "layer".to_owned(),
            active: false,
        };

        node.on_input("video", video_frame(1), &mut rt).expect("on_input");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));

        node.on_stop(&mut rt).expect("on_stop");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Idle));

        // stop 后 active 复位，再次收帧应重新上报 running
        node.on_input("video", video_frame(2), &mut rt).expect("on_input");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));
    }

    #[test]
    fn pass_through_rejects_actions() {
        let outputs = OutputRegistry::default();
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let mut node = PassThroughNode {
            kind: "videoLayer",
            output_port: "layer".to_owned(),
            active: false,
        };
        let err = node.on_action(NodeAction::Trigger, &mut rt).expect_err("unsupported");
        assert!(matches!(err, NodeError::UnsupportedAction(_)));
    }

    #[test]
    fn frame_sampler_rate_limits_by_fps_limit() {
        let mut outputs = OutputRegistry::default();
        let relayed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |packet| {
            if let DataPacket::VideoFrame(frame) = packet {
                sink.lock().unwrap().push(frame.identity.frame_sequence);
            }
        }));

        let mut spec = pt_spec("frameSampler", "frames");
        // 超低 fpsLimit=1 → 间隔 ~1s，连续两帧只应发射第一帧。
        spec.config = serde_json::json!({"fpsLimit": 1.0});
        let mut node = FrameSamplerNode {
            spec,
            last_emit_ns: None,
            active: false,
        };

        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);

        node.on_start(&mut rt).expect("on_start");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Ready));

        node.on_input("video", video_frame(1), &mut rt).expect("on_input");
        node.on_input("video", video_frame(2), &mut rt).expect("on_input");
        node.on_input("video", video_frame(3), &mut rt).expect("on_input");

        let seqs = relayed.lock().unwrap().clone();
        assert_eq!(seqs, vec![1]);
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));
    }

    #[test]
    fn frame_sampler_ignores_non_video_packets() {
        let mut outputs = OutputRegistry::default();
        let relayed: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |_| *sink.lock().unwrap() += 1));

        let spec = pt_spec("frameSampler", "frames");
        let mut node = FrameSamplerNode {
            spec,
            last_emit_ns: None,
            active: false,
        };
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);

        node.on_input("video", DataPacket::Json(Arc::new(serde_json::json!({}))), &mut rt)
            .expect("on_input");
        assert_eq!(*relayed.lock().unwrap(), 0);
    }

    #[test]
    fn fps_interval_is_clamped() {
        let spec = pt_spec("frameSampler", "frames");
        assert_eq!(fps_interval_ns(&spec), 1_000_000_000 / 30);

        let mut spec = pt_spec("frameSampler", "frames");
        spec.config = serde_json::json!({"fpsLimit": 0.0});
        // 0 → clamp 到 1.0，间隔 1s
        assert_eq!(fps_interval_ns(&spec), 1_000_000_000);

        let mut spec = pt_spec("frameSampler", "frames");
        spec.config = serde_json::json!({"fpsLimit": 1000.0});
        // 1000 → clamp 到 240，间隔 1e9/240
        assert_eq!(fps_interval_ns(&spec), 1_000_000_000 / 240);
    }
}
