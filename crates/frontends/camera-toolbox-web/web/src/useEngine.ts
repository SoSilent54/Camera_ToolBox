import { useCallback, useEffect, useRef, useState } from 'react';
import { loadRuntimeNodeOutput, nodeAction, type DatasetSampleActionPayload, type NodeActionName } from './workflow';
import { subscribeTopic } from './useEngineSocket';

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
  const [pendingActions, setPendingActions] = useState<Record<string, string>>({});
  const pendingActionsRef = useRef<Record<string, string>>({});
  const [nodeOutputs, setNodeOutputs] = useState<Record<string, unknown>>({});

  /** 读取服务端 latest output；尚无输出是常规状态，保持已有摘要不闪烁。 */
  const refreshNodeOutput = useCallback((nodeId: string) => {
    void loadRuntimeNodeOutput(nodeId)
      .then((output) => setNodeOutputs((current) => ({ ...current, [nodeId]: output })))
      .catch(() => undefined);
  }, []);

  const clearPendingAction = useCallback((nodeId: string, action?: NodeActionName) => {
    if (!(nodeId in pendingActionsRef.current)) {
      return;
    }
    if (action && pendingActionsRef.current[nodeId] !== action) {
      return;
    }
    const nextPending = { ...pendingActionsRef.current };
    delete nextPending[nodeId];
    pendingActionsRef.current = nextPending;
    setPendingActions(nextPending);
    refreshNodeOutput(nodeId);
  }, [refreshNodeOutput]);
  // 订阅 status 推送，按 nodeId 覆盖 state/diagnostic，合并进本地状态。
  useEffect(() => {
    return subscribeTopic('status', (payload) => {
      const status = payload as StatusPush;
      if (!status || typeof status.nodeId !== 'string' || typeof status.state !== 'string') {
        return;
      }
      setNodeStates((current) => ({ ...current, [status.nodeId]: status.state }));
      clearPendingAction(status.nodeId);
      setNodeDiagnostics((current) => {
        if (typeof status.diagnostic === 'string' && status.diagnostic.trim()) {
          return { ...current, [status.nodeId]: status.diagnostic };
        }
        const next = { ...current };
        delete next[status.nodeId];
        return next;
      });
    });
  }, [clearPendingAction]);


  /** 派发节点动作；样本审核可带非持久化的 `{ sampleId }` payload，同一节点同一时刻只允许一个动作在途。 */
  const sendAction = useCallback((nodeId: string, action: NodeActionName, payload?: DatasetSampleActionPayload) => {
    if (pendingActionsRef.current[nodeId]) {
      return;
    }
    pendingActionsRef.current = { ...pendingActionsRef.current, [nodeId]: action };
    setPendingActions(pendingActionsRef.current);
    void nodeAction(nodeId, action, payload)
      .then(() => {
        // WS ack 只代表动作已入队，节点 actor 可能还没 emit runtime output；延迟刷新避免抢读旧 dataset。
        window.setTimeout(() => refreshNodeOutput(nodeId), 100);
        window.setTimeout(() => {
          if (pendingActionsRef.current[nodeId] !== action) {
            return;
          }
          const nextPending = { ...pendingActionsRef.current };
          delete nextPending[nodeId];
          pendingActionsRef.current = nextPending;
          setPendingActions(nextPending);
          refreshNodeOutput(nodeId);
        }, 5_000);
      })
      .catch((error: unknown) => {
        console.warn('node action failed', nodeId, action, error);
        if (pendingActionsRef.current[nodeId] !== action) {
          return;
        }
        const nextPending = { ...pendingActionsRef.current };
        delete nextPending[nodeId];
        pendingActionsRef.current = nextPending;
        setPendingActions(nextPending);
      });
  }, [clearPendingAction]);

  return { nodeStates, nodeDiagnostics, nodeOutputs, pendingActions, sendAction, refreshNodeOutput };
}
