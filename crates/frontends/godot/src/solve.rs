//! 标定求解：dataset TCP YUV/luma 帧 → Gray PNG → 亚像素棋盘检测 → OpenCV 内参标定。
//!
//! 复用 adapters 的 `OpenCvCalibrationBackend`（feature calibration-opencv）；
//! 后台线程执行，结果回主线程展示；完整 `CalibrationSolution` 供 EEPROM 写入。

use crate::preview::CapturedDatasetFrame;
use camera_toolbox_adapters::calibration::OpenCvCalibrationBackend;
use camera_toolbox_app::ports::calibration::{
    CalibrationBackend, CalibrationCancellation, SubpixelRefinementOptions,
};
use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationRequest, CalibrationSolution,
    ChessboardDetectionOutcome, InitialIntrinsics,
};
use godot::prelude::*;
use std::sync::Arc;

/// 单路求解结果（主线程展示 + EEPROM 写入用）。
pub struct SolveResult {
    pub channel: u16,
    /// 完整标定解（EEPROM 写入使用）。
    pub solution: CalibrationSolution,
}

impl SolveResult {
    /// 结果摘要文本（中文，就地展示）。
    pub fn summary(&self) -> String {
        solution_detail_summary(&format!("CH{}", self.channel), &self.solution)
    }
}

/// 标定结果几何摘要：内参、FOV、光心偏移角、畸变与单图误差统计。
pub fn solution_detail_summary(label: &str, solution: &CalibrationSolution) -> String {
    let k = solution.camera_matrix;
    let distortion = distortion_summary(&solution.distortion_coefficients);
    let rmse_values = view_rmse_values(solution);
    let max_view_rmse = rmse_values.iter().copied().fold(0.0_f64, f64::max);
    format!(
        "{label} 求解完成：{} 帧有效 · RMS {:.3} px · 单图最大 {:.3} px\n{}\n畸变：{}",
        solution.views.len(),
        solution.rms_error,
        max_view_rmse,
        format_intrinsics_geometry(
            solution.image_size.width,
            solution.image_size.height,
            k[0],
            k[4],
            k[2],
            k[5]
        ),
        distortion
    )
}

/// EEPROM/求解共用的内参几何展示。
pub fn format_intrinsics_geometry(
    width: u32,
    height: u32,
    fx: f64,
    fy: f64,
    cx: f64,
    cy: f64,
) -> String {
    let half_w = f64::from(width) * 0.5;
    let half_h = f64::from(height) * 0.5;
    let hfov = 2.0 * (half_w / fx).atan().to_degrees();
    let vfov = 2.0 * (half_h / fy).atan().to_degrees();
    let optical_x = ((cx - half_w) / fx).atan().to_degrees();
    let optical_y = ((cy - half_h) / fy).atan().to_degrees();
    format!(
        "K: fx={fx:.2} fy={fy:.2} cx={cx:.2} cy={cy:.2} · FOV H/V={hfov:.2}°/{vfov:.2}° · 光心偏移 x/y={optical_x:+.3}°/{optical_y:+.3}°"
    )
}

/// 单张图片的重投影 RMSE，用于柱状图。
pub fn view_rmse_values(solution: &CalibrationSolution) -> Vec<f64> {
    solution
        .views
        .iter()
        .map(|view| view.reprojection_rmse)
        .collect()
}

