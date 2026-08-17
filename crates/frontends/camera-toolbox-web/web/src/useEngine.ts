import { useCallback, useEffect, useState } from 'react';
import { subscribeTopic, wsRequest } from './useEngineSocket';
import type { WorkflowGraph } from './workflow';

export type EngineState = 'disabled' | 'idle' | 'ready' | 'running' | 'warning' | 'error';

/** status topic 推送的单个节点状态。 */
interface StatusPush {
  nodeId: string;
  state: EngineState;
  diagnostic?: string;
}

/**
 * 数据流引擎前端桥接：经单一 WebSocket 装载图、订阅节点状态、派发节点动作、图级 run/start。
 * 状态面由后端 status 推送驱动，不再轮询。
 */
export function useEngine() {
  const [nodeStates, setNodeStates] = useState<Record<string, EngineState>>({});

  // 订阅 status 推送，按 nodeId 覆盖 state，合并进 nodeStates。
  useEffect(() => {
    return subscribeTopic('status', (payload) => {
      const status = payload as StatusPush;
      if (!status || typeof status.nodeId !== 'string' || typeof status.state !== 'string') {
        return;
      }
      setNodeStates((current) => ({ ...current, [status.nodeId]: status.state }));
    });
  }, []);

  /** 装载图到引擎（后端会替换并停止旧图）；调用方负责决定何时把编辑图应用为运行图。 */
  const loadGraph = useCallback((graph: WorkflowGraph) => wsRequest('runtime.run', graph).then(() => {
    setNodeStates({});
  }), []);

  /** 图级 run/start：一键启动所有可启动节点。 */
  const startAll = useCallback(() => wsRequest('runtime.start').then(() => undefined), []);

  /** 派发节点动作（connect/disconnect/trigger/arm/disarm）。 */
  const sendAction = useCallback((nodeId: string, action: string) => {
    wsRequest('runtime.node.action', { nodeId, action }).catch((error: unknown) => {
      console.warn('node action failed', nodeId, action, error);
    });
  }, []);

  return { nodeStates, loadGraph, startAll, sendAction };
}
