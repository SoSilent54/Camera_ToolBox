import { useEffect, useState, type ReactNode } from 'react';
import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData, NodeActionName, X5ControlResponse, X5SnapshotMode } from './workflow';
import {
  configureX5Rtsp,
  inspectEepromProvision,
  previewEepromProvision,
  previewI2cTransfer,
  probeX5Control,
  registerSshPassword,
  runEepromProvision,
  runI2cTransfer,
  startX5RtspChannel,
  statusX5Control,
  stopX5RtspChannel,
  type EepromExecuteRequest,
  type EepromInspectRequest,
  type EepromPreviewRequest,
  type I2cExecuteRequest,
  type I2cPreviewRequest,
} from './workflow';
import { configText } from './nodeConfig';
import { NodeActionButtons, NodeHeader, PortHandles, RuntimeOutputSummary } from './nodes/shared';
import { FileBrowser } from './FileBrowser';

const SOURCE_TRIGGER_ACTIONS = [{ action: 'trigger', label: '加载' }] as const;

/** 本地图片源：根目录绝对路径，selection 是相对根目录的完整文件路径。 */
export function LocalFileSourceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const root = configText(node, 'root', '');
  const directory = configText(node, 'directory', '');
  const selection = configText(node, 'selection', '');
  const actionPending = Boolean(nodeData.actionPending);
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;
  const [draftRoot, setDraftRoot] = useState(root);
  useEffect(() => setDraftRoot(root), [root]);
  const applyRoot = () => nodeData.onNodeConfigChange?.(node.id, 'root', draftRoot.trim());
  return (
    <div className="workflow-node-shell">
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
          <span className="node-hint">选择 PNG/JPEG 后点击加载；目录仅用于浏览，不参与文件路径拼接。</span>
          <RuntimeOutputSummary output={nodeData.runtimeOutput} />
          <NodeActionButtons
            nodeId={node.id}
            actions={SOURCE_TRIGGER_ACTIONS}
            pending={actionPending}
            onAction={nodeData.onNodeAction}
            onRefreshOutput={nodeData.onRefreshNodeOutput}
          />
        </div>
      </section>
      {runtimeDiagnostic ? <div className="node-diagnostic-below" title={runtimeDiagnostic}>{runtimeDiagnostic}</div> : null}
    </div>
  );
}


/** SFTP 图片源：密码注册后直接连接 SFTP 并触发读取；不声明未实现的 workspace/SSH 端口。 */
export function SftpFileSourceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const remoteRoot = configText(node, 'remoteRoot', '/');
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;
  const actionPending = Boolean(nodeData.actionPending);
  const set = (key: string, value: string) => nodeData.onNodeConfigChange?.(node.id, key, value);
  return (
    <div className="workflow-node-shell">
      <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
        <NodeHeader node={node} runtimeState={runtimeState} />

        <PortHandles node={node} />
        <div className="node-body compact">
          <Field id={`${node.id}-host`} label="Host" value={configText(node, 'host', '')} onChange={(value) => set('host', value)} placeholder="camera.local" />
          <Field id={`${node.id}-port`} label="Port" value={configText(node, 'port', '22')} onChange={(value) => set('port', value)} type="number" />
          <Field id={`${node.id}-username`} label="User" value={configText(node, 'username', 'root')} onChange={(value) => set('username', value)} />
          <PasswordCredentialField nodeId={node.id} credentialRef={configText(node, 'credentialRef', '')} onCredentialRef={(value) => set('credentialRef', value)} />
          <Field id={`${node.id}-remote-root`} label="Remote root" value={remoteRoot} onChange={(value) => set('remoteRoot', value)} />
          <Field id={`${node.id}-selection`} label="Selection" value={configText(node, 'selection', '')} onChange={(value) => set('selection', value)} />
          <span className="node-hint">仅支持 PNG/JPEG；认证只使用当前服务端进程内密码 session，密码和私钥路径都不会写入工作流。</span>
          <RuntimeOutputSummary output={nodeData.runtimeOutput} />
          <NodeActionButtons
            nodeId={node.id}
            actions={SOURCE_TRIGGER_ACTIONS}
            pending={actionPending}
            onAction={nodeData.onNodeAction}
            onRefreshOutput={nodeData.onRefreshNodeOutput}
          />
        </div>
      </section>
      {runtimeDiagnostic ? <div className="node-diagnostic-below" title={runtimeDiagnostic}>{runtimeDiagnostic}</div> : null}
    </div>
  );
}


