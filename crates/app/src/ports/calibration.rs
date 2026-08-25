//! 相机标定算法端口。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use camera_toolbox_core::{
    BoardSpec, CalibrationDataError, CalibrationImageSize, CalibrationRequest, CalibrationSolution,
    ChessboardDetection, ChessboardDetectionOutcome, InitialIntrinsics, ViewCalibrationResult,
};
use thiserror::Error;

/// `OpenCV` 长调用只能在调用边界协作取消；此标记不承诺硬中断。
#[derive(Clone, Debug, Default)]
pub struct CalibrationCancellation(Arc<AtomicBool>);

impl CalibrationCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// 用户显式配置的角点亚像素细化；关闭时保留 `findChessboardCorners` 的原始角点。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubpixelRefinementOptions {
    pub enabled: bool,
    pub window_radius: i32,
    pub max_iterations: i32,
    pub epsilon: f64,
}

impl SubpixelRefinementOptions {
    /// 校验 OpenCV `cornerSubPix` 的窗口和终止条件。
    pub fn validate(self) -> Result<(), CalibrationBackendError> {
        if !(1..=64).contains(&self.window_radius)
            || !(1..=10_000).contains(&self.max_iterations)
            || !self.epsilon.is_finite()
            || self.epsilon <= 0.0
        {
            return Err(CalibrationBackendError::InvalidSubpixelOptions);
        }
        Ok(())
    }
}

/// 由 adapter 实现的棋盘检测与内参标定能力。
pub trait CalibrationBackend: Send + Sync {
    /// 返回实际链接 `OpenCV` 的版本和构建信息，用于 parity 报告与启动诊断。
    ///
    /// # Errors
    ///
    /// `OpenCV runtime` 无法查询时返回错误。
    fn build_information(&self) -> Result<String, CalibrationBackendError>;

    /// 从原始 `PNG encoded bytes` 检测 `Pangbot` 兼容棋盘角点。
    ///
    /// `expected_size` 来自独立 `PNG metadata preflight`；实现必须在解码后复核。
    /// `decoded_byte_limit` 同时约束 `OpenCV BGR` 与 `Gray` 的峰值像素预算。
    ///
    /// # Errors
    ///
    /// 输入不是 `PNG`、预算不足、尺寸变化、取消或 `OpenCV` 调用失败时返回错误。
    fn detect_png(
        &self,
        encoded_png: &[u8],
        expected_size: CalibrationImageSize,
        decoded_byte_limit: usize,
        board: BoardSpec,
        cancellation: &CalibrationCancellation,
    ) -> Result<ChessboardDetectionOutcome, CalibrationBackendError>;

    /// 使用显式亚像素配置检测棋盘。旧 backend 可忽略配置并复用 `detect_png`，避免破坏独立测试实现。
    fn detect_png_with_options(
        &self,
        encoded_png: &[u8],
        expected_size: CalibrationImageSize,
        decoded_byte_limit: usize,
        board: BoardSpec,
        options: SubpixelRefinementOptions,
        cancellation: &CalibrationCancellation,
    ) -> Result<ChessboardDetectionOutcome, CalibrationBackendError> {
        let _ = options;
        self.detect_png(
            encoded_png,
            expected_size,
            decoded_byte_limit,
            board,
            cancellation,
        )
    }

    /// 用当前初始内参对单张 authoritative 棋盘检测求解外参，用于自动采集约束收益评估。
    ///
    /// # Errors
    ///
    /// 输入不变量无效、PnP 无解、输出非有限或取消时返回错误。
    fn estimate_pose(
        &self,
        detection: &ChessboardDetection,
        initial_intrinsics: &InitialIntrinsics,
        board: BoardSpec,
        cancellation: &CalibrationCancellation,
    ) -> Result<ViewCalibrationResult, CalibrationBackendError>;

    /// 使用固定 `Pangbot flags` 标定，并计算逐图投影点与误差。
    ///
    /// # Errors
    ///
    /// 输入/输出不变量无效、取消或 `OpenCV` 调用失败时返回错误。
    fn calibrate(
        &self,
        request: &CalibrationRequest,
        cancellation: &CalibrationCancellation,
    ) -> Result<CalibrationSolution, CalibrationBackendError>;
}

#[derive(Debug, Error)]
pub enum CalibrationBackendError {
    #[error("calibration operation was cancelled")]
    Cancelled,
    #[error(transparent)]
    InvalidData(#[from] CalibrationDataError),
    #[error("calibration detector accepts PNG encoded bytes only")]
    InputNotPng,
    #[error("invalid subpixel refinement options")]
    InvalidSubpixelOptions,
    #[error("decoded BGR+Gray requires {required} bytes, limit is {limit} bytes")]
    DecodedImageTooLarge { required: usize, limit: usize },
    #[error("decoded image size differs: expected {expected:?}, got {actual:?}")]
    ImageSizeMismatch {
        expected: CalibrationImageSize,
        actual: CalibrationImageSize,
    },
    #[error("OpenCV {operation} failed: {message}")]
    OpenCv {
        operation: &'static str,
        message: String,
    },
}
