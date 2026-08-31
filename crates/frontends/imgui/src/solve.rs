//! 标定求解：dataset TCP YUV/luma 帧 → Gray PNG → 亚像素棋盘检测 → OpenCV 内参标定。
//!
//! 复用 adapters 的 `OpenCvCalibrationBackend`（feature calibration-opencv）；
//! 后台线程执行，结果回主线程展示；完整 `CalibrationSolution` 供 EEPROM 写入。

use crate::observability::{ObservabilityReport, analyze_solution};
use crate::preview::CapturedDatasetFrame;
use camera_toolbox_adapters::calibration::OpenCvCalibrationBackend;
use camera_toolbox_app::ports::calibration::{CalibrationBackend, CalibrationCancellation};
use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationRequest, CalibrationSolution,
    ChessboardDetection, ChessboardDetectionOutcome, InitialIntrinsics,
};
use std::sync::Arc;

/// OpenCV 标定器的技术最小可用视图数；采集端仅作不可见的求解前置条件。
pub const MIN_USABLE_CALIBRATION_VIEWS: usize = 5;
const CALIBRATION_DETECT_MAX_PNG_BYTES: usize = 256 * 1024 * 1024;

/// 单张图像重投影 RMSE 的剔除阈值（像素）；超过该值的视图从标定集剔除后重新求解。
pub(crate) const MAX_VIEW_REPROJECTION_RMSE: f64 = 0.15;

/// 已完成 raw dataset 亚像素棋盘检测的缓存视图。
///
/// 实时可观测性流程复用采集瞬间的 raw 重检角点，避免每采一张后重复检测全部 dataset。
#[derive(Clone, Debug)]
pub struct DetectedDatasetFrame {
    pub frame: Arc<CapturedDatasetFrame>,
    pub detection: ChessboardDetection,
}

/// 单路求解结果（主线程展示 + EEPROM 写入用）。
pub struct SolveResult {
    pub channel: u16,
    /// 完整标定解（EEPROM 写入使用；已剔除超阈值视图后的最终解）。
    pub solution: CalibrationSolution,
    /// 按单图重投影 RMSE 超阈值自动剔除的帧索引（相对入参 frames，升序去重）。
    pub rejected_view_indices: Vec<usize>,
    /// 求解轮数：首轮为 1，每剔除一批超阈值视图后 +1。
    pub iterations: u32,
    /// 最新 dataset 在最终解附近的数值可观测性；视图不足或矩阵退化时为空。
    pub observability: Option<ObservabilityReport>,
}

impl SolveResult {
    /// 结果摘要文本（中文，就地展示）。
    pub fn summary(&self) -> String {
        let mut text = solution_detail_summary(&format!("CH{}", self.channel), &self.solution);
        if !self.rejected_view_indices.is_empty() {
            text.push_str(&format!(
                "\n已自动剔除 {} 张单图重投影误差超阈值（>{MAX_VIEW_REPROJECTION_RMSE}px）的图像并重新标定（共 {} 轮）",
                self.rejected_view_indices.len(),
                self.iterations
            ));
        }
        if let Some(report) = &self.observability {
            text.push_str(&format!(
                "\n可观测性：{} · cond {:.2e} · fx/fy σ {:.3}%/{:.3}% · cx/cy σ {:.2}/{:.2}px",
                if report.goal_met() {
                    "达标"
                } else {
                    report.missing_hint()
                },
                report.condition_number,
                report.focal_relative_stddev[0] * 100.0,
                report.focal_relative_stddev[1] * 100.0,
                report.principal_point_stddev_px[0],
                report.principal_point_stddev_px[1]
            ));
        }
        text
    }
}

/// 单路标定结果的结构化指标（Step 3 列表/图表展示用）。
#[derive(Clone, Debug)]
pub struct SolutionDetail {
    pub label: String,
    pub view_count: usize,
    pub rms: f64,
    pub max_view_rmse: f64,
    pub fx: f64,
    pub fy: f64,
    pub cx: f64,
    pub cy: f64,
    pub hfov_degrees: f64,
    pub vfov_degrees: f64,
    pub optical_x_degrees: f64,
    pub optical_y_degrees: f64,
    pub distortion: Vec<f64>,
}

