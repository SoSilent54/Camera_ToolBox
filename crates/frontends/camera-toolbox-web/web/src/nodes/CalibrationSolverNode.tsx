import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { NodeHeader, PortHandles } from './shared';

/** 标定求解节点：显示棋盘参数 + Trigger 触发一次求解。 */
export function CalibrationSolverNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;
  const cfg = node.config;
  return (
    <section className={`workflow-node generic-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} runtimeState={runtimeState} runtimeDiagnostic={runtimeDiagnostic} />

      <PortHandles node={node} />
      <div className="node-body compact">
        <span className="node-param">
          <code>board</code>&nbsp;{String(cfg.boardCols ?? 8)}×{String(cfg.boardRows ?? 11)}
        </span>
        <span className="node-param">
          <code>square</code>&nbsp;{String(cfg.squareSizeMm ?? 30)}mm
        </span>
        <div className="node-actions">
          <button
            type="button"
            disabled={runtimeState === 'disabled'}
            onClick={() => nodeData.onNodeAction?.(node.id, 'trigger')}
          >
            {runtimeState === 'running' ? '求解中…' : '求解'}
          </button>
        </div>
      </div>
    </section>
  );
}
