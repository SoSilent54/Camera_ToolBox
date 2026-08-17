import { useCallback, useEffect, useMemo, useRef, useState, type DragEvent, type MouseEvent as ReactMouseEvent } from 'react';
import {
  Background,
  BackgroundVariant,
  Controls,
  MiniMap,
  Panel,
  ReactFlow,
  addEdge,
  useEdgesState,
  useNodesState,
  type Connection,
  type Edge,
  type EdgeChange,
  type Node,
  type NodeChange,
  type OnSelectionChangeParams,
  type ReactFlowInstance,
} from '@xyflow/react';
import {
  WORKFLOW_SCHEMA_VERSION,
  deleteWorkflow,
  importWorkflow,
  labelForPortKind,
  listWorkflows,
  loadNodeCatalog,
  loadSavedWorkflow,
  loadWorkflow,
  loadWorkmodeTemplates,
  saveWorkflow,
  validateConnectionKinds,
  type FlowEdgeData,
  type FlowNodeData,
  type NodeDefinition,
  type NodeKind,
  type PortKind,
  type WorkflowEdge,
  type WorkflowGraph,
  type WorkflowNode,
  type WorkflowPort,
  type WorkmodeTemplate,
} from './workflow';
import {
  LocalFileSourceNode,
  SftpFileSourceNode,
  SshSessionNode,
  X5DeviceNode,
} from './WorkflowNodes';
import {
  AutoCaptureNode,
  CalibrationSolverNode,
  GenericWorkflowNode,
  NodeLibraryItem,
  RtspSourceNode,
  ViewerNode,
} from './nodes';
import { Console } from './Console';
import { useEngine } from './useEngine';
import { subscribeSnapshot, subscribeTopic } from './useEngineSocket';

type FlowNode = Node<FlowNodeData>;
type FlowEdge = Edge<FlowEdgeData>;
export type Selection =
  | { type: 'node'; node: WorkflowNode }
  | { type: 'edge'; edge: FlowEdge }
  | { type: 'none' };
const DEFAULT_RTSP_URL = 'rtsp://10.21.12.108:554/PRR';
const DND_NODE_KIND = 'application/x-camera-toolbox-node-kind';
const GENERIC_NODE_KINDS: NodeKind[] = [
  'rtspDecoder',
  'frameSampler',
  'imageLayer',
  'videoLayer',
  'overlayComposer',
  'chessboardDetector',
  'datasetCollector',
  'coverageAnalyzer',
  'poseGuide',
  'i2cTransfer',
  'eepromProvision',
];

const nodeTypes = Object.fromEntries([
  ['rtspSource', RtspSourceNode],
  ['localFileSource', LocalFileSourceNode],
  ['sftpFileSource', SftpFileSourceNode],
  ['sshSession', SshSessionNode],
  ['x5Device', X5DeviceNode],
  ['viewer', ViewerNode],
  ['calibrationSolver', CalibrationSolverNode],
  ['autoCaptureController', AutoCaptureNode],
  ...GENERIC_NODE_KINDS.map((kind) => [kind, GenericWorkflowNode]),
]);

