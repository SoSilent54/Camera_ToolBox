//! 图执行器：实例化节点、建立 mailbox 接线、spawn actor、管理生命周期。

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, RwLock, atomic::AtomicBool, mpsc},
    thread::JoinHandle,
};

use thiserror::Error;

use crate::platform::LatestDecodedFrameSlot;

use super::{
    channel::{MailboxReceiver, MailboxSender, NodeMessage, create_mailbox},
    node::{NodeAction, NodeError, NodeInstance},
    packet::DataPacket,
    registry::NodeRegistry,
    runtime::{NodeReporter, NodeRuntime, OutputRegistry, SpawnContext},
    services::EngineServices,
    spec::{
        NodeEvent, NodeId, NodeRuntimeState, NodeSpec, NodeStatusReport, PortCardinality, PortId,
    },
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
    #[error("edge `{edge}` connects a second source into cardinality=One input `{node}:{port}`")]
    CardinalityViolation {
        edge: String,
        node: NodeId,
        port: PortId,
    },
    #[error("node `{0}` already exists (incremental add_node)")]
    DuplicateNode(NodeId),
    #[error("edge `{0}` already exists (incremental add_edge)")]
    DuplicateEdge(String),
    #[error("edge `{edge}` would create a cycle")]
    WouldCreateCycle { edge: String },
    #[error("edge `{edge}` port kinds mismatch: `{from}` != `{to}`")]
    PortKindMismatch {
        edge: String,
        from: String,
        to: String,
    },
}

/// 节点 actor 句柄。
struct EngineNodeHandle {
    /// 保留完整 spec：增量 add_edge/remove_edge/update_node 需要据此校验端口与 cardinality。
    spec: NodeSpec,
    mailbox: MailboxSender,
    outputs: OutputRegistry,
    handle: Option<JoinHandle<()>>,
}

/// 运行中图的可变注册表，由 [`RwLock`] 保护：增量 API 以 `&self` 调用，写侧短暂持写锁，
/// 读侧（viewer/latest_output/边遍历）持读锁，与运行中的 actor 并发安全。
struct Inner {
    nodes: HashMap<NodeId, EngineNodeHandle>,
    /// edge id → spec，用于 remove_edge 定位、cardinality 判重与 add_edge 成环检测。
    edges: HashMap<String, EdgeSpec>,
    viewer_slots: HashMap<NodeId, Arc<LatestDecodedFrameSlot>>,
}

