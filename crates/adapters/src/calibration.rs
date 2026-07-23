//! `OpenCV` 标定 adapter；所有 `OpenCV` 类型限定在本模块内。

use camera_toolbox_app::{CalibrationBackend, CalibrationBackendError, CalibrationCancellation};
use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationRequest, CalibrationSolution,
    ChessboardDetection, ChessboardDetectionOutcome, PANGBOT_CALIBRATION_FLAGS,
    ViewCalibrationResult,
};
use opencv::calib;
use opencv::core::{self, Mat, Point2f, Point3f, Size, TermCriteria, Vector};
use opencv::geometry;
use opencv::imgcodecs;
use opencv::imgproc;
use opencv::objdetect;
use opencv::prelude::*;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const DETECTOR_FLAGS: i32 =
    objdetect::CALIB_CB_ADAPTIVE_THRESH | objdetect::CALIB_CB_NORMALIZE_IMAGE;
const SUBPIX_MIN_HALF_WINDOW: i32 = 3;
const SUBPIX_MAX_WINDOW: Size = Size::new(11, 11);
const SUBPIX_MIN_GRID_SPACING: f32 = 12.0;
const SUBPIX_WINDOW_TO_SPACING_RATIO: f32 = 0.25;
const SUBPIX_NEAR_LIMIT_RATIO: f32 = 0.8;
const SUBPIX_STABILITY_PERTURBATION: f32 = 0.25;
const SUBPIX_STABILITY_TOLERANCE: f32 = 0.05;
const SUBPIX_ZERO_ZONE: Size = Size::new(-1, -1);
const SUBPIX_MAX_ITERATIONS: i32 = 100;
const SUBPIX_EPSILON: f64 = 1e-4;
const CALIBRATION_FLAGS: i32 =
    calib::CALIB_USE_INTRINSIC_GUESS | calib::CALIB_RATIONAL_MODEL | calib::CALIB_THIN_PRISM_MODEL;
const CALIBRATION_MAX_ITERATIONS: i32 = 30;
const CALIBRATION_EPSILON: f64 = f64::EPSILON;

const _: () = assert!(CALIBRATION_FLAGS == PANGBOT_CALIBRATION_FLAGS);

#[derive(Debug, Default)]
pub struct OpenCvCalibrationBackend;

impl CalibrationBackend for OpenCvCalibrationBackend {
    fn build_information(&self) -> Result<String, CalibrationBackendError> {
        core::get_build_information().map_err(|error| cv_error("getBuildInformation", &error))
    }

    fn detect_png(
        &self,
        encoded_png: &[u8],
        expected_size: CalibrationImageSize,
        decoded_byte_limit: usize,
        board: BoardSpec,
        cancellation: &CalibrationCancellation,
    ) -> Result<ChessboardDetectionOutcome, CalibrationBackendError> {
        checkpoint(cancellation)?;
        board.validate()?;
        CalibrationImageSize::new(expected_size.width, expected_size.height)?;
        if !encoded_png.starts_with(PNG_SIGNATURE) {
            return Err(CalibrationBackendError::InputNotPng);
        }
        ensure_decoded_budget(expected_size, decoded_byte_limit)?;

        let encoded = Vector::<u8>::from_slice(encoded_png);
        let bgr = imgcodecs::imdecode(&encoded, imgcodecs::IMREAD_COLOR)
            .map_err(|error| cv_error("imdecode", &error))?;
        if bgr.empty() {
            return Err(CalibrationBackendError::OpenCv {
                operation: "imdecode",
                message: "decoder returned an empty image".to_owned(),
            });
        }
        let actual_size = calibration_image_size(&bgr)?;
        if actual_size != expected_size {
            return Err(CalibrationBackendError::ImageSizeMismatch {
                expected: expected_size,
                actual: actual_size,
            });
        }
        checkpoint(cancellation)?;

        let mut gray = Mat::default();
        imgproc::cvt_color_def(&bgr, &mut gray, imgproc::COLOR_BGR2GRAY)
            .map_err(|error| cv_error("cvtColor(BGR2GRAY)", &error))?;
        let mut corners = Vector::<Point2f>::new();
        let board_size = Size::new(i32::from(board.inner_cols), i32::from(board.inner_rows));
        let found =
            objdetect::find_chessboard_corners(&gray, board_size, &mut corners, DETECTOR_FLAGS)
                .map_err(|error| cv_error("findChessboardCorners", &error))?;
        checkpoint(cancellation)?;

        if !found {
            return Ok(ChessboardDetectionOutcome::NotFound {
                image_size: actual_size,
            });
        }
        let refinement = refine_detected_corners(&gray, &mut corners, board, cancellation)?;
        debug_assert!(
            refinement.final_window.is_none() || refinement.preferred_window.is_some(),
            "a final subpixel window requires a preferred window"
        );
        if !refinement.accepted {
            return Ok(ChessboardDetectionOutcome::NotFound {
                image_size: actual_size,
            });
        }
        checkpoint(cancellation)?;

        let detection = ChessboardDetection {
            image_size: actual_size,
            corners: corners
                .to_vec()
                .into_iter()
                .map(|point| CalibrationPoint::new(point.x, point.y))
                .collect(),
        };
        detection.validate(board)?;
        Ok(ChessboardDetectionOutcome::Found(detection))
    }

