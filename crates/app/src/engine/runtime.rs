//! 节点运行时上下文：节点用它产出数据、上报状态/事件、派生后台任务。

use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

use crate::platform::{LatestDecodedFrameSlot, host_monotonic_time_ns};

use super::{
    channel::{ChannelFull, MailboxSender, NodeMessage},
    flow::EdgeFlowPulse,
    packet::DataPacket,
    services::EngineServices,
    spec::{NodeEvent, NodeId, NodeRuntimeState, NodeStatusReport, PortId},
};

/// 输出端口注册表：`port_id -> 下游 mailbox 列表`（fan-out）。
///
/// `record` 是一个可选回调，在 `emit` 成功（有下游投递或无下游 no-op）时被调用，
/// 用于把「节点最近输出」回灌到引擎的 `latest_outputs` 缓存（中间结果查看）。
///
/// `flow_tx` 只在具体下游投递成功时发送边级脉冲，不阻塞真实数据流；队列满时只丢动画事件。
/// 端口→下游列表用 `Arc<RwLock<HashMap>>` 承载内部可变性：增量图编辑（`add_edge`/
/// `remove_edge`）从引擎线程 `connect`/`disconnect`，而节点的 actor 线程同时 `emit`；
/// `emit` 只短暂持有读锁遍历下游，`connect`/`disconnect` 持有写锁短暂修改，
/// 二者不破坏彼此的一致性。`Clone` 仍为廉价 `Arc` 克隆，所有克隆共享同一张下游表。
#[derive(Clone)]
pub struct OutputRegistry {
    ports: Arc<RwLock<HashMap<PortId, Vec<Downstream>>>>,
    record: Arc<dyn Fn(DataPacket) + Send + Sync>,
    flow_tx: Option<mpsc::SyncSender<EdgeFlowPulse>>,
}

#[derive(Clone)]
struct Downstream {
    link: super::flow::EdgeLink,
    sender: MailboxSender,
}

impl Default for OutputRegistry {
    fn default() -> Self {
        Self {
            ports: Arc::new(RwLock::new(HashMap::new())),
            record: Arc::new(|_packet| {}),
            flow_tx: None,
        }
    }
}

impl OutputRegistry {
    /// 绑定边级脉冲发送端；build 时由图执行器注入。
    pub fn set_flow_tx(&mut self, flow_tx: mpsc::SyncSender<EdgeFlowPulse>) {
        self.flow_tx = Some(flow_tx);
    }

    /// 把一个下游 mailbox 挂到输出端口上；投递时使用边的目标输入端口。
    pub fn connect(&self, link: super::flow::EdgeLink, sender: MailboxSender) {
        self.ports
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(link.source_port_id.clone())
            .or_default()
            .push(Downstream { link, sender });
    }

    /// 从一个输出端口摘除指定下游 mailbox + 边 id（增量 remove_edge）。
    ///
    /// 同一源端口可连到同一节点的不同输入端口；因此 disconnect 必须同时匹配边与 mailbox。
    pub fn disconnect(&self, source_port: PortId, edge_id: &str, sender: &MailboxSender) {
        let mut ports = self
            .ports
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(senders) = ports.get_mut(&source_port) {
            senders
                .retain(|existing| existing.link.edge_id != edge_id || existing.sender != *sender);
            if senders.is_empty() {
                ports.remove(&source_port);
            }
        }
    }

    /// 设置输出的最近负载记录回调（引擎在 build 时注入，把 packet 写进 `latest_outputs`）。
    pub fn set_record(&mut self, record: Arc<dyn Fn(DataPacket) + Send + Sync>) {
        self.record = record;
    }

    /// 目标输出端口当前连接的下游数（用于 fan-out 诊断与测试断言）。
    #[must_use]
    pub fn fanout_count(&self, port: &str) -> usize {
        self.ports
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(port)
            .map_or(0, Vec::len)
    }

    fn report_flow(&self, link: &super::flow::EdgeLink, packet: &DataPacket) {
        let Some(flow_tx) = &self.flow_tx else {
            return;
        };
        let pulse = EdgeFlowPulse {
            edge_id: link.edge_id.clone(),
            source_node_id: link.source_node_id.clone(),
            source_port_id: link.source_port_id.clone(),
            target_node_id: link.target_node_id.clone(),
            target_port_id: link.target_port_id.clone(),
            packet_kind: packet.port_kind().to_owned(),
            sequence: packet.flow_sequence(),
            emitted_at_ns: host_monotonic_time_ns(),
        };
        let _ = flow_tx.try_send(pulse);
    }

