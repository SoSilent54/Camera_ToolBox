import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  Background,
  Controls,
  Handle,
  MiniMap,
  Panel,
  Position,
  ReactFlow,
  addEdge,
  useEdgesState,
  useNodesState,
  type Connection,
  type Edge,
  type Node,
  type NodeProps,
  type OnSelectionChangeParams,
  type ReactFlowInstance,
} from '@xyflow/react';
import {
  WORKFLOW_SCHEMA_VERSION,
  labelForPortKind,
  listWorkflows,
  loadNodeCatalog,
  loadRuntimeStatus,
  loadSavedWorkflow,
  loadWorkflow,
  loadWorkmodeTemplates,
  previewEepromProvision,
  previewI2cTransfer,
  runWorkflowRuntime,
  saveWorkflow,
  stopWorkflowRuntime,
  type ControlRequestPreview,
  type FlowEdgeData,
  type FlowNodeData,
  type NodeDefinition,
  type NodeKind,
  type PortKind,
  type RuntimeGraphStatus,
  type WorkflowEdge,
  type WorkflowGraph,
  type WorkflowNode,
  type WorkflowPort,
  type WorkmodeTemplate,
  type ViewerPreview,
  validateConnectionKinds,
} from './workflow';

type FlowNode = Node<FlowNodeData>;
type FlowEdge = Edge<FlowEdgeData>;
type Selection =
  | { type: 'node'; node: WorkflowNode }
  | { type: 'edge'; edge: FlowEdge }
  | { type: 'none' };

const DEFAULT_RTSP_URL = 'rtsp://10.21.12.108:554/PRR';
const GENERIC_NODE_KINDS: NodeKind[] = [
  'sftpWorkspace',
  'fileBrowser',
  'sshSession',
  'x5Device',
  'x5RtspChannel',
  'x5Snapshot',
  'rtspDecoder',
  'frameSampler',
  'imageLayer',
  'videoLayer',
  'overlayComposer',
  'chessboardDetector',
  'datasetCollector',
  'coverageAnalyzer',
  'captureScorer',
  'autoCaptureController',
  'poseGuide',
  'calibrationSolver',
  'reprojectionInspector',
  'calibrationExport',
  'i2cBusDiscovery',
  'i2cTransfer',
  'eepromMapLoader',
  'eepromProvision',
  'resultView',
];

const nodeTypes = Object.fromEntries([
  ['rtspSource', RtspSourceNode],
  ['localWorkspace', LocalWorkspaceNode],
  ['imageFileSource', ImageFileSourceNode],
  ['viewer', ViewerNode],
  ...GENERIC_NODE_KINDS.map((kind) => [kind, GenericWorkflowNode]),
]);