    #[allow(clippy::too_many_lines)] // 保持一次标定事务的输入转换、求解和结果校验连续可审计。
    fn calibrate(
        &self,
        request: &CalibrationRequest,
        cancellation: &CalibrationCancellation,
    ) -> Result<CalibrationSolution, CalibrationBackendError> {
        checkpoint(cancellation)?;
        request.validate()?;

        let object_points_one_view = object_points(request.board)?;
        let mut object_points = Vector::<Vector<Point3f>>::new();
        let mut image_points = Vector::<Vector<Point2f>>::new();
        for view in &request.image_points {
            object_points.push(object_points_one_view.clone());
            image_points.push(Vector::from_iter(
                view.iter().map(|point| Point2f::new(point.x, point.y)),
            ));
        }

        let matrix = &request.initial_intrinsics.camera_matrix;
        let mut camera_matrix = Mat::from_slice_2d(&[&matrix[0..3], &matrix[3..6], &matrix[6..9]])
            .map_err(|error| cv_error("camera matrix conversion", &error))?;
        let mut distortion_coefficients = Mat::from_slice_2d(&[request
            .initial_intrinsics
            .distortion_coefficients
            .as_slice()])
        .map_err(|error| cv_error("distortion conversion", &error))?;
        let mut rotation_vectors = Vector::<Mat>::new();
        let mut translation_vectors = Vector::<Mat>::new();
        let criteria = TermCriteria::new(
            core::TermCriteria_COUNT | core::TermCriteria_EPS,
            CALIBRATION_MAX_ITERATIONS,
            CALIBRATION_EPSILON,
        )
        .map_err(|error| cv_error("TermCriteria(calibration)", &error))?;
        let image_size = Size::new(
            i32::try_from(request.image_size.width).map_err(|_| size_conversion_error())?,
            i32::try_from(request.image_size.height).map_err(|_| size_conversion_error())?,
        );
        let rms_error = calib::calibrate_camera(
            &object_points,
            &image_points,
            image_size,
            &mut camera_matrix,
            &mut distortion_coefficients,
            &mut rotation_vectors,
            &mut translation_vectors,
            CALIBRATION_FLAGS,
            criteria,
        )
        .map_err(|error| cv_error("calibrateCamera", &error))?;
        checkpoint(cancellation)?;

        if rotation_vectors.len() != request.image_points.len()
            || translation_vectors.len() != request.image_points.len()
        {
            return Err(CalibrationBackendError::OpenCv {
                operation: "calibrateCamera",
                message: format!(
                    "returned {} rotation and {} translation vectors for {} views",
                    rotation_vectors.len(),
                    translation_vectors.len(),
                    request.image_points.len()
                ),
            });
        }
        let calibrated_matrix = matrix_3x3(&camera_matrix)?;
        let calibrated_distortion = distortion_coefficients
            .data_typed::<f64>()
            .map_err(|error| cv_error("read distortion coefficients", &error))?
            .to_vec();

        let mut views = Vec::with_capacity(request.image_points.len());
        for (index, observed) in request.image_points.iter().enumerate() {
            let rotation = rotation_vectors
                .get(index)
                .map_err(|error| cv_error("read rotation vector", &error))?;
            let translation = translation_vectors
                .get(index)
                .map_err(|error| cv_error("read translation vector", &error))?;
            let mut projected = Vector::<Point2f>::new();
            geometry::project_points_def(
                &object_points_one_view,
                &rotation,
                &translation,
                &camera_matrix,
                &distortion_coefficients,
                &mut projected,
            )
            .map_err(|error| cv_error("projectPoints", &error))?;
            let projected_points: Vec<_> = projected
                .to_vec()
                .into_iter()
                .map(|point| CalibrationPoint::new(point.x, point.y))
                .collect();
            let (reprojection_rmse, max_reprojection_error) =
                reprojection_metrics(observed, &projected_points)?;
            views.push(ViewCalibrationResult {
                rotation_vector: vector3(&rotation, "rotation vector")?,
                translation_vector: vector3(&translation, "translation vector")?,
                projected_points,
                reprojection_rmse,
                max_reprojection_error,
            });
        }
        checkpoint(cancellation)?;

        let solution = CalibrationSolution {
            image_size: request.image_size,
            camera_matrix: calibrated_matrix,
            distortion_coefficients: calibrated_distortion,
            rms_error,
            calibration_flags: CALIBRATION_FLAGS,
            views,
        };
        solution.validate_against(request)?;
        Ok(solution)
    }
}

