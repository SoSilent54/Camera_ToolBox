//! 图执行器：实例化节点、建立 mailbox 接线、spawn actor、管理生命周期。

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, atomic::AtomicBool, mpsc},
    thread::JoinHandle,
};

use thiserror::Error;

use crate::platform::LatestDecodedFrameSlot;

use super::{
    channel::{MailboxReceiver, MailboxSender, NodeMessage, create_mailbox},
    node::{NodeAction, NodeError, NodeInstance},
    registry::NodeRegistry,
    runtime::{NodeReporter, NodeRuntime, OutputRegistry, SpawnContext},
    services::EngineServices,
    spec::{NodeEvent, NodeId, NodeRuntimeState, NodeSpec, NodeStatusReport, PortId},
};

/// 端口端点引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortEndpoint {
    pub node_id: NodeId,
    pub port_id: PortId,
}

/// 一条边：源输出端口 → 目标输入端口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeSpec {
    pub id: String,
    pub source: PortEndpoint,
    pub target: PortEndpoint,
}

/// 引擎输入图：节点规格 + 边。
#[derive(Debug, Clone, Default)]
pub struct GraphSpec {
    pub nodes: Vec<NodeSpec>,
    pub edges: Vec<EdgeSpec>,
}

/// 图构建错误。
#[derive(Debug, Error)]
pub enum GraphBuildError {
    #[error("unknown node kind `{0}`")]
    UnknownKind(String),
    #[error("edge `{0}` references missing node `{1}`")]
    MissingNode(String, NodeId),
    #[error("edge `{0}` references missing port `{1}` on node `{2}`")]
    MissingPort(String, PortId, NodeId),
    #[error("node `{0}` failed to instantiate: {1}")]
    Instantiate(NodeId, #[source] NodeError),
}

/// 节点 actor 句柄。
struct EngineNodeHandle {
    mailbox: MailboxSender,
    handle: Option<JoinHandle<()>>,
}

pub struct GraphEngine {
    nodes: HashMap<NodeId, EngineNodeHandle>,
    status_rx: mpsc::Receiver<NodeStatusReport>,
    event_rx: mpsc::Receiver<NodeEvent>,
    viewer_slots: HashMap<NodeId, Arc<LatestDecodedFrameSlot>>,
}

impl GraphEngine {
    /// 构建图：校验 → 实例化 → 接线 → spawn actor。
    pub fn build(
        spec: GraphSpec,
        registry: &NodeRegistry,
        services: EngineServices,
    ) -> Result<Self, GraphBuildError> {
        let (status_tx, status_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let services = Arc::new(services);

        // 1. 实例化节点。
        let mut instances: HashMap<NodeId, Box<dyn NodeInstance>> = HashMap::new();
        for node in &spec.nodes {
            let factory = registry
                .get(&node.kind)
                .ok_or_else(|| GraphBuildError::UnknownKind(node.kind.clone()))?;
            let instance = factory
                .instantiate(node.clone())
                .map_err(|error| GraphBuildError::Instantiate(node.id.clone(), error))?;
            instances.insert(node.id.clone(), instance);
        }

        // 2. 每节点建 mailbox 与输出注册表。
        let mut mailboxes: HashMap<NodeId, (MailboxSender, MailboxReceiver)> = HashMap::new();
        let mut outputs: HashMap<NodeId, OutputRegistry> = HashMap::new();
        for node in &spec.nodes {
            let (tx, rx) = create_mailbox(4);
            mailboxes.insert(node.id.clone(), (tx, rx));
            outputs.insert(node.id.clone(), OutputRegistry::default());
        }

        // 3. 接线（fan-out）：源输出端口 → 目标 mailbox。
        let mut connected_inputs: HashMap<NodeId, HashSet<PortId>> = HashMap::new();
        for edge in &spec.edges {
            let (target_tx, _) = mailboxes
                .get(&edge.target.node_id)
                .ok_or_else(|| GraphBuildError::MissingNode(edge.id.clone(), edge.target.node_id.clone()))?;
            let source_outputs = outputs
                .get_mut(&edge.source.node_id)
                .ok_or_else(|| GraphBuildError::MissingNode(edge.id.clone(), edge.source.node_id.clone()))?;
            source_outputs.connect(edge.source.port_id.clone(), target_tx.clone());
            connected_inputs
                .entry(edge.target.node_id.clone())
                .or_default()
                .insert(edge.target.port_id.clone());
        }

        // 4. spawn actor，并推导初始状态。
        let mut nodes = HashMap::new();
        let mut viewer_slots = HashMap::new();
        for node in &spec.nodes {
            let instance = instances.remove(&node.id).expect("node instantiated");
            let (mailbox, mailbox_rx) = mailboxes.remove(&node.id).expect("mailbox created");
            let outputs = outputs.remove(&node.id).expect("outputs created");

            let initial_state = initial_state(node, connected_inputs.get(&node.id));
            let reporter = NodeReporter::new(node.id.clone(), status_tx.clone(), event_tx.clone());
            reporter.report_state(
                initial_state,
                state_diagnostic(
                    initial_state,
                    &connected_inputs.get(&node.id).cloned().unwrap_or_default(),
                    node,
                ),
            );

            // viewer 节点预分配帧出口；其余节点为 None。
            let viewer_slot = (node.kind == crate::engine::node::kinds::VIEWER)
                .then(|| Arc::new(LatestDecodedFrameSlot::default()));
            let handle = spawn_node_actor(
                node.clone(),
                instance,
                mailbox_rx,
                outputs,
                reporter,
                Arc::clone(&services),
                viewer_slot.clone(),
            );
            if let Some(slot) = viewer_slot {
                viewer_slots.insert(node.id.clone(), slot);
            }
            nodes.insert(
                node.id.clone(),
                EngineNodeHandle {
                    mailbox,
                    handle: Some(handle),
                },
            );
        }

        Ok(Self {
            nodes,
            status_rx,
            event_rx,
            viewer_slots,
        })
    }

    /// 向指定节点投递动作（connect/disconnect/trigger/arm/disarm）。
    pub fn send_action(&self, node_id: &str, action: NodeAction) -> Result<(), NodeError> {
        let handle = self
            .nodes
            .get(node_id)
            .ok_or_else(|| NodeError::Precondition(format!("node `{node_id}` not found")))?;
        handle
            .mailbox
            .send(NodeMessage::Action(action))
            .map_err(|_| NodeError::Execution(format!("node `{node_id}` mailbox closed")))
    }

    /// 非阻塞取回节点状态更新（web 层轮询后推 WebSocket）。
    #[must_use]
    pub fn drain_status(&self) -> Vec<NodeStatusReport> {
        let mut out = Vec::new();
        while let Ok(status) = self.status_rx.try_recv() {
            out.push(status);
        }
        out
    }

    /// 非阻塞取回节点事件（web 层轮询后推 WebSocket）。
    #[must_use]
    pub fn drain_events(&self) -> Vec<NodeEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            out.push(event);
        }
        out
    }

