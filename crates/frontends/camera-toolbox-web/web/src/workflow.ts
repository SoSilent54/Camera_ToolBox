import { wsRequest } from './useEngineSocket';

export const WORKFLOW_SCHEMA_VERSION = 'workflow.v1';

export type NodeKind =
  | 'localFileSource'
  | 'sftpFileSource'
  | 'rtspSource'
  | 'sshSession'
  | 'x5233Driver'
  | 'hexArmDevice'
  | 'rtspDecoder'
  | 'demosaic'
  | 'frameSampler'
  | 'imageLayer'
  | 'videoLayer'
  | 'overlayComposer'
  | 'viewer'
  | 'chessboardDetector'
  | 'calibrationFrameScorer'
  | 'scoreThresholdGate'
  | 'consecutiveHoldGate'
  | 'captureRequestBuilder'
  | 'datasetCollector'
  | 'coverageAnalyzer'
  | 'autoCaptureController'
  | 'calibrationSolver'
  | 'poseGuide'
  | 'i2cTransfer'
  | 'eepromProvision';

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
  | 'control.hexarm'
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
  | 'capture.signal'
  | 'capture.trigger'
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

/** 节点的交互式增量更新；`config` 只携带待合并的键，绝不替换完整配置对象。 */
export interface WorkflowNodePatch {
  title?: string;
  config?: Record<string, unknown>;
}

export interface NodePosition {
  x: number;
  y: number;
}

export interface WorkflowNodePositionUpdate {
  nodeId: string;
  position: NodePosition;
}

export interface WorkflowSelectionDeletion {
  nodeIds: string[];
  edgeIds: string[];
}

