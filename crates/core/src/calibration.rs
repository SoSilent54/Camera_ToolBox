//! 相机内参标定的纯领域类型与不变量。
//!
//! 本模块不依赖 `OpenCV`；外部算法实现必须先校验输入，并在安装结果前再次校验输出。

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// `Pangbot` 兼容标定使用的固定 `OpenCV flags`。
///
/// 数值来自 `OpenCV` 的 `CALIB_USE_INTRINSIC_GUESS | CALIB_RATIONAL_MODEL |
/// CALIB_THIN_PRISM_MODEL`，作为跨 adapter 的可审计契约。
pub const PANGBOT_CALIBRATION_FLAGS: i32 = 1 | 16_384 | 32_768;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationImageSize {
    pub width: u32,
    pub height: u32,
}

impl CalibrationImageSize {
    /// # Errors
    ///
    /// 任一维度为零时返回错误。
    pub const fn new(width: u32, height: u32) -> Result<Self, CalibrationDataError> {
        if width == 0 || height == 0 {
            return Err(CalibrationDataError::ZeroImageDimension { width, height });
        }
        Ok(Self { width, height })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoardSpec {
    pub inner_cols: u16,
    pub inner_rows: u16,
    /// 相邻角点的物理距离；单位由调用方显式管理。
    pub square_size: f64,
}

impl BoardSpec {
    /// # Errors
    ///
    /// 棋盘尺寸过小、角点总数溢出或方格尺寸非有限正数时返回错误。
    pub fn new(
        inner_cols: u16,
        inner_rows: u16,
        square_size: f64,
    ) -> Result<Self, CalibrationDataError> {
        let board = Self {
            inner_cols,
            inner_rows,
            square_size,
        };
        board.validate()?;
        Ok(board)
    }

    /// # Errors
    ///
    /// 棋盘参数不满足标定约束时返回错误。
    pub fn validate(self) -> Result<(), CalibrationDataError> {
        if self.inner_cols < 2 || self.inner_rows < 2 {
            return Err(CalibrationDataError::BoardTooSmall {
                inner_cols: self.inner_cols,
                inner_rows: self.inner_rows,
            });
        }
        if self.inner_cols > 256 || self.inner_rows > 256 {
            return Err(CalibrationDataError::BoardTooLarge {
                inner_cols: self.inner_cols,
                inner_rows: self.inner_rows,
            });
        }
        let max_index = f64::from(self.inner_cols.max(self.inner_rows) - 1);
        if !self.square_size.is_finite()
            || self.square_size <= 0.0
            || self.square_size * max_index > f64::from(f32::MAX)
        {
            return Err(CalibrationDataError::InvalidSquareSize(self.square_size));
        }
        self.corner_count()?;
        Ok(())
    }

    /// # Errors
    ///
    /// 角点总数不能表示为 `usize` 时返回错误。
    pub fn corner_count(self) -> Result<usize, CalibrationDataError> {
        usize::from(self.inner_cols)
            .checked_mul(usize::from(self.inner_rows))
            .ok_or(CalibrationDataError::CornerCountOverflow)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationPoint {
    pub x: f32,
    pub y: f32,
}

impl CalibrationPoint {
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    #[must_use]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChessboardDetection {
    pub image_size: CalibrationImageSize,
    /// 与 OpenCV/Pangbot 一致的行优先角点顺序。
    pub corners: Vec<CalibrationPoint>,
}

impl ChessboardDetection {
    /// # Errors
    ///
    /// 角点数量与棋盘不符或存在 NaN/Inf 时返回错误。
    pub fn validate(&self, board: BoardSpec) -> Result<(), CalibrationDataError> {
        board.validate()?;
        let expected = board.corner_count()?;
        if self.corners.len() != expected {
            return Err(CalibrationDataError::PointCountMismatch {
                view: 0,
                expected,
                actual: self.corners.len(),
            });
        }
        validate_points(0, &self.corners)
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ChessboardDetectionOutcome {
    Found(ChessboardDetection),
    NotFound { image_size: CalibrationImageSize },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InitialIntrinsics {
    /// 3x3 row-major camera matrix。
    pub camera_matrix: [f64; 9],
    /// `OpenCV` distortion coefficients；`Pangbot` 输入至少需要一个系数。
    pub distortion_coefficients: Vec<f64>,
}

impl InitialIntrinsics {
    /// # Errors
    ///
    /// 矩阵/畸变系数为空或含非有限值，或焦距不为正数时返回错误。
    pub fn validate(&self) -> Result<(), CalibrationDataError> {
        if self.camera_matrix.iter().any(|value| !value.is_finite()) {
            return Err(CalibrationDataError::NonFiniteCameraMatrix);
        }
        if self.camera_matrix[0] <= 0.0 || self.camera_matrix[4] <= 0.0 {
            return Err(CalibrationDataError::NonPositiveFocalLength {
                fx: self.camera_matrix[0],
                fy: self.camera_matrix[4],
            });
        }
        if self.distortion_coefficients.is_empty() {
            return Err(CalibrationDataError::EmptyDistortionCoefficients);
        }
        if self
            .distortion_coefficients
            .iter()
            .any(|value| !value.is_finite())
        {
            return Err(CalibrationDataError::NonFiniteDistortionCoefficients);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationRequest {
    pub image_size: CalibrationImageSize,
    pub board: BoardSpec,
    /// 每个 view 的角点，顺序必须与 `ChessboardDetection` 一致。
    pub image_points: Vec<Vec<CalibrationPoint>>,
    pub initial_intrinsics: InitialIntrinsics,
}

impl CalibrationRequest {
    /// # Errors
    ///
    /// 输入为空、每图角点数量错误或任一数值无效时返回错误。
    pub fn validate(&self) -> Result<(), CalibrationDataError> {
        self.board.validate()?;
        self.initial_intrinsics.validate()?;
        if self.image_points.is_empty() {
            return Err(CalibrationDataError::NoCalibrationViews);
        }
        let expected = self.board.corner_count()?;
        for (view, points) in self.image_points.iter().enumerate() {
            if points.len() != expected {
                return Err(CalibrationDataError::PointCountMismatch {
                    view,
                    expected,
                    actual: points.len(),
                });
            }
            validate_points(view, points)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewCalibrationResult {
    /// `OpenCV Rodrigues` rotation vector，`board frame -> camera frame`。
    pub rotation_vector: [f64; 3],
    /// board frame -> camera frame；单位与 `BoardSpec::square_size` 一致。
    pub translation_vector: [f64; 3],
    pub projected_points: Vec<CalibrationPoint>,
    pub reprojection_rmse: f64,
    pub max_reprojection_error: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationSolution {
    pub image_size: CalibrationImageSize,
    pub camera_matrix: [f64; 9],
    pub distortion_coefficients: Vec<f64>,
    pub rms_error: f64,
    pub calibration_flags: i32,
    pub views: Vec<ViewCalibrationResult>,
}

impl CalibrationSolution {
    /// # Errors
    ///
    /// 输出含非有限值、flags 不兼容或 view/point 数量与请求不一致时返回错误。
    pub fn validate_against(
        &self,
        request: &CalibrationRequest,
    ) -> Result<(), CalibrationDataError> {
        request.validate()?;
        if self.image_size != request.image_size {
            return Err(CalibrationDataError::ResultImageSizeMismatch {
                expected: request.image_size,
                actual: self.image_size,
            });
        }
        if self.calibration_flags != PANGBOT_CALIBRATION_FLAGS {
            return Err(CalibrationDataError::CalibrationFlagsMismatch {
                expected: PANGBOT_CALIBRATION_FLAGS,
                actual: self.calibration_flags,
            });
        }
        InitialIntrinsics {
            camera_matrix: self.camera_matrix,
            distortion_coefficients: self.distortion_coefficients.clone(),
        }
        .validate()?;
        if !self.rms_error.is_finite() || self.rms_error < 0.0 {
            return Err(CalibrationDataError::InvalidReprojectionMetric);
        }
        if self.views.len() != request.image_points.len() {
            return Err(CalibrationDataError::ResultViewCountMismatch {
                expected: request.image_points.len(),
                actual: self.views.len(),
            });
        }
        for (view_index, (view, observed)) in
            self.views.iter().zip(&request.image_points).enumerate()
        {
            if view
                .rotation_vector
                .iter()
                .chain(&view.translation_vector)
                .any(|value| !value.is_finite())
                || !view.reprojection_rmse.is_finite()
                || view.reprojection_rmse < 0.0
                || !view.max_reprojection_error.is_finite()
                || view.max_reprojection_error < 0.0
            {
                return Err(CalibrationDataError::InvalidViewResult { view: view_index });
            }
            if view.projected_points.len() != observed.len() {
                return Err(CalibrationDataError::PointCountMismatch {
                    view: view_index,
                    expected: observed.len(),
                    actual: view.projected_points.len(),
                });
            }
            validate_points(view_index, &view.projected_points)?;
        }
        Ok(())
    }
}

fn validate_points(view: usize, points: &[CalibrationPoint]) -> Result<(), CalibrationDataError> {
    if let Some(point) = points.iter().position(|point| !point.is_finite()) {
        return Err(CalibrationDataError::NonFinitePoint { view, point });
    }
    Ok(())
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum CalibrationDataError {
    #[error("calibration image dimensions must be positive, got {width}x{height}")]
    ZeroImageDimension { width: u32, height: u32 },
    #[error("chessboard must have at least 2x2 inner corners, got {inner_cols}x{inner_rows}")]
    BoardTooSmall { inner_cols: u16, inner_rows: u16 },
    #[error("chessboard supports at most 256x256 inner corners, got {inner_cols}x{inner_rows}")]
    BoardTooLarge { inner_cols: u16, inner_rows: u16 },
    #[error("square size must be finite and positive, got {0}")]
    InvalidSquareSize(f64),
    #[error("chessboard corner count overflows host address space")]
    CornerCountOverflow,
    #[error("calibration requires at least one view")]
    NoCalibrationViews,
    #[error("view {view} has {actual} points, expected {expected}")]
    PointCountMismatch {
        view: usize,
        expected: usize,
        actual: usize,
    },
    #[error("view {view} point {point} is not finite")]
    NonFinitePoint { view: usize, point: usize },
    #[error("camera matrix contains NaN or infinity")]
    NonFiniteCameraMatrix,
    #[error("camera focal lengths must be positive, got fx={fx}, fy={fy}")]
    NonPositiveFocalLength { fx: f64, fy: f64 },
    #[error("distortion coefficients must not be empty")]
    EmptyDistortionCoefficients,
    #[error("distortion coefficients contain NaN or infinity")]
    NonFiniteDistortionCoefficients,
    #[error("calibration result image size differs: expected {expected:?}, got {actual:?}")]
    ResultImageSizeMismatch {
        expected: CalibrationImageSize,
        actual: CalibrationImageSize,
    },
    #[error("calibration flags differ: expected {expected}, got {actual}")]
    CalibrationFlagsMismatch { expected: i32, actual: i32 },
    #[error("calibration RMS/reprojection metric is invalid")]
    InvalidReprojectionMetric,
    #[error("calibration result has {actual} views, expected {expected}")]
    ResultViewCountMismatch { expected: usize, actual: usize },
    #[error("calibration result view {view} contains invalid pose or error metrics")]
    InvalidViewResult { view: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> CalibrationRequest {
        CalibrationRequest {
            image_size: CalibrationImageSize::new(640, 480).unwrap(),
            board: BoardSpec::new(2, 2, 20.0).unwrap(),
            image_points: vec![vec![
                CalibrationPoint::new(10.0, 10.0),
                CalibrationPoint::new(20.0, 10.0),
                CalibrationPoint::new(10.0, 20.0),
                CalibrationPoint::new(20.0, 20.0),
            ]],
            initial_intrinsics: InitialIntrinsics {
                camera_matrix: [500.0, 0.0, 320.0, 0.0, 500.0, 240.0, 0.0, 0.0, 1.0],
                distortion_coefficients: vec![0.0; 12],
            },
        }
    }

    #[test]
    fn board_rejects_non_finite_square_size() {
        assert!(matches!(
            BoardSpec::new(11, 8, f64::NAN),
            Err(CalibrationDataError::InvalidSquareSize(value)) if value.is_nan()
        ));
    }

    #[test]
    fn board_rejects_dimensions_above_supported_limit() {
        assert!(matches!(
            BoardSpec::new(257, 8, 1.0),
            Err(CalibrationDataError::BoardTooLarge {
                inner_cols: 257,
                inner_rows: 8
            })
        ));
    }
    #[test]
    fn request_rejects_wrong_corner_count() {
        let mut request = request();
        request.image_points[0].pop();
        assert!(matches!(
            request.validate(),
            Err(CalibrationDataError::PointCountMismatch {
                view: 0,
                expected: 4,
                actual: 3
            })
        ));
    }

    #[test]
    fn request_rejects_non_finite_point() {
        let mut request = request();
        request.image_points[0][2].x = f32::INFINITY;
        assert_eq!(
            request.validate(),
            Err(CalibrationDataError::NonFinitePoint { view: 0, point: 2 })
        );
    }

    #[test]
    fn solution_rejects_wrong_flags() {
        let request = request();
        let solution = CalibrationSolution {
            image_size: request.image_size,
            camera_matrix: request.initial_intrinsics.camera_matrix,
            distortion_coefficients: vec![0.0; 12],
            rms_error: 0.1,
            calibration_flags: 0,
            views: Vec::new(),
        };
        assert!(matches!(
            solution.validate_against(&request),
            Err(CalibrationDataError::CalibrationFlagsMismatch { .. })
        ));
    }
}
