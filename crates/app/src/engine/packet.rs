//! 节点间统一数据负载。
//!
//! 大负载（帧、检测结果、解）一律用 `Arc` 零拷贝传递；`Clone` 仅复制句柄。

use std::sync::Arc;

use camera_toolbox_core::{CalibrationSolution, ChessboardDetection};

use crate::platform::DecodedVideoFrame;

/// 节点间流动的统一数据包。
///
/// 变体与端口 `kind` 对应；骨架阶段先落地帧与解，其余负载类型随对应节点补齐。
#[derive(Clone)]
pub enum DataPacket {
    /// `stream.video-frame`：视频帧流。
    VideoFrame(Arc<DecodedVideoFrame>),
    /// `image.frame`：单张图片帧。
    ImageFrame(Arc<DecodedVideoFrame>),
    /// `calib.detection`：棋盘格检测结果。
    Detection(Arc<ChessboardDetection>),
    /// `calib.solution`：标定求解结果。
    Solution(Arc<CalibrationSolution>),
    /// 通用 JSON 负载：控制结果、指标、命令等未强类型化的数据。
    Json(Arc<serde_json::Value>),
}

impl DataPacket {
    /// 负载的端口类型标识，用于引擎在接线时做一致性校验。
    #[must_use]
    pub fn port_kind(&self) -> &'static str {
        match self {
            Self::VideoFrame(_) => "stream.video-frame",
            Self::ImageFrame(_) => "image.frame",
            Self::Detection(_) => "calib.detection",
            Self::Solution(_) => "calib.solution",
            Self::Json(_) => "status.metrics",
        }
    }
}

impl std::fmt::Debug for DataPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VideoFrame(frame) => f
                .debug_struct("VideoFrame")
                .field("width", &frame.width)
                .field("height", &frame.height)
                .field("sequence", &frame.identity.frame_sequence)
                .finish(),
            Self::ImageFrame(frame) => f
                .debug_struct("ImageFrame")
                .field("width", &frame.width)
                .field("height", &frame.height)
                .finish(),
            Self::Detection(_) => f.write_str("Detection(..)"),
            Self::Solution(solution) => f
                .debug_struct("Solution")
                .field("views", &solution.views.len())
                .field("rms", &solution.rms_error)
                .finish(),
            Self::Json(_) => f.write_str("Json(..)"),
        }
    }
}
