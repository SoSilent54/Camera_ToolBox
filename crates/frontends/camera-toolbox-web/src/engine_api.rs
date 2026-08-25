//! 数据流引擎 WebSocket 接入辅助：运行时槽、图投影、节点动作解析与输出 JSON 投影。

use std::sync::{Arc, Mutex};

#[cfg(feature = "hex-arm-control")]
use camera_toolbox_adapters::hex_arm::HexArmWebSocketClient;
use camera_toolbox_adapters::media::FfmpegRtspStreamService;
use camera_toolbox_adapters::{ImageRasterCodec, LocalRawLoader};
use camera_toolbox_app::engine::{
    DataPacket, EdgeSpec, EngineServices, GraphEngine, GraphSpec, ImageFrameIdentity, NodeAction,
    NodeRegistry, NodeSpec, PortCardinality, PortEndpoint, PortSpec, StreamServiceFactory,
};
use camera_toolbox_app::platform::{
    RtspStreamConfig, SourcePts, SourcePtsProvenance, StreamService,
};
use serde_json::json;

use crate::{
    AppState,
    serial_field::SerialFieldFactory,
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
        registry.register(Box::new(SerialFieldFactory));
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
        // 计划执行与 SSH 会话均由 Web ControlRuntime 注入；旧的自由 I²C/EEPROM
        // executor 不再属于引擎服务接口。
        #[cfg(feature = "platform-ssh")]
        i2c_task_executor: Some(state.control_runtime.clone()),
        #[cfg(not(feature = "platform-ssh"))]
        i2c_task_executor: None,
        #[cfg(feature = "platform-ssh")]
        ssh_connection_service: Some(state.control_runtime.clone()),
        #[cfg(not(feature = "platform-ssh"))]
        ssh_connection_service: None,
        // X5 控制客户端：纯 TCP（无 SSH），始终可用；ControlRuntime 无条件 impl X5ControlClient。
        x5_client: Some(state.control_runtime.clone()),
        // Hex Arm 协议客户端只有显式启用 feature 后才注入。
        #[cfg(feature = "hex-arm-control")]
        hex_arm_client: Some(Arc::new(HexArmWebSocketClient::default())),
        #[cfg(not(feature = "hex-arm-control"))]
        hex_arm_client: None,
        // SFTP / SSH 命令执行器继续复用 ControlRuntime 的 allowlisted 服务。
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

/// 把 `DataPacket` 折叠成可序列化的 JSON；计划节点输出保留结构化审计信息。
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
        DataPacket::CalibrationBoardParams(params) => serde_json::to_value(params.as_ref())
            .unwrap_or(json!({ "kind": "calib.board.params.v1" })),
        DataPacket::CameraModelParams(params) => serde_json::to_value(params.as_ref())
            .unwrap_or(json!({ "kind": "calib.camera.model.v1" })),
        DataPacket::DistortionModelParams(params) => serde_json::to_value(params.as_ref())
            .unwrap_or(json!({ "kind": "calib.distortion.model.v1" })),
        DataPacket::DetectionPose(pose) => json!({
            "kind": pose.kind,
            "frameIdentity": pose_frame_identity_to_json(&pose.frame_identity),
            "convention": pose.convention,
            "translationM": pose.translation_m,
            "rotationRodrigues": pose.rotation_rodrigues,
            "reprojectionErrorPx": pose.reprojection_error_px,
        }),
        DataPacket::StructuredPacket(packet) => {
            serde_json::to_value(packet.as_ref()).unwrap_or(json!({ "type": "structured.packet" }))
        }
        DataPacket::SshConnection(connection) => json!({
            "type": "ssh.connection.v1",
            "id": connection.id(),
            "target": {
                "host": connection.target().host,
                "port": connection.target().port,
                "username": connection.target().username,
            },
        }),
        DataPacket::TypedField {
            datum, generation, ..
        } => {
            let mut value =
                serde_json::to_value(datum.as_ref()).unwrap_or(json!({ "type": "data.field" }));
            if let serde_json::Value::Object(object) = &mut value {
                object.insert("generation".to_owned(), json!(generation));
            }
            value
        }
        DataPacket::I2cReadReport(report) => json!({
            "type": "i2c.read-report.v1",
            "mapId": report.map_id,
            "mapDigest": report.map_digest,
            "target": format!("{:?}", report.target),
            "imageSha256": report.image_sha256,
            "byteLength": report.byte_len,
            "valid": report.valid,
            "error": report.error,
        }),
        DataPacket::I2cExecutionReport(report) => {
            let pages = report
                .pages
                .iter()
                .map(|page| json!({
                    "offset": page.offset,
                    "expectedHex": page.expected.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
                    "readbackHex": page.readback.as_ref().map(|bytes| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()),
                    "error": page.error,
                }))
                .collect::<Vec<_>>();
            json!({
                "type": "i2c.execution-report.v1",
                "beforeImageSha256": report.before_image_sha256,
                "pages": pages,
                "pageCount": report.pages.len(),
                "finalVerified": report.final_verified,
                "error": report.error,
            })
        }
        DataPacket::Coverage(value)
        | DataPacket::Dataset(value)
        | DataPacket::Report(value)
        | DataPacket::Target(value)
        | DataPacket::Json(value) => (**value).clone(),
        DataPacket::Score(score) => json!({
            "type": "capture.score",
            "score": score.score,
            "frameSequence": score.frame_identity.frame_sequence,
        }),
        DataPacket::CaptureSignal(signal) => json!({
            "type": "capture.signal",
            "accepted": signal.accepted,
            "frameSequence": signal.frame_identity.frame_sequence,
        }),
        DataPacket::CaptureTrigger(trigger) => json!({
            "type": "capture.trigger",
            "frameSequence": trigger.frame_identity.frame_sequence,
        }),
        DataPacket::CaptureRequest(request) => json!({
            "type": "command.capture.request.v1",
            "target": format!("{:?}", request.target),
            "mode": format!("{:?}", request.mode),
            "frameSequence": request.source_identity.as_ref().map(|identity| identity.frame_sequence),
        }),
    }
}