export function App() {
  const [graph, setGraph] = useState<WorkflowGraph | null>(null);
  const [catalog, setCatalog] = useState<NodeDefinition[]>([]);
  const [templates, setTemplates] = useState<WorkmodeTemplate[]>([]);
  const [savedWorkflows, setSavedWorkflows] = useState<Array<{ id: string; title: string; revision: string }>>([]);
  const [nodes, setNodes, onNodesChange] = useNodesState<FlowNode>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<FlowEdge>([]);
  const [selection, setSelection] = useState<Selection>({ type: 'none' });
  const [events, setEvents] = useState<string[]>(['等待 Workflow API...']);
  const [runtimeStatus, setRuntimeStatus] = useState<RuntimeGraphStatus | null>(null);
  const flowInstanceRef = useRef<ReactFlowInstance<FlowNode, FlowEdge> | null>(null);

  const pushEvent = useCallback((event: string) => {
    setEvents((current) => [event, ...current].slice(0, 10));
  }, []);

  const applyGraph = useCallback(
    (nextGraph: WorkflowGraph, event: string) => {
      setGraph(nextGraph);
      setNodes(toFlowNodes(nextGraph));
      setEdges(toFlowEdges(nextGraph));
      setSelection({ type: 'none' });
      setEvents([event, `节点 ${nextGraph.nodes.length} 个，连接 ${nextGraph.edges.length} 条`]);
      setRuntimeStatus(null);
    },
    [setEdges, setNodes],
  );

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
      return { ok: true, port: sourcePort };
    },
    [nodeById],
  );

  const onConnect = useCallback(
    (connection: Connection) => {
      const validation = canConnect(connection);
      if (!validation.ok) {
        pushEvent(`拒绝连接：${validation.reason}`);
        return;
      }
      const edgeId = `edge-${connection.source}-${connection.sourceHandle}-${connection.target}-${connection.targetHandle}`;
      setEdges((current) => {
        const nextEdges = addEdge(
          {
            ...connection,
            id: edgeId,
            animated: true,
            label: labelForPortKind(validation.port.kind),
            data: { kind: validation.port.kind, schema: validation.port.schema },
            className: 'workflow-edge',
          },
          current.filter((edge) => edge.id !== edgeId),
        );
        setNodes((currentNodes) => withViewerPreviews(currentNodes, nextEdges));
        return nextEdges;
      });
      pushEvent(`新增连接：${edgeId}`);
    },
    [canConnect, pushEvent, setEdges, setNodes],
  );

  const handleRtspUrlChange = useCallback(
    (nodeId: string, nextUrl: string) => {
      const trimmedUrl = nextUrl.trim();
      if (!trimmedUrl.startsWith('rtsp://') && !trimmedUrl.startsWith('rtsps://')) {
        pushEvent('拒绝 RTSP URL：必须使用 rtsp:// 或 rtsps://');
        return;
      }
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
      pushEvent(`RTSP URL 已更新：${trimmedUrl}`);
    },
    [edges, pushEvent, setNodes],
  );

  const handleLocalImageConfigChange = useCallback(
    (nodeId: string, field: 'root' | 'relativePath', nextValue: string) => {
      const value = nextValue.trim();
      setNodes((current) => {
        const updated = current.map((flowNode) => {
          if (flowNode.id !== nodeId) {
            return flowNode;
          }
          const workflowNode = flowNode.data.workflowNode;
          const expectedKind = field === 'root' ? 'localWorkspace' : 'imageFileSource';
          if (workflowNode.kind !== expectedKind) {
            return flowNode;
          }
          return {
            ...flowNode,
            data: {
              ...flowNode.data,
              workflowNode: {
                ...workflowNode,
                config: { ...workflowNode.config, [field]: value },
              },
            },
          };
        });
        return withViewerPreviews(updated, edges);
      });
      setSelection((current) => current.type === 'node' && current.node.id === nodeId
        ? { type: 'node', node: { ...current.node, config: { ...current.node.config, [field]: value } } }
        : current);
      pushEvent(field === 'root' ? '本地 workspace 根目录已更新' : '本地图像相对路径已更新');
    },
    [edges, pushEvent, setNodes],
  );

  const handleNodeTitleChange = useCallback(
    (nodeId: string, nextTitle: string) => {
      const title = nextTitle.trim();
      if (!title) {
        pushEvent('节点标题不能为空');
        return;
      }
      setNodes((current) => current.map((flowNode) => flowNode.id === nodeId
        ? { ...flowNode, data: { ...flowNode.data, workflowNode: { ...flowNode.data.workflowNode, title } } }
        : flowNode));
      setSelection((current) => current.type === 'node' && current.node.id === nodeId
        ? { type: 'node', node: { ...current.node, title } }
        : current);
      pushEvent(`节点已重命名：${title}`);
    },
    [pushEvent, setNodes],
  );

  const handleNodeConfigChange = useCallback(
    (nodeId: string, key: string, value: string | boolean) => {
      setNodes((current) => current.map((flowNode) => flowNode.id === nodeId
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
        : flowNode));
      setSelection((current) => current.type === 'node' && current.node.id === nodeId
        ? { type: 'node', node: { ...current.node, config: { ...current.node.config, [key]: value } } }
        : current);
    },
    [setNodes],
  );

  useEffect(() => {
    setNodes((current) => current.map((flowNode) => (
      flowNode.data.onRtspUrlChange === handleRtspUrlChange
        && flowNode.data.onLocalImageConfigChange === handleLocalImageConfigChange
        ? flowNode
        : {
          ...flowNode,
          data: {
            ...flowNode.data,
            onRtspUrlChange: handleRtspUrlChange,
            onLocalImageConfigChange: handleLocalImageConfigChange,
          },
        }
    )));
  }, [handleLocalImageConfigChange, handleRtspUrlChange, nodes.length, setNodes]);

  const handleAddNode = useCallback(
    (kind: NodeKind) => {
      const definition = catalogByKind.get(kind);
      const count = nodes.filter((node) => node.data.workflowNode.kind === kind).length + 1;
      const workflowNode = createWorkflowNode(kind, count, { x: 96 + (nodes.length % 4) * 56, y: 96 + nodes.length * 36 }, definition);
      const flowNode = toFlowNode(workflowNode);
      setNodes((current) => withViewerPreviews([...current, flowNode], edges));
      setSelection({ type: 'node', node: workflowNode });
      pushEvent(`新增节点：${workflowNode.title}`);
    },
    [catalogByKind, edges, nodes, pushEvent, setNodes],
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
    if (selection.type === 'edge') {
      const nextEdges = edges.filter((edge) => edge.id !== selection.edge.id);
      setEdges(nextEdges);
      setNodes((current) => withViewerPreviews(current, nextEdges));
      setSelection({ type: 'none' });
      pushEvent(`删除连线：${selection.edge.id}`);
      return;
    }
    const nodeId = selection.node.id;
    const nextEdges = edges.filter((edge) => edge.source !== nodeId && edge.target !== nodeId);
    setEdges(nextEdges);
    setNodes((current) => withViewerPreviews(current.filter((node) => node.id !== nodeId), nextEdges));
    setSelection({ type: 'none' });
    pushEvent(`删除节点：${selection.node.title}`);
  }, [edges, pushEvent, selection, setEdges, setNodes]);

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
    setNodes((current) => withViewerPreviews([...current, toFlowNode(duplicated)], edges));
    setSelection({ type: 'node', node: duplicated });
    pushEvent(`复制节点：${duplicated.title}`);
  }, [edges, nodes, pushEvent, selection, setNodes]);

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

  const handleRunWorkflow = useCallback(() => {
    if (!graph) {
      pushEvent('工作流尚未加载，无法启动运行时诊断');
      return;
    }
    const draft = toWorkflowGraph(nodes, edges, graph);
    runWorkflowRuntime(draft)
      .then((status) => {
        setRuntimeStatus(status);
        pushEvent(`RuntimeGraph 已启动：${status.nodes.filter((node) => node.state === 'running').length} 个安全节点运行中`);
      })
      .catch((error: unknown) => pushEvent(`启动 RuntimeGraph 失败：${error instanceof Error ? error.message : String(error)}`));
  }, [edges, graph, nodes, pushEvent]);

  const handleStopWorkflow = useCallback(() => {
    if (!graph) {
      pushEvent('工作流尚未加载，无法停止运行时诊断');
      return;
    }
    stopWorkflowRuntime(graph.id)
      .then((status) => {
        setRuntimeStatus(status);
        pushEvent('RuntimeGraph 已停止；所有节点均为空闲状态');
      })
      .catch((error: unknown) => pushEvent(`停止 RuntimeGraph 失败：${error instanceof Error ? error.message : String(error)}`));
  }, [graph, pushEvent]);

  const refreshRuntimeStatus = useCallback(() => {
    if (!graph) {
      return;
    }
    loadRuntimeStatus(graph.id)
      .then(setRuntimeStatus)
      .catch(() => setRuntimeStatus(null));
  }, [graph]);

  useEffect(() => {
    refreshRuntimeStatus();
  }, [refreshRuntimeStatus]);

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
    if (firstNode) {
      setSelection({ type: 'node', node: firstNode.data.workflowNode });
      return;
    }
    const firstEdge = params.edges[0] as FlowEdge | undefined;
    if (firstEdge) {
      setSelection({ type: 'edge', edge: firstEdge });
      return;
    }
    setSelection({ type: 'none' });
  }, []);

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
          </div>
          <div className="menu-group">
            <button onClick={loadSeedWorkflow}>Reset demo</button>
            <button onClick={handleDeleteSelection}>Delete</button>
            <button onClick={handleDuplicateSelection}>Duplicate</button>
          </div>
          <div className="menu-group">
            <button onClick={handleRunWorkflow}>Run</button>
            <button onClick={handleStopWorkflow}>Stop</button>
            <button onClick={handleFitView}>Fit</button>
          </div>
        </nav>
        <div className="service-pill">{nodes.length} nodes / {edges.length} edges</div>
      </header>

      <aside className="left-rail">
        <h2>Templates</h2>
        {templates.map((template) => (
          <button key={template.id} className="library-item" type="button" onClick={() => handleApplyTemplate(template)}>
            <strong>{template.title}</strong>
            <span>{template.description}</span>
          </button>
        ))}
        <h2>Node Library</h2>
        {groupCatalog(catalog).map(([category, definitions]) => (
          <section key={category} className="library-group">
            <h3>{category}</h3>
            {definitions.map((item) => <NodeLibraryItem key={item.kind} definition={item} onAdd={handleAddNode} />)}
          </section>
        ))}
        <div className="rail-note">模板只生成拓扑；SSH/X5/I²C/EEPROM 写入类节点必须后续在 Inspector 明确触发。</div>
      </aside>

      <main className="canvas-region">
        <ReactFlow
          nodes={nodes}
          edges={edges}
          nodeTypes={nodeTypes}
          onNodesChange={onNodesChange}
          onEdgesChange={onEdgesChange}
          onConnect={onConnect}
          onSelectionChange={onSelectionChange}
          onInit={(instance) => {
            flowInstanceRef.current = instance;
          }}
          fitView
          minZoom={0.2}
          maxZoom={1.8}
          proOptions={{ hideAttribution: true }}
        >
          <Background color="#1e293b" gap={28} size={1} />
          <MiniMap className="workflow-minimap" pannable zoomable nodeStrokeWidth={2} />
          <Controls className="workflow-controls" position="bottom-left" />
          <Panel position="top-left" className="canvas-panel">
            {selection.type === 'none' ? 'Select a node or edge' : `${selection.type}: ${selection.type === 'node' ? selection.node.title : selection.edge.id}`}
          </Panel>
        </ReactFlow>
      </main>

      <aside className="inspector">
        <Inspector
          events={events}
          selection={selection}
          onDeleteSelection={handleDeleteSelection}
          runtimeStatus={runtimeStatus}
          onDuplicateSelection={handleDuplicateSelection}
          onNodeTitleChange={handleNodeTitleChange}
          onNodeConfigChange={handleNodeConfigChange}
        />
      </aside>
    </div>
  );
}

function RtspSourceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const url = String(node.config.url ?? DEFAULT_RTSP_URL);
  const [draftUrl, setDraftUrl] = useState(url);
  useEffect(() => setDraftUrl(url), [url]);
  const applyUrl = () => nodeData.onRtspUrlChange?.(node.id, draftUrl);
  return (
    <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <div className="node-body">
        <label htmlFor={`${node.id}-url`}>RTSP URL</label>
        <input
          id={`${node.id}-url`}
          className="rtsp-url-input nodrag"
          value={draftUrl}
          spellCheck={false}
          onChange={(event) => setDraftUrl(event.target.value)}
          onBlur={applyUrl}
          onKeyDown={(event) => {
            if (event.key === 'Enter') {
              applyUrl();
              event.currentTarget.blur();
            }
          }}
        />
        <span>Transport: {String(node.config.transport ?? 'tcp')}</span>
      </div>
      <PortHandles node={node} />
    </section>
  );
}

function LocalWorkspaceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const root = typeof node.config.root === 'string' ? node.config.root : '';
  const [draftRoot, setDraftRoot] = useState(root);
  useEffect(() => setDraftRoot(root), [root]);
  const applyRoot = () => nodeData.onLocalImageConfigChange?.(node.id, 'root', draftRoot);
  return (
    <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <div className="node-body">
        <label htmlFor={`${node.id}-root`}>Workspace root</label>
        <input
          id={`${node.id}-root`}
          className="rtsp-url-input nodrag"
          value={draftRoot}
          placeholder="/absolute/path/to/workspace"
          spellCheck={false}
          onChange={(event) => setDraftRoot(event.target.value)}
          onBlur={applyRoot}
          onKeyDown={(event) => {
            if (event.key === 'Enter') {
              applyRoot();
              event.currentTarget.blur();
            }
          }}
        />
        <span>Explicit root; directories are never scanned.</span>
      </div>
      <PortHandles node={node} />
    </section>
  );
}

function ImageFileSourceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const relativePath = typeof node.config.relativePath === 'string' ? node.config.relativePath : '';
  const [draftPath, setDraftPath] = useState(relativePath);
  useEffect(() => setDraftPath(relativePath), [relativePath]);
  const applyPath = () => nodeData.onLocalImageConfigChange?.(node.id, 'relativePath', draftPath);
  return (
    <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <div className="node-body">
        <label htmlFor={`${node.id}-relative-path`}>Image path</label>
        <input
          id={`${node.id}-relative-path`}
          className="rtsp-url-input nodrag"
          value={draftPath}
          placeholder="images/example.png"
          spellCheck={false}
          onChange={(event) => setDraftPath(event.target.value)}
          onBlur={applyPath}
          onKeyDown={(event) => {
            if (event.key === 'Enter') {
              applyPath();
              event.currentTarget.blur();
            }
          }}
        />
        <span>Path is relative to the connected Local Workspace.</span>
      </div>
      <PortHandles node={node} />
    </section>
  );
}

