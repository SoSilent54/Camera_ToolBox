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
    X5Device,
    RtspDecoder,
    FrameSampler,
    ImageLayer,
    VideoLayer,
    OverlayComposer,
    Viewer,
    ChessboardDetector,
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
        node_definition(NodeKind::LocalFileSource),
        node_definition(NodeKind::SftpFileSource),
        node_definition(NodeKind::RtspSource),
        node_definition(NodeKind::SshSession),
        node_definition(NodeKind::X5Device),
        node_definition(NodeKind::RtspDecoder),
        node_definition(NodeKind::FrameSampler),
        node_definition(NodeKind::ImageLayer),
        node_definition(NodeKind::VideoLayer),
        node_definition(NodeKind::OverlayComposer),
        node_definition(NodeKind::Viewer),
        node_definition(NodeKind::ChessboardDetector),
        node_definition(NodeKind::DatasetCollector),
        node_definition(NodeKind::CoverageAnalyzer),
        node_definition(NodeKind::AutoCaptureController),
        node_definition(NodeKind::CalibrationSolver),
        node_definition(NodeKind::PoseGuide),
        node_definition(NodeKind::I2cTransfer),
        node_definition(NodeKind::EepromProvision),
    ]
}

pub fn node_definition(kind: NodeKind) -> NodeDefinition {
    let (category, title, description, inputs, outputs, default_config) = match kind {
        // ① LocalFileSource（吸收 LocalWorkspace + FileBrowser + ImageFileSource）
        NodeKind::LocalFileSource => (
            NodeCategory::Workspace,
            "Local File Source",
            "浏览本地目录并加载单张图片帧（内嵌资源根、目录浏览、文件选择与 filter）",
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
            json!({"root": "", "directory": "", "selection": "", "filter": "*.png;*.jpg;*.jpeg", "reload": "manual"}),
        ),
        // ② SftpFileSource（吸收 SftpWorkspace + FileBrowser remote）
        NodeKind::SftpFileSource => (
            NodeCategory::Workspace,
            "SFTP File Source",
            "通过 SSH/SFTP 暴露远程目录并加载图片（保留 workspace 可选输出供多消费者复用）",
            vec![port(
                "ssh",
                "SSH",
                PortDirection::Input,
                PortKind::ControlSsh,
                "control.ssh.v1",
                Some(PortRole::Control),
            )],
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
                    "fileRef",
                    "File Ref",
                    PortDirection::Output,
                    PortKind::FileRef,
                    "file.ref.v1",
                    Some(PortRole::Image),
                ),
                optional_port(
                    "workspace",
                    "Remote Workspace",
                    PortDirection::Output,
                    PortKind::WorkspaceRemoteSftp,
                    "workspace.remote.sftp.v1",
                    Some(PortRole::Workspace),
                ),
            ],
            json!({"sourceId": "sftp-main", "remoteRoot": "/", "mountLabel": "Remote SFTP", "selection": "", "filter": "*.png;*.jpg;*.jpeg"}),
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
            json!({"profileId": "", "host": "", "port": "22", "username": "root", "autoConnect": false}),
        ),
        // ③ X5Device（吸收 X5RtspChannel + X5Snapshot）
        NodeKind::X5Device => (
            NodeCategory::Control,
            "X5 Device",
            "X5_233 TCP 控制、RTSP 通道资源与抓帧（rtsp 多路 + snapshot/video 可选输出）",
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
                optional_port(
                    "snapshot",
                    "Snapshot Image",
                    PortDirection::Output,
                    PortKind::ImageFrame,
                    "image.frame.v1",
                    Some(PortRole::Image),
                ),
                optional_port(
                    "video",
                    "Video Frames",
                    PortDirection::Output,
                    PortKind::StreamVideoFrame,
                    "stream.video-frame.v1",
                    Some(PortRole::Stream),
                ),
            ],
            json!({"host": "10.21.12.108", "tcpPort": 9073, "fps": 60, "bitrateKbps": 12000, "channels": [0]}),
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
                    "Video Frames",
                    PortDirection::Input,
                    PortKind::StreamVideoFrame,
                    "stream.video-frame.v1",
                    Some(PortRole::Stream),
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
        // ⑥ AutoCaptureController（吸收 CaptureScorer）
        NodeKind::AutoCaptureController => (
            NodeCategory::Calibration,
            "Auto Capture",
            "把评分、帧流和目标位姿转换为抓帧命令（内嵌评分，score 可选输出保留原始能力）",
            vec![
                optional_port(
                    "frames",
                    "Video Frames",
                    PortDirection::Input,
                    PortKind::StreamVideoFrame,
                    "stream.video-frame.v1",
                    Some(PortRole::Stream),
                ),
                optional_port(
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
                optional_port(
                    "target",
                    "Capture Target",
                    PortDirection::Input,
                    PortKind::CaptureTarget,
                    "capture.target.v1",
                    Some(PortRole::Command),
                ),
            ],
            vec![
                port(
                    "command",
                    "Capture Command",
                    PortDirection::Output,
                    PortKind::CommandCapture,
                    "command.capture.v1",
                    Some(PortRole::Command),
                ),
                optional_port(
                    "score",
                    "Capture Score",
                    PortDirection::Output,
                    PortKind::CaptureScore,
                    "capture.score.v1",
                    Some(PortRole::Status),
                ),
            ],
            json!({"armed": false, "strategy": "datasetGain", "cooldownMs": 800}),
        ),
        // ⑦ CalibrationSolver（吸收 ReprojectionInspector + CalibrationExport）
        NodeKind::CalibrationSolver => (
            NodeCategory::Calibration,
            "Calibration Solver",
            "手动触发标定求解（内嵌重投影检查与导出，reprojection/report/payload 可选输出）",
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
                    "solution",
                    "Solution",
                    PortDirection::Output,
                    PortKind::CalibSolution,
                    "calib.solution.v1",
                    Some(PortRole::Solution),
                ),
                optional_port(
                    "reprojection",
                    "Reprojection Overlay",
                    PortDirection::Output,
                    PortKind::LayerOverlay,
                    "viewer.layer.overlay.v1",
                    Some(PortRole::Overlay),
                ),
                optional_port(
                    "report",
                    "Report",
                    PortDirection::Output,
                    PortKind::CalibReport,
                    "calib.report.v1",
                    Some(PortRole::Solution),
                ),
                optional_port(
                    "payload",
                    "EEPROM Payload",
                    PortDirection::Output,
                    PortKind::EepromPayload,
                    "eeprom.payload.v1",
                    Some(PortRole::Command),
                ),
            ],
            json!({
                "model": "pinhole",
                "trigger": "manual",
                "boardCols": 8,
                "boardRows": 11,
                "squareSizeMm": 30.0,
                "imageWidth": 1920,
                "imageHeight": 1080,
                "fx": 1234.56,
                "fy": 1234.56,
                "cx": 960.0,
                "cy": 540.0,
                "distortionCoefficients": vec![0.0; 12],
                "maxResidualPx": 1.0,
                "format": "yaml",
            }),
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
        // ④ I2cTransfer（吸收 I2cBusDiscovery）
        NodeKind::I2cTransfer => (
            NodeCategory::Control,
            "I²C Transfer",
            "预览并手动执行 I²C 读写请求（内嵌 bus 刷新，rawResps 可选输出原始响应）",
            vec![
                port(
                    "ssh",
                    "SSH",
                    PortDirection::Input,
                    PortKind::ControlSsh,
                    "control.ssh.v1",
                    Some(PortRole::Control),
                ),
                optional_port(
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
                    "result",
                    "Result",
                    PortDirection::Output,
                    PortKind::I2cResult,
                    "i2c.result.v1",
                    Some(PortRole::Status),
                ),
                optional_port(
                    "rawResps",
                    "Raw Responses",
                    PortDirection::Output,
                    PortKind::I2cResult,
                    "i2c.result.v1",
                    Some(PortRole::Status),
                ),
            ],
            json!({"profileId": "x5-lab", "bus": "i2c-1", "address": "0x50", "register": "0x0000", "payload": "", "pageSize": 16, "mode": "read", "confirmWrites": true}),
        ),
        // ⑤ EepromProvision（吸收 EepromMapLoader）
        NodeKind::EepromProvision => (
            NodeCategory::Control,
            "EEPROM Provision",
            "根据 map/payload/bus 生成写入与校验动作（内嵌 map 加载，transfer 可选输出）",
            vec![
                port(
                    "ssh",
                    "SSH",
                    PortDirection::Input,
                    PortKind::ControlSsh,
                    "control.ssh.v1",
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
                optional_port(
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
            ],
            vec![
                port(
                    "result",
                    "Result",
                    PortDirection::Output,
                    PortKind::I2cResult,
                    "i2c.result.v1",
                    Some(PortRole::Status),
                ),
                optional_port(
                    "transfer",
                    "Transfer",
                    PortDirection::Output,
                    PortKind::I2cTransfer,
                    "i2c.transfer.v1",
                    Some(PortRole::Command),
                ),
            ],
            json!({"profileId": "x5-lab", "bus": "i2c-1", "address": "0x50", "register": "0x0010", "payload": "", "pageSize": 32, "mapId": "yg-stereo-p24c64g-v1", "confirmWrites": true, "verifyAfterWrite": true}),
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
            description: "RTSP → Decoder → Detector → Dataset / Coverage / AutoCapture / Reprojection / Export / Solver",
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
    for node in &graph.nodes {
        validate_node_config(node)?;
    }
    for edge in &graph.edges {
        validate_edge(graph, edge)?;
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

fn validate_node_config(node: &WorkflowNode) -> Result<(), String> {
    match node.kind {
        NodeKind::SshSession => validate_ssh_session_config(node),
        NodeKind::SftpFileSource => validate_sftp_file_source_config(node),
        NodeKind::LocalFileSource => validate_local_file_source_config(node),
        _ => Ok(()),
    }
}

fn validate_ssh_session_config(node: &WorkflowNode) -> Result<(), String> {
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
    Ok(())
}

fn validate_sftp_file_source_config(node: &WorkflowNode) -> Result<(), String> {
    let config = node_config_object(node)?;
    let source_id = required_config_string(node, config, "sourceId")?;
    validate_file_source_id(node, "sourceId", source_id)?;
    let remote_root = required_config_string(node, config, "remoteRoot")?;
    validate_remote_root(node, remote_root)?;
    if let Some(label) = config_string(config, "mountLabel") {
        validate_printable_config_text(node, "mountLabel", label)?;
    }
    Ok(())
}

fn validate_local_file_source_config(node: &WorkflowNode) -> Result<(), String> {
    let config = node_config_object(node)?;
    if let Some(root) = config_string(config, "root") {
        validate_source_relative_path(node, "root", root, true)?;
    }
    if let Some(directory) = config_string(config, "directory") {
        validate_source_relative_path(node, "directory", directory, true)?;
    }
    if let Some(selection) = config_string(config, "selection") {
        validate_source_relative_path(node, "selection", selection, true)?;
    }
    if let Some(filter) = config_string(config, "filter") {
        validate_printable_config_text(node, "filter", filter)?;
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
        "password",
        "credential",
        "credentialRef",
        "privateKey",
        "secret",
    ];
    // 递归遍历所有嵌套 object 与数组元素，堵住 {"auth":{"password":...}} 这类落盘绕过；
    // 旧实现只检查顶层 object，嵌套 secret 会原样写入 .ctworkflow.json。
    fn walk(value: &serde_json::Value) -> Result<(), String> {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    if FORBIDDEN_KEYS.contains(&key.as_str()) {
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

fn required_config_string<'a>(
    node: &WorkflowNode,
    config: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<&'a str, String> {
    config_string(config, key)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("node `{}` config `{key}` must not be empty", node.id))
}

fn config_string<'a>(
    config: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    config.get(key).and_then(serde_json::Value::as_str)
}

fn validate_file_source_id(node: &WorkflowNode, key: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return Err(format!(
            "node `{}` config `{key}` must be a non-empty source id without path separators",
            node.id
        ));
    }
    Ok(())
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

/// 本地图像模板：LocalFileSource 已吸收 workspace/浏览/加载，默认路径只需 source→layer→viewer。
fn local_image_template_graph() -> WorkflowGraph {
    let source = workflow_node(
        "local-file-source-1",
        NodeKind::LocalFileSource,
        "Local File Source",
        NodePosition { x: 80.0, y: 140.0 },
    );
    let layer = workflow_node(
        "image-layer-1",
        NodeKind::ImageLayer,
        "Image Layer",
        NodePosition { x: 420.0, y: 140.0 },
    );
    let viewer = workflow_node(
        "viewer-1",
        NodeKind::Viewer,
        "Viewer",
        NodePosition { x: 760.0, y: 120.0 },
    );
    graph(
        "camera-toolbox-local-image-template",
        "Local Image Workspace",
        vec![source, layer, viewer],
        vec![
            edge(
                "local-image-e-source-layer",
                "local-file-source-1",
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
    let nodes = vec![
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
            NodePosition { x: 620.0, y: 120.0 },
        ),
        workflow_node(
            "calib-dataset",
            NodeKind::DatasetCollector,
            "Dataset Collector",
            NodePosition { x: 940.0, y: 80.0 },
        ),
        workflow_node(
            "calib-coverage",
            NodeKind::CoverageAnalyzer,
            "Coverage Analyzer",
            NodePosition { x: 1240.0, y: 80.0 },
        ),
        workflow_node(
            "calib-solver",
            NodeKind::CalibrationSolver,
            "Calibration Solver",
            NodePosition { x: 1540.0, y: 80.0 },
        ),
        workflow_node(
            "calib-autocapture",
            NodeKind::AutoCaptureController,
            "Auto Capture",
            NodePosition {
                x: 1240.0,
                y: 300.0,
            },
        ),
        workflow_node(
            "calib-pose-guide",
            NodeKind::PoseGuide,
            "Pose Guide",
            NodePosition {
                x: 1540.0,
                y: 300.0,
            },
        ),
        workflow_node(
            "calib-overlay",
            NodeKind::OverlayComposer,
            "Overlay Composer",
            NodePosition { x: 940.0, y: 520.0 },
        ),
        workflow_node(
            "calib-viewer",
            NodeKind::Viewer,
            "Viewer",
            NodePosition {
                x: 1240.0,
                y: 520.0,
            },
        ),
    ];
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
                "calib-e-decoder-dataset-image",
                "calib-decoder",
                "frames",
                "calib-dataset",
                "image",
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
                "calib-e-detection-autocapture",
                "calib-detector",
                "detection",
                "calib-autocapture",
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
                "calib-e-coverage-autocapture",
                "calib-coverage",
                "coverage",
                "calib-autocapture",
                "coverage",
                PortKind::CalibCoverage,
                "calib.coverage.v1",
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
            edge(
                "calib-e-solver-reprojection-overlay",
                "calib-solver",
                "reprojection",
                "calib-overlay",
                "overlay",
                PortKind::LayerOverlay,
                "viewer.layer.overlay.v1",
            ),
            edge(
                "calib-e-video-overlay",
                "calib-detector",
                "overlay",
                "calib-overlay",
                "overlay",
                PortKind::LayerOverlay,
                "viewer.layer.overlay.v1",
            ),
            edge(
                "calib-e-coverage-overlay",
                "calib-coverage",
                "overlay",
                "calib-overlay",
                "overlay",
                PortKind::LayerOverlay,
                "viewer.layer.overlay.v1",
            ),
            edge(
                "calib-e-pose-overlay",
                "calib-pose-guide",
                "overlay",
                "calib-overlay",
                "overlay",
                PortKind::LayerOverlay,
                "viewer.layer.overlay.v1",
            ),
            edge(
                "calib-e-overlay-viewer",
                "calib-overlay",
                "scene",
                "calib-viewer",
                "scene",
                PortKind::ViewerScene,
                "viewer.scene.v1",
            ),
        ],
    )
}

fn i2c_template_graph() -> WorkflowGraph {
    // I2cBusDiscovery 已并入 I2cTransfer，EepromMapLoader 已并入 EepromProvision，
    // ResultView 已删除（结果改节点内嵌 + 图级 Console）。
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
                "i2c-transfer",
                NodeKind::I2cTransfer,
                "I²C Transfer",
                NodePosition { x: 420.0, y: 80.0 },
            ),
            workflow_node(
                "i2c-eeprom",
                NodeKind::EepromProvision,
                "EEPROM Provision",
                NodePosition { x: 420.0, y: 320.0 },
            ),
        ],
        vec![
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
                "i2c-e-ssh-eeprom",
                "i2c-ssh",
                "ssh",
                "i2c-eeprom",
                "ssh",
                PortKind::ControlSsh,
                "control.ssh.v1",
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
    fn calibration_template_contains_full_dataset_chain() {
        let graph = calibration_template_graph();
        validate_workflow(&graph).expect("calibration template is valid");
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::DatasetCollector)
        );
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::CalibrationSolver)
        );
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::AutoCaptureController)
        );
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::OverlayComposer)
        );
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.id == "calib-e-decoder-dataset-image")
        );
        // 合并后 solver 的 reprojection 可选输出直接进 overlay，不再有独立 ReprojectionInspector。
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.id == "calib-e-solver-reprojection-overlay")
        );
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.id == "calib-e-overlay-viewer")
        );
    }

    #[test]
    fn node_catalog_has_19_nodes() {
        let catalog = node_catalog();
        assert_eq!(catalog.len(), 19);
        // 每个 kind 都能通过 node_definition 展开，且无重复。
        let kinds: Vec<NodeKind> = catalog.iter().map(|def| def.kind).collect();
        assert_eq!(kinds.len(), 19);
    }

    #[test]
    fn merged_nodes_have_expanded_port_surface() {
        // X5Device：control + rtsp(多路) + snapshot(可选 image) + video(可选 video-frame)。
        let x5 = node_definition(NodeKind::X5Device);
        assert_eq!(x5.outputs.len(), 4);
        assert!(x5.outputs.iter().any(|p| p.id == "rtsp" && p.cardinality == PortCardinality::Many));
        assert!(x5.outputs.iter().any(|p| p.id == "snapshot" && p.kind == PortKind::ImageFrame && !p.required));
        assert!(x5.outputs.iter().any(|p| p.id == "video" && p.kind == PortKind::StreamVideoFrame));

        // CalibrationSolver：solution + reprojection/report/payload 三个可选输出。
        let solver = node_definition(NodeKind::CalibrationSolver);
        assert_eq!(solver.outputs.len(), 4);
        assert!(solver.outputs.iter().any(|p| p.id == "solution" && p.kind == PortKind::CalibSolution));
        assert!(solver.outputs.iter().any(|p| p.id == "reprojection" && p.kind == PortKind::LayerOverlay && !p.required));
        assert!(solver.outputs.iter().any(|p| p.id == "report" && p.kind == PortKind::CalibReport && !p.required));
        assert!(solver.outputs.iter().any(|p| p.id == "payload" && p.kind == PortKind::EepromPayload && !p.required));

        // AutoCaptureController：frames/detection/coverage/target 输入 + command/score 输出。
        let auto = node_definition(NodeKind::AutoCaptureController);
        assert_eq!(auto.inputs.len(), 4);
        assert_eq!(auto.outputs.len(), 2);
        assert!(auto.inputs.iter().any(|p| p.id == "detection"));
        assert!(auto.inputs.iter().any(|p| p.id == "coverage"));
        assert!(auto.outputs.iter().any(|p| p.id == "command"));
        assert!(auto.outputs.iter().any(|p| p.id == "score" && p.kind == PortKind::CaptureScore && !p.required));

        // LocalFileSource：image + preview/fileRef 可选输出。
        let local = node_definition(NodeKind::LocalFileSource);
        assert_eq!(local.outputs.len(), 3);
        assert!(local.outputs.iter().any(|p| p.id == "image" && p.kind == PortKind::ImageFrame));
        assert!(local.outputs.iter().any(|p| p.id == "preview" && !p.required));
        assert!(local.outputs.iter().any(|p| p.id == "fileRef" && p.kind == PortKind::FileRef && !p.required));
    }

    #[test]
    fn templates_generate_valid_graphs() {
        for template in workmode_templates() {
            validate_workflow(&template.graph).expect(template.id);
        }
    }

    #[test]
    fn local_file_source_emits_image_and_file_ref() {
        let source = node_definition(NodeKind::LocalFileSource);
        assert!(source.outputs.iter().any(|p| p.kind == PortKind::ImageFrame));
        assert!(source.outputs.iter().any(|p| p.kind == PortKind::FileRef && p.schema == "file.ref.v1"));
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

        // 数组元素里携带 secret 也必须被拒。
        let array = json!({"tokens": [{"credentialRef": "key-file:/x"}]});
        let error = reject_runtime_config(&array).expect_err("array secret must be rejected");
        assert!(error.contains("credentialRef"));

        // 合法嵌套（不含敏感键）应通过。
        let benign = json!({"auth": {"host": "camera.local", "port": 22}});
        assert!(reject_runtime_config(&benign).is_ok());
    }
}