/// 沿用 Dataset 的帧身份 JSON 字段，避免为 Pose 引入第二套身份协议。
fn pose_frame_identity_to_json(identity: &ImageFrameIdentity) -> serde_json::Value {
    json!({
        "frameSequence": identity.frame_sequence,
        "sourcePts": pose_source_pts_to_json(&identity.source_pts),
        "hostMonotonicTimeNs": identity.host_monotonic_time_ns,
    })
}

/// 沿用 Dataset 的 Source PTS JSON 形状，供姿态运行时输出显示来源时钟。
fn pose_source_pts_to_json(source_pts: &SourcePts) -> serde_json::Value {
    match source_pts {
        SourcePts::Known {
            ticks,
            time_base_numerator,
            time_base_denominator,
            provenance,
        } => json!({
            "kind": "known",
            "ticks": ticks,
            "timeBase": {
                "numerator": time_base_numerator,
                "denominator": time_base_denominator,
            },
            "provenance": match provenance {
                SourcePtsProvenance::FfmpegDecodedFrame => "ffmpegDecodedFrame",
                SourcePtsProvenance::FfmpegShowinfo => "ffmpegShowinfo",
                SourcePtsProvenance::Unavailable => "unavailable",
            },
        }),
        SourcePts::Unavailable { reason } => json!({"kind": "unavailable", "reason": reason}),
    }
}

