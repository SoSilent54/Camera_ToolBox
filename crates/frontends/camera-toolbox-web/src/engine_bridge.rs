//! 引擎桥接：把运行中引擎的状态/事件/帧从同步 actor 世界搬到 WebSocket 推送边界。
//!
//! 三个独立循环（`tokio::spawn`）均在单线程 tokio runtime 内协作，每个循环：
//! 短暂持锁取数据（`drain_status`/`drain_events`/`viewer_frame` 均为非阻塞）、立即释放，
//! 再 `tokio::time::sleep` 让出事件循环，避免饿死其它任务或死锁（从不跨 `.await` 持有引擎锁）。
//!
//! - `drain_status` → `ws_hub.broadcast_text(status push)`
//! - `drain_events` → `ws_hub.broadcast_text(event push)`
//! - viewer 帧：按 `frame_sequence` 去重（同帧不重复编码），`viewer_encode_width=960` 降采样
//!   编码 JPEG → `ws_hub.publish_frame`（先 frame_meta 文本再 Binary）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::ws_hub::WsHub;
use crate::AppState;

/// 状态/事件循环的轮询间隔（非阻塞 `try_recv`，间隔仅决定延迟上限）。
const STATUS_POLL_MS: u64 = 50;
const EVENT_POLL_MS: u64 = 50;
/// 帧循环的轮询间隔：约 60fps 的离散采样窗口；实际帧率由上游 pummp 事件驱动决定。
const FRAME_POLL_MS: u64 = 16;

/// viewer 帧 JPEG 编码目标宽度（等比缩放；见计划参数表 `viewer_encode_width`）。
const VIEWER_ENCODE_WIDTH: u32 = 960;

/// 启动桥接任务。`AppState` 与 `WsHub` 均为廉价 `Clone`（内部 `Arc`），传入的克隆被任务持走。
pub fn spawn(state: AppState, hub: Arc<WsHub>) {
    let status_state = state.clone();
    let status_hub = Arc::clone(&hub);
    tokio::spawn(async move {
        drain_status_loop(status_state, status_hub).await;
    });

    let event_state = state.clone();
    let event_hub = Arc::clone(&hub);
    tokio::spawn(async move {
        drain_events_loop(event_state, event_hub).await;
    });

    tokio::spawn(async move {
        frame_loop(state, hub).await;
    });
}

/// 状态循环：把引擎上报的 `NodeStatusReport` 逐条转成 `status` push 广播。
async fn drain_status_loop(state: AppState, hub: Arc<WsHub>) {
    loop {
        {
            // 引擎锁（`MutexGuard` 非 `Send`）必须在此块内释放，不能跨 `.await`。
            let engine = state.engine_runtime.engine();
            if let Some(engine) = engine.as_ref() {
                for status in engine.drain_status() {
                    let push = json!({
                        "kind": "push",
                        "topic": "status",
                        "payload": status,
                    });
                    hub.broadcast_text(push.to_string());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(STATUS_POLL_MS)).await;
    }
}

/// 事件循环：把引擎上报的 `NodeEvent` 逐条转成 `event` push 广播。
async fn drain_events_loop(state: AppState, hub: Arc<WsHub>) {
    loop {
        {
            let engine = state.engine_runtime.engine();
            if let Some(engine) = engine.as_ref() {
                for event in engine.drain_events() {
                    let push = json!({
                        "kind": "push",
                        "topic": "event",
                        "payload": event,
                    });
                    hub.broadcast_text(push.to_string());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(EVENT_POLL_MS)).await;
    }
}

/// 帧循环：对每个 viewer 节点做 latest-wins 帧推送。
///
/// 按 `frame_sequence` 去重：只有 seq 变化才重新编码并推送（同帧不重复编码）。
async fn frame_loop(state: AppState, hub: Arc<WsHub>) {
    // viewer node_id → 最近一次已推送的 seq；新增/删除节点会随每次枚举同步收敛。
    let mut last_seen: HashMap<String, u64> = HashMap::new();

    loop {
        {
            let engine = state.engine_runtime.engine();
            if let Some(engine) = engine.as_ref() {
                let viewer_ids = engine.viewer_node_ids();
                for node_id in &viewer_ids {
                    let Some(frame) = engine.viewer_frame(node_id) else {
                        continue;
                    };
                    let seq = frame.identity.frame_sequence;
                    if last_seen.get(node_id) == Some(&seq) {
                        continue; // 同帧，不重复编码/推送。
                    }
                    last_seen.insert(node_id.clone(), seq);
                    match crate::encode_rgba_scaled_jpeg(&frame, VIEWER_ENCODE_WIDTH) {
                        Ok(encoded) => {
                            hub.publish_frame(node_id, seq, encoded.width, encoded.height, &encoded.jpeg);
                        }
                        Err(error) => {
                            tracing::warn!(node_id = %node_id, seq, "viewer frame encode failed: {error}");
                        }
                    }
                }
                // 收敛缓存大小：旧 viewer 节点移除后清理对应 seq 记录。
                prune_last_seen(&mut last_seen, &viewer_ids);
            } else {
                last_seen.clear();
            }
        }
        tokio::time::sleep(Duration::from_millis(FRAME_POLL_MS)).await;
    }
}

/// 清理已不在图中的 viewer 节点的 seq 记录，避免 hash map 无界增长。
fn prune_last_seen(last_seen: &mut HashMap<String, u64>, active: &[String]) {
    let active_set: std::collections::HashSet<&str> = active.iter().map(String::as_str).collect();
    last_seen.retain(|node_id, _| active_set.contains(node_id.as_str()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_removes_inactive_viewers() {
        let mut seen = HashMap::from([
            ("v1".to_owned(), 1u64),
            ("v2".to_owned(), 2u64),
        ]);
        let active = vec!["v1".to_owned()];
        prune_last_seen(&mut seen, &active);
        assert!(seen.contains_key("v1"));
        assert!(!seen.contains_key("v2"));
    }
}
