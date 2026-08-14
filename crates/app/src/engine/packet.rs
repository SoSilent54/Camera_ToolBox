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
    /// `calib.coverage`：标定数据覆盖度（弱类型，`Arc<Value>` 承载）。
    Coverage(Arc<serde_json::Value>),
    /// `calib.dataset`：标定数据集（弱类型）。
    Dataset(Arc<serde_json::Value>),
    /// `calib.report`：标定报告（弱类型）。
    Report(Arc<serde_json::Value>),
    /// `capture.score`：采集评分（弱类型）。
    Score(Arc<serde_json::Value>),
    /// `capture.target`：采集目标位姿（弱类型）。
    Target(Arc<serde_json::Value>),
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
            Self::Coverage(_) => "calib.coverage",
            Self::Dataset(_) => "calib.dataset",
            Self::Report(_) => "calib.report",
            Self::Score(_) => "capture.score",
            Self::Target(_) => "capture.target",
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
            Self::Coverage(_) => f.write_str("Coverage(..)"),
            Self::Dataset(_) => f.write_str("Dataset(..)"),
            Self::Report(_) => f.write_str("Report(..)"),
            Self::Score(_) => f.write_str("Score(..)"),
            Self::Target(_) => f.write_str("Target(..)"),
            Self::Json(_) => f.write_str("Json(..)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_variants_map_to_declared_port_kinds() {
        let coverage = DataPacket::Coverage(Arc::new(serde_json::json!({})));
        let dataset = DataPacket::Dataset(Arc::new(serde_json::json!({})));
        let report = DataPacket::Report(Arc::new(serde_json::json!({})));
        let score = DataPacket::Score(Arc::new(serde_json::json!({})));
        let target = DataPacket::Target(Arc::new(serde_json::json!({})));
        assert_eq!(coverage.port_kind(), "calib.coverage");
        assert_eq!(dataset.port_kind(), "calib.dataset");
        assert_eq!(report.port_kind(), "calib.report");
        assert_eq!(score.port_kind(), "capture.score");
        assert_eq!(target.port_kind(), "capture.target");
    }

    #[test]
    fn existing_variants_keep_their_port_kinds() {
        let json = DataPacket::Json(Arc::new(serde_json::json!({})));
        assert_eq!(json.port_kind(), "status.metrics");
    }
}
