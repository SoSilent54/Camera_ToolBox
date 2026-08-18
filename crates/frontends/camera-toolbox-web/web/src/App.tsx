import { useCallback, useEffect, useMemo, useRef, useState, type DragEvent, type MouseEvent as ReactMouseEvent } from 'react';
import {
  Background,
  BackgroundVariant,
  Controls,
  MiniMap,
  Panel,
  ReactFlow,
  useEdgesState,
  useNodesState,
  type Connection,
  type Edge,
  type EdgeChange,
  type Node,
  type NodeChange,
  type OnSelectionChangeParams,
  type OnConnectEnd,
  type OnConnectStart,
  type OnConnectStartParams,
  type OnMove,
  type ReactFlowInstance,
} from '@xyflow/react';
import {
  WORKFLOW_SCHEMA_VERSION,
  addGraphEdge,
  addGraphNode,
  addGraphNodeAndEdge,
  deleteWorkflow,
  importWorkflow,
  labelForPortKind,
  listWorkflows,
  loadNodeCatalog,
  loadSeedGraph,
  loadSavedWorkflow,
  loadWorkflow,
  loadWorkmodeTemplates,
  removeGraphNode,
  removeGraphSelection,
  replaceGraph,
  saveWorkflow,
  patchGraphNode,
  updateGraphNodePosition,
  updateGraphNodePositions,
  validateConnectionKinds,
  type FlowEdgeData,
  type FlowNodeData,
  type NodeActionControl,
  type NodeDefinition,
  type NodeKind,
  type PortKind,
  type ScalarConfigValue,
  type WorkflowEdge,
  type WorkflowGraph,
  type WorkflowNode,
  type WorkflowPort,
  type WorkmodeTemplate,
} from './workflow';
import {
  EepromProvisionNode,
  I2cTransferNode,
  LocalFileSourceNode,
  SftpFileSourceNode,
  SshSessionNode,
  X5DeviceNode,
} from './WorkflowNodes';
import {
  AutoCaptureNode,
  CalibrationSolverNode,
  CalibrationWorkflowNode,
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
type NodeCreateMenuMode =
  | {
      kind: 'freeAdd';
      position: { x: number; y: number };
      screenPosition: { x: number; y: number };
    }
  | {
      kind: 'connectAdd';
      position: { x: number; y: number };
      screenPosition: { x: number; y: number };
      fromNodeId: string;
      fromPortId: string;
      fromDirection: 'input' | 'output';
      fromPortKind: PortKind;
      fromSchema: string;
      fromCardinality: 'one' | 'many';
    };

type PaneClickState = {
  startX: number;
  startY: number;
  moved: boolean;
  active: boolean;
};

type NodeCreateCandidate = {
  definition: NodeDefinition;
  compatiblePort?: WorkflowPort;
};

const DEFAULT_RTSP_URL = 'rtsp://10.21.12.108:554/PRR';
const DND_NODE_KIND = 'application/x-camera-toolbox-node-kind';
const GENERIC_NODE_KINDS: NodeKind[] = [
  'rtspDecoder',
  'frameSampler',
  'imageLayer',
  'videoLayer',
  'overlayComposer',
];

const nodeTypes = Object.fromEntries([
  ['rtspSource', RtspSourceNode],
  ['localFileSource', LocalFileSourceNode],
  ['sftpFileSource', SftpFileSourceNode],
  ['sshSession', SshSessionNode],
  ['x5Device', X5DeviceNode],
  ['i2cTransfer', I2cTransferNode],
  ['eepromProvision', EepromProvisionNode],
  ['viewer', ViewerNode],
  ['calibrationSolver', CalibrationSolverNode],
  ['autoCaptureController', AutoCaptureNode],
  ['chessboardDetector', CalibrationWorkflowNode],
  ['datasetCollector', CalibrationWorkflowNode],
  ['coverageAnalyzer', CalibrationWorkflowNode],
  ['poseGuide', CalibrationWorkflowNode],
  ...GENERIC_NODE_KINDS.map((kind) => [kind, GenericWorkflowNode]),
]);

const PANE_CLICK_DISTANCE_PX = 5;
const GRID_TARGET_SCREEN_GAP_PX = 28;
const GRID_MIN_GAP = 24;
const GRID_MAX_GAP = 144;

const MANUAL_TRIGGER_ACTIONS: readonly NodeActionControl[] = [{ action: 'trigger', label: '触发' }];

/** generic 节点须显式以 `trigger: "manual"` 声明动作，避免暴露后端尚未实现的入口。 */
function genericNodeActions(node: WorkflowNode): readonly NodeActionControl[] | undefined {
  return GENERIC_NODE_KINDS.includes(node.kind) && node.config.trigger === 'manual'
    ? MANUAL_TRIGGER_ACTIONS
    : undefined;
}

export function App() {
  const [graph, setGraph] = useState<WorkflowGraph | null>(null);
  const [catalog, setCatalog] = useState<NodeDefinition[]>([]);
  const [templates, setTemplates] = useState<WorkmodeTemplate[]>([]);
  const [savedWorkflows, setSavedWorkflows] = useState<Array<{ id: string; title: string; revision: string }>>([]);
  const [nodes, setNodes, onNodesChangeBase] = useNodesState<FlowNode>([]);
  const [edges, setEdges, onEdgesChangeBase] = useEdgesState<FlowEdge>([]);
  const [selection, setSelection] = useState<Selection>({ type: 'none' });
  const [events, setEvents] = useState<string[]>(['等待后端图快照...']);
  const { nodeStates, nodeDiagnostics, nodeOutputs, pendingActions, sendAction, refreshNodeOutput } = useEngine();
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number; nodeId: string } | null>(null);
  const flowInstanceRef = useRef<ReactFlowInstance<FlowNode, FlowEdge> | null>(null);
  const [collapsedCategories, setCollapsedCategories] = useState<Set<string>>(new Set());
  const [marquee, setMarquee] = useState<{ x: number; y: number; width: number; height: number } | null>(null);
  const canvasRegionRef = useRef<HTMLElement | null>(null);
  const marqueeRef = useRef({ startX: 0, startY: 0, curX: 0, curY: 0, active: false });
  const nodesRef = useRef<FlowNode[]>(nodes);
  const edgesRef = useRef<FlowEdge[]>(edges);
  const [nodeCreateMenu, setNodeCreateMenu] = useState<NodeCreateMenuMode | null>(null);
  const connectionStartRef = useRef<OnConnectStartParams | null>(null);
  const graphRef = useRef<WorkflowGraph | null>(null);
  const draggingNodeIdRef = useRef<string | null>(null);
  const paneClickRef = useRef<PaneClickState>({ startX: 0, startY: 0, moved: false, active: false });
  const [backgroundGap, setBackgroundGap] = useState(() => backgroundGapForZoom(1));

  const pushEvent = useCallback((event: string) => {
    setEvents((current) => [event, ...current].slice(0, 10));
  }, []);

  useEffect(() => {
    nodesRef.current = nodes;
  }, [nodes]);

  useEffect(() => {
    edgesRef.current = edges;
  }, [edges]);

  const renderGraph = useCallback(
    (nextGraph: WorkflowGraph, event?: string, options?: { source?: 'response' | 'snapshot' }) => {
      const currentGraph = graphRef.current;
      if (options?.source === 'snapshot') {
        if (draggingNodeIdRef.current) {
          return;
        }
        if (currentGraph && !isNewerGraphRevision(nextGraph.revision, currentGraph.revision)) {
          return;
        }
      }
      graphRef.current = nextGraph;
      setGraph(nextGraph);
      setNodes((current) => mergeFlowNodes(current, nextGraph));
      setEdges((current) => mergeFlowEdges(current, nextGraph));
      if (event) {
        setEvents([event, `节点 ${nextGraph.nodes.length} 个，连接 ${nextGraph.edges.length} 条`]);
      }
    },
    [setEdges, setNodes],
  );

  const commitGraph = useCallback(
    (request: Promise<WorkflowGraph>, event: string) => {
      request
        .then((nextGraph) => renderGraph(nextGraph, event, { source: 'response' }))
        .catch((error: unknown) => pushEvent(`图更新失败：${error instanceof Error ? error.message : String(error)}`));
    },
    [pushEvent, renderGraph],
  );

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

  // 后端 authoritative snapshot 是持久图状态唯一来源；前端只保留拖拽/菜单等临时交互态。
  useEffect(() => subscribeSnapshot((snapshot) => {
    const nextGraph = snapshot.payload.graph as WorkflowGraph | undefined;
    if (!nextGraph || !Array.isArray(nextGraph.nodes) || !Array.isArray(nextGraph.edges)) {
      return;
    }
    renderGraph(nextGraph, '已同步后端图快照', { source: 'snapshot' });
  }), [renderGraph]);

  const refreshSavedWorkflows = useCallback(() => {
    listWorkflows()
      .then((workflows) => setSavedWorkflows(workflows))
      .catch((error: unknown) => pushEvent(`工作流列表失败：${error instanceof Error ? error.message : String(error)}`));
  }, [pushEvent]);

  const loadCurrentGraph = useCallback(() => {
    loadWorkflow()
      .then((loaded) => renderGraph(loaded, `已加载后端图：${loaded.title}`))
      .catch((error: unknown) => setEvents([`加载失败：${error instanceof Error ? error.message : String(error)}`]));
  }, [renderGraph]);

  useEffect(() => {
    loadCurrentGraph();
    refreshSavedWorkflows();
    Promise.all([loadNodeCatalog(), loadWorkmodeTemplates()])
      .then(([loadedCatalog, loadedTemplates]) => {
        setCatalog(loadedCatalog);
        setTemplates(loadedTemplates);
      })
      .catch((error: unknown) => pushEvent(`节点目录加载失败：${error instanceof Error ? error.message : String(error)}`));
  }, [loadCurrentGraph, pushEvent, refreshSavedWorkflows]);

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

  const onNodesChange = useCallback((changes: NodeChange<FlowNode>[]) => {
    onNodesChangeBase(changes);
  }, [onNodesChangeBase]);

  const onEdgesChange = useCallback((changes: EdgeChange<FlowEdge>[]) => {
    onEdgesChangeBase(changes);
  }, [onEdgesChangeBase]);

  const applyGraph = useCallback(
    (nextGraph: WorkflowGraph, event: string) => {
      commitGraph(replaceGraph(nextGraph), event);
    },
    [commitGraph],
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
      const edgeId = `edge-${connection.source}-${connection.sourceHandle}-${connection.target}-${connection.targetHandle}`;
      const workflowEdge: WorkflowEdge = {
        id: edgeId,
        source: { nodeId: String(connection.source), portId: String(connection.sourceHandle) },
        target: { nodeId: String(connection.target), portId: String(connection.targetHandle) },
        kind: validation.port.kind,
        schema: validation.port.schema,
        schemaVersion: WORKFLOW_SCHEMA_VERSION,
      };
      commitGraph(addGraphEdge(workflowEdge), `新增连接：${edgeId}`);
    },
    [canConnect, commitGraph, pushEvent],
  );


  const handleRtspUrlChange = useCallback(
    (nodeId: string, nextUrl: string) => {
      const trimmedUrl = nextUrl.trim();
      if (!trimmedUrl.startsWith('rtsp://') && !trimmedUrl.startsWith('rtsps://')) {
        pushEvent('拒绝 RTSP URL：必须使用 rtsp:// 或 rtsps://');
        return;
      }
      commitGraph(
        patchGraphNode(nodeId, { config: { url: trimmedUrl } }),
        `RTSP URL 已更新：${trimmedUrl}`,
      );
    },
    [commitGraph, pushEvent],
  );

  const handleNodeTitleChange = useCallback(
    (nodeId: string, nextTitle: string) => {
      const title = nextTitle.trim();
      if (!title) {
        pushEvent('节点标题不能为空');
        return;
      }
      commitGraph(patchGraphNode(nodeId, { title }), `节点已重命名：${title}`);
    },
    [commitGraph, pushEvent],
  );

  const handleNodeConfigChange = useCallback(
    (nodeId: string, key: string, value: ScalarConfigValue) => {
      commitGraph(patchGraphNode(nodeId, { config: { [key]: value } }), `节点配置已更新：${key}`);
    },
    [commitGraph],
  );

  useEffect(() => {
    setNodes((current) => current.map((flowNode) => {
      const runtimeState = nodeStates[flowNode.id];
      const runtimeDiagnostic = nodeDiagnostics[flowNode.id];
      const runtimeOutput = nodeOutputs[flowNode.id];
      const actionPending = Boolean(pendingActions[flowNode.id]);
      const availableActions = genericNodeActions(flowNode.data.workflowNode);
      if (
        flowNode.data.onRtspUrlChange === handleRtspUrlChange
        && flowNode.data.onNodeConfigChange === handleNodeConfigChange
        && flowNode.data.onNodeAction === sendAction
        && flowNode.data.onRefreshNodeOutput === refreshNodeOutput
        && flowNode.data.runtimeState === runtimeState
        && flowNode.data.runtimeDiagnostic === runtimeDiagnostic
        && flowNode.data.runtimeOutput === runtimeOutput
        && flowNode.data.availableActions === availableActions
        && flowNode.data.actionPending === actionPending
      ) {
        return flowNode;
      }
      return {
        ...flowNode,
        data: {
          ...flowNode.data,
          runtimeState,
          runtimeDiagnostic,
          runtimeOutput,
          availableActions,
          onRtspUrlChange: handleRtspUrlChange,
          actionPending,
          onNodeConfigChange: handleNodeConfigChange,
          onNodeAction: sendAction,
          onRefreshNodeOutput: refreshNodeOutput,
        },
      };
    }));
  }, [handleNodeConfigChange, handleRtspUrlChange, nodeDiagnostics, nodeOutputs, nodeStates, pendingActions, refreshNodeOutput, nodes.length, sendAction, setNodes]);

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
      commitGraph(addGraphNode(flowNode.data.workflowNode), `${source === 'drag' ? '拖入' : '新增'}节点：${flowNode.data.workflowNode.title}`);
    },
    [commitGraph],
  );

  const handleAddNode = useCallback(
    (kind: NodeKind) => {
      const viewportCenter = flowInstanceRef.current?.screenToFlowPosition({ x: window.innerWidth * 0.5, y: window.innerHeight * 0.5 })
        ?? { x: 96 + (nodes.length % 4) * 56, y: 96 + nodes.length * 36 };
      insertFlowNode(createFlowNodeAt(kind, viewportCenter), 'click');
    },
    [createFlowNodeAt, insertFlowNode, nodes.length],
  );

  const openFreeNodeMenu = useCallback((event: { clientX: number; clientY: number }) => {
    const position = flowInstanceRef.current?.screenToFlowPosition({ x: event.clientX, y: event.clientY })
      ?? { x: event.clientX, y: event.clientY };
    setNodeCreateMenu({
      kind: 'freeAdd',
      position,
      screenPosition: clampMenuPosition(event.clientX, event.clientY),
    });
  }, []);

  const nodeCreateCandidates = useMemo((): NodeCreateCandidate[] => {
    if (!nodeCreateMenu) {
      return [];
    }
    if (nodeCreateMenu.kind === 'freeAdd') {
      return catalog.map((definition) => ({ definition }));
    }
    return catalog.flatMap((definition) => compatiblePortsForCreateMenu(definition, nodeCreateMenu, edges)
      .map((compatiblePort) => ({ definition, compatiblePort })));
  }, [catalog, edges, nodeCreateMenu]);

  const handleNodeCreateMenuPick = useCallback((candidate: NodeCreateCandidate) => {
    if (!nodeCreateMenu) {
      return;
    }
    const flowNode = createFlowNodeAt(candidate.definition.kind, centerNodeAt(nodeCreateMenu.position));
    if (nodeCreateMenu.kind === 'freeAdd') {
      insertFlowNode(flowNode, 'click');
      setNodeCreateMenu(null);
      return;
    }
    const compatiblePort = candidate.compatiblePort;
    if (!compatiblePort) {
      return;
    }
    const existingPort = {
      nodeId: nodeCreateMenu.fromNodeId,
      portId: nodeCreateMenu.fromPortId,
    };
    const newPort = { nodeId: flowNode.id, portId: compatiblePort.id };
    const source = nodeCreateMenu.fromDirection === 'output' ? existingPort : newPort;
    const target = nodeCreateMenu.fromDirection === 'output' ? newPort : existingPort;
    const edgeId = `edge-${source.nodeId}-${source.portId}-${target.nodeId}-${target.portId}`;
    const workflowEdge: WorkflowEdge = {
      id: edgeId,
      source,
      target,
      kind: compatiblePort.kind,
      schema: compatiblePort.schema,
      schemaVersion: WORKFLOW_SCHEMA_VERSION,
    };
    commitGraph(addGraphNodeAndEdge(flowNode.data.workflowNode, workflowEdge), `新增并连接节点：${flowNode.data.workflowNode.title}`);
    setNodeCreateMenu(null);
  }, [commitGraph, createFlowNodeAt, insertFlowNode, nodeCreateMenu]);

  const handleConnectStart = useCallback<OnConnectStart>((_event, params) => {
    connectionStartRef.current = params;
  }, []);

  const handleConnectEnd = useCallback<OnConnectEnd>((event, connectionState) => {
    const start = connectionStartRef.current;
    connectionStartRef.current = null;
    if (!start?.nodeId || !start.handleId || connectionState.toHandle) {
      return;
    }
    const workflowNode = nodeById.get(start.nodeId);
    const fromDirection = start.handleType === 'source' ? 'output' : 'input';
    const fromPort = fromDirection === 'output'
      ? workflowNode?.outputs.find((port) => port.id === start.handleId)
      : workflowNode?.inputs.find((port) => port.id === start.handleId);
    if (!fromPort) {
      return;
    }
    const point = 'clientX' in event
      ? { x: event.clientX, y: event.clientY }
      : { x: connectionState.pointer?.x ?? 0, y: connectionState.pointer?.y ?? 0 };
    const position = flowInstanceRef.current?.screenToFlowPosition(point) ?? point;
    setNodeCreateMenu({
      kind: 'connectAdd',
      position,
      screenPosition: clampMenuPosition(point.x, point.y),
      fromNodeId: start.nodeId,
      fromPortId: start.handleId,
      fromDirection,
      fromPortKind: fromPort.kind,
      fromSchema: fromPort.schema,
      fromCardinality: fromPort.cardinality,
    });
  }, [nodeById]);

  const handleDragNodeStart = useCallback((event: DragEvent<HTMLElement>, kind: NodeKind) => {
    event.dataTransfer.setData(DND_NODE_KIND, kind);
    event.dataTransfer.effectAllowed = 'copy';
  }, []);

  const handleDragNodeOver = useCallback((event: DragEvent<HTMLElement>) => {
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
    const selectedNodeIds = nodesRef.current
      .filter((node) => node.selected)
      .map((node) => node.data.workflowNode.id);
    const selectedEdgeIds = edgesRef.current
      .filter((edge) => edge.selected)
      .map((edge) => edge.id);
    if (selectedNodeIds.length === 0 && selectedEdgeIds.length === 0) {
      pushEvent('没有选中的节点或连线');
      return;
    }
    setSelection({ type: 'none' });
    setContextMenu(null);
    commitGraph(removeGraphSelection({ nodeIds: selectedNodeIds, edgeIds: selectedEdgeIds }), `删除选中项：${selectedNodeIds.length} 个节点、${selectedEdgeIds.length} 条连线`);
  }, [commitGraph, pushEvent]);

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
    const duplicated: WorkflowNode = {
      ...source.data.workflowNode,
      id: createNodeId(source.data.workflowNode.kind),
      title: `${source.data.workflowNode.title} Copy`,
      position: { x: source.position.x + 48, y: source.position.y + 48 },
      config: { ...source.data.workflowNode.config },
    };
    commitGraph(addGraphNode(duplicated), `复制节点：${duplicated.title}`);
  }, [commitGraph, nodes, pushEvent, selection]);

  const handleSaveWorkflow = useCallback(() => {
    const currentGraph = graph ? toWorkflowGraph(nodes, edges, graph) : emptyWorkflowGraph();
    saveWorkflow(currentGraph)
      .then((saved) => {
        renderGraph(saved, `工作流已保存：${saved.id} @ ${saved.revision}`);
        refreshSavedWorkflows();
      })
      .catch((error: unknown) => pushEvent(`保存失败：${error instanceof Error ? error.message : String(error)}`));
  }, [edges, graph, nodes, pushEvent, refreshSavedWorkflows, renderGraph]);

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
    const currentGraph = graph ? toWorkflowGraph(nodes, edges, graph) : emptyWorkflowGraph();
    const blob = new Blob([`${JSON.stringify(currentGraph, null, 2)}\n`], { type: 'application/json;charset=utf-8' });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement('a');
    anchor.href = url;
    anchor.download = `${currentGraph.id}.ctworkflow.json`;
    anchor.click();
    URL.revokeObjectURL(url);
    pushEvent(`已导出当前后端图：${currentGraph.id}`);
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
    loadSeedGraph()
      .then((seed) => applyGraph({ ...seed, id: createGraphId(), revision: 'draft' }, '已新建工作流'))
      .catch((error: unknown) => pushEvent(`新建失败：${error instanceof Error ? error.message : String(error)}`));
  }, [applyGraph, pushEvent]);

  const handleResetWorkflow = useCallback(() => {
    loadSeedGraph()
      .then((seed) => applyGraph(seed, `已重置为内置工作流：${seed.title}`))
      .catch((error: unknown) => pushEvent(`重置失败：${error instanceof Error ? error.message : String(error)}`));
  }, [applyGraph, pushEvent]);

  const handleFitView = useCallback(() => {
    void flowInstanceRef.current?.fitView({ padding: 0.2, duration: 180 });
    pushEvent('画布已适配视图');
  }, [pushEvent]);

  const handleViewportMove = useCallback<OnMove>((event, viewport) => {
    if (event && paneClickRef.current.active) {
      paneClickRef.current.moved = true;
    }
    const nextGap = backgroundGapForZoom(viewport.zoom);
    setBackgroundGap(nextGap);
  }, []);

  const handleNodeDragStart = useCallback((_event: MouseEvent | TouchEvent, node: FlowNode) => {
    draggingNodeIdRef.current = node.id;
  }, []);

  const handleNodeDragStop = useCallback((_event: MouseEvent | TouchEvent, node: FlowNode) => {
    draggingNodeIdRef.current = null;
    const movedNodes = nodesRef.current.filter((candidate) => {
      if (candidate.id !== node.id && !candidate.selected) {
        return false;
      }
      const workflowNode = candidate.data.workflowNode;
      return workflowNode.position.x !== candidate.position.x || workflowNode.position.y !== candidate.position.y;
    });
    if (movedNodes.length === 0) {
      return;
    }
    if (movedNodes.length === 1) {
      const workflowNode = movedNodes[0].data.workflowNode;
      commitGraph(updateGraphNodePosition(workflowNode.id, { x: movedNodes[0].position.x, y: movedNodes[0].position.y }), `节点位置已更新：${workflowNode.title}`);
      return;
    }
    commitGraph(
      updateGraphNodePositions(movedNodes.map((candidate) => ({
        nodeId: candidate.data.workflowNode.id,
        position: { x: candidate.position.x, y: candidate.position.y },
      }))),
      `已批量更新 ${movedNodes.length} 个节点位置`,
    );
  }, [commitGraph]);

  const handleNodeContextMenu = useCallback((event: ReactMouseEvent, node: FlowNode) => {
    event.preventDefault();
    setNodeCreateMenu(null);
    setContextMenu({ x: event.clientX, y: event.clientY, nodeId: node.id });
  }, []);

  const handleContextMenuAction = useCallback((action: 'rename' | 'duplicate' | 'delete') => {
    const current = contextMenu;
    if (!current) {
      return;
    }
    const node = nodesRef.current.find((candidate) => candidate.id === current.nodeId);
    setContextMenu(null);
    if (!node) {
      return;
    }
    const workflowNode = node.data.workflowNode;
    if (action === 'rename') {
      const title = window.prompt('节点重命名', workflowNode.title);
      if (title && title.trim()) {
        handleNodeTitleChange(workflowNode.id, title);
      }
    } else if (action === 'delete') {
      commitGraph(removeGraphNode(workflowNode.id), `删除节点：${workflowNode.title}`);
    } else if (action === 'duplicate') {
      const duplicated = {
        ...workflowNode,
        id: createNodeId(workflowNode.kind),
        title: `${workflowNode.title} Copy`,
        position: { x: node.position.x + 48, y: node.position.y + 48 },
        config: { ...workflowNode.config },
      };
      commitGraph(addGraphNode(duplicated), `复制节点：${duplicated.title}`);
    }
  }, [commitGraph, contextMenu, handleNodeTitleChange]);

  const handleCanvasContextMenu = useCallback((event: ReactMouseEvent | globalThis.MouseEvent) => {
    event.preventDefault();
  }, []);

  const handlePaneClick = useCallback((event: ReactMouseEvent) => {
    const clickState = paneClickRef.current;
    const moved = clickState.moved;
    clickState.active = false;
    clickState.moved = false;
    if (event.button !== 0 || moved) {
      return;
    }
    setContextMenu(null);
    openFreeNodeMenu(event);
  }, [openFreeNodeMenu]);

  const handleCanvasMouseDown = useCallback((event: ReactMouseEvent) => {
    const target = event.target as HTMLElement | null;
    if (target?.closest('.react-flow__node, .react-flow__handle, input, textarea, select, .context-menu, .node-create-menu')) {
      return;
    }
    if (event.button === 0 && target?.closest('.react-flow__pane')) {
      paneClickRef.current = { startX: event.clientX, startY: event.clientY, moved: false, active: true };
      return;
    }
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
    const clickState = paneClickRef.current;
    if (clickState.active && !clickState.moved) {
      const dx = event.clientX - clickState.startX;
      const dy = event.clientY - clickState.startY;
      clickState.moved = Math.hypot(dx, dy) > PANE_CLICK_DISTANCE_PX;
    }
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
  }, [openFreeNodeMenu, setNodes]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target && ['INPUT', 'TEXTAREA', 'SELECT'].includes(target.tagName)) {
        return;
      }
      if (event.key === 'Delete' || event.key === 'Backspace') {
        event.preventDefault();
        handleDeleteSelection();
      }
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [handleDeleteSelection]);

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

  const graphLabel = graph ? `Graph ${graph.revision}` : 'Awaiting graph';

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
            <button onClick={handleResetWorkflow}>Reset</button>
            <button onClick={handleDeleteSelection}>Del</button>
            <button onClick={handleDuplicateSelection}>Dup</button>
          </div>
          <div className="menu-group">
            <button onClick={handleFitView}>Fit</button>
          </div>
        </nav>
        <div className="service-pill">{nodes.length}N / {edges.length}E · {graphLabel}</div>
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
        <div className="rail-note compact">Graph is backend-authoritative; nodes own runtime state.</div>
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
          onConnectStart={handleConnectStart}
          onConnectEnd={handleConnectEnd}
          onDragOver={handleDragNodeOver}
          onDrop={handleDropNode}
          onSelectionChange={onSelectionChange}
          onPaneClick={handlePaneClick}
          onPaneContextMenu={handleCanvasContextMenu}
          onMove={handleViewportMove}
          onNodeDragStart={handleNodeDragStart}
          onNodeDragStop={handleNodeDragStop}
          onNodeContextMenu={handleNodeContextMenu}
          onInit={(instance) => {
            flowInstanceRef.current = instance;
            setBackgroundGap(backgroundGapForZoom(instance.getViewport().zoom));
          }}
          fitView
          fitViewOptions={{ padding: 0.18, duration: 260 }}
          minZoom={0.2}
          maxZoom={1.8}
          zoomOnScroll
          elevateEdgesOnSelect
          deleteKeyCode={null}
          paneClickDistance={5}
          proOptions={{ hideAttribution: true }}
        >
          <Background variant={BackgroundVariant.Lines} color="#334155" gap={backgroundGap} size={1.25} />
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
      {nodeCreateMenu && (
        <div className="context-menu-backdrop" onClick={() => setNodeCreateMenu(null)}>
          <NodeCreateMenu
            mode={nodeCreateMenu}
            candidates={nodeCreateCandidates}
            onPick={handleNodeCreateMenuPick}
            onClose={() => setNodeCreateMenu(null)}
          />
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
// 空闲边使用显式 SVG 样式，避免仅靠低对比度 CSS 而在深色画布上消失。
const DORMANT_EDGE_STYLE = { stroke: '#94a3b8', strokeWidth: 2, opacity: 0.9 } as const;
const ACTIVE_EDGE_STYLE = { stroke: '#38bdf8', strokeWidth: 2.25, opacity: 1 } as const;

function toFlowEdges(graph: WorkflowGraph): FlowEdge[] {
  return graph.edges.map((edge) => ({
    id: edge.id,
    source: edge.source.nodeId,
    sourceHandle: edge.source.portId,
    target: edge.target.nodeId,
    targetHandle: edge.target.portId,
    animated: false,
    style: DORMANT_EDGE_STYLE,
    label: labelForPortKind(edge.kind),
    data: { workflowEdge: edge, kind: edge.kind, schema: edge.schema, schemaVersion: edge.schemaVersion },
    className: 'workflow-edge flow-inactive',
  }));
}

function mergeFlowNodes(current: FlowNode[], graph: WorkflowGraph): FlowNode[] {
  const currentById = new Map(current.map((node) => [node.id, node]));
  return graph.nodes.map((workflowNode) => {
    const existing = currentById.get(workflowNode.id);
    if (!existing) {
      return toFlowNode(workflowNode);
    }
    return {
      ...existing,
      type: workflowNode.kind,
      position: workflowNode.position,
      data: {
        ...existing.data,
        workflowNode,
      },
    };
  });
}

function mergeFlowEdges(current: FlowEdge[], graph: WorkflowGraph): FlowEdge[] {
  const currentById = new Map(current.map((edge) => [edge.id, edge]));
  return graph.edges.map((workflowEdge) => {
    const nextEdge = toFlowEdges({ ...graph, edges: [workflowEdge] })[0];
    const existing = currentById.get(workflowEdge.id);
    if (!existing) {
      return nextEdge;
    }
    return {
      ...existing,
      source: nextEdge.source,
      sourceHandle: nextEdge.sourceHandle,
      target: nextEdge.target,
      targetHandle: nextEdge.targetHandle,
      label: nextEdge.label,
      data: nextEdge.data,
      animated: nextEdge.animated,
      style: nextEdge.style,
      className: nextEdge.className,
    };
  });
}

function isNewerGraphRevision(nextRevision: string, currentRevision: string): boolean {
  if (nextRevision === currentRevision) {
    return false;
  }
  const next = Number(nextRevision.replace(/^rev-/, ''));
  const current = Number(currentRevision.replace(/^rev-/, ''));
  if (Number.isFinite(next) && Number.isFinite(current)) {
    return next > current;
  }
  return nextRevision > currentRevision;
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
      style: active ? ACTIVE_EDGE_STYLE : DORMANT_EDGE_STYLE,
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

function NodeCreateMenu({
  mode,
  candidates,
  onPick,
  onClose,
}: {
  mode: NodeCreateMenuMode;
  candidates: NodeCreateCandidate[];
  onPick: (candidate: NodeCreateCandidate) => void;
  onClose: () => void;
}) {
  const [query, setQuery] = useState('');
  const [activeIndex, setActiveIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    setQuery('');
    setActiveIndex(0);
    requestAnimationFrame(() => inputRef.current?.focus());
  }, [mode]);

  const filteredCandidates = useMemo(() => {
    const normalized = query.trim().toLowerCase();
    if (!normalized) {
      return candidates;
    }
    return candidates.filter(({ definition, compatiblePort }) => [
      definition.title,
      definition.kind,
      definition.category,
      definition.description,
      compatiblePort?.label,
      compatiblePort?.schema,
    ].some((text) => text?.toLowerCase().includes(normalized)));
  }, [candidates, query]);

  useEffect(() => {
    setActiveIndex((current) => clamp(current, 0, Math.max(0, filteredCandidates.length - 1)));
  }, [filteredCandidates.length]);

  const title = mode.kind === 'freeAdd' ? '创建节点' : '创建并连接节点';
  const emptyText = mode.kind === 'freeAdd' ? '没有匹配节点' : '没有兼容节点';

  return (
    <div
      className="node-create-menu"
      style={{ left: mode.screenPosition.x, top: mode.screenPosition.y }}
      onClick={(event) => event.stopPropagation()}
      onKeyDown={(event) => {
        if (event.key === 'Escape') {
          event.preventDefault();
          onClose();
        } else if (event.key === 'ArrowDown') {
          event.preventDefault();
          setActiveIndex((current) => clamp(current + 1, 0, Math.max(0, filteredCandidates.length - 1)));
        } else if (event.key === 'ArrowUp') {
          event.preventDefault();
          setActiveIndex((current) => clamp(current - 1, 0, Math.max(0, filteredCandidates.length - 1)));
        } else if (event.key === 'Enter' && filteredCandidates[activeIndex]) {
          event.preventDefault();
          onPick(filteredCandidates[activeIndex]);
        }
      }}
    >
      <header className="node-create-menu-header">
        <strong>{title}</strong>
        {mode.kind === 'connectAdd' && <span>{labelForPortKind(mode.fromPortKind)} · {mode.fromSchema}</span>}
      </header>
      <input
        ref={inputRef}
        value={query}
        onChange={(event) => setQuery(event.target.value)}
        placeholder="搜索节点 / 端口 / schema"
        aria-label="搜索节点"
      />
      <div className="node-create-menu-list">
        {filteredCandidates.length === 0 ? (
          <div className="node-create-empty">{emptyText}</div>
        ) : filteredCandidates.map((candidate, index) => (
          <button
            key={`${candidate.definition.kind}-${candidate.compatiblePort?.id ?? 'free'}`}
            type="button"
            className={index === activeIndex ? 'active' : ''}
            onMouseEnter={() => setActiveIndex(index)}
            onClick={() => onPick(candidate)}
          >
            <strong>{candidate.definition.title}</strong>
            <span>{candidate.definition.category} · {candidate.definition.description}</span>
            {candidate.compatiblePort && <em>连接端口：{candidate.compatiblePort.label}</em>}
          </button>
        ))}
      </div>
    </div>
  );
}

function compatiblePortsForCreateMenu(definition: NodeDefinition, mode: Extract<NodeCreateMenuMode, { kind: 'connectAdd' }>, edges: FlowEdge[]): WorkflowPort[] {
  if (mode.fromDirection === 'input' && mode.fromCardinality === 'one') {
    const occupied = edges.some((edge) => edge.target === mode.fromNodeId && edge.targetHandle === mode.fromPortId);
    if (occupied) {
      return [];
    }
  }
  const fromPort: WorkflowPort = {
    id: mode.fromPortId,
    label: mode.fromPortId,
    direction: mode.fromDirection,
    kind: mode.fromPortKind,
    schema: mode.fromSchema,
    required: false,
    cardinality: mode.fromCardinality,
  };
  const candidatePorts = mode.fromDirection === 'output' ? definition.inputs : definition.outputs;
  return candidatePorts.filter((port) => (
    mode.fromDirection === 'output'
      ? validateConnectionKinds(fromPort, port) === null
      : validateConnectionKinds(port, fromPort) === null
  ));
}

function clampMenuPosition(x: number, y: number): { x: number; y: number } {
  const maxX = typeof window === 'undefined' ? x : window.innerWidth - 340;
  const maxY = typeof window === 'undefined' ? y : window.innerHeight - 420;
  return { x: clamp(x, 8, Math.max(8, maxX)), y: clamp(y, 8, Math.max(8, maxY)) };
}

function centerNodeAt(position: { x: number; y: number }): { x: number; y: number } {
  return { x: position.x - 90, y: position.y - 50 };
}

function backgroundGapForZoom(zoom: number): number {
  const safeZoom = Number.isFinite(zoom) && zoom > 0 ? zoom : 1;
  return clamp(GRID_TARGET_SCREEN_GAP_PX / safeZoom, GRID_MIN_GAP, GRID_MAX_GAP);
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

