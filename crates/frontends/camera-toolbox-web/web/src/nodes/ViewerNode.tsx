import { useEffect, useRef } from 'react';
import { NodeResizer, type NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { subscribeFrame } from '../useEngineSocket';
import { NodeHeader, PortHandles } from './shared';

/** Viewer 节点：订阅 WS 二进制帧推送渲染；数据流随引擎 connect/disconnect 严格同步。 */
export function ViewerNode({ data, selected, width, height }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;
  return (
    <section
      className={`workflow-node viewer-node ${selected ? 'selected' : ''}`}
      style={{ width, height }}
    >
      <NodeResizer isVisible={selected} minWidth={260} minHeight={160} />
      <NodeHeader node={node} runtimeState={runtimeState} runtimeDiagnostic={runtimeDiagnostic} />
      <PortHandles node={node} />
      <EngineFrame nodeId={node.id} />
    </section>
  );
}

/** 订阅引擎 viewer 二进制 JPEG 帧；latest-wins，丢弃旧帧，无帧时保持上一帧（冻结）。 */
function EngineFrame({ nodeId }: { nodeId: string }) {
  const imgRef = useRef<HTMLImageElement>(null);

  useEffect(() => {
    let disposed = false;

    const unsubscribe = subscribeFrame(nodeId, (blob) => {
      if (disposed) {
        return;
      }
      const nextUrl = URL.createObjectURL(blob);
      if (imgRef.current) {
        const previous = imgRef.current.src;
        imgRef.current.src = nextUrl;
        if (previous.startsWith('blob:')) {
          URL.revokeObjectURL(previous);
        }
      }
    });

    return () => {
      disposed = true;
      unsubscribe();
    };
  }, [nodeId]);

  return (
    <div className="viewer-preview">
      <img ref={imgRef} alt="viewer frame" />
    </div>
  );
}
