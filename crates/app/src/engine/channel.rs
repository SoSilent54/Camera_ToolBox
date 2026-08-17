//! 节点 mailbox：节点间数据流 + 引擎控制命令的统一通道。
//!
//! 每个节点 actor 只有一个 mailbox；上游输出与控制命令都投递到这里。
//! 帧流在 mailbox 满时丢弃「新到」的帧（`try_send` 失败即丢新），控制命令用阻塞发送保证不丢。

use std::sync::{atomic::{AtomicU64, Ordering}, mpsc};

use super::{node::NodeAction, packet::DataPacket, spec::PortId};

/// 引擎或上游节点投递给节点 actor 的消息。
#[derive(Debug)]
pub enum NodeMessage {
    /// 上游数据到达某个输入端口。
    Input { port: PortId, packet: DataPacket },
    /// 控制动作（连接/断开/触发/arm/disarm）。
    Action(NodeAction),
    /// 停止信号。
    Stop,
}

/// 发送失败/通道满。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelFull;

/// mailbox 发送端（可克隆，支持 fan-out）。
///
/// `id` 是进程内唯一通道标识，在 `create_mailbox` 时分配；克隆共享同一 `id`。
/// `PartialEq`/`Eq` 按 `id` 比较：同一下游 mailbox 的克隆彼此相等，用于
/// `OutputRegistry::disconnect` 按身份摘除，而非按值比较消息内容。
#[derive(Clone)]
pub struct MailboxSender {
    tx: mpsc::SyncSender<NodeMessage>,
    id: u64,
}

impl PartialEq for MailboxSender {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for MailboxSender {}

/// mailbox 接收端。
pub struct MailboxReceiver {
    rx: mpsc::Receiver<NodeMessage>,
}

impl MailboxSender {
    /// 非阻塞投递；通道满时返回 `ChannelFull`（帧流据此丢帧）。
    pub fn try_send(&self, message: NodeMessage) -> Result<(), ChannelFull> {
        self.tx.try_send(message).map_err(|_| ChannelFull)
    }

    /// 阻塞投递；用于控制命令（不丢）。
    pub fn send(&self, message: NodeMessage) -> Result<(), ChannelFull> {
        self.tx.send(message).map_err(|_| ChannelFull)
    }
}

impl MailboxReceiver {
    /// 阻塞等待下一条消息。
    pub fn recv(&self) -> Result<NodeMessage, mpsc::RecvError> {
        self.rx.recv()
    }

    /// 非阻塞取一条消息。
    pub fn try_recv(&self) -> Result<NodeMessage, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
}

/// 创建 mailbox。`capacity` 为队列容量；帧流用较小值（如 4）配合消费端 drain 丢旧保新。
#[must_use]
pub fn create_mailbox(capacity: usize) -> (MailboxSender, MailboxReceiver) {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let capacity = capacity.clamp(1, 256);
    let (tx, rx) = mpsc::sync_channel(capacity);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    (MailboxSender { tx, id }, MailboxReceiver { rx })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn json(value: &str) -> DataPacket {
        DataPacket::Json(Arc::new(serde_json::Value::String(value.to_owned())))
    }

    fn input(port: &str, value: &str) -> NodeMessage {
        NodeMessage::Input {
            port: port.to_owned(),
            packet: json(value),
        }
    }

    #[test]
    fn mailbox_drops_new_when_full() {
        let (tx, rx) = create_mailbox(1);
        assert!(tx.try_send(input("a", "first")).is_ok());
        assert_eq!(tx.try_send(input("a", "second")), Err(ChannelFull));
        assert!(
            matches!(rx.recv(), Ok(NodeMessage::Input { port, .. }) if port == "a")
        );
    }

    #[test]
    fn mailbox_control_send_delivers_after_input() {
        let (tx, rx) = create_mailbox(1);
        tx.try_send(input("a", "data")).unwrap();
        assert!(matches!(rx.try_recv(), Ok(NodeMessage::Input { .. })));
        tx.send(NodeMessage::Stop).unwrap();
        assert!(matches!(rx.try_recv(), Ok(NodeMessage::Stop)));
    }
}
