//! Viewer 节点：终端节点，把收到的视频帧发布到引擎预分配的帧出口。

use std::sync::Arc;

use crate::{
    engine::{DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec},
};

pub struct ViewerFactory;

impl NodeFactory for ViewerFactory {
    fn kind(&self) -> &'static str {
        crate::engine::node::kinds::VIEWER
    }

    fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(ViewerNode { received: false }))
    }
}

pub struct ViewerNode {
    received: bool,
}

impl NodeInstance for ViewerNode {
    fn kind(&self) -> &'static str {
        crate::engine::node::kinds::VIEWER
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
        let DataPacket::VideoFrame(frame) = packet else {
            return Ok(());
        };
        if let Some(slot) = rt.context().viewer_slot.as_ref() {
            // 帧数据零拷贝转移进槽位；仅当唯一引用时直接取出，否则克隆。
            slot.publish(Arc::unwrap_or_clone(frame));
        }
        if !self.received {
            self.received = true;
            rt.report_state(NodeRuntimeState::Running, "receiving frames");
        }
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}
