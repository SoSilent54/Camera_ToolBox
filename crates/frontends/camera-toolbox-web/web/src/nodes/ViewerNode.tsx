import { useEffect, useRef, useState } from 'react';
import { NodeResizer, type NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { subscribeFrame } from '../useEngineSocket';
import { NodeHeader, PortHandles, RuntimeOutputSummary } from './shared';

/** Viewer 节点：订阅 WS 二进制帧推送，并明确展示当前帧与断流状态。 */
export function ViewerNode({ data, selected, width, height }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;
  return (
    <div className="workflow-node-shell viewer-node-shell">
      <section
        className={`workflow-node viewer-node ${selected ? 'selected' : ''}`}
        style={{ width, height }}
      >
        <NodeResizer isVisible={selected} minWidth={260} minHeight={160} />
        <NodeHeader node={node} runtimeState={runtimeState} />
        <PortHandles node={node} />
        <EngineFrame
          nodeId={node.id}
          imageFormatHint={node.inputs.find((port) => port.id === 'image')?.formatHint}
          runtimeState={runtimeState}
          runtimeDiagnostic={runtimeDiagnostic}
          runtimeOutput={nodeData.runtimeOutput}
        />
      </section>
      {runtimeDiagnostic ? <div className="node-diagnostic-below" title={runtimeDiagnostic}>{runtimeDiagnostic}</div> : null}
    </div>
  );
}

type FrameSnapshot = {
  lastFrameAt: number | null;
  seq?: number;
  width?: number;
  height?: number;
  fps?: number;
  stalled: boolean;
};

/** 订阅 latest-wins JPEG；只以 `frame_meta` 的实际编码尺寸/序号显示状态，不推断源端参数。 */
function EngineFrame({
  nodeId,
  imageFormatHint,
  runtimeState,
  runtimeDiagnostic,
  runtimeOutput,
}: {
  nodeId: string;
  imageFormatHint?: string;
  runtimeState?: string;
  runtimeDiagnostic?: string;
  runtimeOutput: unknown;
}) {

  const imgRef = useRef<HTMLImageElement>(null);
  const frameTimes = useRef<number[]>([]);
  const lastUiUpdateAt = useRef(0);
  const frameRef = useRef<FrameSnapshot>({ lastFrameAt: null, stalled: false });
  const [frame, setFrame] = useState<FrameSnapshot>({ lastFrameAt: null, stalled: false });

  useEffect(() => {
    let disposed = false;
    let currentUrl: string | null = null;
    let stallTimer: number | undefined;

    frameTimes.current = [];
    lastUiUpdateAt.current = 0;
    frameRef.current = { lastFrameAt: null, stalled: false };
    setFrame(frameRef.current);

    const scheduleStallCheck = (receivedAt: number) => {
      clearTimeout(stallTimer);
      stallTimer = window.setTimeout(() => {
        if (!disposed && frameRef.current.lastFrameAt === receivedAt) {
          const stalledFrame = { ...frameRef.current, stalled: true };
          frameRef.current = stalledFrame;
          setFrame(stalledFrame);
        }
      }, 1_100);
    };

    const unsubscribe = subscribeFrame(nodeId, (blob, meta) => {
      if (disposed) {
        return;
      }
      const nextUrl = URL.createObjectURL(blob);
      if (imgRef.current) {
        imgRef.current.src = nextUrl;
      }
      if (currentUrl) {
        URL.revokeObjectURL(currentUrl);
      }
      currentUrl = nextUrl;

      const receivedAt = performance.now();
      const timestamps = frameTimes.current;
      timestamps.push(receivedAt);
      if (timestamps.length > 24) {
        timestamps.shift();
      }
      const fps = timestamps.length > 1
        ? ((timestamps.length - 1) * 1000) / (receivedAt - timestamps[0])
        : undefined;
      const previousFrame = frameRef.current;
      const nextFrame = {
        lastFrameAt: receivedAt,
        seq: meta.seq,
        width: meta.width,
        height: meta.height,
        fps,
        stalled: false,
      };
      frameRef.current = nextFrame;
      const resolutionChanged = meta.width !== previousFrame.width || meta.height !== previousFrame.height;
      if (receivedAt - lastUiUpdateAt.current >= 250 || resolutionChanged || previousFrame.lastFrameAt === null || previousFrame.stalled) {
        lastUiUpdateAt.current = receivedAt;
        setFrame(nextFrame);
      }
      scheduleStallCheck(receivedAt);
    });

    return () => {
      disposed = true;
      unsubscribe();
      clearTimeout(stallTimer);
      if (currentUrl) {
        URL.revokeObjectURL(currentUrl);
      }
    };
  }, [nodeId]);

  return (
    <div className="viewer-panel">
      <div className="viewer-preview">
        <img ref={imgRef} alt="viewer frame" />
        {!frame.lastFrameAt ? <span className="viewer-placeholder">等待引擎 JPEG 帧</span> : null}
      </div>
      <div className={`viewer-status ${frame.stalled ? 'viewer-status-stalled' : ''}`}>
        {viewerStatus(frame, runtimeState, runtimeDiagnostic)}
      </div>
      {frame.lastFrameAt ? <div className="viewer-meta">{formatFrameMeta(frame)}</div> : null}
      {runtimeOutput !== undefined ? (
        <div className="viewer-runtime-output">
          <span>引擎输出</span>
          <RuntimeOutputSummary output={runtimeOutput} />
        </div>
      ) : null}
      <span className="node-hint">
        Viewer 只负责预览和显示状态；支持 {imageFormatHint ?? 'Rgba8 | Gray8 | Gray16Le | Nv12'}。BayerRaw 必须先经过显式 Demosaic；RAW/YUV 设备采集请在 X5_233 Driver 上触发。
      </span>
    </div>
  );
}

function viewerStatus(frame: FrameSnapshot, runtimeState?: string, runtimeDiagnostic?: string): string {
  if (frame.stalled) {
    return '上游断流：保留最后一帧，等待新的 JPEG 帧';
  }
  if (frame.lastFrameAt) {
    return '正在接收引擎 JPEG 帧';
  }
  return runtimeDiagnostic ?? (runtimeState === 'idle'
    ? 'Viewer 已停止或上游未连接'
    : '等待上游视频帧');
}

function formatFrameMeta({ seq, width, height, fps }: FrameSnapshot): string {
  const resolution = width && height ? `${width} × ${height}` : '尺寸未声明';
  const sequence = seq === undefined ? '序号未声明' : `seq ${seq}`;
  const rate = fps && Number.isFinite(fps) ? `${fps.toFixed(1)} FPS（到达率）` : '正在估算 FPS';
  return `${resolution} · ${sequence} · ${rate}`;
}
