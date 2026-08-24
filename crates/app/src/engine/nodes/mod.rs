//! 引擎内置节点实现。
//!
//! # 节点开发范式
//!
//! 新增一个节点（以「源/转换/按钮/自动」四类之一）的固定步骤：
//!
//! 1. 新建 `nodes/<name>.rs`，实现 `NodeFactory`（kind + `instantiate`）与 `NodeInstance`
//!    （`on_start` / `on_input` / `on_action` / `on_stop`）。
//! 2. 在 `register_builtin` 里 `registry.register(Box::new(YourFactory))`。
//! 3. 若需要新的负载类型，在 `packet.rs` 的 `DataPacket` 增加变体。
//!
//! 四类节点触发语义（范式样板）：
//!
//! | 类别 | 样板 | 触发方式 | 关键方法 |
//! |---|---|---|---|
//! | 源 source | `rtsp_source.rs` | `on_action(Connect)` 启动生产循环 | `on_action` + `spawn` |
//! | 转换 transform | `transform.rs` | 输入触发：`on_input` 变换后 `emit` | `on_input` |
//! | 按钮 action | `calibration_solver.rs` | `on_action(Trigger)` 一次执行 | `on_action` + `services` |
//! | 自动 auto | `auto_capture.rs` | `Arm` 后条件自动触发 | `on_action` + `on_input` |
//!
//! 节点只依赖 `NodeRuntime`（`emit`/`report_state`/`report_event`/`spawn`）与
//! `EngineServices` 的 trait；外部 IO（RTSP/SSH/X5/标定）一律经服务注入，不在节点内构造具体适配器。

pub mod auto_capture;
pub mod calibration_params;
pub mod calibration_solver;
pub mod capture_logic;
pub mod composite;
pub mod control_nodes;
pub mod detection;
pub mod i2c_plan_nodes;
pub mod local_source;
pub mod pose_estimator;
pub mod rtsp_source;
pub mod structured_field_extractor;
pub mod transform;
pub mod viewer;

use crate::engine::NodeRegistry;

pub use auto_capture::{AutoCaptureFactory, AutoCaptureNode};
pub use calibration_params::{
    CalibrationBoardParamsFactory, CalibrationBoardParamsNode, CameraInitialParamsFactory,
    CameraInitialParamsNode,
};
pub use calibration_solver::{CalibrationSolverFactory, CalibrationSolverNode};
pub use capture_logic::{
    CalibrationFrameScorerFactory, CalibrationFrameScorerNode, CaptureRequestBuilderFactory,
    CaptureRequestBuilderNode, ConsecutiveHoldGateFactory, ConsecutiveHoldGateNode,
    ScoreThresholdGateFactory, ScoreThresholdGateNode,
};
pub use composite::{
    CoverageAnalyzerFactory, CoverageAnalyzerNode, DatasetCollectorFactory, DatasetCollectorNode,
    OverlayComposerFactory, OverlayComposerNode, PoseGuideFactory, PoseGuideNode,
};
pub use control_nodes::{
    HexArmDeviceFactory, HexArmDeviceNode, SftpFileSourceFactory, SftpFileSourceNode,
    SshSessionFactory, SshSessionNode, X5233DriverFactory, X5233DriverNode,
};
pub use detection::{ChessboardDetectorFactory, ChessboardDetectorNode};
pub use i2c_plan_nodes::{I2cTaskBuilderFactory, I2cTaskBuilderNode, SshConnectionFactory, SshConnectionNode};
pub use local_source::{LocalFileSourceFactory, LocalFileSourceNode};
pub use pose_estimator::{PoseEstimatorFactory, PoseEstimatorNode};
pub use rtsp_source::{RtspSourceFactory, RtspSourceNode};
pub use structured_field_extractor::{
    StructuredFieldExtractorFactory, StructuredFieldExtractorNode,
};
pub use transform::{
    DemosaicFactory, DemosaicNode, FrameSamplerFactory, FrameSamplerNode, ImageLayerFactory,
    PassThroughNode, RtspDecoderFactory, VideoLayerFactory,
};
pub use viewer::{ViewerFactory, ViewerNode};

/// 注册引擎内置节点到注册表。
pub fn register_builtin(registry: &mut NodeRegistry) {
    registry.register(Box::new(RtspSourceFactory));
    registry.register(Box::new(RtspDecoderFactory));
    registry.register(Box::new(FrameSamplerFactory));
    registry.register(Box::new(VideoLayerFactory));
    registry.register(Box::new(DemosaicFactory));
    registry.register(Box::new(ImageLayerFactory));
    registry.register(Box::new(ViewerFactory));
    registry.register(Box::new(CalibrationBoardParamsFactory));
    registry.register(Box::new(CameraInitialParamsFactory));
    registry.register(Box::new(CalibrationSolverFactory));
    registry.register(Box::new(AutoCaptureFactory));
    registry.register(Box::new(OverlayComposerFactory));
    registry.register(Box::new(DatasetCollectorFactory));
    registry.register(Box::new(CoverageAnalyzerFactory));
    registry.register(Box::new(PoseGuideFactory));
    registry.register(Box::new(PoseEstimatorFactory));
    registry.register(Box::new(ChessboardDetectorFactory));
    registry.register(Box::new(CalibrationFrameScorerFactory));
    registry.register(Box::new(ScoreThresholdGateFactory));
    registry.register(Box::new(ConsecutiveHoldGateFactory));
    registry.register(Box::new(CaptureRequestBuilderFactory));
    registry.register(Box::new(LocalFileSourceFactory));
    registry.register(Box::new(SftpFileSourceFactory));
    registry.register(Box::new(SshSessionFactory));
    registry.register(Box::new(X5233DriverFactory));
    registry.register(Box::new(HexArmDeviceFactory));
    registry.register(Box::new(SshConnectionFactory));
    registry.register(Box::new(StructuredFieldExtractorFactory));
    registry.register(Box::new(I2cTaskBuilderFactory));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_covers_all_nodes() {
        let mut registry = NodeRegistry::new();
        register_builtin(&mut registry);
        let kinds: Vec<&str> = registry.kinds().collect();
        // 全部内置 NodeKind 均已注册（含采集逻辑与 Hex Arm 控制节点）。
        for expected in [
            "rtspSource",
            "rtspDecoder",
            "frameSampler",
            "demosaic",
            "videoLayer",
            "imageLayer",
            "viewer",
            "calibrationBoardParams",
            "cameraInitialParams",
            "calibrationSolver",
            "autoCaptureController",
            "overlayComposer",
            "datasetCollector",
            "coverageAnalyzer",
            "poseGuide",
            "chessboardDetector",
            "poseEstimator",
            "calibrationFrameScorer",
            "scoreThresholdGate",
            "consecutiveHoldGate",
            "captureRequestBuilder",
            "localFileSource",
            "sftpFileSource",
            "sshSession",
            "x5233Driver",
            "sshConnection",
            "structuredFieldExtractor",
            "i2cTaskBuilder",
        ] {
            assert!(kinds.contains(&expected), "missing node kind {expected}");
        }
    }
}
