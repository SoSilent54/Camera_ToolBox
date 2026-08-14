export const WORKFLOW_SCHEMA_VERSION = 'workflow.v1';

export type NodeKind =
  | 'localWorkspace'
  | 'sftpWorkspace'
  | 'fileBrowser'
  | 'imageFileSource'
  | 'rtspSource'
  | 'sshSession'
  | 'x5Device'
  | 'x5RtspChannel'
  | 'x5Snapshot'
  | 'rtspDecoder'
  | 'frameSampler'
  | 'imageLayer'
  | 'videoLayer'
  | 'overlayComposer'
  | 'viewer'
  | 'chessboardDetector'
  | 'datasetCollector'
  | 'coverageAnalyzer'
  | 'captureScorer'
  | 'autoCaptureController'
  | 'poseGuide'
  | 'calibrationSolver'
  | 'reprojectionInspector'
  | 'calibrationExport'
  | 'i2cBusDiscovery'
  | 'i2cTransfer'
  | 'eepromMapLoader'
  | 'eepromProvision'
  | 'resultView';

export type NodeCategory = 'workspace' | 'source' | 'media' | 'viewer' | 'calibration' | 'control' | 'diagnostics';
export type NodeRuntimeState = 'idle' | 'ready' | 'running' | 'warning' | 'error';
export type PortDirection = 'input' | 'output';
export type PortCardinality = 'one' | 'many';
export type PortRole = 'workspace' | 'endpoint' | 'stream' | 'image' | 'layer' | 'overlay' | 'control' | 'status' | 'dataset' | 'solution' | 'command';

export type PortKind =
  | 'workspace.local'
  | 'workspace.remote.sftp'
  | 'file.ref'
  | 'control.ssh'
  | 'control.x5tcp'
  | 'endpoint.rtsp'
  | 'stream.encoded-video'
  | 'stream.video-frame'
  | 'image.frame'
  | 'layer.image'
  | 'layer.video'
  | 'layer.overlay'
  | 'viewer.scene'
  | 'calib.detection'
  | 'calib.coverage'
  | 'calib.dataset'
  | 'calib.solution'
  | 'calib.report'
  | 'capture.score'
  | 'capture.target'
  | 'command.capture'
  | 'i2c.bus'
  | 'i2c.transfer'
  | 'i2c.result'
  | 'eeprom.map'
  | 'eeprom.payload'
  | 'status.metrics';

export interface WorkflowGraph {
  schemaVersion: string;
  id: string;
  title: string;
  revision: string;
  nodes: WorkflowNode[];
  edges: WorkflowEdge[];
  viewport?: WorkflowViewport;
}

export interface WorkflowViewport {
  x: number;
  y: number;
  zoom: number;
}

export interface WorkflowNode {
  id: string;
  kind: NodeKind;
  title: string;
  position: NodePosition;
  state: NodeRuntimeState;
  category: NodeCategory;
  inputs: WorkflowPort[];
  outputs: WorkflowPort[];
  config: Record<string, unknown>;
}

export interface NodePosition {
  x: number;
  y: number;
}

export interface WorkflowPort {
  id: string;
  label: string;
  direction: PortDirection;
  kind: PortKind;
  schema: string;
  role?: PortRole;
  required: boolean;
  cardinality: PortCardinality;
}

export interface WorkflowEdge {
  id: string;
  source: PortEndpoint;
  target: PortEndpoint;
  kind: PortKind;
  schema: string;
  schemaVersion: string;
}

export interface PortEndpoint {
  nodeId: string;
  portId: string;
}

export interface NodeDefinition {
  kind: NodeKind;
  category: NodeCategory;
  title: string;
  description: string;
  inputs: WorkflowPort[];
  outputs: WorkflowPort[];
  defaultConfig: Record<string, unknown>;
}

export interface WorkmodeTemplate {
  id: string;
  title: string;
  description: string;
  graph: WorkflowGraph;
}

export interface WorkflowSummary {
  id: string;
  title: string;
  revision: string;
  nodeCount: number;
  edgeCount: number;
}

export type I2cPreviewOperation = 'read' | 'write';

