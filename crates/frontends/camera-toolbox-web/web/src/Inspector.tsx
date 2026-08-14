import { useEffect, useState } from 'react';
import type { Edge } from '@xyflow/react';
import {
  captureX5Snapshot,
  configureX5Rtsp,
  inspectEepromProvision,
  labelForPortKind,
  previewEepromProvision,
  previewI2cTransfer,
  probeX5Control,
  runCalibrationSolver,
  runEepromProvision,
  runI2cTransfer,
  startX5RtspChannel,
  statusX5Control,
  stopX5RtspChannel,
  type CalibrationRequest,
  type CalibrationSolution,
  type ControlExecutionResult,
  type ControlRequestPreview,
  type EepromInspectResponse,
  type FlowEdgeData,
  type NodeKind,
  type RuntimeGraphStatus,
  type SshExecutionBinding,
  type WorkflowNode,
  type X5BindingRequest,
  type X5ControlResponse,
  type X5SnapshotMode,
} from './workflow';
import { configText, normalizeSourcePathDraft } from './nodeConfig';

type FlowEdge = Edge<FlowEdgeData>;
export type Selection =
  | { type: 'node'; node: WorkflowNode }
  | { type: 'edge'; edge: FlowEdge }
  | { type: 'none' };

export function Inspector({
  events,
  selection,
  runtimeStatus,
  onDeleteSelection,
  onDuplicateSelection,
  onNodeTitleChange,
  onNodeConfigChange,
}: {
  events: string[];
  selection: Selection;
  runtimeStatus: RuntimeGraphStatus | null;
  onDeleteSelection: () => void;
  onDuplicateSelection: () => void;
  onNodeTitleChange: (nodeId: string, title: string) => void;
  onNodeConfigChange: (nodeId: string, key: string, value: string | boolean) => void;
}) {
  if (selection.type === 'none') {
    return (
      <div>
        <h2>Inspector</h2>
        <p className="muted">选择节点或连线后显示参数。</p>
        <RuntimeDiagnostics status={runtimeStatus} />
        <InspectorEvents events={events} />
      </div>
    );
  }
  if (selection.type === 'edge') {
    return (
      <div>
        <h2>Edge</h2>
        <div className="inspector-actions">
          <button type="button" onClick={onDeleteSelection}>Delete edge</button>
        </div>
        <KeyValue label="ID" value={selection.edge.id} />
        <KeyValue label="Source" value={`${selection.edge.source}:${selection.edge.sourceHandle ?? ''}`} />
        <KeyValue label="Target" value={`${selection.edge.target}:${selection.edge.targetHandle ?? ''}`} />
        <KeyValue label="Kind" value={labelForPortKind(selection.edge.data?.kind ?? 'endpoint.rtsp')} />
        <KeyValue label="Schema" value={selection.edge.data?.schema ?? 'n/a'} />
        <RuntimeDiagnostics status={runtimeStatus} />
        <InspectorEvents events={events} />
      </div>
    );
  }
  const node = selection.node;
  const nodeRuntime = runtimeStatus?.nodes.find((status) => status.nodeId === node.id);
  return (
    <div>
      <h2>{node.title}</h2>
      <div className="inspector-actions">
        <button type="button" onClick={onDuplicateSelection}>Duplicate</button>
        <button type="button" onClick={onDeleteSelection}>Delete</button>
      </div>
      <label className="field-label" htmlFor={`${node.id}-title`}>Title</label>
      <input
        id={`${node.id}-title`}
        className="inspector-input"
        defaultValue={node.title}
        onBlur={(event) => onNodeTitleChange(node.id, event.currentTarget.value)}
        onKeyDown={(event) => {
          if (event.key === 'Enter') {
            onNodeTitleChange(node.id, event.currentTarget.value);
            event.currentTarget.blur();
          }
        }}
      />
      <KeyValue label="Kind" value={node.kind} />
      <KeyValue label="Category" value={node.category} />
      <KeyValue label="State" value={node.state} />
      <KeyValue label="Runtime" value={nodeRuntime?.state ?? 'not started'} />
      {nodeRuntime && <KeyValue label="Runtime diagnostic" value={nodeRuntime.diagnostic} />}
      <h3>Ports</h3>
      {[...node.inputs, ...node.outputs].map((port) => (
        <KeyValue key={`${port.direction}-${port.id}`} label={`${port.direction}:${port.id}`} value={`${port.kind} / ${port.schema}`} />
      ))}
      <h3>Config</h3>
      <pre>{JSON.stringify(node.config, null, 2)}</pre>
      {isX5ConfigNode(node.kind) && (
        <X5ConfigPanel node={node} onNodeConfigChange={onNodeConfigChange} />
      )}
      {isRemoteConfigNode(node.kind) && (
        <RemoteConfigPanel node={node} onNodeConfigChange={onNodeConfigChange} />
      )}
      {(node.kind === 'i2cTransfer' || node.kind === 'eepromProvision') && (
        <ControlPreviewPanel node={node} onNodeConfigChange={onNodeConfigChange} />
      )}
      {node.kind === 'calibrationSolver' && (
        <CalibrationSolverPanel node={node} onNodeConfigChange={onNodeConfigChange} />
      )}
      <InspectorEvents events={events} />
      <RuntimeDiagnostics status={runtimeStatus} nodeId={node.id} />
    </div>
  );
}

