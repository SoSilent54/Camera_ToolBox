import { useEffect, useState } from 'react';
import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { DEFAULT_RTSP_URL, NodeHeader, PortHandles } from './shared';

/** RTSP 源节点：编辑 URL + Connect/Disconnect 触发引擎连接。 */
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
  return (
    <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} runtimeState={runtimeState} runtimeDiagnostic={runtimeDiagnostic} />

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
        <span>Transport: {String(node.config.transport ?? 'tcp')}</span>
        <div className="node-actions">
          <button
            type="button"
            disabled={runtimeState === 'disabled'}
            onClick={() => nodeData.onNodeAction?.(node.id, connected ? 'disconnect' : 'connect')}
          >
            {connected ? '断开' : '连接'}
          </button>
        </div>
      </div>
    </section>
  );
}
