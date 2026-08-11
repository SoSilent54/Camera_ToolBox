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
    LocalWorkspace,
    SftpWorkspace,
    FileBrowser,
    ImageFileSource,
    RtspSource,
    SshSession,
    X5Device,
    X5RtspChannel,
    X5Snapshot,
    RtspDecoder,
    FrameSampler,
    ImageLayer,
    VideoLayer,
    OverlayComposer,
    Viewer,
    ChessboardDetector,
    DatasetCollector,
    CoverageAnalyzer,
    CaptureScorer,
    AutoCaptureController,
    PoseGuide,
    CalibrationSolver,
    ReprojectionInspector,
    CalibrationExport,
    I2cBusDiscovery,
    I2cTransfer,
    EepromMapLoader,
    EepromProvision,
    ResultView,
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

/// Stage 7 运行时诊断快照；仅驻留服务进程内存，绝不写入 `WorkflowGraph`。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeGraphStatus {
    pub graph_id: String,
    pub running: bool,
    pub nodes: Vec<RuntimeNodeStatus>,
    pub events: Vec<RuntimeNodeEvent>,
}

/// 单个节点的运行时状态，使用节点 ID 与持久化拓扑关联。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeNodeStatus {
    pub node_id: String,
    pub state: NodeRuntimeState,
    pub diagnostic: String,
}

