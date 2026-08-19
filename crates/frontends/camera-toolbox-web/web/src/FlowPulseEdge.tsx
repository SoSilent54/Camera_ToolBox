import { useEffect, useSyncExternalStore } from 'react';
import { BaseEdge, getBezierPath, type EdgeProps } from '@xyflow/react';
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

/** ReactFlow 自定义边：只负责基础连线和 path 注册；脉冲统一由 overlay 渲染。 */
export function FlowPulseEdge(props: EdgeProps & { data: FlowEdgeData }) {
  const [path] = getBezierPath(props);

  useEffect(() => registerFlowEdgePath(props.id, path), [props.id, path]);

  return (
    <BaseEdge
      id={props.id}
      path={path}
      style={props.style}
      markerStart={props.markerStart}
      markerEnd={props.markerEnd}
      interactionWidth={props.interactionWidth}
    />
  );
}