function isX5ConfigNode(kind: NodeKind): boolean {
  return kind === 'x5Device' || kind === 'x5RtspChannel' || kind === 'x5Snapshot';
}

function X5ConfigPanel({
  node,
  onNodeConfigChange,
}: {
  node: WorkflowNode;
  onNodeConfigChange: (nodeId: string, key: string, value: string | boolean) => void;
}) {
  const [result, setResult] = useState<X5ControlResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const configValue = (key: string, fallback: string): string => configText(node, key, fallback);
  const binding = (): X5BindingRequest => ({
    host: configValue('host', '10.21.12.108'),
    tcpPort: parseControlInteger(configValue('tcpPort', '9073'), 'X5 TCP port'),
  });
  const channel = (): number => parseControlInteger(configValue('channel', '0'), 'X5 channel');
  const runX5 = async (operation: () => Promise<X5ControlResponse>) => {
    try {
      setLoading(true);
      setError(null);
      setResult(await operation());
    } catch (x5Error) {
      setResult(null);
      setError(x5Error instanceof Error ? x5Error.message : String(x5Error));
    } finally {
      setLoading(false);
    }
  };
  const snapshotRequest = () => {
    const mode = configValue('mode', 'latest') as X5SnapshotMode;
    return {
      ...binding(),
      channel: channel(),
      mode,
      frameId: mode === 'frame_id' ? parseControlInteger(configValue('frameId', '0'), 'Frame ID') : undefined,
      timestampNs: mode === 'timestamp_ns' ? parseControlInteger(configValue('timestampNs', '0'), 'Timestamp ns') : undefined,
      rtspPts90k: mode === 'rtsp_pts_90k' ? parseControlInteger(configValue('rtspPts90k', '0'), 'RTSP PTS 90k') : undefined,
      rtspPtsTolerance90k: mode === 'rtsp_pts_90k' ? parseControlInteger(configValue('rtspPtsTolerance90k', '0'), 'RTSP PTS tolerance 90k') : undefined,
    };
  };
  return (
    <section className="control-preview">
      <h3>X5 TCP runtime</h3>
      <p className="muted">X5 TCP 连接只在点击按钮时建立；节点仍只保存 host、端口、通道和抓帧参数。</p>
      <ControlConfigField id={`${node.id}-host`} label="Host" value={configValue('host', '10.21.12.108')} onChange={(value) => onNodeConfigChange(node.id, 'host', value.trim())} />
      <ControlConfigField id={`${node.id}-tcp-port`} label="TCP port" value={configValue('tcpPort', '9073')} onChange={(value) => onNodeConfigChange(node.id, 'tcpPort', value.trim())} />
      {node.kind === 'x5Device' && (
        <>
          <ControlConfigField id={`${node.id}-fps`} label="RTSP FPS" value={configValue('fps', '60')} onChange={(value) => onNodeConfigChange(node.id, 'fps', value.trim())} />
          <ControlConfigField id={`${node.id}-bitrate`} label="RTSP bitrate Kbps" value={configValue('bitrateKbps', '12000')} onChange={(value) => onNodeConfigChange(node.id, 'bitrateKbps', value.trim())} />
        </>
      )}
      {(node.kind === 'x5RtspChannel' || node.kind === 'x5Snapshot') && (
        <ControlConfigField id={`${node.id}-channel`} label="Channel" value={configValue('channel', '0')} onChange={(value) => onNodeConfigChange(node.id, 'channel', value.trim())} />
      )}
      {node.kind === 'x5RtspChannel' && (
        <ControlConfigField id={`${node.id}-path`} label="RTSP path" value={configValue('path', '/PRR')} onChange={(value) => onNodeConfigChange(node.id, 'path', value.trim() || '/PRR')} />
      )}
      {node.kind === 'x5Snapshot' && (
        <>
          <label className="field-label" htmlFor={`${node.id}-mode`}>
            Capture mode
            <select id={`${node.id}-mode`} className="inspector-input" value={configValue('mode', 'latest')} onChange={(event) => onNodeConfigChange(node.id, 'mode', event.currentTarget.value)}>
              <option value="latest">Latest</option>
              <option value="frame_id">Frame ID</option>
              <option value="timestamp_ns">Timestamp ns</option>
              <option value="rtsp_pts_90k">RTSP PTS 90k</option>
            </select>
          </label>
          <ControlConfigField id={`${node.id}-frame-id`} label="Frame ID" value={configValue('frameId', '0')} onChange={(value) => onNodeConfigChange(node.id, 'frameId', value.trim())} />
          <ControlConfigField id={`${node.id}-timestamp-ns`} label="Timestamp ns" value={configValue('timestampNs', '0')} onChange={(value) => onNodeConfigChange(node.id, 'timestampNs', value.trim())} />
          <ControlConfigField id={`${node.id}-rtsp-pts`} label="RTSP PTS 90k" value={configValue('rtspPts90k', '0')} onChange={(value) => onNodeConfigChange(node.id, 'rtspPts90k', value.trim())} />
          <ControlConfigField id={`${node.id}-rtsp-pts-tolerance`} label="RTSP PTS tolerance 90k" value={configValue('rtspPtsTolerance90k', '0')} onChange={(value) => onNodeConfigChange(node.id, 'rtspPtsTolerance90k', value.trim())} />
        </>
      )}
      <div className="inspector-actions">
        <button type="button" onClick={() => void runX5(() => statusX5Control(binding()))} disabled={loading}>{loading ? 'Working…' : 'Read status'}</button>
        {node.kind === 'x5Device' && <button type="button" onClick={() => void runX5(() => probeX5Control(binding()))} disabled={loading}>Probe</button>}
        {node.kind === 'x5Device' && <button type="button" onClick={() => void runX5(() => configureX5Rtsp({ ...binding(), fps: parseControlInteger(configValue('fps', '60'), 'RTSP FPS'), bitrateKbps: parseControlInteger(configValue('bitrateKbps', '12000'), 'RTSP bitrate Kbps') }))} disabled={loading}>Apply RTSP config</button>}
        {node.kind === 'x5RtspChannel' && <button type="button" onClick={() => void runX5(() => startX5RtspChannel({ ...binding(), channel: channel() }))} disabled={loading}>Start RTSP</button>}
        {node.kind === 'x5RtspChannel' && <button type="button" onClick={() => void runX5(() => stopX5RtspChannel({ ...binding(), channel: channel() }))} disabled={loading}>Stop RTSP</button>}
        {node.kind === 'x5Snapshot' && <button type="button" onClick={() => void runX5(() => captureX5Snapshot(snapshotRequest()))} disabled={loading}>Capture snapshot</button>}
      </div>
      {error && <p className="control-preview-error">X5 request failed: {error}</p>}
      {result && <pre className="control-preview-result">{JSON.stringify(result, null, 2)}</pre>}
    </section>
  );
}
function isRemoteConfigNode(kind: NodeKind): boolean {
  return kind === 'sshSession' || kind === 'sftpWorkspace' || kind === 'fileBrowser';
}