#[derive(Debug, Clone, Copy)]
struct SubpixelRefinementReport {
    accepted: bool,
    preferred_window: Option<Size>,
    final_window: Option<Size>,
}

#[derive(Debug, PartialEq, Eq)]
struct SubpixelRefinementStats {
    unchanged_indices: Vec<usize>,
    near_limit_count: usize,
}

/// 由棋盘拓扑相邻点的低分位间距选择逐帧半窗口，避免远侧压缩区域混入相邻角点。
#[allow(clippy::cast_possible_truncation)] // 缩放值已在 3..=11 内取整并限幅。
fn adaptive_subpixel_window(initial: &[Point2f], board: BoardSpec) -> Option<Size> {
    let rows = usize::from(board.inner_rows);
    let columns = usize::from(board.inner_cols);
    if initial.len() != rows.checked_mul(columns)? {
        return None;
    }

    let horizontal_count = rows.checked_mul(columns.checked_sub(1)?)?;
    let vertical_count = rows.checked_sub(1)?.checked_mul(columns)?;
    let mut squared_spacings = Vec::with_capacity(horizontal_count.checked_add(vertical_count)?);
    let mut collect_spacing = |first: Point2f, second: Point2f| {
        let dx = second.x - first.x;
        let dy = second.y - first.y;
        let squared = dx.mul_add(dx, dy * dy);
        if squared.is_finite() && squared > 0.0 {
            squared_spacings.push(squared);
        }
    };
    for row in 0..rows {
        let row_start = row * columns;
        for column in 0..columns - 1 {
            collect_spacing(initial[row_start + column], initial[row_start + column + 1]);
        }
    }
    for row in 0..rows - 1 {
        let row_start = row * columns;
        let next_row_start = row_start + columns;
        for column in 0..columns {
            collect_spacing(
                initial[row_start + column],
                initial[next_row_start + column],
            );
        }
    }
    if squared_spacings.is_empty() {
        return None;
    }

    squared_spacings.sort_unstable_by(f32::total_cmp);
    let lower_decile_index = (squared_spacings.len() - 1) / 10;
    let lower_decile_spacing = squared_spacings[lower_decile_index].sqrt();
    if lower_decile_spacing < SUBPIX_MIN_GRID_SPACING {
        return None;
    }
    let half_window = (lower_decile_spacing * SUBPIX_WINDOW_TO_SPACING_RATIO)
        .round()
        .clamp(
            SUBPIX_MIN_HALF_WINDOW as f32,
            SUBPIX_MAX_WINDOW.width as f32,
        ) as i32;
    Some(Size::new(half_window, half_window))
}

#[allow(clippy::cast_precision_loss)] // 半窗口固定在 3..=11，转换不会损失有效精度。
fn subpixel_refinement_stats(
    initial: &[Point2f],
    refined: &[Point2f],
    window: Size,
) -> Option<SubpixelRefinementStats> {
    if initial.len() != refined.len() {
        return None;
    }
    let near_limit_x = SUBPIX_NEAR_LIMIT_RATIO * window.width as f32;
    let near_limit_y = SUBPIX_NEAR_LIMIT_RATIO * window.height as f32;
    let mut unchanged_indices = Vec::new();
    let mut near_limit_count = 0;
    for (index, (initial, refined)) in initial.iter().zip(refined).enumerate() {
        if initial.x.to_bits() == refined.x.to_bits() && initial.y.to_bits() == refined.y.to_bits()
        {
            unchanged_indices.push(index);
        }
        let dx = (refined.x - initial.x).abs();
        let dy = (refined.y - initial.y).abs();
        near_limit_count += usize::from(dx >= near_limit_x || dy >= near_limit_y);
    }
    Some(SubpixelRefinementStats {
        unchanged_indices,
        near_limit_count,
    })
}

fn corner_sub_pix_with_window(
    gray: &Mat,
    corners: &mut Vector<Point2f>,
    window: Size,
) -> Result<(), CalibrationBackendError> {
    let criteria = TermCriteria::new(
        core::TermCriteria_EPS | core::TermCriteria_COUNT,
        SUBPIX_MAX_ITERATIONS,
        SUBPIX_EPSILON,
    )
    .map_err(|error| cv_error("TermCriteria(subpixel)", &error))?;
    imgproc::corner_sub_pix(gray, corners, window, SUBPIX_ZERO_ZONE, criteria)
        .map_err(|error| cv_error("cornerSubPix", &error))
}