export interface I2cPreviewRequest {
  nodeId: string;
  profileId: string;
  bus: string;
  address: number;
  register: number;
  payload: number[];
  pageSize: number;
  operation: I2cPreviewOperation;
}

export interface EepromPreviewRequest {
  nodeId: string;
  profileId: string;
  bus: string;
  address: number;
  register: number;
  payload: number[];
  pageSize: number;
  mapId: string;
  verifyAfterWrite: boolean;
}

export interface SshExecutionBinding {
  host: string;
  port?: number;
  username?: string;
  credentialRef: string;
}

export interface I2cExecuteRequest extends I2cPreviewRequest {
  confirmExecution: boolean;
  ssh: SshExecutionBinding;
}

export interface EepromInspectRequest extends EepromPreviewRequest {
  ssh: SshExecutionBinding;
}

export interface EepromExecuteRequest extends EepromPreviewRequest {
  confirmExecution: boolean;
  expectedBeforeSha256?: string;
  ssh: SshExecutionBinding;
}
export interface CalibrationImageSize {
  width: number;
  height: number;
}

export interface BoardSpec {
  innerCols: number;
  innerRows: number;
  squareSize: number;
}

export interface CalibrationPoint {
  x: number;
  y: number;
}

export interface InitialIntrinsics {
  cameraMatrix: number[];
  distortionCoefficients: number[];
}

export interface CalibrationRequest {
  imageSize: CalibrationImageSize;
  board: BoardSpec;
  imagePoints: CalibrationPoint[][];
  initialIntrinsics: InitialIntrinsics;
}

export interface ViewCalibrationResult {
  rotationVector: number[];
  translationVector: number[];
  projectedPoints: CalibrationPoint[];
  reprojectionRmse: number;
  maxReprojectionError: number;
}

export interface CalibrationSolution {
  imageSize: CalibrationImageSize;
  cameraMatrix: number[];
  distortionCoefficients: number[];
  rmsError: number;
  calibrationFlags: number;
  views: ViewCalibrationResult[];
}

export interface X5BindingRequest {
  host: string;
  tcpPort?: number;
}

export interface X5ConfigureRequest extends X5BindingRequest {
  fps: number;
  bitrateKbps: number;
}

export interface X5ChannelRequest extends X5BindingRequest {
  channel: number;
}

export type X5SnapshotMode = 'latest' | 'frame_id' | 'timestamp_ns' | 'rtsp_pts_90k';

export interface X5SnapshotRequest extends X5BindingRequest {
  channel: number;
  mode: X5SnapshotMode;
  frameId?: number;
  timestampNs?: number;
  rtspPts90k?: number;
  rtspPtsTolerance90k?: number;
}

export type X5ControlResponse = Record<string, unknown>;

export interface EepromInspectResponse {
  preview: ControlRequestPreview;
  snapshot: {
    key: string;
    imageSha256: string;
    target: {
      nodeId: string;
      host: string;
      port: number;
      username: string;
      mapId: string;
      bus: string;
      address: number;
    };
  };
  result: unknown;
}

export interface ControlRequestPreview {
  target: {
    nodeId: string;
    profileId: string;
    bus: string;
    address: number;
    register: number;
    payload: number[];
  };
  operation: 'read' | 'write' | 'provision';
  pageSplitEstimate: {
    pageSize: number;
    writeCount: number;
    segments: Array<{ register: number; payloadLength: number }>;
  };
  requiresConfirmation: boolean;
  execution: 'preview-only';
  mapId: string | null;
  verifyAfterWrite: boolean | null;
}

export interface ControlExecutionResult {
  preview: ControlRequestPreview;
  execution: 'completed' | 'blocked';
  result: unknown;
}

export type ViewerPreview =
  | { kind: 'rtsp'; url: string }
  | { kind: 'local-image'; url: string };

