use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationSolution, ChessboardDetection,
    PANGBOT_CALIBRATION_FLAGS, ViewCalibrationResult,
};
use pongbot_calib_tool::observability::analyze_solution;
use pongbot_calib_tool::preview::{CapturedDatasetFrame, CapturedDatasetSource};
use pongbot_calib_tool::solve::DetectedDatasetFrame;
use std::fmt::Write as _;
use std::sync::Arc;

const IMAGE_SIZE: CalibrationImageSize = CalibrationImageSize {
    width: 1920,
    height: 1080,
};
const RMS_ERROR_PX: f64 = 0.08;

#[derive(Clone, Copy)]
struct SyntheticPose {
    label: &'static str,
    rvec: [f64; 3],
    center_camera: [f64; 3],
}

struct ScenarioResult {
    name: String,
    rows: Vec<ScenarioRow>,
}

struct ScenarioRow {
    views: usize,
    label: &'static str,
    goal: &'static str,
    rms: String,
    cond: String,
    focal: String,
    principal: String,
    distortion: String,
    gain: String,
    hint: String,
}

#[test]
#[ignore = "generates H2 observability simulation tables for documentation"]
fn print_h2_observability_simulation_tables() {
    let board = BoardSpec::new(11, 8, 40.0).expect("valid board");
    let scenarios = [
        run_scenario("fronto_parallel_only", 12, board, &fronto_parallel_only()),
        run_scenario(
            "same_depth_pose_diverse",
            12,
            board,
            &same_depth_pose_diverse(),
        ),
        run_scenario(
            "progressive_full_coverage_D12",
            12,
            board,
            &progressive_full_coverage(),
        ),
        run_scenario(
            "progressive_full_coverage_D5",
            5,
            board,
            &progressive_full_coverage(),
        ),
        run_scenario(
            "aggressive_edge_coverage_D5",
            5,
            board,
            &aggressive_edge_coverage(),
        ),
    ];
    let mut markdown = String::new();
    for scenario in &scenarios {
        print_scenario(scenario);
        append_markdown_table(&mut markdown, scenario);
    }
    if let Ok(path) = std::env::var("PONGBOT_OBSERVABILITY_SIM_MD") {
        std::fs::write(path, markdown).expect("write markdown artifact");
    }
}

fn run_scenario(
    name: &'static str,
    distortion_count: usize,
    board: BoardSpec,
    poses: &[SyntheticPose],
) -> ScenarioResult {
    let mut rows = Vec::new();
    let mut previous = None;
    for views in 1..=poses.len() {
        let (solution, detections) = synthetic_dataset(board, &poses[..views], distortion_count);
        let label = poses[views - 1].label;
        match analyze_solution(&solution, board, &detections, previous.as_ref()) {
            Ok(report) => {
                let goal = if report.goal_met() { "OK" } else { "NO" };
                let hint = report.missing_hint().to_owned();
                let row = ScenarioRow {
                    views,
                    label,
                    goal,
                    rms: format!("{:.3}", report.rms_error),
                    cond: format!("{:.2e}", report.condition_number),
                    focal: format!(
                        "{:.3}/{:.3}%",
                        report.focal_relative_stddev[0] * 100.0,
                        report.focal_relative_stddev[1] * 100.0
                    ),
                    principal: format!(
                        "{:.2}/{:.2}px",
                        report.principal_point_stddev_px[0], report.principal_point_stddev_px[1]
                    ),
                    distortion: format!(
                        "D5 {:.2}px / D12 {:.2}px",
                        max_finite(report.primary_distortion_edge_stddev_px()),
                        max_finite(&report.distortion_edge_stddev_px)
                    ),
                    gain: report
                        .last_info_gain
                        .map_or_else(|| "--".to_owned(), |value| format!("{value:+.2}")),
                    hint,
                };
                previous = Some(report);
                rows.push(row);
            }
            Err(error) => rows.push(ScenarioRow {
                views,
                label,
                goal: "ERR",
                rms: "--".to_owned(),
                cond: "--".to_owned(),
                focal: "--".to_owned(),
                principal: "--".to_owned(),
                distortion: "--".to_owned(),
                gain: "--".to_owned(),
                hint: error,
            }),
        }
    }
    ScenarioResult {
        name: format!("{name} ({distortion_count} active distortion params)"),
        rows,
    }
}

