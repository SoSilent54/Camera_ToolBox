import { useEffect, useRef, useState } from 'react';
import { NodeResizer, type NodeProps } from '@xyflow/react';
import type { FlowNodeData } from './workflow';
import { configText, normalizeSourcePathDraft } from './nodeConfig';
import { NodeHeader, PortHandles } from './nodes/shared';
import { FileBrowser } from './FileBrowser';


export function LocalWorkspaceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const root = typeof node.config.root === 'string' ? node.config.root : '';
  const [draftRoot, setDraftRoot] = useState(root);
  useEffect(() => setDraftRoot(root), [root]);
  const applyRoot = () => nodeData.onLocalImageConfigChange?.(node.id, 'root', draftRoot);
  return (
    <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
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
    </section>
  );
}

export function SftpWorkspaceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const remoteRoot = configText(node, 'remoteRoot', '/');
  const sourceId = configText(node, 'sourceId', 'sftp-main');
  const [draftRoot, setDraftRoot] = useState(remoteRoot);
  useEffect(() => setDraftRoot(remoteRoot), [remoteRoot]);
  const applyRoot = () => nodeData.onNodeConfigChange?.(node.id, 'remoteRoot', draftRoot.trim() || '/');
  return (
    <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body">
        <label htmlFor={`${node.id}-remote-root`}>Remote root</label>
        <input
          id={`${node.id}-remote-root`}
          className="rtsp-url-input nodrag"
          value={draftRoot}
          placeholder="/data/captures"
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
        <span>Source ID: {sourceId}</span>
        <span>Session-bound; no password or directory cache is persisted.</span>
      </div>
    </section>
  );
}
export function FileBrowserNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const root = configText(node, 'root', '');
  const directory = configText(node, 'directory', '');
  const selection = configText(node, 'selection', '');
  const runtimeState = nodeData.runtimeState;
  const [draftRoot, setDraftRoot] = useState(root);
  useEffect(() => setDraftRoot(root), [root]);
  const applyRoot = () => nodeData.onNodeConfigChange?.(node.id, 'root', draftRoot.trim());
  return (
    <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} runtimeState={runtimeState} />
      <PortHandles node={node} />
      <div className="node-body">
        <label htmlFor={`${node.id}-root`}>Workspace root</label>
        <input
          id={`${node.id}-root`}
          className="rtsp-url-input nodrag"
          value={draftRoot}
          placeholder="/absolute/path"
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
        <FileBrowser
          root={root}
          directory={directory}
          selection={selection}
          onDirectory={(path) => nodeData.onNodeConfigChange?.(node.id, 'directory', path)}
          onSelection={(path) => nodeData.onNodeConfigChange?.(node.id, 'selection', path)}
        />
      </div>
    </section>
  );
}

export function SshSessionNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const host = configText(node, 'host', '');
  const profileId = configText(node, 'profileId', '');
  const username = configText(node, 'username', 'root');
  return (
    <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body compact">
        <span>Profile: {profileId || 'manual'}</span>
        <span>Host: {host || 'unset'}</span>
        <span>User: {username}</span>
        <span>Auto: {String(node.config.autoConnect === true)}</span>
      </div>
    </section>
  );
}

export function X5DeviceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  return (
    <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body compact">
        <span>Host: {configText(node, 'host', '10.21.12.108')}</span>
        <span>TCP: {configText(node, 'tcpPort', '9073')}</span>
        <span>Control: X5 TCP + RTSP channel catalog</span>
      </div>
    </section>
  );
}

export function X5RtspChannelNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  return (
    <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body compact">
        <span>Channel: {configText(node, 'channel', '0')}</span>
        <span>Path: {configText(node, 'path', '/PRR')}</span>
        <span>Source: X5 device RTSP endpoint</span>
      </div>
    </section>
  );
}

export function X5SnapshotNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  return (
    <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body compact">
        <span>Mode: {configText(node, 'mode', 'latest')}</span>
        <span>Capture: X5 TCP snapshot</span>
        <span>Output: image frame</span>
      </div>
    </section>
  );
}

export function ImageFileSourceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const relativePath = typeof node.config.relativePath === 'string' ? node.config.relativePath : '';
  const [draftPath, setDraftPath] = useState(relativePath);
  useEffect(() => setDraftPath(relativePath), [relativePath]);
  const applyPath = () => nodeData.onLocalImageConfigChange?.(node.id, 'relativePath', draftPath);
  return (
    <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
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
        <span>Path is relative to the connected workspace/file ref.</span>
      </div>
    </section>
  );
}


export function ViewerNode({ data, selected, width, height }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const preview = nodeData.preview;
  return (
    <section className={`workflow-node viewer-node ${selected ? 'selected' : ''}`} style={{ width, height }}>
      <NodeResizer isVisible={selected} minWidth={260} minHeight={160} />
      <NodeHeader node={node} />
      <PortHandles node={node} />
      {preview?.kind === 'rtsp' && (
        <MjpegPreview
          streamUrl={`/api/streams/mjpeg?url=${encodeURIComponent(preview.url)}&width=960&height=540`}
          previewUrl={preview.url}
        />
      )}
      {preview?.kind === 'local-image' && <LocalImagePreview imageUrl={preview.url} />}
      {!preview && <LocalImagePreview imageUrl={undefined} />}
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

function LocalImagePreview({ imageUrl }: { imageUrl: string | undefined }) {
  const [state, setState] = useState<'empty' | 'loading' | 'ready' | 'error'>(imageUrl ? 'loading' : 'empty');
  useEffect(() => {
    setState(imageUrl ? 'loading' : 'empty');
  }, [imageUrl]);
  return (
    <div className="viewer-panel nodrag nopan">
      <div className={`viewer-preview ${state}`}>
        {imageUrl && (
          <img
            src={imageUrl}
            alt="Local workspace image preview"
            onLoad={() => setState('ready')}
            onError={() => setState('error')}
          />
        )}
        {state !== 'ready' && <div className="preview-grid" />}
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

  return (
    <div className="viewer-panel nodrag nopan">
      <div className={`viewer-preview ${streamState}`}>
        {frameUrl && (
          <img src={frameUrl} alt={`Preview from ${previewUrl}`} />
        )}
        {streamState !== 'playing' && <div className="preview-grid" />}
      </div>
      <div className="viewer-status">{statusText}</div>
      <details className="viewer-metrics">
        <summary>Stream metrics</summary>
        <dl>
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
      </details>
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
