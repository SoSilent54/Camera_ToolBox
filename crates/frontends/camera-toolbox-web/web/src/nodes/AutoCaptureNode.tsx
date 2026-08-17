import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { NodeHeader, PortHandles } from './shared';

/** 自动采集节点：Arm/Disarm 切换条件自动触发。 */
export function AutoCaptureNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;
  const armed = runtimeState === 'running';
  return (
    <div className="workflow-node-shell">
      <section className={`workflow-node generic-node ${selected ? 'selected' : ''}`}>
        <NodeHeader node={node} runtimeState={runtimeState} />

        <PortHandles node={node} />
        <div className="node-body compact">
          <span className="node-param">
            <code>strategy</code>&nbsp;{String(node.config.strategy ?? 'datasetGain')}
          </span>
          <div className="node-actions">
            <button
              type="button"
              disabled={runtimeState === 'disabled'}
              onClick={() => nodeData.onNodeAction?.(node.id, armed ? 'disarm' : 'arm')}
            >
              {armed ? '解除布防' : '布防'}
            </button>
          </div>
        </div>
      </section>
      {runtimeDiagnostic ? <div className="node-diagnostic-below" title={runtimeDiagnostic}>{runtimeDiagnostic}</div> : null}
    </div>
  );
}
