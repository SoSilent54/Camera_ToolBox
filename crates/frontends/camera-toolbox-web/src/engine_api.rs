//! 数据流引擎 HTTP 接入：把工作流图装载进引擎、驱动节点动作、暴露状态与 viewer 帧。

use std::sync::{Arc, Mutex};

use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

use camera_toolbox_app::engine::{
    EdgeSpec, EngineServices, GraphBuildError, GraphEngine, GraphSpec, NodeAction, NodeRegistry,
    NodeSpec, PortCardinality, PortEndpoint, PortSpec, StreamServiceFactory,
};
use camera_toolbox_app::platform::{RtspStreamConfig, StreamService};
use camera_toolbox_adapters::media::FfmpegRtspStreamService;
use camera_toolbox_adapters::{ImageRasterCodec, LocalRawLoader};

use crate::{
    AppState,
    workflow::{
        NodeKind, PortDirection, PortKind, WorkflowEdge, WorkflowGraph, WorkflowNode,
        WorkflowPort,
    },
};

/// 引擎运行时：节点注册表 + 当前已装载的图。
pub struct EngineRuntime {
    pub registry: NodeRegistry,
    engine: Mutex<Option<GraphEngine>>,
}

impl EngineRuntime {
    pub fn new() -> Self {
        let mut registry = NodeRegistry::new();
        camera_toolbox_app::engine::register_builtin(&mut registry);
        Self {
            registry,
            engine: Mutex::new(None),
        }
    }