    /// 获取 viewer 节点的最新解码帧（web 层据此编码 JPEG 推流）。
    #[must_use]
    pub fn viewer_frame(&self, node_id: &str) -> Option<Arc<crate::platform::DecodedVideoFrame>> {
        self.viewer_slots.get(node_id)?.latest()
    }

    /// 停止图：发 Stop 给所有 actor 并等待退出。
    pub fn stop(&mut self) {
        for handle in self.nodes.values() {
            let _ = handle.mailbox.send(NodeMessage::Stop);
        }
        for handle in self.nodes.values_mut() {
            if let Some(join) = handle.handle.take() {
                let _ = join.join();
            }
        }
    }
}

impl Drop for GraphEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 推导节点初始状态：存在必需输入但未连接 → `Disabled`。
fn initial_state(node: &NodeSpec, connected: Option<&HashSet<PortId>>) -> NodeRuntimeState {
    let connected = connected.map_or_else(HashSet::new, Clone::clone);
    let missing_required = node
        .inputs
        .iter()
        .any(|port| port.required && !connected.contains(&port.id));
    if missing_required {
        NodeRuntimeState::Disabled
    } else {
        NodeRuntimeState::Idle
    }
}

fn state_diagnostic(state: NodeRuntimeState, connected: &HashSet<PortId>, node: &NodeSpec) -> String {
    match state {
        NodeRuntimeState::Disabled => {
            let missing: Vec<&str> = node
                .inputs
                .iter()
                .filter(|port| port.required && !connected.contains(&port.id))
                .map(|port| port.label.as_str())
                .collect();
            format!("precondition unmet: connect {}", missing.join(", "))
        }
        _ => "ready to start".to_owned(),
    }
}