function GenericWorkflowNode({ data, selected }: NodeProps) {
  const node = (data as FlowNodeData).workflowNode;
  return (
    <section className={`workflow-node generic-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <div className="node-body compact">
        <span>Kind: {node.kind}</span>
        <span>Category: {node.category}</span>
        <span>In: {node.inputs.length}</span>
        <span>Out: {node.outputs.length}</span>
      </div>
      <PortHandles node={node} />
    </section>
  );
}

function PortHandles({ node }: { node: WorkflowNode }) {
  return (
    <>
      {node.inputs.map((port, index) => (
        <Handle
          key={`in-${port.id}`}
          id={port.id}
          type="target"
          position={Position.Left}
          className="stream-handle"
          style={{ top: `${portOffset(index, node.inputs.length)}%` }}
          title={`${port.label}: ${port.kind}`}
        />
      ))}
      {node.outputs.map((port, index) => (
        <Handle
          key={`out-${port.id}`}
          id={port.id}
          type="source"
          position={Position.Right}
          className="stream-handle"
          style={{ top: `${portOffset(index, node.outputs.length)}%` }}
          title={`${port.label}: ${port.kind}`}
        />
      ))}
    </>
  );
}

function portOffset(index: number, total: number): number {
  if (total <= 1) {
    return 50;
  }
  return 24 + (index * 52) / (total - 1);
}

function ViewerNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const preview = nodeData.preview;
  return (
    <section className={`workflow-node viewer-node ${selected ? 'selected' : ''}`}>
      <PortHandles node={node} />
      <NodeHeader node={node} />
      {preview?.kind === 'rtsp' && (
        <MjpegPreview
          streamUrl={`/api/streams/mjpeg?url=${encodeURIComponent(preview.url)}&width=960&height=540`}
          previewUrl={preview.url}
        />
      )}
      {preview?.kind === 'local-image' && <LocalImagePreview imageUrl={preview.url} />}
      {!preview && <LocalImagePreview imageUrl={undefined} />}
      <div className="node-body compact">
        <span>Fit: {String(node.config.fitMode ?? 'contain')}</span>
        <span>Overlay: {String(node.config.overlay ?? 'status')}</span>
      </div>
    </section>
  );
}

interface ViewerMetrics {
  streamFps: number;
  sentFps: number;
  publishFps: number;
  decodedFps: number;
  renderFps: number;
  frameCount: number;
  bytes: number;
  lastFrameAgeMs: number | null;
  jpegBytes: number;
  jpegEncodeMs: number | null;
  codecMs: number | null;
  scaleMs: number | null;
  copyMs: number | null;
  decoderFrames: number;
  error: string | null;
}

interface MjpegPart {
  jpeg: Uint8Array<ArrayBufferLike>;
  headers: Map<string, string>;
  consumed: number;
}

interface ViewerTransform {
  scale: number;
  x: number;
  y: number;
}

function LocalImagePreview({ imageUrl }: { imageUrl: string | undefined }) {
  const [state, setState] = useState<'empty' | 'loading' | 'ready' | 'error'>(imageUrl ? 'loading' : 'empty');
  const [transform, setTransform] = useState<ViewerTransform>({ scale: 1, x: 0, y: 0 });
  const dragStartRef = useRef<{ pointerId: number; x: number; y: number; originX: number; originY: number } | null>(null);
  useEffect(() => {
    setState(imageUrl ? 'loading' : 'empty');
    setTransform({ scale: 1, x: 0, y: 0 });
  }, [imageUrl]);
  const zoomBy = (ratio: number) => setTransform((current) => ({ ...current, scale: clamp(current.scale * ratio, 0.25, 4) }));
  const fit = () => setTransform({ scale: 1, x: 0, y: 0 });
  return (
    <div className="viewer-panel nodrag nopan">
      <div
        className={`viewer-preview ${state}`}
        onWheel={(event) => {
          event.stopPropagation();
          event.preventDefault();
          zoomBy(event.deltaY < 0 ? 1.12 : 0.88);
        }}
        onDoubleClick={fit}
        onPointerDown={(event) => {
          dragStartRef.current = { pointerId: event.pointerId, x: event.clientX, y: event.clientY, originX: transform.x, originY: transform.y };
          event.currentTarget.setPointerCapture(event.pointerId);
        }}
        onPointerMove={(event) => {
          const drag = dragStartRef.current;
          if (!drag || drag.pointerId !== event.pointerId) {
            return;
          }
          setTransform((current) => ({ ...current, x: drag.originX + event.clientX - drag.x, y: drag.originY + event.clientY - drag.y }));
        }}
        onPointerUp={() => {
          dragStartRef.current = null;
        }}
      >
        {imageUrl && (
          <img
            src={imageUrl}
            alt="Local workspace image preview"
            style={{ transform: `translate(${transform.x}px, ${transform.y}px) scale(${transform.scale})` }}
            onLoad={() => setState('ready')}
            onError={() => setState('error')}
          />
        )}
        {state !== 'ready' && <div className="preview-grid" />}
      </div>
      <div className="viewer-toolbar">
        <button type="button" onClick={fit}>Fit</button>
        <button type="button" onClick={() => setTransform((current) => ({ ...current, scale: 1 }))}>1:1</button>
        <button type="button" onClick={() => zoomBy(1.2)}>Zoom +</button>
        <button type="button" onClick={() => zoomBy(0.8)}>Zoom -</button>
      </div>
      <div className="viewer-status">
        {state === 'ready' ? 'Local image via guarded workspace endpoint' : state === 'loading' ? 'Loading local image...' : state === 'error' ? 'Local image preview unavailable' : 'Connect a configured local image path'}
      </div>
    </div>
  );
}

function MjpegPreview({ streamUrl, previewUrl }: { streamUrl: string | undefined; previewUrl: string | undefined }) {
  const [streamState, setStreamState] = useState<'connecting' | 'playing' | 'error'>(streamUrl ? 'connecting' : 'error');
  const [frameUrl, setFrameUrl] = useState<string | undefined>();
  const [metrics, setMetrics] = useState<ViewerMetrics>(() => initialViewerMetrics());
  const [transform, setTransform] = useState<ViewerTransform>({ scale: 1, x: 0, y: 0 });
  const objectUrlRef = useRef<string | undefined>();
  const lastFrameAtRef = useRef<number | null>(null);
  const dragStartRef = useRef<{ pointerId: number; x: number; y: number; originX: number; originY: number } | null>(null);

  useEffect(() => {
    let cancelled = false;
    let animationFrame = 0;
    let renderedFrames = 0;
    let lastSampleAt = performance.now();
    const tick = (now: number) => {
      if (cancelled) {
        return;
      }
      renderedFrames += 1;
      if (now - lastSampleAt >= 1000) {
        const elapsedSeconds = (now - lastSampleAt) / 1000;
        const renderFps = renderedFrames / elapsedSeconds;
        const lastFrameAt = lastFrameAtRef.current;
        setMetrics((current) => ({ ...current, renderFps, lastFrameAgeMs: lastFrameAt === null ? null : now - lastFrameAt }));
        renderedFrames = 0;
        lastSampleAt = now;
      }
      animationFrame = requestAnimationFrame(tick);
    };
    animationFrame = requestAnimationFrame(tick);
    return () => {
      cancelled = true;
      cancelAnimationFrame(animationFrame);
    };
  }, []);

  useEffect(() => {
    if (objectUrlRef.current) {
      URL.revokeObjectURL(objectUrlRef.current);
      objectUrlRef.current = undefined;
    }
    if (!streamUrl) {
      setStreamState('error');
      setFrameUrl(undefined);
      lastFrameAtRef.current = null;
      setMetrics((current) => ({ ...initialViewerMetrics(), renderFps: current.renderFps }));
      return;
    }

    const abortController = new AbortController();
    let buffer: Uint8Array<ArrayBufferLike> = new Uint8Array();
    let bytes = 0;
    let frameCount = 0;
    let frameTimes: number[] = [];
    let publishTimesNs: number[] = [];
    let sentTimesNs: number[] = [];
    let decoderSamples: Array<{ time: number; frames: number }> = [];
    lastFrameAtRef.current = null;
    setStreamState('connecting');
    setFrameUrl(undefined);
    setMetrics((current) => ({ ...initialViewerMetrics(), renderFps: current.renderFps }));

    void (async () => {
      try {
        const response = await fetch(streamUrl, { signal: abortController.signal });
        if (!response.ok || !response.body) {
          throw new Error(`stream request failed: ${response.status} ${response.statusText}`);
        }
        const reader = response.body.getReader();
        while (!abortController.signal.aborted) {
          const { value, done } = await reader.read();
          if (done) {
            break;
          }
          if (!value) {
            continue;
          }
          bytes += value.byteLength;
          buffer = appendBytes(buffer, value);
          while (true) {
            const part = takeMjpegPart(buffer);
            if (!part) {
              break;
            }
            buffer = buffer.slice(part.consumed);
            const jpegPart = new Uint8Array(part.jpeg.byteLength);
            jpegPart.set(part.jpeg);
            const nextObjectUrl = URL.createObjectURL(new Blob([jpegPart.buffer], { type: 'image/jpeg' }));
            if (objectUrlRef.current) {
              URL.revokeObjectURL(objectUrlRef.current);
            }
            objectUrlRef.current = nextObjectUrl;
            setFrameUrl(nextObjectUrl);
            setStreamState('playing');
            const now = performance.now();
            frameCount += 1;
            lastFrameAtRef.current = now;
            frameTimes = [...frameTimes.filter((time) => now - time <= 2000), now];
            const publishedNs = numberHeader(part.headers, 'x-frame-published-at-ns');
            if (publishedNs !== null) {
              publishTimesNs = [...publishTimesNs.filter((time) => publishedNs - time <= 2_000_000_000), publishedNs];
            }
            const sentNs = numberHeader(part.headers, 'x-mjpeg-sent-at-ns');
            if (sentNs !== null) {
              sentTimesNs = [...sentTimesNs.filter((time) => sentNs - time <= 2_000_000_000), sentNs];
            }
            const decoderFrames = numberHeader(part.headers, 'x-decoder-frames') ?? 0;
            decoderSamples = [...decoderSamples.filter((sample) => now - sample.time <= 2000), { time: now, frames: decoderFrames }];
            const windowMs = frameTimes.length > 1 ? frameTimes[frameTimes.length - 1] - frameTimes[0] : 1000;
            const publishWindowNs = publishTimesNs.length > 1 ? publishTimesNs[publishTimesNs.length - 1] - publishTimesNs[0] : 1_000_000_000;
            const sentWindowNs = sentTimesNs.length > 1 ? sentTimesNs[sentTimesNs.length - 1] - sentTimesNs[0] : 1_000_000_000;
            const decoderWindowMs = decoderSamples.length > 1 ? decoderSamples[decoderSamples.length - 1].time - decoderSamples[0].time : 1000;
            const decoderFrameDelta = decoderSamples.length > 1 ? decoderSamples[decoderSamples.length - 1].frames - decoderSamples[0].frames : 0;
            setMetrics((current) => ({
              ...current,
              streamFps: frameTimes.length > 1 ? ((frameTimes.length - 1) * 1000) / Math.max(windowMs, 1) : 0,
              sentFps: sentTimesNs.length > 1 ? ((sentTimesNs.length - 1) * 1_000_000_000) / Math.max(sentWindowNs, 1) : 0,
              publishFps: publishTimesNs.length > 1 ? ((publishTimesNs.length - 1) * 1_000_000_000) / Math.max(publishWindowNs, 1) : 0,
              decodedFps: decoderSamples.length > 1 ? (decoderFrameDelta * 1000) / Math.max(decoderWindowMs, 1) : 0,
              frameCount,
              bytes,
              lastFrameAgeMs: 0,
              jpegBytes: numberHeader(part.headers, 'x-mjpeg-jpeg-bytes') ?? part.jpeg.byteLength,
              jpegEncodeMs: nsToMs(numberHeader(part.headers, 'x-mjpeg-encode-ns')),
              codecMs: averageStageMs(part.headers, 'x-decoder-codec-ns', decoderFrames),
              scaleMs: averageStageMs(part.headers, 'x-decoder-scale-ns', decoderFrames),
              copyMs: averageStageMs(part.headers, 'x-decoder-copy-ns', decoderFrames),
              decoderFrames,
              error: null,
            }));
          }
        }
      } catch (error) {
        if (!abortController.signal.aborted) {
          const message = error instanceof Error ? error.message : String(error);
          setStreamState('error');
          setMetrics((current) => ({ ...current, error: message }));
        }
      }
    })();

    return () => abortController.abort();
  }, [streamUrl]);

  useEffect(() => () => {
    if (objectUrlRef.current) {
      URL.revokeObjectURL(objectUrlRef.current);
    }
  }, []);

  const statusText = !streamUrl
    ? 'No connected RTSP source'
    : streamState === 'playing'
      ? 'MJPEG preview via internal ffmpeg-next'
      : streamState === 'connecting'
        ? 'Connecting RTSP via internal decoder...'
        : 'Preview stream unavailable';

  const zoomBy = (ratio: number) => setTransform((current) => ({ ...current, scale: clamp(current.scale * ratio, 0.25, 4) }));
  const fit = () => setTransform({ scale: 1, x: 0, y: 0 });

  return (
    <div className="viewer-panel nodrag nopan">
      <div
        className={`viewer-preview ${streamState}`}
        onWheel={(event) => {
          event.stopPropagation();
          event.preventDefault();
          zoomBy(event.deltaY < 0 ? 1.12 : 0.88);
        }}
        onDoubleClick={fit}
        onPointerDown={(event) => {
          dragStartRef.current = { pointerId: event.pointerId, x: event.clientX, y: event.clientY, originX: transform.x, originY: transform.y };
          event.currentTarget.setPointerCapture(event.pointerId);
        }}
        onPointerMove={(event) => {
          const drag = dragStartRef.current;
          if (!drag || drag.pointerId !== event.pointerId) {
            return;
          }
          setTransform((current) => ({ ...current, x: drag.originX + event.clientX - drag.x, y: drag.originY + event.clientY - drag.y }));
        }}
        onPointerUp={() => {
          dragStartRef.current = null;
        }}
      >
        {frameUrl && (
          <img
            src={frameUrl}
            alt={`Preview from ${previewUrl}`}
            style={{ transform: `translate(${transform.x}px, ${transform.y}px) scale(${transform.scale})` }}
          />
        )}
        {streamState !== 'playing' && <div className="preview-grid" />}
      </div>
      <div className="viewer-toolbar">
        <button type="button" onClick={fit}>Fit</button>
        <button type="button" onClick={() => setTransform((current) => ({ ...current, scale: 1 }))}>1:1</button>
        <button type="button" onClick={() => zoomBy(1.2)}>Zoom +</button>
        <button type="button" onClick={() => zoomBy(0.8)}>Zoom -</button>
      </div>
      <div className="viewer-status">{statusText}</div>
      <dl className="viewer-metrics">
        <div><dt>browser</dt><dd>{metrics.streamFps.toFixed(1)} fps</dd></div>
        <div><dt>sent</dt><dd>{metrics.sentFps.toFixed(1)} fps</dd></div>
        <div><dt>publish</dt><dd>{metrics.publishFps.toFixed(1)} fps</dd></div>
        <div><dt>decoded</dt><dd>{metrics.decodedFps.toFixed(1)} fps</dd></div>
        <div><dt>ui raf</dt><dd>{metrics.renderFps.toFixed(1)} fps</dd></div>
        <div><dt>parts</dt><dd>{metrics.frameCount}</dd></div>
        <div><dt>decoded n</dt><dd>{metrics.decoderFrames}</dd></div>
        <div><dt>jpeg</dt><dd>{formatBytes(metrics.jpegBytes)}</dd></div>
        <div><dt>bytes</dt><dd>{formatBytes(metrics.bytes)}</dd></div>
        <div><dt>age</dt><dd>{metrics.lastFrameAgeMs === null ? 'n/a' : `${Math.round(metrics.lastFrameAgeMs)} ms`}</dd></div>
        <div><dt>enc</dt><dd>{formatMs(metrics.jpegEncodeMs)}</dd></div>
        <div><dt>codec</dt><dd>{formatMs(metrics.codecMs)}</dd></div>
        <div><dt>scale</dt><dd>{formatMs(metrics.scaleMs)}</dd></div>
        <div><dt>copy</dt><dd>{formatMs(metrics.copyMs)}</dd></div>
        {metrics.error && <div className="metric-error"><dt>error</dt><dd>{metrics.error}</dd></div>}
      </dl>
    </div>
  );
}

function initialViewerMetrics(): ViewerMetrics {
  return {
    streamFps: 0,
    sentFps: 0,
    publishFps: 0,
    decodedFps: 0,
    renderFps: 0,
    frameCount: 0,
    bytes: 0,
    lastFrameAgeMs: null,
    jpegBytes: 0,
    jpegEncodeMs: null,
    codecMs: null,
    scaleMs: null,
    copyMs: null,
    decoderFrames: 0,
    error: null,
  };
}

function appendBytes(existing: Uint8Array<ArrayBufferLike>, incoming: Uint8Array<ArrayBufferLike>): Uint8Array<ArrayBuffer> {
  const merged = new Uint8Array(existing.length + incoming.length);
  merged.set(existing);
  merged.set(incoming, existing.length);
  return merged;
}

function takeMjpegPart(buffer: Uint8Array<ArrayBufferLike>): MjpegPart | undefined {
  const boundary = indexOfAscii(buffer, '--frame');
  if (boundary < 0) {
    return undefined;
  }
  const headerEnd = indexOfAscii(buffer, '\r\n\r\n', boundary);
  if (headerEnd < 0) {
    return undefined;
  }
  const headerBytes = buffer.slice(boundary, headerEnd);
  const headers = parseMjpegHeaders(headerBytes);
  const jpegStart = headerEnd + 4;
  const contentLength = numberHeader(headers, 'content-length');
  if (contentLength !== null) {
    const jpegEnd = jpegStart + contentLength;
    if (buffer.length < jpegEnd) {
      return undefined;
    }
    const consumed = buffer[jpegEnd] === 13 && buffer[jpegEnd + 1] === 10 ? jpegEnd + 2 : jpegEnd;
    return { jpeg: buffer.slice(jpegStart, jpegEnd), headers, consumed };
  }
  const end = findJpegEnd(buffer, jpegStart + 2);
  return end >= 0 ? { jpeg: buffer.slice(jpegStart, end + 2), headers, consumed: end + 2 } : undefined;
}

function parseMjpegHeaders(headerBytes: Uint8Array<ArrayBufferLike>): Map<string, string> {
  const text = new TextDecoder('ascii').decode(headerBytes);
  const headers = new Map<string, string>();
  for (const line of text.split('\r\n')) {
    const separator = line.indexOf(':');
    if (separator <= 0) {
      continue;
    }
    headers.set(line.slice(0, separator).trim().toLowerCase(), line.slice(separator + 1).trim());
  }
  return headers;
}

function indexOfAscii(buffer: Uint8Array<ArrayBufferLike>, needle: string, start = 0): number {
  const bytes = [...needle].map((char) => char.charCodeAt(0));
  for (let index = start; index + bytes.length <= buffer.length; index += 1) {
    if (bytes.every((byte, offset) => buffer[index + offset] === byte)) {
      return index;
    }
  }
  return -1;
}

function findJpegEnd(buffer: Uint8Array<ArrayBufferLike>, start = 0): number {
  for (let index = start; index + 1 < buffer.length; index += 1) {
    if (buffer[index] === 0xff && buffer[index + 1] === 0xd9) {
      return index;
    }
  }
  return -1;
}

function numberHeader(headers: Map<string, string>, key: string): number | null {
  const raw = headers.get(key);
  if (!raw) {
    return null;
  }
  const parsed = Number(raw);
  return Number.isFinite(parsed) ? parsed : null;
}

function averageStageMs(headers: Map<string, string>, key: string, frames: number): number | null {
  const totalNs = numberHeader(headers, key);
  return totalNs === null || frames <= 0 ? null : nsToMs(totalNs / frames);
}

function nsToMs(ns: number | null): number | null {
  return ns === null ? null : ns / 1_000_000;
}

function formatMs(ms: number | null): string {
  return ms === null ? 'n/a' : `${ms.toFixed(1)} ms`;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KiB`;
  }
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

function NodeHeader({ node }: { node: WorkflowNode }) {
  return (
    <header className="node-header">
      <span>{node.title}</span>
      <small className={`state-dot ${node.state}`}>{node.state}</small>
    </header>
  );
}

function NodeLibraryItem({ definition, onAdd }: { definition: NodeDefinition; onAdd: (kind: NodeKind) => void }) {
  return (
    <button className="library-item" type="button" onClick={() => onAdd(definition.kind)}>
      <strong>{definition.title}</strong>
      <span>{definition.description}</span>
    </button>
  );
}

function Inspector({
  events,
  selection,
  runtimeStatus,
  onDeleteSelection,
  onDuplicateSelection,
  onNodeTitleChange,
  onNodeConfigChange,
}: {
  events: string[];
  selection: Selection;
  runtimeStatus: RuntimeGraphStatus | null;
  onDeleteSelection: () => void;
  onDuplicateSelection: () => void;
  onNodeTitleChange: (nodeId: string, title: string) => void;
  onNodeConfigChange: (nodeId: string, key: string, value: string | boolean) => void;
}) {
  if (selection.type === 'none') {
    return (
      <div>
        <h2>Inspector</h2>
        <p className="muted">选择节点或连线后显示参数。</p>
        <RuntimeDiagnostics status={runtimeStatus} />
        <InspectorEvents events={events} />
      </div>
    );
  }
  if (selection.type === 'edge') {
    return (
      <div>
        <h2>Edge</h2>
        <div className="inspector-actions">
          <button type="button" onClick={onDeleteSelection}>Delete edge</button>
        </div>
        <KeyValue label="ID" value={selection.edge.id} />
        <KeyValue label="Source" value={`${selection.edge.source}:${selection.edge.sourceHandle ?? ''}`} />
        <KeyValue label="Target" value={`${selection.edge.target}:${selection.edge.targetHandle ?? ''}`} />
        <KeyValue label="Kind" value={labelForPortKind(selection.edge.data?.kind ?? 'endpoint.rtsp')} />
        <KeyValue label="Schema" value={selection.edge.data?.schema ?? 'n/a'} />
        <RuntimeDiagnostics status={runtimeStatus} />
        <InspectorEvents events={events} />
      </div>
    );
  }
  const node = selection.node;
  const nodeRuntime = runtimeStatus?.nodes.find((status) => status.nodeId === node.id);
  return (
    <div>
      <h2>{node.title}</h2>
      <div className="inspector-actions">
        <button type="button" onClick={onDuplicateSelection}>Duplicate</button>
        <button type="button" onClick={onDeleteSelection}>Delete</button>
      </div>
      <label className="field-label" htmlFor={`${node.id}-title`}>Title</label>
      <input
        id={`${node.id}-title`}
        className="inspector-input"
        defaultValue={node.title}
        onBlur={(event) => onNodeTitleChange(node.id, event.currentTarget.value)}
        onKeyDown={(event) => {
          if (event.key === 'Enter') {
            onNodeTitleChange(node.id, event.currentTarget.value);
            event.currentTarget.blur();
          }
        }}
      />
      <KeyValue label="Kind" value={node.kind} />
      <KeyValue label="Category" value={node.category} />
      <KeyValue label="State" value={node.state} />
      <KeyValue label="Runtime" value={nodeRuntime?.state ?? 'not started'} />
      {nodeRuntime && <KeyValue label="Runtime diagnostic" value={nodeRuntime.diagnostic} />}
      <h3>Ports</h3>
      {[...node.inputs, ...node.outputs].map((port) => (
        <KeyValue key={`${port.direction}-${port.id}`} label={`${port.direction}:${port.id}`} value={`${port.kind} / ${port.schema}`} />
      ))}
      <h3>Config</h3>
      <pre>{JSON.stringify(node.config, null, 2)}</pre>
      {(node.kind === 'i2cTransfer' || node.kind === 'eepromProvision') && (
        <ControlPreviewPanel node={node} onNodeConfigChange={onNodeConfigChange} />
      )}
      <InspectorEvents events={events} />
      <RuntimeDiagnostics status={runtimeStatus} nodeId={node.id} />
    </div>
  );
}

function RuntimeDiagnostics({ status, nodeId }: { status: RuntimeGraphStatus | null; nodeId?: string }) {
  if (!status) {
    return (
      <section className="inspector-events">
        <h3>Runtime</h3>
        <p className="muted">尚未启动 RuntimeGraph。</p>
      </section>
    );
  }
  const events = nodeId
    ? status.events.filter((event) => event.nodeId === nodeId)
    : status.events;
  return (
    <section className="inspector-events">
      <h3>Runtime</h3>
      <KeyValue label="Session" value={status.running ? 'running' : 'stopped'} />
      <ol>
        {events.map((event) => <li key={`${event.nodeId}-${event.message}`}><strong>{event.level}</strong> · {event.nodeId}: {event.message}</li>)}
      </ol>
    </section>
  );
}

function ControlPreviewPanel({
  node,
  onNodeConfigChange,
}: {
  node: WorkflowNode;
  onNodeConfigChange: (nodeId: string, key: string, value: string | boolean) => void;
}) {
  const [preview, setPreview] = useState<ControlRequestPreview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const isEeprom = node.kind === 'eepromProvision';
  const configText = (key: string, fallback: string): string => {
    const value = node.config[key];
    return typeof value === 'string' || typeof value === 'number' ? String(value) : fallback;
  };

  useEffect(() => {
    setPreview(null);
    setError(null);
  }, [node.id]);

  const requestPreview = async () => {
    try {
      setLoading(true);
      setError(null);
      const address = parseControlInteger(configText('address', '0x50'), 'Address');
      const register = parseControlInteger(configText('register', '0x0000'), 'Register');
      const pageSize = parseControlInteger(configText('pageSize', '16'), 'Page size');
      const payload = parseHexPayload(configText('payload', ''));
      const common = {
        nodeId: node.id,
        profileId: configText('profileId', ''),
        bus: configText('bus', ''),
        address,
        register,
        payload,
        pageSize,
      };
      const result = isEeprom
        ? await previewEepromProvision({
          ...common,
          mapId: configText('mapId', ''),
          verifyAfterWrite: node.config.verifyAfterWrite === true,
        })
        : await previewI2cTransfer({
          ...common,
          operation: configText('mode', 'read') === 'write' ? 'write' : 'read',
        });
      setPreview(result);
    } catch (previewError) {
      setPreview(null);
      setError(previewError instanceof Error ? previewError.message : String(previewError));
    } finally {
      setLoading(false);
    }
  };

  return (
    <section className="control-preview">
      <h3>安全请求预览</h3>
      <p className="muted">配置只会保存为节点轻量参数。点击预览只校验请求，绝不连接 SSH 或 I²C。</p>
      <ControlConfigField id={`${node.id}-profile`} label="Session profile" value={configText('profileId', '')} onChange={(value) => onNodeConfigChange(node.id, 'profileId', value)} />
      <ControlConfigField id={`${node.id}-bus`} label="I²C bus" value={configText('bus', 'i2c-1')} onChange={(value) => onNodeConfigChange(node.id, 'bus', value)} />
      <ControlConfigField id={`${node.id}-address`} label="Address (hex)" value={configText('address', '0x50')} onChange={(value) => onNodeConfigChange(node.id, 'address', value)} />
      <ControlConfigField id={`${node.id}-register`} label="Register (hex)" value={configText('register', '0x0000')} onChange={(value) => onNodeConfigChange(node.id, 'register', value)} />
      <ControlConfigField id={`${node.id}-payload`} label="Payload (hex bytes)" value={configText('payload', '')} onChange={(value) => onNodeConfigChange(node.id, 'payload', value)} />
      <ControlConfigField id={`${node.id}-page-size`} label="EEPROM page size" value={configText('pageSize', '16')} onChange={(value) => onNodeConfigChange(node.id, 'pageSize', value)} />
      {isEeprom ? (
        <>
          <ControlConfigField id={`${node.id}-map`} label="EEPROM map" value={configText('mapId', '')} onChange={(value) => onNodeConfigChange(node.id, 'mapId', value)} />
          <label className="control-checkbox">
            <input type="checkbox" checked={node.config.verifyAfterWrite === true} onChange={(event) => onNodeConfigChange(node.id, 'verifyAfterWrite', event.currentTarget.checked)} />
            Verify after write (preview only)
          </label>
        </>
      ) : (
        <label className="field-label" htmlFor={`${node.id}-mode`}>
          Operation
          <select id={`${node.id}-mode`} className="inspector-input" value={configText('mode', 'read')} onChange={(event) => onNodeConfigChange(node.id, 'mode', event.currentTarget.value)}>
            <option value="read">Read</option>
            <option value="write">Write</option>
          </select>
        </label>
      )}
      <div className="inspector-actions">
        <button type="button" onClick={() => void requestPreview()} disabled={loading}>{loading ? 'Validating…' : 'Preview request'}</button>
        <button type="button" disabled title="Execution is intentionally unavailable in Workflow Web">Execute disabled</button>
      </div>
      {error && <p className="control-preview-error">Preview rejected: {error}</p>}
      {preview && (
        <div className="control-preview-result">
          <KeyValue label="Mode" value={preview.operation} />
          <KeyValue label="Execution" value={preview.execution} />
          <KeyValue label="Node" value={preview.target.nodeId} />
          <KeyValue label="Profile" value={preview.target.profileId} />
          <KeyValue label="Bus" value={preview.target.bus} />
          <KeyValue label="Address" value={`0x${preview.target.address.toString(16).padStart(2, '0')}`} />
          <KeyValue label="Register" value={`0x${preview.target.register.toString(16).padStart(4, '0')}`} />
          <KeyValue label="Payload" value={preview.target.payload.map((byte) => byte.toString(16).padStart(2, '0')).join(' ') || '(empty)'} />
          {preview.mapId && <KeyValue label="EEPROM map" value={preview.mapId} />}
          {preview.verifyAfterWrite !== null && <KeyValue label="Verify after write" value={preview.verifyAfterWrite ? 'yes' : 'no'} />}
          <KeyValue label="Page split" value={`${preview.pageSplitEstimate.writeCount} write(s), ${preview.pageSplitEstimate.pageSize} B/page`} />
          {preview.pageSplitEstimate.segments.length > 0 && <pre>{JSON.stringify(preview.pageSplitEstimate.segments, null, 2)}</pre>}
          {preview.requiresConfirmation && <p className="control-confirmation">Write-like operation: explicit confirmation is required before any future execution path.</p>}
        </div>
      )}
    </section>
  );
}

function ControlConfigField({
  id,
  label,
  value,
  onChange,
}: {
  id: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
}) {
  return (
    <label className="field-label" htmlFor={id}>
      {label}
      <input id={id} className="inspector-input" value={value} onChange={(event) => onChange(event.currentTarget.value)} />
    </label>
  );
}

/** 接受十进制或 0x 前缀十六进制，保留服务端的范围校验。 */
function parseControlInteger(value: string, label: string): number {
  const text = value.trim();
  if (!/^(?:0x[0-9a-f]+|\d+)$/i.test(text)) {
    throw new Error(`${label} must be a decimal or 0x-prefixed hexadecimal integer`);
  }
  const parsed = Number(text);
  if (!Number.isSafeInteger(parsed) || parsed < 0) {
    throw new Error(`${label} must be a non-negative integer`);
  }
  return parsed;
}

/** 将空格或逗号分隔的字节文本转换为 JSON 数组，避免把原始文本混入请求。 */
function parseHexPayload(value: string): number[] {
  const text = value.trim();
  if (!text) {
    return [];
  }
  return text.split(/[\s,]+/).map((token) => {
    if (!/^(?:0x)?[0-9a-f]{1,2}$/i.test(token)) {
      throw new Error(`Invalid payload byte: ${token}`);
    }
    return Number.parseInt(token.replace(/^0x/i, ''), 16);
  });
}


function InspectorEvents({ events }: { events: string[] }) {
  return (
    <section className="inspector-events">
      <h3>Events</h3>
      <ol>
        {events.map((event, index) => <li key={`${event}-${index}`}>{event}</li>)}
      </ol>
    </section>
  );
}

function KeyValue({ label, value }: { label: string; value: string }) {
  return (
    <div className="key-value">
      <span>{label}</span>
      <strong>{value}</strong>
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
      preview: node.kind === 'viewer' ? viewerPreview(graph, node.id) : undefined,
    },
  }));
}

