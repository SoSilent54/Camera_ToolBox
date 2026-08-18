//! 数据流引擎 WebSocket 接入辅助：运行时槽、图投影、节点动作解析与输出 JSON 投影。

use std::sync::{Arc, Mutex};

#[cfg(feature = "hex-arm-control")]
use camera_toolbox_adapters::hex_arm::HexArmWebSocketClient;
use camera_toolbox_adapters::media::FfmpegRtspStreamService;
use camera_toolbox_adapters::{ImageRasterCodec, LocalRawLoader};
use camera_toolbox_app::engine::{
    DataPacket, EdgeSpec, EngineServices, GraphEngine, GraphSpec, NodeAction, NodeRegistry,
    NodeSpec, PortCardinality, PortEndpoint, PortSpec, StreamServiceFactory,
};
use camera_toolbox_app::platform::{RtspStreamConfig, StreamService};
use serde_json::json;

use crate::{
    AppState,
    workflow::{
        NodeKind, PortDirection, PortKind, WorkflowEdge, WorkflowGraph, WorkflowNode, WorkflowPort,
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

    /// 访问当前已装载的引擎槽（`None` 表示引擎未运行）。ws_router 增量图命令复用。
    pub(crate) fn engine(&self) -> std::sync::MutexGuard<'_, Option<GraphEngine>> {
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
pub(crate) fn build_services(state: &AppState) -> EngineServices {
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
        // I²C / EEPROM 执行器：ControlRuntime 已实现 I2cExecutor/EepromExecutor，
        // 内部复用 SshI2cHelperService/SshEepromProvisionService + helper payload。
        #[cfg(feature = "platform-ssh")]
        i2c_executor: Some(state.control_runtime.clone()),
        #[cfg(feature = "platform-ssh")]
        eeprom_executor: Some(state.control_runtime.clone()),
        #[cfg(not(feature = "platform-ssh"))]
        i2c_executor: None,
        #[cfg(not(feature = "platform-ssh"))]
        eeprom_executor: None,
        // X5 控制客户端：纯 TCP（无 SSH），始终可用；ControlRuntime 无条件 impl X5ControlClient。
        x5_client: Some(state.control_runtime.clone()),
        // Hex Arm 协议客户端只有显式启用 feature 后才注入；默认构建不保留网络控制能力。
        #[cfg(feature = "hex-arm-control")]
        hex_arm_client: Some(Arc::new(HexArmWebSocketClient::default())),
        #[cfg(not(feature = "hex-arm-control"))]
        hex_arm_client: None,
        // SFTP / SSH 命令执行器：ControlRuntime 已实现 SftpFileReader/SshCommandExecutor。
        #[cfg(feature = "platform-ssh")]
        sftp_reader: Some(state.control_runtime.clone()),
        #[cfg(feature = "platform-ssh")]
        ssh_command_executor: Some(state.control_runtime.clone()),
        #[cfg(not(feature = "platform-ssh"))]
        sftp_reader: None,
        #[cfg(not(feature = "platform-ssh"))]
        ssh_command_executor: None,
    }
}

/// 把 `DataPacket` 折叠成可序列化的 JSON：帧类只给元数据，其余（Detection/Solution/弱类型）直接序列化。
pub(crate) fn packet_to_json(packet: &DataPacket) -> serde_json::Value {
    match packet {
        DataPacket::VideoFrame(frame) => json!({
            "type": "video-frame",
            "width": frame.width,
            "height": frame.height,
            "sequence": frame.identity.frame_sequence,
        }),
        DataPacket::ImageFrame(frame) => json!({
            "type": "image-frame",
            "width": frame.width,
            "height": frame.height,
            "format": frame.format.to_string(),
            "sequence": frame.identity.frame_sequence,
        }),
        DataPacket::Detection(detection) => {
            let mut value = serde_json::to_value(detection.detection.as_ref())
                .unwrap_or(json!({ "type": "detection" }));
            if let serde_json::Value::Object(object) = &mut value {
                object.insert(
                    "frameSequence".to_owned(),
                    json!(detection.frame_identity.frame_sequence),
                );
            }
            value
        }
        DataPacket::Solution(solution) => {
            serde_json::to_value(solution.as_ref()).unwrap_or(json!({ "type": "solution" }))
        }
        DataPacket::Coverage(value)
        | DataPacket::Dataset(value)
        | DataPacket::Report(value)
        | DataPacket::Target(value)
        | DataPacket::Json(value) => (**value).clone(),
        DataPacket::Score(score) => json!({
            "type": "capture.score",
            "gain": score.gain,
            "frameSequence": score.frame_identity.frame_sequence,
        }),
        DataPacket::CaptureRequest(request) => json!({
            "type": "command.capture.request.v1",
            "target": format!("{:?}", request.target),
            "mode": format!("{:?}", request.mode),
            "frameSequence": request.source_identity.as_ref().map(|identity| identity.frame_sequence),
        }),
    }
}

pub(crate) fn parse_action_str(action: &str) -> Result<NodeAction, String> {
    match action {
        "connect" => Ok(NodeAction::Connect),
        "disconnect" => Ok(NodeAction::Disconnect),
        "trigger" => Ok(NodeAction::Trigger),
        "arm" => Ok(NodeAction::Arm),
        "disarm" => Ok(NodeAction::Disarm),
        "clear"
        | "probe"
        | "status"
        | "capture_yuv"
        | "capture_raw"
        | "initialize_api_control"
        | "calibrate"
        | "clear_parking_stop"
        | "zero_current"
        | "send_joint_positions" => Ok(NodeAction::Custom {
            name: action.to_owned(),
            payload: serde_json::Value::Null,
        }),
        other => Err(format!("unknown action `{other}`")),
    }
}

/// 把持久化工作流图投影为引擎图规格。
pub(crate) fn to_engine_spec(graph: &WorkflowGraph) -> GraphSpec {
    let nodes = graph.nodes.iter().map(to_node_spec).collect();
    let edges = graph.edges.iter().map(to_edge_spec).collect();
    GraphSpec { nodes, edges }
}

pub(crate) fn to_node_spec(node: &WorkflowNode) -> NodeSpec {
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

pub(crate) fn to_edge_spec(edge: &WorkflowEdge) -> EdgeSpec {
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
        assert!(spec.nodes.iter().any(|node| node.kind == "x5233Driver"));
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

    #[test]
    fn clear_action_maps_to_dataset_custom_action() {
        assert!(matches!(
            parse_action_str("clear"),
            Ok(NodeAction::Custom { ref name, .. }) if name == "clear"
        ));
    }

    #[test]
    fn hex_arm_actions_map_to_explicit_custom_actions() {
        for action in [
            "probe",
            "status",
            "initialize_api_control",
            "calibrate",
            "clear_parking_stop",
            "zero_current",
            "send_joint_positions",
        ] {
            assert!(matches!(
                parse_action_str(action),
                Ok(NodeAction::Custom { name, .. }) if name == action
            ));
        }
    }
}
