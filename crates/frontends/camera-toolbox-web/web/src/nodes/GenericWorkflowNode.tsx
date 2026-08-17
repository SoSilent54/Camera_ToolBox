import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { NodeHeader, PortHandles } from './shared';

/** 通用节点：显示 config 的标量参数摘要（不含 Kind/Category/In/Out 等无用信息）。 */
export function GenericWorkflowNode({ data, selected }: NodeProps) {
  const node = (data as FlowNodeData).workflowNode;
  const runtimeState = (data as FlowNodeData).runtimeState;
  const runtimeDiagnostic = (data as FlowNodeData).runtimeDiagnostic;
  const params = Object.entries(node.config)
    .filter(([, value]) => typeof value !== 'object' && value !== null && value !== undefined && value !== '')
    .slice(0, 4);
  return (
    <section className={`workflow-node generic-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} runtimeState={runtimeState} runtimeDiagnostic={runtimeDiagnostic} />

      <PortHandles node={node} />
      <div className="node-body compact">
        {params.length === 0 ? (
          <span className="node-hint">no config</span>
        ) : params.map(([key, value]) => (
          <span key={key} className="node-param">
            <code>{key}</code>&nbsp;{String(value)}
          </span>
        ))}
      </div>
    </section>
  );
}