function Field({
  id,
  label,
  value,
  onChange,
  type = 'text',
  placeholder,
}: {
  id: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
  type?: 'text' | 'number' | 'password';
  placeholder?: string;
}) {
  return (
    <label className="node-config-field">
      <code>{label}</code>
      <input id={id} className="nodrag nowheel" type={type} value={value} placeholder={placeholder} onChange={(event) => onChange(event.target.value)} />
    </label>
  );
}

function SelectField({
  id,
  label,
  value,
  options,
  onChange,
}: {
  id: string;
  label: string;
  value: string;
  options: readonly { value: string; label: string }[];
  onChange: (value: string) => void;
}) {
  return (
    <label className="node-config-field">
      <code>{label}</code>
      <select id={id} className="nodrag nowheel" value={value} onChange={(event) => onChange(event.target.value)}>
        {options.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}
      </select>
    </label>
  );
}

function configBool(node: FlowNodeData['workflowNode'], key: string, fallback: boolean): boolean {
  const value = node.config[key];
  return typeof value === 'boolean' ? value : fallback;
}

function numberValue(value: string, fallback = 0): number {
  const parsed = Number(value.trim());
  return Number.isFinite(parsed) ? parsed : fallback;
}

function ResultBox({ value }: { value: unknown }) {
  if (value === undefined) return null;
  const text = typeof value === 'string' ? value : JSON.stringify(value);
  return <pre className="node-runtime-output" title={text}>{text}</pre>;
}

function RemoteFrame({ nodeData, selected, children }: { nodeData: FlowNodeData; selected?: boolean; children: ReactNode }) {
  const { workflowNode: node, runtimeState, runtimeDiagnostic } = nodeData;
  return (
    <div className="workflow-node-shell">
      <section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
        <NodeHeader node={node} runtimeState={runtimeState} />
        <PortHandles node={node} />
        <div className="node-body">{children}</div>
      </section>
      {runtimeDiagnostic ? <div className="node-diagnostic-below" title={runtimeDiagnostic}>{runtimeDiagnostic}</div> : null}
    </div>
  );
}

/** SSH 节点只接受密码；密码写入服务端进程内凭据库，图仅保存 session 引用。 */
export function SshSessionNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const set = (key: string, value: string | boolean) => nodeData.onNodeConfigChange?.(node.id, key, value);
  return (
    <RemoteFrame nodeData={nodeData} selected={selected}>
      <Field id={`${node.id}-host`} label="Host" value={configText(node, 'host', '')} onChange={(value) => set('host', value)} placeholder="camera.local" />
      <Field id={`${node.id}-port`} label="Port" value={configText(node, 'port', '22')} onChange={(value) => set('port', value)} type="number" />
      <Field id={`${node.id}-username`} label="User" value={configText(node, 'username', 'root')} onChange={(value) => set('username', value)} />
      <PasswordCredentialField nodeId={node.id} credentialRef={configText(node, 'credentialRef', '')} onCredentialRef={(value) => set('credentialRef', value)} />
      <label className="node-hint">Password authentication only. The password is registered in the current server process and is never saved in the workflow.</label>
      <div className="node-actions">
        <button type="button" className="nodrag nowheel" disabled={!nodeData.onNodeAction} onClick={() => nodeData.onNodeAction?.(node.id, 'trigger')}>Run recipe</button>
      </div>
    </RemoteFrame>
  );
}

