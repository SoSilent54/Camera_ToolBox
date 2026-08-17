import type { DragEvent } from 'react';
import { Handle, Position } from '@xyflow/react';
import type { NodeDefinition, NodeKind, PortKind, WorkflowNode } from '../workflow';

export const DEFAULT_RTSP_URL = 'rtsp://10.21.12.108:554/PRR';
/** 节点标题 + 实时状态点；状态优先取引擎实时值，回退到持久化 state。 */
export function NodeHeader({ node, runtimeState, runtimeDiagnostic }: { node: WorkflowNode; runtimeState?: string; runtimeDiagnostic?: string }) {
  const state = runtimeState ?? node.state;
  return (
    <header className="node-header">
      <span>{node.title}</span>
      <small className={`state-dot ${state}`}>{state}</small>
      {runtimeDiagnostic ? <span className="node-diagnostic" title={runtimeDiagnostic}>{runtimeDiagnostic}</span> : null}
    </header>
  );
}

/** 三段式连接区：输入/输出端口 + 类型标签。 */
export function PortHandles({ node }: { node: WorkflowNode }) {
  return (
    <div className="node-ports">
      <div className="port-group port-group-inputs">
        {node.inputs.map((port) => (
          <div key={`in-${port.id}`} className="port-row port-row-input">
            <Handle
              id={port.id}
              type="target"
              position={Position.Left}
              className={`stream-handle ${portKindTone(port.kind)}`}
              title={`${port.label}: ${port.kind}`}
            />
            <span
              className={`port-label port-label-input ${portKindTone(port.kind)}`}
              title={`${port.label} · ${port.kind}`}
            >
              {port.kind}
            </span>
          </div>
        ))}
      </div>
      <div className="port-group port-group-outputs">
        {node.outputs.map((port) => (
          <div key={`out-${port.id}`} className="port-row port-row-output">
            <span
              className={`port-label port-label-output ${portKindTone(port.kind)}`}
              title={`${port.label} · ${port.kind}`}
            >
              {port.kind}
            </span>
            <Handle
              id={port.id}
              type="source"
              position={Position.Right}
              className={`stream-handle ${portKindTone(port.kind)}`}
              title={`${port.label}: ${port.kind}`}
            />
          </div>
        ))}
      </div>
    </div>
  );
}

export function portKindTone(kind: PortKind): string {
  if (kind.startsWith('workspace') || kind.startsWith('file')) return 'port-tone-workspace';
  if (kind.startsWith('control')) return 'port-tone-control';
  if (kind.startsWith('endpoint') || kind.startsWith('stream')) return 'port-tone-media';
  if (kind.startsWith('image') || kind.startsWith('layer') || kind.startsWith('viewer')) return 'port-tone-image';
  if (kind.startsWith('calib') || kind.startsWith('capture') || kind.startsWith('command')) return 'port-tone-calib';
  if (kind.startsWith('i2c') || kind.startsWith('eeprom')) return 'port-tone-io';
  if (kind.startsWith('status')) return 'port-tone-status';
  return 'port-tone-default';
}

/** 左侧节点库条目：拖拽或点击添加。 */
export function NodeLibraryItem({
  definition,
  onAdd,
  onDragStart,
}: {
  definition: NodeDefinition;
  onAdd: (kind: NodeKind) => void;
  onDragStart: (event: DragEvent<HTMLElement>, kind: NodeKind) => void;
}) {
  return (
    <button
      className="library-item"
      draggable
      type="button"
      title={`拖拽或点击添加：${definition.title}`}
      onClick={() => onAdd(definition.kind)}
      onDragStart={(event) => onDragStart(event, definition.kind)}
      onDragEnd={(event) => event.currentTarget.blur()}
    >
      <strong>{definition.title}</strong>
      <span>{definition.description}</span>
      <code>{definition.kind}</code>
    </button>
  );
}