/// 节点级诊断事件；Stage 7 不持有帧、套接字、日志或其他重型运行时数据。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeNodeEvent {
    pub node_id: String,
    pub level: RuntimeEventLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeEventLevel {
    Info,
    Warning,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPort {
    pub id: String,
    pub label: String,
    pub direction: PortDirection,
    pub kind: PortKind,
    pub schema: String,
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
    #[serde(rename = "control.ssh")]
    ControlSsh,
    #[serde(rename = "control.x5tcp")]
    ControlX5Tcp,
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
    vec![
        node_definition(NodeKind::LocalWorkspace),
        node_definition(NodeKind::SftpWorkspace),
        node_definition(NodeKind::FileBrowser),
        node_definition(NodeKind::ImageFileSource),
        node_definition(NodeKind::RtspSource),
        node_definition(NodeKind::SshSession),
        node_definition(NodeKind::X5Device),
        node_definition(NodeKind::X5RtspChannel),
        node_definition(NodeKind::X5Snapshot),
        node_definition(NodeKind::RtspDecoder),
        node_definition(NodeKind::FrameSampler),
        node_definition(NodeKind::ImageLayer),
        node_definition(NodeKind::VideoLayer),
        node_definition(NodeKind::OverlayComposer),
        node_definition(NodeKind::Viewer),
        node_definition(NodeKind::ChessboardDetector),
        node_definition(NodeKind::DatasetCollector),
        node_definition(NodeKind::CoverageAnalyzer),
        node_definition(NodeKind::CaptureScorer),
        node_definition(NodeKind::AutoCaptureController),
        node_definition(NodeKind::PoseGuide),
        node_definition(NodeKind::CalibrationSolver),
        node_definition(NodeKind::ReprojectionInspector),
        node_definition(NodeKind::CalibrationExport),
        node_definition(NodeKind::I2cBusDiscovery),
        node_definition(NodeKind::I2cTransfer),
        node_definition(NodeKind::EepromMapLoader),
        node_definition(NodeKind::EepromProvision),
        node_definition(NodeKind::ResultView),
    ]
}

pub fn node_definition(kind: NodeKind) -> NodeDefinition {
    let (category, title, description, inputs, outputs, default_config) = match kind {
        NodeKind::LocalWorkspace => (
            NodeCategory::Workspace,
            "Local Workspace",
            "本地 workspace 根目录或项目目录",
            vec![],
            vec![port(
                "workspace",
                "Workspace",
                PortDirection::Output,
                PortKind::WorkspaceLocal,
                "workspace.local.v1",
                Some(PortRole::Workspace),
            )],
            json!({"root": ""}),
        ),
        NodeKind::SftpWorkspace => (
            NodeCategory::Workspace,
            "SFTP Workspace",
            "通过 SSH/SFTP 暴露远程目录",
            vec![port(
                "ssh",
                "SSH",
                PortDirection::Input,
                PortKind::ControlSsh,
                "control.ssh.v1",
                Some(PortRole::Control),
            )],
            vec![port(
                "workspace",
                "Remote Workspace",
                PortDirection::Output,
                PortKind::WorkspaceRemoteSftp,
                "workspace.remote.sftp.v1",
                Some(PortRole::Workspace),
            )],
            json!({"path": "/"}),
        ),
        NodeKind::FileBrowser => (
            NodeCategory::Workspace,
            "File Browser",
            "浏览本地或远程 workspace 并选择文件",
            vec![
                port(
                    "local",
                    "Local Workspace",
                    PortDirection::Input,
                    PortKind::WorkspaceLocal,
                    "workspace.local.v1",
                    Some(PortRole::Workspace),
                ),
                optional_port(
                    "remote",
                    "Remote Workspace",
                    PortDirection::Input,
                    PortKind::WorkspaceRemoteSftp,
                    "workspace.remote.sftp.v1",
                    Some(PortRole::Workspace),
                ),
            ],
            vec![port(
                "file",
                "File",
                PortDirection::Output,
                PortKind::ImageFrame,
                "image.frame.v1",
                Some(PortRole::Image),
            )],
            json!({"filter": "*.png;*.jpg;*.jpeg"}),
        ),
        NodeKind::ImageFileSource => (
            NodeCategory::Source,
            "Image File Source",
            "从文件引用加载单张图片帧",
            vec![port(
                "file",
                "Image File",
                PortDirection::Input,
                PortKind::ImageFrame,
                "image.frame.v1",
                Some(PortRole::Image),
            )],
            vec![port(
                "image",
                "Image",
                PortDirection::Output,
                PortKind::ImageFrame,
                "image.frame.v1",
                Some(PortRole::Image),
            )],
            json!({"relativePath": "", "reload": "manual"}),
        ),
        NodeKind::RtspSource => (
            NodeCategory::Source,
            "RTSP Input",
            "摄像头或板端 RTSP URL 入口",
            vec![],
            vec![port(
                "endpoint",
                "RTSP endpoint",
                PortDirection::Output,
                PortKind::EndpointRtsp,
                "media.rtsp.endpoint.v1",
                Some(PortRole::Endpoint),
            )],
            json!({"url": DEFAULT_RTSP_URL, "transport": "tcp", "expectedWidth": 1920, "expectedHeight": 1080, "expectedFps": 60}),
        ),
        NodeKind::SshSession => (
            NodeCategory::Control,
            "SSH Session",
            "远程命令、SFTP、I²C helper 的控制会话",
            vec![],
            vec![port(
                "ssh",
                "SSH",
                PortDirection::Output,
                PortKind::ControlSsh,
                "control.ssh.v1",
                Some(PortRole::Control),
            )],
            json!({"profileId": "", "autoConnect": false}),
        ),
        NodeKind::X5Device => (
            NodeCategory::Control,
            "X5 Device",
            "X5_233 TCP 控制与 RTSP 通道资源",
            vec![optional_port(
                "ssh",
                "SSH",
                PortDirection::Input,
                PortKind::ControlSsh,
                "control.ssh.v1",
                Some(PortRole::Control),
            )],
            vec![
                port(
                    "control",
                    "X5 TCP",
                    PortDirection::Output,
                    PortKind::ControlX5Tcp,
                    "control.x5tcp.v1",
                    Some(PortRole::Control),
                ),
                many_port(
                    "rtsp",
                    "RTSP Channels",
                    PortDirection::Output,
                    PortKind::EndpointRtsp,
                    "media.rtsp.endpoint.v1",
                    Some(PortRole::Endpoint),
                ),
            ],
            json!({"host": "10.21.12.108", "tcpPort": 9073}),
        ),
        NodeKind::X5RtspChannel => (
            NodeCategory::Source,
            "X5 RTSP Channel",
            "从 X5 设备选择一个 RTSP 通道",
            vec![optional_port(
                "control",
                "X5 TCP",
                PortDirection::Input,
                PortKind::ControlX5Tcp,
                "control.x5tcp.v1",
                Some(PortRole::Control),
            )],
            vec![port(
                "endpoint",
                "RTSP endpoint",
                PortDirection::Output,
                PortKind::EndpointRtsp,
                "media.rtsp.endpoint.v1",
                Some(PortRole::Endpoint),
            )],
            json!({"channel": 0, "path": "/PRR"}),
        ),
        NodeKind::X5Snapshot => (
            NodeCategory::Control,
            "X5 Snapshot",
            "手动或自动快门命令触发 X5 抓帧",
            vec![
                port(
                    "control",
                    "X5 TCP",
                    PortDirection::Input,
                    PortKind::ControlX5Tcp,
                    "control.x5tcp.v1",
                    Some(PortRole::Control),
                ),
                optional_port(
                    "command",
                    "Capture Command",
                    PortDirection::Input,
                    PortKind::CommandCapture,
                    "command.capture.v1",
                    Some(PortRole::Command),
                ),
            ],
            vec![port(
                "image",
                "Image",
                PortDirection::Output,
                PortKind::ImageFrame,
                "image.frame.v1",
                Some(PortRole::Image),
            )],
            json!({"mode": "latest"}),
        ),
        NodeKind::RtspDecoder => (
            NodeCategory::Media,
            "RTSP Decoder",
            "将 RTSP 端点解码为视频帧流",
            vec![port(
                "endpoint",
                "RTSP endpoint",
                PortDirection::Input,
                PortKind::EndpointRtsp,
                "media.rtsp.endpoint.v1",
                Some(PortRole::Endpoint),
            )],
            vec![port(
                "frames",
                "Video Frames",
                PortDirection::Output,
                PortKind::StreamVideoFrame,
                "stream.video-frame.v1",
                Some(PortRole::Stream),
            )],
            json!({"transport": "tcp", "latency": "low", "previewWidth": 960, "previewHeight": 540}),
        ),
        NodeKind::FrameSampler => (
            NodeCategory::Media,
            "Frame Sampler",
            "按显式 fps 对视频帧流降采样",
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
            json!({"fpsLimit": 30, "dropPolicy": "latest"}),
        ),
        NodeKind::ImageLayer => (
            NodeCategory::Viewer,
            "Image Layer",
            "把单张图片转为 Viewer 图层",
            vec![port(
                "image",
                "Image",
                PortDirection::Input,
                PortKind::ImageFrame,
                "image.frame.v1",
                Some(PortRole::Image),
            )],
            vec![port(
                "layer",
                "Image Layer",
                PortDirection::Output,
                PortKind::LayerImage,
                "viewer.layer.image.v1",
                Some(PortRole::Layer),
            )],
            json!({"visible": true, "opacity": 1.0}),
        ),
        NodeKind::VideoLayer => (
            NodeCategory::Viewer,
            "Video Layer",
            "把视频帧流转为 Viewer 图层",
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
            "Overlay Composer",
            "组合图像/视频层和 overlay 层为 Viewer scene",
            vec![
                many_port(
                    "video",
                    "Video Layers",
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
                    "Overlay Layers",
                    PortDirection::Input,
                    PortKind::LayerOverlay,
                    "viewer.layer.overlay.v1",
                    Some(PortRole::Overlay),
                ),
            ],
            vec![port(
                "scene",
                "Viewer Scene",
                PortDirection::Output,
                PortKind::ViewerScene,
                "viewer.scene.v1",
                Some(PortRole::Layer),
            )],
            json!({"blendMode": "normal"}),
        ),
        NodeKind::Viewer => (
            NodeCategory::Viewer,
            "Viewer",
            "显示图层、scene、MJPEG fallback 与指标",
            vec![
                optional_port(
                    "scene",
                    "Viewer Scene",
                    PortDirection::Input,
                    PortKind::ViewerScene,
                    "viewer.scene.v1",
                    Some(PortRole::Layer),
                ),
                optional_port(
                    "video",
                    "Video Layer",
                    PortDirection::Input,
                    PortKind::LayerVideo,
                    "viewer.layer.video.v1",
                    Some(PortRole::Layer),
                ),
                optional_port(
                    "image",
                    "Image Layer",
                    PortDirection::Input,
                    PortKind::LayerImage,
                    "viewer.layer.image.v1",
                    Some(PortRole::Layer),
                ),
            ],
            vec![],
            json!({"fitMode": "contain", "overlay": "status", "viewport": {"scale": 1.0, "x": 0.0, "y": 0.0}}),
        ),
        NodeKind::ChessboardDetector => (
            NodeCategory::Calibration,
            "Chessboard Detector",
            "检测棋盘格角点并输出 overlay",
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
            json!({"boardRows": 11, "boardCols": 8, "squareSizeMm": 30.0, "enabled": true}),
        ),
        NodeKind::DatasetCollector => (
            NodeCategory::Calibration,
            "Dataset Collector",
            "人工接受/移除样本并形成标定数据集",
            vec![
                port(
                    "image",
                    "Image",
                    PortDirection::Input,
                    PortKind::ImageFrame,
                    "image.frame.v1",
                    Some(PortRole::Image),
                ),
                port(
                    "detection",
                    "Detection",
                    PortDirection::Input,
                    PortKind::CalibDetection,
                    "calib.detection.v1",
                    Some(PortRole::Dataset),
                ),
            ],
            vec![port(
                "dataset",
                "Dataset",
                PortDirection::Output,
                PortKind::CalibDataset,
                "calib.dataset.v1",
                Some(PortRole::Dataset),
            )],
            json!({"mode": "manual", "maxSamples": 80}),
        ),
        NodeKind::CoverageAnalyzer => (
            NodeCategory::Calibration,
            "Coverage Analyzer",
            "分析标定数据覆盖度并输出指导 overlay",
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
        NodeKind::CaptureScorer => (
            NodeCategory::Calibration,
            "Capture Scorer",
            "根据检测和覆盖度给自动快门评分",
            vec![
                port(
                    "detection",
                    "Detection",
                    PortDirection::Input,
                    PortKind::CalibDetection,
                    "calib.detection.v1",
                    Some(PortRole::Dataset),
                ),
                optional_port(
                    "coverage",
                    "Coverage",
                    PortDirection::Input,
                    PortKind::CalibCoverage,
                    "calib.coverage.v1",
                    Some(PortRole::Dataset),
                ),
            ],
            vec![port(
                "score",
                "Capture Score",
                PortDirection::Output,
                PortKind::CaptureScore,
                "capture.score.v1",
                Some(PortRole::Status),
            )],
            json!({"strategy": "datasetGain"}),
        ),
        NodeKind::AutoCaptureController => (
            NodeCategory::Calibration,
            "Auto Capture",
            "把评分、帧流和目标位姿转换为抓帧命令",
            vec![
                port(
                    "score",
                    "Capture Score",
                    PortDirection::Input,
                    PortKind::CaptureScore,
                    "capture.score.v1",
                    Some(PortRole::Status),
                ),
                optional_port(
                    "frames",
                    "Video Frames",
                    PortDirection::Input,
                    PortKind::StreamVideoFrame,
                    "stream.video-frame.v1",
                    Some(PortRole::Stream),
                ),
                optional_port(
                    "target",
                    "Capture Target",
                    PortDirection::Input,
                    PortKind::CaptureTarget,
                    "capture.target.v1",
                    Some(PortRole::Command),
                ),
            ],
            vec![port(
                "command",
                "Capture Command",
                PortDirection::Output,
                PortKind::CommandCapture,
                "command.capture.v1",
                Some(PortRole::Command),
            )],
            json!({"armed": false, "strategy": "datasetGain", "cooldownMs": 800}),
        ),
        NodeKind::PoseGuide => (
            NodeCategory::Calibration,
            "Pose Guide",
            "根据覆盖度生成 guided pose 目标和 overlay",
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
                    "Capture Target",
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
        NodeKind::CalibrationSolver => (
            NodeCategory::Calibration,
            "Calibration Solver",
            "手动触发标定求解",
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
            json!({"model": "pinhole", "trigger": "manual"}),
        ),
        NodeKind::ReprojectionInspector => (
            NodeCategory::Calibration,
            "Reprojection Inspector",
            "从 solution 和 dataset 生成重投影 overlay/report",
            vec![
                port(
                    "solution",
                    "Solution",
                    PortDirection::Input,
                    PortKind::CalibSolution,
                    "calib.solution.v1",
                    Some(PortRole::Solution),
                ),
                port(
                    "dataset",
                    "Dataset",
                    PortDirection::Input,
                    PortKind::CalibDataset,
                    "calib.dataset.v1",
                    Some(PortRole::Dataset),
                ),
            ],
            vec![
                port(
                    "overlay",
                    "Overlay",
                    PortDirection::Output,
                    PortKind::LayerOverlay,
                    "viewer.layer.overlay.v1",
                    Some(PortRole::Overlay),
                ),
                port(
                    "report",
                    "Report",
                    PortDirection::Output,
                    PortKind::CalibReport,
                    "calib.report.v1",
                    Some(PortRole::Solution),
                ),
            ],
            json!({"maxResidualPx": 1.0}),
        ),
        NodeKind::CalibrationExport => (
            NodeCategory::Calibration,
            "Calibration Export",
            "导出标定结果或生成 EEPROM payload",
            vec![port(
                "solution",
                "Solution",
                PortDirection::Input,
                PortKind::CalibSolution,
                "calib.solution.v1",
                Some(PortRole::Solution),
            )],
            vec![port(
                "payload",
                "EEPROM Payload",
                PortDirection::Output,
                PortKind::EepromPayload,
                "eeprom.payload.v1",
                Some(PortRole::Command),
            )],
            json!({"format": "yaml"}),
        ),
        NodeKind::I2cBusDiscovery => (
            NodeCategory::Control,
            "I²C Bus Discovery",
            "手动刷新远端 Linux I²C bus 列表",
            vec![port(
                "ssh",
                "SSH",
                PortDirection::Input,
                PortKind::ControlSsh,
                "control.ssh.v1",
                Some(PortRole::Control),
            )],
            vec![port(
                "bus",
                "I²C Bus",
                PortDirection::Output,
                PortKind::I2cBus,
                "i2c.bus.v1",
                Some(PortRole::Control),
            )],
            json!({"trigger": "manual"}),
        ),
        NodeKind::I2cTransfer => (
            NodeCategory::Control,
            "I²C Transfer",
            "预览并手动执行 I²C 读写请求",
            vec![
                port(
                    "ssh",
                    "SSH",
                    PortDirection::Input,
                    PortKind::ControlSsh,
                    "control.ssh.v1",
                    Some(PortRole::Control),
                ),
                port(
                    "bus",
                    "I²C Bus",
                    PortDirection::Input,
                    PortKind::I2cBus,
                    "i2c.bus.v1",
                    Some(PortRole::Control),
                ),
            ],
            vec![
                port(
                    "transfer",
                    "Transfer",
                    PortDirection::Output,
                    PortKind::I2cTransfer,
                    "i2c.transfer.v1",
                    Some(PortRole::Command),
                ),
                port(
                    "result",
                    "Result",
                    PortDirection::Output,
                    PortKind::I2cResult,
                    "i2c.result.v1",
                    Some(PortRole::Status),
                ),
            ],
            json!({"profileId": "x5-lab", "bus": "i2c-1", "address": "0x50", "register": "0x0000", "payload": "", "pageSize": 16, "mode": "read", "confirmWrites": true}),
        ),
        NodeKind::EepromMapLoader => (
            NodeCategory::Control,
            "EEPROM Map Loader",
            "加载内置或文件 EEPROM map",
            vec![],
            vec![port(
                "map",
                "EEPROM Map",
                PortDirection::Output,
                PortKind::EepromMap,
                "eeprom.map.v1",
                Some(PortRole::Command),
            )],
            json!({"mapId": "x5_233_default"}),
        ),
        NodeKind::EepromProvision => (
            NodeCategory::Control,
            "EEPROM Provision",
            "根据 map/payload/bus 生成写入与校验动作",
            vec![
                port(
                    "map",
                    "EEPROM Map",
                    PortDirection::Input,
                    PortKind::EepromMap,
                    "eeprom.map.v1",
                    Some(PortRole::Command),
                ),
                optional_port(
                    "payload",
                    "Payload",
                    PortDirection::Input,
                    PortKind::EepromPayload,
                    "eeprom.payload.v1",
                    Some(PortRole::Command),
                ),
                port(
                    "bus",
                    "I²C Bus",
                    PortDirection::Input,
                    PortKind::I2cBus,
                    "i2c.bus.v1",
                    Some(PortRole::Control),
                ),
            ],
            vec![port(
                "transfer",
                "Transfer",
                PortDirection::Output,
                PortKind::I2cTransfer,
                "i2c.transfer.v1",
                Some(PortRole::Command),
            )],
            json!({"profileId": "x5-lab", "bus": "i2c-1", "address": "0x50", "register": "0x0000", "payload": "00", "pageSize": 16, "mapId": "x5_233_default", "confirmWrites": true, "verifyAfterWrite": true}),
        ),
        NodeKind::ResultView => (
            NodeCategory::Diagnostics,
            "Result View",
            "显示 I²C/EEPROM/诊断结果",
            vec![port(
                "result",
                "Result",
                PortDirection::Input,
                PortKind::I2cResult,
                "i2c.result.v1",
                Some(PortRole::Status),
            )],
            vec![],
            json!({"format": "table"}),
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
            id: "viewer",
            title: "Viewer",
            description: "RTSP/X5 channel → Decoder → VideoLayer → Viewer",
            graph: viewer_template_graph(),
        },
        WorkmodeTemplate {
            id: "local-image",
            title: "Local Image",
            description: "Local Workspace → File Browser → Image File Source → Image Layer → Viewer",
            graph: local_image_template_graph(),
        },
        WorkmodeTemplate {
            id: "calibration",
            title: "Calibration",
            description: "RTSP → Detector → Dataset/Coverage/AutoCapture/Solver",
            graph: calibration_template_graph(),
        },
        WorkmodeTemplate {
            id: "i2c-tools",
            title: "I²C Tools",
            description: "SSH → I²C bus discovery/transfer/EEPROM provision",
            graph: i2c_template_graph(),
        },
    ]
}

/// 生成默认演示图；使用完整端口类型链路，Viewer 仍通过 RTSP 源派生 MJPEG fallback。
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
    for edge in &graph.edges {
        validate_edge(graph, edge)?;
    }
    Ok(())
}

/// 构建 Stage 7 诊断运行时快照。此函数只标记可安全自动启动的纯媒体节点，
/// 不创建 RTSP、SSH、X5 或 I²C 连接，也不执行校准或 EEPROM 操作。
pub fn runtime_graph_status(graph: &WorkflowGraph, running: bool) -> RuntimeGraphStatus {
    let nodes = graph
        .nodes
        .iter()
        .map(|node| runtime_node_status(node, running))
        .collect::<Vec<_>>();
    let events = nodes
        .iter()
        .map(|node| RuntimeNodeEvent {
            node_id: node.node_id.clone(),
            level: match node.state {
                NodeRuntimeState::Running => RuntimeEventLevel::Info,
                _ => RuntimeEventLevel::Warning,
            },
            message: node.diagnostic.clone(),
        })
        .collect();

    RuntimeGraphStatus {
        graph_id: graph.id.clone(),
        running,
        nodes,
        events,
    }
}

fn runtime_node_status(node: &WorkflowNode, running: bool) -> RuntimeNodeStatus {
    let (state, diagnostic) = if !running {
        (NodeRuntimeState::Idle, "runtime stopped".to_owned())
    } else if safe_auto_start_node(node.kind) {
        (
            NodeRuntimeState::Running,
            "Stage 7 diagnostic session active; no external action was started".to_owned(),
        )
    } else if manual_node(node.kind) {
        (
            NodeRuntimeState::Idle,
            "manual or dangerous node was not auto-executed".to_owned(),
        )
    } else {
        (
            NodeRuntimeState::Idle,
            "no Stage 7 runtime executor is attached to this node".to_owned(),
        )
    };

    RuntimeNodeStatus {
        node_id: node.id.clone(),
        state,
        diagnostic,
    }
}

fn safe_auto_start_node(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::RtspDecoder
            | NodeKind::FrameSampler
            | NodeKind::ImageLayer
            | NodeKind::VideoLayer
            | NodeKind::OverlayComposer
            | NodeKind::Viewer
            | NodeKind::ResultView
    )
}

fn manual_node(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::SshSession
            | NodeKind::X5Device
            | NodeKind::X5RtspChannel
            | NodeKind::X5Snapshot
            | NodeKind::CalibrationSolver
            | NodeKind::I2cBusDiscovery
            | NodeKind::I2cTransfer
            | NodeKind::EepromProvision
    )
}