/** X5_233 Driver 通过后端 TCP 请求执行；RTSP 编码和快照匹配参数分别持久化，避免互相污染。 */
export function X5233DriverNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const host = configText(node, 'host', '10.21.12.108');
  const tcpPort = numberValue(configText(node, 'tcpPort', '9073'), 9073);
  const rtspChannel = x5Channel(node, 'rtspChannel');
  const snapshotChannel = x5Channel(node, 'snapshotChannel');
  const snapshotMode = x5SnapshotMode(configText(node, 'snapshotMode', 'latest'));
  const rawCamera = numberValue(configText(node, 'rawCamera', '0'), 0);
  const [pending, setPending] = useState(false);
  const [result, setResult] = useState<X5ControlResponse | string>();
  const set = (key: string, value: string) => nodeData.onNodeConfigChange?.(node.id, key, value);
  const call = async (operation: () => Promise<X5ControlResponse>) => {
    setPending(true);
    try { setResult(await operation()); } catch (error) { setResult(String(error)); } finally { setPending(false); }
  };
  const binding = { host, tcpPort };
  const actionPending = Boolean(nodeData.actionPending);
  const workflowActionDisabled = !nodeData.onNodeAction || actionPending;
  const captureDisabled = workflowActionDisabled;
  const workflowAction = (action: 'open_rtsp_ch0' | 'open_rtsp_ch3' | 'open_rtsp_all' | 'close_rtsp' | 'probe' | 'status' | 'capture_yuv' | 'capture_raw') => nodeData.onNodeAction?.(node.id, action);
  return (
    <RemoteFrame nodeData={nodeData} selected={selected}>
      <strong>Connection</strong>
      <Field id={`${node.id}-host`} label="Host" value={host} onChange={(value) => set('host', value)} />
      <Field id={`${node.id}-tcp-port`} label="TCP port" value={configText(node, 'tcpPort', '9073')} onChange={(value) => set('tcpPort', value)} type="number" />

      <strong>RTSP stream</strong>
      <Field id={`${node.id}-rtsp-channel`} label="RTSP channel" value={configText(node, 'rtspChannel', String(rtspChannel))} onChange={(value) => set('rtspChannel', value)} type="number" />
      <Field id={`${node.id}-fps`} label="Encoder FPS" value={configText(node, 'fps', '60')} onChange={(value) => set('fps', value)} type="number" />
      <Field id={`${node.id}-bitrate`} label="Encoder kbps" value={configText(node, 'bitrateKbps', '12000')} onChange={(value) => set('bitrateKbps', value)} type="number" />
      <div className="node-actions">
        <button type="button" className="nodrag nowheel" disabled={workflowActionDisabled} onClick={() => workflowAction('probe')}>{actionPending ? '查询中…' : 'Probe'}</button>
        <button type="button" className="nodrag nowheel" disabled={workflowActionDisabled} onClick={() => workflowAction('status')}>{actionPending ? '查询中…' : 'Status'}</button>
        <button type="button" className="nodrag nowheel" disabled={workflowActionDisabled} onClick={() => workflowAction('open_rtsp_ch0')}>{actionPending ? '连接中…' : 'RTSP Open CH0'}</button>
        <button type="button" className="nodrag nowheel" disabled={workflowActionDisabled} onClick={() => workflowAction('open_rtsp_ch3')}>{actionPending ? '连接中…' : 'RTSP Open CH3'}</button>
        <button type="button" className="nodrag nowheel" disabled={workflowActionDisabled} onClick={() => workflowAction('open_rtsp_all')}>{actionPending ? '连接中…' : 'RTSP Open CH0+CH3'}</button>
        <button type="button" className="nodrag nowheel" disabled={workflowActionDisabled} onClick={() => workflowAction('close_rtsp')}>{actionPending ? '断开中…' : 'RTSP Close All'}</button>
        <button type="button" className="nodrag nowheel" disabled={pending} onClick={() => call(() => configureX5Rtsp({ ...binding, fps: numberValue(configText(node, 'fps', '60'), 60), bitrateKbps: numberValue(configText(node, 'bitrateKbps', '12000'), 12000) }))}>Configure RTSP encoder</button>
        <button type="button" className="nodrag nowheel" disabled={pending} onClick={() => call(() => startX5RtspChannel({ ...binding, channel: rtspChannel }))}>Start RTSP encoder</button>
        <button type="button" className="nodrag nowheel" disabled={pending} onClick={() => call(() => stopX5RtspChannel({ ...binding, channel: rtspChannel }))}>Stop RTSP encoder</button>
      </div>

      <strong>Manual Capture</strong>
      <Field id={`${node.id}-snapshot-channel`} label="YUV channel" value={configText(node, 'snapshotChannel', String(snapshotChannel))} onChange={(value) => set('snapshotChannel', value)} type="number" />
      <SelectField
        id={`${node.id}-snapshot-mode`}
        label="YUV match mode"
        value={snapshotMode}
        options={X5_SNAPSHOT_MODES}
        onChange={(value) => set('snapshotMode', value)}
      />
      {snapshotMode === 'frame_id' ? <Field id={`${node.id}-snapshot-frame-id`} label="Frame ID" value={configText(node, 'snapshotFrameId', '')} onChange={(value) => set('snapshotFrameId', value)} type="number" /> : null}
      {snapshotMode === 'timestamp_ns' ? <Field id={`${node.id}-snapshot-timestamp`} label="Timestamp ns" value={configText(node, 'snapshotTimestampNs', '')} onChange={(value) => set('snapshotTimestampNs', value)} type="number" /> : null}
      <Field id={`${node.id}-raw-camera`} label="RAW camera" value={configText(node, 'rawCamera', String(rawCamera))} onChange={(value) => set('rawCamera', value)} type="number" />
      <div className="node-actions">
        <button type="button" className="nodrag nowheel" disabled={captureDisabled} onClick={() => workflowAction('capture_yuv')}>{actionPending ? '采集中…' : 'Capture YUV'}</button>
        <button type="button" className="nodrag nowheel" disabled={captureDisabled} onClick={() => workflowAction('capture_raw')}>{actionPending ? '采集中…' : 'Capture RAW'}</button>
      </div>
      <span className="node-hint">直接连线只建立图边，不会自动拉流；RTSP Open CH0/CH3/CH0+CH3 才会在本工作流中拉取对应端口并向 videoCh0/videoCh3 输出帧。Start/Stop RTSP encoder 只控制板端编码器。手动采集：YUV 走 snapshotChannel/mode，RAW 只支持 latest；RAW 解释参数统一在 Demosaic 节点选择。</span>
      <RuntimeOutputSummary output={nodeData.runtimeOutput} />
      <ResultBox value={result} />
    </RemoteFrame>
  );
}