fn synthetic_dataset(
    board: BoardSpec,
    poses: &[SyntheticPose],
    distortion_count: usize,
) -> (CalibrationSolution, Vec<DetectedDatasetFrame>) {
    let intrinsics = camera_matrix();
    let distortion = distortion_coefficients()
        .into_iter()
        .take(distortion_count)
        .collect::<Vec<_>>();
    let mut views = Vec::with_capacity(poses.len());
    let mut detections = Vec::with_capacity(poses.len());
    for (index, pose) in poses.iter().enumerate() {
        let translation = translation_for_board_center(board, pose.rvec, pose.center_camera);
        let corners = project_board(board, pose.rvec, translation, intrinsics, &distortion);
        views.push(ViewCalibrationResult {
            rotation_vector: pose.rvec,
            translation_vector: translation,
            projected_points: corners.clone(),
            reprojection_rmse: RMS_ERROR_PX,
            max_reprojection_error: RMS_ERROR_PX * 2.0,
        });
        detections.push(DetectedDatasetFrame {
            frame: Arc::new(CapturedDatasetFrame {
                channel: 0,
                width: IMAGE_SIZE.width,
                height: IMAGE_SIZE.height,
                luma: Arc::<[u8]>::from(vec![0_u8; 1].into_boxed_slice()),
                source: CapturedDatasetSource::SyntheticRgba {
                    frame_sequence: index as u64,
                },
            }),
            detection: ChessboardDetection {
                image_size: IMAGE_SIZE,
                corners,
            },
        });
    }
    (
        CalibrationSolution {
            image_size: IMAGE_SIZE,
            camera_matrix: intrinsics,
            distortion_coefficients: distortion,
            rms_error: RMS_ERROR_PX,
            calibration_flags: PANGBOT_CALIBRATION_FLAGS,
            views,
        },
        detections,
    )
}

fn fronto_parallel_only() -> Vec<SyntheticPose> {
    [
        ("center-z900", [0.0, 0.0, 0.0], [0.0, 0.0, 900.0]),
        ("left", [0.0, 0.0, 0.0], [-230.0, 0.0, 900.0]),
        ("right", [0.0, 0.0, 0.0], [230.0, 0.0, 900.0]),
        ("top", [0.0, 0.0, 0.0], [0.0, -150.0, 900.0]),
        ("bottom", [0.0, 0.0, 0.0], [0.0, 150.0, 900.0]),
        ("near", [0.0, 0.0, 0.0], [0.0, 0.0, 650.0]),
        ("far", [0.0, 0.0, 0.0], [0.0, 0.0, 1250.0]),
        ("upper-left", [0.0, 0.0, 0.0], [-200.0, -120.0, 950.0]),
        ("upper-right", [0.0, 0.0, 0.0], [200.0, -120.0, 950.0]),
        ("lower-left", [0.0, 0.0, 0.0], [-200.0, 120.0, 950.0]),
        ("lower-right", [0.0, 0.0, 0.0], [200.0, 120.0, 950.0]),
        ("center-z760", [0.0, 0.0, 0.0], [0.0, 0.0, 760.0]),
    ]
    .into_iter()
    .map(|(label, rvec, center_camera)| SyntheticPose {
        label,
        rvec,
        center_camera,
    })
    .collect()
}

fn same_depth_pose_diverse() -> Vec<SyntheticPose> {
    [
        ("center", [0.0, 0.0, 0.0], [0.0, 0.0, 900.0]),
        ("yaw-left", [0.0, 0.30, 0.0], [-130.0, 0.0, 900.0]),
        ("yaw-right", [0.0, -0.30, 0.0], [130.0, 0.0, 900.0]),
        ("pitch-up", [0.30, 0.0, 0.0], [0.0, -90.0, 900.0]),
        ("pitch-down", [-0.30, 0.0, 0.0], [0.0, 90.0, 900.0]),
        ("roll-left", [0.0, 0.0, 0.45], [-120.0, -80.0, 900.0]),
        ("roll-right", [0.0, 0.0, -0.45], [120.0, 80.0, 900.0]),
        ("diag-a", [0.22, 0.22, 0.25], [-150.0, 80.0, 900.0]),
        ("diag-b", [-0.22, -0.22, -0.25], [150.0, -80.0, 900.0]),
        ("corner-a", [0.26, -0.18, 0.35], [190.0, -120.0, 900.0]),
        ("corner-b", [-0.26, 0.18, -0.35], [-190.0, 120.0, 900.0]),
        ("mixed", [0.18, -0.26, -0.18], [0.0, 145.0, 900.0]),
    ]
    .into_iter()
    .map(|(label, rvec, center_camera)| SyntheticPose {
        label,
        rvec,
        center_camera,
    })
    .collect()
}