/// 从小扰动重新收敛到同一点，才能把“未移动”视为初值已处于稳定最优点。
#[allow(clippy::cast_precision_loss)] // OpenCV 图像尺寸转换仅用于 0.25 px 的内向扰动。
fn unchanged_points_are_stable(
    gray: &Mat,
    refined: &[Point2f],
    unchanged_indices: &[usize],
    window: Size,
    cancellation: &CalibrationCancellation,
) -> Result<bool, CalibrationBackendError> {
    if unchanged_indices.is_empty() {
        return Ok(true);
    }

    let center_x = gray.cols() as f32 * 0.5;
    let center_y = gray.rows() as f32 * 0.5;
    let mut stability_probe = Vector::from_iter(unchanged_indices.iter().map(|&index| {
        let point = refined[index];
        let dx = if point.x < center_x {
            SUBPIX_STABILITY_PERTURBATION
        } else {
            -SUBPIX_STABILITY_PERTURBATION
        };
        let dy = if point.y < center_y {
            SUBPIX_STABILITY_PERTURBATION
        } else {
            -SUBPIX_STABILITY_PERTURBATION
        };
        Point2f::new(point.x + dx, point.y + dy)
    }));
    corner_sub_pix_with_window(gray, &mut stability_probe, window)?;
    checkpoint(cancellation)?;

    Ok(stability_probe
        .to_vec()
        .iter()
        .zip(unchanged_indices)
        .all(|(probe, &index)| {
            let reference = refined[index];
            (probe.x - reference.x).abs() <= SUBPIX_STABILITY_TOLERANCE
                && (probe.y - reference.y).abs() <= SUBPIX_STABILITY_TOLERANCE
        }))
}

fn subpixel_pass_is_stable(
    gray: &Mat,
    refined: &[Point2f],
    stats: &SubpixelRefinementStats,
    window: Size,
    cancellation: &CalibrationCancellation,
) -> Result<bool, CalibrationBackendError> {
    if stats.near_limit_count > 0 {
        return Ok(false);
    }
    unchanged_points_are_stable(
        gray,
        refined,
        &stats.unchanged_indices,
        window,
        cancellation,
    )
}

/// 防止动态小窗口触发 OpenCV 静默回退，同时保留可稳定复现的首选窗口。
fn refine_detected_corners(
    gray: &Mat,
    corners: &mut Vector<Point2f>,
    board: BoardSpec,
    cancellation: &CalibrationCancellation,
) -> Result<SubpixelRefinementReport, CalibrationBackendError> {
    let initial = corners.to_vec();
    let Some(preferred_window) = adaptive_subpixel_window(&initial, board) else {
        return Ok(SubpixelRefinementReport {
            accepted: false,
            preferred_window: None,
            final_window: None,
        });
    };

    let mut final_window = preferred_window;
    corner_sub_pix_with_window(gray, corners, final_window)?;
    checkpoint(cancellation)?;
    let mut refined = corners.to_vec();
    let Some(mut stats) = subpixel_refinement_stats(&initial, &refined, final_window) else {
        return Ok(SubpixelRefinementReport {
            accepted: false,
            preferred_window: Some(preferred_window),
            final_window: Some(final_window),
        });
    };
    let mut accepted = subpixel_pass_is_stable(gray, &refined, &stats, final_window, cancellation)?;

    if !accepted && final_window != SUBPIX_MAX_WINDOW {
        *corners = Vector::from_iter(initial.iter().copied());
        final_window = SUBPIX_MAX_WINDOW;
        corner_sub_pix_with_window(gray, corners, final_window)?;
        checkpoint(cancellation)?;
        refined = corners.to_vec();
        let Some(retry_stats) = subpixel_refinement_stats(&initial, &refined, final_window) else {
            return Ok(SubpixelRefinementReport {
                accepted: false,
                preferred_window: Some(preferred_window),
                final_window: Some(final_window),
            });
        };
        stats = retry_stats;
        accepted = subpixel_pass_is_stable(gray, &refined, &stats, final_window, cancellation)?;
    }

    Ok(SubpixelRefinementReport {
        accepted,
        preferred_window: Some(preferred_window),
        final_window: Some(final_window),
    })
}

fn checkpoint(cancellation: &CalibrationCancellation) -> Result<(), CalibrationBackendError> {
    if cancellation.is_cancelled() {
        Err(CalibrationBackendError::Cancelled)
    } else {
        Ok(())
    }
}

