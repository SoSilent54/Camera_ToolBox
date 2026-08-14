//! 节点运行时上下文：节点用它产出数据、上报状态/事件、派生后台任务。

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

use crate::platform::LatestDecodedFrameSlot;

use super::{
    channel::{ChannelFull, MailboxSender, NodeMessage},
    packet::DataPacket,
    services::EngineServices,
    spec::{NodeEvent, NodeId, NodeRuntimeState, NodeStatusReport, PortId},
};

/// 输出端口注册表：`port_id -> 下游 mailbox 列表`（fan-out）。
///
/// `record` 是一个可选回调，在 `emit` 成功（有下游投递或无下游 no-op）时被调用，
/// 用于把「节点最近输出」回灌到引擎的 `latest_outputs` 缓存（中间结果查看）。
#[derive(Clone)]
pub struct OutputRegistry {
    ports: Arc<HashMap<PortId, Vec<MailboxSender>>>,
    record: Arc<dyn Fn(DataPacket) + Send + Sync>,
}

impl Default for OutputRegistry {
    fn default() -> Self {
        Self {
            ports: Arc::new(HashMap::new()),
            record: Arc::new(|_packet| {}),
        }
    }
}

impl OutputRegistry {
    /// 把一个下游 mailbox 挂到输出端口上（fan-out 追加）。
    pub fn connect(&mut self, port: PortId, sender: MailboxSender) {
        Arc::make_mut(&mut self.ports)
            .entry(port)
            .or_default()
            .push(sender);
    }

    /// 设置输出的最近负载记录回调（引擎在 build 时注入，把 packet 写进 `latest_outputs`）。
    pub fn set_record(&mut self, record: Arc<dyn Fn(DataPacket) + Send + Sync>) {
        self.record = record;
    }

    #[must_use]
    pub fn senders(&self, port: &str) -> Option<&[MailboxSender]> {
        self.ports.get(port).map(Vec::as_slice)
    }

    /// 向输出端口发布数据；无下游时视为 no-op 成功（未连接输出是合法状态，不算错误），
    /// 仅当下游存在但全部通道满时返回 `ChannelFull`。
    pub fn emit(&self, port: &str, packet: DataPacket) -> Result<(), ChannelFull> {
        let Some(senders) = self.ports.get(port) else {
            (self.record)(packet);
            return Ok(());
        };
        let mut delivered = false;
        for sender in senders {
            if sender
                .try_send(NodeMessage::Input {
                    port: port.to_owned(),
                    packet: packet.clone(),
                })
                .is_ok()
            {
                delivered = true;
            }
        }
        if delivered {
            (self.record)(packet);
            Ok(())
        } else {
            Err(ChannelFull)
        }
    }
}

/// 节点状态/事件上报器。引擎持有接收端，节点持有克隆的发送端。
#[derive(Clone)]
pub struct NodeReporter {
    node_id: NodeId,
    status_tx: mpsc::Sender<NodeStatusReport>,
    event_tx: mpsc::Sender<NodeEvent>,
}

impl NodeReporter {
    #[must_use]
    pub fn new(
        node_id: NodeId,
        status_tx: mpsc::Sender<NodeStatusReport>,
        event_tx: mpsc::Sender<NodeEvent>,
    ) -> Self {
        Self {
            node_id,
            status_tx,
            event_tx,
        }
    }

    pub fn report_state(&self, state: NodeRuntimeState, diagnostic: impl Into<String>) {
        let _ = self.status_tx.send(NodeStatusReport {
            node_id: self.node_id.clone(),
            state,
            diagnostic: diagnostic.into(),
        });
    }

    pub fn report_event(&self, message: impl Into<String>) {
        let _ = self.event_tx.send(NodeEvent {
            node_id: self.node_id.clone(),
            message: message.into(),
        });
    }
}

/// 后台任务上下文：节点 spawn 的任务用它独立产出数据/上报，不借用 `NodeRuntime`。
#[derive(Clone)]
pub struct SpawnContext {
    pub outputs: OutputRegistry,
    pub reporter: NodeReporter,
    pub services: Arc<EngineServices>,
    pub cancel: Arc<AtomicBool>,
    /// viewer 节点的帧出口；非 viewer 节点为 `None`。
    pub viewer_slot: Option<Arc<LatestDecodedFrameSlot>>,
}

