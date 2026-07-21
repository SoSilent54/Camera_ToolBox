//! `OpenCV` 标定 adapter；所有 `OpenCV` 类型限定在本模块内。

use camera_toolbox_app::{CalibrationBackend, CalibrationBackendError, CalibrationCancellation};
use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationRequest, CalibrationSolution,
    ChessboardDetection, ChessboardDetectionOutcome, PANGBOT_CALIBRATION_FLAGS,
    ViewCalibrationResult,
};
use opencv::calib3d;
use opencv::core::{self, Mat, Point2f, Point3f, Size, TermCriteria, Vector};
use opencv::imgcodecs;
use opencv::imgproc;
use opencv::prelude::*;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const DETECTOR_FLAGS: i32 = calib3d::CALIB_CB_ADAPTIVE_THRESH | calib3d::CALIB_CB_NORMALIZE_IMAGE;
const SUBPIX_WINDOW: Size = Size::new(11, 11);
const SUBPIX_ZERO_ZONE: Size = Size::new(-1, -1);
const SUBPIX_MAX_ITERATIONS: i32 = 100;
const SUBPIX_EPSILON: f64 = 1e-4;
const CALIBRATION_FLAGS: i32 = calib3d::CALIB_USE_INTRINSIC_GUESS
    | calib3d::CALIB_RATIONAL_MODEL
    | calib3d::CALIB_THIN_PRISM_MODEL;
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
            calib3d::find_chessboard_corners(&gray, board_size, &mut corners, DETECTOR_FLAGS)
                .map_err(|error| cv_error("findChessboardCorners", &error))?;
        checkpoint(cancellation)?;

        if !found {
            return Ok(ChessboardDetectionOutcome::NotFound {
                image_size: actual_size,
            });
        }
        let criteria = TermCriteria::new(
            core::TermCriteria_EPS | core::TermCriteria_COUNT,
            SUBPIX_MAX_ITERATIONS,
            SUBPIX_EPSILON,
        )
        .map_err(|error| cv_error("TermCriteria(subpixel)", &error))?;
        imgproc::corner_sub_pix(
            &gray,
            &mut corners,
            SUBPIX_WINDOW,
            SUBPIX_ZERO_ZONE,
            criteria,
        )
        .map_err(|error| cv_error("cornerSubPix", &error))?;
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
        let rms_error = calib3d::calibrate_camera(
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
            calib3d::project_points_def(
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
        let build_information = OpenCvCalibrationBackend.build_information().unwrap();
        println!("OpenCV runtime: {version}");
        assert!(build_information.contains(&version));
    }

    #[test]
    fn pangbot_implicit_detector_parameters_are_locked() {
        assert_eq!(DETECTOR_FLAGS, 3);
        assert_eq!(SUBPIX_WINDOW, Size::new(11, 11));
        assert_eq!(SUBPIX_ZERO_ZONE, Size::new(-1, -1));
        assert_eq!(SUBPIX_MAX_ITERATIONS, 100);
        assert_eq!(SUBPIX_EPSILON, 1e-4);
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
    fn encoded_png_matches_pangbot_imread_pixel_and_corner_chain() {
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
        let found = calib3d::find_chessboard_corners_def(
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
                calib3d::project_points_def(
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