pub(crate) fn parse_action_str(
    action: &str,
    payload: serde_json::Value,
) -> Result<NodeAction, String> {
    match action {
        "connect" => Ok(NodeAction::Connect),
        "disconnect" => Ok(NodeAction::Disconnect),
        "trigger" => Ok(NodeAction::Trigger),
        "arm" => Ok(NodeAction::Arm),
        "disarm" => Ok(NodeAction::Disarm),
        "select"
        | "accept"
        | "reject"
        | "enable"
        | "disable"
        | "delete"
        | "clear"
        | "probe"
        | "status"
        | "capture_yuv"
        | "capture_raw"
        | "open_rtsp_ch0"
        | "open_rtsp_ch3"
        | "open_rtsp_all"
        | "close_rtsp"
        | "connect_ssh"
        | "initialize_api_control"
        | "calibrate"
        | "clear_parking_stop"
        | "zero_current"
        | "send_joint_positions"
        | "read"
        | "write" => Ok(NodeAction::Custom {
            name: action.to_owned(),
            payload,
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
    use std::sync::Arc;

    use camera_toolbox_app::{
        engine::{
            CalibrationBoardParams, CalibrationVector3, CameraModelParams, DetectionPose,
            DistortionModelParams, FrameProvenance, ImageFrameIdentity,
        },
        platform::{SourcePts, SourcePtsProvenance, StreamSessionId},
    };

    fn pose_identity() -> ImageFrameIdentity {
        ImageFrameIdentity {
            provenance: FrameProvenance::Stream {
                stream_id: StreamSessionId::new("rtsp-camera-0").expect("valid stream id"),
                channel: 3,
            },
            frame_sequence: 42,
            source_pts: SourcePts::Known {
                ticks: 9_000,
                time_base_numerator: 1,
                time_base_denominator: 90_000,
                provenance: SourcePtsProvenance::FfmpegDecodedFrame,
            },
            host_monotonic_time_ns: 123_456,
            device_timestamp_ns: Some(987_654),
        }
    }

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
    fn packet_to_json_projects_calibration_parameter_and_pose_contracts() {
        for (packet, expected) in [
            (
                DataPacket::CalibrationBoardParams(Arc::new(CalibrationBoardParams::default())),
                serde_json::json!({
                    "kind": "calib.board.params.v1",
                    "boardKind": "chessboard",
                    "cols": 11,
                    "rows": 8,
                    "squareSizeMm": 40.0,
                }),
            ),
            (
                DataPacket::CameraModelParams(Arc::new(CameraModelParams::default())),
                serde_json::json!({
                    "kind": "calib.camera.model.v1",
                    "model": "pinhole",
                    "fx": 900.0,
                    "fy": 900.0,
                    "cx": 960.0,
                    "cy": 540.0,
                    "imageSize": {"width": 1920, "height": 1080},
                }),
            ),
            (
                DataPacket::DistortionModelParams(Arc::new(DistortionModelParams::default())),
                serde_json::json!({
                    "kind": "calib.distortion.model.v1",
                    "model": "none",
                    "coefficients": [],
                }),
            ),
        ] {
            assert_eq!(packet_to_json(&packet), expected);
        }

        let pose = DetectionPose::new(
            pose_identity(),
            CalibrationVector3::new(0.0, 0.0, 1.0),
            CalibrationVector3::new(0.0, 0.1, 0.0),
            Some(0.25),
        )
        .expect("finite pose");
        assert_eq!(
            packet_to_json(&DataPacket::DetectionPose(Arc::new(pose))),
            serde_json::json!({
                "kind": "calib.pose.v1",
                "frameIdentity": {
                    "frameSequence": 42,
                    "sourcePts": {
                        "kind": "known",
                        "ticks": 9_000,
                        "timeBase": {"numerator": 1, "denominator": 90_000},
                        "provenance": "ffmpegDecodedFrame",
                    },
                    "hostMonotonicTimeNs": 123_456,
                },
                "convention": "T_camera_board",
                "translationM": {"x": 0.0, "y": 0.0, "z": 1.0},
                "rotationRodrigues": {"x": 0.0, "y": 0.1, "z": 0.0},
                "reprojectionErrorPx": 0.25,
            })
        );
    }

    #[test]
    fn clear_action_maps_to_dataset_custom_action_with_null_payload() {
        assert!(matches!(
            parse_action_str("clear", serde_json::Value::Null),
            Ok(NodeAction::Custom { ref name, payload: serde_json::Value::Null }) if name == "clear"
        ));
    }

    #[test]
    fn dataset_row_actions_preserve_sample_id_payload() {
        let payload = serde_json::json!({ "sampleId": "sample-1" });
        for action in ["select", "accept", "reject", "enable", "disable", "delete"] {
            assert!(matches!(
                parse_action_str(action, payload.clone()),
                Ok(NodeAction::Custom { name, payload: actual })
                    if name == action && actual == payload
            ));
        }
    }

    #[test]
    fn custom_control_actions_map_to_explicit_custom_actions() {
        for action in [
            "probe",
            "status",
            "open_rtsp_ch0",
            "open_rtsp_ch3",
            "open_rtsp_all",
            "close_rtsp",
            "connect_ssh",
            "initialize_api_control",
            "calibrate",
            "clear_parking_stop",
            "zero_current",
            "send_joint_positions",
        ] {
            assert!(matches!(
                parse_action_str(action, serde_json::Value::Null),
                Ok(NodeAction::Custom { name, .. }) if name == action
            ));
        }
    }
}