    /// 向输出端口发布数据；无下游时视为 no-op 成功（未连接输出是合法状态，不算错误），
    /// 仅当下游存在但全部通道满时返回 `ChannelFull`。
    pub fn emit(&self, port: &str, packet: DataPacket) -> Result<(), ChannelFull> {
        let guard = self
            .ports
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(senders) = guard.get(port) else {
            drop(guard);
            (self.record)(packet);
            return Ok(());
        };
        let mut delivered = false;
        for downstream in senders {
            if downstream
                .sender
                .try_send(NodeMessage::Input {
                    port: downstream.link.target_port_id.clone(),
                    packet: packet.clone(),
                })
                .is_ok()
            {
                delivered = true;
                self.report_flow(&downstream.link, &packet);
            }
        }
        drop(guard);
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

    /// 停止所有后台任务并等待结束。
    pub fn stop_background(&mut self) {
        self.ctx.cancel.store(true, Ordering::Release);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, mpsc};

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
        let outputs = OutputRegistry::default();
        let (tx, rx) = crate::engine::channel::create_mailbox(1);
        outputs.connect(
            crate::engine::flow::EdgeLink {
                edge_id: "edge-1".to_owned(),
                source_node_id: "src".to_owned(),
                source_port_id: "out".to_owned(),
                target_node_id: "dst".to_owned(),
                target_port_id: "in".to_owned(),
            },
            tx,
        );
        let packet = DataPacket::Json(Arc::new(serde_json::json!({"k": 1})));
        assert!(outputs.emit("out", packet).is_ok());
        assert!(matches!(
            rx.try_recv(),
            Ok(NodeMessage::Input { port, .. }) if port == "in"
        ));
    }

    #[test]
    fn emit_invokes_record_sink_with_packet() {
        // 有下游时 emit 成功后应回调 record（中间结果查看的缓存回灌）。
        let mut outputs = OutputRegistry::default();
        let (tx, _rx) = crate::engine::channel::create_mailbox(1);
        outputs.connect(
            crate::engine::flow::EdgeLink {
                edge_id: "edge-1".to_owned(),
                source_node_id: "src".to_owned(),
                source_port_id: "out".to_owned(),
                target_node_id: "dst".to_owned(),
                target_port_id: "in".to_owned(),
            },
            tx,
        );

        let captured: Arc<Mutex<Vec<DataPacket>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        outputs.set_record(Arc::new(move |packet| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(packet);
        }));