/// spawn 节点 actor：主循环阻塞接收 mailbox 消息并分发到 `NodeInstance`。
fn spawn_node_actor(
    spec: NodeSpec,
    mut instance: Box<dyn NodeInstance>,
    mailbox: MailboxReceiver,
    outputs: OutputRegistry,
    reporter: NodeReporter,
    services: Arc<EngineServices>,
    viewer_slot: Option<Arc<LatestDecodedFrameSlot>>,
) -> JoinHandle<()> {
    let name = format!("node-{}-{}", spec.kind, spec.id);
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let cancel = Arc::new(AtomicBool::new(false));
            let ctx = SpawnContext {
                outputs,
                reporter: reporter.clone(),
                services,
                cancel,
                viewer_slot,
            };
            let mut rt = NodeRuntime::new(ctx);
            if let Err(error) = instance.on_start(&mut rt) {
                reporter.report_state(NodeRuntimeState::Error, error.to_string());
            }

            loop {
                match mailbox.recv() {
                    Ok(NodeMessage::Input { port, packet }) => {
                        if let Err(error) = instance.on_input(&port, packet, &mut rt) {
                            reporter.report_event(format!("input on `{port}` failed: {error}"));
                        }
                    }
                    Ok(NodeMessage::Action(action)) => {
                        if let Err(error) = instance.on_action(action, &mut rt) {
                            reporter.report_event(format!("action failed: {error}"));
                        }
                    }
                    Ok(NodeMessage::Stop) | Err(_) => break,
                }
            }

            if let Err(error) = instance.on_stop(&mut rt) {
                reporter.report_event(format!("stop failed: {error}"));
            }
            rt.stop_background();
        })
        .expect("spawn node actor")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::engine::{DataPacket, NodeFactory, PortSpec};
    use crate::engine::spec::PortCardinality;

    struct PassthroughFactory;

    impl NodeFactory for PassthroughFactory {
        fn kind(&self) -> &'static str {
            "passthrough"
        }

        fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
            Ok(Box::new(PassthroughNode))
        }
    }

    struct PassthroughNode;

    impl NodeInstance for PassthroughNode {
        fn kind(&self) -> &'static str {
            "passthrough"
        }

        fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
            rt.report_state(NodeRuntimeState::Ready, "started");
            Ok(())
        }

        fn on_input(
            &mut self,
            _port: &str,
            packet: DataPacket,
            rt: &mut NodeRuntime,
        ) -> Result<(), NodeError> {
            let _ = rt.emit("out", packet);
            Ok(())
        }

        fn on_action(&mut self, _action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
            Ok(())
        }

        fn on_stop(&mut self, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
            Ok(())
        }
    }

    fn port(id: &str, required: bool) -> PortSpec {
        PortSpec {
            id: id.to_owned(),
            label: id.to_owned(),
            kind: "status.metrics".to_owned(),
            cardinality: PortCardinality::One,
            required,
        }
    }

    fn node_spec(id: &str, inputs: Vec<PortSpec>, outputs: Vec<PortSpec>) -> NodeSpec {
        NodeSpec {
            id: id.to_owned(),
            kind: "passthrough".to_owned(),
            title: id.to_owned(),
            inputs,
            outputs,
            config: serde_json::json!({}),
        }
    }

    fn edge(id: &str, source: &str, target: &str) -> EdgeSpec {
        EdgeSpec {
            id: id.to_owned(),
            source: PortEndpoint {
                node_id: source.to_owned(),
                port_id: "out".to_owned(),
            },
            target: PortEndpoint {
                node_id: target.to_owned(),
                port_id: "in".to_owned(),
            },
        }
    }

    #[test]
    fn build_derives_disabled_for_unconnected_required_input() {
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![node_spec("b", vec![port("in", true)], vec![port("out", false)])],
            edges: vec![],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();
        let statuses = engine.drain_status();
        assert!(statuses.iter().any(|status| {
            status.node_id == "b" && status.state == NodeRuntimeState::Disabled
        }));
        let _ = engine;
    }

    #[test]
    fn build_marks_connected_required_input_idle() {
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![
                node_spec("a", vec![], vec![port("out", false)]),
                node_spec("b", vec![port("in", true)], vec![port("out", false)]),
            ],
            edges: vec![edge("e", "a", "b")],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();
        let statuses = engine.drain_status();
        assert!(statuses.iter().any(|status| {
            status.node_id == "b" && status.state == NodeRuntimeState::Idle
        }));
        let _ = engine;
    }

    #[test]
    fn build_rejects_unknown_kind() {
        let registry = NodeRegistry::new();
        let spec = GraphSpec {
            nodes: vec![node_spec("a", vec![], vec![])],
            edges: vec![],
        };
        let result = GraphEngine::build(spec, &registry, EngineServices::default());
        assert!(matches!(result, Err(GraphBuildError::UnknownKind(_))));
    }

    #[test]
    fn action_reaches_node_instance() {
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![node_spec("a", vec![], vec![port("out", false)])],
            edges: vec![],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();
        assert!(engine.send_action("a", NodeAction::Trigger).is_ok());
        assert!(engine.send_action("missing", NodeAction::Trigger).is_err());
        let _ = engine;
    }

    #[test]
    fn packet_has_stable_port_kind() {
        let packet = DataPacket::Json(Arc::new(serde_json::json!({})));
        assert_eq!(packet.port_kind(), "status.metrics");
    }
}