/** 沿已连接的端口反向查找可预览的源；只把浏览器 URL 留在 React Flow 运行时状态。 */
function viewerPreview(graph: WorkflowGraph, viewerNodeId: string): ViewerPreview | undefined {
  const nodes = new Map(graph.nodes.map((node) => [node.id, node]));
  const incoming = new Map<string, string[]>();
  for (const edge of graph.edges) {
    incoming.set(edge.target.nodeId, [...(incoming.get(edge.target.nodeId) ?? []), edge.source.nodeId]);
  }
  const workspaceRootFor = (nodeId: string, visited: Set<string>): string | undefined => {
    if (visited.has(nodeId)) {
      return undefined;
    }
    visited.add(nodeId);
    const node = nodes.get(nodeId);
    if (!node) {
      return undefined;
    }
    if (node.kind === 'localWorkspace' && typeof node.config.root === 'string' && node.config.root.trim()) {
      return node.config.root.trim();
    }
    for (const sourceNodeId of incoming.get(nodeId) ?? []) {
      const root = workspaceRootFor(sourceNodeId, visited);
      if (root) {
        return root;
      }
    }
    return undefined;
  };
  const visit = (nodeId: string, visited: Set<string>): ViewerPreview | undefined => {
    if (visited.has(nodeId)) {
      return undefined;
    }
    visited.add(nodeId);
    const node = nodes.get(nodeId);
    if (!node) {
      return undefined;
    }
    if (node.kind === 'rtspSource' && typeof node.config.url === 'string' && node.config.url.trim()) {
      return { kind: 'rtsp', url: node.config.url.trim() };
    }
    if (node.kind === 'imageFileSource' && typeof node.config.relativePath === 'string' && node.config.relativePath.trim()) {
      const workspaceRoot = workspaceRootFor(nodeId, new Set());
      if (workspaceRoot) {
        const query = new URLSearchParams({ workspaceRoot, relativePath: node.config.relativePath.trim() });
        return { kind: 'local-image', url: `/api/images/local?${query}` };
      }
    }
    for (const sourceNodeId of incoming.get(nodeId) ?? []) {
      const preview = visit(sourceNodeId, visited);
      if (preview) {
        return preview;
      }
    }
    return undefined;
  };
  return visit(viewerNodeId, new Set());
}

