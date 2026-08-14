//! 引擎内置节点实现。

pub mod auto_capture;
pub mod calibration_solver;
pub mod rtsp_source;
pub mod transform;
pub mod viewer;

use crate::engine::NodeRegistry;

pub use auto_capture::{AutoCaptureFactory, AutoCaptureNode};
pub use calibration_solver::{CalibrationSolverFactory, CalibrationSolverNode};
pub use rtsp_source::{RtspSourceFactory, RtspSourceNode};
pub use transform::{
    FrameSamplerFactory, FrameSamplerNode, ImageLayerFactory, PassThroughNode, RtspDecoderFactory,
    VideoLayerFactory,
};
pub use viewer::{ViewerFactory, ViewerNode};

/// 注册引擎内置节点到注册表。
pub fn register_builtin(registry: &mut NodeRegistry) {
    registry.register(Box::new(RtspSourceFactory));
    registry.register(Box::new(RtspDecoderFactory));
    registry.register(Box::new(FrameSamplerFactory));
    registry.register(Box::new(VideoLayerFactory));
    registry.register(Box::new(ImageLayerFactory));
    registry.register(Box::new(ViewerFactory));
    registry.register(Box::new(CalibrationSolverFactory));
    registry.register(Box::new(AutoCaptureFactory));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_covers_all_nodes() {
        let mut registry = NodeRegistry::new();
        register_builtin(&mut registry);
        let kinds: Vec<&str> = registry.kinds().collect();
        for expected in [
            "rtspSource",
            "rtspDecoder",
            "frameSampler",
            "videoLayer",
            "imageLayer",
            "viewer",
            "calibrationSolver",
            "autoCaptureController",
        ] {
            assert!(kinds.contains(&expected), "missing node kind {expected}");
        }
    }
}