/// 标准化保存前的工作流图：修正 schema/revision，并拒绝运行时字段进入持久化文件。
pub fn normalize_workflow(
    mut graph: WorkflowGraph,
    revision: String,
) -> Result<WorkflowGraph, String> {
    graph.schema_version = WORKFLOW_SCHEMA_VERSION.to_owned();
    graph.revision = revision;
    for node in &mut graph.nodes {
        reject_runtime_config(&node.config)?;
        let definition = node_definition(node.kind);
        node.category = definition.category;
        node.inputs = definition.inputs;
        node.outputs = definition.outputs;
    }
    validate_workflow(&graph)?;
    Ok(graph)
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
    if edge.schema_version != WORKFLOW_SCHEMA_VERSION {
        return Err(format!(
            "edge schema version `{}` is unsupported",
            edge.schema_version
        ));
    }
    Ok(())
}

fn reject_runtime_config(config: &serde_json::Value) -> Result<(), String> {
    const FORBIDDEN_KEYS: &[&str] = &[
        "objectUrl",
        "streamSessionId",
        "decoderFrames",
        "mjpegHeaders",
        "frameBytes",
        "socketId",
        "longLogs",
    ];
    if let Some(object) = config.as_object() {
        for key in FORBIDDEN_KEYS {
            if object.contains_key(*key) {
                return Err(format!("runtime field `{key}` must not be persisted"));
            }
        }
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

fn viewer_template_graph() -> WorkflowGraph {
    let rtsp = workflow_node(
        "rtsp-source-1",
        NodeKind::RtspSource,
        "RTSP Input",
        NodePosition { x: 80.0, y: 140.0 },
    );
    let decoder = workflow_node(
        "rtsp-decoder-1",
        NodeKind::RtspDecoder,
        "RTSP Decoder",
        NodePosition { x: 380.0, y: 140.0 },
    );
    let layer = workflow_node(
        "video-layer-1",
        NodeKind::VideoLayer,
        "Video Layer",
        NodePosition { x: 700.0, y: 140.0 },
    );
    let viewer = workflow_node(
        "viewer-1",
        NodeKind::Viewer,
        "Viewer",
        NodePosition {
            x: 1000.0,
            y: 120.0,
        },
    );
    graph(
        "camera-toolbox-demo-workflow",
        "RTSP Preview Workspace",
        vec![rtsp, decoder, layer, viewer],
        vec![
            edge(
                "edge-rtsp-decoder",
                "rtsp-source-1",
                "endpoint",
                "rtsp-decoder-1",
                "endpoint",
                PortKind::EndpointRtsp,
                "media.rtsp.endpoint.v1",
            ),
            edge(
                "edge-decoder-layer",
                "rtsp-decoder-1",
                "frames",
                "video-layer-1",
                "frames",
                PortKind::StreamVideoFrame,
                "stream.video-frame.v1",
            ),
            edge(
                "edge-layer-viewer",
                "video-layer-1",
                "layer",
                "viewer-1",
                "video",
                PortKind::LayerVideo,
                "viewer.layer.video.v1",
            ),
        ],
    )
}

/// 本地图像模板只声明 workspace 根和相对文件路径；不会扫描目录或自动读取文件。
fn local_image_template_graph() -> WorkflowGraph {
    let workspace = workflow_node(
        "local-workspace-1",
        NodeKind::LocalWorkspace,
        "Local Workspace",
        NodePosition { x: 80.0, y: 140.0 },
    );
    let browser = workflow_node(
        "file-browser-1",
        NodeKind::FileBrowser,
        "File Browser",
        NodePosition { x: 340.0, y: 140.0 },
    );
    let image_source = workflow_node(
        "image-file-source-1",
        NodeKind::ImageFileSource,
        "Image File Source",
        NodePosition { x: 620.0, y: 140.0 },
    );
    let layer = workflow_node(
        "image-layer-1",
        NodeKind::ImageLayer,
        "Image Layer",
        NodePosition { x: 900.0, y: 140.0 },
    );
    let viewer = workflow_node(
        "viewer-1",
        NodeKind::Viewer,
        "Viewer",
        NodePosition {
            x: 1180.0,
            y: 120.0,
        },
    );
    graph(
        "camera-toolbox-local-image-template",
        "Local Image Workspace",
        vec![workspace, browser, image_source, layer, viewer],
        vec![
            edge(
                "local-image-e-workspace-browser",
                "local-workspace-1",
                "workspace",
                "file-browser-1",
                "local",
                PortKind::WorkspaceLocal,
                "workspace.local.v1",
            ),
            edge(
                "local-image-e-browser-source",
                "file-browser-1",
                "file",
                "image-file-source-1",
                "file",
                PortKind::ImageFrame,
                "image.frame.v1",
            ),
            edge(
                "local-image-e-source-layer",
                "image-file-source-1",
                "image",
                "image-layer-1",
                "image",
                PortKind::ImageFrame,
                "image.frame.v1",
            ),
            edge(
                "local-image-e-layer-viewer",
                "image-layer-1",
                "layer",
                "viewer-1",
                "image",
                PortKind::LayerImage,
                "viewer.layer.image.v1",
            ),
        ],
    )
}

fn calibration_template_graph() -> WorkflowGraph {
    let mut nodes = vec![
        workflow_node(
            "calib-rtsp-source",
            NodeKind::RtspSource,
            "RTSP Input",
            NodePosition { x: 60.0, y: 120.0 },
        ),
        workflow_node(
            "calib-decoder",
            NodeKind::RtspDecoder,
            "RTSP Decoder",
            NodePosition { x: 320.0, y: 120.0 },
        ),
        workflow_node(
            "calib-detector",
            NodeKind::ChessboardDetector,
            "Chessboard Detector",
            NodePosition { x: 600.0, y: 100.0 },
        ),
        workflow_node(
            "calib-dataset",
            NodeKind::DatasetCollector,
            "Dataset Collector",
            NodePosition { x: 900.0, y: 80.0 },
        ),
        workflow_node(
            "calib-coverage",
            NodeKind::CoverageAnalyzer,
            "Coverage Analyzer",
            NodePosition { x: 1180.0, y: 80.0 },
        ),
        workflow_node(
            "calib-solver",
            NodeKind::CalibrationSolver,
            "Calibration Solver",
            NodePosition { x: 1460.0, y: 80.0 },
        ),
        workflow_node(
            "calib-scorer",
            NodeKind::CaptureScorer,
            "Capture Scorer",
            NodePosition { x: 900.0, y: 300.0 },
        ),
        workflow_node(
            "calib-autocapture",
            NodeKind::AutoCaptureController,
            "Auto Capture",
            NodePosition {
                x: 1180.0,
                y: 300.0,
            },
        ),
        workflow_node(
            "calib-pose-guide",
            NodeKind::PoseGuide,
            "Pose Guide",
            NodePosition {
                x: 1460.0,
                y: 300.0,
            },
        ),
    ];
    nodes.push(workflow_node(
        "calib-viewer",
        NodeKind::Viewer,
        "Viewer",
        NodePosition {
            x: 1180.0,
            y: 520.0,
        },
    ));
    graph(
        "camera-toolbox-calibration-template",
        "Calibration Workspace",
        nodes,
        vec![
            edge(
                "calib-e-rtsp-decoder",
                "calib-rtsp-source",
                "endpoint",
                "calib-decoder",
                "endpoint",
                PortKind::EndpointRtsp,
                "media.rtsp.endpoint.v1",
            ),
            edge(
                "calib-e-decoder-detector",
                "calib-decoder",
                "frames",
                "calib-detector",
                "frames",
                PortKind::StreamVideoFrame,
                "stream.video-frame.v1",
            ),
            edge(
                "calib-e-detection-dataset",
                "calib-detector",
                "detection",
                "calib-dataset",
                "detection",
                PortKind::CalibDetection,
                "calib.detection.v1",
            ),
            edge(
                "calib-e-dataset-coverage",
                "calib-dataset",
                "dataset",
                "calib-coverage",
                "dataset",
                PortKind::CalibDataset,
                "calib.dataset.v1",
            ),
            edge(
                "calib-e-dataset-solver",
                "calib-dataset",
                "dataset",
                "calib-solver",
                "dataset",
                PortKind::CalibDataset,
                "calib.dataset.v1",
            ),
            edge(
                "calib-e-detection-scorer",
                "calib-detector",
                "detection",
                "calib-scorer",
                "detection",
                PortKind::CalibDetection,
                "calib.detection.v1",
            ),
            edge(
                "calib-e-score-autocapture",
                "calib-scorer",
                "score",
                "calib-autocapture",
                "score",
                PortKind::CaptureScore,
                "capture.score.v1",
            ),
            edge(
                "calib-e-coverage-pose",
                "calib-coverage",
                "coverage",
                "calib-pose-guide",
                "coverage",
                PortKind::CalibCoverage,
                "calib.coverage.v1",
            ),
            edge(
                "calib-e-target-autocapture",
                "calib-pose-guide",
                "target",
                "calib-autocapture",
                "target",
                PortKind::CaptureTarget,
                "capture.target.v1",
            ),
        ],
    )
}

fn i2c_template_graph() -> WorkflowGraph {
    graph(
        "camera-toolbox-i2c-template",
        "I²C Tools Workspace",
        vec![
            workflow_node(
                "i2c-ssh",
                NodeKind::SshSession,
                "SSH Session",
                NodePosition { x: 80.0, y: 120.0 },
            ),
            workflow_node(
                "i2c-bus",
                NodeKind::I2cBusDiscovery,
                "I²C Bus Discovery",
                NodePosition { x: 380.0, y: 120.0 },
            ),
            workflow_node(
                "i2c-transfer",
                NodeKind::I2cTransfer,
                "I²C Transfer",
                NodePosition { x: 700.0, y: 80.0 },
            ),
            workflow_node(
                "i2c-eeprom-map",
                NodeKind::EepromMapLoader,
                "EEPROM Map",
                NodePosition { x: 380.0, y: 320.0 },
            ),
            workflow_node(
                "i2c-eeprom",
                NodeKind::EepromProvision,
                "EEPROM Provision",
                NodePosition { x: 700.0, y: 320.0 },
            ),
            workflow_node(
                "i2c-result",
                NodeKind::ResultView,
                "Result View",
                NodePosition {
                    x: 1040.0,
                    y: 120.0,
                },
            ),
        ],
        vec![
            edge(
                "i2c-e-ssh-bus",
                "i2c-ssh",
                "ssh",
                "i2c-bus",
                "ssh",
                PortKind::ControlSsh,
                "control.ssh.v1",
            ),
            edge(
                "i2c-e-ssh-transfer",
                "i2c-ssh",
                "ssh",
                "i2c-transfer",
                "ssh",
                PortKind::ControlSsh,
                "control.ssh.v1",
            ),
            edge(
                "i2c-e-bus-transfer",
                "i2c-bus",
                "bus",
                "i2c-transfer",
                "bus",
                PortKind::I2cBus,
                "i2c.bus.v1",
            ),
            edge(
                "i2c-e-result-view",
                "i2c-transfer",
                "result",
                "i2c-result",
                "result",
                PortKind::I2cResult,
                "i2c.result.v1",
            ),
            edge(
                "i2c-e-map-eeprom",
                "i2c-eeprom-map",
                "map",
                "i2c-eeprom",
                "map",
                PortKind::EepromMap,
                "eeprom.map.v1",
            ),
            edge(
                "i2c-e-bus-eeprom",
                "i2c-bus",
                "bus",
                "i2c-eeprom",
                "bus",
                PortKind::I2cBus,
                "i2c.bus.v1",
            ),
        ],
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
            | NodeKind::LocalWorkspace
            | NodeKind::SshSession
            | NodeKind::X5Device => NodeRuntimeState::Ready,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_graph_contains_only_valid_edges() {
        let graph = seed_workflow_graph();
        validate_workflow(&graph).expect("seed graph is valid");
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.kind == PortKind::EndpointRtsp)
        );
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.kind == PortKind::LayerVideo)
        );
    }

    #[test]
    fn validation_rejects_self_loop() {
        let graph = seed_workflow_graph();
        let edge = WorkflowEdge {
            id: "bad".to_owned(),
            source: PortEndpoint {
                node_id: "rtsp-source-1".to_owned(),
                port_id: "endpoint".to_owned(),
            },
            target: PortEndpoint {
                node_id: "rtsp-source-1".to_owned(),
                port_id: "endpoint".to_owned(),
            },
            kind: PortKind::EndpointRtsp,
            schema: "media.rtsp.endpoint.v1".to_owned(),
            schema_version: WORKFLOW_SCHEMA_VERSION.to_owned(),
        };
        assert!(validate_edge(&graph, &edge).is_err());
    }

    #[test]
    fn validation_rejects_incompatible_port_kinds() {
        let mut graph = seed_workflow_graph();
        let bad = WorkflowEdge {
            id: "bad".to_owned(),
            source: PortEndpoint {
                node_id: "rtsp-source-1".to_owned(),
                port_id: "endpoint".to_owned(),
            },
            target: PortEndpoint {
                node_id: "viewer-1".to_owned(),
                port_id: "video".to_owned(),
            },
            kind: PortKind::EndpointRtsp,
            schema: "media.rtsp.endpoint.v1".to_owned(),
            schema_version: WORKFLOW_SCHEMA_VERSION.to_owned(),
        };
        graph.edges.push(bad.clone());
        assert!(validate_edge(&graph, &bad).is_err());
    }

    #[test]
    fn templates_generate_valid_graphs() {
        for template in workmode_templates() {
            validate_workflow(&template.graph).expect(template.id);
        }
    }

    #[test]
    fn normalize_rejects_runtime_config_fields() {
        let mut graph = seed_workflow_graph();
        graph.nodes[0].config["objectUrl"] = json!("blob:runtime");
        let error =
            normalize_workflow(graph, "next".to_owned()).expect_err("runtime fields are rejected");
        assert!(error.contains("objectUrl"));
    }
}
