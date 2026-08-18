import { useEffect, useState, type DragEvent, type KeyboardEvent } from 'react';
import { Handle, Position } from '@xyflow/react';
import type { NodeActionControl, NodeActionName, NodeDefinition, NodeKind, PortKind, ScalarConfigValue, WorkflowNode } from '../workflow';

export const DEFAULT_RTSP_URL = 'rtsp://10.21.12.108:554/PRR';
/** 节点标题 + 实时状态点；状态优先取引擎实时值，回退到持久化 state。 */
export function NodeHeader({ node, runtimeState }: { node: WorkflowNode; runtimeState?: string }) {
  const state = runtimeState ?? node.state;
  return (
    <header className="node-header">
      <span>{node.title}</span>
      <small className={`state-dot ${state}`}>{state}</small>
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

/** 仅暴露可无歧义序列化回工作流的标量配置，复杂对象仍由专用节点负责。 */
export function ScalarConfigFields({
  nodeId,
  config,
  onChange,
}: {
  nodeId: string;
  config: Record<string, unknown>;
  onChange?: (nodeId: string, key: string, value: ScalarConfigValue) => void;
}) {
  const entries = Object.entries(config).filter((entry): entry is [string, ScalarConfigValue] => isScalarConfigValue(entry[1]));
  if (entries.length === 0) {
    return <span className="node-hint">no scalar config</span>;
  }
  return (
    <div className="node-config-fields">
      {entries.map(([key, value]) => (
        <ScalarConfigField key={key} nodeId={nodeId} name={key} value={value} onChange={onChange} />
      ))}
    </div>
  );
}

function ScalarConfigField({
  nodeId,
  name,
  value,
  onChange,
}: {
  nodeId: string;
  name: string;
  value: ScalarConfigValue;
  onChange?: (nodeId: string, key: string, value: ScalarConfigValue) => void;
}) {
  const [draft, setDraft] = useState(String(value));
  useEffect(() => setDraft(String(value)), [value]);

  if (typeof value === 'boolean') {
    return (
      <label className="node-config-field node-config-checkbox">
        <code>{name}</code>
        <input
          className="nodrag nowheel"
          type="checkbox"
          checked={value}
          disabled={!onChange}
          onChange={(event) => onChange?.(nodeId, name, event.target.checked)}
        />
      </label>
    );
  }

  const commit = () => {
    if (!onChange) {
      return;
    }
    if (typeof value === 'number') {
      const next = Number(draft);
      if (!Number.isFinite(next)) {
        setDraft(String(value));
        return;
      }
      if (!Object.is(next, value)) {
        onChange(nodeId, name, next);
      }
      return;
    }
    if (draft !== value) {
      onChange(nodeId, name, draft);
    }
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === 'Enter') {
      event.currentTarget.blur();
    }
  };

  return (
    <label className="node-config-field">
      <code>{name}</code>
      <input
        className="nodrag nowheel"
        type={typeof value === 'number' ? 'number' : 'text'}
        value={draft}
        disabled={!onChange}
        onChange={(event) => setDraft(event.target.value)}
        onBlur={commit}
        onKeyDown={handleKeyDown}
      />
    </label>
  );
}

/** 通用动作区仅呈现显式能力，避免为未实现动作的节点制造无效入口。 */
export function NodeActionButtons({
  nodeId,
  actions,
  pending = false,
  onAction,
  onRefreshOutput,
}: {
  nodeId: string;
  actions?: readonly NodeActionControl[];
  pending?: boolean;
  onAction?: (nodeId: string, action: NodeActionName) => void;
  onRefreshOutput?: (nodeId: string) => void;
}) {
  if ((!actions || actions.length === 0) && !onRefreshOutput) {
    return null;
  }
  return (
    <div className="node-actions">
      {actions?.map(({ action, label }) => (
        <button
          key={action}
          type="button"
          className="nodrag nowheel"
          disabled={!onAction || pending}
          onClick={() => onAction?.(nodeId, action)}
        >
          {pending ? '处理中…' : label}
        </button>
      ))}
      {onRefreshOutput ? (
        <button
          type="button"
          className="nodrag nowheel"
          disabled={pending}
          onClick={() => onRefreshOutput(nodeId)}
        >
          刷新结果
        </button>
      ) : null}
    </div>
  );
}

/** 将任意 JSON 输出压缩为节点内安全摘要，完整 JSON 保留在 title 中。 */
export function RuntimeOutputSummary({ output }: { output: unknown }) {
  if (output === undefined) {
    return null;
  }
  const full = stringifyRuntimeOutput(output);
  const summary = full.length > 160 ? `${full.slice(0, 159)}…` : full;
  return <div className="node-runtime-output" title={full}>{summary}</div>;
}

function isScalarConfigValue(value: unknown): value is ScalarConfigValue {
  return typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean';
}

function stringifyRuntimeOutput(output: unknown): string {
  if (typeof output === 'string') {
    return output;
  }
  try {
    return JSON.stringify(output) ?? String(output);
  } catch {
    return String(output);
  }
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