impl SolutionDetail {
    /// 从完整标定解提取内参几何与单图误差统计。
    #[must_use]
    pub fn from_solution(label: &str, solution: &CalibrationSolution) -> Self {
        let k = solution.camera_matrix;
        let half_w = f64::from(solution.image_size.width) * 0.5;
        let half_h = f64::from(solution.image_size.height) * 0.5;
        let fx = k[0];
        let fy = k[4];
        let cx = k[2];
        let cy = k[5];
        let max_view_rmse = view_rmse_values(solution)
            .iter()
            .copied()
            .fold(0.0_f64, f64::max);
        Self {
            label: label.to_owned(),
            view_count: solution.views.len(),
            rms: solution.rms_error,
            max_view_rmse,
            fx,
            fy,
            cx,
            cy,
            hfov_degrees: 2.0 * (half_w / fx).atan().to_degrees(),
            vfov_degrees: 2.0 * (half_h / fy).atan().to_degrees(),
            optical_x_degrees: ((cx - half_w) / fx).atan().to_degrees(),
            optical_y_degrees: ((cy - half_h) / fy).atan().to_degrees(),
            distortion: solution.distortion_coefficients.clone(),
        }
    }

    /// 与 `solution_detail_summary` 等价的展示文本（日志/兜底用）。
    #[must_use]
    pub fn summary_text(&self) -> String {
        format!(
            "{} 求解完成：{} 帧有效 · RMS {:.3} px · 单图最大 {:.3} px\nK: fx={:.2} fy={:.2} cx={:.2} cy={:.2}\nFOV H/V={:.2}°/{:.2}° · 光心偏移 x/y={:+.3}°/{:+.3}°\n畸变(D12)：{}",
            self.label,
            self.view_count,
            self.rms,
            self.max_view_rmse,
            self.fx,
            self.fy,
            self.cx,
            self.cy,
            self.hfov_degrees,
            self.vfov_degrees,
            self.optical_x_degrees,
            self.optical_y_degrees,
            distortion_summary(&self.distortion)
        )
    }
}

