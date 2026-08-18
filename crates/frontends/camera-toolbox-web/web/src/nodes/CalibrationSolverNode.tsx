import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { NodeHeader, PortHandles, RuntimeOutputSummary, ScalarConfigFields } from './shared';

/** 标定求解节点：显示棋盘参数 + Trigger 触发一次求解。 */
export function CalibrationSolverNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;
  const cfg = node.config;
  const actionPending = Boolean(nodeData.actionPending);
  return (
    <div className="workflow-node-shell">
      <section className={`workflow-node generic-node ${selected ? 'selected' : ''}`}>
        <NodeHeader node={node} runtimeState={runtimeState} />

        <PortHandles node={node} />
        <div className="node-body compact">
          <ScalarConfigFields
            nodeId={node.id}
            config={node.config}
            onChange={nodeData.onNodeConfigChange}
          />
          <RuntimeOutputSummary output={nodeData.runtimeOutput} />
          <div className="node-actions">
            <button
              type="button"
              className="nodrag nowheel"
              disabled={runtimeState === 'disabled' || actionPending}
              onClick={() => nodeData.onNodeAction?.(node.id, 'trigger')}
            >
              {actionPending ? '处理中…' : runtimeState === 'running' ? '求解中…' : '求解'}
            </button>
          </div>
        </div>
      </section>
      {runtimeDiagnostic ? <div className="node-diagnostic-below" title={runtimeDiagnostic}>{runtimeDiagnostic}</div> : null}
    </div>
  );
}