export interface WorkflowPort {
  id: string;
  label: string;
  direction: PortDirection;
  kind: PortKind;
  schema: string;
  role?: PortRole;
  /** 图像像素格式提示；不改变统一的 image.frame 数据类型。 */
  formatHint?: string;
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

/** 密码只通过本机 WebSocket 写入服务端进程内凭据库；图中只保存返回的 session 引用。 */
export interface SshPasswordRegistration {
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

export type X5SnapshotMode = 'latest' | 'frame_id' | 'timestamp_ns';

export interface X5SnapshotRequest extends X5BindingRequest {
  channel: number;
  mode: X5SnapshotMode;
  frameId?: number;
  timestampNs?: number;
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

/** 可持久化的通用节点标量配置值。 */
export type ScalarConfigValue = string | number | boolean;

/** Dataset Collector 样本审核动作；请求体只在运行时传输，不写入工作流图。 */
export type DatasetSampleActionName = 'accept' | 'reject' | 'enable' | 'disable' | 'delete';

export interface DatasetSampleActionPayload {
  sampleId: string;
}

/** 后端 `runtime.node.action` 当前支持的节点动作。 */
export type NodeActionName =
  | 'connect'
  | 'disconnect'
  | 'trigger'
  | 'arm'
  | 'disarm'
  | 'clear'
  | DatasetSampleActionName
  | 'probe'
  | 'status'
  | 'capture_yuv'
  | 'capture_raw'
  | 'open_rtsp_ch0'
  | 'open_rtsp_ch3'
  | 'open_rtsp_all'
  | 'close_rtsp'
  | 'initialize_api_control'
  | 'calibrate'
  | 'clear_parking_stop'
  | 'zero_current'
  | 'send_joint_positions';

/** 节点显式声明的可用动作；未声明时 GenericWorkflowNode 不渲染动作按钮。 */
export interface NodeActionControl {
  action: NodeActionName;
  label: string;
}
/** Dataset Collector 的最近一次运行时输出；它由引擎维护，绝不回写 WorkflowGraph。 */
export interface DatasetCollectorRuntimeOutput {
  kind: 'calib.dataset.v1';
  count: number;
  samples: DatasetSampleRuntimeOutput[];
}

/** 单个可审核标定样本；图像引用仅含元数据/引用，不含图像字节。 */
export interface DatasetSampleRuntimeOutput {
  id: string;
  imageRef?: DatasetImageReference;
  detection?: unknown;
  score?: DatasetFrameScore | null;
  acceptance?: DatasetSampleAcceptance;
  provenance?: DatasetSampleProvenance;
}

/** 采集图像的轻量引用；format 为 null 表示采集端没有声明像素格式。 */
export interface DatasetImageReference {
  ref: string;
  width: number;
  height: number;
  format: string | null;
}

export interface DatasetFrameScore {
  score: number;
  frameSequence: number;
}

export interface DatasetSampleAcceptance {
  accepted?: boolean;
  enabled?: boolean;
}

export interface DatasetSampleProvenance {
  source?: DatasetSampleSource;
  frameIdentity?: DatasetFrameIdentity;
}

export type DatasetSampleSource = Record<string, unknown>;

export interface DatasetFrameIdentity {
  frameSequence?: number;
  sourcePts?: DatasetSourcePts;
  hostMonotonicTimeNs?: number;
}

export type DatasetSourcePts = Record<string, unknown>;

export interface FlowNodeData extends Record<string, unknown> {
  workflowNode: WorkflowNode;
  preview?: ViewerPreview;
  /** 引擎实时状态；缺省回退到节点的持久化 state。 */
  runtimeState?: 'disabled' | 'idle' | 'ready' | 'running' | 'warning' | 'error';
  /** 引擎实时诊断；用于解释 error/warning 的根因。 */
  runtimeDiagnostic?: string;
  /** 节点动作请求是否仍在等待后端确认。 */
  actionPending?: boolean;
  /** `runtime.node.output` 返回的最新可序列化输出；不写回工作流图。 */
  runtimeOutput?: unknown;
  /** 由节点类型或配置显式声明的可用动作。 */
  availableActions?: readonly NodeActionControl[];
  onRtspUrlChange?: (nodeId: string, url: string) => void;
  onNodeConfigChange?: (nodeId: string, key: string, value: ScalarConfigValue) => void;
  /** 触发节点动作；样本审核 payload 仅经运行时 WS 透传，绝不持久化到图配置。 */
  onNodeAction?: (nodeId: string, action: NodeActionName, payload?: DatasetSampleActionPayload) => void;
  /** 拉取节点最近一次输出；无输出时保留当前摘要。 */
  onRefreshNodeOutput?: (nodeId: string) => void;
}

export interface EdgePulseView {
  id: string;
  edgeId: string;
  packetKind: string;
  sequence?: number;
  startedAt: number;
}

export interface FlowEdgeData extends Record<string, unknown> {
  workflowEdge?: WorkflowEdge;
  kind: PortKind;
  schema: string;
  schemaVersion: string;
}

export async function loadWorkflow(): Promise<WorkflowGraph> {
  return request('graph.current');
}

export async function loadSeedGraph(): Promise<WorkflowGraph> {
  return request('workflow.seed');
}

export async function addGraphNode(node: WorkflowNode): Promise<WorkflowGraph> {
  return request('graph.addNode', node);
}

export async function addGraphNodeAndEdge(node: WorkflowNode, edge: WorkflowEdge): Promise<WorkflowGraph> {
  return request('graph.addNodeAndEdge', { node, edge });
}

export async function addGraphEdge(edge: WorkflowEdge): Promise<WorkflowGraph> {
  return request('graph.addEdge', { edge });
}

export async function removeGraphNode(nodeId: string): Promise<WorkflowGraph> {
  return request('graph.removeNode', { nodeId });
}

export async function removeGraphEdge(edgeId: string): Promise<WorkflowGraph> {
  return request('graph.removeEdge', { edgeId });
}

/**
 * 提交单节点的字段级补丁。
 *
 * 服务端基于当前权威节点合并 `config`，避免陈旧客户端快照覆盖并发编辑。
 */
export async function patchGraphNode(nodeId: string, patch: WorkflowNodePatch): Promise<WorkflowGraph> {
  return request('graph.patchNode', { nodeId, ...patch });
}

export async function updateGraphNodePosition(nodeId: string, position: NodePosition): Promise<WorkflowGraph> {
  return request('graph.updateNode', { nodeId, position });
}

export async function updateGraphNodePositions(nodes: WorkflowNodePositionUpdate[]): Promise<WorkflowGraph> {
  return request('graph.updateNodePositions', { nodes });
}

export async function removeGraphSelection(selection: WorkflowSelectionDeletion): Promise<WorkflowGraph> {
  return request('graph.removeSelection', selection);
}

export async function replaceGraph(graph: WorkflowGraph): Promise<WorkflowGraph> {
  return request('graph.replace', graph);
}

export async function loadNodeCatalog(): Promise<NodeDefinition[]> {
  return request('workflow.nodeCatalog');
}

export async function loadWorkmodeTemplates(): Promise<WorkmodeTemplate[]> {
  return request('workflow.workmodeTemplates');
}

export async function listWorkflows(): Promise<WorkflowSummary[]> {
  return request('workflow.list');
}

export async function loadSavedWorkflow(id: string): Promise<WorkflowGraph> {
  return request('workflow.get', { id });
}

export async function saveWorkflow(graph: WorkflowGraph): Promise<WorkflowGraph> {
  return request('workflow.save', { graph, revision: graph.revision });
}

export async function importWorkflow(graph: WorkflowGraph): Promise<WorkflowGraph> {
  return request('workflow.import', graph);
}

export async function deleteWorkflow(id: string): Promise<void> {
  await request('workflow.delete', { id });
}

export async function validateWorkflow(graph: WorkflowGraph): Promise<void> {
  await request('workflow.validate', graph);
}

/** 拉取引擎记录的某节点最新输出；缺少输出时服务端返回错误。 */
export async function loadRuntimeNodeOutput(nodeId: string): Promise<unknown> {
  return request('runtime.node.output', { nodeId });
}

/** 注册或替换一个仅存在于当前服务端进程的 SSH 密码。 */
export async function registerSshPassword(nodeId: string, password: string): Promise<SshPasswordRegistration> {
  return request('control.ssh.password', { nodeId, password });
}


/** 执行一次显式 I²C 请求；写操作必须由调用者传入确认与 SSH 运行时绑定。 */
export async function runI2cTransfer(requestBody: I2cExecuteRequest): Promise<ControlExecutionResult> {
  return request('control.i2c.run', requestBody);
}

/** EEPROM Inspect 会建立显式 SSH helper 会话，并把最新读回 hash 作为进程内写入门禁。 */
export async function inspectEepromProvision(requestBody: EepromInspectRequest): Promise<EepromInspectResponse> {
  return request('control.eeprom.inspect', requestBody);
}

/** EEPROM 写入必须复用同一进程内 Inspect 快照，并由 helper 执行字节级回读校验。 */
export async function runEepromProvision(requestBody: EepromExecuteRequest): Promise<ControlExecutionResult> {
  return request('control.eeprom.run', requestBody);
}

/** 手动触发一次标定求解；请求体只包含原始 CalibrationRequest，不持久化到 WorkflowGraph。 */
export async function runCalibrationSolver(requestBody: CalibrationRequest): Promise<CalibrationSolution> {
  return request('control.calibration.solver.run', requestBody);
}

/** X5 TCP 控制面只在显式按钮触发时连接设备；host/port 来自节点轻量配置。 */
export async function probeX5Control(requestBody: X5BindingRequest): Promise<X5ControlResponse> {
  return request('control.x5.probe', requestBody);
}

export async function statusX5Control(requestBody: X5BindingRequest): Promise<X5ControlResponse> {
  return request('control.x5.status', requestBody);
}

export async function configureX5Rtsp(requestBody: X5ConfigureRequest): Promise<X5ControlResponse> {
  return request('control.x5.configure-rtsp', requestBody);
}

export async function startX5RtspChannel(requestBody: X5ChannelRequest): Promise<X5ControlResponse> {
  return request('control.x5.start-rtsp', requestBody);
}

export async function stopX5RtspChannel(requestBody: X5ChannelRequest): Promise<X5ControlResponse> {
  return request('control.x5.stop-rtsp', requestBody);
}

export async function captureX5Snapshot(requestBody: X5SnapshotRequest): Promise<X5ControlResponse> {
  return request('control.x5.snapshot', requestBody);
}



/** 向引擎节点投递动作；可选 payload 仅用于 Dataset Collector 的样本审核。 */
export async function nodeAction(nodeId: string, action: string, payload?: DatasetSampleActionPayload): Promise<{ ok: boolean }> {
  return request('runtime.node.action', payload ? { nodeId, action, payload } : { nodeId, action });
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
  return request('file.local.list', { root, path });
}

/** 请求服务器校验 I²C 配置并返回预览；该端点不执行任何 I/O。 */
export async function previewI2cTransfer(requestBody: I2cPreviewRequest): Promise<ControlRequestPreview> {
  return request('control.i2c.preview', requestBody);
}

/** 请求服务器校验 EEPROM 配置并返回预览；该端点不执行任何 I/O。 */
export async function previewEepromProvision(requestBody: EepromPreviewRequest): Promise<ControlRequestPreview> {
  return request('control.eeprom.preview', requestBody);
}

export function labelForPortKind(kind: PortKind): string {
  return kind;
}

/**
 * 校验 source→target 端口可连接性，对齐后端 `validate_edge`（workflow.rs）：
 * - `source.kind != target.kind` → 拒绝（后端同款检查）
 * - `source.schema != target.schema` → 拒绝（后端靠 `edge.schema == source.schema` 隐含，前端更严）
 * 后端额外的 edge 级检查（`edge.kind == source.kind`、`edge.schema == source.schema`、
 * `edge.schema_version == WORKFLOW_SCHEMA_VERSION`）由 onConnect 在构造 edge 时从 source
 * 端口派生写入 + save 时 `toWorkflowGraph` 写入 schemaVersion，保证不出现「前端放行→后端拒绝」。
 */
export function validateConnectionKinds(source: WorkflowPort, target: WorkflowPort): string | null {
  if (source.kind !== target.kind) {
    return `${labelForPortKind(source.kind)} 不能连接到 ${labelForPortKind(target.kind)}`;
  }
  if (source.schema !== target.schema) {
    return `${source.schema} 不能连接到 ${target.schema}`;
  }
  if (!formatHintsCompatible(source.formatHint, target.formatHint)) {
    return `像素格式 ${source.formatHint ?? '未声明'} 不能连接到 ${target.formatHint ?? '未声明'}`;
  }
  return null;
}

function formatHintsCompatible(source?: string, target?: string): boolean {
  if (!source || !target) {
    return true;
  }
  const sourceFormats = splitFormatHints(source);
  const targetFormats = splitFormatHints(target);
  return sourceFormats.some((format) => targetFormats.includes(format));
}

function splitFormatHints(value: string): string[] {
  return value
    .split('|')
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

async function request<T>(path: string, payload?: unknown): Promise<T> {
  return (await wsRequest(path, payload)) as T;
}