function RemoteConfigPanel({
  node,
  onNodeConfigChange,
}: {
  node: WorkflowNode;
  onNodeConfigChange: (nodeId: string, key: string, value: string | boolean) => void;
}) {
  return (
    <section className="control-preview">
      <h3>Remote binding</h3>
      <p className="muted">只保存 profile、host、source id 和路径引用；密码、会话句柄和目录快照只属于运行时。</p>
      {node.kind === 'sshSession' && (
        <>
          <ControlConfigField id={`${node.id}-profile-id`} label="Profile ID" value={configText(node, 'profileId', '')} onChange={(value) => onNodeConfigChange(node.id, 'profileId', value.trim())} />
          <ControlConfigField id={`${node.id}-host`} label="Host" value={configText(node, 'host', '')} onChange={(value) => onNodeConfigChange(node.id, 'host', value.trim())} />
          <ControlConfigField id={`${node.id}-port`} label="SSH port" value={configText(node, 'port', '22')} onChange={(value) => onNodeConfigChange(node.id, 'port', value.trim())} />
          <ControlConfigField id={`${node.id}-username`} label="Username" value={configText(node, 'username', 'root')} onChange={(value) => onNodeConfigChange(node.id, 'username', value.trim())} />
          <label className="control-checkbox">
            <input type="checkbox" checked={node.config.autoConnect === true} onChange={(event) => onNodeConfigChange(node.id, 'autoConnect', event.currentTarget.checked)} />
            Auto-connect when an explicit runtime run is started
          </label>
        </>
      )}
      {node.kind === 'sftpWorkspace' && (
        <>
          <ControlConfigField id={`${node.id}-source-id`} label="Source ID" value={configText(node, 'sourceId', 'sftp-main')} onChange={(value) => onNodeConfigChange(node.id, 'sourceId', value.trim())} />
          <ControlConfigField id={`${node.id}-remote-root`} label="Remote root" value={configText(node, 'remoteRoot', '/')} onChange={(value) => onNodeConfigChange(node.id, 'remoteRoot', value.trim() || '/')} />
          <ControlConfigField id={`${node.id}-mount-label`} label="Mount label" value={configText(node, 'mountLabel', 'Remote SFTP')} onChange={(value) => onNodeConfigChange(node.id, 'mountLabel', value.trim())} />
        </>
      )}
      {node.kind === 'fileBrowser' && (
        <>
          <ControlConfigField id={`${node.id}-directory`} label="Directory" value={configText(node, 'directory', '')} onChange={(value) => onNodeConfigChange(node.id, 'directory', normalizeSourcePathDraft(value))} />
          <ControlConfigField id={`${node.id}-selection`} label="Selected file" value={configText(node, 'selection', '')} onChange={(value) => onNodeConfigChange(node.id, 'selection', normalizeSourcePathDraft(value))} />
          <ControlConfigField id={`${node.id}-filter`} label="Filter" value={configText(node, 'filter', '*.png;*.jpg;*.jpeg')} onChange={(value) => onNodeConfigChange(node.id, 'filter', value.trim())} />
        </>
      )}
      <p className="control-confirmation">Connect/list/read is not automatic. Future SFTP execution must bind this node to an explicit SSH runtime session.</p>
    </section>
  );
}

