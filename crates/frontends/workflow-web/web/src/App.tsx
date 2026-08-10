import { useCallback, useEffect, useMemo, useState } from 'react';
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
} from '@xyflow/react';
import {
  labelForDataKind,
  loadWorkflow,
  type FlowEdgeData,
  type FlowNodeData,
  type WorkflowGraph,
  type WorkflowNode,
  type WorkflowPort,
} from './workflow';

type FlowNode = Node<FlowNodeData>;
type FlowEdge = Edge<FlowEdgeData>;
type Selection =
  | { type: 'node'; node: WorkflowNode }
  | { type: 'edge'; edge: FlowEdge }
  | { type: 'none' };

const nodeTypes = {
  rtspSource: RtspSourceNode,
  viewer: ViewerNode,
};

export function App() {
  const [graph, setGraph] = useState<WorkflowGraph | null>(null);
  const [nodes, setNodes, onNodesChange] = useNodesState<FlowNode>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<FlowEdge>([]);
  const [selection, setSelection] = useState<Selection>({ type: 'none' });
  const [events, setEvents] = useState<string[]>(['等待 Workflow API...']);

  useEffect(() => {
    let alive = true;
    loadWorkflow()
      .then((loaded) => {
        if (!alive) {
          return;
        }
        setGraph(loaded);
        setNodes(toFlowNodes(loaded));
        setEdges(toFlowEdges(loaded));
        setEvents([
          `已加载 ${loaded.title}`,
          `节点 ${loaded.nodes.length} 个，连接 ${loaded.edges.length} 条`,
        ]);
      })
      .catch((error: unknown) => {
        if (!alive) {
          return;
        }
        const message = error instanceof Error ? error.message : String(error);
        setEvents([`加载失败：${message}`]);
      });
    return () => {
      alive = false;
    };
  }, [setEdges, setNodes]);

  const nodeById = useMemo(() => {
    const map = new Map<string, WorkflowNode>();
    nodes.forEach((node) => map.set(node.id, node.data.workflowNode));
    return map;
  }, [nodes]);

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
      if (sourcePort.dataKind !== targetPort.dataKind) {
        return {
          ok: false,
          reason: `${labelForDataKind(sourcePort.dataKind)} 不能连接到 ${labelForDataKind(targetPort.dataKind)}`,
        };
      }
      return { ok: true, port: sourcePort };
    },
    [nodeById],
  );

  const onConnect = useCallback(
    (connection: Connection) => {
      const validation = canConnect(connection);
      if (!validation.ok) {
        setEvents((current) => [`拒绝连接：${validation.reason}`, ...current].slice(0, 6));
        return;
      }
      const edgeId = `edge-${connection.source}-${connection.sourceHandle}-${connection.target}-${connection.targetHandle}`;
      setEdges((current) =>
        addEdge(
          {
            ...connection,
            id: edgeId,
            animated: true,
            label: labelForDataKind(validation.port.dataKind),
            data: { dataKind: validation.port.dataKind },
            className: 'workflow-edge',
          },
          current.filter((edge) => edge.id !== edgeId),
        ),
      );
      setEvents((current) => [`新增连接：${edgeId}`, ...current].slice(0, 6));
    },
    [canConnect, setEdges],
  );

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
          <h1>Workflow Web</h1>
        </div>
        <nav className="top-menu" aria-label="Workflow menu">
          <button>File</button>
          <button>Workspace</button>
          <button>Run</button>
          <button>View</button>
        </nav>
        <div className="service-pill">Browser service</div>
      </header>

      <aside className="left-rail">
        <h2>Node Library</h2>
        <NodeLibraryItem title="RTSP Input" description="摄像头或板端 RTSP 流入口" />
        <NodeLibraryItem title="Viewer" description="显示输入流和运行状态" />
        <div className="rail-note">当前版本先固定 RTSP → Viewer，拖拽新增节点后续接入。</div>
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
          fitView
          minZoom={0.2}
          maxZoom={1.8}
          proOptions={{ hideAttribution: true }}
        >
          <Background color="#334155" gap={24} size={1} />
          <MiniMap pannable zoomable nodeStrokeWidth={3} />
          <Controls position="bottom-left" />
          <Panel position="top-left" className="canvas-panel">
            {graph?.title ?? 'Loading workflow'}
          </Panel>
        </ReactFlow>
      </main>

      <aside className="inspector">
        <Inspector selection={selection} />
      </aside>

      <footer className="runtime-panel">
        <div>
          <strong>Runtime</strong>
          <span>{nodes.length} nodes / {edges.length} edges</span>
        </div>
        <ol>
          {events.map((event, index) => (
            <li key={`${event}-${index}`}>{event}</li>
          ))}
        </ol>
      </footer>
    </div>
  );
}

