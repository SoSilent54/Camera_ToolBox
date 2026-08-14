import { useEffect, useState } from 'react';
import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from './workflow';
import { configText } from './nodeConfig';
import { NodeHeader, PortHandles } from './nodes/shared';
import { FileBrowser } from './FileBrowser';

/**
 * ① LocalFileSource（吸收 LocalWorkspace + FileBrowser + ImageFileSource）：
 * root + directory/selection（内嵌目录浏览）+ filter 展示。
 */
export function LocalFileSourceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const root = configText(node, 'root', '');
  const directory = configText(node, 'directory', '');
  const selection = configText(node, 'selection', '');
  const filter = configText(node, 'filter', '*.png;*.jpg;*.jpeg');
  const runtimeState = nodeData.runtimeState;
  const [draftRoot, setDraftRoot] = useState(root);
  useEffect(() => setDraftRoot(root), [root]);
  const applyRoot = () => nodeData.onNodeConfigChange?.(node.id, 'root', draftRoot.trim());
  return (
    <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} runtimeState={runtimeState} />
      <PortHandles node={node} />
      <div className="node-body">
        <label htmlFor={`${node.id}-root`}>Workspace root</label>
        <input
          id={`${node.id}-root`}
          className="rtsp-url-input nodrag"
          value={draftRoot}
          placeholder="/absolute/path/to/workspace"
          spellCheck={false}
          onChange={(event) => setDraftRoot(event.target.value)}
          onBlur={applyRoot}
          onKeyDown={(event) => {
            if (event.key === 'Enter') {
              applyRoot();
              event.currentTarget.blur();
            }
          }}
        />
        <FileBrowser
          root={root}
          directory={directory}
          selection={selection}
          onDirectory={(path) => nodeData.onNodeConfigChange?.(node.id, 'directory', path)}
          onSelection={(path) => nodeData.onNodeConfigChange?.(node.id, 'selection', path)}
        />
        <span>Filter: {filter}</span>
      </div>
    </section>
  );
}

/**
 * ② SftpFileSource（吸收 SftpWorkspace + FileBrowser remote）：
 * sourceId + remoteRoot 编辑，可选 workspace/fileRef/image 输出由 PortHandles 渲染。
 */
export function SftpFileSourceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const remoteRoot = configText(node, 'remoteRoot', '/');
  const sourceId = configText(node, 'sourceId', 'sftp-main');
  const [draftRoot, setDraftRoot] = useState(remoteRoot);
  useEffect(() => setDraftRoot(remoteRoot), [remoteRoot]);
  const applyRoot = () => nodeData.onNodeConfigChange?.(node.id, 'remoteRoot', draftRoot.trim() || '/');
  return (
    <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body">
        <label htmlFor={`${node.id}-remote-root`}>Remote root</label>
        <input
          id={`${node.id}-remote-root`}
          className="rtsp-url-input nodrag"
          value={draftRoot}
          placeholder="/data/captures"
          spellCheck={false}
          onChange={(event) => setDraftRoot(event.target.value)}
          onBlur={applyRoot}
          onKeyDown={(event) => {
            if (event.key === 'Enter') {
              applyRoot();
              event.currentTarget.blur();
            }
          }}
        />
        <span>Source ID: {sourceId}</span>
        <span>Session-bound; no password or directory cache is persisted.</span>
      </div>
    </section>
  );
}

/** SshSession：控制会话展示。SSH 参数编辑/expectedHostKey pin 属 M3（见 t8 遗留记录）。 */
export function SshSessionNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const host = configText(node, 'host', '');
  const profileId = configText(node, 'profileId', '');
  const username = configText(node, 'username', 'root');
  const expectedHostKey = configText(node, 'expectedHostKey', '');
  return (
    <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body compact">
        <span>Profile: {profileId || 'manual'}</span>
        <span>Host: {host || 'unset'}</span>
        <span>User: {username}</span>
        <span>Auto: {String(node.config.autoConnect === true)}</span>
        {expectedHostKey && <span>HostKey: {expectedHostKey.slice(0, 16)}…</span>}
      </div>
    </section>
  );
}

/**
 * ③ X5Device（吸收 X5RtspChannel + X5Snapshot）：
 * TCP 控制 + 多路 RTSP channel + snapshot/video 可选输出。
 */
export function X5DeviceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const channels = configText(node, 'channels', '[0]');
  return (
    <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body compact">
        <span>Host: {configText(node, 'host', '10.21.12.108')}</span>
        <span>TCP: {configText(node, 'tcpPort', '9073')}</span>
        <span>FPS: {configText(node, 'fps', '60')}</span>
        <span>Bitrate: {configText(node, 'bitrateKbps', '12000')} kbps</span>
        <span>Channels: {channels}</span>
      </div>
    </section>
  );
}
