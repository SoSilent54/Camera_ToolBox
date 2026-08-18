import { useEffect, useState } from 'react';
import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { DEFAULT_RTSP_URL, NodeHeader, PortHandles, RuntimeOutputSummary, ScalarConfigFields } from './shared';

function numericConfig(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

/** RTSP 源节点：连接时由 StreamService 解码为视频帧，公开后端实际读取的连接参数。 */
export function RtspSourceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;
  const url = String(node.config.url ?? DEFAULT_RTSP_URL);
  const [draftUrl, setDraftUrl] = useState(url);
  useEffect(() => setDraftUrl(url), [url]);
  const applyUrl = () => nodeData.onRtspUrlChange?.(node.id, draftUrl);
  const connected = runtimeState === 'running';
  const actionPending = Boolean(nodeData.actionPending);
  const streamConfig = {
    transport: String(node.config.transport ?? 'tcp'),
    channel: numericConfig(node.config.channel, 0),
    width: numericConfig(node.config.width, 1920),
    height: numericConfig(node.config.height, 1080),
    connectTimeoutMs: numericConfig(node.config.connectTimeoutMs, 8000),
    idleTimeoutMs: numericConfig(node.config.idleTimeoutMs, 10000),
  };
  return (
    <div className="workflow-node-shell">
      <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
        <NodeHeader node={node} runtimeState={runtimeState} />

        <PortHandles node={node} />
        <div className="node-body">
          <label htmlFor={`${node.id}-url`}>RTSP URL</label>
          <input
            id={`${node.id}-url`}
            className="rtsp-url-input nodrag"
            value={draftUrl}
            spellCheck={false}
            onChange={(event) => setDraftUrl(event.target.value)}
            onBlur={applyUrl}
            onKeyDown={(event) => {
              if (event.key === 'Enter') {
                applyUrl();
                event.currentTarget.blur();
              }
            }}
          />
          <ScalarConfigFields
            nodeId={node.id}
            config={streamConfig}
            onChange={nodeData.onNodeConfigChange}
          />
          <span className="node-hint">输出已解码的视频帧；channel、尺寸与超时在下次连接时生效。</span>
          <RuntimeOutputSummary output={nodeData.runtimeOutput} />
          <div className="node-actions">
            <button
              type="button"
              className="nodrag nowheel"
              disabled={runtimeState === 'disabled' || actionPending}
              onClick={() => nodeData.onNodeAction?.(node.id, connected ? 'disconnect' : 'connect')}
            >
              {actionPending ? '处理中…' : connected ? '断开' : '连接'}
            </button>
          </div>
        </div>
      </section>
      {runtimeDiagnostic ? <div className="node-diagnostic-below" title={runtimeDiagnostic}>{runtimeDiagnostic}</div> : null}
    </div>
  );
}
