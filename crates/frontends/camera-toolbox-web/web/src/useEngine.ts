import { useCallback, useEffect, useRef, useState } from 'react';
import {
  fetchEngineStatus,
  nodeAction,
  runEngine,
  type WorkflowGraph,
} from './workflow';

export type EngineState = 'disabled' | 'idle' | 'ready' | 'running' | 'error';

/**
 * 数据流引擎前端桥接：装载图、轮询节点状态、派发节点动作。
 *
 * 引擎是触发式的——没有全图级 run/stop；加载图即装载，节点各自 connect/trigger/arm。
 */
export function useEngine() {
  const [nodeStates, setNodeStates] = useState<Record<string, EngineState>>({});
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  /** 装载图到引擎（后端会替换并停止旧图）。 */
  const loadGraph = useCallback((graph: WorkflowGraph) => {
    runEngine(graph).catch((error: unknown) => {
      console.warn('engine load failed', error);
    });
  }, []);

  /** 派发节点动作（connect/disconnect/trigger/arm/disarm）。 */
  const sendAction = useCallback((nodeId: string, action: string) => {
    nodeAction(nodeId, action).catch((error: unknown) => {
      console.warn('node action failed', nodeId, action, error);
    });
  }, []);

  // 轮询引擎状态：drain 增量状态并合并到 nodeStates。
  useEffect(() => {
    let disposed = false;
    const poll = async () => {
      try {
        const statuses = await fetchEngineStatus();
        if (disposed || statuses.length === 0) {
          return;
        }
        setNodeStates((current) => {
          const next = { ...current };
          for (const status of statuses) {
            next[status.nodeId] = status.state;
          }
          return next;
        });
      } catch {
        // 引擎未装载或端点不可用时忽略。
      }
    };
    poll();
    const timer = setInterval(poll, 1000);
    return () => {
      disposed = true;
      clearInterval(timer);
    };
  }, []);

  return { nodeStates, loadGraph, sendAction };
}
