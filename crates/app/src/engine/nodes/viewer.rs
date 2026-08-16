//! Viewer 节点：终端节点，把收到的视频帧发布到引擎预分配的帧出口。
//!
//! 用「最后收帧时间戳 + 活性检测」实现数据流状态统一：上游断开后超时回落 idle。

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use crate::{
    engine::{DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec},
    platform::host_monotonic_time_ns,
};

/// 帧超时阈值：超过此时间无新帧视为上游已停止。
const FRAME_STALL_TIMEOUT_NS: u64 = 1_000_000_000;

pub struct ViewerFactory;

impl NodeFactory for ViewerFactory {
    fn kind(&self) -> &'static str {
        crate::engine::node::kinds::VIEWER
    }

    fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(ViewerNode {
            last_frame_at: Arc::new(AtomicU64::new(0)),
        }))
    }
}

pub struct ViewerNode {
    /// 最近一次收帧的进程单调时间戳；0 表示「未收到帧」或「已回落 idle」。
    last_frame_at: Arc<AtomicU64>,
}

impl NodeInstance for ViewerNode {
    fn kind(&self) -> &'static str {
        crate::engine::node::kinds::VIEWER
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for frames");
        let last_frame_at = Arc::clone(&self.last_frame_at);
        let reporter = rt.context().reporter.clone();
        let cancel = Arc::clone(&rt.context().cancel);
        rt.spawn("viewer-liveness", move |_ctx| {
            liveness_loop(last_frame_at, reporter, cancel);
        });
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
        let now = host_monotonic_time_ns();
        // 从 0（未收帧/已回落）变为非 0 时上报 running。
        if self.last_frame_at.swap(now, Ordering::Relaxed) == 0 {
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

/// 活性检测：超过阈值无新帧时回落到 idle（抑制重复上报）。
fn liveness_loop(
    last_frame_at: Arc<AtomicU64>,
    reporter: crate::engine::NodeReporter,
    cancel: Arc<AtomicBool>,
) {
    while !cancel.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(500));
        let last = last_frame_at.load(Ordering::Relaxed);
        if last == 0 {
            continue;
        }
        let now = host_monotonic_time_ns();
        if now.saturating_sub(last) > FRAME_STALL_TIMEOUT_NS
            && last_frame_at.swap(0, Ordering::Relaxed) != 0
        {
            reporter.report_state(NodeRuntimeState::Idle, "no frames (upstream stopped)");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicBool, mpsc};

    use super::*;
    use crate::engine::{NodeReporter, OutputRegistry, SpawnContext};
    use crate::platform::{DecodedVideoFrame, LatestDecodedFrameSlot, StreamFrameIdentity, StreamSessionId};

    fn video_frame() -> DataPacket {
        let session = StreamSessionId::new("viewer-test").expect("session id");
        DataPacket::VideoFrame(Arc::new(DecodedVideoFrame {
            width: 1,
            height: 1,
            rgba: Arc::from(vec![0u8; 4]),
            identity: StreamFrameIdentity::unavailable(session, 0, 1, "test"),
        }))
    }

    fn runtime_with_slot(slot: Arc<LatestDecodedFrameSlot>, state_tx: mpsc::Sender<crate::engine::NodeStatusReport>) -> NodeRuntime {
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("viewer-1".to_owned(), state_tx, event_tx);
        let ctx = SpawnContext {
            outputs: OutputRegistry::default(),
            reporter,
            services: Arc::new(crate::engine::EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: Some(slot),
        };
        NodeRuntime::new(ctx)
    }

    fn last_state(rx: &mpsc::Receiver<crate::engine::NodeStatusReport>) -> Option<NodeRuntimeState> {
        let mut last = None;
        while let Ok(report) = rx.try_recv() {
            last = Some(report.state);
        }
        last
    }

    #[test]
    fn factory_instantiates_with_expected_kind() {
        assert_eq!(ViewerFactory.kind(), "viewer");
        let spec = crate::engine::NodeSpec {
            id: "viewer-1".to_owned(),
            kind: "viewer".to_owned(),
            title: "Viewer".to_owned(),
            inputs: vec![],
            outputs: vec![],
            config: serde_json::json!({}),
        };
        let instance = ViewerFactory.instantiate(spec).expect("instantiate");
        assert_eq!(instance.kind(), "viewer");
    }

    #[test]
    fn on_start_reports_ready() {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime_with_slot(slot, state_tx);
        let mut node = ViewerFactory.instantiate(crate::engine::NodeSpec {
            id: "viewer-1".to_owned(),
            kind: "viewer".to_owned(),
            title: "Viewer".to_owned(),
            inputs: vec![],
            outputs: vec![],
            config: serde_json::json!({}),
        }).expect("instantiate");

        node.on_start(&mut rt).expect("on_start");
        rt.stop_background(); // 关闭 liveness 线程，避免测试泄漏
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Ready));
    }

    #[test]
    fn on_input_publishes_to_slot_and_reports_running_once() {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime_with_slot(Arc::clone(&slot), state_tx);
        let mut node = ViewerFactory.instantiate(crate::engine::NodeSpec {
            id: "viewer-1".to_owned(),
            kind: "viewer".to_owned(),
            title: "Viewer".to_owned(),
            inputs: vec![],
            outputs: vec![],
            config: serde_json::json!({}),
        }).expect("instantiate");

        node.on_input("video", video_frame(), &mut rt).expect("on_input");
        assert!(slot.latest().is_some(), "frame should be published to viewer slot");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));

        // 第二帧不重复上报 running（状态仍是 running，但 try_recv 之后应无新报告）
        node.on_input("video", video_frame(), &mut rt).expect("on_input");
        let mut extra = None;
        while let Ok(report) = state_rx.try_recv() {
            extra = Some(report.state);
        }
        assert_eq!(extra, None, "second frame must not emit a fresh running report");
    }

    #[test]
    fn on_input_ignores_non_video_packets() {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime_with_slot(Arc::clone(&slot), state_tx);
        let mut node = ViewerFactory.instantiate(crate::engine::NodeSpec {
            id: "viewer-1".to_owned(),
            kind: "viewer".to_owned(),
            title: "Viewer".to_owned(),
            inputs: vec![],
            outputs: vec![],
            config: serde_json::json!({}),
        }).expect("instantiate");

        node.on_input("video", DataPacket::Json(Arc::new(serde_json::json!({}))), &mut rt)
            .expect("on_input");
        assert!(slot.latest().is_none(), "non-video packet must not publish");
        assert_eq!(last_state(&state_rx), None);
    }

    #[test]
    fn on_stop_reports_idle_and_action_is_unsupported() {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime_with_slot(slot, state_tx);
        let mut node = ViewerFactory.instantiate(crate::engine::NodeSpec {
            id: "viewer-1".to_owned(),
            kind: "viewer".to_owned(),
            title: "Viewer".to_owned(),
            inputs: vec![],
            outputs: vec![],
            config: serde_json::json!({}),
        }).expect("instantiate");

        node.on_stop(&mut rt).expect("on_stop");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Idle));

        let err = node.on_action(NodeAction::Trigger, &mut rt).expect_err("unsupported");
        assert!(matches!(err, NodeError::UnsupportedAction(_)));
    }
}
