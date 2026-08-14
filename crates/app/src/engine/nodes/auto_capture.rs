//! 自动采集控制器节点：条件自动触发的节点（范式样板）。
//!
//! `Arm` 后进入待命状态；收到评分输入且超过阈值时自动输出抓帧命令。
//! 这是「arm/disarm + 条件自动触发」的完整样板。

use std::sync::Arc;

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
        }))
    }
}

pub struct AutoCaptureNode {
    spec: NodeSpec,
    armed: bool,
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
        let DataPacket::Json(score) = packet else {
            return Ok(());
        };
        let gain = score.get("gain").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
        let threshold = config_f64(&self.spec, "triggerThreshold", 0.5);
        if gain >= threshold {
            rt.emit(
                "command",
                DataPacket::Json(Arc::new(json!({
                    "action": "capture",
                    "reason": "score-gain",
                    "gain": gain,
                }))),
            )?;
            rt.report_event(format!("auto-capture triggered (gain {gain:.3} ≥ {threshold})"));
        }
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

fn config_f64(spec: &NodeSpec, key: &str, fallback: f64) -> f64 {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(fallback)
}
