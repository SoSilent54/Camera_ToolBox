use serde::{Deserialize, Serialize};
use serde_json::json;

/// 工作流图的最小传输模型；第一版只承载 UI 画布、端口和连接校验所需字段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowGraph {
    pub id: String,
    pub title: String,
    pub nodes: Vec<WorkflowNode>,
    pub edges: Vec<WorkflowEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowNode {
    pub id: String,
    pub kind: NodeKind,
    pub title: String,
    pub position: NodePosition,
    pub state: NodeRuntimeState,
    pub inputs: Vec<WorkflowPort>,
    pub outputs: Vec<WorkflowPort>,
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NodePosition {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NodeKind {
    RtspSource,
    Viewer,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NodeRuntimeState {
    Idle,
    Ready,
    Running,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPort {
    pub id: String,
    pub label: String,
    pub direction: PortDirection,
    pub data_kind: DataKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PortDirection {
    Input,
    Output,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DataKind {
    RtspStream,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEdge {
    pub id: String,
    pub source: PortEndpoint,
    pub target: PortEndpoint,
    pub data_kind: DataKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PortEndpoint {
    pub node_id: String,
    pub port_id: String,
}

/// 生成第一版演示图：RTSP 输入节点把流句柄连接到 Viewer 节点。
pub fn seed_workflow_graph() -> WorkflowGraph {
    WorkflowGraph {
        id: "camera-toolbox-demo-workflow".to_owned(),
        title: "RTSP Preview Workspace".to_owned(),
        nodes: vec![
            WorkflowNode {
                id: "rtsp-source-1".to_owned(),
                kind: NodeKind::RtspSource,
                title: "RTSP Input".to_owned(),
                position: NodePosition { x: 80.0, y: 140.0 },
                state: NodeRuntimeState::Ready,
                inputs: Vec::new(),
                outputs: vec![WorkflowPort {
                    id: "stream".to_owned(),
                    label: "RTSP stream".to_owned(),
                    direction: PortDirection::Output,
                    data_kind: DataKind::RtspStream,
                }],
                config: json!({
                    "url": "rtsp://10.21.12.108:554/PRR",
                    "transport": "tcp",
                    "expectedWidth": 1920,
                    "expectedHeight": 1080,
                    "expectedFps": 60
                }),
            },
            WorkflowNode {
                id: "viewer-1".to_owned(),
                kind: NodeKind::Viewer,
                title: "Viewer".to_owned(),
                position: NodePosition { x: 520.0, y: 132.0 },
                state: NodeRuntimeState::Idle,
                inputs: vec![WorkflowPort {
                    id: "stream".to_owned(),
                    label: "Stream input".to_owned(),
                    direction: PortDirection::Input,
                    data_kind: DataKind::RtspStream,
                }],
                outputs: Vec::new(),
                config: json!({
                    "fitMode": "contain",
                    "overlay": "status"
                }),
            },
        ],
        edges: vec![WorkflowEdge {
            id: "edge-rtsp-viewer".to_owned(),
            source: PortEndpoint {
                node_id: "rtsp-source-1".to_owned(),
                port_id: "stream".to_owned(),
            },
            target: PortEndpoint {
                node_id: "viewer-1".to_owned(),
                port_id: "stream".to_owned(),
            },
            data_kind: DataKind::RtspStream,
        }],
    }
}

/// 校验端口方向与数据类型，避免 UI 产生无法执行的数据流边。
pub fn validate_edge(graph: &WorkflowGraph, edge: &WorkflowEdge) -> Result<(), String> {
    if edge.source.node_id == edge.target.node_id {
        return Err("self-loop connections are not supported".to_owned());
    }

    let source = find_port(graph, &edge.source, PortDirection::Output)?;
    let target = find_port(graph, &edge.target, PortDirection::Input)?;
    if source.data_kind != target.data_kind {
        return Err(format!(
            "data kind mismatch: source {:?}, target {:?}",
            source.data_kind, target.data_kind
        ));
    }
    if source.data_kind != edge.data_kind {
        return Err(format!(
            "edge declares {:?}, but source emits {:?}",
            edge.data_kind, source.data_kind
        ));
    }
    Ok(())
}

fn find_port<'a>(
    graph: &'a WorkflowGraph,
    endpoint: &PortEndpoint,
    direction: PortDirection,
) -> Result<&'a WorkflowPort, String> {
    let node = graph
        .nodes
        .iter()
        .find(|node| node.id == endpoint.node_id)
        .ok_or_else(|| format!("node `{}` does not exist", endpoint.node_id))?;
    let ports = match direction {
        PortDirection::Input => &node.inputs,
        PortDirection::Output => &node.outputs,
    };
    ports
        .iter()
        .find(|port| port.id == endpoint.port_id && port.direction == direction)
        .ok_or_else(|| {
            format!(
                "{:?} port `{}` does not exist on node `{}`",
                direction, endpoint.port_id, endpoint.node_id
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_graph_contains_valid_rtsp_viewer_edge() {
        let graph = seed_workflow_graph();
        let edge = graph.edges.first().expect("seed graph edge exists");
        validate_edge(&graph, edge).expect("seed edge is valid");
    }

    #[test]
    fn validation_rejects_self_loop() {
        let graph = seed_workflow_graph();
        let edge = WorkflowEdge {
            id: "bad".to_owned(),
            source: PortEndpoint {
                node_id: "rtsp-source-1".to_owned(),
                port_id: "stream".to_owned(),
            },
            target: PortEndpoint {
                node_id: "rtsp-source-1".to_owned(),
                port_id: "stream".to_owned(),
            },
            data_kind: DataKind::RtspStream,
        };
        assert!(validate_edge(&graph, &edge).is_err());
    }
}