function CalibrationSolverPanel({
  node,
  onNodeConfigChange,
}: {
  node: WorkflowNode;
  onNodeConfigChange: (nodeId: string, key: string, value: string | boolean) => void;
}) {
  const [imagePointsDraft, setImagePointsDraft] = useState('');
  const [solution, setSolution] = useState<CalibrationSolution | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    setImagePointsDraft('');
    setSolution(null);
    setError(null);
  }, [node.id]);

  const configValue = (key: string, fallback: string): string => configText(node, key, fallback);
  const updateConfig = (key: string, value: string) => onNodeConfigChange(node.id, key, value.trim());
  const buildRequest = (): CalibrationRequest => {
    const imageWidth = parseControlInteger(configValue('imageWidth', '1920'), 'Image width');
    const imageHeight = parseControlInteger(configValue('imageHeight', '1080'), 'Image height');
    const boardCols = parseControlInteger(configValue('boardCols', '8'), 'Board columns');
    const boardRows = parseControlInteger(configValue('boardRows', '11'), 'Board rows');
    const squareSizeMm = parseControlFloat(configValue('squareSizeMm', '30'), 'Square size mm');
    const fx = parseControlFloat(configValue('fx', '900'), 'fx');
    const fy = parseControlFloat(configValue('fy', '900'), 'fy');
    const cx = parseControlFloat(configValue('cx', String(imageWidth / 2)), 'cx');
    const cy = parseControlFloat(configValue('cy', String(imageHeight / 2)), 'cy');
    return {
      imageSize: { width: imageWidth, height: imageHeight },
      board: { innerCols: boardCols, innerRows: boardRows, squareSize: squareSizeMm },
      imagePoints: parseCalibrationImagePoints(imagePointsDraft),
      initialIntrinsics: {
        cameraMatrix: [fx, 0, cx, 0, fy, cy, 0, 0, 1],
        distortionCoefficients: calibrationDistortionCoefficients(node),
      },
    };
  };
  const runSolver = async () => {
    try {
      setLoading(true);
      setError(null);
      setSolution(await runCalibrationSolver(buildRequest()));
    } catch (solverError) {
      setSolution(null);
      setError(solverError instanceof Error ? solverError.message : String(solverError));
    } finally {
      setLoading(false);
    }
  };
  return (
    <section className="control-preview">
      <h3>Calibration solver runtime</h3>
      <p className="muted">Solver 只通过 HTTP/JSON 手动触发；imagePoints dataset 和 solution 只保留在当前 Inspector 运行态，不写入 WorkflowGraph。</p>
      <ControlConfigField id={`${node.id}-board-cols`} label="Board inner cols" value={configValue('boardCols', '8')} onChange={(value) => updateConfig('boardCols', value)} />
      <ControlConfigField id={`${node.id}-board-rows`} label="Board inner rows" value={configValue('boardRows', '11')} onChange={(value) => updateConfig('boardRows', value)} />
      <ControlConfigField id={`${node.id}-square-size`} label="Square size mm" value={configValue('squareSizeMm', '30')} onChange={(value) => updateConfig('squareSizeMm', value)} />
      <ControlConfigField id={`${node.id}-image-width`} label="Image width" value={configValue('imageWidth', '1920')} onChange={(value) => updateConfig('imageWidth', value)} />
      <ControlConfigField id={`${node.id}-image-height`} label="Image height" value={configValue('imageHeight', '1080')} onChange={(value) => updateConfig('imageHeight', value)} />
      <ControlConfigField id={`${node.id}-fx`} label="fx" value={configValue('fx', '900')} onChange={(value) => updateConfig('fx', value)} />
      <ControlConfigField id={`${node.id}-fy`} label="fy" value={configValue('fy', '900')} onChange={(value) => updateConfig('fy', value)} />
      <ControlConfigField id={`${node.id}-cx`} label="cx" value={configValue('cx', '960')} onChange={(value) => updateConfig('cx', value)} />
      <ControlConfigField id={`${node.id}-cy`} label="cy" value={configValue('cy', '540')} onChange={(value) => updateConfig('cy', value)} />
      <ControlConfigField id={`${node.id}-distortion`} label="D coefficients" value={calibrationDistortionText(node)} onChange={(value) => updateConfig('distortionCoefficients', value)} />
      <label className="field-label" htmlFor={`${node.id}-image-points`}>
        imagePoints JSON
        <textarea
          id={`${node.id}-image-points`}
          className="inspector-input control-textarea"
          value={imagePointsDraft}
          placeholder={'[[{"x":120.5,"y":80.0}], [{"x":118.0,"y":82.0}]]'}
          onChange={(event) => setImagePointsDraft(event.currentTarget.value)}
        />
      </label>
      <div className="inspector-actions">
        <button type="button" onClick={() => void runSolver()} disabled={loading}>{loading ? 'Solving…' : 'Run solver'}</button>
      </div>
      {error && <p className="control-preview-error">Calibration solver rejected: {error}</p>}
      {solution && (
        <div className="control-preview-result">
          <KeyValue label="RMS" value={solution.rmsError.toFixed(4)} />
          <KeyValue label="Views" value={String(solution.views.length)} />
          <KeyValue label="Image size" value={`${solution.imageSize.width}x${solution.imageSize.height}`} />
          <KeyValue label="Flags" value={String(solution.calibrationFlags)} />
          <pre>{JSON.stringify(solution, null, 2)}</pre>
        </div>
      )}
    </section>
  );
}