fn ensure_decoded_budget(
    size: CalibrationImageSize,
    limit: usize,
) -> Result<(), CalibrationBackendError> {
    // IMREAD_COLOR 的 BGR8 与 BGR2GRAY 的 Gray8 同时存活，峰值按每像素 4 bytes 计。
    let required_u64 = u64::from(size.width)
        .checked_mul(u64::from(size.height))
        .and_then(|pixels| pixels.checked_mul(4))
        .unwrap_or(u64::MAX);
    let required = usize::try_from(required_u64).unwrap_or(usize::MAX);
    if required_u64 > limit as u64 || required_u64 > usize::MAX as u64 {
        return Err(CalibrationBackendError::DecodedImageTooLarge { required, limit });
    }
    Ok(())
}

fn calibration_image_size(mat: &Mat) -> Result<CalibrationImageSize, CalibrationBackendError> {
    let width = u32::try_from(mat.cols()).map_err(|_| size_conversion_error())?;
    let height = u32::try_from(mat.rows()).map_err(|_| size_conversion_error())?;
    CalibrationImageSize::new(width, height).map_err(Into::into)
}

#[allow(clippy::cast_possible_truncation)] // BoardSpec 已保证全部棋盘坐标可表示为有限 f32。
fn object_points(board: BoardSpec) -> Result<Vector<Point3f>, CalibrationBackendError> {
    let square_size = board.square_size as f32;
    if !square_size.is_finite() || square_size <= 0.0 {
        return Err(CalibrationBackendError::OpenCv {
            operation: "object point conversion",
            message: "square_size cannot be represented as finite positive f32".to_owned(),
        });
    }
    Ok(Vector::from_iter((0..board.inner_rows).flat_map(|row| {
        (0..board.inner_cols).map(move |column| {
            Point3f::new(
                f32::from(column) * square_size,
                f32::from(row) * square_size,
                0.0,
            )
        })
    })))
}

fn matrix_3x3(mat: &Mat) -> Result<[f64; 9], CalibrationBackendError> {
    let values = mat
        .data_typed::<f64>()
        .map_err(|error| cv_error("read camera matrix", &error))?;
    values
        .try_into()
        .map_err(|_| CalibrationBackendError::OpenCv {
            operation: "read camera matrix",
            message: format!("expected 9 values, got {}", values.len()),
        })
}

fn vector3(mat: &Mat, operation: &'static str) -> Result<[f64; 3], CalibrationBackendError> {
    let values = mat
        .data_typed::<f64>()
        .map_err(|error| cv_error(operation, &error))?;
    values
        .try_into()
        .map_err(|_| CalibrationBackendError::OpenCv {
            operation,
            message: format!("expected 3 values, got {}", values.len()),
        })
}

fn reprojection_metrics(
    observed: &[CalibrationPoint],
    projected: &[CalibrationPoint],
) -> Result<(f64, f64), CalibrationBackendError> {
    if observed.is_empty() || observed.len() != projected.len() {
        return Err(CalibrationBackendError::OpenCv {
            operation: "reprojection metrics",
            message: format!(
                "observed/projected point counts differ: {}/{}",
                observed.len(),
                projected.len()
            ),
        });
    }
    let mut squared_error_sum = 0.0;
    let mut max_error = 0.0_f64;
    for (observed, projected) in observed.iter().zip(projected) {
        let dx = f64::from(observed.x - projected.x);
        let dy = f64::from(observed.y - projected.y);
        let error = dx.hypot(dy);
        squared_error_sum += error * error;
        max_error = max_error.max(error);
    }
    let point_count =
        u32::try_from(observed.len()).map_err(|_| CalibrationBackendError::OpenCv {
            operation: "reprojection metrics",
            message: "point count exceeds u32 range".to_owned(),
        })?;
    Ok((
        (squared_error_sum / f64::from(point_count)).sqrt(),
        max_error,
    ))
}

fn size_conversion_error() -> CalibrationBackendError {
    CalibrationBackendError::OpenCv {
        operation: "image size conversion",
        message: "dimensions exceed OpenCV i32 range".to_owned(),
    }
}

