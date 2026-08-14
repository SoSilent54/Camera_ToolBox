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