function RuntimeDiagnostics({ status, nodeId }: { status: RuntimeGraphStatus | null; nodeId?: string }) {
  if (!status) {
    return (
      <section className="inspector-events">
        <h3>Runtime</h3>
        <p className="muted">尚未启动 RuntimeGraph。</p>
      </section>
    );
  }
  const events = nodeId
    ? status.events.filter((event) => event.nodeId === nodeId)
    : status.events;
  return (
    <section className="inspector-events">
      <h3>Runtime</h3>
      <KeyValue label="Session" value={status.running ? 'running' : 'stopped'} />
      <ol>
        {events.map((event) => <li key={`${event.nodeId}-${event.message}`}><strong>{event.level}</strong> · {event.nodeId}: {event.message}</li>)}
      </ol>
    </section>
  );
}

function ControlPreviewPanel({
  node,
  onNodeConfigChange,
}: {
  node: WorkflowNode;
  onNodeConfigChange: (nodeId: string, key: string, value: string | boolean) => void;
}) {
  const [preview, setPreview] = useState<ControlRequestPreview | null>(null);
  const [execution, setExecution] = useState<ControlExecutionResult | null>(null);
  const [inspectResult, setInspectResult] = useState<EepromInspectResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [sshHost, setSshHost] = useState('');
  const [sshPort, setSshPort] = useState('22');
  const [sshUsername, setSshUsername] = useState('root');
  const [credentialRef, setCredentialRef] = useState('');
  const [expectedBeforeSha256, setExpectedBeforeSha256] = useState('');
  const isEeprom = node.kind === 'eepromProvision';
  const configText = (key: string, fallback: string): string => {
    const value = node.config[key];
    return typeof value === 'string' || typeof value === 'number' ? String(value) : fallback;
  };

  useEffect(() => {
    setPreview(null);
    setExecution(null);
    setError(null);
    setSshHost('');
    setSshPort('22');
    setSshUsername('root');
    setCredentialRef('');
    setExpectedBeforeSha256('');
  }, [node.id]);

  const sshBinding = (): SshExecutionBinding => ({
    host: sshHost,
    port: parseControlInteger(sshPort, 'SSH port'),
    username: sshUsername,
    credentialRef,
  });

  const controlCommon = () => {
    const address = parseControlInteger(configText('address', '0x50'), 'Address');
    const register = parseControlInteger(configText('register', '0x0000'), 'Register');
    const pageSize = parseControlInteger(configText('pageSize', '16'), 'Page size');
    const payload = parseHexPayload(configText('payload', ''));
    return {
      nodeId: node.id,
      profileId: configText('profileId', ''),
      bus: configText('bus', ''),
      address,
      register,
      payload,
      pageSize,
    };
  };
  const requestPreview = async () => {
    try {
      setLoading(true);
      setError(null);
      const common = controlCommon();
      const result = isEeprom
        ? await previewEepromProvision({
          ...common,
          mapId: configText('mapId', ''),
          verifyAfterWrite: node.config.verifyAfterWrite === true,
        })
        : await previewI2cTransfer({
          ...common,
          operation: configText('mode', 'read') === 'write' ? 'write' : 'read',
        });
      setPreview(result);
    } catch (previewError) {
      setPreview(null);
      setError(previewError instanceof Error ? previewError.message : String(previewError));
    } finally {
      setLoading(false);
    }
  };

  const executeRequest = async () => {
    try {
      setLoading(true);
      setError(null);
      setExecution(null);
      const common = controlCommon();
      const result = isEeprom
        ? await runEepromProvision({
          ...common,
          mapId: configText('mapId', ''),
          verifyAfterWrite: node.config.verifyAfterWrite === true,
          confirmExecution: true,
          expectedBeforeSha256,
          ssh: sshBinding(),
        })
        : await runI2cTransfer({
          ...common,
          operation: configText('mode', 'read') === 'write' ? 'write' : 'read',
          confirmExecution: true,
          ssh: sshBinding(),
        });
      setPreview(result.preview);
      setExecution(result);
    } catch (executionError) {
      setError(executionError instanceof Error ? executionError.message : String(executionError));
    } finally {
      setLoading(false);
    }
  };

  const inspectEeprom = async () => {
    try {
      setLoading(true);
      setError(null);
      setExecution(null);
      const result = await inspectEepromProvision({
        ...controlCommon(),
        mapId: configText('mapId', ''),
        verifyAfterWrite: node.config.verifyAfterWrite === true,
        ssh: sshBinding(),
      });
      setPreview(result.preview);
      setInspectResult(result);
      setExpectedBeforeSha256(result.snapshot.imageSha256);
    } catch (inspectError) {
      setInspectResult(null);
      setError(inspectError instanceof Error ? inspectError.message : String(inspectError));
    } finally {
      setLoading(false);
    }
  };

  return (
    <section className="control-preview">
      <h3>安全请求预览</h3>
      <p className="muted">配置只会保存为节点轻量参数。点击预览只校验请求，绝不连接 SSH 或 I²C。</p>
      <ControlConfigField id={`${node.id}-profile`} label="Session profile" value={configText('profileId', '')} onChange={(value) => onNodeConfigChange(node.id, 'profileId', value)} />
      <ControlConfigField id={`${node.id}-bus`} label="I²C bus" value={configText('bus', 'i2c-1')} onChange={(value) => onNodeConfigChange(node.id, 'bus', value)} />
      <ControlConfigField id={`${node.id}-address`} label="Address (hex)" value={configText('address', '0x50')} onChange={(value) => onNodeConfigChange(node.id, 'address', value)} />
      <ControlConfigField id={`${node.id}-register`} label="Register (hex)" value={configText('register', '0x0000')} onChange={(value) => onNodeConfigChange(node.id, 'register', value)} />
      <ControlConfigField id={`${node.id}-payload`} label="Payload (hex bytes)" value={configText('payload', '')} onChange={(value) => onNodeConfigChange(node.id, 'payload', value)} />
      <ControlConfigField id={`${node.id}-page-size`} label="EEPROM page size" value={configText('pageSize', '32')} onChange={(value) => onNodeConfigChange(node.id, 'pageSize', value)} />
      {isEeprom ? (
        <>
          <ControlConfigField id={`${node.id}-map`} label="EEPROM map" value={configText('mapId', '')} onChange={(value) => onNodeConfigChange(node.id, 'mapId', value)} />
          <ControlConfigField id={`${node.id}-ssh-host`} label="SSH host" value={sshHost} onChange={(value) => setSshHost(value.trim())} />
          <ControlConfigField id={`${node.id}-ssh-port`} label="SSH port" value={sshPort} onChange={(value) => setSshPort(value.trim())} />
          <ControlConfigField id={`${node.id}-ssh-user`} label="SSH username" value={sshUsername} onChange={(value) => setSshUsername(value.trim())} />
          <ControlConfigField id={`${node.id}-credential-ref`} label="Credential ref" value={credentialRef} onChange={(value) => setCredentialRef(value.trim())} />
          <ControlConfigField id={`${node.id}-expected-before`} label="Expected before SHA-256" value={expectedBeforeSha256} onChange={(value) => setExpectedBeforeSha256(value.trim())} />
          <label className="control-checkbox">
            <input type="checkbox" checked={node.config.verifyAfterWrite === true} onChange={(event) => onNodeConfigChange(node.id, 'verifyAfterWrite', event.currentTarget.checked)} />
            Verify after write (required)
          </label>
        </>
      ) : (
        <>
          <label className="field-label" htmlFor={`${node.id}-mode`}>
            Operation
            <select id={`${node.id}-mode`} className="inspector-input" value={configText('mode', 'read')} onChange={(event) => onNodeConfigChange(node.id, 'mode', event.currentTarget.value)}>
              <option value="read">Read</option>
              <option value="write">Write</option>
            </select>
          </label>
          <ControlConfigField id={`${node.id}-ssh-host`} label="SSH host" value={sshHost} onChange={(value) => setSshHost(value.trim())} />
          <ControlConfigField id={`${node.id}-ssh-port`} label="SSH port" value={sshPort} onChange={(value) => setSshPort(value.trim())} />
          <ControlConfigField id={`${node.id}-ssh-user`} label="SSH username" value={sshUsername} onChange={(value) => setSshUsername(value.trim())} />
          <ControlConfigField id={`${node.id}-credential-ref`} label="Credential ref" value={credentialRef} onChange={(value) => setCredentialRef(value.trim())} />
        </>
      )}
      <div className="inspector-actions">
        <button type="button" onClick={() => void requestPreview()} disabled={loading}>{loading ? 'Working…' : 'Preview request'}</button>
        {isEeprom && <button type="button" onClick={() => void inspectEeprom()} disabled={loading}>Inspect EEPROM</button>}
        <button type="button" onClick={() => void executeRequest()} disabled={loading}>{isEeprom ? 'Write EEPROM' : 'Execute I²C'}</button>
      </div>
      {error && <p className="control-preview-error">Preview rejected: {error}</p>}
      {preview && (
        <div className="control-preview-result">
          <KeyValue label="Mode" value={preview.operation} />
          <KeyValue label="Execution" value={preview.execution} />
          <KeyValue label="Node" value={preview.target.nodeId} />
          <KeyValue label="Profile" value={preview.target.profileId} />
          <KeyValue label="Bus" value={preview.target.bus} />
          <KeyValue label="Address" value={`0x${preview.target.address.toString(16).padStart(2, '0')}`} />
          <KeyValue label="Register" value={`0x${preview.target.register.toString(16).padStart(4, '0')}`} />
          <KeyValue label="Payload" value={preview.target.payload.map((byte) => byte.toString(16).padStart(2, '0')).join(' ') || '(empty)'} />
          {preview.mapId && <KeyValue label="EEPROM map" value={preview.mapId} />}
          {preview.verifyAfterWrite !== null && <KeyValue label="Verify after write" value={preview.verifyAfterWrite ? 'yes' : 'no'} />}
          <KeyValue label="Page split" value={`${preview.pageSplitEstimate.writeCount} write(s), ${preview.pageSplitEstimate.pageSize} B/page`} />
          {preview.pageSplitEstimate.segments.length > 0 && <pre>{JSON.stringify(preview.pageSplitEstimate.segments, null, 2)}</pre>}
          {preview.requiresConfirmation && <p className="control-confirmation">Write-like operation: explicit confirmation is required before any future execution path.</p>}
        </div>
      )}
      {inspectResult && (
        <div className="control-preview-result">
          <KeyValue label="Inspect snapshot" value={inspectResult.snapshot.imageSha256} />
          <KeyValue label="Inspect target" value={`${inspectResult.snapshot.target.username}@${inspectResult.snapshot.target.host}:${inspectResult.snapshot.target.port} / ${inspectResult.snapshot.target.bus}`} />
          <pre>{JSON.stringify(inspectResult.result, null, 2)}</pre>
        </div>
      )}
      {execution && (
        <div className="control-preview-result">
          <KeyValue label="Execution result" value={execution.execution} />
          <pre>{JSON.stringify(execution.result, null, 2)}</pre>
        </div>
      )}
    </section>
  );
}