fn cv_error(operation: &'static str, error: &opencv::Error) -> CalibrationBackendError {
    CalibrationBackendError::OpenCv {
        operation,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linked_opencv_runtime_is_available() {
        let version = core::get_version_string().unwrap();
        assert_eq!(version, "5.0.0");
        let build_information = OpenCvCalibrationBackend.build_information().unwrap();
        println!("OpenCV runtime: {version}");
        assert!(build_information.contains(&version));
    }

    #[test]
    fn subpixel_refinement_parameters_are_locked() {
        assert_eq!(DETECTOR_FLAGS, 3);
        assert_eq!(SUBPIX_MIN_HALF_WINDOW, 3);
        assert_eq!(SUBPIX_MAX_WINDOW, Size::new(11, 11));
        assert_eq!(SUBPIX_MIN_GRID_SPACING, 12.0);
        assert_eq!(SUBPIX_WINDOW_TO_SPACING_RATIO, 0.25);
        assert_eq!(SUBPIX_NEAR_LIMIT_RATIO, 0.8);
        assert_eq!(SUBPIX_STABILITY_PERTURBATION, 0.25);
        assert_eq!(SUBPIX_STABILITY_TOLERANCE, 0.05);
        assert_eq!(SUBPIX_ZERO_ZONE, Size::new(-1, -1));
        assert_eq!(SUBPIX_MAX_ITERATIONS, 100);
        assert_eq!(SUBPIX_EPSILON, 1e-4);
    }

    #[test]
    fn adaptive_subpixel_window_uses_lower_decile_and_bounds() {
        let board = BoardSpec::new(4, 3, 20.0).unwrap();
        let compressed = grid_points(&[0.0, 20.0, 60.0, 100.0], &[0.0, 40.0, 80.0]);
        assert_eq!(
            adaptive_subpixel_window(&compressed, board),
            Some(Size::new(5, 5))
        );

        let nominal = grid_points(&[0.0, 40.0, 80.0, 120.0], &[0.0, 40.0, 80.0]);
        assert_eq!(
            adaptive_subpixel_window(&nominal, board),
            Some(Size::new(10, 10))
        );

        let large = grid_points(&[0.0, 80.0, 160.0, 240.0], &[0.0, 80.0, 160.0]);
        assert_eq!(
            adaptive_subpixel_window(&large, board),
            Some(Size::new(11, 11))
        );

        let undersampled = grid_points(&[0.0, 11.0, 22.0, 33.0], &[0.0, 11.0, 22.0]);
        assert_eq!(adaptive_subpixel_window(&undersampled, board), None);
    }

    #[test]
    fn subpixel_stats_detect_unchanged_and_near_limit_points() {
        let initial = [
            Point2f::new(1.0, 1.0),
            Point2f::new(10.0, 10.0),
            Point2f::new(20.0, 20.0),
        ];
        let refined = [
            initial[0],
            Point2f::new(17.9, 10.0),
            Point2f::new(28.0, 20.0),
        ];
        let stats = subpixel_refinement_stats(&initial, &refined, Size::new(10, 10)).unwrap();
        assert_eq!(stats.unchanged_indices, vec![0]);
        assert_eq!(stats.near_limit_count, 1);
        assert!(stats.near_limit_count > 0);
    }

    #[test]
    fn blank_image_rejects_unstable_unchanged_refinement() {
        let gray = Mat::zeros(240, 320, core::CV_8UC1)
            .unwrap()
            .to_mat()
            .unwrap();
        let board = BoardSpec::new(4, 3, 20.0).unwrap();
        let mut corners = Vector::from_iter(grid_points(
            &[40.0, 80.0, 120.0, 160.0],
            &[40.0, 80.0, 120.0],
        ));
        let report = refine_detected_corners(
            &gray,
            &mut corners,
            board,
            &CalibrationCancellation::default(),
        )
        .unwrap();
        assert!(!report.accepted);
        assert_eq!(report.preferred_window, Some(Size::new(10, 10)));
        assert_eq!(report.final_window, Some(Size::new(11, 11)));
    }

    #[test]
    fn synthetic_checkerboard_retains_preferred_window_across_scales() {
        let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/chessboard_11x8_clean.png");
        let source =
            imgcodecs::imread(fixture_path.to_str().unwrap(), imgcodecs::IMREAD_GRAYSCALE).unwrap();
        let board = BoardSpec::new(11, 8, 20.0).unwrap();
        let cases = [
            (Size::new(320, 240), Size::new(5, 5)),
            (Size::new(640, 480), Size::new(10, 10)),
            (Size::new(1280, 960), Size::new(11, 11)),
        ];

        for (image_size, expected_window) in cases {
            let mut gray = Mat::default();
            imgproc::resize(
                &source,
                &mut gray,
                image_size,
                0.0,
                0.0,
                imgproc::INTER_LINEAR,
            )
            .unwrap();
            let mut corners = Vector::<Point2f>::new();
            let found = objdetect::find_chessboard_corners(
                &gray,
                Size::new(11, 8),
                &mut corners,
                DETECTOR_FLAGS,
            )
            .unwrap();
            assert!(found, "checkerboard not found at {image_size:?}");

            let report = refine_detected_corners(
                &gray,
                &mut corners,
                board,
                &CalibrationCancellation::default(),
            )
            .unwrap();
            assert!(report.accepted, "unstable refinement at {image_size:?}");
            assert_eq!(report.preferred_window, Some(expected_window));
            assert_eq!(report.final_window, Some(expected_window));
        }
    }

    fn grid_points(xs: &[f32], ys: &[f32]) -> Vec<Point2f> {
        ys.iter()
            .flat_map(|&y| xs.iter().map(move |&x| Point2f::new(x, y)))
            .collect()
    }

    #[test]
    fn pangbot_implicit_calibration_criteria_are_locked() {
        assert_eq!(CALIBRATION_FLAGS, 49_153);
        assert_eq!(CALIBRATION_MAX_ITERATIONS, 30);
        assert_eq!(CALIBRATION_EPSILON, f64::EPSILON);
    }

    #[test]
    fn detection_budget_accounts_for_bgr_and_gray() {
        let size = CalibrationImageSize::new(100, 50).unwrap();
        assert!(ensure_decoded_budget(size, 20_000).is_ok());
        assert!(matches!(
            ensure_decoded_budget(size, 19_999),
            Err(CalibrationBackendError::DecodedImageTooLarge {
                required: 20_000,
                limit: 19_999
            })
        ));
    }
    #[test]
    fn clean_png_preserves_reference_pixel_and_corner_output() {
        let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/chessboard_11x8_clean.png");
        let encoded = include_bytes!("../tests/data/chessboard_11x8_clean.png");

        let baseline_bgr =
            imgcodecs::imread(fixture_path.to_str().unwrap(), imgcodecs::IMREAD_COLOR).unwrap();
        let decoded_bgr =
            imgcodecs::imdecode(&Vector::<u8>::from_slice(encoded), imgcodecs::IMREAD_COLOR)
                .unwrap();
        assert_eq!(baseline_bgr.size().unwrap(), decoded_bgr.size().unwrap());
        assert_eq!(baseline_bgr.typ(), decoded_bgr.typ());
        assert_eq!(
            baseline_bgr.data_bytes().unwrap(),
            decoded_bgr.data_bytes().unwrap()
        );

        let mut baseline_gray = Mat::default();
        let mut decoded_gray = Mat::default();
        imgproc::cvt_color_def(&baseline_bgr, &mut baseline_gray, imgproc::COLOR_BGR2GRAY).unwrap();
        imgproc::cvt_color_def(&decoded_bgr, &mut decoded_gray, imgproc::COLOR_BGR2GRAY).unwrap();
        assert_eq!(
            baseline_gray.data_bytes().unwrap(),
            decoded_gray.data_bytes().unwrap()
        );

        let mut baseline_corners = Vector::<Point2f>::new();
        let found = objdetect::find_chessboard_corners_def(
            &baseline_gray,
            Size::new(11, 8),
            &mut baseline_corners,
        )
        .unwrap();
        assert!(found);
        imgproc::corner_sub_pix(
            &baseline_gray,
            &mut baseline_corners,
            Size::new(11, 11),
            Size::new(-1, -1),
            TermCriteria::new(core::TermCriteria_EPS | core::TermCriteria_COUNT, 100, 1e-4)
                .unwrap(),
        )
        .unwrap();

        let outcome = OpenCvCalibrationBackend
            .detect_png(
                encoded,
                CalibrationImageSize::new(640, 480).unwrap(),
                640 * 480 * 4,
                BoardSpec::new(11, 8, 20.0).unwrap(),
                &CalibrationCancellation::default(),
            )
            .unwrap();
        let ChessboardDetectionOutcome::Found(detection) = outcome else {
            panic!("synthetic chessboard was not detected");
        };
        assert_eq!(detection.corners.len(), baseline_corners.len());
        let max_corner_difference = detection
            .corners
            .iter()
            .zip(baseline_corners.to_vec())
            .map(|(actual, expected)| {
                f64::from(actual.x - expected.x).hypot(f64::from(actual.y - expected.y))
            })
            .fold(0.0_f64, f64::max);
        assert!(max_corner_difference <= 1e-4, "{max_corner_difference}");

        use sha2::{Digest, Sha256};
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("../tests/data/chessboard_11x8_clean.json")).unwrap();
        assert_eq!(
            golden["detector_flags"].as_i64(),
            Some(i64::from(DETECTOR_FLAGS))
        );
        assert_eq!(
            golden["sha256"].as_str(),
            Some(format!("{:x}", Sha256::digest(encoded)).as_str())
        );
        assert_eq!(
            golden["bgr_sha256"].as_str(),
            Some(format!("{:x}", Sha256::digest(baseline_bgr.data_bytes().unwrap())).as_str())
        );
        assert_eq!(
            golden["gray_sha256"].as_str(),
            Some(format!("{:x}", Sha256::digest(baseline_gray.data_bytes().unwrap())).as_str())
        );
        let golden_corners = golden["corners"].as_array().unwrap();
        assert_eq!(golden_corners.len(), detection.corners.len());
        let max_golden_difference = detection
            .corners
            .iter()
            .zip(golden_corners)
            .map(|(actual, expected)| {
                let expected_x = expected[0].as_f64().unwrap();
                let expected_y = expected[1].as_f64().unwrap();
                (f64::from(actual.x) - expected_x).hypot(f64::from(actual.y) - expected_y)
            })
            .fold(0.0_f64, f64::max);
        assert!(max_golden_difference <= 0.05, "{max_golden_difference}");
    }

    #[test]
    fn detector_rejects_non_png_before_opencv_decode() {
        let error = OpenCvCalibrationBackend
            .detect_png(
                &[0xff, 0xd8, 0xff],
                CalibrationImageSize::new(640, 480).unwrap(),
                640 * 480 * 4,
                BoardSpec::new(11, 8, 20.0).unwrap(),
                &CalibrationCancellation::default(),
            )
            .unwrap_err();
        assert!(matches!(error, CalibrationBackendError::InputNotPng));
    }

    #[test]
    fn object_points_use_inner_corner_layout_and_square_size() {
        let points = object_points(BoardSpec::new(3, 2, 40.0).unwrap()).unwrap();
        assert_eq!(points.len(), 6);
        let points = points.to_vec();
        assert_eq!(points[0], Point3f::new(0.0, 0.0, 0.0));
        assert_eq!(points[2], Point3f::new(80.0, 0.0, 0.0));
        assert_eq!(points[5], Point3f::new(80.0, 40.0, 0.0));
    }

    #[test]
    fn calibration_matches_pangbot_fixed_observation_contract() {
        let request = pangbot_synthetic_request();
        let solution = OpenCvCalibrationBackend
            .calibrate(&request, &CalibrationCancellation::default())
            .unwrap();
        solution.validate_against(&request).unwrap();
        assert_eq!(solution.calibration_flags, 49_153);
        assert_eq!(solution.views.len(), 3);
        assert_eq!(solution.distortion_coefficients.len(), 12);
        assert!(solution.rms_error < 1e-4, "{}", solution.rms_error);
        assert!(
            solution
                .views
                .iter()
                .all(|view| view.reprojection_rmse < 1e-4)
        );
    }

    /// 逐项复现 Pangbot `testCalibrationRunnerComputesPerImageReprojectionErrors`。
    fn pangbot_synthetic_request() -> CalibrationRequest {
        let board = BoardSpec::new(9, 6, 0.04).unwrap();
        let objects = object_points(board).unwrap();
        let camera_matrix = Mat::from_slice_2d(&[
            &[700.0_f64, 0.0, 320.0],
            &[0.0, 710.0, 240.0],
            &[0.0, 0.0, 1.0],
        ])
        .unwrap();
        let distortion = Mat::from_slice_2d(&[&[0.0_f64; 12]]).unwrap();
        let poses = [
            ([0.02, -0.01, 0.03], [0.0, 0.0, 1.2]),
            ([-0.03, 0.04, 0.01], [0.05, -0.02, 1.3]),
            ([0.01, 0.02, -0.02], [-0.04, 0.03, 1.1]),
        ];
        let image_points = poses
            .into_iter()
            .map(|(rotation, translation)| {
                let rotation = Mat::from_slice_2d(&[&rotation]).unwrap();
                let translation = Mat::from_slice_2d(&[&translation]).unwrap();
                let mut projected = Vector::<Point2f>::new();
                geometry::project_points_def(
                    &objects,
                    &rotation,
                    &translation,
                    &camera_matrix,
                    &distortion,
                    &mut projected,
                )
                .unwrap();
                projected
                    .to_vec()
                    .into_iter()
                    .map(|point| CalibrationPoint::new(point.x, point.y))
                    .collect()
            })
            .collect();
        CalibrationRequest {
            image_size: CalibrationImageSize::new(640, 480).unwrap(),
            board,
            image_points,
            initial_intrinsics: camera_toolbox_core::InitialIntrinsics {
                camera_matrix: [700.0, 0.0, 320.0, 0.0, 710.0, 240.0, 0.0, 0.0, 1.0],
                distortion_coefficients: vec![0.0; 12],
            },
        }
    }
}
