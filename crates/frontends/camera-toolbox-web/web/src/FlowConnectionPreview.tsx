import { getBezierPath, type ConnectionLineComponentProps, type Node } from '@xyflow/react';
import { portKindColor } from './nodes/shared';
import type { FlowNodeData } from './workflow';

/** 拖拽连线使用与已提交边一致的 Bézier 曲线，虚线表示该连接尚未提交。 */
export function FlowConnectionPreview({
  connectionLineStyle,
  connectionStatus,
  fromHandle,
  fromNode,
  fromPosition,
  fromX,
  fromY,
  toPosition,
  toX,
  toY,
}: ConnectionLineComponentProps<Node<FlowNodeData>>) {
  const port = [...fromNode.data.workflowNode.outputs, ...fromNode.data.workflowNode.inputs]
    .find((candidate) => candidate.id === fromHandle.id);
  const color = connectionStatus === 'invalid' ? '#f87171' : portKindColor(port?.kind ?? '');
  const [path] = getBezierPath({
    sourceX: fromX,
    sourceY: fromY,
    sourcePosition: fromPosition,
    targetX: toX,
    targetY: toY,
    targetPosition: toPosition,
  });

  return (
    <g className="workflow-connection-preview">
      <path
        d={path}
        fill="none"
        stroke="#0a0d12"
        strokeWidth={6}
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path
        d={path}
        fill="none"
        stroke={color}
        strokeWidth={2.5}
        strokeLinecap="round"
        strokeLinejoin="round"
        strokeDasharray="8 5"
        style={connectionLineStyle}
        className="workflow-connection-preview-primary"
      />
    </g>
  );
}
