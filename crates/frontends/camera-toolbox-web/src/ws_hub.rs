//! WebSocket 连接注册表与广播中心。
//!
//! 单一长连接协议下，后端所有面向前端的推送（状态 / 事件 / 帧）都经 [`WsHub`] 广播。
//! 每个已注册连接对应一个 `tokio::sync::mpsc::UnboundedSender<Message>`；发送由连接自身的
//! 转发任务消费并写入 socket（见 `main.rs` 的 `/api/ws` handler）。
//!
//! 连接表用 [`std::sync::Mutex`] 包裹：广播路径短且只做「尝试发送 + 惰性清理」，不涉及 await，
//! 在单线程 tokio runtime 中不会阻塞事件循环，也无需跨线程共享的 `tokio::sync::Mutex`。

use std::sync::{Mutex, atomic::{AtomicUsize, Ordering}};

use axum::extract::ws::Message;
use tokio::sync::mpsc::UnboundedSender;

/// WebSocket 广播中心。`Arc<WsHub>` 挂在 `AppState` 上，供 WS handler 与（后续的）引擎桥接任务共享。
pub struct WsHub {
    connections: Mutex<Vec<Connection>>,
    next_id: AtomicUsize,
}

/// 一个已注册连接：单调递增的连接 id + 出站通道 sender。
struct Connection {
    id: usize,
    sender: UnboundedSender<Message>,
}

impl WsHub {
    /// 创建一个空的连接注册表。
    #[must_use]
    pub fn new() -> Self {
        Self {
            connections: Mutex::new(Vec::new()),
            next_id: AtomicUsize::new(1),
        }
    }

    /// 注册一个连接：把它的出站通道 sender 加入广播列表，返回唯一连接 id。
    ///
    /// 调用方（WS handler）在握手成功后、进入接收循环前调用，并保留返回的 id 用于后续注销。
    pub fn register(&self, sender: UnboundedSender<Message>) -> usize {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut conns = self
            .connections
            .lock()
            .expect("ws_hub connection lock poisoned");
        conns.push(Connection { id, sender });
        id
    }

    /// 注销一个连接（按 `register` 返回的 id）。通常在 WS 接收循环结束 / 连接断开时调用。
    pub fn unregister(&self, id: usize) {
        let mut conns = self
            .connections
            .lock()
            .expect("ws_hub connection lock poisoned");
        conns.retain(|conn| conn.id != id);
    }

    /// 向所有存活连接广播一条文本消息。
    ///
    /// 对发送失败的连接（接收端已关闭 / 转发任务已退出）做惰性清理：直接摘除，不重试。
    pub fn broadcast_text(&self, text: impl Into<String>) {
        let text = text.into();
        let msg = Message::Text(text.into());
        self.broadcast(msg);
    }

    /// 向所有存活连接推送一帧 JPEG。
    ///
    /// 遵循计划信封规范：先推一条 `frame_meta` 文本消息（`kind:"push"`, `topic:"frame_meta"`，
    /// 含 `nodeId`/`seq`/`width`/`height`），再推一条独立二进制帧（JPEG bytes）。宽高/降采样/
    /// 编码缓存由 P2 的 `engine_bridge` 负责，此处只负责生成 meta 头并透传字节。
    #[allow(dead_code)]
    pub fn publish_frame(
        &self,
        node_id: &str,
        seq: u64,
        width: u32,
        height: u32,
        jpeg_bytes: &[u8],
    ) {
        let meta = serde_json::json!({
            "kind": "push",
            "topic": "frame_meta",
            "payload": {
                "nodeId": node_id,
                "seq": seq,
                "width": width,
                "height": height,
            },
        })
        .to_string();

        self.broadcast(Message::Text(meta.into()));
        let frame = Message::Binary(jpeg_bytes.to_vec().into());
        self.broadcast(frame);
    }

    /// 内部广播：遍历连接表，逐个尝试发送；`send` 返回 `Err` 即视为死连接并摘除。
    fn broadcast(&self, msg: Message) {
        let mut conns = self
            .connections
            .lock()
            .expect("ws_hub connection lock poisoned");

        // 先把消息克隆分发给每个存活连接；mpsc `UnboundedSender::send` 失败仅当接收端被 drop。
        let mut dead = Vec::new();
        for (idx, conn) in conns.iter().enumerate() {
            if conn.sender.send(msg.clone()).is_err() {
                dead.push(idx);
            }
        }
        // 由后向前删除，避免索引错位。
        for idx in dead.into_iter().rev() {
            conns.swap_remove(idx);
        }
    }
}

impl Default for WsHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_broadcast_text_reaches_live_connection() {
        let hub = WsHub::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let key = hub.register(tx);

        hub.broadcast_text("hello");

        let msg = rx.recv().await.expect("broadcast should deliver");
        assert_eq!(msg, Message::Text("hello".to_string().into()));

        hub.unregister(key);
        hub.broadcast_text("after-unregister");
        assert!(
            rx.try_recv().is_err(),
            "unregistered connection must not receive further messages"
        );
    }

    #[tokio::test]
    async fn dead_connection_is_lazily_removed() {
        let hub = WsHub::new();

        // 连接 1 存活；连接 2 的接收端先 drop，模拟断线。
        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let (tx2, rx2) = tokio::sync::mpsc::unbounded_channel::<Message>();
        hub.register(tx1);
        hub.register(tx2);
        drop(rx2);

        hub.broadcast_text("boom");

        // 存活连接收到，死连接被摘除。
        assert_eq!(
            rx1.recv().await.expect("live connection should receive"),
            Message::Text("boom".to_string().into())
        );

        // 再次广播只应送到连接 1，连接 2 已被清理（不 panic、不重复投递）。
        hub.broadcast_text("second");
        assert_eq!(
            rx1.recv().await.expect("live connection should receive again"),
            Message::Text("second".to_string().into())
        );
    }

    #[tokio::test]
    async fn publish_frame_sends_meta_then_binary() {
        let hub = WsHub::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        hub.register(tx);

        let jpeg = vec![0xff, 0xd8, 0xff, 0xd9];
        hub.publish_frame("node-1", 42, 960, 540, &jpeg);

        let meta = rx.recv().await.expect("frame_meta text");
        match meta {
            Message::Text(text) => {
                let v: serde_json::Value = serde_json::from_str(&text).expect("valid json");
                assert_eq!(v["kind"], "push");
                assert_eq!(v["topic"], "frame_meta");
                assert_eq!(v["payload"]["nodeId"], "node-1");
                assert_eq!(v["payload"]["seq"], 42);
                assert_eq!(v["payload"]["width"], 960);
                assert_eq!(v["payload"]["height"], 540);
            }
            other => panic!("expected text frame_meta, got {other:?}"),
        }

        let frame = rx.recv().await.expect("binary jpeg frame");
        match frame {
            Message::Binary(bytes) => assert_eq!(&*bytes, &jpeg[..]),
            other => panic!("expected binary jpeg, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn publish_frame_reaches_all_connections() {
        let hub = WsHub::new();
        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel::<Message>();
        hub.register(tx1);
        hub.register(tx2);

        hub.publish_frame("node-1", 1, 960, 540, &[1, 2, 3]);

        for rx in [&mut rx1, &mut rx2] {
            let _meta = rx.recv().await.expect("meta");
            let frame = rx.recv().await.expect("binary");
            if let Message::Binary(bytes) = frame {
                assert_eq!(&*bytes, &[1u8, 2, 3]);
            } else {
                panic!("expected binary");
            }
        }
    }
}
