use opencv::calib;
use opencv::core::{self, Mat, Point2f, Point3f, Scalar, Size, TermCriteria, Vector};
use opencv::geometry;
use opencv::imgcodecs;
use opencv::objdetect;

fn main() -> opencv::Result<()> {
    let version = core::get_version_string()?;
    assert_eq!(version, "5.0.0", "unexpected OpenCV runtime version");

    let gray = Mat::new_rows_cols_with_default(64, 64, core::CV_8UC1, Scalar::all(0.0))?;
    let mut png = Vector::<u8>::new();
    assert!(
        imgcodecs::imencode_def(".png", &gray, &mut png)?,
        "PNG codec smoke failed"
    );
    assert!(!png.is_empty(), "PNG encoder returned an empty payload");

    let mut corners = Vector::<Point2f>::new();
    let found = objdetect::find_chessboard_corners(
        &gray,
        Size::new(3, 3),
        &mut corners,
        objdetect::CALIB_CB_ADAPTIVE_THRESH | objdetect::CALIB_CB_NORMALIZE_IMAGE,
    )?;
    assert!(!found, "blank image unexpectedly contains a chessboard");

    let object_points = Vector::<Point3f>::from_iter([Point3f::new(0.0, 0.0, 1.0)]);
    let rotation = Mat::from_slice_2d(&[&[0.0_f64], &[0.0], &[0.0]])?;
    let translation = Mat::from_slice_2d(&[&[0.0_f64], &[0.0], &[0.0]])?;
    let camera_matrix =
        Mat::from_slice_2d(&[&[1.0_f64, 0.0, 0.0], &[0.0, 1.0, 0.0], &[0.0, 0.0, 1.0]])?;
    let distortion = Mat::from_slice_2d(&[&[0.0_f64; 5]])?;
    let mut projected = Vector::<Point2f>::new();
    geometry::project_points_def(
        &object_points,
        &rotation,
        &translation,
        &camera_matrix,
        &distortion,
        &mut projected,
    )?;
    assert_eq!(projected.len(), 1, "point projection returned wrong size");
    let point = projected.get(0)?;
    assert!(
        point.x.is_finite() && point.y.is_finite(),
        "point projection returned non-finite coordinates"
    );

    let empty_object_points = Vector::<Vector<Point3f>>::new();
    let empty_image_points = Vector::<Vector<Point2f>>::new();
    let mut mutable_camera_matrix = camera_matrix.clone();
    let mut mutable_distortion = distortion.clone();
    let mut rotations = Vector::<Mat>::new();
    let mut translations = Vector::<Mat>::new();
    let criteria = TermCriteria::new(core::TermCriteria_COUNT | core::TermCriteria_EPS, 30, 1e-6)?;
    let calibration_result = calib::calibrate_camera(
        &empty_object_points,
        &empty_image_points,
        Size::new(64, 64),
        &mut mutable_camera_matrix,
        &mut mutable_distortion,
        &mut rotations,
        &mut translations,
        0,
        criteria,
    );
    assert!(
        calibration_result.is_err(),
        "empty calibration input was not rejected"
    );

    println!("OpenCV {version} relocated opencv-rust smoke passed");
    Ok(())
}