/// EEPROM 状态中也需要展示畸变参数；过长时保留 D12 全量但压缩精度。
pub fn distortion_summary(values: &[f64]) -> String {
    const NAMES: [&str; 12] = [
        "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
    ];
    NAMES
        .iter()
        .zip(values.iter().copied())
        .map(|(name, value)| format!("{name}={value:.4}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 对单路 dataset 求解（后台线程调用）。
///
/// dataset 已在采集时转换成 luma：真实设备来自 TCP NV12 的 Y plane；检测显式启用
/// `cornerSubPix`，避免落回 findChessboardCorners 初始角点。
pub fn solve_channel(
    channel: u16,
    frames: &[Arc<CapturedDatasetFrame>],
    board: BoardSpec,
) -> Result<SolveResult, String> {
    if frames.is_empty() {
        return Err(format!("CH{channel} dataset 为空"));
    }
    let backend = OpenCvCalibrationBackend;
    let cancellation = CalibrationCancellation::default();
    let subpixel_options = default_subpixel_options();
    let mut image_points: Vec<Vec<CalibrationPoint>> = Vec::new();
    let mut image_size = None;
    for frame in frames {
        let png = encode_luma_png(&frame.luma, frame.width, frame.height)?;
        let expected = CalibrationImageSize {
            width: frame.width,
            height: frame.height,
        };
        match backend.detect_png_with_options(
            &png,
            expected,
            256 * 1024 * 1024,
            board,
            subpixel_options,
            &cancellation,
        ) {
            Ok(ChessboardDetectionOutcome::Found(detection)) => {
                image_size = Some(detection.image_size);
                image_points.push(detection.corners);
            }
            Ok(ChessboardDetectionOutcome::NotFound { .. }) => {}
            Err(error) => {
                // 单帧检测失败不中断整体求解（如运动模糊帧）。
                godot_print!("CH{channel} 检测失败（跳过）：{error}");
            }
        }
    }
    if image_points.len() < 5 {
        return Err(format!(
            "有效检测帧不足（{}/5）：请重采或调整棋盘参数",
            image_points.len()
        ));
    }
    let image_size = image_size.expect("有检测必有尺寸");
    let initial = InitialIntrinsics {
        camera_matrix: [
            900.0,
            0.0,
            f64::from(image_size.width) / 2.0,
            0.0,
            900.0,
            f64::from(image_size.height) / 2.0,
            0.0,
            0.0,
            1.0,
        ],
        distortion_coefficients: vec![0.0, 0.0, 0.0, 0.0, 0.0],
    };
    let request = CalibrationRequest {
        image_size,
        board,
        image_points,
        initial_intrinsics: initial,
    };
    let solution: CalibrationSolution = backend
        .calibrate(&request, &cancellation)
        .map_err(|error| format!("标定失败：{error}"))?;
    Ok(SolveResult { channel, solution })
}

/// Gray8 luma 帧编码为 PNG 字节；真实设备输入来自 TCP NV12 的 Y plane。
fn encode_luma_png(luma: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let img = image::GrayImage::from_raw(width, height, luma.to_vec()).ok_or("luma 帧尺寸非法")?;
    let mut buf = Vec::new();
    image::DynamicImage::ImageLuma8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|error| format!("PNG 编码失败：{error}"))?;
    Ok(buf)
}

fn default_subpixel_options() -> SubpixelRefinementOptions {
    SubpixelRefinementOptions {
        enabled: true,
        window_radius: 5,
        max_iterations: 30,
        epsilon: 0.01,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_core::{ViewCalibrationResult, PANGBOT_CALIBRATION_FLAGS};

    fn solution() -> CalibrationSolution {
        CalibrationSolution {
            image_size: CalibrationImageSize {
                width: 1920,
                height: 1080,
            },
            camera_matrix: [1200.0, 0.0, 970.0, 0.0, 1180.0, 535.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![
                0.1, -0.02, 0.001, -0.002, 0.003, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
            ],
            rms_error: 0.42,
            calibration_flags: PANGBOT_CALIBRATION_FLAGS,
            views: vec![
                ViewCalibrationResult {
                    rotation_vector: [0.0; 3],
                    translation_vector: [0.0; 3],
                    projected_points: Vec::new(),
                    reprojection_rmse: 0.30,
                    max_reprojection_error: 0.8,
                },
                ViewCalibrationResult {
                    rotation_vector: [0.0; 3],
                    translation_vector: [0.0; 3],
                    projected_points: Vec::new(),
                    reprojection_rmse: 0.70,
                    max_reprojection_error: 1.2,
                },
            ],
        }
    }

    #[test]
    fn summary_includes_intrinsics_geometry_and_view_errors() {
        let text = solution_detail_summary("CH0", &solution());
        assert!(text.contains("CH0 求解完成：2 帧有效 · RMS 0.420 px · 单图最大 0.700 px"));
        assert!(text.contains("K: fx=1200.00 fy=1180.00 cx=970.00 cy=535.00"));
        assert!(text.contains("FOV H/V="));
        assert!(text.contains("光心偏移 x/y="));
        assert!(text.contains("k1=0.1000"));
    }

    #[test]
    fn view_rmse_values_keep_dataset_order() {
        assert_eq!(view_rmse_values(&solution()), vec![0.30, 0.70]);
    }
}
