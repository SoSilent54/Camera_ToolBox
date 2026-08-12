import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  Background,
  Controls,
  MiniMap,
  Panel,
  ReactFlow,
  addEdge,
  useEdgesState,
  useNodesState,
  type Connection,
  type Edge,
  type Node,
  type OnSelectionChangeParams,
  type ReactFlowInstance,
} from '@xyflow/react';
import {
  WORKFLOW_SCHEMA_VERSION,
  deleteWorkflow,
  exportWorkflow,
  importWorkflow,
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
import { Inspector, type Selection } from './Inspector';
import {
  FileBrowserNode,
  GenericWorkflowNode,
  ImageFileSourceNode,
  LocalWorkspaceNode,
  NodeLibraryItem,
  RtspSourceNode,
  SftpWorkspaceNode,
  SshSessionNode,
  ViewerNode,
  X5DeviceNode,
  X5RtspChannelNode,
  X5SnapshotNode,
} from './WorkflowNodes';

type FlowNode = Node<FlowNodeData>;
type FlowEdge = Edge<FlowEdgeData>;
const DEFAULT_RTSP_URL = 'rtsp://10.21.12.108:554/PRR';
const GENERIC_NODE_KINDS: NodeKind[] = [
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
  ['sftpWorkspace', SftpWorkspaceNode],
  ['fileBrowser', FileBrowserNode],
  ['imageFileSource', ImageFileSourceNode],
  ['sshSession', SshSessionNode],
  ['x5Device', X5DeviceNode],
  ['x5RtspChannel', X5RtspChannelNode],
  ['x5Snapshot', X5SnapshotNode],
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
    },
    [edges, setNodes],
  );

  useEffect(() => {
    setNodes((current) => current.map((flowNode) => (
      flowNode.data.onRtspUrlChange === handleRtspUrlChange
        && flowNode.data.onLocalImageConfigChange === handleLocalImageConfigChange
        && flowNode.data.onNodeConfigChange === handleNodeConfigChange
        ? flowNode
        : {
          ...flowNode,
          data: {
            ...flowNode.data,
            onRtspUrlChange: handleRtspUrlChange,
            onLocalImageConfigChange: handleLocalImageConfigChange,
            onNodeConfigChange: handleNodeConfigChange,
          },
        }
    )));
  }, [handleLocalImageConfigChange, handleNodeConfigChange, handleRtspUrlChange, nodes.length, setNodes]);

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
    const fallbackGraph = graph ? toWorkflowGraph(nodes, edges, graph) : emptyWorkflowGraph();
    const download = (workflow: WorkflowGraph) => {
      const blob = new Blob([`${JSON.stringify(workflow, null, 2)}\n`], { type: 'application/json;charset=utf-8' });
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement('a');
      anchor.href = url;
      anchor.download = `${workflow.id}.ctworkflow.json`;
      anchor.click();
      URL.revokeObjectURL(url);
    };
    if (!graph) {
      download(fallbackGraph);
      pushEvent('已下载本地草稿工作流');
      return;
    }
    exportWorkflow(graph.id)
      .then((exported) => {
        download(exported);
        pushEvent(`已导出工作流：${exported.id}`);
      })
      .catch((error: unknown) => {
        download(fallbackGraph);
        pushEvent(`导出失败，已下载本地草稿：${error instanceof Error ? error.message : String(error)}`);
      });
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
            <button onClick={handleImportWorkflow}>Import</button>
            <button onClick={handleExportWorkflow}>Export</button>
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
        <h2>Saved Workflows</h2>
        {savedWorkflows.length === 0 ? (
          <div className="rail-note">没有保存的工作流</div>
        ) : (
          savedWorkflows.map((item) => (
            <div key={item.id} className="saved-workflow-card">
              <strong>{item.title}</strong>
              <span>{item.id} · {item.revision}</span>
              <div className="saved-workflow-actions">
                <button type="button" onClick={() => loadSavedWorkflow(item.id).then((loaded) => applyGraph(loaded, `已载入保存工作流：${loaded.title}`)).catch((error: unknown) => pushEvent(`载入失败：${error instanceof Error ? error.message : String(error)}`))}>
                  Load
                </button>
                <button type="button" onClick={() => deleteWorkflow(item.id).then(() => { pushEvent(`已删除工作流：${item.id}`); refreshSavedWorkflows(); }).catch((error: unknown) => pushEvent(`删除失败：${error instanceof Error ? error.message : String(error)}`))}>
                  Delete
                </button>
              </div>
            </div>
          ))
        )}
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
  const selectedPathFor = (nodeId: string, visited: Set<string>): string | undefined => {
    if (visited.has(nodeId)) {
      return undefined;
    }
    visited.add(nodeId);
    const node = nodes.get(nodeId);
    if (!node) {
      return undefined;
    }
    if (node.kind === 'fileBrowser' && typeof node.config.selection === 'string' && node.config.selection.trim()) {
      return node.config.selection.trim();
    }
    for (const sourceNodeId of incoming.get(nodeId) ?? []) {
      const selection = selectedPathFor(sourceNodeId, visited);
      if (selection) {
        return selection;
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
    if (node.kind === 'imageFileSource') {
      const relativePath = typeof node.config.relativePath === 'string' && node.config.relativePath.trim()
        ? node.config.relativePath.trim()
        : selectedPathFor(nodeId, new Set());
      if (relativePath) {
        const workspaceRoot = workspaceRootFor(nodeId, new Set());
        if (workspaceRoot) {
          const query = new URLSearchParams({ workspaceRoot, relativePath });
          return { kind: 'local-image', url: `/api/images/local?${query}` };
        }
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
    state: kind === 'rtspSource' || kind === 'localWorkspace' || kind === 'sftpWorkspace' || kind === 'fileBrowser' || kind === 'sshSession' || kind === 'x5Device' ? 'ready' : 'idle',
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
