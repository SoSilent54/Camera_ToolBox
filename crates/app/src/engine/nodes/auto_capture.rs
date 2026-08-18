//! 自动采集控制器节点：条件自动触发的节点（范式样板）。
//!
//! `Arm` 后进入待命状态；收到评分输入且超过阈值时自动输出抓帧命令。
//! 这是「arm/disarm + 条件自动触发」的完整样板。

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::json;

use crate::engine::{
    DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState,
    NodeSpec,
};

pub struct AutoCaptureFactory;

impl NodeFactory for AutoCaptureFactory {
    fn kind(&self) -> &'static str {
        "autoCaptureController"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(AutoCaptureNode {
            spec,
            armed: false,
            last_capture_at: None,
        }))
    }
}

pub struct AutoCaptureNode {
    spec: NodeSpec,
    armed: bool,
    last_capture_at: Option<Instant>,
}

impl NodeInstance for AutoCaptureNode {
    fn kind(&self) -> &'static str {
        "autoCaptureController"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "arm to enable auto-capture");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if port != "score" || !self.armed {
            return Ok(());
        }
        let DataPacket::Score(score) = packet else {
            return Err(NodeError::Precondition(
                "autoCaptureController.score requires capture.score".to_owned(),
            ));
        };
        let gain = score.gain;
        let threshold = config_f64(&self.spec, "triggerThreshold", 0.5);
        if gain < threshold {
            return Ok(());
        }
        let cooldown = Duration::from_millis(config_u64(&self.spec, "cooldownMs", 800));
        if self
            .last_capture_at
            .is_some_and(|last| last.elapsed() < cooldown)
        {
            return Ok(());
        }
        rt.emit(
            "command",
            DataPacket::Json(Arc::new(json!({
                "action": "capture",
                "reason": "score-gain",
                "gain": gain,
            }))),
        )?;
        self.last_capture_at = Some(Instant::now());
        rt.report_event(format!(
            "auto-capture triggered (gain {gain:.3} ≥ {threshold})"
        ));
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Arm => {
                self.armed = true;
                self.last_capture_at = None;
                rt.report_state(NodeRuntimeState::Running, "armed");
                Ok(())
            }
            NodeAction::Disarm => {
                self.armed = false;
                self.last_capture_at = None;
                rt.report_state(NodeRuntimeState::Idle, "disarmed");
                Ok(())
            }
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.armed = false;
        self.last_capture_at = None;
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

fn config_f64(spec: &NodeSpec, key: &str, fallback: f64) -> f64 {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(fallback)
}

