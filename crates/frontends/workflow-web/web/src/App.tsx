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

  const handleRtspUrlChange = useCallback(
    (nodeId: string, nextUrl: string) => {
      const trimmedUrl = nextUrl.trim();
      if (!trimmedUrl.startsWith('rtsp://') && !trimmedUrl.startsWith('rtsps://')) {
        setEvents((current) => [`拒绝 RTSP URL：必须使用 rtsp:// 或 rtsps://`, ...current].slice(0, 6));
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
        const workflowNodes = new Map(updated.map((flowNode) => [flowNode.id, flowNode.data.workflowNode]));
        return updated.map((flowNode) => {
          if (flowNode.data.workflowNode.kind !== 'viewer') {
            return {
              ...flowNode,
              data: { ...flowNode.data, onRtspUrlChange: handleRtspUrlChange },
            };
          }
          const incoming = edges.find(
            (edge) => edge.target === flowNode.id && edge.data?.dataKind === 'rtsp-stream',
          );
          const source = incoming ? workflowNodes.get(incoming.source) : undefined;
          return {
            ...flowNode,
            data: {
              ...flowNode.data,
              previewUrl: typeof source?.config.url === 'string' ? source.config.url : undefined,
              onRtspUrlChange: handleRtspUrlChange,
            },
          };
        });
      });
      setSelection((current) => {
        if (current.type !== 'node' || current.node.id !== nodeId) {
          return current;
        }
        return {
          type: 'node',
          node: { ...current.node, config: { ...current.node.config, url: trimmedUrl } },
        };
      });
      setEvents((current) => [`RTSP URL 已更新：${trimmedUrl}`, ...current].slice(0, 6));
    },
    [edges, setNodes],
  );

  useEffect(() => {
    setNodes((current) =>
      current.map((flowNode) =>
        flowNode.data.onRtspUrlChange === handleRtspUrlChange
          ? flowNode
          : { ...flowNode, data: { ...flowNode.data, onRtspUrlChange: handleRtspUrlChange } },
      ),
    );
  }, [handleRtspUrlChange, nodes.length, setNodes]);

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
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const url = String(node.config.url ?? 'rtsp://');
  const [draftUrl, setDraftUrl] = useState(url);
  useEffect(() => {
    setDraftUrl(url);
  }, [url]);
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
      <Handle id="stream" type="source" position={Position.Right} className="stream-handle" />
    </section>
  );
}

function ViewerNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const previewUrl = nodeData.previewUrl;
  const streamUrl = previewUrl
    ? `/api/streams/mjpeg?url=${encodeURIComponent(previewUrl)}&width=960&height=540`
    : undefined;
  return (
    <section className={`workflow-node viewer-node ${selected ? 'selected' : ''}`}>
      <Handle id="stream" type="target" position={Position.Left} className="stream-handle" />
      <NodeHeader node={node} />
      <MjpegPreview streamUrl={streamUrl} previewUrl={previewUrl} />
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

function MjpegPreview({ streamUrl, previewUrl }: { streamUrl: string | undefined; previewUrl: string | undefined }) {
  const [streamState, setStreamState] = useState<'connecting' | 'playing' | 'error'>(streamUrl ? 'connecting' : 'error');
  const [frameUrl, setFrameUrl] = useState<string | undefined>();
  const [metrics, setMetrics] = useState<ViewerMetrics>(() => initialViewerMetrics());
  const objectUrlRef = useRef<string | undefined>();
  const lastFrameAtRef = useRef<number | null>(null);

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
        setMetrics((current) => ({
          ...current,
          renderFps,
          lastFrameAgeMs: lastFrameAt === null ? null : now - lastFrameAt,
        }));
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

    return () => {
      abortController.abort();
    };
  }, [streamUrl]);

  useEffect(() => {
    return () => {
      if (objectUrlRef.current) {
        URL.revokeObjectURL(objectUrlRef.current);
      }
    };
  }, []);

  const statusText = !streamUrl
    ? 'No connected RTSP source'
    : streamState === 'playing'
      ? 'MJPEG preview via internal ffmpeg-next'
      : streamState === 'connecting'
        ? 'Connecting RTSP via internal decoder...'
        : 'Preview stream unavailable';

  return (
    <div className="viewer-panel">
      <div className={`viewer-preview ${streamState}`}>
        {frameUrl && <img src={frameUrl} alt={`Preview from ${previewUrl}`} />}
        {streamState !== 'playing' && <div className="preview-grid" />}
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