function toFlowEdges(graph: WorkflowGraph): FlowEdge[] {
  return graph.edges.map((edge) => ({
    id: edge.id,
    source: edge.source.nodeId,
    sourceHandle: edge.source.portId,
    target: edge.target.nodeId,
    targetHandle: edge.target.portId,
    animated: true,
    label: labelForPortKind(edge.kind),
    data: { workflowEdge: edge, kind: edge.kind, schema: edge.schema },
    className: 'workflow-edge',
  }));
}

function createGraphId(): string {
  return `workflow-${crypto.randomUUID()}`;
}

function createNodeId(kind: NodeKind): string {
  return `${kind}-${crypto.randomUUID()}`;
}

function createWorkflowNode(kind: NodeKind, count: number, position: { x: number; y: number }, definition: NodeDefinition | undefined): WorkflowNode {
  return {
    id: createNodeId(kind),
    kind,
    title: `${definition?.title ?? kind} ${count}`,
    position,
    state: kind === 'rtspSource' || kind === 'localWorkspace' || kind === 'sshSession' || kind === 'x5Device' ? 'ready' : 'idle',
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
      schemaVersion: WORKFLOW_SCHEMA_VERSION,
    })),
  };
}

function withViewerPreviews(nodes: FlowNode[], edges: FlowEdge[]): FlowNode[] {
  const graph = toWorkflowGraph(nodes, edges, emptyWorkflowGraph());
  return nodes.map((node) => node.data.workflowNode.kind === 'viewer'
    ? { ...node, data: { ...node.data, preview: viewerPreview(graph, node.id) } }
    : node);
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
