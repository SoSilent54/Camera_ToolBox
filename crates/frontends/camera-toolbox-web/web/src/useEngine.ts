import { useCallback, useEffect, useState } from 'react';
import { subscribeTopic, wsRequest } from './useEngineSocket';

export type EngineState = 'disabled' | 'idle' | 'ready' | 'running' | 'warning' | 'error';

/** status topic 推送的单个节点状态。 */
interface StatusPush {
  nodeId: string;
  state: EngineState;
  diagnostic?: string;
}

/**
 * 数据流引擎前端桥接：订阅节点状态、派发节点动作。
 * 图结构由后端 authoritative snapshot 驱动，不暴露全局 run/start 状态。
 */
export function useEngine() {
  const [nodeStates, setNodeStates] = useState<Record<string, EngineState>>({});
  const [nodeDiagnostics, setNodeDiagnostics] = useState<Record<string, string>>({});

  // 订阅 status 推送，按 nodeId 覆盖 state/diagnostic，合并进本地状态。
  useEffect(() => {
    return subscribeTopic('status', (payload) => {
      const status = payload as StatusPush;
      if (!status || typeof status.nodeId !== 'string' || typeof status.state !== 'string') {
        return;
      }
      setNodeStates((current) => ({ ...current, [status.nodeId]: status.state }));
      setNodeDiagnostics((current) => {
        if (typeof status.diagnostic === 'string' && status.diagnostic.trim()) {
          return { ...current, [status.nodeId]: status.diagnostic };
        }
        const next = { ...current };
        delete next[status.nodeId];
        return next;
      });
    });
  }, []);


  /** 派发节点动作（connect/disconnect/trigger/arm/disarm）。 */
  const sendAction = useCallback((nodeId: string, action: string) => {
    wsRequest('runtime.node.action', { nodeId, action }).catch((error: unknown) => {
      console.warn('node action failed', nodeId, action, error);
    });
  }, []);

  return { nodeStates, nodeDiagnostics, sendAction };
}
