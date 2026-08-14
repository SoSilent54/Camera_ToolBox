import { useEffect, useRef, useState } from 'react';
import { NodeResizer, type NodeProps } from '@xyflow/react';
import type { FlowNodeData } from './workflow';
import { configText, normalizeSourcePathDraft } from './nodeConfig';
import { NodeHeader, PortHandles } from './nodes/shared';
import { FileBrowser } from './FileBrowser';


export function LocalWorkspaceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const root = typeof node.config.root === 'string' ? node.config.root : '';
  const [draftRoot, setDraftRoot] = useState(root);
  useEffect(() => setDraftRoot(root), [root]);
  const applyRoot = () => nodeData.onLocalImageConfigChange?.(node.id, 'root', draftRoot);
  return (
    <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
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
        <span>Explicit root; directories are never scanned.</span>
      </div>
    </section>
  );
}

export function SftpWorkspaceNode({ data, selected }: NodeProps) {
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
export function FileBrowserNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const root = configText(node, 'root', '');
  const directory = configText(node, 'directory', '');
  const selection = configText(node, 'selection', '');
  const runtimeState = nodeData.runtimeState;
  const [draftRoot, setDraftRoot] = useState(root);
  useEffect(() => setDraftRoot(root), [root]);
  const applyRoot = () => nodeData.onNodeConfigChange?.(node.id, 'root', draftRoot.trim());
  return (
    <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} runtimeState={runtimeState} />
      <PortHandles node={node} />
      <div className="node-body">
        <label htmlFor={`${node.id}-root`}>Workspace root</label>
        <input
          id={`${node.id}-root`}
          className="rtsp-url-input nodrag"
          value={draftRoot}
          placeholder="/absolute/path"
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
      </div>
    </section>
  );
}

export function SshSessionNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const host = configText(node, 'host', '');
  const profileId = configText(node, 'profileId', '');
  const username = configText(node, 'username', 'root');
  return (
    <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body compact">
        <span>Profile: {profileId || 'manual'}</span>
        <span>Host: {host || 'unset'}</span>
        <span>User: {username}</span>
        <span>Auto: {String(node.config.autoConnect === true)}</span>
      </div>
    </section>
  );
}

export function X5DeviceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  return (
    <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body compact">
        <span>Host: {configText(node, 'host', '10.21.12.108')}</span>
        <span>TCP: {configText(node, 'tcpPort', '9073')}</span>
        <span>Control: X5 TCP + RTSP channel catalog</span>
      </div>
    </section>
  );
}

export function X5RtspChannelNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  return (
    <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body compact">
        <span>Channel: {configText(node, 'channel', '0')}</span>
        <span>Path: {configText(node, 'path', '/PRR')}</span>
        <span>Source: X5 device RTSP endpoint</span>
      </div>
    </section>
  );
}

export function X5SnapshotNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  return (
    <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body compact">
        <span>Mode: {configText(node, 'mode', 'latest')}</span>
        <span>Capture: X5 TCP snapshot</span>
        <span>Output: image frame</span>
      </div>
    </section>
  );
}

export function ImageFileSourceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const relativePath = typeof node.config.relativePath === 'string' ? node.config.relativePath : '';
  const [draftPath, setDraftPath] = useState(relativePath);
  useEffect(() => setDraftPath(relativePath), [relativePath]);
  const applyPath = () => nodeData.onLocalImageConfigChange?.(node.id, 'relativePath', draftPath);
  return (
    <section className={`workflow-node source-node ${selected ? 'selected' : ''}`}>
      <NodeHeader node={node} />
      <PortHandles node={node} />
      <div className="node-body">
        <label htmlFor={`${node.id}-relative-path`}>Image path</label>
        <input
          id={`${node.id}-relative-path`}
          className="rtsp-url-input nodrag"
          value={draftPath}
          placeholder="images/example.png"
          spellCheck={false}
          onChange={(event) => setDraftPath(event.target.value)}
          onBlur={applyPath}
          onKeyDown={(event) => {
            if (event.key === 'Enter') {
              applyPath();
              event.currentTarget.blur();
            }
          }}
        />
        <span>Path is relative to the connected workspace/file ref.</span>
      </div>
    </section>
  );
}

