import { type ConnectionLineComponentProps, type Node } from '@xyflow/react';
import { octilinearPreviewPath } from './FlowPulseEdge';
import { portKindColor } from './nodes/shared';
import type { FlowNodeData } from './workflow';

/** 拖拽连线时复用已提交边的八方向几何，虚线表示该连接尚未提交。 */
export function FlowConnectionPreview({
  connectionLineStyle,
  connectionStatus,
  fromHandle,
  fromNode,
  fromX,
  fromY,
  toX,
  toY,
}: ConnectionLineComponentProps<Node<FlowNodeData>>) {
  const port = [...fromNode.data.workflowNode.outputs, ...fromNode.data.workflowNode.inputs]
    .find((candidate) => candidate.id === fromHandle.id);
  const color = connectionStatus === 'invalid' ? '#f87171' : portKindColor(port?.kind ?? '');
  const path = octilinearPreviewPath({ x: fromX, y: fromY }, { x: toX, y: toY });

  return (
    <g className="workflow-connection-preview">
      <path
        d={path}
        fill="none"
        stroke="#0a0d12"
        strokeWidth={6}
        strokeLinecap="square"
        strokeLinejoin="miter"
      />
      <path
        d={path}
        fill="none"
        stroke={color}
        strokeWidth={2.5}
        strokeLinecap="square"
        strokeLinejoin="miter"
        strokeDasharray="8 5"
        style={connectionLineStyle}
        className="workflow-connection-preview-primary"
      />
    </g>
  );
}