/** Demosaic 节点：显式配置 RAW 解释参数与输出像素格式。 */
export function DemosaicNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const algorithm = configText(node, 'algorithm', 'bilinear');
  const outputFormatDraft = configText(node, 'outputFormat', 'rgba');
  const outputFormat = DEMOSAIC_OUTPUT_FORMATS.some((option) => option.value === outputFormatDraft) ? outputFormatDraft : 'rgba';
  const bayer = configText(node, 'bayer', 'rggb');
  const bitsPerSample = configText(node, 'bitsPerSample', '12');
  const blackLevel = configText(node, 'blackLevel', '0');
  const set = (key: string, value: string) => nodeData.onNodeConfigChange?.(node.id, key, value);
  return (
    <RemoteFrame nodeData={nodeData} selected={selected}>
      <strong>Decode</strong>
      <SelectField
        id={`${node.id}-algorithm`}
        label="Algorithm"
        value={algorithm}
        options={DEMOSAIC_ALGORITHMS}
        onChange={(value) => set('algorithm', value)}
      />
      <SelectField
        id={`${node.id}-output-format`}
        label="Output format"
        value={outputFormat}
        options={DEMOSAIC_OUTPUT_FORMATS}
        onChange={(value) => set('outputFormat', value)}
      />
      <SelectField
        id={`${node.id}-bayer`}
        label="RAW Bayer"
        value={bayer}
        options={X5_RAW_BAYER_PATTERNS}
        onChange={(value) => set('bayer', value)}
      />
      <Field id={`${node.id}-bits`} label="RAW bits" value={bitsPerSample} onChange={(value) => set('bitsPerSample', value)} type="number" />
      <Field id={`${node.id}-black-level`} label="Black level" value={blackLevel} onChange={(value) => set('blackLevel', value)} type="number" />
      <span className="node-hint">Demosaic 仅做显式 RAW 解释；Bayer / bit depth / black level 用于 RAW 归一化，Output format 决定实际输出像素格式。</span>
      <RuntimeOutputSummary output={nodeData.runtimeOutput} />
    </RemoteFrame>
  );
}

