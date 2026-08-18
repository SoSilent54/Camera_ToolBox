import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData, NodeActionControl } from '../workflow';
import { NodeActionButtons, NodeHeader, PortHandles, RuntimeOutputSummary, ScalarConfigFields } from './shared';

const DATASET_ACTIONS: readonly NodeActionControl[] = [
  { action: 'trigger', label: '输出数据集' },
  { action: 'clear', label: '清空样本' },
];

/** 标定辅助节点：只暴露实际实现的配置、动作和运行时输出，避免把输入驱动节点伪装成完整自动闭环。 */
export function CalibrationWorkflowNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;
  const actions = node.kind === 'datasetCollector' ? DATASET_ACTIONS : undefined;

  return (
    <div className="workflow-node-shell">
      <section className={`workflow-node generic-node ${selected ? 'selected' : ''}`}>
        <NodeHeader node={node} runtimeState={runtimeState} />
        <PortHandles node={node} />
        <div className="node-body compact">
          <span className="node-hint">{nodeHint(node.kind)}</span>
          <ScalarConfigFields
            nodeId={node.id}
            config={node.config}
            onChange={nodeData.onNodeConfigChange}
          />
          <RuntimeOutputSummary output={nodeData.runtimeOutput} />
          <NodeActionButtons
            nodeId={node.id}
            actions={actions}
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

function nodeHint(kind: string): string {
  switch (kind) {
    case 'chessboardDetector':
      return '随输入帧检测棋盘格';
    case 'gainScorer':
      return '按角点完整度计算 gain，并保留帧身份';
    case 'captureGate':
      return '满足阈值并稳定保持后发出抓帧请求';
    case 'datasetCollector':
      return '累积 detection，手动输出或清空';
    case 'coverageAnalyzer':
      return '按棋盘中心栅格统计覆盖度';
    case 'poseGuide':
      return '提示下一个未覆盖图像栅格';
    default:
      return '输入驱动';
  }
}
