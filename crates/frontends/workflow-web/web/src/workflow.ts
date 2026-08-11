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

export interface FlowNodeData extends Record<string, unknown> {
  workflowNode: WorkflowNode;
  previewUrl?: string;
  onRtspUrlChange?: (nodeId: string, url: string) => void;
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