type DemosaicOutputFormat = 'rgba' | 'gray8' | 'gray16le';

const DEMOSAIC_ALGORITHMS: readonly { value: string; label: string }[] = [
  { value: 'bilinear', label: 'Bilinear' },
];

const DEMOSAIC_OUTPUT_FORMATS: readonly { value: DemosaicOutputFormat; label: string }[] = [
  { value: 'rgba', label: 'RGBA8' },
  { value: 'gray8', label: 'GRAY8' },
  { value: 'gray16le', label: 'GRAY16LE' },
];

/** Hex Arm 仅通过工作流节点动作控制；所有关节值以逗号分隔的弧度保存，运动按钮必须经过双重门禁。 */
export function HexArmDeviceNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const [jointDraft, setJointDraft] = useState(configText(node, 'jointPositions', ''));
  useEffect(() => setJointDraft(configText(node, 'jointPositions', '')), [node.config.jointPositions]);
  const set = (key: string, value: string | boolean) => nodeData.onNodeConfigChange?.(node.id, key, value);
  const jointPositionsValid = jointDraft.split(',').map((item) => item.trim()).every((item) => item.length > 0 && Number.isFinite(Number(item)));
  const jointPositionsPersisted = configText(node, 'jointPositions', '').trim() === jointDraft.trim();
  const transport = configText(node, 'transport', 'websocket');
  const controlEnabled = configBool(node, 'controlEnabled', false);
  const transportSupported = transport === 'websocket';
  const actionsDisabled = !nodeData.onNodeAction || !transportSupported;
  const send = (action: NodeActionName) => nodeData.onNodeAction?.(node.id, action);
  const applyJointPositions = () => {
    if (jointPositionsValid) set('jointPositions', jointDraft.trim());
  };
  return (
    <RemoteFrame nodeData={nodeData} selected={selected}>
      <strong>Connection</strong>
      <Field id={`${node.id}-host`} label="Host" value={configText(node, 'host', '127.0.0.1')} onChange={(value) => set('host', value)} />
      <Field id={`${node.id}-port`} label="Port" value={configText(node, 'port', '8439')} onChange={(value) => set('port', value)} type="number" />
      <SelectField
        id={`${node.id}-transport`}
        label="Transport"
        value={transportSupported ? transport : 'websocket'}
        options={[{ value: 'websocket', label: 'WebSocket binary protobuf' }]}
        onChange={(value) => set('transport', value)}
      />
      {!transportSupported ? <span className="node-hint">KCP is unsupported and will not fall back to WebSocket; save a WebSocket transport before sending commands.</span> : null}
      <Field id={`${node.id}-command-timeout`} label="Command timeout ms" value={configText(node, 'commandTimeoutMs', '200')} onChange={(value) => set('commandTimeoutMs', value)} type="number" />
      <Field id={`${node.id}-connect-timeout`} label="Connect timeout ms" value={configText(node, 'connectTimeoutMs', '3000')} onChange={(value) => set('connectTimeoutMs', value)} type="number" />
      <div className="node-actions">
        <button type="button" className="nodrag nowheel" disabled={actionsDisabled} onClick={() => send('probe')}>Probe</button>
        <button type="button" className="nodrag nowheel" disabled={actionsDisabled} onClick={() => send('status')}>Status</button>
        <button type="button" className="nodrag nowheel" disabled={actionsDisabled} onClick={() => send('connect')}>Connect</button>
        <button type="button" className="nodrag nowheel" disabled={actionsDisabled} onClick={() => send('disconnect')}>Disconnect</button>
      </div>
      <strong>API control</strong>
      <div className="node-actions">
        <button type="button" className="nodrag nowheel" disabled={actionsDisabled} onClick={() => send('initialize_api_control')}>Initialize API control</button>
        <button type="button" className="nodrag nowheel" disabled={actionsDisabled} onClick={() => send('calibrate')}>Calibrate</button>
        <button type="button" className="nodrag nowheel" disabled={actionsDisabled} onClick={() => send('clear_parking_stop')}>Clear parking stop</button>
        <button type="button" className="nodrag nowheel" disabled={actionsDisabled} onClick={() => send('zero_current')}>Zero current</button>
      </div>
      <strong>Motion</strong>
      <label className="node-config-checkbox"><code>Enable motion control</code><input className="nodrag nowheel" type="checkbox" checked={controlEnabled} onChange={(event) => set('controlEnabled', event.target.checked)} /></label>
      <label className="node-config-field">
        <code>Joint radians</code>
        <input
          id={`${node.id}-joint-radians`}
          className="nodrag nowheel"
          value={jointDraft}
          placeholder="0.0, -1.57, …"
          onChange={(event) => setJointDraft(event.target.value)}
          onBlur={applyJointPositions}
          onKeyDown={(event) => { if (event.key === 'Enter') { applyJointPositions(); event.currentTarget.blur(); } }}
        />
      </label>
      {!jointPositionsValid ? <span className="node-hint">Enter a non-empty comma-separated list of finite joint radians before sending motion.</span> : null}
      {!jointPositionsPersisted ? <span className="node-hint">Apply the edited joint radians before sending motion.</span> : null}
      <div className="node-actions">
        <button type="button" className="nodrag nowheel" disabled={actionsDisabled || !controlEnabled || !jointPositionsValid || !jointPositionsPersisted} onClick={() => send('send_joint_positions')}>Send joint positions</button>
      </div>
      <span className="node-hint">Motion is off by default. Sending positions requires this checkbox and finite radians; the backend independently requires enabled control, an active connection, and initialized API control.</span>
      <RuntimeOutputSummary output={nodeData.runtimeOutput} />
    </RemoteFrame>
  );
}


