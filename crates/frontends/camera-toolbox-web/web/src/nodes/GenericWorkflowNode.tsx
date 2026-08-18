import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { NodeActionButtons, NodeHeader, PortHandles, RuntimeOutputSummary, ScalarConfigFields } from './shared';

/** 通用节点：复用标量 config 编辑、显式动作与运行时输出摘要基础设施。 */
export function GenericWorkflowNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;
  const layerCapabilityNotice = node.kind === 'imageLayer'
    ? '图片帧会原样转发到 Viewer；visible/opacity 仅保存为图层声明，不参与合成。'
    : node.kind === 'videoLayer'
      ? '视频帧会原样转发；visible/opacity 仅保存为图层声明，不参与合成。'
      : node.kind === 'overlayComposer'
        ? '帧类负载会原样转发到 scene；不执行图层混合或 overlay 光栅化。'
        : null;
  return (
    <div className="workflow-node-shell">
      <section className={`workflow-node generic-node ${selected ? 'selected' : ''}`}>
        <NodeHeader node={node} runtimeState={runtimeState} />
        <PortHandles node={node} />
        <div className="node-body compact">
          {layerCapabilityNotice ? <div className="node-capability-note">{layerCapabilityNotice}</div> : null}
          <ScalarConfigFields
            nodeId={node.id}
            config={node.config}
            onChange={nodeData.onNodeConfigChange}
          />
          <RuntimeOutputSummary output={nodeData.runtimeOutput} />
          <NodeActionButtons
            nodeId={node.id}
            actions={nodeData.availableActions}
            pending={nodeData.actionPending}
            onAction={nodeData.onNodeAction}
            onRefreshOutput={nodeData.onRefreshNodeOutput}
          />
        </div>
      </section>
      {runtimeDiagnostic ? <div className="node-diagnostic-below" title={runtimeDiagnostic}>{runtimeDiagnostic}</div> : null}
    </div>
  );
}