export interface FlowNodeData extends Record<string, unknown> {
  workflowNode: WorkflowNode;
  preview?: ViewerPreview;
  /** 引擎实时状态；缺省回退到节点的持久化 state。 */
  runtimeState?: 'disabled' | 'idle' | 'ready' | 'running' | 'error';
  onRtspUrlChange?: (nodeId: string, url: string) => void;
  onLocalImageConfigChange?: (nodeId: string, field: 'root' | 'relativePath', value: string) => void;
  onNodeConfigChange?: (nodeId: string, key: string, value: string | boolean) => void;
  /** 触发节点动作（connect/disconnect/trigger/arm/disarm）。 */
  onNodeAction?: (nodeId: string, action: string) => void;
}

export interface FlowEdgeData extends Record<string, unknown> {
  workflowEdge?: WorkflowEdge;
  kind: PortKind;
  schema: string;
}

export async function loadWorkflow(): Promise<WorkflowGraph> {
  return fetchJson('/api/workflow');
}

export async function loadNodeCatalog(): Promise<NodeDefinition[]> {
  return fetchJson('/api/node-catalog');
}

export async function loadWorkmodeTemplates(): Promise<WorkmodeTemplate[]> {
  return fetchJson('/api/workmode-templates');
}

export async function listWorkflows(): Promise<WorkflowSummary[]> {
  return fetchJson('/api/workflows');
}

export async function loadSavedWorkflow(id: string): Promise<WorkflowGraph> {
  return fetchJson(`/api/workflows/${encodeURIComponent(id)}`);
}

export async function saveWorkflow(graph: WorkflowGraph): Promise<WorkflowGraph> {
  const response = await fetch(`/api/workflows/${encodeURIComponent(graph.id)}`, {
    method: 'PUT',
    headers: {
      'content-type': 'application/json',
      'if-match': graph.revision,
    },
    body: JSON.stringify(graph),
  });
  if (!response.ok) {
    throw new Error(await response.text());
  }
  return (await response.json()) as WorkflowGraph;
}

export async function importWorkflow(graph: WorkflowGraph): Promise<WorkflowGraph> {
  const response = await fetch('/api/workflows/import', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(graph),
  });
  if (!response.ok) {
    const error = await response.json().catch(() => null) as { error?: unknown } | null;
    throw new Error(typeof error?.error === 'string' ? error.error : `request failed: ${response.status} ${response.statusText}`);
  }
  return (await response.json()) as WorkflowGraph;
}

export async function exportWorkflow(id: string): Promise<WorkflowGraph> {
  return fetchJson(`/api/workflows/${encodeURIComponent(id)}/export`);
}

