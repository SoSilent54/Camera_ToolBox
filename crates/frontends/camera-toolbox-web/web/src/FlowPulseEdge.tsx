import { useEffect, useSyncExternalStore } from 'react';
import { BaseEdge, getBezierPath, type EdgeProps } from '@xyflow/react';
import { portKindColor } from './nodes/shared';
import type { FlowEdgeData } from './workflow';

let edgePathSnapshot = new Map<string, string>();
const edgePaths = new Map<string, string>();
const edgePathListeners = new Set<() => void>();

function publishEdgePaths() {
  edgePathSnapshot = new Map(edgePaths);
  edgePathListeners.forEach((listener) => listener());
}

export function registerFlowEdgePath(edgeId: string, path: string): () => void {
  if (edgePaths.get(edgeId) !== path) {
    edgePaths.set(edgeId, path);
    publishEdgePaths();
  }
  return () => {
    if (edgePaths.get(edgeId) === path) {
      edgePaths.delete(edgeId);
      publishEdgePaths();
    }
  };
}

export function useFlowEdgePaths(): ReadonlyMap<string, string> {
  return useSyncExternalStore(
    (listener) => {
      edgePathListeners.add(listener);
      return () => {
        edgePathListeners.delete(listener);
      };
    },
    () => edgePathSnapshot,
    () => edgePathSnapshot,
  );
}

/** 工作流边直接采用 React Flow Bézier 曲线；不再计算节点障碍物、绕行通道或局部轨道。 */
export function FlowPulseEdge(props: EdgeProps & { data: FlowEdgeData }) {
  const [path] = getBezierPath({
    sourceX: props.sourceX,
    sourceY: props.sourceY,
    sourcePosition: props.sourcePosition,
    targetX: props.targetX,
    targetY: props.targetY,
    targetPosition: props.targetPosition,
  });
  const style = {
    ...props.style,
    stroke: portKindColor(props.data.kind),
    strokeLinecap: 'round' as const,
    strokeLinejoin: 'round' as const,
  };

  useEffect(() => registerFlowEdgePath(props.id, path), [props.id, path]);

  return (
    <>
      {props.selected && (
        <BaseEdge
          id={`${props.id}-selection-outline`}
          path={path}
          style={{ stroke: '#f8fafc', strokeWidth: 7, opacity: 0.82, strokeLinecap: 'round', strokeLinejoin: 'round' }}
          interactionWidth={0}
        />
      )}
      <BaseEdge
        id={props.id}
        path={path}
        style={style}
        markerStart={props.markerStart}
        markerEnd={props.markerEnd}
        interactionWidth={props.interactionWidth}
      />
    </>
  );
}
