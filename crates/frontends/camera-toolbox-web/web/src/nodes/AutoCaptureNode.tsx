import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { NodeHeader, PortHandles, RuntimeOutputSummary, ScalarConfigFields } from './shared';

/** 自动采集节点：Arm/Disarm 切换条件自动触发。 */
export function AutoCaptureNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;
  const armed = runtimeState === 'running';
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
              onClick={() => nodeData.onNodeAction?.(node.id, armed ? 'disarm' : 'arm')}
            >
              {actionPending ? '处理中…' : armed ? '解除布防' : '布防'}
            </button>
          </div>
        </div>
      </section>
      {runtimeDiagnostic ? <div className="node-diagnostic-below" title={runtimeDiagnostic}>{runtimeDiagnostic}</div> : null}
    </div>
  );
}
