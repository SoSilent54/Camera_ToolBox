//! 标定求解：dataset 帧 → PNG → 棋盘检测 → OpenCV 内参标定。
//!
//! 复用 adapters 的 `OpenCvCalibrationBackend`（feature calibration-opencv）；
//! 后台线程执行，结果回主线程展示。

use camera_toolbox_adapters::calibration::OpenCvCalibrationBackend;
use camera_toolbox_app::platform::DecodedVideoFrame;
use camera_toolbox_app::ports::calibration::{
    CalibrationBackend, CalibrationCancellation,
};
use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationRequest,
    CalibrationSolution, ChessboardDetectionOutcome, InitialIntrinsics,
};
use std::sync::Arc;
use godot::prelude::*;

/// 单路求解结果（主线程展示用）。
pub struct SolveResult {
    pub channel: u16,
    pub views_used: usize,
    pub rms_error: f64,
    pub camera_matrix: [f64; 9],
    pub distortion_coefficients: Vec<f64>,
}

impl SolveResult {
    /// 结果摘要文本（中文，就地展示）。
    pub fn summary(&self) -> String {
        let [_, _, _, _, fy, _, _, _, _] = self.camera_matrix;
        let fx = self.camera_matrix[0];
        let cx = self.camera_matrix[2];
        let cy = self.camera_matrix[5];
        format!(
            "CH{} 求解完成：{} 帧有效 · RMS {:.3} px · fx {fx:.1} fy {fy:.1} cx {cx:.1} cy {cy:.1}",
            self.channel, self.views_used, self.rms_error
        )
    }
}

/// 对单路 dataset 求解（后台线程调用）。
///
/// 帧先编码为 PNG（backend 仅接受 PNG 输入），逐帧检测棋盘；
/// 有效检测不足 5 帧返回错误。
pub fn solve_channel(
    channel: u16,
    frames: &[Arc<DecodedVideoFrame>],
    board: BoardSpec,
) -> Result<SolveResult, String> {
    if frames.is_empty() {
        return Err(format!("CH{channel} dataset 为空"));
    }
    let backend = OpenCvCalibrationBackend;
    let cancellation = CalibrationCancellation::default();
    let mut image_points: Vec<Vec<CalibrationPoint>> = Vec::new();
    let mut image_size = None;
    for frame in frames {
        let png = encode_png(&frame.rgba, frame.width, frame.height)?;
        let expected = CalibrationImageSize {
            width: frame.width,
            height: frame.height,
        };
        match backend.detect_png(
            &png,
            expected,
            256 * 1024 * 1024,
            board,
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
        .map_err(|error| format!("CH{channel} 标定失败：{error}"))?;
    Ok(SolveResult {
        channel,
        views_used: solution.views.len(),
        rms_error: solution.rms_error,
        camera_matrix: solution.camera_matrix,
        distortion_coefficients: solution.distortion_coefficients,
    })
}

/// RGBA 帧编码为 PNG 字节。
fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let img =
        image::RgbaImage::from_raw(width, height, rgba.to_vec()).ok_or("帧尺寸非法")?;
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|error| format!("PNG 编码失败：{error}"))?;
    Ok(buf)
}