export function App() {


  const [graph, setGraph] = useState<WorkflowGraph | null>(null);
  const [catalog, setCatalog] = useState<NodeDefinition[]>([]);
  const [templates, setTemplates] = useState<WorkmodeTemplate[]>([]);
  const [savedWorkflows, setSavedWorkflows] = useState<Array<{ id: string; title: string; revision: string }>>([]);
  const [nodes, setNodes, onNodesChangeBase] = useNodesState<FlowNode>([]);
  const [edges, setEdges, onEdgesChangeBase] = useEdgesState<FlowEdge>([]);
  const [selection, setSelection] = useState<Selection>({ type: 'none' });
  const [events, setEvents] = useState<string[]>(['等待 Workflow API...']);
  const { nodeStates, loadGraph, startAll, sendAction } = useEngine();
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number; nodeId: string } | null>(null);
  const flowInstanceRef = useRef<ReactFlowInstance<FlowNode, FlowEdge> | null>(null);
  const [collapsedCategories, setCollapsedCategories] = useState<Set<string>>(new Set());
  const [marquee, setMarquee] = useState<{ x: number; y: number; width: number; height: number } | null>(null);
  const [historyVersion, setHistoryVersion] = useState(0);
  const canvasRegionRef = useRef<HTMLElement | null>(null);
  const marqueeRef = useRef({ startX: 0, startY: 0, curX: 0, curY: 0, active: false });
  const pastRef = useRef<Array<{ nodes: FlowNode[]; edges: FlowEdge[] }>>([]);
  const futureRef = useRef<Array<{ nodes: FlowNode[]; edges: FlowEdge[] }>>([]);
  const nodesRef = useRef<FlowNode[]>(nodes);
  const edgesRef = useRef<FlowEdge[]>(edges);

  const [runtimeGraph, setRuntimeGraph] = useState<{ id: string; revision: string } | null>(null);
  const [draftDirtyFromRuntime, setDraftDirtyFromRuntime] = useState(false);

  /** 编辑图与运行图分离：普通画布编辑只标记 draft dirty，不隐式改运行中的 GraphEngine。 */
  const markDraftChanged = useCallback(() => {
    setDraftDirtyFromRuntime((current) => current || runtimeGraph !== null);
  }, [runtimeGraph]);
  const pushEvent = useCallback((event: string) => {
    setEvents((current) => [event, ...current].slice(0, 10));
  }, []);


  // 订阅引擎真实事件（event topic 推送），叠加到 Console；本地 pushEvent 保留作回退。
  useEffect(() => {
    return subscribeTopic('event', (payload) => {
      const event = payload as { nodeId?: string; message?: string } | null;
      if (!event || typeof event.message !== 'string') {
        return;
      }
      const prefix = typeof event.nodeId === 'string' && event.nodeId ? `[${event.nodeId}] ` : '';
      pushEvent(`${prefix}${event.message}`);
    });
  }, [pushEvent]);
  // 撤销/恢复：nodes/edges 快照同步 + 历史栈
  useEffect(() => {
    nodesRef.current = nodes;
  }, [nodes]);

  useEffect(() => {
    edgesRef.current = edges;
  }, [edges]);

  const recordSnapshot = useCallback(() => {
    markDraftChanged();
    // 深拷贝快照：React Flow 拖拽时 onNodesChange 会原位 mutate node 对象（position 字段），
    // 若直接存 nodesRef.current 引用，撤销栈会与后续 state 串味。这里至少复制 position 与
    // workflowNode；workflowNode.config 也可能被后续 setNodes 原地替换的浅层引用共享，
    // 一并复制以避免撤销后 config 被污染。
    pastRef.current.push({
      nodes: nodesRef.current.map((node) => ({
        ...node,
        position: { ...node.position },
        data: {
          ...node.data,
          workflowNode: {
            ...node.data.workflowNode,
            config: { ...node.data.workflowNode.config },
          },
        },
      })),
      edges: edgesRef.current.map((edge) => ({ ...edge })),
    });
    if (pastRef.current.length > 50) {
      pastRef.current.shift();
    }
    futureRef.current = [];
    setHistoryVersion((version) => version + 1);
  }, [markDraftChanged]);

  const undo = useCallback(() => {
    const snapshot = pastRef.current.pop();
    if (!snapshot) {
      return;
    }
    futureRef.current.push({ nodes: nodesRef.current, edges: edgesRef.current });
    markDraftChanged();
    setNodes(snapshot.nodes);
    setEdges(snapshot.edges);
    setSelection({ type: 'none' });
    setHistoryVersion((version) => version + 1);
    pushEvent('已撤销');
  }, [pushEvent, setEdges, setNodes]);

  const redo = useCallback(() => {
    const snapshot = futureRef.current.pop();
    if (!snapshot) {
      return;
    }
    pastRef.current.push({ nodes: nodesRef.current, edges: edgesRef.current });
    markDraftChanged();
    setNodes(snapshot.nodes);
    setEdges(snapshot.edges);
    setSelection({ type: 'none' });
    setHistoryVersion((version) => version + 1);
    pushEvent('已恢复');
  }, [pushEvent, setEdges, setNodes]);

  const toggleCategory = useCallback((category: string) => {
    setCollapsedCategories((current) => {
      const next = new Set(current);
      if (next.has(category)) {
        next.delete(category);
      } else {
        next.add(category);
      }
      return next;
    });
  }, []);

  const onNodesChange = useCallback((changes: NodeChange<FlowNode>[]) => {
    if (changes.length > 0) {
      markDraftChanged();
    }
    onNodesChangeBase(changes);
  }, [markDraftChanged, onNodesChangeBase]);

  const onEdgesChange = useCallback((changes: EdgeChange<FlowEdge>[]) => {
    if (changes.length > 0) {
      markDraftChanged();
    }
    onEdgesChangeBase(changes);
  }, [markDraftChanged, onEdgesChangeBase]);

  const applyGraph = useCallback(
    (nextGraph: WorkflowGraph, event: string) => {
      setGraph(nextGraph);
      setNodes(toFlowNodes(nextGraph));
      setEdges(toFlowEdges(nextGraph));
      setSelection({ type: 'none' });
      setDraftDirtyFromRuntime(runtimeGraph !== null);
      setEvents([event, `节点 ${nextGraph.nodes.length} 个，连接 ${nextGraph.edges.length} 条`, '仅更新编辑图；点击 Run 才会应用到运行图']);
    },
    [runtimeGraph, setEdges, setNodes],
  );

  // WS 重连后后端重推 snapshot。若本地草稿已偏离运行图，保留编辑图，避免旧运行图覆盖未应用修改。
  useEffect(() => {
    return subscribeSnapshot((snapshot) => {
      const nextGraph = snapshot.payload.graph as WorkflowGraph | undefined;
      if (!nextGraph || !Array.isArray(nextGraph.nodes) || !Array.isArray(nextGraph.edges)) {
        return;
      }
      setRuntimeGraph({ id: nextGraph.id, revision: nextGraph.revision });
      if (draftDirtyFromRuntime) {
        pushEvent('收到运行图快照；当前编辑图未应用，已保留本地草稿');
        return;
      }
      setGraph(nextGraph);
      setNodes(toFlowNodes(nextGraph));
      setEdges(toFlowEdges(nextGraph));
      setSelection({ type: 'none' });
      pushEvent('已从运行图快照对齐编辑图');
    });
  }, [draftDirtyFromRuntime, pushEvent, setEdges, setNodes]);

  const refreshSavedWorkflows = useCallback(() => {
    listWorkflows()
      .then((workflows) => setSavedWorkflows(workflows))
      .catch((error: unknown) => pushEvent(`工作流列表失败：${error instanceof Error ? error.message : String(error)}`));
  }, [pushEvent]);

  const loadSeedWorkflow = useCallback(() => {
    loadWorkflow()
      .then((loaded) => applyGraph(loaded, `已加载 ${loaded.title}`))
      .catch((error: unknown) => setEvents([`加载失败：${error instanceof Error ? error.message : String(error)}`]));
  }, [applyGraph]);

  useEffect(() => {
    loadSeedWorkflow();
    refreshSavedWorkflows();
    Promise.all([loadNodeCatalog(), loadWorkmodeTemplates()])
      .then(([loadedCatalog, loadedTemplates]) => {
        setCatalog(loadedCatalog);
        setTemplates(loadedTemplates);
      })
      .catch((error: unknown) => pushEvent(`节点目录加载失败：${error instanceof Error ? error.message : String(error)}`));
  }, [loadSeedWorkflow, pushEvent, refreshSavedWorkflows]);

  const nodeById = useMemo(() => {
    const map = new Map<string, WorkflowNode>();
    nodes.forEach((node) => map.set(node.id, node.data.workflowNode));
    return map;
  }, [nodes]);

  const catalogByKind = useMemo(() => new Map(catalog.map((definition) => [definition.kind, definition])), [catalog]);

  const runtimeNodeStates = useMemo(
    () => new Map<string, string>(Object.entries(nodeStates)),
    [nodeStates],
  );

  const displayedEdges = useMemo(
    () => decorateFlowEdges(edges, nodes, runtimeNodeStates),
    [edges, nodes, runtimeNodeStates],
  );

  const canConnect = useCallback(
    (connection: Connection): { ok: true; port: WorkflowPort } | { ok: false; reason: string } => {
      if (!connection.source || !connection.target || !connection.sourceHandle || !connection.targetHandle) {
        return { ok: false, reason: '需要从输出端口连接到输入端口' };
      }
      if (connection.source === connection.target) {
        return { ok: false, reason: '暂不支持节点自连接' };
      }
      const source = nodeById.get(connection.source);
      const target = nodeById.get(connection.target);
      if (!source || !target) {
        return { ok: false, reason: '连接端点不存在' };
      }
      const sourcePort = source.outputs.find((port) => port.id === connection.sourceHandle);
      const targetPort = target.inputs.find((port) => port.id === connection.targetHandle);
      if (!sourcePort || !targetPort) {
        return { ok: false, reason: '必须从输出端口连接到输入端口' };
      }
      const reason = validateConnectionKinds(sourcePort, targetPort);
      if (reason) {
        return { ok: false, reason };
      }
      // D7 cardinality 预检：One 输入端口只允许一条入边；Many 输入允许多入边；输出不限。
      if (targetPort.cardinality === 'one') {
        const alreadyConnected = edges.some(
          (edge) => edge.target === connection.target && edge.targetHandle === connection.targetHandle,
        );
        if (alreadyConnected) {
          return { ok: false, reason: `输入端口 ${targetPort.label} 只允许一条连接（cardinality=One）` };
        }
      }
      return { ok: true, port: sourcePort };
    },
    [edges, nodeById],
  );

  const onConnect = useCallback(
    (connection: Connection) => {
      const validation = canConnect(connection);
      if (!validation.ok) {
        pushEvent(`拒绝连接：${validation.reason}`);
        return;
      }
      recordSnapshot();
      const edgeId = `edge-${connection.source}-${connection.sourceHandle}-${connection.target}-${connection.targetHandle}`;
      const workflowEdge: WorkflowEdge = {
        id: edgeId,
        source: { nodeId: String(connection.source), portId: String(connection.sourceHandle) },
        target: { nodeId: String(connection.target), portId: String(connection.targetHandle) },
        kind: validation.port.kind,
        schema: validation.port.schema,
        schemaVersion: WORKFLOW_SCHEMA_VERSION,
      };
      setEdges((current) => {
        const nextEdges = addEdge(
          {
            ...connection,
            id: edgeId,
            animated: false,
            label: labelForPortKind(validation.port.kind),
            data: { workflowEdge, kind: validation.port.kind, schema: validation.port.schema, schemaVersion: WORKFLOW_SCHEMA_VERSION },
            className: 'workflow-edge flow-inactive',
          },
          current.filter((edge) => edge.id !== edgeId),
        );
        setNodes((currentNodes) => withViewerPreviews(currentNodes, nextEdges));
        return nextEdges;
      });
      pushEvent(`新增连接：${edgeId}（编辑图未应用）`);
    },
    [canConnect, pushEvent, recordSnapshot, setEdges, setNodes],
  );

  const handleRtspUrlChange = useCallback(
    (nodeId: string, nextUrl: string) => {
      const trimmedUrl = nextUrl.trim();
      if (!trimmedUrl.startsWith('rtsp://') && !trimmedUrl.startsWith('rtsps://')) {
        pushEvent('拒绝 RTSP URL：必须使用 rtsp:// 或 rtsps://');
        return;
      }
      recordSnapshot();
      setNodes((current) => {
        const updated = current.map((flowNode) => {
          if (flowNode.id !== nodeId) {
            return flowNode;
          }
          const workflowNode = flowNode.data.workflowNode;
          return {
            ...flowNode,
            data: {
              ...flowNode.data,
              workflowNode: {
                ...workflowNode,
                config: { ...workflowNode.config, url: trimmedUrl },
              },
            },
          };
        });
        return withViewerPreviews(updated, edges);
      });
      setSelection((current) => current.type === 'node' && current.node.id === nodeId
        ? { type: 'node', node: { ...current.node, config: { ...current.node.config, url: trimmedUrl } } }
        : current);
      pushEvent(`RTSP URL 已更新：${trimmedUrl}（编辑图未应用）`);
    },
    [edges, pushEvent, recordSnapshot, setNodes],
  );

  const handleNodeTitleChange = useCallback(
    (nodeId: string, nextTitle: string) => {
      const title = nextTitle.trim();
      if (!title) {
        pushEvent('节点标题不能为空');
        return;
      }
      recordSnapshot();
      setNodes((current) => current.map((flowNode) => flowNode.id === nodeId
        ? { ...flowNode, data: { ...flowNode.data, workflowNode: { ...flowNode.data.workflowNode, title } } }
        : flowNode));
      setSelection((current) => current.type === 'node' && current.node.id === nodeId
        ? { type: 'node', node: { ...current.node, title } }
        : current);
      pushEvent(`节点已重命名：${title}（编辑图未应用）`);
    },
    [pushEvent, recordSnapshot, setNodes],
  );

  const handleNodeConfigChange = useCallback(
    (nodeId: string, key: string, value: string | boolean) => {
      recordSnapshot();
      setNodes((current) => {
        const updated = current.map((flowNode) => flowNode.id === nodeId
          ? {
            ...flowNode,
            data: {
              ...flowNode.data,
              workflowNode: {
                ...flowNode.data.workflowNode,
                config: { ...flowNode.data.workflowNode.config, [key]: value },
              },
            },
          }
          : flowNode);
        return withViewerPreviews(updated, edges);
      });
      setSelection((current) => current.type === 'node' && current.node.id === nodeId
        ? { type: 'node', node: { ...current.node, config: { ...current.node.config, [key]: value } } }
        : current);
      pushEvent(`节点配置已更新：${key}（编辑图未应用）`);
    },
    [edges, pushEvent, recordSnapshot, setNodes],
  );

  useEffect(() => {
    setNodes((current) => current.map((flowNode) => {
      const runtimeState = nodeStates[flowNode.id];
      if (
        flowNode.data.onRtspUrlChange === handleRtspUrlChange
        && flowNode.data.onNodeConfigChange === handleNodeConfigChange
        && flowNode.data.onNodeAction === sendAction
        && flowNode.data.runtimeState === runtimeState
      ) {
        return flowNode;
      }
      return {
        ...flowNode,
        data: {
          ...flowNode.data,
          runtimeState,
          onRtspUrlChange: handleRtspUrlChange,
          onNodeConfigChange: handleNodeConfigChange,
          onNodeAction: sendAction,
        },
      };
    }));
  }, [handleNodeConfigChange, handleRtspUrlChange, nodeStates, nodes.length, sendAction, setNodes]);

  const createFlowNodeAt = useCallback(
    (kind: NodeKind, position: { x: number; y: number }): FlowNode => {
      const definition = catalogByKind.get(kind);
      const count = nodes.filter((node) => node.data.workflowNode.kind === kind).length + 1;
      return toFlowNode(createWorkflowNode(kind, count, position, definition));
    },
    [catalogByKind, nodes],
  );

  const insertFlowNode = useCallback(
    (flowNode: FlowNode, source: 'click' | 'drag') => {
      recordSnapshot();
      setNodes((current) => withViewerPreviews([...current, flowNode], edges));
      setSelection({ type: 'node', node: flowNode.data.workflowNode });
      pushEvent(`${source === 'drag' ? '拖入' : '新增'}节点：${flowNode.data.workflowNode.title}（编辑图未应用）`);
    },
    [edges, pushEvent, recordSnapshot, setNodes],
  );

  const handleAddNode = useCallback(
    (kind: NodeKind) => {
      const viewportCenter = flowInstanceRef.current?.screenToFlowPosition({ x: window.innerWidth * 0.5, y: window.innerHeight * 0.5 })
        ?? { x: 96 + (nodes.length % 4) * 56, y: 96 + nodes.length * 36 };
      insertFlowNode(createFlowNodeAt(kind, viewportCenter), 'click');
    },
    [createFlowNodeAt, insertFlowNode, nodes.length],
  );

  const handleDragNodeStart = useCallback((event: DragEvent<HTMLElement>, kind: NodeKind) => {
    event.dataTransfer.setData(DND_NODE_KIND, kind);
    event.dataTransfer.effectAllowed = 'copy';
  }, []);

  const handleDragNodeOver = useCallback((event: DragEvent<HTMLElement>) => {
    // 无条件 preventDefault：这是 React Flow 官方的拖放写法，避免依赖
    // DataTransfer.types.includes 在不同浏览器（DOMStringList vs FrozenArray）上的差异。
    // 真正校验（kind 是否来自节点库）放在 handleDropNode 里做。
    event.preventDefault();
    event.dataTransfer.dropEffect = 'copy';
  }, []);

  const handleDropNode = useCallback(
    (event: DragEvent<HTMLElement>) => {
      const kind = event.dataTransfer.getData(DND_NODE_KIND) as NodeKind;
      if (!kind || !catalogByKind.has(kind)) {
        return;
      }
      event.preventDefault();
      const position = flowInstanceRef.current?.screenToFlowPosition({ x: event.clientX, y: event.clientY })
        ?? { x: event.clientX, y: event.clientY };
      insertFlowNode(createFlowNodeAt(kind, position), 'drag');
    },
    [catalogByKind, createFlowNodeAt, insertFlowNode],
  );

  const handleApplyTemplate = useCallback(
    (template: WorkmodeTemplate) => {
      applyGraph({ ...template.graph, id: createGraphId(), revision: 'draft' }, `已插入模板：${template.title}`);
    },
    [applyGraph],
  );

  const handleDeleteSelection = useCallback(() => {
    if (selection.type === 'none') {
      pushEvent('没有选中的节点或连线');
      return;
    }
    recordSnapshot();
    if (selection.type === 'edge') {
      const edgeId = selection.edge.id;
      const nextEdges = edges.filter((edge) => edge.id !== edgeId);
      setEdges(nextEdges);
      setNodes((current) => withViewerPreviews(current, nextEdges));
      setSelection({ type: 'none' });
      pushEvent(`删除连线：${edgeId}（编辑图未应用）`);
      return;
    }
    const nodeId = selection.node.id;
    const nextEdges = edges.filter((edge) => edge.source !== nodeId && edge.target !== nodeId);
    setEdges(nextEdges);
    setNodes((current) => withViewerPreviews(current.filter((node) => node.id !== nodeId), nextEdges));
    setSelection({ type: 'none' });
    pushEvent(`删除节点：${selection.node.title}（编辑图未应用）`);
  }, [edges, pushEvent, recordSnapshot, selection, setEdges, setNodes]);

  const handleDuplicateSelection = useCallback(() => {
    if (selection.type !== 'node') {
      pushEvent('只有节点可以复制');
      return;
    }
    const source = nodes.find((node) => node.id === selection.node.id);
    if (!source) {
      pushEvent('选中节点不存在');
      return;
    }
    recordSnapshot();
    const duplicated: WorkflowNode = {
      ...source.data.workflowNode,
      id: createNodeId(source.data.workflowNode.kind),
      title: `${source.data.workflowNode.title} Copy`,
      position: { x: source.position.x + 48, y: source.position.y + 48 },
      config: { ...source.data.workflowNode.config },
    };
    setNodes((current) => withViewerPreviews([...current, toFlowNode(duplicated)], edges));
    setSelection({ type: 'node', node: duplicated });
    pushEvent(`复制节点：${duplicated.title}（编辑图未应用）`);
  }, [edges, nodes, pushEvent, recordSnapshot, selection, setNodes]);

  const handleSaveWorkflow = useCallback(() => {
    const draft = toWorkflowGraph(nodes, edges, graph ?? emptyWorkflowGraph());
    saveWorkflow(draft)
      .then((saved) => {
        setGraph(saved);
        refreshSavedWorkflows();
        pushEvent(`工作流已保存：${saved.id} @ ${saved.revision}`);
      })
      .catch((error: unknown) => pushEvent(`保存失败：${error instanceof Error ? error.message : String(error)}`));
  }, [edges, graph, nodes, pushEvent, refreshSavedWorkflows]);

  const handleRunGraph = useCallback(() => {
    const draft = toWorkflowGraph(nodes, edges, graph ?? emptyWorkflowGraph());
    loadGraph(draft)
      .then(() => startAll())
      .then(() => {
        setGraph(draft);
        setRuntimeGraph({ id: draft.id, revision: draft.revision });
        setDraftDirtyFromRuntime(false);
        pushEvent(`运行图已应用并启动：${draft.title}`);
      })
      .catch((error: unknown) => pushEvent(`运行失败：${error instanceof Error ? error.message : String(error)}`));
  }, [edges, graph, loadGraph, nodes, pushEvent, startAll]);

  const handleImportWorkflow = useCallback(() => {
    const raw = window.prompt('粘贴 .ctworkflow.json 内容');
    if (!raw) {
      return;
    }
    let parsed: WorkflowGraph;
    try {
      parsed = JSON.parse(raw) as WorkflowGraph;
    } catch (error) {
      pushEvent(`导入失败：${error instanceof Error ? error.message : String(error)}`);
      return;
    }
    importWorkflow(parsed)
      .then((imported) => {
        applyGraph(imported, `已导入工作流：${imported.title}`);
        refreshSavedWorkflows();
      })
      .catch((error: unknown) => pushEvent(`导入失败：${error instanceof Error ? error.message : String(error)}`));
  }, [applyGraph, pushEvent, refreshSavedWorkflows]);

  const handleExportWorkflow = useCallback(() => {
    const draft = graph ? toWorkflowGraph(nodes, edges, graph) : emptyWorkflowGraph();
    const blob = new Blob([`${JSON.stringify(draft, null, 2)}\n`], { type: 'application/json;charset=utf-8' });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement('a');
    anchor.href = url;
    anchor.download = `${draft.id}.ctworkflow.json`;
    anchor.click();
    URL.revokeObjectURL(url);
    pushEvent(`已导出当前编辑图：${draft.id}`);
  }, [edges, graph, nodes, pushEvent]);

  const handleLoadSavedWorkflow = useCallback(() => {
    const first = savedWorkflows[0];
    if (!first) {
      pushEvent('没有已保存工作流');
      return;
    }
    loadSavedWorkflow(first.id)
      .then((loaded) => applyGraph(loaded, `已载入保存工作流：${loaded.title}`))
      .catch((error: unknown) => pushEvent(`载入失败：${error instanceof Error ? error.message : String(error)}`));
  }, [applyGraph, pushEvent, savedWorkflows]);

  const handleNewWorkflow = useCallback(() => {
    applyGraph(emptyWorkflowGraph(), '已新建空白工作流');
  }, [applyGraph]);

  const handleFitView = useCallback(() => {
    void flowInstanceRef.current?.fitView({ padding: 0.2, duration: 180 });
    pushEvent('画布已适配视图');
  }, [pushEvent]);
  const handleNodeDragStart = useCallback(() => {
    recordSnapshot();
  }, [recordSnapshot]);

  const handleNodeContextMenu = useCallback((event: ReactMouseEvent, node: FlowNode) => {
    event.preventDefault();
    setContextMenu({ x: event.clientX, y: event.clientY, nodeId: node.id });
  }, []);

  const handleContextMenuAction = useCallback((action: 'rename' | 'duplicate' | 'delete') => {
    setContextMenu((current) => {
      if (!current) {
        return null;
      }
      const nodeId = current.nodeId;
      const node = nodesRef.current.find((candidate) => candidate.id === nodeId);
      if (!node) {
        return null;
      }
      const workflowNode = node.data.workflowNode;
      if (action === 'rename') {
        const title = window.prompt('节点重命名', workflowNode.title);
        if (title && title.trim()) {
          handleNodeTitleChange(nodeId, title);
        }
      } else if (action === 'delete') {
        recordSnapshot();
        const removedEdges = edgesRef.current.filter((edge) => edge.source === nodeId || edge.target === nodeId);
        const nextEdges = edgesRef.current.filter((edge) => edge.source !== nodeId && edge.target !== nodeId);
        setEdges(nextEdges);
        setNodes((currentNodes) => withViewerPreviews(currentNodes.filter((candidate) => candidate.id !== nodeId), nextEdges));
        setSelection({ type: 'none' });
        pushEvent(`删除节点：${workflowNode.title}（编辑图未应用）`);
      } else if (action === 'duplicate') {
        recordSnapshot();
        const duplicated = {
          ...workflowNode,
          id: createNodeId(workflowNode.kind),
          title: `${workflowNode.title} Copy`,
          position: { x: node.position.x + 48, y: node.position.y + 48 },
          config: { ...workflowNode.config },
        };
        setNodes((currentNodes) => withViewerPreviews([...currentNodes, toFlowNode(duplicated)], edgesRef.current));
        setSelection({ type: 'node', node: duplicated });
        pushEvent(`复制节点：${duplicated.title}（编辑图未应用）`);
      }
      return null;
    });
  }, [handleNodeTitleChange, pushEvent, recordSnapshot, setEdges, setNodes]);

  const handleCanvasContextMenu = useCallback((event: ReactMouseEvent) => {
    event.preventDefault();
  }, []);

  const handleCanvasMouseDown = useCallback((event: ReactMouseEvent) => {
    if (event.button !== 2) {
      return;
    }
    const bounds = canvasRegionRef.current?.getBoundingClientRect();
    if (!bounds) {
      return;
    }
    const startX = event.clientX - bounds.left;
    const startY = event.clientY - bounds.top;
    marqueeRef.current = { startX, startY, curX: startX, curY: startY, active: true };
    setMarquee({ x: startX, y: startY, width: 0, height: 0 });
  }, []);

  const handleCanvasMouseMove = useCallback((event: ReactMouseEvent) => {
    const current = marqueeRef.current;
    if (!current.active) {
      return;
    }
    const bounds = canvasRegionRef.current?.getBoundingClientRect();
    if (!bounds) {
      return;
    }
    current.curX = event.clientX - bounds.left;
    current.curY = event.clientY - bounds.top;
    setMarquee({
      x: Math.min(current.startX, current.curX),
      y: Math.min(current.startY, current.curY),
      width: Math.abs(current.curX - current.startX),
      height: Math.abs(current.curY - current.startY),
    });
  }, []);

  const finishMarqueeSelection = useCallback((event: ReactMouseEvent) => {
    const current = marqueeRef.current;
    if (!current.active) {
      return;
    }
    current.active = false;
    setMarquee(null);
    if (event.button !== 2) {
      return;
    }
    const flow = flowInstanceRef.current;
    const bounds = canvasRegionRef.current?.getBoundingClientRect();
    if (!flow || !bounds) {
      return;
    }
    if (Math.abs(current.curX - current.startX) < 5 && Math.abs(current.curY - current.startY) < 5) {
      return;
    }
    const topLeft = flow.screenToFlowPosition({
      x: bounds.left + Math.min(current.startX, current.curX),
      y: bounds.top + Math.min(current.startY, current.curY),
    });
    const bottomRight = flow.screenToFlowPosition({
      x: bounds.left + Math.max(current.startX, current.curX),
      y: bounds.top + Math.max(current.startY, current.curY),
    });
    const minX = Math.min(topLeft.x, bottomRight.x);
    const maxX = Math.max(topLeft.x, bottomRight.x);
    const minY = Math.min(topLeft.y, bottomRight.y);
    const maxY = Math.max(topLeft.y, bottomRight.y);
    setNodes((currentNodes) => currentNodes.map((node) => {
      const width = node.measured?.width ?? 180;
      const height = node.measured?.height ?? 100;
      const inside = node.position.x + width >= minX && node.position.x <= maxX && node.position.y + height >= minY && node.position.y <= maxY;
      return { ...node, selected: inside };
    }));
  }, [setNodes]);


  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target && ['INPUT', 'TEXTAREA', 'SELECT'].includes(target.tagName)) {
        return;
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'z') {
        event.preventDefault();
        if (event.shiftKey) {
          redo();
        } else {
          undo();
        }
        return;
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'y') {
        event.preventDefault();
        redo();
        return;
      }
      if (event.key === 'Delete' || event.key === 'Backspace') {
        event.preventDefault();
        handleDeleteSelection();
      }
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [handleDeleteSelection, redo, undo]);

  const onSelectionChange = useCallback((params: OnSelectionChangeParams) => {
    const firstNode = params.nodes[0] as FlowNode | undefined;
    if (firstNode && nodes.some((candidate) => candidate.id === firstNode.id)) {
      setSelection({ type: 'node', node: firstNode.data.workflowNode });
      return;
    }
    const firstEdge = params.edges[0] as FlowEdge | undefined;
    if (firstEdge && edges.some((candidate) => candidate.id === firstEdge.id)) {
      setSelection({ type: 'edge', edge: firstEdge });
      return;
    }
    setSelection({ type: 'none' });
  }, [edges, nodes]);

  const runtimeLabel = runtimeGraph
    ? draftDirtyFromRuntime
      ? `Draft changed · runtime ${runtimeGraph.revision}`
      : `Runtime ${runtimeGraph.revision}`
    : 'Draft only';

  return (
    <div className="studio-shell">
      <header className="top-bar">
        <div>
          <span className="eyebrow">Camera Toolbox</span>
          <h1>{graph?.title ?? 'Workflow Web'}</h1>
        </div>
        <nav className="top-menu" aria-label="Workflow menu">
          <div className="menu-group">
            <button onClick={handleNewWorkflow}>New</button>
            <button onClick={handleSaveWorkflow}>Save</button>
            <button onClick={handleLoadSavedWorkflow}>Load</button>
            <button onClick={handleImportWorkflow}>Import</button>
            <button onClick={handleExportWorkflow}>Export</button>
          </div>
          <div className="menu-group">
            <button type="button" onClick={undo} disabled={pastRef.current.length === 0}>Undo</button>
            <button type="button" onClick={redo} disabled={futureRef.current.length === 0}>Redo</button>
          </div>
          <div className="menu-group">
            <button onClick={loadSeedWorkflow}>Reset</button>
            <button onClick={handleDeleteSelection}>Del</button>
            <button onClick={handleDuplicateSelection}>Dup</button>
          </div>
          <div className="menu-group">
            <button onClick={handleFitView}>Fit</button>
          </div>
          <div className="menu-group">
            <button type="button" onClick={handleRunGraph}>Run</button>
          </div>
        </nav>
        <div className="service-pill">{nodes.length}N / {edges.length}E · {runtimeLabel}</div>
      </header>

      <aside className="left-rail">
        <section className="rail-section">
          <h2>Templates</h2>
          <div className="library-list compact-list">
            {templates.map((template) => (
              <button key={template.id} className="library-item template-item" type="button" onClick={() => handleApplyTemplate(template)}>
                <strong>{template.title}</strong>
                <span>{template.description}</span>
              </button>
            ))}
          </div>
        </section>
        <section className="rail-section">
          <h2>Saved</h2>
          {savedWorkflows.length === 0 ? (
            <div className="rail-note compact">No saved workflows</div>
          ) : (
            <div className="library-list compact-list">
              {savedWorkflows.map((item) => (
                <div key={item.id} className="saved-workflow-card">
                  <strong>{item.title}</strong>
                  <span>{item.id} · {item.revision}</span>
                  <div className="saved-workflow-actions">
                    <button type="button" onClick={() => loadSavedWorkflow(item.id).then((loaded) => applyGraph(loaded, `已载入保存工作流：${loaded.title}`)).catch((error: unknown) => pushEvent(`载入失败：${error instanceof Error ? error.message : String(error)}`))}>Load</button>
                    <button type="button" onClick={() => deleteWorkflow(item.id).then(() => { pushEvent(`已删除工作流：${item.id}`); refreshSavedWorkflows(); }).catch((error: unknown) => pushEvent(`删除失败：${error instanceof Error ? error.message : String(error)}`))}>Delete</button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </section>
        <section className="rail-section">
          <h2>Node Library <span>drag or click</span></h2>
          <div className="library-list">
            {groupCatalog(catalog).map(([category, definitions]) => {
              const collapsed = collapsedCategories.has(category);
              return (
                <section key={category} className={`library-group ${collapsed ? 'collapsed' : ''}`}>
                  <button
                    type="button"
                    className="library-group-header"
                    onClick={() => toggleCategory(category)}
                    aria-expanded={!collapsed}
                    title={collapsed ? '展开分类' : '折叠分类'}
                  >
                    <span className="chevron" aria-hidden="true">{collapsed ? '▸' : '▾'}</span>
                    <span className="group-title">{category}</span>
                    <span className="group-count">{definitions.length}</span>
                  </button>
                  {!collapsed && (
                    <div className="library-group-items">
                      {definitions.map((item) => (
                        <NodeLibraryItem key={item.kind} definition={item} onAdd={handleAddNode} onDragStart={handleDragNodeStart} />
                      ))}
                    </div>
                  )}
                </section>
              );
            })}
          </div>
        </section>
        <div className="rail-note compact">Runtime handles and secrets stay in Inspector/runtime only.</div>
      </aside>

      <main
        className="canvas-region"
        ref={canvasRegionRef}
        onContextMenu={handleCanvasContextMenu}
        onMouseDown={handleCanvasMouseDown}
        onMouseMove={handleCanvasMouseMove}
        onMouseUp={finishMarqueeSelection}
        onMouseLeave={finishMarqueeSelection}
      >
        <ReactFlow
          nodes={nodes}
          edges={displayedEdges}
          nodeTypes={nodeTypes}
          onNodesChange={onNodesChange}
          onEdgesChange={onEdgesChange}
          onConnect={onConnect}
          onDragOver={handleDragNodeOver}
          onDrop={handleDropNode}
          onSelectionChange={onSelectionChange}
          onNodeDragStart={handleNodeDragStart}
          onNodeContextMenu={handleNodeContextMenu}
          onInit={(instance) => {
            flowInstanceRef.current = instance;
          }}
          fitView
          fitViewOptions={{ padding: 0.18, duration: 260 }}
          minZoom={0.2}
          maxZoom={1.8}
          zoomOnScroll
          elevateEdgesOnSelect
          deleteKeyCode={['Backspace', 'Delete']}
          proOptions={{ hideAttribution: true }}
        >
          <Background variant={BackgroundVariant.Lines} color="#334155" gap={24} size={1.25} />
          <MiniMap className="workflow-minimap" pannable zoomable nodeStrokeWidth={2} />
          <Controls className="workflow-controls" position="bottom-left" />
          <Panel position="top-left" className="canvas-panel">
            {selection.type === 'none' ? 'Select a node or edge' : `${selection.type}: ${selection.type === 'node' ? selection.node.title : selection.edge.id}`}
          </Panel>
        </ReactFlow>
        {marquee && (
          <div className="selection-marquee" style={{ left: marquee.x, top: marquee.y, width: marquee.width, height: marquee.height }} />
        )}
      </main>

      <Console events={events} onClear={() => setEvents([])} />
      {contextMenu && (
        <div className="context-menu-backdrop" onClick={() => setContextMenu(null)}>
          <div
            className="context-menu"
            style={{ left: contextMenu.x, top: contextMenu.y }}
            onClick={(event) => event.stopPropagation()}
          >
            <button type="button" onClick={() => handleContextMenuAction('rename')}>重命名</button>
            <button type="button" onClick={() => handleContextMenuAction('duplicate')}>复制</button>
            <button type="button" onClick={() => handleContextMenuAction('delete')}>删除</button>
          </div>
        </div>
      )}
    </div>
  );
}

function toFlowNodes(graph: WorkflowGraph): FlowNode[] {
  return graph.nodes.map((node) => ({
    id: node.id,
    type: node.kind,
    position: node.position,
    data: {
      workflowNode: node,
    },
  }));
}

function toFlowEdges(graph: WorkflowGraph): FlowEdge[] {
  return graph.edges.map((edge) => ({
    id: edge.id,
    source: edge.source.nodeId,
    sourceHandle: edge.source.portId,
    target: edge.target.nodeId,
    targetHandle: edge.target.portId,
    animated: false,
    label: labelForPortKind(edge.kind),
    data: { workflowEdge: edge, kind: edge.kind, schema: edge.schema, schemaVersion: edge.schemaVersion },
    className: 'workflow-edge flow-inactive',
  }));
}

function decorateFlowEdges(edges: FlowEdge[], nodes: FlowNode[], runtimeNodeStates: Map<string, string>): FlowEdge[] {
  const outgoing = new Map<string, FlowEdge[]>();
  for (const edge of edges) {
    outgoing.set(edge.source, [...(outgoing.get(edge.source) ?? []), edge]);
  }

  const activeNodes = new Set<string>();
  const queue: string[] = [];
  for (const node of nodes) {
    if (isActiveSeedNode(node.data.workflowNode, runtimeNodeStates)) {
      activeNodes.add(node.id);
      queue.push(node.id);
    }
  }

  while (queue.length > 0) {
    const sourceId = queue.shift() as string;
    for (const edge of outgoing.get(sourceId) ?? []) {
      if (activeNodes.has(edge.target)) {
        continue;
      }
      activeNodes.add(edge.target);
      queue.push(edge.target);
    }
  }

  return edges.map((edge) => {
    const active = activeNodes.has(edge.source)
      || runtimeNodeStates.get(edge.source) === 'running'
      || runtimeNodeStates.get(edge.target) === 'running';
    return {
      ...edge,
      animated: active,
      className: `workflow-edge ${active ? 'flow-active' : 'flow-inactive'}`,
    };
  });
}

function isActiveSeedNode(node: WorkflowNode, runtimeNodeStates: Map<string, string>): boolean {
  // 连线是否「活跃」应只依据真实运行时状态（引擎 drain_status 上报），
  // 不能只看 config 里有没有默认 URL/路径文本——否则默认 RTSP URL 会让整条链误显示为活跃。
  const runtime = runtimeNodeStates.get(node.id);
  if (runtime === 'running' || runtime === 'ready') {
    return true;
  }
  return false;
}

/** 生成 RFC4122 v4 UUID；randomUUID 仅在 secure context（HTTPS/localhost）可用，非 HTTPS 非 localhost 时回退 getRandomValues。 */
function createUuid(): string {
  const cryptoApi = globalThis.crypto;
  if (typeof cryptoApi?.randomUUID === 'function') {
    return cryptoApi.randomUUID();
  }
  const bytes = new Uint8Array(16);
  if (typeof cryptoApi?.getRandomValues === 'function') {
    cryptoApi.getRandomValues(bytes);
  } else {
    for (let index = 0; index < bytes.length; index += 1) {
      bytes[index] = Math.floor(Math.random() * 256);
    }
  }
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

function createGraphId(): string {
  return `workflow-${createUuid()}`;
}

function createNodeId(kind: NodeKind): string {
  return `${kind}-${createUuid()}`;
}

function createWorkflowNode(kind: NodeKind, count: number, position: { x: number; y: number }, definition: NodeDefinition | undefined): WorkflowNode {
  return {
    id: createNodeId(kind),
    kind,
    title: `${definition?.title ?? kind} ${count}`,
    position,
    state: kind === 'rtspSource' || kind === 'localFileSource' || kind === 'sftpFileSource' || kind === 'sshSession' || kind === 'x5Device' ? 'ready' : 'idle',
    category: definition?.category ?? 'diagnostics',
    inputs: definition?.inputs ?? [],
    outputs: definition?.outputs ?? [],
    config: definition?.defaultConfig ?? (kind === 'rtspSource' ? { url: DEFAULT_RTSP_URL, transport: 'tcp' } : {}),
  };
}

function toFlowNode(node: WorkflowNode): FlowNode {
  return { id: node.id, type: node.kind, position: node.position, data: { workflowNode: node } };
}

function toWorkflowGraph(nodes: FlowNode[], edges: FlowEdge[], base: WorkflowGraph): WorkflowGraph {
  return {
    ...base,
    schemaVersion: WORKFLOW_SCHEMA_VERSION,
    nodes: nodes.map((node) => ({ ...node.data.workflowNode, position: { x: node.position.x, y: node.position.y } })),
    edges: edges.map((edge) => ({
      id: edge.id,
      source: { nodeId: String(edge.source), portId: String(edge.sourceHandle ?? '') },
      target: { nodeId: String(edge.target), portId: String(edge.targetHandle ?? '') },
      kind: edge.data?.kind ?? inferPortKind(nodes, String(edge.source), String(edge.sourceHandle ?? '')),
      schema: edge.data?.schema ?? inferPortSchema(nodes, String(edge.source), String(edge.sourceHandle ?? '')),
      schemaVersion: edge.data?.schemaVersion ?? WORKFLOW_SCHEMA_VERSION,
    })),
  };
}

/** 引擎接管 viewer 帧后不再需要 preview 注入；保留此函数维持调用点稳定。 */
function withViewerPreviews(nodes: FlowNode[], _edges: FlowEdge[]): FlowNode[] {
  return nodes;
}

function inferPortKind(nodes: FlowNode[], nodeId: string, portId: string): PortKind {
  const node = nodes.find((candidate) => candidate.id === nodeId)?.data.workflowNode;
  return node?.outputs.find((port) => port.id === portId)?.kind ?? 'endpoint.rtsp';
}

function inferPortSchema(nodes: FlowNode[], nodeId: string, portId: string): string {
  const node = nodes.find((candidate) => candidate.id === nodeId)?.data.workflowNode;
  return node?.outputs.find((port) => port.id === portId)?.schema ?? 'media.rtsp.endpoint.v1';
}

function emptyWorkflowGraph(): WorkflowGraph {
  return {
    schemaVersion: WORKFLOW_SCHEMA_VERSION,
    id: createGraphId(),
    title: 'Untitled Workflow',
    revision: 'draft',
    nodes: [],
    edges: [],
    viewport: { x: 0, y: 0, zoom: 1 },
  };
}

function groupCatalog(catalog: NodeDefinition[]): Array<[string, NodeDefinition[]]> {
  const groups = new Map<string, NodeDefinition[]>();
  for (const definition of catalog) {
    groups.set(definition.category, [...(groups.get(definition.category) ?? []), definition]);
  }
  return [...groups.entries()];
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