pub struct GraphEngine {
    /// 引擎级服务，供增量 add_node 装配新 actor 复用。
    services: Arc<EngineServices>,
    /// 状态/事件发送端：build 时创建并长期保留，供增量 add_node 的新 reporter 克隆。
    status_tx: mpsc::Sender<NodeStatusReport>,
    event_tx: mpsc::Sender<NodeEvent>,
    status_rx: mpsc::Receiver<NodeStatusReport>,
    event_rx: mpsc::Receiver<NodeEvent>,
    latest_outputs: Arc<Mutex<HashMap<NodeId, DataPacket>>>,
    inner: RwLock<Inner>,
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
        // 节点最近输出缓存：emit 成功后由每个节点的 OutputRegistry 回灌，供中间结果查看。
        let latest_outputs: Arc<Mutex<HashMap<NodeId, DataPacket>>> =
            Arc::new(Mutex::new(HashMap::new()));

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
            outputs.insert(
                node.id.clone(),
                make_output_registry(node.id.clone(), &latest_outputs),
            );
        }

        // 3. 接线（fan-out）：源输出端口 → 目标 mailbox。
        //    先按 node_id 建规格索引，校验边的端口确实存在于声明的 inputs/outputs 上，
        //    并执行 D7 cardinality 判重：One 输入端口只允许一条入边。
        let node_specs: HashMap<&NodeId, &NodeSpec> =
            spec.nodes.iter().map(|node| (&node.id, node)).collect();
        let mut connected_inputs: HashMap<NodeId, HashSet<PortId>> = HashMap::new();
        for edge in &spec.edges {
            validate_edge(edge, &node_specs, &mut connected_inputs)?;
            let (target_tx, _) = mailboxes
                .get(&edge.target.node_id)
                .expect("target node validated to exist");
            let source_outputs = outputs
                .get_mut(&edge.source.node_id)
                .expect("source node validated to exist");
            source_outputs.connect(edge.source.port_id.clone(), target_tx.clone());
        }

        // 4. spawn actor，并推导初始状态。
        let mut nodes = HashMap::new();
        let mut viewer_slots = HashMap::new();
        for node in &spec.nodes {
            let instance = instances.remove(&node.id).expect("node instantiated");
            let (mailbox, mailbox_rx) = mailboxes.remove(&node.id).expect("mailbox created");
            let outputs = outputs.remove(&node.id).expect("outputs created");

            let initial_state = initial_state(node, connected_inputs.get(&node.id));
            let diagnostic = state_diagnostic(
                initial_state,
                &connected_inputs.get(&node.id).cloned().unwrap_or_default(),
                node,
            );

            // viewer 节点预分配帧出口；actor 与 viewer_slots 注册共享同一 Arc。
            let viewer_slot = (node.kind == crate::engine::node::kinds::VIEWER)
                .then(|| Arc::new(LatestDecodedFrameSlot::default()));
            let reporter = NodeReporter::new(node.id.clone(), status_tx.clone(), event_tx.clone());
            // 同步上报引擎推导的初始状态（与 build 原语义一致，避免 actor 线程竞态）。
            reporter.report_state(initial_state, diagnostic);
            let handle = spawn_node_actor(
                node.clone(),
                instance,
                mailbox_rx,
                outputs.clone(),
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
                    spec: node.clone(),
                    mailbox,
                    outputs,
                    handle: Some(handle),
                },
            );
        }

        let engine = Inner::new(nodes, spec.edges, viewer_slots);
        Ok(Self {
            services,
            status_tx,
            event_tx,
            status_rx,
            event_rx,
            latest_outputs,
            inner: RwLock::new(engine),
        })
    }

    /// 向指定节点投递动作（connect/disconnect/trigger/arm/disarm）。
    pub fn send_action(&self, node_id: &str, action: NodeAction) -> Result<(), NodeError> {
        let inner = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = inner
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
        let inner = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.viewer_slots.get(node_id)?.latest()
    }

    /// 枚举当前图中所有 viewer 节点 id（web 层据此对每个 viewer 做帧推送）。
    #[must_use]
    pub fn viewer_node_ids(&self) -> Vec<NodeId> {
        let inner = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ids: Vec<NodeId> = inner.viewer_slots.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// 获取任意节点的最近输出负载（中间结果查看）；未 emit 过返回 `None`。
    #[must_use]
    pub fn latest_output(&self, node_id: &str) -> Option<DataPacket> {
        self.latest_outputs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(node_id)
            .cloned()
    }

    /// 停止图：发 Stop 给所有 actor 并等待退出。
    pub fn stop(&mut self) {
        let inner = self
            .inner
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for handle in inner.nodes.values() {
            let _ = handle.mailbox.send(NodeMessage::Stop);
        }
        for handle in inner.nodes.values_mut() {
            if let Some(join) = handle.handle.take() {
                let _ = join.join();
            }
        }
    }

    /// 增量：向运行中的图新增一个节点（实例化 → mailbox/outputs → spawn actor）。
    ///
    /// node id 重复则返回 [`GraphBuildError::DuplicateNode`]。新增节点初始状态按当前图
    /// 的边推导（无入边且 required 输入未满足 → Disabled，否则 Idle）。
    pub fn add_node(&self, node: NodeSpec, registry: &NodeRegistry) -> Result<(), GraphBuildError> {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.nodes.contains_key(&node.id) {
            return Err(GraphBuildError::DuplicateNode(node.id.clone()));
        }
        let factory = registry
            .get(&node.kind)
            .ok_or_else(|| GraphBuildError::UnknownKind(node.kind.clone()))?;
        let instance = factory
            .instantiate(node.clone())
            .map_err(|error| GraphBuildError::Instantiate(node.id.clone(), error))?;

        let (mailbox_tx, mailbox_rx) = create_mailbox(4);
        let outputs = make_output_registry(node.id.clone(), &self.latest_outputs);

        let connected = connected_inputs_for(&inner.edges, &node.id);
        let initial_state = initial_state(&node, Some(&connected));
        let diagnostic = state_diagnostic(initial_state, &connected, &node);

        let viewer_slot = (node.kind == crate::engine::node::kinds::VIEWER)
            .then(|| Arc::new(LatestDecodedFrameSlot::default()));
        let reporter = NodeReporter::new(
            node.id.clone(),
            self.status_tx.clone(),
            self.event_tx.clone(),
        );
        reporter.report_state(initial_state, diagnostic);
        let handle = spawn_node_actor(
            node.clone(),
            instance,
            mailbox_rx,
            outputs.clone(),
            reporter,
            Arc::clone(&self.services),
            viewer_slot.clone(),
        );

        if let Some(slot) = viewer_slot {
            inner.viewer_slots.insert(node.id.clone(), slot);
        }
        inner.nodes.insert(
            node.id.clone(),
            EngineNodeHandle {
                spec: node,
                mailbox: mailbox_tx,
                outputs,
                handle: Some(handle),
            },
        );
        Ok(())
    }

    /// 增量：从运行中的图移除一个节点。先摘图内引用，再停 actor。
    pub fn remove_node(&self, node_id: &str) -> Result<(), GraphBuildError> {
        let mut handle = {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(handle) = inner.nodes.remove(node_id) else {
                return Err(GraphBuildError::MissingNode(
                    format!("remove node `{node_id}`"),
                    node_id.to_owned(),
                ));
            };

            // 先断开所有相关边并从图中摘除该节点；释放图锁后再 join，避免慢停止卡住全图读写。
            let related: Vec<(String, EdgeSpec)> = inner
                .edges
                .iter()
                .filter(|(_, edge)| {
                    edge.source.node_id == node_id || edge.target.node_id == node_id
                })
                .map(|(id, edge)| (id.clone(), edge.clone()))
                .collect();
            for (id, edge) in &related {
                if edge.source.node_id == node_id {
                    if let Some(target) = inner.nodes.get(&edge.target.node_id) {
                        handle
                            .outputs
                            .disconnect(edge.source.port_id.clone(), &target.mailbox);
                    }
                } else if let Some(source) = inner.nodes.get_mut(&edge.source.node_id) {
                    source
                        .outputs
                        .disconnect(edge.source.port_id.clone(), &handle.mailbox);
                }
                inner.edges.remove(id);
            }

            inner.viewer_slots.remove(node_id);
            self.latest_outputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(node_id);
            handle
        };

        let _ = handle.mailbox.send(NodeMessage::Stop);
        if let Some(join) = handle.handle.take() {
            let join_name = format!("node-stop-{}", node_id);
            let _ = std::thread::Builder::new().name(join_name).spawn(move || {
                let _ = join.join();
            });
        }
        Ok(())
    }

    /// 增量：向运行中的图新增一条边（校验后可作用于运行中图）。
    ///
    /// 校验：源/目标节点与端口存在、kind 匹配、目标输入端口 cardinality（One 已有入边则拒）、
    /// 无环。全部通过后才 `connect`，失败不改引擎。
    pub fn add_edge(&self, edge: EdgeSpec) -> Result<(), GraphBuildError> {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.edges.contains_key(&edge.id) {
            return Err(GraphBuildError::DuplicateEdge(edge.id.clone()));
        }
        // 用自有快照做校验，避免借用 inner 与后续可变借用冲突。
        let snapshot = snapshot_spec(&inner);
        let index: HashMap<&NodeId, &NodeSpec> =
            snapshot.nodes.iter().map(|node| (&node.id, node)).collect();
        let mut connected = all_connected_inputs(&inner.edges);
        validate_edge(&edge, &index, &mut connected)?;
        // 成环检测：沿目标 → 源方向 BFS，确认不会回到源。
        if would_create_cycle(&inner.edges, &edge) {
            return Err(GraphBuildError::WouldCreateCycle {
                edge: edge.id.clone(),
            });
        }
        let target_mailbox = inner
            .nodes
            .get(&edge.target.node_id)
            .expect("target validated")
            .mailbox
            .clone();
        inner
            .nodes
            .get_mut(&edge.source.node_id)
            .expect("source validated")
            .outputs
            .connect(edge.source.port_id.clone(), target_mailbox);
        inner.edges.insert(edge.id.clone(), edge);
        Ok(())
    }

    /// 增量：从运行中的图移除一条边（源输出端口 disconnect 掉目标 mailbox）。
    pub fn remove_edge(&self, edge_id: &str) -> Result<(), GraphBuildError> {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(edge) = inner.edges.remove(edge_id) else {
            return Err(GraphBuildError::MissingNode(
                format!("remove edge `{edge_id}`"),
                edge_id.to_owned(),
            ));
        };
        // 先取目标 mailbox 克隆（不可变借用结束），再可变借用源节点的 outputs 摘除。
        let target_mailbox = inner
            .nodes
            .get(&edge.target.node_id)
            .map(|handle| handle.mailbox.clone());
        if let Some(target_mailbox) = target_mailbox {
            if let Some(source) = inner.nodes.get_mut(&edge.source.node_id) {
                source
                    .outputs
                    .disconnect(edge.source.port_id.clone(), &target_mailbox);
            }
        }
        Ok(())
    }

    /// 增量：更新节点 config（本阶段为 config 替换），保留其余 spec 字段不动。
    pub fn update_node(
        &self,
        node_id: &str,
        config: serde_json::Value,
    ) -> Result<(), GraphBuildError> {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = inner.nodes.get_mut(node_id).ok_or_else(|| {
            GraphBuildError::MissingNode(format!("update node `{node_id}`"), node_id.to_owned())
        })?;
        handle.spec.config = config;
        Ok(())
    }
}