    fn engine(&self) -> std::sync::MutexGuard<'_, Option<GraphEngine>> {
        self.engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// 按 URL 配置创建独立 FFmpeg RTSP 流服务。
struct FfmpegStreamServiceFactory;

impl StreamServiceFactory for FfmpegStreamServiceFactory {
    fn create(&self, config: RtspStreamConfig) -> Arc<dyn StreamService> {
        Arc::new(FfmpegRtspStreamService::new(
            format!("web-engine-{}", config.url),
            config,
        ))
    }
}

/// 装配引擎服务：流工厂 + 可选标定后端 + RAW 加载器 + raster 编解码。
fn build_services(state: &AppState) -> EngineServices {
    EngineServices {
        stream_factory: Some(Arc::new(FfmpegStreamServiceFactory)),
        #[cfg(feature = "calibration-opencv")]
        calibration: Some(state.calibration_backend.clone()),
        #[cfg(not(feature = "calibration-opencv"))]
        calibration: None,
        // 本地 RAW 加载与 raster 编解码始终可用（无 feature gate），
        // 供 LocalFileSource 与 ChessboardDetector 等节点使用。
        raw_loader: Some(Arc::new(LocalRawLoader)),
        image_codec: Some(Arc::new(ImageRasterCodec)),
    }
}

/// 装载工作流图进引擎（替换旧图）。
pub async fn run_engine(
    State(state): State<AppState>,
    Json(graph): Json<WorkflowGraph>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let spec = to_engine_spec(&graph);
    let services = build_services(&state);
    let engine = GraphEngine::build(spec, &state.engine_runtime.registry, services)
        .map_err(|error: GraphBuildError| (StatusCode::BAD_REQUEST, error.to_string()))?;
    let mut slot = state.engine_runtime.engine();
    if let Some(mut previous) = slot.take() {
        previous.stop();
    }
    *slot = Some(engine);
    Ok(Json(json!({ "running": true, "nodes": graph.nodes.len() })))
}

/// 停止并卸载当前引擎图。
pub async fn stop_engine(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut slot = state.engine_runtime.engine();
    if let Some(mut engine) = slot.take() {
        engine.stop();
    }
    Json(json!({ "running": false }))
}

/// 图级 run/start：向所有可启动节点派发启动动作（尽力启动）。
pub async fn start_engine(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let engine = state.engine_runtime.engine();
    let engine = engine
        .as_ref()
        .ok_or_else(|| (StatusCode::CONFLICT, "engine not running".to_owned()))?;
    engine
        .start_all()
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    Ok(Json(json!({ "started": true })))
}

/// 节点动作请求体。
#[derive(Debug, Deserialize)]
pub struct ActionRequest {
    pub action: String,
}

/// 向节点投递动作（connect/disconnect/trigger/arm/disarm）。
pub async fn node_action(
    State(state): State<AppState>,
    Path(node_id): Path<String>,
    Json(request): Json<ActionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let action = parse_action(&request.action)?;
    let engine = state.engine_runtime.engine();
    let engine = engine
        .as_ref()
        .ok_or_else(|| (StatusCode::CONFLICT, "engine not running".to_owned()))?;
    engine
        .send_action(&node_id, action)
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

/// 非阻塞取回节点状态更新。
pub async fn engine_status(State(state): State<AppState>) -> Json<Vec<camera_toolbox_app::engine::NodeStatusReport>> {
    let engine = state.engine_runtime.engine();
    let statuses = engine
        .as_ref()
        .map(GraphEngine::drain_status)
        .unwrap_or_default();
    Json(statuses)
}

/// 取回 viewer 节点的最新帧并编码为 JPEG。
pub async fn viewer_frame(
    State(state): State<AppState>,
    Path(node_id): Path<String>,
) -> Response {
    let frame = {
        let engine = state.engine_runtime.engine();
        engine.as_ref().and_then(|engine| engine.viewer_frame(&node_id))
    };
    let Some(frame) = frame else {
        return (StatusCode::NOT_FOUND, "no frame available").into_response();
    };
    match encode_jpeg(&frame) {
        Ok(jpeg) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "image/jpeg")],
            jpeg,
        )
            .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

fn parse_action(action: &str) -> Result<NodeAction, (StatusCode, String)> {
    match action {
        "connect" => Ok(NodeAction::Connect),
        "disconnect" => Ok(NodeAction::Disconnect),
        "trigger" => Ok(NodeAction::Trigger),
        "arm" => Ok(NodeAction::Arm),
        "disarm" => Ok(NodeAction::Disarm),
        other => Err((
            StatusCode::BAD_REQUEST,
            format!("unknown action `{other}`"),
        )),
    }
}

fn encode_jpeg(frame: &camera_toolbox_app::DecodedVideoFrame) -> Result<Vec<u8>, String> {
    crate::encode_rgba_as_jpeg(frame)
}

/// 把持久化工作流图投影为引擎图规格。
fn to_engine_spec(graph: &WorkflowGraph) -> GraphSpec {
    let nodes = graph.nodes.iter().map(to_node_spec).collect();
    let edges = graph.edges.iter().map(to_edge_spec).collect();
    GraphSpec { nodes, edges }
}

fn to_node_spec(node: &WorkflowNode) -> NodeSpec {
    NodeSpec {
        id: node.id.clone(),
        kind: node_kind_str(node.kind),
        title: node.title.clone(),
        inputs: node.inputs.iter().map(to_port_spec).collect(),
        outputs: node.outputs.iter().map(to_port_spec).collect(),
        config: node.config.clone(),
    }
}

fn to_port_spec(port: &WorkflowPort) -> PortSpec {
    PortSpec {
        id: port.id.clone(),
        label: port.label.clone(),
        kind: port_kind_str(port.kind),
        cardinality: to_cardinality(port.cardinality),
        required: port.required && port.direction == PortDirection::Input,
    }
}

fn to_cardinality(cardinality: crate::workflow::PortCardinality) -> PortCardinality {
    match cardinality {
        crate::workflow::PortCardinality::One => PortCardinality::One,
        crate::workflow::PortCardinality::Many => PortCardinality::Many,
    }
}

fn to_edge_spec(edge: &WorkflowEdge) -> EdgeSpec {
    EdgeSpec {
        id: edge.id.clone(),
        source: PortEndpoint {
            node_id: edge.source.node_id.clone(),
            port_id: edge.source.port_id.clone(),
        },
        target: PortEndpoint {
            node_id: edge.target.node_id.clone(),
            port_id: edge.target.port_id.clone(),
        },
    }
}

fn node_kind_str(kind: NodeKind) -> String {
    enum_str(kind)
}

fn port_kind_str(kind: PortKind) -> String {
    enum_str(kind)
}

fn enum_str<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_engine_spec_projects_seed_graph() {
        let graph = crate::workflow::seed_workflow_graph();
        let spec = to_engine_spec(&graph);
        assert_eq!(spec.nodes.len(), graph.nodes.len());
        assert_eq!(spec.edges.len(), graph.edges.len());
        // kind 序列化为 camelCase 字符串（与引擎 kinds 常量一致）。
        assert!(spec
            .nodes
            .iter()
            .any(|node| node.kind == "rtspSource"));
        // 必需输入端口投影为 required。
        for (engine_node, graph_node) in spec.nodes.iter().zip(&graph.nodes) {
            for (engine_port, graph_port) in engine_node.inputs.iter().zip(&graph_node.inputs) {
                assert_eq!(
                    engine_port.required,
                    graph_port.required && graph_port.direction == PortDirection::Input
                );
            }
        }
    }
}