/// 节点运行时上下文。引擎在 actor 线程里以 `&mut self` 传给节点方法。
pub struct NodeRuntime {
    ctx: SpawnContext,
    handles: Vec<JoinHandle<()>>,
}

impl NodeRuntime {
    #[must_use]
    pub fn new(ctx: SpawnContext) -> Self {
        Self {
            ctx,
            handles: Vec::new(),
        }
    }

    #[must_use]
    pub fn context(&self) -> &SpawnContext {
        &self.ctx
    }

    /// 向输出端口发布数据。
    pub fn emit(&self, port: &str, packet: DataPacket) -> Result<(), ChannelFull> {
        self.ctx.outputs.emit(port, packet)
    }

    pub fn report_state(&self, state: NodeRuntimeState, diagnostic: impl Into<String>) {
        self.ctx.reporter.report_state(state, diagnostic);
    }

    pub fn report_event(&self, message: impl Into<String>) {
        self.ctx.reporter.report_event(message);
    }

    #[must_use]
    pub fn services(&self) -> &Arc<EngineServices> {
        &self.ctx.services
    }

    /// 派生后台任务；其取消标志由 `NodeRuntime::stop_background` 统一置位。
    pub fn spawn<F>(&mut self, name: impl Into<String>, task: F)
    where
        F: FnOnce(SpawnContext) + Send + 'static,
    {
        let ctx = self.ctx.clone();
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || task(ctx));
        if let Ok(handle) = spawned {
            self.handles.push(handle);
        }
    }

    /// 置位取消标志并等待所有后台任务退出。
    pub fn stop_background(&mut self) {
        self.ctx.cancel.store(true, Ordering::Release);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::engine::packet::DataPacket;

    #[test]
    fn emit_without_downstream_is_noop_ok() {
        let outputs = OutputRegistry::default();
        let packet = DataPacket::Json(Arc::new(serde_json::json!({})));
        // 未连接任何下游的输出端口不应被视为错误。
        assert!(outputs.emit("unconnected", packet).is_ok());
    }

    #[test]
    fn emit_delivers_to_connected_downstream() {
        let mut outputs = OutputRegistry::default();
        let (tx, rx) = crate::engine::channel::create_mailbox(1);
        outputs.connect("out".to_owned(), tx);
        let packet = DataPacket::Json(Arc::new(serde_json::json!({"k": 1})));
        assert!(outputs.emit("out", packet).is_ok());
        assert!(matches!(
            rx.try_recv(),
            Ok(NodeMessage::Input { port, .. }) if port == "out"
        ));
    }

    #[test]
    fn emit_invokes_record_sink_with_packet() {
        // 有下游时 emit 成功后应回调 record（中间结果查看的缓存回灌）。
        let mut outputs = OutputRegistry::default();
        let (tx, _rx) = crate::engine::channel::create_mailbox(1);
        outputs.connect("out".to_owned(), tx);

        let captured: Arc<Mutex<Vec<DataPacket>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        outputs.set_record(Arc::new(move |packet| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(packet);
        }));

        let packet = DataPacket::Json(Arc::new(serde_json::json!({"k": 1})));
        assert!(outputs.emit("out", packet).is_ok());
        let captured = captured.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(captured.len(), 1);
    }

    #[test]
    fn emit_without_downstream_still_records() {
        // 无下游 no-op 成功也回调 record，保证「未接线但已 emit」的节点最近输出也可查。
        let mut outputs = OutputRegistry::default();
        let captured: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let sink = Arc::clone(&captured);
        outputs.set_record(Arc::new(move |_packet| {
            *sink.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        }));

        let packet = DataPacket::Json(Arc::new(serde_json::json!({})));
        assert!(outputs.emit("unconnected", packet).is_ok());
        assert_eq!(*captured.lock().unwrap_or_else(std::sync::PoisonError::into_inner), 1);
    }
}