/// 标定结果几何摘要文本（兼容旧调用；内容与 `SolutionDetail::summary_text` 一致）。
#[must_use]
pub fn solution_detail_summary(label: &str, solution: &CalibrationSolution) -> String {
    SolutionDetail::from_solution(label, solution).summary_text()
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

/// 用与最终求解完全相同的 PNG/luma 与棋盘检测路径验证一张 dataset 帧。
///
/// 采集端必须在入库前调用本函数；这样质量证据与 `solve_channel` 的实际输入没有分叉。
pub(crate) fn detect_dataset_frame(
    backend: &OpenCvCalibrationBackend,
    frame: &CapturedDatasetFrame,
    board: BoardSpec,
    cancellation: &CalibrationCancellation,
) -> Result<ChessboardDetectionOutcome, String> {
    let png = encode_luma_png(&frame.luma, frame.width, frame.height)?;
    let expected = CalibrationImageSize {
        width: frame.width,
        height: frame.height,
    };
    backend
        .detect_png(
            &png,
            expected,
            CALIBRATION_DETECT_MAX_PNG_BYTES,
            board,
            cancellation,
        )
        .map_err(|error| format!("dataset 棋盘检测失败：{error}"))
}

/// 对已缓存的 raw/subpixel 检测结果求解，避免实时质量评估重复检测历史帧。
pub(crate) fn solve_channel_from_detections(
    channel: u16,
    detections: &[DetectedDatasetFrame],
    board: BoardSpec,
    previous_observability: Option<&ObservabilityReport>,
) -> Result<SolveResult, String> {
    if detections.is_empty() {
        return Err(format!("CH{channel} dataset 为空"));
    }
    if detections.len() < MIN_USABLE_CALIBRATION_VIEWS {
        return Err(format!(
            "有效检测帧不足（{}/{MIN_USABLE_CALIBRATION_VIEWS}）：请继续采集",
            detections.len()
        ));
    }
    let backend = OpenCvCalibrationBackend;
    let cancellation = CalibrationCancellation::default();
    let image_size = detections_image_size(detections)?;
    let initial = initial_intrinsics_for_size(image_size);
    let mut detected: Vec<(usize, DetectedDatasetFrame)> =
        detections.iter().cloned().enumerate().collect();
    let mut rejected: Vec<usize> = Vec::new();
    let mut iterations = 1_u32;
    let solution = loop {
        let image_points: Vec<Vec<CalibrationPoint>> = detected
            .iter()
            .map(|(_, frame)| frame.detection.corners.clone())
            .collect();
        let request = CalibrationRequest {
            image_size,
            board,
            image_points,
            initial_intrinsics: initial.clone(),
        };
        let solution: CalibrationSolution = backend
            .calibrate(&request, &cancellation)
            .map_err(|error| format!("标定失败：{error}"))?;

        let over: Vec<usize> = views_above_threshold(
            &solution
                .views
                .iter()
                .map(|view| view.reprojection_rmse)
                .collect::<Vec<_>>(),
            MAX_VIEW_REPROJECTION_RMSE,
        );
        if over.is_empty() {
            break solution;
        }
        let remaining = detected.len().saturating_sub(over.len());
        if remaining < MIN_USABLE_CALIBRATION_VIEWS {
            tracing::warn!(
                "CH{channel} 有 {} 张视图重投影误差超阈值（>{MAX_VIEW_REPROJECTION_RMSE}px），但剔除后不足 {MIN_USABLE_CALIBRATION_VIEWS} 张，保留全部",
                over.len()
            );
            break solution;
        }
        iterations += 1;
        let mut kept = Vec::with_capacity(remaining);
        for (index, (frame_index, frame)) in detected.into_iter().enumerate() {
            if over.contains(&index) {
                rejected.push(frame_index);
            } else {
                kept.push((frame_index, frame));
            }
        }
        detected = kept;
    };

    rejected.sort_unstable();
    rejected.dedup();
    let kept_detections = detected
        .iter()
        .map(|(_, detection)| detection.clone())
        .collect::<Vec<_>>();
    let observability =
        analyze_solution(&solution, board, &kept_detections, previous_observability)
            .map_err(|error| {
                tracing::warn!("CH{channel} 可观测性分析失败：{error}");
                error
            })
            .ok();
    Ok(SolveResult {
        channel,
        solution,
        rejected_view_indices: rejected,
        iterations,
        observability,
    })
}

/// 求解后自动剔除单图重投影 RMSE 超过 [`MAX_VIEW_REPROJECTION_RMSE`] 的视图并
/// 重新标定，直到没有超阈值视图或剩余视图跌破 [`MIN_USABLE_CALIBRATION_VIEWS`]。
pub fn solve_channel(
    channel: u16,
    frames: &[Arc<CapturedDatasetFrame>],
    board: BoardSpec,
) -> Result<SolveResult, String> {
    let backend = OpenCvCalibrationBackend;
    let cancellation = CalibrationCancellation::default();
    let mut detected = Vec::new();
    for frame in frames {
        match detect_dataset_frame(&backend, frame, board, &cancellation) {
            Ok(ChessboardDetectionOutcome::Found(detection)) => {
                detected.push(DetectedDatasetFrame {
                    frame: Arc::clone(frame),
                    detection,
                });
            }
            Ok(ChessboardDetectionOutcome::NotFound { .. }) => {}
            Err(error) => {
                // 单帧检测失败不中断整体求解（如运动模糊帧）。
                tracing::warn!("CH{channel} 检测失败（跳过）：{error}");
            }
        }
    }
    solve_channel_from_detections(channel, &detected, board, None)
}

fn initial_intrinsics_for_size(image_size: CalibrationImageSize) -> InitialIntrinsics {
    InitialIntrinsics {
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
    }
}

fn detections_image_size(
    detections: &[DetectedDatasetFrame],
) -> Result<CalibrationImageSize, String> {
    let Some(first) = detections.first() else {
        return Err("dataset 为空".to_owned());
    };
    let image_size = first.detection.image_size;
    for (index, detection) in detections.iter().enumerate() {
        if detection.detection.image_size != image_size {
            return Err(format!(
                "第 {index} 张检测尺寸不一致：expected {}x{}, got {}x{}",
                image_size.width,
                image_size.height,
                detection.detection.image_size.width,
                detection.detection.image_size.height
            ));
        }
    }
    Ok(image_size)
}

/// 找出重投影 RMSE 严格超过阈值的视图索引（升序；阈值本身不剔除）。
fn views_above_threshold(rmse_values: &[f64], threshold: f64) -> Vec<usize> {
    rmse_values
        .iter()
        .enumerate()
        .filter(|(_, rmse)| **rmse > threshold)
        .map(|(index, _)| index)
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_core::{PANGBOT_CALIBRATION_FLAGS, ViewCalibrationResult};

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
    fn views_above_threshold_uses_strict_gt() {
        // 阈值本身（0.15）不剔除；严格大于才剔除，索引保持升序。
        assert_eq!(
            views_above_threshold(&[0.10, 0.15, 0.16, 0.30, 0.05], 0.15),
            vec![2, 3]
        );
        assert!(views_above_threshold(&[0.15, 0.14], 0.15).is_empty());
    }

    #[test]
    fn solve_result_summary_reports_rejected_views() {
        let result = SolveResult {
            channel: 0,
            solution: solution(),
            rejected_view_indices: vec![3, 7, 11],
            iterations: 2,
            observability: None,
        };
        let text = result.summary();
        assert!(
            text.contains(
                "已自动剔除 3 张单图重投影误差超阈值（>0.15px）的图像并重新标定（共 2 轮）"
            )
        );
        assert!(text.contains("CH0 求解完成"));
    }
}