fn progressive_full_coverage() -> Vec<SyntheticPose> {
    [
        ("front-center", [0.0, 0.0, 0.0], [0.0, 0.0, 930.0]),
        ("front-left", [0.0, 0.0, 0.0], [-170.0, 0.0, 930.0]),
        ("front-right", [0.0, 0.0, 0.0], [170.0, 0.0, 930.0]),
        ("front-top", [0.0, 0.0, 0.0], [0.0, -115.0, 930.0]),
        ("front-bottom", [0.0, 0.0, 0.0], [0.0, 115.0, 930.0]),
        ("near-tilt-x", [0.34, 0.0, 0.0], [-90.0, -60.0, 680.0]),
        ("near-tilt-y", [0.0, -0.34, 0.0], [90.0, 60.0, 700.0]),
        ("far-tilt-x", [-0.34, 0.0, 0.0], [120.0, -80.0, 1280.0]),
        ("far-tilt-y", [0.0, 0.34, 0.0], [-120.0, 80.0, 1280.0]),
        ("roll-cw", [0.10, 0.18, 0.50], [170.0, -120.0, 860.0]),
        ("roll-ccw", [-0.10, -0.18, -0.50], [-170.0, 120.0, 860.0]),
        ("corner-ur", [0.28, -0.25, 0.32], [230.0, -145.0, 980.0]),
        ("corner-ll", [-0.28, 0.25, -0.32], [-230.0, 145.0, 980.0]),
        ("near-corner", [0.22, 0.28, -0.22], [-150.0, -105.0, 720.0]),
        ("far-corner", [-0.22, -0.28, 0.22], [150.0, 105.0, 1350.0]),
        ("steep-x", [0.45, 0.10, 0.15], [0.0, -130.0, 820.0]),
        ("steep-y", [-0.10, -0.45, -0.15], [210.0, 0.0, 880.0]),
        ("edge-left-roll", [0.18, 0.36, 0.58], [-260.0, 10.0, 1040.0]),
        (
            "edge-right-roll",
            [-0.18, -0.36, -0.58],
            [260.0, -10.0, 1040.0],
        ),
        ("final-mixed", [0.32, -0.20, 0.40], [0.0, 155.0, 780.0]),
    ]
    .into_iter()
    .map(|(label, rvec, center_camera)| SyntheticPose {
        label,
        rvec,
        center_camera,
    })
    .collect()
}

fn aggressive_edge_coverage() -> Vec<SyntheticPose> {
    let mut poses = progressive_full_coverage();
    let centers = [
        [-260.0, -150.0, 640.0],
        [260.0, -150.0, 640.0],
        [-260.0, 150.0, 640.0],
        [260.0, 150.0, 640.0],
        [-330.0, 0.0, 820.0],
        [330.0, 0.0, 820.0],
        [0.0, -210.0, 820.0],
        [0.0, 210.0, 820.0],
    ];
    let rotations = [
        [0.42, 0.24, 0.0],
        [-0.42, -0.24, 0.0],
        [0.24, -0.42, 0.35],
        [-0.24, 0.42, -0.35],
        [0.50, -0.15, 0.55],
        [-0.50, 0.15, -0.55],
    ];
    for center_camera in centers {
        for rvec in rotations {
            poses.push(SyntheticPose {
                label: "edge-extra",
                rvec,
                center_camera,
            });
        }
    }
    poses
}

fn translation_for_board_center(
    board: BoardSpec,
    rvec: [f64; 3],
    center_camera: [f64; 3],
) -> [f64; 3] {
    let rotation = rodrigues_matrix(rvec);
    let board_center = [
        f64::from(board.inner_cols - 1) * board.square_size * 0.5,
        f64::from(board.inner_rows - 1) * board.square_size * 0.5,
        0.0,
    ];
    let rotated = mat_vec_mul(rotation, board_center);
    [
        center_camera[0] - rotated[0],
        center_camera[1] - rotated[1],
        center_camera[2] - rotated[2],
    ]
}

fn project_board(
    board: BoardSpec,
    rvec: [f64; 3],
    tvec: [f64; 3],
    intrinsics: [f64; 9],
    distortion: &[f64],
) -> Vec<CalibrationPoint> {
    let mut points =
        Vec::with_capacity(usize::from(board.inner_cols) * usize::from(board.inner_rows));
    let rotation = rodrigues_matrix(rvec);
    for row in 0..board.inner_rows {
        for col in 0..board.inner_cols {
            let object = [
                f64::from(col) * board.square_size,
                f64::from(row) * board.square_size,
                0.0,
            ];
            let camera = add_vec(mat_vec_mul(rotation, object), tvec);
            assert!(camera[2] > 0.0, "synthetic board behind camera");
            let x = camera[0] / camera[2];
            let y = camera[1] / camera[2];
            let [xd, yd] = distort(x, y, distortion);
            points.push(CalibrationPoint {
                x: (intrinsics[0] * xd + intrinsics[2]) as f32,
                y: (intrinsics[4] * yd + intrinsics[5]) as f32,
            });
        }
    }
    points
}