function RtspSourceNode({ data, selected }: NodeProps) {
  const node = (data as FlowNodeData).workflowNode;
  const url = String(node.config.url ?? 'rtsp://');
  return (
    <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <div className="node-body">
        <label>RTSP URL</label>
        <code>{url}</code>
        <span>Transport: {String(node.config.transport ?? 'tcp')}</span>
      </div>
      <Handle id="stream" type="source" position={Position.Right} className="stream-handle" />
    </section>
  );
}

function ViewerNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const previewUrl = nodeData.previewUrl;
  const streamUrl = previewUrl
    ? `/api/streams/mjpeg?url=${encodeURIComponent(previewUrl)}&fps=10&width=960`
    : undefined;
  const [streamState, setStreamState] = useState<'connecting' | 'playing' | 'error'>(
    streamUrl ? 'connecting' : 'error',
  );
  useEffect(() => {
    setStreamState(streamUrl ? 'connecting' : 'error');
  }, [streamUrl]);

  const statusText = !streamUrl
    ? 'No connected RTSP source'
    : streamState === 'playing'
      ? 'MJPEG preview via FFmpeg'
      : streamState === 'connecting'
        ? 'Connecting RTSP via FFmpeg...'
        : 'Preview stream unavailable';

  return (
    <section className={`workflow-node viewer-node ${selected ? 'selected' : ''}`}>
      <Handle id="stream" type="target" position={Position.Left} className="stream-handle" />
      <NodeHeader node={node} />
      <div className={`viewer-preview ${streamState}`}>
        {streamUrl && (
          <img
            src={streamUrl}
            alt={`Preview from ${previewUrl}`}
            onLoad={() => setStreamState('playing')}
            onError={() => setStreamState('error')}
          />
        )}
        {streamState !== 'playing' && <div className="preview-grid" />}
        <span className="viewer-overlay">{statusText}</span>
      </div>
      <div className="node-body compact">
        <span>Fit: {String(node.config.fitMode ?? 'contain')}</span>
        <span>Overlay: {String(node.config.overlay ?? 'status')}</span>
      </div>
    </section>
  );
}

function NodeHeader({ node }: { node: WorkflowNode }) {
  return (
    <header className="node-header">
      <span>{node.title}</span>
      <small className={`state-dot ${node.state}`}>{node.state}</small>
    </header>
  );
}

function NodeLibraryItem({ title, description }: { title: string; description: string }) {
  return (
    <article className="library-item">
      <strong>{title}</strong>
      <span>{description}</span>
    </article>
  );
}

function Inspector({ selection }: { selection: Selection }) {
  if (selection.type === 'none') {
    return (
      <div>
        <h2>Inspector</h2>
        <p className="muted">选择节点或连线后显示参数。</p>
      </div>
    );
  }
  if (selection.type === 'edge') {
    return (
      <div>
        <h2>Edge</h2>
        <KeyValue label="ID" value={selection.edge.id} />
        <KeyValue label="Source" value={`${selection.edge.source}:${selection.edge.sourceHandle ?? ''}`} />
        <KeyValue label="Target" value={`${selection.edge.target}:${selection.edge.targetHandle ?? ''}`} />
        <KeyValue label="Data" value={labelForDataKind(selection.edge.data?.dataKind ?? 'rtsp-stream')} />
      </div>
    );
  }
  const node = selection.node;
  return (
    <div>
      <h2>{node.title}</h2>
      <KeyValue label="Kind" value={node.kind} />
      <KeyValue label="State" value={node.state} />
      <h3>Ports</h3>
      {[...node.inputs, ...node.outputs].map((port) => (
        <KeyValue
          key={`${port.direction}-${port.id}`}
          label={`${port.direction}:${port.id}`}
          value={labelForDataKind(port.dataKind)}
        />
      ))}
      <h3>Config</h3>
      <pre>{JSON.stringify(node.config, null, 2)}</pre>
    </div>
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
      previewUrl: node.kind === 'viewer' ? incomingRtspUrl(graph, node.id) : undefined,
    },
  }));
}

function incomingRtspUrl(graph: WorkflowGraph, viewerNodeId: string): string | undefined {
  const incoming = graph.edges.find((edge) => edge.target.nodeId === viewerNodeId && edge.dataKind === 'rtsp-stream');
  if (!incoming) {
    return undefined;
  }
  const source = graph.nodes.find((node) => node.id === incoming.source.nodeId && node.kind === 'rtspSource');
  return typeof source?.config.url === 'string' ? source.config.url : undefined;
}

function toFlowEdges(graph: WorkflowGraph): FlowEdge[] {
  return graph.edges.map((edge) => ({
    id: edge.id,
    source: edge.source.nodeId,
    sourceHandle: edge.source.portId,
    target: edge.target.nodeId,
    targetHandle: edge.target.portId,
    animated: true,
    label: labelForDataKind(edge.dataKind),
    data: { workflowEdge: edge, dataKind: edge.dataKind },
    className: 'workflow-edge',
  }));
}
