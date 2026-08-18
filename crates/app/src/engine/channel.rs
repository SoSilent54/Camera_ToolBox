//! 节点 mailbox：节点间数据流 + 引擎控制命令的统一通道。
//!
//! 每个节点 actor 只有一个 mailbox；上游输出与控制命令都投递到这里。
//! 帧流在 mailbox 满时丢弃「新到」的帧（`try_send` 失败即丢新），控制命令不占帧容量，避免被高频帧流堵住。

use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
    mpsc,
};

use super::{node::NodeAction, packet::DataPacket, spec::PortId};

/// 引擎或上游节点投递给节点 actor 的消息。
#[derive(Debug)]
pub enum NodeMessage {
    /// 上游数据到达某个输入端口。
    Input { port: PortId, packet: DataPacket },
    /// 控制动作（连接/断开/触发/arm/disarm）。
    Action(NodeAction),
    /// 在 actor 线程安全应用配置；sender 用于同步返回钩子结果。
    ConfigUpdate {
        config: serde_json::Value,
        result: mpsc::SyncSender<Result<(), super::node::NodeError>>,
    },
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
    tx: mpsc::Sender<NodeMessage>,
    id: u64,
    pending_inputs: Arc<AtomicUsize>,
    input_capacity: usize,
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
    pending_inputs: Arc<AtomicUsize>,
}

impl MailboxSender {
    /// 非阻塞投递帧输入；队列满时丢弃新到输入。控制命令走无界队列，避免被帧流堵住。
    pub fn try_send(&self, message: NodeMessage) -> Result<(), ChannelFull> {
        if matches!(message, NodeMessage::Input { .. }) {
            self.reserve_input_slot()?;
            if self.tx.send(message).is_err() {
                self.pending_inputs.fetch_sub(1, Ordering::Release);
                return Err(ChannelFull);
            }
            return Ok(());
        }
        self.tx.send(message).map_err(|_| ChannelFull)
    }

    /// 投递控制命令；不与帧输入共用容量，保证 stop/action 不被高频帧流阻塞。
    pub fn send(&self, message: NodeMessage) -> Result<(), ChannelFull> {
        self.tx.send(message).map_err(|_| ChannelFull)
    }

    fn reserve_input_slot(&self) -> Result<(), ChannelFull> {
        let mut current = self.pending_inputs.load(Ordering::Acquire);
        loop {
            if current >= self.input_capacity {
                return Err(ChannelFull);
            }
            match self.pending_inputs.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(next) => current = next,
            }
        }
    }
}

impl MailboxReceiver {
    /// 阻塞等待下一条消息。
    pub fn recv(&self) -> Result<NodeMessage, mpsc::RecvError> {
        let message = self.rx.recv()?;
        self.release_if_input(&message);
        Ok(message)
    }

    /// 非阻塞取一条消息。
    pub fn try_recv(&self) -> Result<NodeMessage, mpsc::TryRecvError> {
        let message = self.rx.try_recv()?;
        self.release_if_input(&message);
        Ok(message)
    }

    fn release_if_input(&self, message: &NodeMessage) {
        if matches!(message, NodeMessage::Input { .. }) {
            self.pending_inputs.fetch_sub(1, Ordering::Release);
        }
    }
}

/// 创建 mailbox。`capacity` 为队列容量；帧流用较小值（如 4）配合消费端 drain 丢旧保新。
#[must_use]
pub fn create_mailbox(capacity: usize) -> (MailboxSender, MailboxReceiver) {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let input_capacity = capacity.clamp(1, 256);
    let (tx, rx) = mpsc::channel();
    let pending_inputs = Arc::new(AtomicUsize::new(0));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    (
        MailboxSender {
            tx,
            id,
            pending_inputs: Arc::clone(&pending_inputs),
            input_capacity,
        },
        MailboxReceiver { rx, pending_inputs },
    )
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
        assert!(matches!(rx.recv(), Ok(NodeMessage::Input { port, .. }) if port == "a"));
    }

    #[test]
    fn mailbox_control_send_delivers_when_input_capacity_is_full() {
        let (tx, rx) = create_mailbox(1);
        tx.try_send(input("a", "data")).unwrap();
        assert_eq!(tx.try_send(input("a", "dropped")), Err(ChannelFull));
        tx.send(NodeMessage::Stop).unwrap();
        assert!(matches!(rx.recv(), Ok(NodeMessage::Input { .. })));
        assert!(matches!(rx.recv(), Ok(NodeMessage::Stop)));
    }
}