        let packet = DataPacket::Json(Arc::new(serde_json::json!({"k": 1})));
        assert!(outputs.emit("out", packet).is_ok());
        let captured = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(captured.len(), 1);
    }

    #[test]
    fn emit_without_downstream_still_records() {
        // 无下游 no-op 成功也回调 record，保证「未接线但已 emit」的节点最近输出也可查。
        let mut outputs = OutputRegistry::default();
        let captured: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let sink = Arc::clone(&captured);
        outputs.set_record(Arc::new(move |_packet| {
            *sink
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        }));

        let packet = DataPacket::Json(Arc::new(serde_json::json!({})));
        assert!(outputs.emit("unconnected", packet).is_ok());
        assert_eq!(
            *captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            1
        );
    }

    #[test]
    fn emit_fans_out_to_all_connected_senders() {
        // 1 连多：一个输出端口连多个下游 mailbox，emit 一次全送达（fan-out）。
        let outputs = OutputRegistry::default();
        let (tx1, rx1) = crate::engine::channel::create_mailbox(1);
        let (tx2, rx2) = crate::engine::channel::create_mailbox(1);
        let (tx3, rx3) = crate::engine::channel::create_mailbox(1);
        outputs.connect(
            crate::engine::flow::EdgeLink {
                edge_id: "edge-1".to_owned(),
                source_node_id: "src".to_owned(),
                source_port_id: "out".to_owned(),
                target_node_id: "dst1".to_owned(),
                target_port_id: "in".to_owned(),
            },
            tx1,
        );
        outputs.connect(
            crate::engine::flow::EdgeLink {
                edge_id: "edge-2".to_owned(),
                source_node_id: "src".to_owned(),
                source_port_id: "out".to_owned(),
                target_node_id: "dst2".to_owned(),
                target_port_id: "in".to_owned(),
            },
            tx2,
        );
        outputs.connect(
            crate::engine::flow::EdgeLink {
                edge_id: "edge-3".to_owned(),
                source_node_id: "src".to_owned(),
                source_port_id: "out".to_owned(),
                target_node_id: "dst3".to_owned(),
                target_port_id: "in".to_owned(),
            },
            tx3,
        );

        let packet = DataPacket::Json(Arc::new(serde_json::json!({"k": 1})));
        assert!(outputs.emit("out", packet).is_ok());

        // 三个下游都必须收到目标输入端口 "in" 的数据。
        assert!(matches!(rx1.try_recv(), Ok(NodeMessage::Input { port, .. }) if port == "in"));
        assert!(matches!(rx2.try_recv(), Ok(NodeMessage::Input { port, .. }) if port == "in"));
        assert!(matches!(rx3.try_recv(), Ok(NodeMessage::Input { port, .. }) if port == "in"));
    }

    #[test]
    fn disconnect_removes_only_targeted_sender() {
        // fan-out 后摘除其中一个下游，其余照常收到，被摘者不再收到；端口清空则删除条目。
        let outputs = OutputRegistry::default();
        let (tx1, rx1) = crate::engine::channel::create_mailbox(1);
        let (tx2, rx2) = crate::engine::channel::create_mailbox(1);
        outputs.connect(
            crate::engine::flow::EdgeLink {
                edge_id: "edge-1".to_owned(),
                source_node_id: "src".to_owned(),
                source_port_id: "out".to_owned(),
                target_node_id: "dst1".to_owned(),
                target_port_id: "in1".to_owned(),
            },
            tx1.clone(),
        );
        outputs.connect(
            crate::engine::flow::EdgeLink {
                edge_id: "edge-2".to_owned(),
                source_node_id: "src".to_owned(),
                source_port_id: "out".to_owned(),
                target_node_id: "dst2".to_owned(),
                target_port_id: "in2".to_owned(),
            },
            tx2.clone(),
        );
        assert_eq!(outputs.fanout_count("out"), 2);

        outputs.disconnect("out".to_owned(), "edge-2", &tx2);
        assert_eq!(outputs.fanout_count("out"), 1);

        outputs
            .emit(
                "out",
                DataPacket::Json(Arc::new(serde_json::json!({"k": 1}))),
            )
            .expect("emit");
        // 仍连接的下游收到，被摘除的下游收不到。
        assert!(matches!(rx1.try_recv(), Ok(NodeMessage::Input { .. })));
        assert!(matches!(
            rx2.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        // 摘掉最后一个下游后端口条目被删除（幂等，重复 disconnect 亦 no-op）。
        outputs.disconnect("out".to_owned(), "edge-1", &tx1);
        assert_eq!(outputs.fanout_count("out"), 0);
        outputs.disconnect("out".to_owned(), "edge-1", &tx1); // idempotent no-op
        assert_eq!(outputs.fanout_count("out"), 0);
    }

    #[test]
    fn emit_reports_flow_only_for_successful_downstream_delivery() {
        let mut outputs = OutputRegistry::default();
        let (flow_tx, flow_rx) = mpsc::sync_channel(8);
        outputs.set_flow_tx(flow_tx);
        let (tx, rx) = crate::engine::channel::create_mailbox(1);
        outputs.connect(
            crate::engine::flow::EdgeLink {
                edge_id: "edge-flow".to_owned(),
                source_node_id: "src".to_owned(),
                source_port_id: "out".to_owned(),
                target_node_id: "dst".to_owned(),
                target_port_id: "in".to_owned(),
            },
            tx,
        );

        outputs
            .emit(
                "out",
                DataPacket::Json(Arc::new(serde_json::json!({"k": 1}))),
            )
            .expect("emit succeeds");

        assert!(matches!(rx.try_recv(), Ok(NodeMessage::Input { port, .. }) if port == "in"));
        let pulse = flow_rx
            .try_recv()
            .expect("successful delivery reports pulse");
        assert_eq!(pulse.edge_id, "edge-flow");
        assert_eq!(pulse.source_node_id, "src");
        assert_eq!(pulse.source_port_id, "out");
        assert_eq!(pulse.target_node_id, "dst");
        assert_eq!(pulse.target_port_id, "in");
        assert_eq!(pulse.packet_kind, "status.metrics");
        assert_eq!(pulse.sequence, None);
    }

    #[test]
    fn emit_does_not_report_flow_when_downstream_mailbox_is_full() {
        let mut outputs = OutputRegistry::default();
        let (flow_tx, flow_rx) = mpsc::sync_channel(8);
        outputs.set_flow_tx(flow_tx);
        let (tx, _rx) = crate::engine::channel::create_mailbox(1);
        tx.try_send(NodeMessage::Input {
            port: "in".to_owned(),
            packet: DataPacket::Json(Arc::new(serde_json::json!({"queued": true}))),
        })
        .expect("pre-fill mailbox");
        outputs.connect(
            crate::engine::flow::EdgeLink {
                edge_id: "edge-full".to_owned(),
                source_node_id: "src".to_owned(),
                source_port_id: "out".to_owned(),
                target_node_id: "dst".to_owned(),
                target_port_id: "in".to_owned(),
            },
            tx,
        );

        assert!(matches!(
            outputs.emit(
                "out",
                DataPacket::Json(Arc::new(serde_json::json!({"k": 1}))),
            ),
            Err(ChannelFull)
        ));
        assert!(matches!(flow_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }
}