function ControlConfigField({
  id,
  label,
  value,
  onChange,
}: {
  id: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
}) {
  return (
    <label className="field-label" htmlFor={id}>
      {label}
      <input id={id} className="inspector-input" value={value} onChange={(event) => onChange(event.currentTarget.value)} />
    </label>
  );
}

/** 接受十进制或 0x 前缀十六进制，保留服务端的范围校验。 */
function parseControlInteger(value: string, label: string): number {
  const text = value.trim();
  if (!/^(?:0x[0-9a-f]+|\d+)$/i.test(text)) {
    throw new Error(`${label} must be a decimal or 0x-prefixed hexadecimal integer`);
  }
  const parsed = Number(text);
  if (!Number.isSafeInteger(parsed) || parsed < 0) {
    throw new Error(`${label} must be a non-negative integer`);
  }
  return parsed;
}

function parseControlFloat(value: string, label: string): number {
  const parsed = Number(value.trim());
  if (!Number.isFinite(parsed)) {
    throw new Error(`${label} must be a finite number`);
  }
  return parsed;
}

function calibrationDistortionText(node: WorkflowNode): string {
  const value = node.config.distortionCoefficients;
  if (Array.isArray(value) && value.every((entry) => typeof entry === 'number')) {
    return value.join(', ');
  }
  return configText(node, 'distortionCoefficients', '0,0,0,0,0,0,0,0,0,0,0,0');
}