const X5_SNAPSHOT_MODES: readonly { value: X5SnapshotMode; label: string }[] = [
  { value: 'latest', label: 'Latest frame' },
  { value: 'frame_id', label: 'Exact frame ID' },
  { value: 'timestamp_ns', label: 'Exact capture timestamp' },
];

const X5_RAW_BAYER_PATTERNS: readonly { value: string; label: string }[] = [
  { value: 'rggb', label: 'RGGB' },
  { value: 'bggr', label: 'BGGR' },
  { value: 'grbg', label: 'GRBG' },
  { value: 'gbrg', label: 'GBRG' },
];

function x5SnapshotMode(value: string): X5SnapshotMode {
  if (X5_SNAPSHOT_MODES.some((option) => option.value === value)) {
    return value as X5SnapshotMode;
  }
  return 'latest';
}

function x5Channel(node: FlowNodeData['workflowNode'], key: 'rtspChannel' | 'snapshotChannel'): number {
  const legacyChannels = node.config.channels;
  const legacyChannel = Array.isArray(legacyChannels) && typeof legacyChannels[0] === 'number'
    ? legacyChannels[0]
    : numberValue(configText(node, 'channel', '0'));
  const value = numberValue(configText(node, key, String(legacyChannel)), legacyChannel);
  return Number.isInteger(value) && value >= 0 && value <= 65_535 ? value : legacyChannel;
}


