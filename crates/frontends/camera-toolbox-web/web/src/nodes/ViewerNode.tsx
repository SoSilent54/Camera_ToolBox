import { useEffect, useRef } from 'react';
import { NodeResizer, type NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { viewerFrameUrl } from '../workflow';
import { NodeHeader, PortHandles } from './shared';

/** Viewer 节点：轮询引擎帧出口渲染；数据流随引擎 connect/disconnect 严格同步。 */
export function ViewerNode({ data, selected, width, height }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const runtimeState = nodeData.runtimeState;
  return (
    <section
      className={`workflow-node viewer-node ${selected ? 'selected' : ''}`}
      style={{ width, height }}
    >
      <NodeResizer isVisible={selected} minWidth={260} minHeight={160} />
      <NodeHeader node={node} runtimeState={runtimeState} />
      <PortHandles node={node} />
      <EngineFrame nodeId={node.id} />
    </section>
  );
}

/** 轮询引擎 viewer 帧；404 时保持上一帧（冻结），引擎断开后不再更新。 */
function EngineFrame({ nodeId }: { nodeId: string }) {
  const imgRef = useRef<HTMLImageElement>(null);

  useEffect(() => {
    let disposed = false;
    let controller: AbortController | null = null;

    const poll = async () => {
      if (disposed) {
        return;
      }
      controller?.abort();
      controller = new AbortController();
      try {
        const response = await fetch(`${viewerFrameUrl(nodeId)}?t=${Date.now()}`, {
          signal: controller.signal,
        });
        if (!disposed && response.ok) {
          const blob = await response.blob();
          const nextUrl = URL.createObjectURL(blob);
          if (imgRef.current) {
            const previous = imgRef.current.src;
            imgRef.current.src = nextUrl;
            if (previous.startsWith('blob:')) {
              URL.revokeObjectURL(previous);
            }
          }
        }
      } catch {
        // 无帧或引擎未就绪：保持上一帧，不报错。
      }
    };

    poll();
    const timer = setInterval(poll, 250);
    return () => {
      disposed = true;
      controller?.abort();
      clearInterval(timer);
    };
  }, [nodeId]);

  return (
    <div className="viewer-preview">
      <img ref={imgRef} alt="viewer frame" />
    </div>
  );
}
