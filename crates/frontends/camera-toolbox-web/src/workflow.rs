use serde::{Deserialize, Serialize};
use serde_json::json;

/// 工作流持久化 schema；只记录拓扑、布局和轻量配置，不保存运行时句柄或帧数据。
pub const WORKFLOW_SCHEMA_VERSION: &str = "workflow.v1";
pub const DEFAULT_RTSP_URL: &str = "rtsp://10.21.12.108:554/PRR";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowGraph {
    pub schema_version: String,
    pub id: String,
    pub title: String,
    pub revision: String,
    pub nodes: Vec<WorkflowNode>,
    pub edges: Vec<WorkflowEdge>,
    #[serde(default)]
    pub viewport: Option<WorkflowViewport>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowViewport {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowNode {
    pub id: String,
    pub kind: NodeKind,
    pub title: String,
    pub position: NodePosition,
    pub state: NodeRuntimeState,
    #[serde(default)]
    pub category: NodeCategory,
    pub inputs: Vec<WorkflowPort>,
    pub outputs: Vec<WorkflowPort>,
    #[serde(default)]
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
pub enum NodeCategory {
    Workspace,
    Source,
    Media,
    Viewer,
    Calibration,
    Control,
    Diagnostics,
}

impl Default for NodeCategory {
    fn default() -> Self {
        Self::Diagnostics
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NodeKind {
    LocalFileSource,
    SftpFileSource,
    RtspSource,
    SshSession,
    #[serde(alias = "x5Device")]
    X5233Driver,
    HexArmDevice,
    RtspDecoder,
    Demosaic,
    FrameSampler,
    ImageLayer,
    VideoLayer,
    OverlayComposer,
    Viewer,
    ChessboardDetector,
    GainScorer,
    CaptureGate,
    DatasetCollector,
    CoverageAnalyzer,
    AutoCaptureController,
    CalibrationSolver,
    PoseGuide,
    I2cTransfer,
    EepromProvision,
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
    pub kind: PortKind,
    pub schema: String,
    /// 图像格式提示只用于工作流编排和 UI 呈现；数据契约仍统一为 `image.frame`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_hint: Option<String>,
    #[serde(default)]
    pub role: Option<PortRole>,
    #[serde(default = "default_required")]
    pub required: bool,
    #[serde(default)]
    pub cardinality: PortCardinality,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PortDirection {
    Input,
    Output,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PortKind {
    #[serde(rename = "workspace.local")]
    WorkspaceLocal,
    #[serde(rename = "workspace.remote.sftp")]
    WorkspaceRemoteSftp,
    #[serde(rename = "file.ref")]
    FileRef,
    #[serde(rename = "control.ssh")]
    ControlSsh,
    #[serde(rename = "control.x5tcp")]
    ControlX5Tcp,
    #[serde(rename = "control.hexarm")]
    ControlHexArm,
    #[serde(rename = "endpoint.rtsp")]
    EndpointRtsp,
    #[serde(rename = "stream.encoded-video")]
    StreamEncodedVideo,
    #[serde(rename = "stream.video-frame")]
    StreamVideoFrame,
    #[serde(rename = "image.frame")]
    ImageFrame,
    #[serde(rename = "layer.image")]
    LayerImage,
    #[serde(rename = "layer.video")]
    LayerVideo,
    #[serde(rename = "layer.overlay")]
    LayerOverlay,
    #[serde(rename = "viewer.scene")]
    ViewerScene,
    #[serde(rename = "calib.detection")]
    CalibDetection,
    #[serde(rename = "calib.coverage")]
    CalibCoverage,
    #[serde(rename = "calib.dataset")]
    CalibDataset,
    #[serde(rename = "calib.solution")]
    CalibSolution,
    #[serde(rename = "calib.report")]
    CalibReport,
    #[serde(rename = "capture.score")]
    CaptureScore,
    #[serde(rename = "capture.target")]
    CaptureTarget,
    #[serde(rename = "command.capture")]
    CommandCapture,
    #[serde(rename = "i2c.bus")]
    I2cBus,
    #[serde(rename = "i2c.transfer")]
    I2cTransfer,
    #[serde(rename = "i2c.result")]
    I2cResult,
    #[serde(rename = "eeprom.map")]
    EepromMap,
    #[serde(rename = "eeprom.payload")]
    EepromPayload,
    #[serde(rename = "status.metrics")]
    StatusMetrics,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PortRole {
    Workspace,
    Endpoint,
    Stream,
    Image,
    Layer,
    Overlay,
    Control,
    Status,
    Dataset,
    Solution,
    Command,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PortCardinality {
    One,
    Many,
}

impl Default for PortCardinality {
    fn default() -> Self {
        Self::One
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEdge {
    pub id: String,
    pub source: PortEndpoint,
    pub target: PortEndpoint,
    pub kind: PortKind,
    pub schema: String,
    pub schema_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PortEndpoint {
    pub node_id: String,
    pub port_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NodeDefinition {
    pub kind: NodeKind,
    pub category: NodeCategory,
    pub title: &'static str,
    pub description: &'static str,
    pub inputs: Vec<WorkflowPort>,
    pub outputs: Vec<WorkflowPort>,
    pub default_config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkmodeTemplate {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub graph: WorkflowGraph,
}

fn default_required() -> bool {
    true
}

pub fn node_catalog() -> Vec<NodeDefinition> {
    let catalog = vec![
        node_definition(NodeKind::LocalFileSource),
        node_definition(NodeKind::SftpFileSource),
        node_definition(NodeKind::RtspSource),
        node_definition(NodeKind::SshSession),
        node_definition(NodeKind::X5233Driver),
        node_definition(NodeKind::RtspDecoder),
        node_definition(NodeKind::Demosaic),
        node_definition(NodeKind::FrameSampler),
        node_definition(NodeKind::ImageLayer),
        node_definition(NodeKind::VideoLayer),
        node_definition(NodeKind::Viewer),
        node_definition(NodeKind::ChessboardDetector),
        node_definition(NodeKind::GainScorer),
        node_definition(NodeKind::CaptureGate),
        node_definition(NodeKind::DatasetCollector),
        node_definition(NodeKind::CoverageAnalyzer),
        node_definition(NodeKind::AutoCaptureController),
        node_definition(NodeKind::CalibrationSolver),
        node_definition(NodeKind::PoseGuide),
        node_definition(NodeKind::I2cTransfer),
        node_definition(NodeKind::EepromProvision),
    ];
    // 默认构建不会向画布暴露无法实例化的硬件控制节点；启用功能后再加入目录。
    #[cfg(feature = "hex-arm-control")]
    {
        let mut catalog = catalog;
        catalog.push(node_definition(NodeKind::HexArmDevice));
        catalog
    }
    #[cfg(not(feature = "hex-arm-control"))]
    {
        catalog
    }
}

pub fn node_definition(kind: NodeKind) -> NodeDefinition {
    let (category, title, description, inputs, outputs, default_config) = match kind {
        // 本地图片源：root 为绝对目录，selection 是相对 root 的完整文件路径。
        NodeKind::LocalFileSource => (
            NodeCategory::Workspace,
            "Local File Source",
            "浏览本地绝对根目录并手动加载一张 PNG/JPEG 图片",
            vec![],
            vec![
                port(
                    "image",
                    "Image",
                    PortDirection::Output,
                    PortKind::ImageFrame,
                    "image.frame.v1",
                    Some(PortRole::Image),
                ),
                optional_port(
                    "preview",
                    "Preview",
                    PortDirection::Output,
                    PortKind::ImageFrame,
                    "image.frame.v1",
                    Some(PortRole::Image),
                ),
                optional_port(
                    "fileRef",
                    "File Ref",
                    PortDirection::Output,
                    PortKind::FileRef,
                    "file.ref.v1",
                    Some(PortRole::Image),
                ),
            ],
            json!({"root": "", "directory": "", "selection": ""}),
        ),
        // SFTP 文件源直接读取一张远端图片；SSH/workspace/fileRef 端口均无运行时实现。
        NodeKind::SftpFileSource => (
            NodeCategory::Workspace,
            "SFTP File Source",
            "配置 SFTP 连接与远端文件，手动加载一张 PNG/JPEG 图片",
            vec![],
            vec![port(
                "image",
                "Image",
                PortDirection::Output,
                PortKind::ImageFrame,
                "image.frame.v1",
                Some(PortRole::Image),
            )],
            json!({"host": "", "port": "22", "username": "root", "credentialRef": "", "expectedHostKey": "", "remoteRoot": "/", "selection": ""}),
        ),
        NodeKind::RtspSource => (
            NodeCategory::Source,
            "RTSP Input",
            "连接 RTSP 并直接输出已解码视频帧；Capture Request 可输出当前缓存快照",
            vec![optional_port(
                "capture",
                "Capture Request",
                PortDirection::Input,
                PortKind::CommandCapture,
                "command.capture.request.v1",
                Some(PortRole::Command),
            )],
            vec![
                port(
                    "frames",
                    "Decoded Video Frames",
                    PortDirection::Output,
                    PortKind::StreamVideoFrame,
                    "stream.video-frame.v1",
                    Some(PortRole::Stream),
                ),
                image_port("snapshot", "Snapshot", PortDirection::Output, "Rgba8"),
            ],
            json!({"url": DEFAULT_RTSP_URL, "transport": "tcp", "channel": 0, "width": 1920, "height": 1080, "connectTimeoutMs": 8000, "idleTimeoutMs": 10000}),
        ),
        NodeKind::SshSession => (
            NodeCategory::Control,
            "SSH Session",
            "密码认证的远程命令会话；密码仅保留在当前服务端进程",
            vec![],
            vec![optional_port(
                "result",
                "Command Result",
                PortDirection::Output,
                PortKind::I2cResult,
                "i2c.result.v1",
                Some(PortRole::Status),
            )],
            json!({"profileId": "", "host": "", "port": "22", "username": "root", "credentialRef": "", "expectedHostKey": "", "recipeId": "", "autoConnect": false}),
        ),
        // X5_233 Driver 的图端口是运行时实现的正式契约；手动快照和 capture 输入必须收敛到同一路径。
        NodeKind::X5233Driver => (
            NodeCategory::Control,
            "X5_233 Driver",
            "X5_233 TCP driver: multi-channel video, NV12/RAW capture, and status output",
            vec![optional_port(
                "capture",
                "Capture Request",
                PortDirection::Input,
                PortKind::CommandCapture,
                "command.capture.request.v1",
                Some(PortRole::Command),
            )],
            vec![
                format_hint_port(
                    port(
                        "videoCh0",
                        "Video CH0",
                        PortDirection::Output,
                        PortKind::StreamVideoFrame,
                        "stream.video-frame.v1",
                        Some(PortRole::Stream),
                    ),
                    "Rgba8",
                ),
                format_hint_port(
                    port(
                        "videoCh3",
                        "Video CH3",
                        PortDirection::Output,
                        PortKind::StreamVideoFrame,
                        "stream.video-frame.v1",
                        Some(PortRole::Stream),
                    ),
                    "Rgba8",
                ),
                image_port("yuvCh0", "YUV CH0", PortDirection::Output, "Nv12"),
                image_port("yuvCh3", "YUV CH3", PortDirection::Output, "Nv12"),
                image_port("rawCam0", "RAW CAM0", PortDirection::Output, "BayerRaw"),
                image_port("rawCam1", "RAW CAM1", PortDirection::Output, "BayerRaw"),
                port(
                    "status",
                    "Status",
                    PortDirection::Output,
                    PortKind::StatusMetrics,
                    "status.metrics.v1",
                    Some(PortRole::Status),
                ),
            ],
            json!({
                "host": "10.21.12.108",
                "tcpPort": 9073,
                "rtspChannel": 0,
                "fps": 60,
                "bitrateKbps": 12000,
                "snapshotChannel": 0,
                "snapshotMode": "latest",
                "snapshotFrameId": "",
                "snapshotTimestampNs": "",
                "snapshotRtspPts90k": "",
                "snapshotRtspPtsTolerance90k": 0,
                "rawCamera": 0,
                "rawBayerPattern": "rggb",
                "rawBitsPerSample": 12
            }),
        ),
        // Hex Arm 只声明实际存在的独立控制面；它不在图内伪造帧或状态数据包。
        NodeKind::HexArmDevice => (
            NodeCategory::Control,
            "Hex Arm Device",
            "WebSocket protobuf control for Hex Arm; motion remains disabled until explicitly enabled",
            vec![],
            vec![],
            json!({
                "host": "127.0.0.1",
                "port": 8439,
                "transport": "websocket",
                "controlEnabled": false,
                "commandTimeoutMs": 200,
                "connectTimeoutMs": 3000,
                "jointPositions": "0,0,0,0,0,0"
            }),
        ),
        NodeKind::RtspDecoder => (
            NodeCategory::Media,
            "Decoded Frame Relay",
            "兼容旧流程的已解码帧直通节点；RTSP Input 已完成连接和解码，不执行二次解码",
            vec![port(
                "frames",
                "Decoded Video Frames",
                PortDirection::Input,
                PortKind::StreamVideoFrame,
                "stream.video-frame.v1",
                Some(PortRole::Stream),
            )],
            vec![port(
                "frames",
                "Video Frames",
                PortDirection::Output,
                PortKind::StreamVideoFrame,
                "stream.video-frame.v1",
                Some(PortRole::Stream),
            )],
            json!({}),
        ),
        NodeKind::Demosaic => (
            NodeCategory::Media,
            "Demosaic",
            "显式 BayerRaw → Rgba8 转换；RAW 进入 Viewer/Detector 前必须经过该节点",
            vec![format_hint_port(
                port(
                    "raw",
                    "Bayer RAW",
                    PortDirection::Input,
                    PortKind::ImageFrame,
                    "image.frame.v1",
                    Some(PortRole::Image),
                ),
                "BayerRaw",
            )],
            vec![format_hint_port(
                port(
                    "image",
                    "Demosaiced Image",
                    PortDirection::Output,
                    PortKind::ImageFrame,
                    "image.frame.v1",
                    Some(PortRole::Image),
                ),
                "Rgba8",
            )],
            json!({"algorithm": "bilinear", "outputFormat": "rgba"}),
        ),
        NodeKind::FrameSampler => (
            NodeCategory::Media,
            "Frame Sampler",
            "按 `fpsLimit` 降采样视频帧；固定使用 latest-wins 丢帧策略",
            vec![port(
                "input",
                "Video Frames",
                PortDirection::Input,
                PortKind::StreamVideoFrame,
                "stream.video-frame.v1",
                Some(PortRole::Stream),
            )],
            vec![port(
                "frames",
                "Sampled Frames",
                PortDirection::Output,
                PortKind::StreamVideoFrame,
                "stream.video-frame.v1",
                Some(PortRole::Stream),
            )],
            json!({"fpsLimit": 30}),
        ),
        NodeKind::ImageLayer => (
            NodeCategory::Viewer,
            "Image Layer",
            "原样转发 Rgba8/Gray8/Gray16Le/Nv12 图像；BayerRaw 必须先经过显式 Demosaic，visible/opacity 仅保存声明",
            vec![format_hint_port(
                port(
                    "image",
                    "Image",
                    PortDirection::Input,
                    PortKind::ImageFrame,
                    "image.frame.v1",
                    Some(PortRole::Image),
                ),
                "Rgba8 | Gray8 | Gray16Le | Nv12",
            )],
            vec![format_hint_port(
                port(
                    "layer",
                    "Image Layer",
                    PortDirection::Output,
                    PortKind::LayerImage,
                    "viewer.layer.image.v1",
                    Some(PortRole::Layer),
                ),
                "Rgba8 | Gray8 | Gray16Le | Nv12",
            )],
            json!({"visible": true, "opacity": 1.0}),
        ),
        NodeKind::VideoLayer => (
            NodeCategory::Viewer,
            "Video Layer",
            "声明视频图层元数据并原样转发；visible/opacity 当前不参与合成",
            vec![port(
                "frames",
                "Video Frames",
                PortDirection::Input,
                PortKind::StreamVideoFrame,
                "stream.video-frame.v1",
                Some(PortRole::Stream),
            )],
            vec![port(
                "layer",
                "Video Layer",
                PortDirection::Output,
                PortKind::LayerVideo,
                "viewer.layer.video.v1",
                Some(PortRole::Layer),
            )],
            json!({"visible": true, "opacity": 1.0}),
        ),
        NodeKind::OverlayComposer => (
            NodeCategory::Viewer,
            "Overlay Composer (legacy pass-through)",
            "过渡性 fan-in 节点：仅把 video/image/overlay 负载原样转发到 scene；当前默认模板不再依赖它",
            vec![
                many_port(
                    "video",
                    "Video Layers (not composited)",
                    PortDirection::Input,
                    PortKind::LayerVideo,
                    "viewer.layer.video.v1",
                    Some(PortRole::Layer),
                ),
                many_port(
                    "image",
                    "Image Layers",
                    PortDirection::Input,
                    PortKind::LayerImage,
                    "viewer.layer.image.v1",
                    Some(PortRole::Layer),
                ),
                many_port(
                    "overlay",
                    "Overlay JSON",
                    PortDirection::Input,
                    PortKind::LayerOverlay,
                    "viewer.layer.overlay.v1",
                    Some(PortRole::Overlay),
                ),
            ],
            vec![port(
                "scene",
                "Pass-through Scene",
                PortDirection::Output,
                PortKind::ViewerScene,
                "viewer.scene.v1",
                Some(PortRole::Layer),
            )],
            json!({}),
        ),
        NodeKind::Viewer => (
            NodeCategory::Viewer,
            "Viewer",
            "直接预览 video/image/overlay；BayerRaw 必须经显式 Demosaic。设备侧 RAW/YUV 采集由 X5_233 Driver 触发",
            vec![
                format_hint_port(
                    optional_port(
                        "video",
                        "Video Frames",
                        PortDirection::Input,
                        PortKind::StreamVideoFrame,
                        "stream.video-frame.v1",
                        Some(PortRole::Stream),
                    ),
                    "Rgba8",
                ),
                format_hint_port(
                    optional_port(
                        "image",
                        "Image",
                        PortDirection::Input,
                        PortKind::ImageFrame,
                        "image.frame.v1",
                        Some(PortRole::Image),
                    ),
                    "Rgba8 | Gray8 | Gray16Le | Nv12",
                ),
                many_port(
                    "overlay",
                    "Overlay JSON",
                    PortDirection::Input,
                    PortKind::LayerOverlay,
                    "viewer.layer.overlay.v1",
                    Some(PortRole::Overlay),
                ),
            ],
            vec![],
            json!({}),
        ),
        NodeKind::ChessboardDetector => (
            NodeCategory::Calibration,
            "Chessboard Detector",
            "输入帧驱动的棋盘格检测，输出 detection 和 overlay",
            vec![
                optional_port(
                    "image",
                    "Image",
                    PortDirection::Input,
                    PortKind::ImageFrame,
                    "image.frame.v1",
                    Some(PortRole::Image),
                ),
                optional_port(
                    "frames",
                    "Video Frames",
                    PortDirection::Input,
                    PortKind::StreamVideoFrame,
                    "stream.video-frame.v1",
                    Some(PortRole::Stream),
                ),
            ],
            vec![
                port(
                    "detection",
                    "Detection",
                    PortDirection::Output,
                    PortKind::CalibDetection,
                    "calib.detection.v1",
                    Some(PortRole::Dataset),
                ),
                port(
                    "overlay",
                    "Overlay",
                    PortDirection::Output,
                    PortKind::LayerOverlay,
                    "viewer.layer.overlay.v1",
                    Some(PortRole::Overlay),
                ),
            ],
            json!({"boardRows": 11, "boardCols": 8, "squareSizeMm": 30.0}),
        ),
        NodeKind::GainScorer => (
            NodeCategory::Calibration,
            "Gain Scorer",
            "将棋盘角点完整度归一化为带原始帧身份的 capture score",
            vec![port(
                "detection",
                "Detection",
                PortDirection::Input,
                PortKind::CalibDetection,
                "calib.detection.v1",
                Some(PortRole::Dataset),
            )],
            vec![port(
                "score",
                "Capture Score",
                PortDirection::Output,
                PortKind::CaptureScore,
                "capture.score.v1",
                Some(PortRole::Status),
            )],
            json!({"expectedCorners": 88}),
        ),
        NodeKind::CaptureGate => (
            NodeCategory::Calibration,
            "Capture Gate",
            "达到最小 gain 并连续稳定 holdFrames 帧后，构造带来源身份的 Capture Request",
            vec![port(
                "score",
                "Capture Score",
                PortDirection::Input,
                PortKind::CaptureScore,
                "capture.score.v1",
                Some(PortRole::Status),
            )],
            vec![port(
                "capture",
                "Capture Request",
                PortDirection::Output,
                PortKind::CommandCapture,
                "command.capture.request.v1",
                Some(PortRole::Command),
            )],
            json!({"minimumGain": 0.4, "holdFrames": 3, "mode": "latest", "channel": 0, "camera": 0, "target": "yuv", "rtspPtsTolerance90k": 0}),
        ),
        NodeKind::DatasetCollector => (
            NodeCategory::Calibration,
            "Dataset Collector",
            "累积检测结果；手动输出或清空 calib.dataset",
            vec![port(
                "detection",
                "Detection",
                PortDirection::Input,
                PortKind::CalibDetection,
                "calib.detection.v1",
                Some(PortRole::Dataset),
            )],
            vec![port(
                "dataset",
                "Dataset",
                PortDirection::Output,
                PortKind::CalibDataset,
                "calib.dataset.v1",
                Some(PortRole::Dataset),
            )],
            json!({"maxSamples": 80}),
        ),
        NodeKind::CoverageAnalyzer => (
            NodeCategory::Calibration,
            "Coverage Analyzer",
            "按棋盘中心落点统计图像栅格覆盖度，输出可复核 coverage 与 overlay",
            vec![port(
                "dataset",
                "Dataset",
                PortDirection::Input,
                PortKind::CalibDataset,
                "calib.dataset.v1",
                Some(PortRole::Dataset),
            )],
            vec![
                port(
                    "coverage",
                    "Coverage",
                    PortDirection::Output,
                    PortKind::CalibCoverage,
                    "calib.coverage.v1",
                    Some(PortRole::Dataset),
                ),
                port(
                    "overlay",
                    "Overlay",
                    PortDirection::Output,
                    PortKind::LayerOverlay,
                    "viewer.layer.overlay.v1",
                    Some(PortRole::Overlay),
                ),
            ],
            json!({"gridCols": 6, "gridRows": 4}),
        ),
        NodeKind::AutoCaptureController => (
            NodeCategory::Calibration,
            "Auto Capture",
            "布防后仅依据 capture.score 的 gain 阈值发送 capture command；不生成评分或采帧",
            vec![port(
                "score",
                "Capture Score",
                PortDirection::Input,
                PortKind::CaptureScore,
                "capture.score.v1",
                Some(PortRole::Status),
            )],
            vec![port(
                "command",
                "Capture Command",
                PortDirection::Output,
                PortKind::CommandCapture,
                "command.capture.v1",
                Some(PortRole::Command),
            )],
            json!({"triggerThreshold": 0.5, "cooldownMs": 800}),
        ),
        NodeKind::CalibrationSolver => (
            NodeCategory::Calibration,
            "Calibration Solver",
            "使用最近一次 calib.dataset 检测点；手动触发后输出 calib.solution",
            vec![port(
                "dataset",
                "Dataset",
                PortDirection::Input,
                PortKind::CalibDataset,
                "calib.dataset.v1",
                Some(PortRole::Dataset),
            )],
            vec![port(
                "solution",
                "Solution",
                PortDirection::Output,
                PortKind::CalibSolution,
                "calib.solution.v1",
                Some(PortRole::Solution),
            )],
            json!({
                "boardCols": 8,
                "boardRows": 11,
                "squareSizeMm": 30.0,
                "imageWidth": 1920,
                "imageHeight": 1080,
                "fx": 1234.56,
                "fy": 1234.56,
                "cx": 960.0,
                "cy": 540.0,
            }),
        ),
        NodeKind::PoseGuide => (
            NodeCategory::Calibration,
            "Pose Guide",
            "根据 coverage 的未覆盖图像栅格给出下一帧目标，不等同于相机 6DoF 位姿",
            vec![port(
                "coverage",
                "Coverage",
                PortDirection::Input,
                PortKind::CalibCoverage,
                "calib.coverage.v1",
                Some(PortRole::Dataset),
            )],
            vec![
                port(
                    "target",
                    "Image-grid Target",
                    PortDirection::Output,
                    PortKind::CaptureTarget,
                    "capture.target.v1",
                    Some(PortRole::Command),
                ),
                port(
                    "overlay",
                    "Overlay",
                    PortDirection::Output,
                    PortKind::LayerOverlay,
                    "viewer.layer.overlay.v1",
                    Some(PortRole::Overlay),
                ),
            ],
            json!({"enabled": true}),
        ),
        // I²C 的 SSH/bus 均为节点内联配置；不声明引擎未消费的控制输入或虚假响应输出。
        NodeKind::I2cTransfer => (
            NodeCategory::Control,
            "I²C Transfer",
            "使用节点内联 SSH 配置预览并手动执行 I²C 读写请求",
            vec![],
            vec![port(
                "result",
                "Result",
                PortDirection::Output,
                PortKind::I2cResult,
                "i2c.result.v1",
                Some(PortRole::Status),
            )],
            json!({"profileId": "x5-lab", "host": "", "port": "22", "username": "root", "credentialRef": "", "expectedHostKey": "", "bus": "i2c-1", "address": "0x50", "register": "0x0000", "payload": "", "pageSize": 16, "mode": "read", "confirmWrites": true}),
        ),
        // EEPROM map/payload/bus 都由节点内联配置，避免展示无运行时实现的输入/输出端口。
        NodeKind::EepromProvision => (
            NodeCategory::Control,
            "EEPROM Provision",
            "使用节点内联 SSH、map、payload 和 bus 配置执行 EEPROM 检查与写入",
            vec![],
            vec![port(
                "result",
                "Result",
                PortDirection::Output,
                PortKind::I2cResult,
                "i2c.result.v1",
                Some(PortRole::Status),
            )],
            json!({"profileId": "x5-lab", "host": "", "port": "22", "username": "root", "credentialRef": "", "expectedHostKey": "", "bus": "i2c-1", "address": "0x50", "register": "0x0010", "payload": "", "pageSize": 32, "mapId": "yg-stereo-p24c64g-v1", "verifyAfterWrite": true}),
        ),
    };
    NodeDefinition {
        kind,
        category,
        title,
        description,
        inputs,
        outputs,
        default_config,
    }
}

pub fn workmode_templates() -> Vec<WorkmodeTemplate> {
    vec![
        WorkmodeTemplate {
            id: "x5233-preview",
            title: "X5_233 Preview",
            description: "X5_233 Driver YUV snapshot → Viewer image (image.frame.v1/Nv12)",
            graph: x5233_preview_template_graph(),
        },
        WorkmodeTemplate {
            id: "rtsp-snapshot",
            title: "RTSP Snapshot",
            description: "RTSP Input video/snapshot → Viewer direct video/image",
            graph: rtsp_snapshot_template_graph(),
        },
        WorkmodeTemplate {
            id: "calibration",
            title: "Calibration Capture",
            description: "RTSP Detection → Gain Scorer → Capture Gate → X5_233 capture request",
            graph: calibration_template_graph(),
        },
        WorkmodeTemplate {
            id: "x5233-yuv-capture",
            title: "X5_233 YUV Capture",
            description: "Authoritative NV12 capture request path with frame identity preserved",
            graph: x5233_yuv_template_graph(),
        },
        WorkmodeTemplate {
            id: "x5233-raw-diagnostic",
            title: "X5_233 RAW Diagnostic",
            description: "X5_233 Driver BayerRaw diagnostic output; add explicit Demosaic before viewing",
            graph: x5233_raw_template_graph(),
        },
        WorkmodeTemplate {
            id: "local-image",
            title: "Local Image",
            description: "Local File Source → ImageFrame → Viewer",
            graph: local_image_template_graph(),
        },
        WorkmodeTemplate {
            id: "i2c-tools",
            title: "I²C Tools",
            description: "Inline SSH config → I²C Transfer / EEPROM Provision",
            graph: i2c_template_graph(),
        },
    ]
}

/// 生成默认演示图；使用 X5_233 统一 `image.frame.v1` 预览链路。
pub fn seed_workflow_graph() -> WorkflowGraph {
    viewer_template_graph()
}

pub fn validate_workflow(graph: &WorkflowGraph) -> Result<(), String> {
    if graph.schema_version != WORKFLOW_SCHEMA_VERSION {
        return Err(format!(
            "unsupported workflow schema `{}`",
            graph.schema_version
        ));
    }
    let hex_arm_count = graph
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::HexArmDevice)
        .count();
    if hex_arm_count > 1 {
        return Err(
            "workflow may contain at most one hexArmDevice; the shared control session is single-owner"
                .to_owned(),
        );
    }
    for node in &graph.nodes {
        validate_node_config(node)?;
    }
    for edge in &graph.edges {
        validate_edge(graph, edge)?;
    }
    validate_cardinality_constraints(graph)?;
    Ok(())
}

/// D7 连接约束：统计每条边目标 `(nodeId, portId)` 的入边数，`cardinality=One` 的输入端口
/// 超过 1 条入边即拒绝（允许 `Many` 多路 fan-in；输出端口 fan-out 不限）。
///
/// 与引擎侧 [`GraphEngine::build`/`add_edge`]（crates/app）的校验呼应：前端保存/装载路径先由
/// 此处拦截，引擎增量路径再由 `add_edge` 兜底。
pub fn validate_cardinality_constraints(graph: &WorkflowGraph) -> Result<(), String> {
    use std::collections::HashMap;
    // target (nodeId, portId) → 已见入边 id 列表。
    let mut incoming: HashMap<(String, String), Vec<&str>> = HashMap::new();
    for edge in &graph.edges {
        incoming
            .entry((edge.target.node_id.clone(), edge.target.port_id.clone()))
            .or_default()
            .push(&edge.id);
    }
    for node in &graph.nodes {
        for port in &node.inputs {
            if port.cardinality != PortCardinality::One {
                continue;
            }
            let Some(edges) = incoming.get(&(node.id.clone(), port.id.clone())) else {
                continue;
            };
            if edges.len() > 1 {
                return Err(format!(
                    "input port `{}` on node `{}` has cardinality=One but {} incoming edges: {}",
                    port.id,
                    node.id,
                    edges.len(),
                    edges.join(", ")
                ));
            }
        }
    }
    Ok(())
}

/// 标准化保存前的工作流图：修正 schema/revision，并拒绝运行时字段进入持久化文件。
pub fn normalize_workflow(
    mut graph: WorkflowGraph,
    revision: String,
) -> Result<WorkflowGraph, String> {
    graph.schema_version = WORKFLOW_SCHEMA_VERSION.to_owned();
    graph.revision = revision;
    normalize_source_contracts(&mut graph);
    for node in &mut graph.nodes {
        reject_runtime_config(&node.config)?;
        let definition = node_definition(node.kind);
        node.category = definition.category;
        node.inputs = definition.inputs;
        node.outputs = definition.outputs;
    }
    drop_invalid_legacy_x5233_edges(&mut graph);
    validate_workflow(&graph)?;
    Ok(graph)
}

/// 旧 X5 图的端口在标准化后可能已不存在或类型已变化；只丢弃这些失效边，避免破坏其他校验路径。
fn drop_invalid_legacy_x5233_edges(graph: &mut WorkflowGraph) {
    let edges = std::mem::take(&mut graph.edges);
    graph.edges = edges
        .into_iter()
        .filter(|edge| {
            let touches_x5233 = graph.nodes.iter().any(|node| {
                node.kind == NodeKind::X5233Driver
                    && (node.id == edge.source.node_id || node.id == edge.target.node_id)
            });
            !touches_x5233 || validate_edge(graph, edge).is_ok()
        })
        .collect();
}
/// 将旧图收敛到 source 已直接输出解码帧的当前契约，避免保存后断开已有 RTSP 连线。
fn normalize_source_contracts(graph: &mut WorkflowGraph) {
    for node in &mut graph.nodes {
        let Some(config) = node.config.as_object_mut() else {
            continue;
        };
        match node.kind {
            NodeKind::LocalFileSource => {
                let directory = config_string(config, "directory")
                    .unwrap_or_default()
                    .trim_matches('/')
                    .to_owned();
                let selection = config
                    .get("selection")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                if let Some(selection) = selection {
                    let selection = selection.trim_start_matches('/');
                    if !directory.is_empty()
                        && !selection.is_empty()
                        && !selection.starts_with(&format!("{directory}/"))
                    {
                        config.insert(
                            "selection".to_owned(),
                            json!(format!("{directory}/{selection}")),
                        );
                    }
                }
                config.remove("filter");
                config.remove("reload");
            }
            NodeKind::SftpFileSource => {
                config.remove("sourceId");
                config.remove("mountLabel");
                config.remove("filter");
            }
            NodeKind::RtspSource => {
                for (legacy, current) in [("expectedWidth", "width"), ("expectedHeight", "height")]
                {
                    if let Some(value) = config.remove(legacy) {
                        config.entry(current.to_owned()).or_insert(value);
                    }
                }
                config.remove("expectedFps");
                config
                    .entry("channel".to_owned())
                    .or_insert_with(|| json!(0));
            }
            NodeKind::X5233Driver => {
                normalize_x5233_driver_config(config);
                node.title = "X5_233 Driver".to_owned();
            }
            NodeKind::HexArmDevice => normalize_hex_arm_device_config(config),
            NodeKind::RtspDecoder => {
                config.clear();
                if node.title == "RTSP Decoder" {
                    node.title = "Decoded Frame Relay".to_owned();
                }
            }
            _ => {}
        }
    }

    // SSH/I²C/EEPROM 控制端口从未参与引擎数据包流。节点已改为内联配置，保存旧图时删除
    // 这些虚假连线，避免标准化后引用不存在的端口。
    graph.edges.retain(|edge| {
        !graph.nodes.iter().any(|node| {
            (node.kind == NodeKind::SshSession
                && node.id == edge.source.node_id
                && edge.source.port_id == "ssh")
                || (matches!(node.kind, NodeKind::I2cTransfer | NodeKind::EepromProvision)
                    && node.id == edge.target.node_id)
        })
    });

    // 旧 X5 图可能包含当前契约不存在的端口。只移除这些失效边，保留已迁移图上的有效 capture
    // 输入与多通道输出连接。
    let x5233_driver = node_definition(NodeKind::X5233Driver);
    graph.edges.retain(|edge| {
        graph
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::X5233Driver)
            .all(|node| {
                (node.id != edge.source.node_id
                    || x5233_driver
                        .outputs
                        .iter()
                        .any(|port| port.id == edge.source.port_id))
                    && (node.id != edge.target.node_id
                        || x5233_driver
                            .inputs
                            .iter()
                            .any(|port| port.id == edge.target.port_id))
            })
    });

    for edge in &mut graph.edges {
        let source_is_rtsp = graph
            .nodes
            .iter()
            .any(|node| node.id == edge.source.node_id && node.kind == NodeKind::RtspSource);
        let target_is_decoder = graph
            .nodes
            .iter()
            .any(|node| node.id == edge.target.node_id && node.kind == NodeKind::RtspDecoder);
        if source_is_rtsp && edge.source.port_id == "endpoint" {
            edge.source.port_id = "frames".to_owned();
            edge.kind = PortKind::StreamVideoFrame;
            edge.schema = "stream.video-frame.v1".to_owned();
        }
        if target_is_decoder && edge.target.port_id == "endpoint" {
            edge.target.port_id = "frames".to_owned();
            edge.kind = PortKind::StreamVideoFrame;
            edge.schema = "stream.video-frame.v1".to_owned();
        }
    }
}
/// 迁移旧的单一 `channel` / `channels` 配置到彼此独立的 RTSP 与快照通道。
fn normalize_x5233_driver_config(config: &mut serde_json::Map<String, serde_json::Value>) {
    let legacy_channel = config
        .remove("channel")
        .or_else(|| {
            config.remove("channels").and_then(|value| {
                value
                    .as_array()
                    .and_then(|channels| channels.first())
                    .cloned()
            })
        })
        .unwrap_or_else(|| json!(0));
    config
        .entry("rtspChannel".to_owned())
        .or_insert_with(|| legacy_channel.clone());
    config
        .entry("snapshotChannel".to_owned())
        .or_insert(legacy_channel);
    config
        .entry("snapshotMode".to_owned())
        .or_insert_with(|| json!("latest"));
    config
        .entry("snapshotRtspPtsTolerance90k".to_owned())
        .or_insert_with(|| json!(0));
    config
        .entry("rawCamera".to_owned())
        .or_insert_with(|| json!(0));
    config
        .entry("rawBayerPattern".to_owned())
        .or_insert_with(|| json!("rggb"));
    config
        .entry("rawBitsPerSample".to_owned())
        .or_insert_with(|| json!(12));
}

/// 未配置的旧 Hex Arm 节点保持不可运动；不为 KCP 等未实现传输路径提供隐式回退。
fn normalize_hex_arm_device_config(config: &mut serde_json::Map<String, serde_json::Value>) {
    config
        .entry("host".to_owned())
        .or_insert_with(|| json!("127.0.0.1"));
    config
        .entry("port".to_owned())
        .or_insert_with(|| json!(8439));
    config
        .entry("transport".to_owned())
        .or_insert_with(|| json!("websocket"));
    config
        .entry("controlEnabled".to_owned())
        .or_insert_with(|| json!(false));
    config
        .entry("commandTimeoutMs".to_owned())
        .or_insert_with(|| json!(200));
    config
        .entry("connectTimeoutMs".to_owned())
        .or_insert_with(|| json!(3000));
    config
        .entry("jointPositions".to_owned())
        .or_insert_with(|| json!("0,0,0,0,0,0"));
}

/// 校验端口方向与类型，避免 UI 产生无法执行的数据流边。
pub fn validate_edge(graph: &WorkflowGraph, edge: &WorkflowEdge) -> Result<(), String> {
    if edge.source.node_id == edge.target.node_id {
        return Err("self-loop connections are not supported".to_owned());
    }

    let source = find_port(graph, &edge.source, PortDirection::Output)?;
    let target = find_port(graph, &edge.target, PortDirection::Input)?;
    if source.kind != target.kind {
        return Err(format!(
            "port kind mismatch: source {:?}, target {:?}",
            source.kind, target.kind
        ));
    }
    if source.kind != edge.kind {
        return Err(format!(
            "edge declares {:?}, but source emits {:?}",
            edge.kind, source.kind
        ));
    }
    if source.schema != edge.schema {
        return Err(format!(
            "edge schema `{}` does not match source schema `{}`",
            edge.schema, source.schema
        ));
    }
    if target.schema != edge.schema {
        return Err(format!(
            "edge schema `{}` does not match target schema `{}`",
            edge.schema, target.schema
        ));
    }
    if edge.schema_version != WORKFLOW_SCHEMA_VERSION {
        return Err(format!(
            "edge schema version `{}` is unsupported",
            edge.schema_version
        ));
    }
    Ok(())
}

fn validate_node_config(node: &WorkflowNode) -> Result<(), String> {
    match node.kind {
        NodeKind::RtspSource => validate_rtsp_source_config(node),
        NodeKind::SshSession | NodeKind::I2cTransfer | NodeKind::EepromProvision => {
            validate_password_ssh_config(node)
        }
        NodeKind::SftpFileSource => validate_sftp_file_source_config(node),
        NodeKind::LocalFileSource => validate_local_file_source_config(node),
        NodeKind::HexArmDevice => validate_hex_arm_device_config(node),
        _ => Ok(()),
    }
}

fn validate_rtsp_source_config(node: &WorkflowNode) -> Result<(), String> {
    let config = node_config_object(node)?;
    if let Some(url) = config_string(config, "url") {
        validate_printable_config_text(node, "url", url)?;
        if !url.starts_with("rtsp://") && !url.starts_with("rtsps://") {
            return Err(format!(
                "node `{}` RTSP url must start with rtsp:// or rtsps://",
                node.id
            ));
        }
    }
    if let Some(transport) = config_string(config, "transport") {
        if !matches!(transport, "tcp" | "udp") {
            return Err(format!(
                "node `{}` config `transport` must be tcp or udp",
                node.id
            ));
        }
    }
    validate_integer_range(node, config, "channel", 0, u16::MAX.into())?;
    validate_integer_range(node, config, "width", 1, 16_384)?;
    validate_integer_range(node, config, "height", 1, 16_384)?;
    validate_timeout_ms(node, config, "connectTimeoutMs")?;
    validate_timeout_ms(node, config, "idleTimeoutMs")?;
    Ok(())
}

/// Hex Arm 传输仅支持 WebSocket；运动关由布尔配置显式保存，jointPositions 仅接受有限弧度数值。
fn validate_hex_arm_device_config(node: &WorkflowNode) -> Result<(), String> {
    let config = node_config_object(node)?;
    let host = config_string(config, "host").ok_or_else(|| {
        format!(
            "node `{}` config `host` must be a non-empty printable host",
            node.id
        )
    })?;
    validate_printable_config_text(node, "host", host)?;
    if host.trim().is_empty() {
        return Err(format!(
            "node `{}` config `host` must be a non-empty printable host",
            node.id
        ));
    }
    validate_integer_range(node, config, "port", 1, u16::MAX.into())?;
    if let Some(transport) = config_string(config, "transport") {
        if transport != "websocket" {
            return Err(format!(
                "node `{}` config `transport` must be websocket; KCP is unsupported",
                node.id
            ));
        }
    }
    if let Some(enabled) = config.get("controlEnabled") {
        if !enabled.is_boolean() {
            return Err(format!(
                "node `{}` config `controlEnabled` must be boolean",
                node.id
            ));
        }
    }
    validate_timeout_ms(node, config, "commandTimeoutMs")?;
    validate_timeout_ms(node, config, "connectTimeoutMs")?;
    let joints = config_string(config, "jointPositions").ok_or_else(|| {
        format!(
            "node `{}` config `jointPositions` must be a non-empty comma-separated finite radians list",
            node.id
        )
    })?;
    let joints = joints.trim();
    if joints.is_empty()
        || joints.split(',').any(|joint| {
            joint
                .trim()
                .parse::<f64>()
                .map_or(true, |radians| !radians.is_finite())
        })
    {
        return Err(format!(
            "node `{}` config `jointPositions` must be a non-empty comma-separated finite radians list",
            node.id
        ));
    }
    Ok(())
}

fn validate_timeout_ms(
    node: &WorkflowNode,
    config: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<(), String> {
    let Some(value) = config.get(key) else {
        return Ok(());
    };
    let ms = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()));
    let Some(ms) = ms else {
        return Err(format!(
            "node `{}` config `{key}` must be milliseconds",
            node.id
        ));
    };
    if ms == 0 || ms > 120_000 {
        return Err(format!(
            "node `{}` config `{key}` must be in 1..=120000 ms",
            node.id
        ));
    }
    Ok(())
}

fn validate_integer_range(
    node: &WorkflowNode,
    config: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    minimum: u64,
    maximum: u64,
) -> Result<(), String> {
    let Some(value) = config.get(key) else {
        return Ok(());
    };
    let number = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()));
    let Some(number) = number else {
        return Err(format!(
            "node `{}` config `{key}` must be an integer",
            node.id
        ));
    };
    if !(minimum..=maximum).contains(&number) {
        return Err(format!(
            "node `{}` config `{key}` must be in {minimum}..={maximum}",
            node.id
        ));
    }
    Ok(())
}

/// SSH Session、I²C 与 EEPROM 只接受密码注册端点生成的进程内 session 引用。
fn validate_password_ssh_config(node: &WorkflowNode) -> Result<(), String> {
    let config = node_config_object(node)?;
    if let Some(host) = config_string(config, "host") {
        validate_printable_config_text(node, "host", host)?;
    }
    if let Some(username) = config_string(config, "username") {
        validate_printable_config_text(node, "username", username)?;
    }
    if let Some(profile_id) = config_string(config, "profileId") {
        validate_printable_config_text(node, "profileId", profile_id)?;
    }
    if let Some(port) = config_string(config, "port") {
        let port = port
            .parse::<u16>()
            .map_err(|_| format!("node `{}` SSH port must be in 1..=65535", node.id))?;
        if port == 0 {
            return Err(format!("node `{}` SSH port must be in 1..=65535", node.id));
        }
    }
    if let Some(reference) = config_string(config, "credentialRef") {
        let reference = reference.trim();
        if !reference.is_empty()
            && (!reference.starts_with("session:")
                || reference.len() == "session:".len()
                || !reference["session:".len()..]
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
        {
            return Err(format!(
                "node `{}` credentialRef must be empty or a password session:<node-id> reference",
                node.id
            ));
        }
    }
    Ok(())
}

fn validate_sftp_file_source_config(node: &WorkflowNode) -> Result<(), String> {
    let config = node_config_object(node)?;
    if let Some(host) = config_string(config, "host") {
        validate_printable_config_text(node, "host", host)?;
    }
    if let Some(username) = config_string(config, "username") {
        validate_printable_config_text(node, "username", username)?;
    }
    if let Some(port) = config_string(config, "port") {
        let port = port
            .parse::<u16>()
            .map_err(|_| format!("node `{}` SFTP port must be in 1..=65535", node.id))?;
        if port == 0 {
            return Err(format!("node `{}` SFTP port must be in 1..=65535", node.id));
        }
    }
    if let Some(remote_root) = config_string(config, "remoteRoot") {
        validate_remote_root(node, remote_root)?;
    }
    if let Some(selection) = config_string(config, "selection") {
        validate_source_relative_path(node, "selection", selection, true)?;
    }
    Ok(())
}

fn validate_local_file_source_config(node: &WorkflowNode) -> Result<(), String> {
    let config = node_config_object(node)?;
    if let Some(root) = config_string(config, "root") {
        let root = root.trim();
        if !root.is_empty() && (!std::path::Path::new(root).is_absolute() || root.contains('\0')) {
            return Err(format!(
                "node `{}` config `root` must be an absolute local directory",
                node.id
            ));
        }
    }
    if let Some(directory) = config_string(config, "directory") {
        validate_source_relative_path(node, "directory", directory, true)?;
    }
    if let Some(selection) = config_string(config, "selection") {
        validate_source_relative_path(node, "selection", selection, true)?;
    }
    Ok(())
}

fn reject_runtime_config(config: &serde_json::Value) -> Result<(), String> {
    const FORBIDDEN_KEYS: &[&str] = &[
        "objectUrl",
        "streamSessionId",
        "decoderFrames",
        "mjpegHeaders",
        "password",
        "privateKey",
        "secret",
    ];

    fn valid_credential_ref(value: &str) -> bool {
        let value = value.trim();
        if value.is_empty() {
            return true;
        }
        if let Some(path) = value.strip_prefix("key-file:") {
            return path.starts_with('/') && path.len() > 1 && !value.contains(['\0', '\r', '\n']);
        }
        if let Some(id) = value.strip_prefix("session:") {
            return !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        }
        false
    }
    // 递归遍历所有嵌套 object 与数组元素，堵住嵌套 secret 的落盘绕过。
    fn walk(value: &serde_json::Value) -> Result<(), String> {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    if key == "credentialRef" {
                        let reference = child.as_str().ok_or_else(|| {
                            "runtime field `credentialRef` must be a safe reference string"
                                .to_owned()
                        })?;
                        if !valid_credential_ref(reference) {
                            return Err("runtime field `credentialRef` must be empty, key-file:/absolute/path, or session:<opaque-id>; secret material is forbidden".to_owned());
                        }
                    } else if FORBIDDEN_KEYS.contains(&key.as_str()) {
                        return Err(format!("runtime field `{key}` must not be persisted"));
                    }
                    walk(child)?;
                }
                Ok(())
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    walk(config)
}

fn node_config_object(
    node: &WorkflowNode,
) -> Result<&serde_json::Map<String, serde_json::Value>, String> {
    node.config
        .as_object()
        .ok_or_else(|| format!("node `{}` config must be a JSON object", node.id))
}

fn config_string<'a>(
    config: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    config.get(key).and_then(serde_json::Value::as_str)
}

fn validate_remote_root(node: &WorkflowNode, value: &str) -> Result<(), String> {
    if !value.starts_with('/')
        || value.contains('\0')
        || value.split('/').any(|component| component == "..")
    {
        return Err(format!(
            "node `{}` config `remoteRoot` must be a safe absolute remote path",
            node.id
        ));
    }
    Ok(())
}

fn validate_source_relative_path(
    node: &WorkflowNode,
    key: &str,
    value: &str,
    allow_empty: bool,
) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return if allow_empty {
            Ok(())
        } else {
            Err(format!(
                "node `{}` config `{key}` must not be empty",
                node.id
            ))
        };
    }
    if trimmed.starts_with('/') || trimmed.contains('\\') || trimmed.contains('\0') {
        return Err(format!(
            "node `{}` config `{key}` must stay source-relative",
            node.id
        ));
    }
    if trimmed
        .split('/')
        .any(|component| component == ".." || component.chars().any(char::is_control))
    {
        return Err(format!(
            "node `{}` config `{key}` must not escape the workspace",
            node.id
        ));
    }
    Ok(())
}

fn validate_printable_config_text(
    node: &WorkflowNode,
    key: &str,
    value: &str,
) -> Result<(), String> {
    if value.chars().any(char::is_control) {
        return Err(format!(
            "node `{}` config `{key}` must not contain control characters",
            node.id
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

fn x5233_preview_template_graph() -> WorkflowGraph {
    let driver = workflow_node(
        "x5233-driver-1",
        NodeKind::X5233Driver,
        "X5_233 Driver",
        NodePosition { x: 80.0, y: 140.0 },
    );
    let viewer = workflow_node(
        "viewer-1",
        NodeKind::Viewer,
        "Viewer",
        NodePosition { x: 520.0, y: 120.0 },
    );
    graph(
        "camera-toolbox-x5233-preview-template",
        "X5_233 Preview Workspace",
        vec![driver, viewer],
        vec![edge(
            "x5233-preview-yuv-viewer",
            "x5233-driver-1",
            "yuvCh0",
            "viewer-1",
            "image",
            PortKind::ImageFrame,
            "image.frame.v1",
        )],
    )
}

fn rtsp_snapshot_template_graph() -> WorkflowGraph {
    let rtsp = workflow_node(
        "rtsp-source-1",
        NodeKind::RtspSource,
        "RTSP Input",
        NodePosition { x: 80.0, y: 140.0 },
    );
    let viewer = workflow_node(
        "viewer-1",
        NodeKind::Viewer,
        "Viewer",
        NodePosition { x: 520.0, y: 120.0 },
    );
    graph(
        "camera-toolbox-rtsp-snapshot-template",
        "RTSP Snapshot Workspace",
        vec![rtsp, viewer],
        vec![
            edge(
                "rtsp-snapshot-video-viewer",
                "rtsp-source-1",
                "frames",
                "viewer-1",
                "video",
                PortKind::StreamVideoFrame,
                "stream.video-frame.v1",
            ),
            edge(
                "rtsp-snapshot-image-viewer",
                "rtsp-source-1",
                "snapshot",
                "viewer-1",
                "image",
                PortKind::ImageFrame,
                "image.frame.v1",
            ),
        ],
    )
}

fn viewer_template_graph() -> WorkflowGraph {
    x5233_preview_template_graph()
}

/// 本地图像模板：LocalFileSource 直接输出统一 ImageFrame，Viewer 直接消费 image。
fn local_image_template_graph() -> WorkflowGraph {
    let source = workflow_node(
        "local-file-source-1",
        NodeKind::LocalFileSource,
        "Local File Source",
        NodePosition { x: 80.0, y: 140.0 },
    );
    let viewer = workflow_node(
        "viewer-1",
        NodeKind::Viewer,
        "Viewer",
        NodePosition { x: 520.0, y: 120.0 },
    );
    graph(
        "camera-toolbox-local-image-template",
        "Local Image Workspace",
        vec![source, viewer],
        vec![edge(
            "local-image-source-viewer",
            "local-file-source-1",
            "image",
            "viewer-1",
            "image",
            PortKind::ImageFrame,
            "image.frame.v1",
        )],
    )
}

fn x5233_yuv_template_graph() -> WorkflowGraph {
    let driver = workflow_node(
        "x5233-driver-yuv",
        NodeKind::X5233Driver,
        "X5_233 Driver",
        NodePosition { x: 80.0, y: 140.0 },
    );
    let viewer = workflow_node(
        "x5233-yuv-viewer",
        NodeKind::Viewer,
        "Viewer",
        NodePosition { x: 520.0, y: 120.0 },
    );
    graph(
        "camera-toolbox-x5233-yuv-capture-template",
        "X5_233 YUV Capture Workspace",
        vec![driver, viewer],
        vec![edge(
            "x5233-yuv-capture-viewer",
            "x5233-driver-yuv",
            "yuvCh0",
            "x5233-yuv-viewer",
            "image",
            PortKind::ImageFrame,
            "image.frame.v1",
        )],
    )
}

fn x5233_raw_template_graph() -> WorkflowGraph {
    let driver = workflow_node(
        "x5233-driver-raw",
        NodeKind::X5233Driver,
        "X5_233 Driver",
        NodePosition { x: 520.0, y: 140.0 },
    );
    let demosaic = workflow_node(
        "x5233-raw-demosaic",
        NodeKind::Demosaic,
        "Demosaic",
        NodePosition { x: 940.0, y: 140.0 },
    );
    let viewer = workflow_node(
        "x5233-raw-viewer",
        NodeKind::Viewer,
        "Viewer",
        NodePosition {
            x: 1260.0,
            y: 120.0,
        },
    );
    let mut gate = workflow_node(
        "x5233-raw-gate",
        NodeKind::CaptureGate,
        "Capture Gate",
        NodePosition { x: 80.0, y: 140.0 },
    );
    gate.config["target"] = json!("raw");
    gate.config["camera"] = json!(0);
    graph(
        "camera-toolbox-x5233-raw-diagnostic-template",
        "X5_233 RAW Diagnostic Workspace",
        vec![gate, driver, demosaic, viewer],
        vec![
            edge(
                "x5233-raw-gate-driver",
                "x5233-raw-gate",
                "capture",
                "x5233-driver-raw",
                "capture",
                PortKind::CommandCapture,
                "command.capture.request.v1",
            ),
            edge(
                "x5233-raw-driver-demosaic",
                "x5233-driver-raw",
                "rawCam0",
                "x5233-raw-demosaic",
                "raw",
                PortKind::ImageFrame,
                "image.frame.v1",
            ),
            edge(
                "x5233-raw-demosaic-viewer",
                "x5233-raw-demosaic",
                "image",
                "x5233-raw-viewer",
                "image",
                PortKind::ImageFrame,
                "image.frame.v1",
            ),
        ],
    )
}

fn calibration_template_graph() -> WorkflowGraph {
    let nodes = vec![
        workflow_node(
            "calib-rtsp-source",
            NodeKind::RtspSource,
            "RTSP Input",
            NodePosition { x: 60.0, y: 140.0 },
        ),
        workflow_node(
            "calib-detector",
            NodeKind::ChessboardDetector,
            "Chessboard Detector",
            NodePosition { x: 360.0, y: 140.0 },
        ),
        workflow_node(
            "calib-gain",
            NodeKind::GainScorer,
            "Gain Scorer",
            NodePosition { x: 660.0, y: 140.0 },
        ),
        workflow_node(
            "calib-gate",
            NodeKind::CaptureGate,
            "Capture Gate",
            NodePosition { x: 960.0, y: 140.0 },
        ),
        workflow_node(
            "calib-x5233-driver",
            NodeKind::X5233Driver,
            "X5_233 Driver",
            NodePosition {
                x: 1260.0,
                y: 140.0,
            },
        ),
        workflow_node(
            "calib-viewer",
            NodeKind::Viewer,
            "Viewer",
            NodePosition { x: 660.0, y: 380.0 },
        ),
    ];
    graph(
        "camera-toolbox-calibration-template",
        "Calibration Capture Workspace",
        nodes,
        vec![
            edge(
                "calib-rtsp-detector",
                "calib-rtsp-source",
                "frames",
                "calib-detector",
                "frames",
                PortKind::StreamVideoFrame,
                "stream.video-frame.v1",
            ),
            edge(
                "calib-rtsp-viewer",
                "calib-rtsp-source",
                "frames",
                "calib-viewer",
                "video",
                PortKind::StreamVideoFrame,
                "stream.video-frame.v1",
            ),
            edge(
                "calib-detection-gain",
                "calib-detector",
                "detection",
                "calib-gain",
                "detection",
                PortKind::CalibDetection,
                "calib.detection.v1",
            ),
            edge(
                "calib-detector-overlay-viewer",
                "calib-detector",
                "overlay",
                "calib-viewer",
                "overlay",
                PortKind::LayerOverlay,
                "viewer.layer.overlay.v1",
            ),
            edge(
                "calib-gain-gate",
                "calib-gain",
                "score",
                "calib-gate",
                "score",
                PortKind::CaptureScore,
                "capture.score.v1",
            ),
            edge(
                "calib-gate-x5233-capture",
                "calib-gate",
                "capture",
                "calib-x5233-driver",
                "capture",
                PortKind::CommandCapture,
                "command.capture.request.v1",
            ),
            edge(
                "calib-rtsp-snapshot-viewer",
                "calib-rtsp-source",
                "snapshot",
                "calib-viewer",
                "image",
                PortKind::ImageFrame,
                "image.frame.v1",
            ),
        ],
    )
}

fn i2c_template_graph() -> WorkflowGraph {
    // I²C 与 EEPROM 都使用节点内联 SSH/bus/map/payload 配置，模板不再伪造 SSH 输入边。
    graph(
        "camera-toolbox-i2c-template",
        "I²C Tools Workspace",
        vec![
            workflow_node(
                "i2c-transfer",
                NodeKind::I2cTransfer,
                "I²C Transfer",
                NodePosition { x: 100.0, y: 80.0 },
            ),
            workflow_node(
                "i2c-eeprom",
                NodeKind::EepromProvision,
                "EEPROM Provision",
                NodePosition { x: 100.0, y: 320.0 },
            ),
        ],
        vec![],
    )
}

fn workflow_node(id: &str, kind: NodeKind, title: &str, position: NodePosition) -> WorkflowNode {
    let definition = node_definition(kind);
    WorkflowNode {
        id: id.to_owned(),
        kind,
        title: title.to_owned(),
        position,
        state: match kind {
            NodeKind::RtspSource
            | NodeKind::SshSession
            | NodeKind::X5233Driver
            | NodeKind::HexArmDevice => NodeRuntimeState::Ready,
            _ => NodeRuntimeState::Idle,
        },
        category: definition.category,
        inputs: definition.inputs,
        outputs: definition.outputs,
        config: definition.default_config,
    }
}

fn graph(
    id: &str,
    title: &str,
    nodes: Vec<WorkflowNode>,
    edges: Vec<WorkflowEdge>,
) -> WorkflowGraph {
    let graph = WorkflowGraph {
        schema_version: WORKFLOW_SCHEMA_VERSION.to_owned(),
        id: id.to_owned(),
        title: title.to_owned(),
        revision: "seed".to_owned(),
        nodes,
        edges,
        viewport: Some(WorkflowViewport {
            x: 0.0,
            y: 0.0,
            zoom: 1.0,
        }),
    };
    debug_assert!(validate_workflow(&graph).is_ok());
    graph
}

fn edge(
    id: &str,
    source_node: &str,
    source_port: &str,
    target_node: &str,
    target_port: &str,
    kind: PortKind,
    schema: &str,
) -> WorkflowEdge {
    WorkflowEdge {
        id: id.to_owned(),
        source: PortEndpoint {
            node_id: source_node.to_owned(),
            port_id: source_port.to_owned(),
        },
        target: PortEndpoint {
            node_id: target_node.to_owned(),
            port_id: target_port.to_owned(),
        },
        kind,
        schema: schema.to_owned(),
        schema_version: WORKFLOW_SCHEMA_VERSION.to_owned(),
    }
}

fn port(
    id: &str,
    label: &str,
    direction: PortDirection,
    kind: PortKind,
    schema: &str,
    role: Option<PortRole>,
) -> WorkflowPort {
    WorkflowPort {
        id: id.to_owned(),
        label: label.to_owned(),
        direction,
        kind,
        schema: schema.to_owned(),
        format_hint: None,
        role,
        required: true,
        cardinality: PortCardinality::One,
    }
}

fn optional_port(
    id: &str,
    label: &str,
    direction: PortDirection,
    kind: PortKind,
    schema: &str,
    role: Option<PortRole>,
) -> WorkflowPort {
    WorkflowPort {
        required: false,
        ..port(id, label, direction, kind, schema, role)
    }
}

fn many_port(
    id: &str,
    label: &str,
    direction: PortDirection,
    kind: PortKind,
    schema: &str,
    role: Option<PortRole>,
) -> WorkflowPort {
    WorkflowPort {
        cardinality: PortCardinality::Many,
        ..port(id, label, direction, kind, schema, role)
    }
}

/// 为非 `image.frame` 图端口保留像素格式契约，不改变端口 kind 或 schema。
fn format_hint_port(port: WorkflowPort, format_hint: &str) -> WorkflowPort {
    WorkflowPort {
        format_hint: Some(format_hint.to_owned()),
        ..port
    }
}

/// 创建带有像素格式提示的统一 `image.frame` 端口。
fn image_port(id: &str, label: &str, direction: PortDirection, format_hint: &str) -> WorkflowPort {
    WorkflowPort {
        format_hint: Some(format_hint.to_owned()),
        ..port(
            id,
            label,
            direction,
            PortKind::ImageFrame,
            "image.frame.v1",
            Some(PortRole::Image),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_graph_is_x5233_preview_with_unified_image_contract() {
        let graph = seed_workflow_graph();
        validate_workflow(&graph).expect("seed graph is valid");
        assert!(
            graph.nodes.iter().any(|node| {
                node.kind == NodeKind::X5233Driver && node.title == "X5_233 Driver"
            })
        );
        assert!(
            graph.edges.iter().any(|edge| {
                edge.kind == PortKind::ImageFrame && edge.schema == "image.frame.v1"
            })
        );
        assert!(
            !graph
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::OverlayComposer)
        );
    }

    #[test]
    fn normalize_migrates_legacy_rtsp_fields_and_ports() {
        let mut graph = rtsp_snapshot_template_graph();
        graph.nodes.push(workflow_node(
            "legacy-decoder",
            NodeKind::RtspDecoder,
            "RTSP Decoder",
            NodePosition { x: 320.0, y: 120.0 },
        ));
        let source = graph
            .nodes
            .iter_mut()
            .find(|node| node.kind == NodeKind::RtspSource)
            .expect("RTSP source exists");
        source.config = json!({
            "url": "rtsp://127.0.0.1:554/test",
            "transport": "tcp",
            "expectedWidth": 1280,
            "expectedHeight": 720,
            "expectedFps": 30,
        });
        let decoder = graph
            .nodes
            .iter_mut()
            .find(|node| node.id == "legacy-decoder")
            .expect("RTSP relay exists");
        decoder.config = json!({"transport": "tcp"});
        let edge = graph.edges.first_mut().expect("source relay edge exists");
        edge.target.node_id = "legacy-decoder".to_owned();
        edge.source.port_id = "endpoint".to_owned();
        edge.target.port_id = "endpoint".to_owned();
        edge.kind = PortKind::EndpointRtsp;
        edge.schema = "media.rtsp.endpoint.v1".to_owned();

        let graph =
            normalize_workflow(graph, "next".to_owned()).expect("legacy source graph migrates");
        let source = graph
            .nodes
            .iter()
            .find(|node| node.kind == NodeKind::RtspSource)
            .expect("normalized RTSP source exists");
        assert_eq!(source.config["width"], json!(1280));
        assert_eq!(source.config["height"], json!(720));
        assert_eq!(source.config["channel"], json!(0));
        assert!(source.config.get("expectedWidth").is_none());
        assert!(source.outputs.iter().any(|port| port.id == "frames"));
        let edge = graph
            .edges
            .first()
            .expect("normalized source relay edge exists");
        assert_eq!(edge.source.port_id, "frames");
        assert_eq!(edge.target.port_id, "frames");
        assert_eq!(edge.kind, PortKind::StreamVideoFrame);
    }
    #[test]
    fn normalize_migrates_legacy_x5_device_to_x5233_driver_contract() {
        let mut graph = seed_workflow_graph();
        let mut serialized = serde_json::to_value(workflow_node(
            "x5-1",
            NodeKind::X5233Driver,
            "X5 Device",
            NodePosition { x: 0.0, y: 0.0 },
        ))
        .expect("serialize legacy X5 fixture");
        serialized["kind"] = json!("x5Device");
        let mut x5: WorkflowNode =
            serde_json::from_value(serialized).expect("saved x5Device graph deserializes");
        x5.config = json!({"host": "10.21.12.108", "channels": [3]});
        x5.outputs = vec![port(
            "snapshot",
            "Snapshot Image",
            PortDirection::Output,
            PortKind::ImageFrame,
            "image.frame.v1",
            Some(PortRole::Image),
        )];
        graph.nodes.push(x5);
        graph.nodes.push(workflow_node(
            "x5-image-layer",
            NodeKind::ImageLayer,
            "X5 Image Layer",
            NodePosition { x: 120.0, y: 0.0 },
        ));
        graph.nodes.push(workflow_node(
            "x5-capture-gate",
            NodeKind::CaptureGate,
            "Capture Gate",
            NodePosition { x: 240.0, y: 0.0 },
        ));
        graph.edges.push(WorkflowEdge {
            id: "obsolete-x5-snapshot".to_owned(),
            source: PortEndpoint {
                node_id: "x5-1".to_owned(),
                port_id: "snapshot".to_owned(),
            },
            target: PortEndpoint {
                node_id: "viewer-1".to_owned(),
                port_id: "video".to_owned(),
            },
            kind: PortKind::ImageFrame,
            schema: "image.frame.v1".to_owned(),
            schema_version: WORKFLOW_SCHEMA_VERSION.to_owned(),
        });
        graph.edges.push(WorkflowEdge {
            id: "x5-yuv-to-image-layer".to_owned(),
            source: PortEndpoint {
                node_id: "x5-1".to_owned(),
                port_id: "yuvCh0".to_owned(),
            },
            target: PortEndpoint {
                node_id: "x5-image-layer".to_owned(),
                port_id: "image".to_owned(),
            },
            kind: PortKind::ImageFrame,
            schema: "image.frame.v1".to_owned(),
            schema_version: WORKFLOW_SCHEMA_VERSION.to_owned(),
        });
        graph.edges.push(edge(
            "x5-capture-request",
            "x5-capture-gate",
            "capture",
            "x5-1",
            "capture",
            PortKind::CommandCapture,
            "command.capture.request.v1",
        ));

        let graph = normalize_workflow(graph, "next".to_owned()).expect("legacy X5 graph migrates");
        let x5 = graph
            .nodes
            .iter()
            .find(|node| node.id == "x5-1")
            .expect("X5 node remains");
        assert_eq!(x5.kind, NodeKind::X5233Driver);
        assert_eq!(x5.title, "X5_233 Driver");
        assert_eq!(x5.config["rtspChannel"], json!(3));
        assert_eq!(x5.config["snapshotChannel"], json!(3));
        assert_eq!(x5.config["snapshotMode"], json!("latest"));
        assert!(x5.inputs.iter().any(|port| {
            port.id == "capture"
                && port.kind == PortKind::CommandCapture
                && port.schema == "command.capture.request.v1"
                && !port.required
        }));
        assert!(x5.outputs.iter().any(|port| {
            port.id == "yuvCh0"
                && port.kind == PortKind::ImageFrame
                && port.format_hint.as_deref() == Some("Nv12")
        }));
        assert!(x5.outputs.iter().any(|port| {
            port.id == "rawCam0"
                && port.kind == PortKind::ImageFrame
                && port.format_hint.as_deref() == Some("BayerRaw")
        }));
        assert!(x5.outputs.iter().any(|port| {
            port.id == "status"
                && port.kind == PortKind::StatusMetrics
                && port.schema == "status.metrics.v1"
        }));
        assert!(
            graph
                .edges
                .iter()
                .all(|edge| edge.id != "obsolete-x5-snapshot")
        );
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.id == "x5-yuv-to-image-layer")
        );
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.id == "x5-capture-request")
        );
    }

    #[test]
    fn x5_device_serde_alias_serializes_as_x5233_driver() {
        let kind: NodeKind = serde_json::from_value(json!("x5Device")).expect("legacy kind parses");
        assert_eq!(kind, NodeKind::X5233Driver);
        assert_eq!(
            serde_json::to_value(kind).expect("kind serializes"),
            json!("x5233Driver")
        );
    }

    #[test]
    fn rtsp_input_declares_optional_capture_and_rgba_snapshot() {
        let rtsp = node_definition(NodeKind::RtspSource);
        assert!(rtsp.inputs.iter().any(|port| {
            port.id == "capture"
                && port.kind == PortKind::CommandCapture
                && port.schema == "command.capture.request.v1"
                && !port.required
        }));
        assert!(rtsp.outputs.iter().any(|port| {
            port.id == "snapshot"
                && port.kind == PortKind::ImageFrame
                && port.format_hint.as_deref() == Some("Rgba8")
        }));
        let snapshot_template = rtsp_snapshot_template_graph();
        assert!(snapshot_template.edges.iter().any(|edge| {
            edge.id == "rtsp-snapshot-image-viewer"
                && edge.kind == PortKind::ImageFrame
                && edge.schema == "image.frame.v1"
        }));
    }

    #[test]
    fn validation_rejects_self_loop() {
        let graph = rtsp_snapshot_template_graph();
        let edge = WorkflowEdge {
            id: "self-loop".to_owned(),
            source: PortEndpoint {
                node_id: "rtsp-source-1".to_owned(),
                port_id: "frames".to_owned(),
            },
            target: PortEndpoint {
                node_id: "rtsp-source-1".to_owned(),
                port_id: "frames".to_owned(),
            },
            kind: PortKind::StreamVideoFrame,
            schema: "stream.video-frame.v1".to_owned(),
            schema_version: WORKFLOW_SCHEMA_VERSION.to_owned(),
        };
        assert!(validate_edge(&graph, &edge).is_err());
    }

    #[test]
    fn validation_rejects_incompatible_port_kinds() {
        let mut graph = seed_workflow_graph();
        let bad = WorkflowEdge {
            id: "bad-kind".to_owned(),
            source: PortEndpoint {
                node_id: "x5233-driver-1".to_owned(),
                port_id: "yuvCh0".to_owned(),
            },
            target: PortEndpoint {
                node_id: "viewer-1".to_owned(),
                port_id: "video".to_owned(),
            },
            kind: PortKind::ImageFrame,
            schema: "image.frame.v1".to_owned(),
            schema_version: WORKFLOW_SCHEMA_VERSION.to_owned(),
        };
        graph.edges.push(bad.clone());
        assert!(validate_edge(&graph, &bad).is_err());
    }

    #[test]
    fn validation_rejects_cardinality_one_second_incoming_edge() {
        let mut graph = seed_workflow_graph();
        let existing = graph
            .edges
            .iter()
            .find(|edge| edge.id == "x5233-preview-yuv-viewer")
            .cloned()
            .expect("seed graph contains the driver→viewer edge");
        assert_eq!(existing.target.node_id, "viewer-1");
        assert_eq!(existing.target.port_id, "image");

        let mut duplicate = existing.clone();
        duplicate.id = "x5233-preview-yuv-viewer-duplicate".to_owned();
        graph.edges.push(duplicate);

        let err =
            validate_workflow(&graph).expect_err("second incoming edge to One port must fail");
        assert!(err.contains("cardinality=One"), "unexpected error: {err}");
        assert!(err.contains("viewer-1"), "unexpected error: {err}");
    }

    #[test]
    fn validation_rejects_incompatible_port_schemas() {
        let mut graph = seed_workflow_graph();
        let edge = graph
            .edges
            .iter()
            .find(|edge| edge.id == "x5233-preview-yuv-viewer")
            .cloned()
            .expect("seed driver→viewer edge exists");
        let viewer = graph
            .nodes
            .iter_mut()
            .find(|node| node.id == "viewer-1")
            .expect("seed viewer exists");
        let image = viewer
            .inputs
            .iter_mut()
            .find(|port| port.id == "image")
            .expect("viewer image input exists");
        image.schema = "image.frame.v2".to_owned();

        let error = validate_edge(&graph, &edge).expect_err("target schema mismatch is rejected");
        assert!(error.contains("target schema"), "unexpected error: {error}");
    }

    #[test]
    fn validation_allows_cardinality_many_fan_in() {
        let mut graph = seed_workflow_graph();
        for node in &mut graph.nodes {
            if node.id == "viewer-1" {
                for port in &mut node.inputs {
                    if port.id == "image" {
                        port.cardinality = PortCardinality::Many;
                    }
                }
            }
        }
        let existing = graph
            .edges
            .iter()
            .find(|edge| edge.id == "x5233-preview-yuv-viewer")
            .cloned()
            .unwrap();
        let mut duplicate = existing;
        duplicate.id = "x5233-preview-yuv-viewer-duplicate".to_owned();
        graph.edges.push(duplicate);

        validate_cardinality_constraints(&graph).expect("Many port fan-in must be allowed");
    }

    #[test]
    fn calibration_template_uses_identity_preserving_capture_chain() {
        let graph = calibration_template_graph();
        validate_workflow(&graph).expect("calibration template is valid");
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::X5233Driver)
        );
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::GainScorer)
        );
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::CaptureGate)
        );
        assert!(
            !graph
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::OverlayComposer)
        );
        for edge_id in [
            "calib-rtsp-detector",
            "calib-detection-gain",
            "calib-gain-gate",
            "calib-gate-x5233-capture",
            "calib-rtsp-viewer",
            "calib-rtsp-snapshot-viewer",
        ] {
            assert!(
                graph.edges.iter().any(|edge| edge.id == edge_id),
                "missing {edge_id}"
            );
        }
    }

    #[test]
    fn node_catalog_exposes_hex_arm_only_when_feature_enabled() {
        let catalog = node_catalog();
        #[cfg(feature = "hex-arm-control")]
        assert!(
            catalog
                .iter()
                .any(|definition| definition.kind == NodeKind::HexArmDevice)
        );
        #[cfg(not(feature = "hex-arm-control"))]
        assert!(
            !catalog
                .iter()
                .any(|definition| definition.kind == NodeKind::HexArmDevice)
        );
    }

    #[test]
    fn hex_arm_config_rejects_unsupported_transport_and_non_finite_joints() {
        let mut node = workflow_node(
            "hex-arm-1",
            NodeKind::HexArmDevice,
            "Hex Arm Device",
            NodePosition { x: 0.0, y: 0.0 },
        );
        node.config["transport"] = json!("kcp");
        let error = validate_node_config(&node).expect_err("KCP must be rejected");
        assert!(error.contains("KCP is unsupported"));

        node.config["transport"] = json!("websocket");
        node.config["jointPositions"] = json!("0.0, not-a-radian");
        let error = validate_node_config(&node).expect_err("joint radians must be finite");
        assert!(error.contains("finite radians"));

        node.config["jointPositions"] = json!("0.0, 1.57");
        node.config["controlEnabled"] = json!(true);
        validate_node_config(&node).expect("valid WebSocket Hex Arm config");
    }

    #[test]
    fn validation_rejects_multiple_hex_arm_session_owners() {
        let mut graph = seed_workflow_graph();
        graph.nodes.push(workflow_node(
            "hex-arm-1",
            NodeKind::HexArmDevice,
            "Hex Arm One",
            NodePosition { x: 0.0, y: 0.0 },
        ));
        graph.nodes.push(workflow_node(
            "hex-arm-2",
            NodeKind::HexArmDevice,
            "Hex Arm Two",
            NodePosition { x: 100.0, y: 0.0 },
        ));
        let error = validate_workflow(&graph).expect_err("Hex Arm session must have one owner");
        assert!(error.contains("at most one hexArmDevice"));
    }

    #[test]
    fn node_catalog_has_feature_dependent_nodes() {
        let catalog = node_catalog();
        assert_eq!(
            catalog.len(),
            if cfg!(feature = "hex-arm-control") {
                22
            } else {
                21
            }
        );
        assert!(
            !catalog
                .iter()
                .any(|definition| definition.kind == NodeKind::OverlayComposer),
            "OverlayComposer is legacy-only; new graphs should connect overlays directly to Viewer"
        );
        // 每个 kind 都能通过 node_definition 展开，且无重复。
        let kinds: Vec<NodeKind> = catalog.iter().map(|def| def.kind).collect();
        assert_eq!(kinds.len(), catalog.len());
    }

    #[test]
    fn viewer_declares_preview_only_contracts() {
        let viewer = node_definition(NodeKind::Viewer);
        assert!(viewer.outputs.is_empty());
        assert!(viewer.inputs.iter().any(|port| {
            port.id == "video"
                && port.kind == PortKind::StreamVideoFrame
                && port.schema == "stream.video-frame.v1"
                && port.format_hint.as_deref() == Some("Rgba8")
        }));
        assert!(viewer.inputs.iter().any(|port| {
            port.id == "image"
                && port.kind == PortKind::ImageFrame
                && port.schema == "image.frame.v1"
                && port.direction == PortDirection::Input
                && port.format_hint.as_deref() == Some("Rgba8 | Gray8 | Gray16Le | Nv12")
        }));
        assert!(viewer.inputs.iter().any(|port| {
            port.id == "overlay"
                && port.kind == PortKind::LayerOverlay
                && port.schema == "viewer.layer.overlay.v1"
                && port.direction == PortDirection::Input
        }));
    }

    #[test]
    fn calibration_nodes_only_declare_implemented_ports() {
        let solver = node_definition(NodeKind::CalibrationSolver);
        assert_eq!(solver.inputs.len(), 1);
        assert_eq!(solver.outputs.len(), 1);
        assert!(
            solver
                .outputs
                .iter()
                .any(|p| p.id == "solution" && p.kind == PortKind::CalibSolution)
        );

        let dataset = node_definition(NodeKind::DatasetCollector);
        assert_eq!(dataset.inputs.len(), 1);
        assert_eq!(dataset.outputs.len(), 1);
        assert!(dataset.inputs.iter().any(|p| p.id == "detection"));
        assert!(
            dataset
                .outputs
                .iter()
                .any(|p| p.id == "dataset" && p.kind == PortKind::CalibDataset)
        );

        let auto = node_definition(NodeKind::AutoCaptureController);
        assert_eq!(auto.inputs.len(), 1);
        assert_eq!(auto.outputs.len(), 1);
        assert!(
            auto.inputs
                .iter()
                .any(|p| p.id == "score" && p.kind == PortKind::CaptureScore)
        );
        assert!(
            auto.outputs
                .iter()
                .any(|p| p.id == "command" && p.kind == PortKind::CommandCapture)
        );

        let gain = node_definition(NodeKind::GainScorer);
        assert_eq!(gain.inputs[0].kind, PortKind::CalibDetection);
        assert_eq!(gain.outputs[0].kind, PortKind::CaptureScore);

        let gate = node_definition(NodeKind::CaptureGate);
        assert_eq!(gate.inputs[0].kind, PortKind::CaptureScore);
        assert!(gate.outputs.iter().any(|port| {
            port.id == "capture"
                && port.kind == PortKind::CommandCapture
                && port.schema == "command.capture.request.v1"
        }));

        let demosaic = node_definition(NodeKind::Demosaic);
        assert!(demosaic.inputs.iter().any(|port| {
            port.id == "raw"
                && port.kind == PortKind::ImageFrame
                && port.schema == "image.frame.v1"
                && port.format_hint.as_deref() == Some("BayerRaw")
        }));
        assert!(demosaic.outputs.iter().any(|port| {
            port.id == "image"
                && port.kind == PortKind::ImageFrame
                && port.schema == "image.frame.v1"
                && port.format_hint.as_deref() == Some("Rgba8")
        }));
        let pose = node_definition(NodeKind::PoseGuide);
        assert!(
            pose.outputs
                .iter()
                .any(|p| p.id == "target" && p.label == "Image-grid Target")
        );
    }

    #[test]
    fn control_nodes_only_declare_runtime_implemented_ports() {
        let ssh = node_definition(NodeKind::SshSession);
        assert!(ssh.inputs.is_empty());
        assert_eq!(
            ssh.outputs
                .iter()
                .map(|port| port.id.as_str())
                .collect::<Vec<_>>(),
            ["result"]
        );

        for kind in [NodeKind::I2cTransfer, NodeKind::EepromProvision] {
            let definition = node_definition(kind);
            assert!(
                definition.inputs.is_empty(),
                "{kind:?} must use inline configuration"
            );
            assert_eq!(
                definition
                    .outputs
                    .iter()
                    .map(|port| port.id.as_str())
                    .collect::<Vec<_>>(),
                ["result"]
            );
        }

        let template = i2c_template_graph();
        assert!(template.edges.is_empty());
        assert_eq!(template.nodes.len(), 2);
    }

    #[test]
    fn password_only_control_nodes_reject_key_file_references() {
        for kind in [
            NodeKind::SshSession,
            NodeKind::I2cTransfer,
            NodeKind::EepromProvision,
        ] {
            let mut node = workflow_node(
                "password-only",
                kind,
                "Password only",
                NodePosition { x: 0.0, y: 0.0 },
            );
            node.config["credentialRef"] = json!("key-file:/home/user/.ssh/id_ed25519");
            let error = validate_node_config(&node).expect_err("key authentication is forbidden");
            assert!(error.contains("password session"));

            node.config["credentialRef"] = json!("session:password_only");
            validate_node_config(&node).expect("password session reference is valid");
        }
    }

    #[test]
    fn templates_generate_valid_graphs() {
        for template in workmode_templates() {
            validate_workflow(&template.graph).expect(template.id);
        }
    }

    #[test]
    fn templates_cover_x5233_rtsp_yuv_and_raw_contracts() {
        let templates = workmode_templates();
        for template in &templates {
            for node in &template.graph.nodes {
                if node.kind == NodeKind::X5233Driver {
                    assert_eq!(node.title, "X5_233 Driver");
                }
            }
        }
        let find = |id: &str| {
            templates
                .iter()
                .find(|template| template.id == id)
                .expect("required workmode template")
        };
        let x5233_preview = find("x5233-preview");
        assert!(
            x5233_preview.graph.edges.iter().any(|edge| {
                edge.kind == PortKind::ImageFrame && edge.schema == "image.frame.v1"
            })
        );
        let rtsp_snapshot = find("rtsp-snapshot");
        assert!(rtsp_snapshot.graph.edges.iter().any(|edge| {
            edge.id == "rtsp-snapshot-image-viewer" && edge.kind == PortKind::ImageFrame
        }));
        let calibration = find("calibration");
        assert!(calibration.graph.edges.iter().any(|edge| {
            edge.id == "calib-detector-overlay-viewer"
                && edge.kind == PortKind::LayerOverlay
                && edge.schema == "viewer.layer.overlay.v1"
        }));
        let yuv = find("x5233-yuv-capture");
        assert!(yuv.graph.nodes.iter().any(|node| {
            node.outputs.iter().any(|port| {
                port.id == "yuvCh0"
                    && port.schema == "image.frame.v1"
                    && port.format_hint.as_deref() == Some("Nv12")
            })
        }));
        let raw = find("x5233-raw-diagnostic");
        assert!(raw.graph.nodes.iter().any(|node| {
            node.outputs.iter().any(|port| {
                port.id == "rawCam0"
                    && port.schema == "image.frame.v1"
                    && port.format_hint.as_deref() == Some("BayerRaw")
            })
        }));
        assert!(
            raw.graph
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::Demosaic)
        );
        assert!(raw.graph.edges.iter().any(|edge| {
            edge.id == "x5233-raw-driver-demosaic" && edge.kind == PortKind::ImageFrame
        }));
    }

    #[test]
    fn local_file_source_emits_image_and_file_ref() {
        let source = node_definition(NodeKind::LocalFileSource);
        assert!(
            source
                .outputs
                .iter()
                .any(|p| p.kind == PortKind::ImageFrame)
        );
        assert!(
            source
                .outputs
                .iter()
                .any(|p| p.kind == PortKind::FileRef && p.schema == "file.ref.v1")
        );
    }

    #[test]
    fn sftp_file_source_config_rejects_unsafe_remote_root() {
        let mut graph = seed_workflow_graph();
        graph.nodes.push(workflow_node(
            "sftp-unsafe",
            NodeKind::SftpFileSource,
            "Unsafe SFTP",
            NodePosition { x: 0.0, y: 0.0 },
        ));
        let node = graph.nodes.last_mut().expect("SFTP node exists");
        node.config["remoteRoot"] = json!("/opt/../etc");
        let error = validate_workflow(&graph).expect_err("unsafe remote root rejected");
        assert!(error.contains("remoteRoot"));
    }

    #[test]
    fn normalize_rejects_runtime_config_fields() {
        let mut graph = seed_workflow_graph();
        graph.nodes[0].config["objectUrl"] = json!("blob:runtime");
        let error =
            normalize_workflow(graph, "next".to_owned()).expect_err("runtime fields are rejected");
        assert!(error.contains("objectUrl"));
    }

    #[test]
    fn reject_runtime_config_rejects_nested_and_array_secrets() {
        // 嵌套 object 中的 secret 必须被拒（旧实现只查顶层会漏）。
        let nested = json!({"auth": {"password": "hunter2"}});
        let error = reject_runtime_config(&nested).expect_err("nested secret must be rejected");
        assert!(error.contains("password"));

        // 数组元素中的 credentialRef 仍需校验引用语法，不能写入密码或裸字符串。
        let array = json!({"tokens": [{"credentialRef": "plaintext-password"}]});
        let error =
            reject_runtime_config(&array).expect_err("unsafe credential reference rejected");
        assert!(error.contains("credentialRef"));

        let empty_reference = json!({"auth": {"credentialRef": ""}});
        assert!(reject_runtime_config(&empty_reference).is_ok());

        let safe = json!({"auth": {"credentialRef": "key-file:/home/user/.ssh/id_ed25519"}});
        assert!(reject_runtime_config(&safe).is_ok());

        // 合法嵌套（不含敏感键）应通过。
        let benign = json!({"auth": {"host": "camera.local", "port": 22}});
        assert!(reject_runtime_config(&benign).is_ok());
    }
}