/** 用密码替换当前节点对应的进程内 session；输入在服务端登记后立即清空。 */
function PasswordCredentialField({ nodeId, credentialRef, onCredentialRef }: { nodeId: string; credentialRef: string; onCredentialRef: (value: string) => void }) {
  const [password, setPassword] = useState('');
  const [pending, setPending] = useState(false);
  const [message, setMessage] = useState<string>();
  const register = async () => {
    setPending(true);
    try {
      const result = await registerSshPassword(nodeId, password);
      onCredentialRef(result.credentialRef);
      setPassword('');
      setMessage('Password registered for this server process.');
    } catch (error) {
      setMessage(String(error));
    } finally {
      setPending(false);
    }
  };
  return (
    <>
      <Field id={`${nodeId}-password`} label="Password" value={password} onChange={setPassword} type="password" placeholder="not saved" />
      <div className="node-actions">
        <button type="button" className="nodrag nowheel" disabled={pending || !password} onClick={register}>Use password</button>
      </div>
      <span className="node-hint">{credentialRef ? 'Password is registered for this server process.' : 'Register a password before running remote operations.'}</span>
      {message ? <span className="node-hint">{message}</span> : null}
    </>
  );
}

const I2C_BUS_OPTIONS = Array.from({ length: 8 }, (_, index) => ({ value: `i2c-${index}`, label: `I²C ${index}` }));
function sshBinding(node: FlowNodeData['workflowNode']) {
  return {
    host: configText(node, 'host', ''),
    port: numberValue(configText(node, 'port', '22'), 22),
    username: configText(node, 'username', 'root'),
    credentialRef: configText(node, 'credentialRef', ''),
  };
}