fn config_u64(spec: &NodeSpec, key: &str, fallback: u64) -> u64 {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};

    use super::*;
    use crate::{
        engine::{
            GainScore, ImageFrameIdentity, NodeReporter, OutputRegistry, PortCardinality, PortSpec,
            SpawnContext,
        },
        platform::{StreamFrameIdentity, StreamSessionId},
    };
    fn spec() -> NodeSpec {
        NodeSpec {
            id: "auto-1".to_owned(),
            kind: "autoCaptureController".to_owned(),
            title: "Auto Capture".to_owned(),
            inputs: vec![PortSpec {
                id: "score".to_owned(),
                label: "Score".to_owned(),
                kind: "capture.score".to_owned(),
                cardinality: PortCardinality::One,
                required: false,
            }],
            outputs: vec![PortSpec {
                id: "command".to_owned(),
                label: "Command".to_owned(),
                kind: "capture.command".to_owned(),
                cardinality: PortCardinality::One,
                required: false,
            }],
            config: serde_json::json!({"triggerThreshold": 0.5}),
        }
    }

    fn runtime(
        state_tx: mpsc::Sender<crate::engine::NodeStatusReport>,
    ) -> (NodeRuntime, OutputRegistry) {
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("auto-1".to_owned(), state_tx, event_tx);
        let outputs = OutputRegistry::default();
        let ctx = SpawnContext {
            outputs: outputs.clone(),
            reporter,
            services: Arc::new(crate::engine::EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        (NodeRuntime::new(ctx), outputs)
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

    fn score(gain: f64) -> DataPacket {
        let stream_identity = StreamFrameIdentity::unavailable(
            StreamSessionId::new("auto-capture-test").expect("valid session id"),
            0,
            0,
            "test identity".to_owned(),
        );
        DataPacket::Score(Arc::new(GainScore {
            gain,
            frame_identity: ImageFrameIdentity::from(&stream_identity),
        }))
    }

    #[test]
    fn factory_instantiates_with_expected_kind() {
        assert_eq!(AutoCaptureFactory.kind(), "autoCaptureController");
        let instance = AutoCaptureFactory.instantiate(spec()).expect("instantiate");
        assert_eq!(instance.kind(), "autoCaptureController");
    }

    #[test]
    fn on_start_reports_ready() {
        let (state_tx, state_rx) = mpsc::channel();
        let (mut rt, _outputs) = runtime(state_tx);
        let mut node = AutoCaptureFactory.instantiate(spec()).expect("instantiate");
        node.on_start(&mut rt).expect("on_start");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Ready));
    }

    #[test]
    fn score_ignored_until_armed() {
        let (state_tx, _state_rx) = mpsc::channel();
        let mut outputs = OutputRegistry::default();
        let emitted: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let sink = Arc::clone(&emitted);
        outputs.set_record(Arc::new(move |_| *sink.lock().unwrap() += 1));
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("auto-1".to_owned(), state_tx, event_tx);
        let ctx = SpawnContext {
            outputs: outputs.clone(),
            reporter,
            services: Arc::new(crate::engine::EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        let mut rt = NodeRuntime::new(ctx);

        let mut node = AutoCaptureFactory.instantiate(spec()).expect("instantiate");
        // 未 arm，即使 gain 超标也不触发
        node.on_input("score", score(0.9), &mut rt)
            .expect("on_input");
        assert_eq!(*emitted.lock().unwrap(), 0);
    }

    #[test]
    fn arm_then_score_above_threshold_emits_command() {
        let (state_tx, state_rx) = mpsc::channel();
        let mut outputs = OutputRegistry::default();
        let commands: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&commands);
        outputs.set_record(Arc::new(move |packet| {
            if let DataPacket::Json(value) = packet {
                sink.lock().unwrap().push((*value).clone());
            }
        }));
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("auto-1".to_owned(), state_tx, event_tx);
        let ctx = SpawnContext {
            outputs: outputs.clone(),
            reporter,
            services: Arc::new(crate::engine::EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        let mut rt = NodeRuntime::new(ctx);

        let mut node = AutoCaptureFactory.instantiate(spec()).expect("instantiate");
        node.on_action(NodeAction::Arm, &mut rt).expect("arm");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));

        node.on_input("score", score(0.8), &mut rt)
            .expect("on_input");
        let commands = commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["action"], "capture");
        assert_eq!(commands[0]["gain"], 0.8);
    }

    #[test]
    fn score_below_threshold_does_not_emit() {
        let (state_tx, _state_rx) = mpsc::channel();
        let mut outputs = OutputRegistry::default();
        let emitted: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let sink = Arc::clone(&emitted);
        outputs.set_record(Arc::new(move |_| *sink.lock().unwrap() += 1));
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("auto-1".to_owned(), state_tx, event_tx);
        let ctx = SpawnContext {
            outputs: outputs.clone(),
            reporter,
            services: Arc::new(crate::engine::EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        let mut rt = NodeRuntime::new(ctx);

        let mut node = AutoCaptureFactory.instantiate(spec()).expect("instantiate");
        node.on_action(NodeAction::Arm, &mut rt).expect("arm");
        node.on_input("score", score(0.1), &mut rt)
            .expect("on_input");
        assert_eq!(*emitted.lock().unwrap(), 0);
    }

    #[test]
    fn disarm_and_stop_report_idle() {
        let (state_tx, state_rx) = mpsc::channel();
        let (mut rt, _outputs) = runtime(state_tx);
        let mut node = AutoCaptureFactory.instantiate(spec()).expect("instantiate");

        node.on_action(NodeAction::Arm, &mut rt).expect("arm");
        node.on_action(NodeAction::Disarm, &mut rt).expect("disarm");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Idle));

        node.on_action(NodeAction::Arm, &mut rt).expect("arm");
        node.on_stop(&mut rt).expect("on_stop");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Idle));
    }

    #[test]
    fn unsupported_action_is_error() {
        let (state_tx, _state_rx) = mpsc::channel();
        let (mut rt, _outputs) = runtime(state_tx);
        let mut node = AutoCaptureFactory.instantiate(spec()).expect("instantiate");
        let err = node
            .on_action(NodeAction::Trigger, &mut rt)
            .expect_err("unsupported");
        assert!(matches!(err, NodeError::UnsupportedAction(_)));
    }
}