function calibrationDistortionCoefficients(node: WorkflowNode): number[] {
  return parseNumberList(calibrationDistortionText(node), 'D coefficients');
}

function parseNumberList(value: string, label: string): number[] {
  const text = value.trim();
  if (!text) {
    throw new Error(`${label} must not be empty`);
  }
  return text.split(/[\s,]+/).map((token) => parseControlFloat(token, label));
}

function parseCalibrationImagePoints(value: string): CalibrationRequest['imagePoints'] {
  const parsed: unknown = JSON.parse(value);
  if (!Array.isArray(parsed)) {
    throw new Error('imagePoints must be an array of views');
  }
  return parsed.map((view, viewIndex) => {
    if (!Array.isArray(view)) {
      throw new Error(`imagePoints[${viewIndex}] must be an array of points`);
    }
    return view.map((point, pointIndex) => {
      if (!isCalibrationPoint(point)) {
        throw new Error(`imagePoints[${viewIndex}][${pointIndex}] must contain finite x/y numbers`);
      }
      return point;
    });
  });
}

function isCalibrationPoint(value: unknown): value is { x: number; y: number } {
  if (typeof value !== 'object' || value === null) {
    return false;
  }
  const record = value as Record<string, unknown>;
  return typeof record.x === 'number'
    && Number.isFinite(record.x)
    && typeof record.y === 'number'
    && Number.isFinite(record.y);
}


/** 将空格或逗号分隔的字节文本转换为 JSON 数组，避免把原始文本混入请求。 */
function parseHexPayload(value: string): number[] {
  const text = value.trim();
  if (!text) {
    return [];
  }
  return text.split(/[\s,]+/).map((token) => {
    if (!/^(?:0x)?[0-9a-f]{1,2}$/i.test(token)) {
      throw new Error(`Invalid payload byte: ${token}`);
    }
    return Number.parseInt(token.replace(/^0x/i, ''), 16);
  });
}


function InspectorEvents({ events }: { events: string[] }) {
  return (
    <section className="inspector-events">
      <h3>Events</h3>
      <ol>
        {events.map((event, index) => <li key={`${event}-${index}`}>{event}</li>)}
      </ol>
    </section>
  );
}

function KeyValue({ label, value }: { label: string; value: string }) {
  return (
    <div className="key-value">
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  );
}