function I2cForm({ data, selected, eeprom = false }: { data: NodeProps['data']; selected?: boolean; eeprom?: boolean }) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const set = (key: string, value: string | boolean) => nodeData.onNodeConfigChange?.(node.id, key, value);
  const rawPayload = configText(node, 'payload', '').trim();
  const payload = rawPayload
    ? rawPayload.split(/[\s,;]+/).filter(Boolean).map((item) => numberValue(item, -1)).filter((item) => Number.isInteger(item) && item >= 0 && item <= 255)
    : [];
  const base = {
    nodeId: node.id,
    profileId: configText(node, 'profileId', 'x5-lab'),
    bus: configText(node, 'bus', 'i2c-1'),
    address: numberValue(configText(node, 'address', '0x50')),
    register: numberValue(configText(node, 'register', '0x0000')),
    payload,
    pageSize: numberValue(configText(node, 'pageSize', '16'), 16),
  };
  const [pending, setPending] = useState(false);
  const [preview, setPreview] = useState<unknown>();
  const [snapshot, setSnapshot] = useState<string>();
  const run = async (operation: () => Promise<unknown>) => {
    setPending(true);
    try {
      const result = await operation();
      setPreview(result);
      if (typeof result === 'object' && result !== null && 'snapshot' in result) {
        const candidate = result.snapshot;
        if (typeof candidate === 'object' && candidate !== null && 'imageSha256' in candidate && typeof candidate.imageSha256 === 'string') {
          setSnapshot(candidate.imageSha256);
        }
      }
    } catch (error) {
      setPreview(String(error));
    } finally {
      setPending(false);
    }
  };
  const ssh = sshBinding(node);
  const warning = eeprom
    ? 'EEPROM writes are irreversible. Inspect first, require verifyAfterWrite, and confirm only after checking the preview.'
    : 'I²C writes can change hardware state. Preview validates the request but performs no I/O.';
  const i2cPreview: I2cPreviewRequest = { ...base, operation: configText(node, 'mode', 'read') === 'write' ? 'write' : 'read' };
  const eepromPreview: EepromPreviewRequest = { ...base, mapId: configText(node, 'mapId', 'yg-stereo-p24c64g-v1'), verifyAfterWrite: configBool(node, 'verifyAfterWrite', true) };
  const eepromInspect: EepromInspectRequest = { ...eepromPreview, ssh };
  const eepromExecute: EepromExecuteRequest | undefined = snapshot
    ? { ...eepromPreview, ssh, confirmExecution: true, expectedBeforeSha256: snapshot }
    : undefined;
  const i2cExecute: I2cExecuteRequest = {
    ...i2cPreview,
    ssh,
    confirmExecution: configText(node, 'mode', 'read') !== 'write' || configBool(node, 'confirmWrites', false),
  };
  return (
    <RemoteFrame nodeData={nodeData} selected={selected}>
      <Field id={`${node.id}-profile`} label="Profile" value={base.profileId} onChange={(value) => set('profileId', value)} />
      <Field id={`${node.id}-host`} label="SSH host" value={ssh.host} onChange={(value) => set('host', value)} />
      <Field id={`${node.id}-port`} label="SSH port" value={configText(node, 'port', '22')} onChange={(value) => set('port', value)} type="number" />
      <Field id={`${node.id}-user`} label="SSH user" value={ssh.username} onChange={(value) => set('username', value)} />
      <PasswordCredentialField nodeId={node.id} credentialRef={ssh.credentialRef} onCredentialRef={(value) => set('credentialRef', value)} />
      <SelectField id={`${node.id}-bus`} label="I²C bus" value={base.bus} options={I2C_BUS_OPTIONS} onChange={(value) => set('bus', value)} />
      <Field id={`${node.id}-address`} label="Address" value={configText(node, 'address', '0x50')} onChange={(value) => set('address', value)} />
      <Field id={`${node.id}-register`} label="Register" value={configText(node, 'register', '0x0000')} onChange={(value) => set('register', value)} />
      <Field id={`${node.id}-payload`} label="Payload bytes" value={configText(node, 'payload', '')} onChange={(value) => set('payload', value)} placeholder="0x01 0x02 …" />
      <Field id={`${node.id}-page-size`} label="Page size" value={configText(node, 'pageSize', '16')} onChange={(value) => set('pageSize', value)} type="number" />
      {eeprom ? <SelectField id={`${node.id}-map`} label="EEPROM map" value={configText(node, 'mapId', 'yg-stereo-p24c64g-v1')} options={[{ value: 'yg-stereo-p24c64g-v1', label: 'YG Stereo P24C64G v1' }]} onChange={(value) => set('mapId', value)} /> : null}
      {eeprom ? <label className="node-config-checkbox"><code>Verify after write</code><input className="nodrag nowheel" type="checkbox" checked={configBool(node, 'verifyAfterWrite', true)} onChange={(event) => set('verifyAfterWrite', event.target.checked)} /></label> : <SelectField id={`${node.id}-mode`} label="Mode" value={configText(node, 'mode', 'read')} options={[{ value: 'read', label: 'Read' }, { value: 'write', label: 'Write' }]} onChange={(value) => set('mode', value)} />}
      <span className="node-hint">This node uses inline SSH host/user plus a process-local password session; private keys and host-key pins are not part of this workflow.</span>
      <span className="node-hint">{warning}</span>
      <div className="node-actions">
        <button type="button" className="nodrag nowheel" disabled={pending} onClick={() => run(() => eeprom ? previewEepromProvision(eepromPreview) : previewI2cTransfer(i2cPreview))}>Preview (no I/O)</button>
        {eeprom ? <button type="button" className="nodrag nowheel" disabled={pending} onClick={() => run(() => inspectEepromProvision(eepromInspect))}>Inspect</button> : null}
        <button type="button" className="nodrag nowheel" disabled={pending || (eeprom && !eepromExecute)} onClick={() => run(() => eeprom ? runEepromProvision(eepromExecute!) : runI2cTransfer(i2cExecute))}>{eeprom ? 'Provision (confirm)' : 'Run'}</button>
      </div>
      {eeprom && !snapshot ? <span className="node-hint">Inspect must succeed in this process before Provision is enabled.</span> : null}
      <ResultBox value={preview} />
    </RemoteFrame>
  );
}

export function I2cTransferNode({ data, selected }: NodeProps) {
  return <I2cForm data={data} selected={selected} />;
}

export function EepromProvisionNode({ data, selected }: NodeProps) {
  return <I2cForm data={data} selected={selected} eeprom />;
}
