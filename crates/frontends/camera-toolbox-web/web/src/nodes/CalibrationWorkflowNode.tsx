import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { NodeHeader, PortHandles, RuntimeOutputSummary, ScalarConfigFields } from './shared';

/** 标定辅助节点：只暴露实际实现的配置、动作和运行时输出，避免把输入驱动节点伪装成完整自动闭环。 */
export function CalibrationWorkflowNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;

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
    case 'calibrationFrameScorer':
      return '依据棋盘角点完整度生成 score，并保留帧身份';
    case 'scoreThresholdGate':
      return '按 score 阈值把评分转换为 capture signal';
    case 'consecutiveHoldGate':
      return '连续稳定的 signal 转换为 capture trigger，并去重帧身份';
    case 'captureRequestBuilder':
      return '将 trigger 和显式配置的 YUV/RAW 目标组装成设备 capture request';
    case 'coverageAnalyzer':
      return '按棋盘中心栅格统计覆盖度';
    case 'poseGuide':
      return '提示下一个未覆盖图像栅格';
    default:
      return '输入驱动';
  }
}