fn camera_matrix() -> [f64; 9] {
    [1200.0, 0.0, 960.0, 0.0, 1180.0, 540.0, 0.0, 0.0, 1.0]
}

fn distortion_coefficients() -> Vec<f64> {
    vec![
        0.08, -0.025, 0.001, -0.0008, 0.004, 0.0, 0.0, 0.0, 0.0002, -0.0001, 0.00015, -0.00012,
    ]
}

fn distort(x: f64, y: f64, d: &[f64]) -> [f64; 2] {
    let get = |index: usize| d.get(index).copied().unwrap_or(0.0);
    let r2 = x.mul_add(x, y * y);
    let r4 = r2 * r2;
    let r6 = r4 * r2;
    let radial = (1.0 + get(0) * r2 + get(1) * r4 + get(4) * r6)
        / (1.0 + get(5) * r2 + get(6) * r4 + get(7) * r6);
    let xy = x * y;
    [
        x * radial + 2.0 * get(2) * xy + get(3) * (r2 + 2.0 * x * x) + get(8) * r2 + get(9) * r4,
        y * radial + get(2) * (r2 + 2.0 * y * y) + 2.0 * get(3) * xy + get(10) * r2 + get(11) * r4,
    ]
}

fn rodrigues_matrix(rvec: [f64; 3]) -> [[f64; 3]; 3] {
    let theta = (rvec[0] * rvec[0] + rvec[1] * rvec[1] + rvec[2] * rvec[2]).sqrt();
    if theta <= f64::EPSILON {
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    }
    let k = [rvec[0] / theta, rvec[1] / theta, rvec[2] / theta];
    let c = theta.cos();
    let s = theta.sin();
    let v = 1.0 - c;
    [
        [
            c + k[0] * k[0] * v,
            k[0] * k[1] * v - k[2] * s,
            k[0] * k[2] * v + k[1] * s,
        ],
        [
            k[1] * k[0] * v + k[2] * s,
            c + k[1] * k[1] * v,
            k[1] * k[2] * v - k[0] * s,
        ],
        [
            k[2] * k[0] * v - k[1] * s,
            k[2] * k[1] * v + k[0] * s,
            c + k[2] * k[2] * v,
        ],
    ]
}

fn mat_vec_mul(matrix: [[f64; 3]; 3], vector: [f64; 3]) -> [f64; 3] {
    [
        matrix[0][0] * vector[0] + matrix[0][1] * vector[1] + matrix[0][2] * vector[2],
        matrix[1][0] * vector[0] + matrix[1][1] * vector[1] + matrix[1][2] * vector[2],
        matrix[2][0] * vector[0] + matrix[2][1] * vector[1] + matrix[2][2] * vector[2],
    ]
}

fn add_vec(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn max_finite(values: &[f64]) -> f64 {
    values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .fold(0.0_f64, f64::max)
}

fn print_scenario(scenario: &ScenarioResult) {
    println!("\n## {}", scenario.name);
    println!("views,label,goal,rms_px,cond,focal_std,principal_std,dist_edge_std_max,gain,hint");
    for row in &scenario.rows {
        println!(
            "{},{},{},{},{},{},{},{},{},{}",
            row.views,
            row.label,
            row.goal,
            row.rms,
            row.cond,
            row.focal,
            row.principal,
            row.distortion,
            row.gain,
            row.hint
        );
    }
}

fn append_markdown_table(output: &mut String, scenario: &ScenarioResult) {
    writeln!(output, "\n### {}\n", scenario.name).unwrap();
    writeln!(
        output,
        "| views | added pose | goal | RMS(px) | cond(H) | fx/fy σ | cx/cy σ | max distortion edge σ | Δlogdet / reason |"
    )
    .unwrap();
    writeln!(output, "|---:|---|---|---:|---:|---:|---:|---:|---|").unwrap();
    for row in &scenario.rows {
        writeln!(
            output,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            row.views,
            row.label,
            row.goal,
            row.rms,
            row.cond,
            row.focal,
            row.principal,
            row.distortion,
            if row.goal == "ERR" {
                &row.hint
            } else {
                &row.gain
            }
        )
        .unwrap();
    }
}