impl Drop for GraphEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Inner {
    fn new(
        nodes: HashMap<NodeId, EngineNodeHandle>,
        edges: Vec<EdgeSpec>,
        viewer_slots: HashMap<NodeId, Arc<LatestDecodedFrameSlot>>,
    ) -> Self {
        let edges = edges
            .into_iter()
            .map(|edge| (edge.id.clone(), edge))
            .collect();
        Self {
            nodes,
            edges,
            viewer_slots,
        }
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

fn state_diagnostic(
    state: NodeRuntimeState,
    connected: &HashSet<PortId>,
    node: &NodeSpec,
) -> String {
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

/// 构造节点的输出注册表，并注入「回灌最近输出到 latest_outputs」的 record 回调。
fn make_output_registry(
    node_id: NodeId,
    latest_outputs: &Arc<Mutex<HashMap<NodeId, DataPacket>>>,
) -> OutputRegistry {
    let mut registry = OutputRegistry::default();
    let sink = Arc::clone(latest_outputs);
    registry.set_record(Arc::new(move |packet: DataPacket| {
        sink.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(node_id.clone(), packet);
    }));
    registry
}

/// 单条边的拓扑校验：源/目标节点与端口存在、kind 匹配、目标输入端口 cardinality 判重。
///
/// 通过 `connected`（目标节点 → 已连接输入端口集合）累计写入，供后续边校验 cardinality
/// 与推导初始状态复用。`specs` 为全图节点规格索引。校验失败返回错误，不改任何状态。
fn validate_edge(
    edge: &EdgeSpec,
    specs: &HashMap<&NodeId, &NodeSpec>,
    connected: &mut HashMap<NodeId, HashSet<PortId>>,
) -> Result<(), GraphBuildError> {
    let source_spec = specs.get(&edge.source.node_id).ok_or_else(|| {
        GraphBuildError::MissingNode(edge.id.clone(), edge.source.node_id.clone())
    })?;
    let target_spec = specs.get(&edge.target.node_id).ok_or_else(|| {
        GraphBuildError::MissingNode(edge.id.clone(), edge.target.node_id.clone())
    })?;

    let source_port = source_spec
        .outputs
        .iter()
        .find(|port| port.id == edge.source.port_id)
        .ok_or_else(|| {
            GraphBuildError::MissingPort(
                edge.id.clone(),
                edge.source.port_id.clone(),
                edge.source.node_id.clone(),
            )
        })?;
    let target_port = target_spec
        .inputs
        .iter()
        .find(|port| port.id == edge.target.port_id)
        .ok_or_else(|| {
            GraphBuildError::MissingPort(
                edge.id.clone(),
                edge.target.port_id.clone(),
                edge.target.node_id.clone(),
            )
        })?;

    // schema 匹配：源输出 port.kind 必须等于目标输入 port.kind。
    if source_port.kind != target_port.kind {
        return Err(GraphBuildError::PortKindMismatch {
            edge: edge.id.clone(),
            from: source_port.kind.clone(),
            to: target_port.kind.clone(),
        });
    }

    // D7 cardinality：One 输入端口只允许一条入边。
    let target_connected = connected.entry(edge.target.node_id.clone()).or_default();
    if target_port.cardinality == PortCardinality::One
        && target_connected.contains(&edge.target.port_id)
    {
        return Err(GraphBuildError::CardinalityViolation {
            edge: edge.id.clone(),
            node: edge.target.node_id.clone(),
            port: edge.target.port_id.clone(),
        });
    }
    target_connected.insert(edge.target.port_id.clone());
    Ok(())
}

/// 从已登记的边推导某节点的已连接输入端口集合。
fn connected_inputs_for(edges: &HashMap<String, EdgeSpec>, node_id: &str) -> HashSet<PortId> {
    edges
        .values()
        .filter(|edge| edge.target.node_id == node_id)
        .map(|edge| edge.target.port_id.clone())
        .collect()
}

/// 从边的集合（Vec 形式）推导全图「目标节点 → 已连接输入端口」映射。
fn all_connected_inputs(edges: &HashMap<String, EdgeSpec>) -> HashMap<NodeId, HashSet<PortId>> {
    let mut connected: HashMap<NodeId, HashSet<PortId>> = HashMap::new();
    for edge in edges.values() {
        connected
            .entry(edge.target.node_id.clone())
            .or_default()
            .insert(edge.target.port_id.clone());
    }
    connected
}

/// 从运行内注册表快照一张 `GraphSpec`，供增量校验复用（自有数据，避免借用冲突）。
fn snapshot_spec(inner: &Inner) -> GraphSpec {
    GraphSpec {
        nodes: inner
            .nodes
            .values()
            .map(|handle| handle.spec.clone())
            .collect(),
        edges: inner.edges.values().cloned().collect(),
    }
}

/// 成环检测：加入 `edge` 后，从目标节点出发沿边方向是否可回到源节点（BFS）。
fn would_create_cycle(edges: &HashMap<String, EdgeSpec>, edge: &EdgeSpec) -> bool {
    // 自环直接拒绝。
    if edge.source.node_id == edge.target.node_id {
        return true;
    }
    let mut visited: HashSet<&NodeId> = HashSet::new();
    let mut frontier: Vec<&NodeId> = vec![&edge.target.node_id];
    while let Some(node) = frontier.pop() {
        if *node == edge.source.node_id {
            return true;
        }
        if !visited.insert(node) {
            continue;
        }
        for outgoing in edges.values() {
            if outgoing.source.node_id == *node {
                frontier.push(&outgoing.target.node_id);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::engine::services::StreamServiceFactory;
    use crate::engine::spec::PortCardinality;
    use crate::engine::{DataPacket, NodeFactory, PortSpec};
    use crate::platform::StreamService;

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

        fn on_action(
            &mut self,
            _action: NodeAction,
            _rt: &mut NodeRuntime,
        ) -> Result<(), NodeError> {
            Ok(())
        }

        fn on_stop(&mut self, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
            Ok(())
        }
    }

    /// 主动发数据的源节点：on_action(Trigger) 时向第一个输出端口 emit，用于图级数据流测试。
    struct EmitterNode {
        out: String,
    }

    impl NodeInstance for EmitterNode {
        fn kind(&self) -> &'static str {
            "emitter"
        }

        fn on_start(&mut self, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
            Ok(())
        }

        fn on_input(
            &mut self,
            _port: &str,
            _packet: DataPacket,
            _rt: &mut NodeRuntime,
        ) -> Result<(), NodeError> {
            Ok(())
        }

        fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
            if matches!(action, NodeAction::Trigger) {
                let _ = rt.emit(
                    &self.out,
                    DataPacket::Json(Arc::new(serde_json::json!({"src": "emitter"}))),
                );
            }
            Ok(())
        }

        fn on_stop(&mut self, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
            Ok(())
        }
    }

    /// 记录收到哪个端口 + payload 的节点，用于 fan-in 端口区分断言。
    struct RecorderNode {
        received: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl NodeInstance for RecorderNode {
        fn kind(&self) -> &'static str {
            "recorder"
        }

        fn on_start(&mut self, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
            Ok(())
        }

        fn on_input(
            &mut self,
            port: &str,
            packet: DataPacket,
            _rt: &mut NodeRuntime,
        ) -> Result<(), NodeError> {
            let marker = match packet {
                DataPacket::Json(value) => value["src"].as_str().unwrap_or("?").to_owned(),
                _ => "?".to_owned(),
            };
            self.received
                .lock()
                .unwrap()
                .push((port.to_owned(), marker));
            Ok(())
        }

        fn on_action(
            &mut self,
            _action: NodeAction,
            _rt: &mut NodeRuntime,
        ) -> Result<(), NodeError> {
            Ok(())
        }

        fn on_stop(&mut self, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
            Ok(())
        }
    }

    /// 主动发数据源节点的工厂：从首个输出端口确定 emit 端口 id。
    struct EmitterFactory;

    impl NodeFactory for EmitterFactory {
        fn kind(&self) -> &'static str {
            "emitter"
        }

        fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
            let out = spec
                .outputs
                .first()
                .map(|port| port.id.clone())
                .unwrap_or_else(|| "out".to_owned());
            Ok(Box::new(EmitterNode { out }))
        }
    }

    /// 记录节点工厂：共享一个 `received` 缓冲，所有实例写入同一处供断言。
    struct RecorderFactory {
        received: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl NodeFactory for RecorderFactory {
        fn kind(&self) -> &'static str {
            "recorder"
        }

        fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
            Ok(Box::new(RecorderNode {
                received: Arc::clone(&self.received),
            }))
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
            nodes: vec![node_spec(
                "b",
                vec![port("in", true)],
                vec![port("out", false)],
            )],
            edges: vec![],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();
        let statuses = engine.drain_status();
        assert!(
            statuses.iter().any(|status| {
                status.node_id == "b" && status.state == NodeRuntimeState::Disabled
            })
        );
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
        assert!(
            statuses
                .iter()
                .any(|status| { status.node_id == "b" && status.state == NodeRuntimeState::Idle })
        );
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
    fn build_rejects_missing_source_port() {
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![
                node_spec("a", vec![], vec![port("out", false)]),
                node_spec("b", vec![port("in", true)], vec![]),
            ],
            edges: vec![EdgeSpec {
                id: "e".to_owned(),
                source: PortEndpoint {
                    node_id: "a".to_owned(),
                    port_id: "nonexistent".to_owned(),
                },
                target: PortEndpoint {
                    node_id: "b".to_owned(),
                    port_id: "in".to_owned(),
                },
            }],
        };
        let result = GraphEngine::build(spec, &registry, EngineServices::default());
        assert!(matches!(
            result,
            Err(GraphBuildError::MissingPort(id, port, node))
                if id == "e" && port == "nonexistent" && node == "a"
        ));
    }

    #[test]
    fn build_rejects_missing_target_port() {
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![
                node_spec("a", vec![], vec![port("out", false)]),
                node_spec("b", vec![port("in", true)], vec![]),
            ],
            edges: vec![EdgeSpec {
                id: "e".to_owned(),
                source: PortEndpoint {
                    node_id: "a".to_owned(),
                    port_id: "out".to_owned(),
                },
                target: PortEndpoint {
                    node_id: "b".to_owned(),
                    port_id: "nonexistent".to_owned(),
                },
            }],
        };
        let result = GraphEngine::build(spec, &registry, EngineServices::default());
        assert!(matches!(
            result,
            Err(GraphBuildError::MissingPort(id, port, node))
                if id == "e" && port == "nonexistent" && node == "b"
        ));
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

    fn node_spec_with_kind(
        id: &str,
        kind: &str,
        inputs: Vec<PortSpec>,
        outputs: Vec<PortSpec>,
    ) -> NodeSpec {
        NodeSpec {
            id: id.to_owned(),
            kind: kind.to_owned(),
            title: id.to_owned(),
            inputs,
            outputs,
            config: serde_json::json!({}),
        }
    }

    #[test]
    fn packet_has_stable_port_kind() {
        let packet = DataPacket::Json(Arc::new(serde_json::json!({})));
        assert_eq!(packet.port_kind(), "status.metrics");
    }

    #[test]
    fn latest_output_is_none_before_emit() {
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![node_spec("a", vec![], vec![port("out", false)])],
            edges: vec![],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();
        // 尚未有任何 emit，最近输出应为 None。
        assert!(engine.latest_output("a").is_none());
        assert!(engine.latest_output("missing").is_none());
        let _ = engine;
    }

    /// 图级 1 连多（fan-out）：源 emitter → A、emitter → B，触发一次后 A/B 均产出。
    #[test]
    fn graph_fans_out_to_all_targets() {
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(EmitterFactory));
        registry.register(Box::new(PassthroughFactory));

        // emitter --out--> a --out--> (nothing) ； emitter --out--> b --out--> (nothing)。
        // 用 Passthrough 作下游并回灌 latest_output，触发后 A/B 均有最近输出。
        let spec = GraphSpec {
            nodes: vec![
                node_spec_with_kind("src", "emitter", vec![], vec![port("out", false)]),
                node_spec_with_kind(
                    "a",
                    "passthrough",
                    vec![port("in", false)],
                    vec![port("out", false)],
                ),
                node_spec_with_kind(
                    "b",
                    "passthrough",
                    vec![port("in", false)],
                    vec![port("out", false)],
                ),
            ],
            edges: vec![
                edge_with_ports("e1", "src", "out", "a", "in"),
                edge_with_ports("e2", "src", "out", "b", "in"),
            ],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();

        // 触发源：它向 "out" emit 一次，fan-out 到 a/b 两个下游 mailbox。
        engine
            .send_action("src", NodeAction::Trigger)
            .expect("trigger src");

        // actor 在线程里跑，短等待 + 轮询直到两个下游都回灌了最近输出。
        wait_until(
            || engine.latest_output("a").is_some() && engine.latest_output("b").is_some(),
            "fan-out targets A/B 均产出",
        );

        assert!(engine.latest_output("a").is_some(), "A 应收到 fan-out 数据");
        assert!(engine.latest_output("b").is_some(), "B 应收到 fan-out 数据");
    }

    /// 图级 多合1（fan-in）：A→t.image、B→t.video，Recorder 按端口区分收到的数据。
    #[test]
    fn graph_fans_in_distinct_ports() {
        let received: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(EmitterFactory));
        registry.register(Box::new(RecorderFactory {
            received: Arc::clone(&received),
        }));

        // src_a --image--> t ； src_b --video--> t 。t 是 Recorder，记录 (port, src) 对。
        let spec = GraphSpec {
            nodes: vec![
                node_spec_with_kind("src_a", "emitter", vec![], vec![port("image", false)]),
                node_spec_with_kind("src_b", "emitter", vec![], vec![port("video", false)]),
                node_spec_with_kind(
                    "t",
                    "recorder",
                    vec![
                        PortSpec {
                            id: "image".to_owned(),
                            label: "Image".to_owned(),
                            kind: "status.metrics".to_owned(),
                            cardinality: PortCardinality::Many,
                            required: false,
                        },
                        PortSpec {
                            id: "video".to_owned(),
                            label: "Video".to_owned(),
                            kind: "status.metrics".to_owned(),
                            cardinality: PortCardinality::Many,
                            required: false,
                        },
                    ],
                    vec![],
                ),
            ],
            edges: vec![
                edge_with_ports("e_img", "src_a", "image", "t", "image"),
                edge_with_ports("e_vid", "src_b", "video", "t", "video"),
            ],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();

        // 两个源各触发一次，分别经不同端口 fan-in 到同一个 Recorder。
        engine
            .send_action("src_a", NodeAction::Trigger)
            .expect("trigger src_a");
        engine
            .send_action("src_b", NodeAction::Trigger)
            .expect("trigger src_b");

        // Recorder 收到两个不同端口的数据，端口区分正确。
        wait_until(
            || received.lock().unwrap().len() >= 2,
            "fan-in 两个端口均到达",
        );

        let mut got = received.lock().unwrap().clone();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("image".to_owned(), "emitter".to_owned()),
                ("video".to_owned(), "emitter".to_owned()),
            ],
            "应按 image/video 端口区分两条 fan-in 数据"
        );
    }

    /// 短等待 + 轮询：actor 在独立线程跑，同步测试用固定窗口反复检查谓词，超时报错。
    fn wait_until(predicate: impl Fn() -> bool, what: &str) {
        for _ in 0..200 {
            if predicate() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timeout waiting for {what}");
    }

    /// 构造一条指定端口 id 的边（fan-out/fan-in 测试需要自定义端口命名）。
    fn edge_with_ports(
        id: &str,
        source: &str,
        source_port: &str,
        target: &str,
        target_port: &str,
    ) -> EdgeSpec {
        EdgeSpec {
            id: id.to_owned(),
            source: PortEndpoint {
                node_id: source.to_owned(),
                port_id: source_port.to_owned(),
            },
            target: PortEndpoint {
                node_id: target.to_owned(),
                port_id: target_port.to_owned(),
            },
        }
    }

    /// 构造指定 cardinality 的端口（kind 统一 status.metrics，便于 kind 匹配校验通过）。
    fn port_card(id: &str, cardinality: PortCardinality, required: bool) -> PortSpec {
        PortSpec {
            id: id.to_owned(),
            label: id.to_owned(),
            kind: "status.metrics".to_owned(),
            cardinality,
            required,
        }
    }

    #[test]
    fn build_rejects_cardinality_violation() {
        // One 输入端口收到第二条入边 → build 拒绝（D7）。
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![
                node_spec_with_kind("a1", "passthrough", vec![], vec![port("out", false)]),
                node_spec_with_kind("a2", "passthrough", vec![], vec![port("out", false)]),
                node_spec_with_kind("b", "passthrough", vec![port("in", true)], vec![]),
            ],
            edges: vec![edge("e1", "a1", "b"), edge("e2", "a2", "b")],
        };
        let result = GraphEngine::build(spec, &registry, EngineServices::default());
        let err = result.as_ref().err();
        assert!(
            matches!(err, Some(GraphBuildError::CardinalityViolation { edge, node, port })
                if *node == "b" && *port == "in" && (*edge == "e1" || *edge == "e2")),
            "unexpected result: {err:?}"
        );
    }

    #[test]
    fn add_edge_rejects_cardinality_violation_on_one_port() {
        // 增量 add_edge：One 输入端口已有入边时第二条被拒，且不改引擎。
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![
                node_spec_with_kind("a1", "passthrough", vec![], vec![port("out", false)]),
                node_spec_with_kind("a2", "passthrough", vec![], vec![port("out", false)]),
                node_spec_with_kind("b", "passthrough", vec![port("in", true)], vec![]),
            ],
            edges: vec![],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();

        engine
            .add_edge(edge("e1", "a1", "b"))
            .expect("first edge ok");
        let second = engine.add_edge(edge("e2", "a2", "b"));
        assert!(
            matches!(second, Err(GraphBuildError::CardinalityViolation { .. })),
            "second edge into One input must be rejected: {second:?}"
        );
    }

    #[test]
    fn add_edge_allows_multiple_sources_into_many_port() {
        // Many 输入端口允许 fan-in（多条入边）。
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![
                node_spec_with_kind("a1", "passthrough", vec![], vec![port("out", false)]),
                node_spec_with_kind("a2", "passthrough", vec![], vec![port("out", false)]),
                node_spec_with_kind(
                    "b",
                    "passthrough",
                    vec![port_card("in", PortCardinality::Many, false)],
                    vec![],
                ),
            ],
            edges: vec![],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();
        engine.add_edge(edge("e1", "a1", "b")).expect("first ok");
        engine
            .add_edge(edge("e2", "a2", "b"))
            .expect("second ok on Many input");
    }

    #[test]
    fn add_edge_rejects_cycle() {
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![
                node_spec_with_kind(
                    "a",
                    "passthrough",
                    vec![port("in", false)],
                    vec![port("out", false)],
                ),
                node_spec_with_kind(
                    "b",
                    "passthrough",
                    vec![port("in", false)],
                    vec![port("out", false)],
                ),
            ],
            edges: vec![edge("e1", "a", "b")],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();
        // b → a 会与既有的 a → b 成环。
        let cycle = engine.add_edge(edge_with_ports("e2", "b", "out", "a", "in"));
        assert!(
            matches!(cycle, Err(GraphBuildError::WouldCreateCycle { .. })),
            "b→a after a→b must be a cycle: {cycle:?}"
        );
    }

    #[test]
    fn add_edge_rejects_kind_mismatch() {
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(PassthroughFactory));
        // 源输出 kind=stream.video-frame，目标输入 kind=status.metrics → 不匹配。
        let source = NodeSpec {
            id: "a".to_owned(),
            kind: "passthrough".to_owned(),
            title: "a".to_owned(),
            inputs: vec![],
            outputs: vec![PortSpec {
                id: "out".to_owned(),
                label: "out".to_owned(),
                kind: "stream.video-frame".to_owned(),
                cardinality: PortCardinality::One,
                required: false,
            }],
            config: serde_json::json!({}),
        };
        let spec = GraphSpec {
            nodes: vec![
                source,
                node_spec_with_kind("b", "passthrough", vec![port("in", false)], vec![]),
            ],
            edges: vec![],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();
        let result = engine.add_edge(edge("e1", "a", "b"));
        assert!(
            matches!(result, Err(GraphBuildError::PortKindMismatch { .. })),
            "kind mismatch must be rejected: {result:?}"
        );
    }

    #[test]
    fn incremental_add_edge_remove_edge_drives_dataflow() {
        // 增量连边后可真实走数据流；断边后不再投递。
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(EmitterFactory));
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![
                node_spec_with_kind("src", "emitter", vec![], vec![port("out", false)]),
                node_spec_with_kind(
                    "b",
                    "passthrough",
                    vec![port("in", false)],
                    vec![port("out", false)],
                ),
            ],
            edges: vec![],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();

        // 初始未连边，触发无下游（latest_output("b") 仍为 None）。
        engine
            .send_action("src", NodeAction::Trigger)
            .expect("trigger");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(engine.latest_output("b").is_none(), "未连边时 b 不应有输出");

        // 连边后触发，数据流生效。
        engine
            .add_edge(edge_with_ports("e1", "src", "out", "b", "in"))
            .expect("add edge");
        engine
            .send_action("src", NodeAction::Trigger)
            .expect("trigger");
        wait_until(
            || engine.latest_output("b").is_some(),
            "data flows to b after add_edge",
        );
        assert!(engine.latest_output("b").is_some());

        // 断边后触发，b 不再有更新（latest_output 保留旧值，改用新触发验证无变化不现实，这里仅验证断边本身不报错且边被移除）。
        engine.remove_edge("e1").expect("remove edge");
        // 再触发，b 的 latest_output 仍为旧值，且不新增（由于无法清除，仅验证 remove_edge 幂等 + 二次 remove 报 MissingNode）。
        let again = engine.remove_edge("e1");
        assert!(
            matches!(again, Err(GraphBuildError::MissingNode(..))),
            "second remove must miss: {again:?}"
        );
    }

    #[test]
    fn incremental_add_remove_node_roundtrip() {
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(EmitterFactory));
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![node_spec_with_kind(
                "src",
                "emitter",
                vec![],
                vec![port("out", false)],
            )],
            edges: vec![],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();

        let new_node = node_spec_with_kind(
            "b",
            "passthrough",
            vec![port("in", false)],
            vec![port("out", false)],
        );
        engine.add_node(new_node, &registry).expect("add node");
        // 重复 add_node 被拒。
        let dup = engine.add_node(
            node_spec_with_kind("b", "passthrough", vec![], vec![]),
            &registry,
        );
        assert!(
            matches!(dup, Err(GraphBuildError::DuplicateNode(_))),
            "dup: {dup:?}"
        );

        // 连边 + 数据流。
        engine
            .add_edge(edge_with_ports("e1", "src", "out", "b", "in"))
            .expect("add edge");
        engine
            .send_action("src", NodeAction::Trigger)
            .expect("trigger");
        wait_until(
            || engine.latest_output("b").is_some(),
            "data flows after add_node+add_edge",
        );

        // remove_node：级联拆边 + 清输出 + 摘除。
        engine.remove_node("b").expect("remove node");
        let missing = engine.send_action("b", NodeAction::Trigger);
        assert!(
            matches!(missing, Err(NodeError::Precondition(_))),
            "b must be gone: {missing:?}"
        );
    }

    #[test]
    fn update_node_replaces_config() {
        let mut registry = NodeRegistry::new();
        registry.register(Box::new(PassthroughFactory));
        let spec = GraphSpec {
            nodes: vec![node_spec_with_kind("a", "passthrough", vec![], vec![])],
            edges: vec![],
        };
        let engine = GraphEngine::build(spec, &registry, EngineServices::default()).unwrap();
        engine
            .update_node("a", serde_json::json!({"url": "rtsp://x"}))
            .expect("update");
        engine
            .update_node("missing", serde_json::json!({}))
            .expect_err("missing node must err");
    }

    /// 端到端 viewer 显示帧率基准（离线 mock 解码器，忽略不随常规测试跑）：
    /// 模拟 FFmpeg 帧级多线程的突发输出（每周期 burst 帧）→ 5ms pump 轮询 →
    /// 节点链路 → viewer slot，最后按前端 250ms 轮询统计「显示帧率」。
    #[test]
    #[ignore = "端到端 viewer 显示帧率基准，需 4s，手动 --ignored 运行"]
    fn viewer_display_fps_end_to_end_benchmark() {
        use crate::platform::{DecodedVideoFrame, StreamFrameIdentity};
        use std::sync::atomic::Ordering;

        struct BurstMockService {
            slot: Arc<LatestDecodedFrameSlot>,
            fps: u64,
            burst: usize,
            cancel: Arc<AtomicBool>,
        }
        impl StreamService for BurstMockService {
            fn service_id(&self) -> &str {
                "burst-mock"
            }
            fn open(
                &self,
                session_id: crate::platform::StreamSessionId,
                _request: crate::platform::StreamOpenRequest,
                control: crate::platform::StreamOperationControl,
            ) -> Result<crate::platform::StreamSession, crate::platform::StreamServiceError>
            {
                let slot = Arc::clone(&self.slot);
                let fps = self.fps;
                let burst = self.burst;
                let cancel = Arc::clone(&self.cancel);
                let producer_session = session_id.clone();
                std::thread::spawn(move || {
                    let mut seq = 0u64;
                    while !cancel.load(Ordering::Relaxed) {
                        for _ in 0..burst {
                            seq += 1;
                            slot.publish(DecodedVideoFrame {
                                width: 64,
                                height: 64,
                                rgba: Arc::from(vec![0u8; 64 * 64 * 4]),
                                identity: StreamFrameIdentity::unavailable(
                                    producer_session.clone(),
                                    0,
                                    seq,
                                    "bench",
                                ),
                            });
                        }
                        std::thread::sleep(std::time::Duration::from_nanos(1_000_000_000 / fps));
                    }
                });
                Ok(crate::platform::StreamSession::new(
                    session_id,
                    Arc::clone(&self.slot),
                    control,
                ))
            }
        }
        struct BurstFactory {
            slot: Arc<LatestDecodedFrameSlot>,
            fps: u64,
            burst: usize,
            cancel: Arc<AtomicBool>,
        }
        impl StreamServiceFactory for BurstFactory {
            fn create(&self, _config: crate::platform::RtspStreamConfig) -> Arc<dyn StreamService> {
                Arc::new(BurstMockService {
                    slot: Arc::clone(&self.slot),
                    fps: self.fps,
                    burst: self.burst,
                    cancel: Arc::clone(&self.cancel),
                })
            }
        }

        let mut registry = NodeRegistry::new();
        crate::engine::register_builtin(&mut registry);
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let producer_cancel = Arc::new(AtomicBool::new(false));
        let services = EngineServices {
            stream_factory: Some(Arc::new(BurstFactory {
                slot,
                fps: 60,
                burst: 3,
                cancel: Arc::clone(&producer_cancel),
            })),
            ..EngineServices::default()
        };

        // rtspSource → videoLayer → viewer 链路（端口 id 与 web 图一致）。
        let spec = GraphSpec {
            nodes: vec![
                NodeSpec {
                    id: "rtsp".to_owned(),
                    kind: "rtspSource".to_owned(),
                    title: "RTSP".to_owned(),
                    inputs: vec![],
                    outputs: vec![PortSpec {
                        id: "endpoint".to_owned(),
                        label: "Endpoint".to_owned(),
                        kind: "stream.video-frame".to_owned(),
                        cardinality: PortCardinality::One,
                        required: false,
                    }],
                    config: serde_json::json!({"url": "rtsp://bench/test", "transport": "tcp"}),
                },
                NodeSpec {
                    id: "layer".to_owned(),
                    kind: "videoLayer".to_owned(),
                    title: "Layer".to_owned(),
                    inputs: vec![PortSpec {
                        id: "frames".to_owned(),
                        label: "Frames".to_owned(),
                        kind: "stream.video-frame".to_owned(),
                        cardinality: PortCardinality::One,
                        required: false,
                    }],
                    outputs: vec![PortSpec {
                        id: "layer".to_owned(),
                        label: "Layer".to_owned(),
                        kind: "viewer.layer.video.v1".to_owned(),
                        cardinality: PortCardinality::One,
                        required: false,
                    }],
                    config: serde_json::json!({}),
                },
                NodeSpec {
                    id: "viewer".to_owned(),
                    kind: "viewer".to_owned(),
                    title: "Viewer".to_owned(),
                    inputs: vec![PortSpec {
                        id: "video".to_owned(),
                        label: "Video".to_owned(),
                        kind: "viewer.layer.video.v1".to_owned(),
                        cardinality: PortCardinality::One,
                        required: false,
                    }],
                    outputs: vec![],
                    config: serde_json::json!({}),
                },
            ],
            edges: vec![
                EdgeSpec {
                    id: "e1".to_owned(),
                    source: PortEndpoint {
                        node_id: "rtsp".to_owned(),
                        port_id: "endpoint".to_owned(),
                    },
                    target: PortEndpoint {
                        node_id: "layer".to_owned(),
                        port_id: "frames".to_owned(),
                    },
                },
                EdgeSpec {
                    id: "e2".to_owned(),
                    source: PortEndpoint {
                        node_id: "layer".to_owned(),
                        port_id: "layer".to_owned(),
                    },
                    target: PortEndpoint {
                        node_id: "viewer".to_owned(),
                        port_id: "video".to_owned(),
                    },
                },
            ],
        };

        let engine = GraphEngine::build(spec, &registry, services).expect("build");
        engine
            .send_action("rtsp", NodeAction::Connect)
            .expect("connect rtsp");

        // 预热 500ms 让链路稳定。
        std::thread::sleep(std::time::Duration::from_millis(500));
        let start = std::time::Instant::now();
        let measure_secs = std::time::Duration::from_secs(4);
        let mut displayed = 0u64;
        let mut last_seq = 0u64;
        let mut engine_frames = 0u64;
        while start.elapsed() < measure_secs {
            std::thread::sleep(std::time::Duration::from_millis(250)); // 前端 ViewerNode 轮询周期
            if let Some(frame) = engine.viewer_frame("viewer") {
                engine_frames += 1;
                if frame.identity.frame_sequence != last_seq {
                    last_seq = frame.identity.frame_sequence;
                    displayed += 1;
                }
            }
        }
        let producer_total = producer_total_from_seq(&engine);
        producer_cancel.store(true, Ordering::Release);
        let _ = producer_total;
        eprintln!(
            "viewer benchmark: 60fps burst=3 解码 → 4s 内前端 250ms 轮询实际显示新帧 {displayed} 帧 = {:.1} fps；轮询命中引擎侧帧 {engine_frames} 次",
            displayed as f64 / 4.0
        );
        assert!(
            displayed <= 16,
            "250ms 轮询 4s 最多显示 16 帧，实际 {displayed}"
        );
    }

    /// 从引擎侧最新帧序列近似生产者总数（帧序号单调递增）。
    fn producer_total_from_seq(engine: &GraphEngine) -> u64 {
        engine
            .viewer_frame("viewer")
            .map(|frame| frame.identity.frame_sequence)
            .unwrap_or(0)
    }
}
