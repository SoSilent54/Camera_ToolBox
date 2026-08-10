export type NodeKind = 'rtspSource' | 'viewer';
export type NodeRuntimeState = 'idle' | 'ready' | 'running' | 'warning' | 'error';
export type PortDirection = 'input' | 'output';
export type DataKind = 'rtsp-stream';

export interface WorkflowGraph {
  id: string;
  title: string;
  nodes: WorkflowNode[];
  edges: WorkflowEdge[];
}

export interface WorkflowNode {
  id: string;
  kind: NodeKind;
  title: string;
  position: NodePosition;
  state: NodeRuntimeState;
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
  dataKind: DataKind;
}

export interface WorkflowEdge {
  id: string;
  source: PortEndpoint;
  target: PortEndpoint;
  dataKind: DataKind;
}

export interface PortEndpoint {
  nodeId: string;
  portId: string;
}

export interface FlowNodeData extends Record<string, unknown> {
  workflowNode: WorkflowNode;
  previewUrl?: string;
}

export interface FlowEdgeData extends Record<string, unknown> {
  workflowEdge?: WorkflowEdge;
  dataKind: DataKind;
}

export async function loadWorkflow(): Promise<WorkflowGraph> {
  const response = await fetch('/api/workflow');
  if (!response.ok) {
    throw new Error(`failed to load workflow: ${response.status} ${response.statusText}`);
  }
  return (await response.json()) as WorkflowGraph;
}

export function labelForDataKind(dataKind: DataKind): string {
  switch (dataKind) {
    case 'rtsp-stream':
      return 'RTSP stream';
  }
}
