//! 自动采集控制器节点：显式布防的触发透传门。
//!
//! 阈值和连续保持分别由专用节点完成；本节点只保留 UI 所需的 Arm/Disarm
//! 语义，布防时原样转发同一份 [`crate::engine::CaptureTrigger`]，不重建帧身份。

use crate::engine::{
    DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState,
    NodeSpec,
};

pub struct AutoCaptureFactory;

impl NodeFactory for AutoCaptureFactory {
    fn kind(&self) -> &'static str {
        "autoCaptureController"
    }

    fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(AutoCaptureNode { armed: false }))
    }
}

pub struct AutoCaptureNode {
    armed: bool,
}

impl NodeInstance for AutoCaptureNode {
    fn kind(&self) -> &'static str {
        "autoCaptureController"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "arm to forward capture triggers");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if port != "trigger" || !self.armed {
            return Ok(());
        }
        let DataPacket::CaptureTrigger(trigger) = &packet else {
            return Err(NodeError::Precondition(
                "autoCaptureController.trigger requires capture.trigger".to_owned(),
            ));
        };
        rt.report_event(format!(
            "armed capture trigger forwarded for frame {}",
            trigger.frame_identity.frame_sequence
        ));
        rt.emit("trigger", packet)?;
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Arm => {
                self.armed = true;
                rt.report_state(NodeRuntimeState::Running, "armed");
                Ok(())
            }
            NodeAction::Disarm => {
                self.armed = false;
                rt.report_state(NodeRuntimeState::Idle, "disarmed");
                Ok(())
            }
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.armed = false;
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};

    use super::*;
    use crate::{
        engine::{
            CaptureTrigger, ImageFrameIdentity, NodeReporter, OutputRegistry, PortCardinality,
            PortSpec, SpawnContext,
        },
        platform::{SourcePts, SourcePtsProvenance, StreamFrameIdentity, StreamSessionId},
    };

    fn spec() -> NodeSpec {
        NodeSpec {
            id: "auto-1".to_owned(),
            kind: "autoCaptureController".to_owned(),
            title: "Arm Gate".to_owned(),
            inputs: vec![PortSpec {
                id: "trigger".to_owned(),
                label: "Trigger".to_owned(),
                kind: "capture.trigger".to_owned(),
                cardinality: PortCardinality::One,
                required: false,
            }],
            outputs: vec![PortSpec {
                id: "trigger".to_owned(),
                label: "Trigger".to_owned(),
                kind: "capture.trigger".to_owned(),
                cardinality: PortCardinality::One,
                required: false,
            }],
            config: serde_json::json!({}),
        }
    }

    fn identity(sequence: u64) -> ImageFrameIdentity {
        ImageFrameIdentity::from(&StreamFrameIdentity::known_at(
            StreamSessionId::new("auto-capture-test").expect("valid stream id"),
            0,
            sequence,
            SourcePts::Known {
                ticks: sequence as i64 * 3_000,
                time_base_numerator: 1,
                time_base_denominator: 90_000,
                provenance: SourcePtsProvenance::FfmpegDecodedFrame,
            },
            sequence * 1_000,
        ))
    }

    fn trigger(sequence: u64) -> DataPacket {
        DataPacket::CaptureTrigger(Arc::new(CaptureTrigger {
            frame_identity: identity(sequence),
        }))
    }

    fn runtime(
        record: Arc<Mutex<Vec<DataPacket>>>,
    ) -> (NodeRuntime, mpsc::Receiver<crate::engine::NodeStatusReport>) {
        let (state_tx, state_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("auto-1".to_owned(), state_tx, event_tx);
        let mut outputs = OutputRegistry::default();
        outputs.set_record(Arc::new(move |packet| {
            record.lock().expect("record lock").push(packet)
        }));
        let ctx = SpawnContext {
            outputs,
            reporter,
            services: Arc::new(crate::engine::EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        (NodeRuntime::new(ctx), state_rx)
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
    fn factory_instantiates_with_expected_kind() {
        assert_eq!(AutoCaptureFactory.kind(), "autoCaptureController");
        let instance = AutoCaptureFactory.instantiate(spec()).expect("instantiate");
        assert_eq!(instance.kind(), "autoCaptureController");
    }

    #[test]
    fn on_start_reports_ready() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let (mut rt, state_rx) = runtime(record);
        let mut node = AutoCaptureFactory.instantiate(spec()).expect("instantiate");
        node.on_start(&mut rt).expect("on_start");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Ready));
    }

    #[test]
    fn trigger_is_ignored_until_armed() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let (mut rt, _state_rx) = runtime(Arc::clone(&record));
        let mut node = AutoCaptureFactory.instantiate(spec()).expect("instantiate");

        node.on_input("trigger", trigger(9), &mut rt)
            .expect("unarmed trigger is ignored");
        assert!(record.lock().expect("record lock").is_empty());
    }

    #[test]
    fn armed_trigger_is_forwarded_without_identity_change() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let (mut rt, state_rx) = runtime(Arc::clone(&record));
        let mut node = AutoCaptureFactory.instantiate(spec()).expect("instantiate");
        node.on_action(NodeAction::Arm, &mut rt).expect("arm");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));

        node.on_input("trigger", trigger(9), &mut rt)
            .expect("forward trigger");
        let output = record
            .lock()
            .expect("record lock")
            .pop()
            .expect("forwarded trigger");
        let DataPacket::CaptureTrigger(trigger) = output else {
            panic!("expected capture trigger");
        };
        assert_eq!(trigger.frame_identity, identity(9));
    }

    #[test]
    fn armed_gate_rejects_non_trigger_packet() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let (mut rt, _state_rx) = runtime(record);
        let mut node = AutoCaptureFactory.instantiate(spec()).expect("instantiate");
        node.on_action(NodeAction::Arm, &mut rt).expect("arm");

        assert!(matches!(
            node.on_input(
                "trigger",
                DataPacket::Json(Arc::new(serde_json::json!({}))),
                &mut rt,
            ),
            Err(NodeError::Precondition(_))
        ));
    }

    #[test]
    fn disarm_and_stop_report_idle() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let (mut rt, state_rx) = runtime(record);
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
        let record = Arc::new(Mutex::new(Vec::new()));
        let (mut rt, _state_rx) = runtime(record);
        let mut node = AutoCaptureFactory.instantiate(spec()).expect("instantiate");
        let err = node
            .on_action(NodeAction::Trigger, &mut rt)
            .expect_err("unsupported");
        assert!(matches!(err, NodeError::UnsupportedAction(_)));
    }
}