export async function deleteWorkflow(id: string): Promise<void> {
  const response = await fetch(`/api/workflows/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
  if (!response.ok && response.status !== 204) {
    throw new Error(await response.text());
  }
}

export async function validateWorkflow(graph: WorkflowGraph): Promise<void> {
  const response = await fetch(`/api/workflows/${encodeURIComponent(graph.id)}/validate`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(graph),
  });
  if (!response.ok) {
    throw new Error(await response.text());
  }
}


/** 执行一次显式 I²C 请求；写操作必须由调用者传入确认与 SSH 运行时绑定。 */
export async function runI2cTransfer(request: I2cExecuteRequest): Promise<ControlExecutionResult> {
  return postJson('/api/control/i2c/run', request);
}

/** EEPROM Inspect 会建立显式 SSH helper 会话，并把最新读回 hash 作为进程内写入门禁。 */
export async function inspectEepromProvision(request: EepromInspectRequest): Promise<EepromInspectResponse> {
  return postJson('/api/control/eeprom/inspect', request);
}

/** EEPROM 写入必须复用同一进程内 Inspect 快照，并由 helper 执行字节级回读校验。 */
export async function runEepromProvision(request: EepromExecuteRequest): Promise<ControlExecutionResult> {
  return postJson('/api/control/eeprom/run', request);
}

/** 手动触发一次标定求解；请求体只包含原始 CalibrationRequest，不持久化到 WorkflowGraph。 */
export async function runCalibrationSolver(request: CalibrationRequest): Promise<CalibrationSolution> {
  return postJson('/api/control/calibration/solver/run', request);
}

/** X5 TCP 控制面只在显式按钮触发时连接设备；host/port 来自节点轻量配置。 */
export async function probeX5Control(request: X5BindingRequest): Promise<X5ControlResponse> {
  return postJson('/api/control/x5/probe', request);
}

export async function statusX5Control(request: X5BindingRequest): Promise<X5ControlResponse> {
  return postJson('/api/control/x5/status', request);
}

export async function configureX5Rtsp(request: X5ConfigureRequest): Promise<X5ControlResponse> {
  return postJson('/api/control/x5/configure-rtsp', request);
}

export async function startX5RtspChannel(request: X5ChannelRequest): Promise<X5ControlResponse> {
  return postJson('/api/control/x5/start-rtsp', request);
}

export async function stopX5RtspChannel(request: X5ChannelRequest): Promise<X5ControlResponse> {
  return postJson('/api/control/x5/stop-rtsp', request);
}

export async function captureX5Snapshot(request: X5SnapshotRequest): Promise<X5ControlResponse> {
  return postJson('/api/control/x5/snapshot', request);
}


/** 引擎节点状态快照。 */
export interface EngineNodeStatus {
  nodeId: string;
  state: 'disabled' | 'idle' | 'ready' | 'running' | 'error';
  diagnostic: string;
}

/** 装载工作流图进数据流引擎（替换旧图）。 */
export async function runEngine(graph: WorkflowGraph): Promise<{ running: boolean; nodes: number }> {
  return postJson('/api/runtime/run', graph);
}

/** 停止并卸载引擎图。 */
export async function stopEngine(): Promise<{ running: boolean }> {
  return postJson('/api/runtime/stop');
}

/** 向引擎节点投递动作（connect/disconnect/trigger/arm/disarm）。 */
export async function nodeAction(nodeId: string, action: string): Promise<{ ok: boolean }> {
  return postJson(`/api/runtime/nodes/${encodeURIComponent(nodeId)}/action`, { action });
}

/** 非阻塞取回引擎节点状态更新。 */
export async function fetchEngineStatus(): Promise<EngineNodeStatus[]> {
  return fetchJson('/api/runtime/status');
}

/** viewer 节点最新帧的 JPEG 地址（可直接作为 <img> src）。 */
export function viewerFrameUrl(nodeId: string): string {
  return `/api/runtime/viewer/${encodeURIComponent(nodeId)}/frame`;
}

/** 目录条目。 */
export interface DirectoryEntry {
  name: string;
  path: string;
  isDirectory: boolean;
  size: number;
}

/** 目录列表响应。 */
export interface FileListResponse {
  path: string;
  entries: DirectoryEntry[];
}

/** 列出本地工作区目录。 */
export async function listLocalFiles(root: string, path: string): Promise<FileListResponse> {
  const query = new URLSearchParams({ root, path });
  return fetchJson(`/api/files/local/list?${query}`);
}

/** 请求服务器校验 I²C 配置并返回预览；该端点不执行任何 I/O。 */
export async function previewI2cTransfer(request: I2cPreviewRequest): Promise<ControlRequestPreview> {
  return postJson('/api/control/i2c/preview', request);
}

/** 请求服务器校验 EEPROM 配置并返回预览；该端点不执行任何 I/O。 */
export async function previewEepromProvision(request: EepromPreviewRequest): Promise<ControlRequestPreview> {
  return postJson('/api/control/eeprom/preview', request);
}

export function labelForPortKind(kind: PortKind): string {
  return kind;
}

export function validateConnectionKinds(source: WorkflowPort, target: WorkflowPort): string | null {
  if (source.kind !== target.kind) {
    return `${labelForPortKind(source.kind)} 不能连接到 ${labelForPortKind(target.kind)}`;
  }
  if (source.schema !== target.schema) {
    return `${source.schema} 不能连接到 ${target.schema}`;
  }
  return null;
}

async function fetchJson<T>(url: string): Promise<T> {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`request failed: ${response.status} ${response.statusText}`);
  }
  return (await response.json()) as T;
}

async function postJson<T>(url: string, body?: unknown): Promise<T> {
  const response = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    const error = await response.json().catch(() => null) as { error?: unknown } | null;
    throw new Error(typeof error?.error === 'string' ? error.error : `request failed: ${response.status} ${response.statusText}`);
  }
  return (await response.json()) as T;
}

