use camera_toolbox_adapters::calibration::OpenCvCalibrationBackend;
use camera_toolbox_app::ports::calibration::{CalibrationBackend, CalibrationCancellation};
use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationRequest, CalibrationSolution,
    ChessboardDetection, InitialIntrinsics,
};
use pongbot_calib_tool::observability::{analyze_solution, ObservabilityReport};
use pongbot_calib_tool::preview::{CapturedDatasetFrame, CapturedDatasetSource};
use pongbot_calib_tool::solve::DetectedDatasetFrame;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const IMAGE_SIZE: CalibrationImageSize = CalibrationImageSize {
    width: 1920,
    height: 1080,
};
const BOARD_COLS: u16 = 11;
const BOARD_ROWS: u16 = 8;
const BOARD_SQUARE_MM: f64 = 40.0;
const TRUE_FX: f64 = 1200.0;
const TRUE_FY: f64 = 1180.0;
const TRUE_CX: f64 = 960.0;
const TRUE_CY: f64 = 540.0;

#[derive(Clone, Copy)]
struct SyntheticPose {
    label: &'static str,
    rvec: [f64; 3],
    center_camera: [f64; 3],
}

struct ScenarioDefinition {
    name: &'static str,
    true_distortion_count: usize,
    poses: Vec<SyntheticPose>,
}

struct ScenarioData {
    rows: Vec<MetricRow>,
    corners: Vec<CornerRow>,
}

struct MetricRow {
    scenario: &'static str,
    true_distortion_count: usize,
    views: usize,
    added_pose: &'static str,
    solve_ok: bool,
    h2_ok: bool,
    goal_met: bool,
    rms_px: Option<f64>,
    fx: Option<f64>,
    fy: Option<f64>,
    cx: Option<f64>,
    cy: Option<f64>,
    fx_error_pct: Option<f64>,
    fy_error_pct: Option<f64>,
    cx_error_px: Option<f64>,
    cy_error_px: Option<f64>,
    k: [Option<f64>; 5],
    k_error: [Option<f64>; 5],
    cond_h: Option<f64>,
    focal_std_max_pct: Option<f64>,
    principal_std_max_px: Option<f64>,
    d5_edge_std_px: Option<f64>,
    d12_edge_std_px: Option<f64>,
    logdet_gain: Option<f64>,
    hint: String,
}

struct CornerRow {
    scenario: &'static str,
    true_distortion_count: usize,
    view: usize,
    label: &'static str,
    corner: usize,
    x: f32,
    y: f32,
}

#[test]
#[ignore = "generates H2 observability simulation CSV data for matplotlib reports"]
fn print_h2_observability_simulation_tables() {
    let board = BoardSpec::new(BOARD_COLS, BOARD_ROWS, BOARD_SQUARE_MM).expect("valid board");
    let scenarios = [
        ScenarioDefinition {
            name: "fronto_parallel_only",
            true_distortion_count: 12,
            poses: fronto_parallel_only(),
        },
        ScenarioDefinition {
            name: "same_depth_pose_diverse",
            true_distortion_count: 12,
            poses: same_depth_pose_diverse(),
        },
        ScenarioDefinition {
            name: "progressive_full_coverage_true_D12",
            true_distortion_count: 12,
            poses: progressive_full_coverage(),
        },
        ScenarioDefinition {
            name: "progressive_full_coverage_true_D5",
            true_distortion_count: 5,
            poses: progressive_full_coverage(),
        },
        ScenarioDefinition {
            name: "expected_progression_true_D5",
            true_distortion_count: 5,
            poses: expected_progression(),
        },
        ScenarioDefinition {
            name: "aggressive_edge_coverage_true_D5",
            true_distortion_count: 5,
            poses: aggressive_edge_coverage(),
        },
    ];

    let data = scenarios
        .into_iter()
        .map(|scenario| run_scenario(board, scenario))
        .collect::<Vec<_>>();

    let output_dir = std::env::var("PONGBOT_OBSERVABILITY_SIM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("h2_observability_sim"));
    std::fs::create_dir_all(&output_dir).expect("create output dir");
    write_metrics_csv(&output_dir.join("metrics.csv"), &data).expect("write metrics csv");
    write_corners_csv(&output_dir.join("corners.csv"), &data).expect("write corners csv");
    println!("H2 observability simulation data: {}", output_dir.display());
}

fn run_scenario(board: BoardSpec, scenario: ScenarioDefinition) -> ScenarioData {
    let backend = OpenCvCalibrationBackend;
    let cancellation = CalibrationCancellation::default();
    let mut rows = Vec::with_capacity(scenario.poses.len());
    let mut previous = None;
    for views in 1..=scenario.poses.len() {
        let detections = synthetic_detections(
            board,
            &scenario.poses[..views],
            scenario.true_distortion_count,
        );
        let added_pose = scenario.poses[views - 1].label;
        let row = match calibrate_from_detections(&backend, board, &detections, &cancellation) {
            Ok(solution) => {
                let analysis = analyze_solution(&solution, board, &detections, previous.as_ref());
                let h2_error = analysis.as_ref().err().cloned();
                let report = analysis.as_ref().ok();
                if let Some(report) = report {
                    previous = Some(report.clone());
                }
                metric_row_from_solution(
                    scenario.name,
                    scenario.true_distortion_count,
                    views,
                    added_pose,
                    &solution,
                    report,
                    h2_error,
                )
            }
            Err(error) => MetricRow {
                scenario: scenario.name,
                true_distortion_count: scenario.true_distortion_count,
                views,
                added_pose,
                solve_ok: false,
                h2_ok: false,
                goal_met: false,
                rms_px: None,
                fx: None,
                fy: None,
                cx: None,
                cy: None,
                fx_error_pct: None,
                fy_error_pct: None,
                cx_error_px: None,
                cy_error_px: None,
                k: [None; 5],
                k_error: [None; 5],
                cond_h: None,
                focal_std_max_pct: None,
                principal_std_max_px: None,
                d5_edge_std_px: None,
                d12_edge_std_px: None,
                logdet_gain: None,
                hint: format!("calibrate: {error}"),
            },
        };
        print_metric_row(&row);
        rows.push(row);
    }

    let final_detections =
        synthetic_detections(board, &scenario.poses, scenario.true_distortion_count);
    let mut corners = Vec::new();
    for (view_index, detection) in final_detections.iter().enumerate() {
        let label = scenario.poses[view_index].label;
        for (corner_index, point) in detection.detection.corners.iter().enumerate() {
            corners.push(CornerRow {
                scenario: scenario.name,
                true_distortion_count: scenario.true_distortion_count,
                view: view_index + 1,
                label,
                corner: corner_index,
                x: point.x,
                y: point.y,
            });
        }
    }

    ScenarioData { rows, corners }
}

fn calibrate_from_detections(
    backend: &OpenCvCalibrationBackend,
    board: BoardSpec,
    detections: &[DetectedDatasetFrame],
    cancellation: &CalibrationCancellation,
) -> Result<CalibrationSolution, String> {
    let image_points = detections
        .iter()
        .map(|frame| frame.detection.corners.clone())
        .collect::<Vec<_>>();
    let request = CalibrationRequest {
        image_size: IMAGE_SIZE,
        board,
        image_points,
        initial_intrinsics: InitialIntrinsics {
            camera_matrix: [900.0, 0.0, TRUE_CX, 0.0, 900.0, TRUE_CY, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 5],
        },
    };
    backend
        .calibrate(&request, cancellation)
        .map_err(|error| error.to_string())
}

fn metric_row_from_solution(
    scenario: &'static str,
    true_distortion_count: usize,
    views: usize,
    added_pose: &'static str,
    solution: &CalibrationSolution,
    report: Option<&ObservabilityReport>,
    h2_error: Option<String>,
) -> MetricRow {
    let d_true = true_distortion_coefficients(true_distortion_count);
    let d = &solution.distortion_coefficients;
    let k = [
        d.first().copied(),
        d.get(1).copied(),
        d.get(2).copied(),
        d.get(3).copied(),
        d.get(4).copied(),
    ];
    let k_error = [
        k[0].map(|value| value - d_true[0]),
        k[1].map(|value| value - d_true[1]),
        k[2].map(|value| value - d_true[2]),
        k[3].map(|value| value - d_true[3]),
        k[4].map(|value| value - d_true[4]),
    ];
    let fx = solution.camera_matrix[0];
    let fy = solution.camera_matrix[4];
    let cx = solution.camera_matrix[2];
    let cy = solution.camera_matrix[5];
    let (
        h2_ok,
        goal_met,
        cond_h,
        focal_std_max_pct,
        principal_std_max_px,
        d5_edge_std_px,
        d12_edge_std_px,
        logdet_gain,
        hint,
    ) = if let Some(report) = report {
        (
            true,
            report.goal_met(),
            Some(report.condition_number),
            Some(max_finite(&report.focal_relative_stddev) * 100.0),
            Some(max_finite(&report.principal_point_stddev_px)),
            Some(max_finite(report.primary_distortion_edge_stddev_px())),
            Some(max_finite(&report.distortion_edge_stddev_px)),
            report.last_info_gain,
            if report.goal_met() {
                "达标".to_owned()
            } else {
                report.missing_hint().to_owned()
            },
        )
    } else {
        (
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            h2_error.unwrap_or_else(|| "observability unavailable".to_owned()),
        )
    };

    MetricRow {
        scenario,
        true_distortion_count,
        views,
        added_pose,
        solve_ok: true,
        h2_ok,
        goal_met,
        rms_px: Some(solution.rms_error),
        fx: Some(fx),
        fy: Some(fy),
        cx: Some(cx),
        cy: Some(cy),
        fx_error_pct: Some((fx - TRUE_FX) / TRUE_FX * 100.0),
        fy_error_pct: Some((fy - TRUE_FY) / TRUE_FY * 100.0),
        cx_error_px: Some(cx - TRUE_CX),
        cy_error_px: Some(cy - TRUE_CY),
        k,
        k_error,
        cond_h,
        focal_std_max_pct,
        principal_std_max_px,
        d5_edge_std_px,
        d12_edge_std_px,
        logdet_gain,
        hint,
    }
}

fn synthetic_detections(
    board: BoardSpec,
    poses: &[SyntheticPose],
    true_distortion_count: usize,
) -> Vec<DetectedDatasetFrame> {
    let intrinsics = camera_matrix();
    let distortion = true_distortion_coefficients(true_distortion_count);
    poses
        .iter()
        .enumerate()
        .map(|(index, pose)| {
            let translation = translation_for_board_center(board, pose.rvec, pose.center_camera);
            let corners = project_board(board, pose.rvec, translation, intrinsics, &distortion);
            DetectedDatasetFrame {
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
            }
        })
        .collect()
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

fn expected_progression() -> Vec<SyntheticPose> {
    let mut poses = [
        ("fronto-fill:center", [0.0, 0.0, 0.0], [0.0, 0.0, 930.0]),
        ("fronto-fill:left", [0.0, 0.0, 0.0], [-210.0, 0.0, 930.0]),
        ("fronto-fill:right", [0.0, 0.0, 0.0], [210.0, 0.0, 930.0]),
        ("fronto-fill:top", [0.0, 0.0, 0.0], [0.0, -140.0, 930.0]),
        ("fronto-fill:bottom", [0.0, 0.0, 0.0], [0.0, 140.0, 930.0]),
        (
            "fronto-fill:upper-left",
            [0.0, 0.0, 0.0],
            [-210.0, -135.0, 930.0],
        ),
        (
            "fronto-fill:upper-right",
            [0.0, 0.0, 0.0],
            [210.0, -135.0, 930.0],
        ),
        (
            "fronto-fill:lower-left",
            [0.0, 0.0, 0.0],
            [-210.0, 135.0, 930.0],
        ),
        (
            "fronto-fill:lower-right",
            [0.0, 0.0, 0.0],
            [210.0, 135.0, 930.0],
        ),
        (
            "multi-pose:yaw-left",
            [0.0, 0.32, 0.0],
            [-130.0, 0.0, 930.0],
        ),
        (
            "multi-pose:yaw-right",
            [0.0, -0.32, 0.0],
            [130.0, 0.0, 930.0],
        ),
        ("multi-pose:pitch-up", [0.32, 0.0, 0.0], [0.0, -95.0, 930.0]),
        (
            "multi-pose:pitch-down",
            [-0.32, 0.0, 0.0],
            [0.0, 95.0, 930.0],
        ),
        (
            "multi-pose:roll-left",
            [0.0, 0.0, 0.50],
            [-130.0, -85.0, 930.0],
        ),
        (
            "multi-pose:roll-right",
            [0.0, 0.0, -0.50],
            [130.0, 85.0, 930.0],
        ),
        (
            "multi-pose:diag-a",
            [0.24, 0.24, 0.28],
            [-155.0, 85.0, 930.0],
        ),
        (
            "multi-pose:diag-b",
            [-0.24, -0.24, -0.28],
            [155.0, -85.0, 930.0],
        ),
        (
            "multi-pose:mixed-a",
            [0.26, -0.18, 0.38],
            [200.0, -125.0, 930.0],
        ),
        (
            "multi-pose:mixed-b",
            [-0.26, 0.18, -0.38],
            [-200.0, 125.0, 930.0],
        ),
        (
            "multi-pose:mixed-c",
            [0.18, -0.28, -0.22],
            [0.0, 145.0, 930.0],
        ),
        (
            "multi-pose:mixed-d",
            [-0.18, 0.28, 0.22],
            [0.0, -145.0, 930.0],
        ),
        ("depth:near-x", [0.36, 0.0, 0.12], [-95.0, -65.0, 680.0]),
        ("depth:near-y", [0.0, -0.36, -0.12], [95.0, 65.0, 700.0]),
        (
            "depth:near-roll",
            [0.22, 0.28, -0.28],
            [-155.0, -95.0, 720.0],
        ),
        (
            "depth:near-mixed",
            [-0.24, -0.30, 0.25],
            [155.0, 110.0, 740.0],
        ),
        ("depth:far-x", [-0.36, 0.0, -0.12], [120.0, -85.0, 1280.0]),
        ("depth:far-y", [0.0, 0.36, 0.12], [-120.0, 85.0, 1280.0]),
        (
            "depth:far-roll",
            [-0.22, -0.28, 0.28],
            [150.0, 105.0, 1350.0],
        ),
        (
            "depth:far-mixed",
            [0.24, 0.30, -0.25],
            [-150.0, -105.0, 1320.0],
        ),
        (
            "depth:mid-steep-x",
            [0.46, 0.10, 0.18],
            [0.0, -135.0, 820.0],
        ),
        (
            "depth:mid-steep-y",
            [-0.10, -0.46, -0.18],
            [215.0, 0.0, 880.0],
        ),
        (
            "depth:mid-roll-cw",
            [0.12, 0.20, 0.55],
            [175.0, -125.0, 860.0],
        ),
        (
            "depth:mid-roll-ccw",
            [-0.12, -0.20, -0.55],
            [-175.0, 125.0, 860.0],
        ),
    ]
    .into_iter()
    .map(|(label, rvec, center_camera)| SyntheticPose {
        label,
        rvec,
        center_camera,
    })
    .collect::<Vec<_>>();

    append_visible_edge_corner_poses(&mut poses, "edge-corner:visible-extra");
    poses
}
fn aggressive_edge_coverage() -> Vec<SyntheticPose> {
    let mut poses = progressive_full_coverage();
    append_visible_edge_corner_poses(&mut poses, "edge-visible-extra");
    poses
}

fn append_visible_edge_corner_poses(poses: &mut Vec<SyntheticPose>, label: &'static str) {
    let pose_specs = [
        ([-0.26, 0.40, 0.32], [-480.0, -259.3, 900.0]),
        ([-0.26, 0.40, 0.32], [-522.7, -282.4, 980.0]),
        ([-0.26, 0.40, 0.32], [-586.7, -316.9, 1100.0]),
        ([-0.26, -0.40, -0.32], [480.0, -259.3, 900.0]),
        ([-0.26, -0.40, -0.32], [522.7, -282.4, 980.0]),
        ([-0.26, -0.40, -0.32], [586.7, -316.9, 1100.0]),
        ([0.36, -0.24, 0.28], [-586.7, 316.9, 1100.0]),
        ([0.36, 0.24, -0.28], [-522.7, 282.4, 980.0]),
        ([0.26, 0.40, -0.32], [-522.7, 282.4, 980.0]),
        ([0.36, 0.24, -0.28], [586.7, 316.9, 1100.0]),
        ([0.26, -0.40, 0.32], [480.0, 259.3, 900.0]),
        ([0.36, -0.24, 0.28], [522.7, 282.4, 980.0]),
        ([0.18, 0.42, 0.62], [-471.5, 0.0, 820.0]),
        ([0.42, 0.12, 0.62], [-632.5, 0.0, 1100.0]),
        ([-0.26, 0.40, 0.32], [-563.5, 0.0, 980.0]),
        ([-0.26, -0.40, -0.32], [563.5, 0.0, 980.0]),
        ([-0.18, -0.42, -0.62], [517.5, 0.0, 900.0]),
        ([-0.26, -0.40, -0.32], [632.5, 0.0, 1100.0]),
        ([-0.36, 0.24, 0.28], [0.0, -307.3, 980.0]),
        ([-0.36, -0.24, -0.28], [0.0, -307.3, 980.0]),
        ([-0.26, 0.40, 0.32], [0.0, -344.9, 1100.0]),
        ([0.26, 0.40, -0.32], [0.0, 344.9, 1100.0]),
        ([0.26, -0.40, 0.32], [0.0, 344.9, 1100.0]),
        ([0.26, -0.40, 0.32], [0.0, 307.3, 980.0]),
    ];
    for (rvec, center_camera) in pose_specs {
        poses.push(SyntheticPose {
            label,
            rvec,
            center_camera,
        });
    }
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
            let u = intrinsics[0] * xd + intrinsics[2];
            let v = intrinsics[4] * yd + intrinsics[5];
            assert!(
                (0.0..=f64::from(IMAGE_SIZE.width)).contains(&u)
                    && (0.0..=f64::from(IMAGE_SIZE.height)).contains(&v),
                "synthetic full-board view projects outside image: ({u:.1}, {v:.1})"
            );
            points.push(CalibrationPoint {
                x: u as f32,
                y: v as f32,
            });
        }
    }
    points
}

fn camera_matrix() -> [f64; 9] {
    [TRUE_FX, 0.0, TRUE_CX, 0.0, TRUE_FY, TRUE_CY, 0.0, 0.0, 1.0]
}

fn true_distortion_coefficients(count: usize) -> Vec<f64> {
    vec![
        0.08, -0.025, 0.001, -0.0008, 0.004, 0.0, 0.0, 0.0, 0.0002, -0.0001, 0.00015, -0.00012,
    ]
    .into_iter()
    .take(count)
    .collect()
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

fn write_metrics_csv(path: &Path, data: &[ScenarioData]) -> std::io::Result<()> {
    let mut output = String::new();
    output.push_str("scenario,true_distortion_count,views,added_pose,solve_ok,h2_ok,goal_met,rms_px,fx,fy,cx,cy,fx_error_pct,fy_error_pct,cx_error_px,cy_error_px,k1,k2,p1,p2,k3,k1_error,k2_error,p1_error,p2_error,k3_error,cond_h,focal_std_max_pct,principal_std_max_px,d5_edge_std_px,d12_edge_std_px,logdet_gain,hint\n");
    for scenario in data {
        for row in &scenario.rows {
            output.push_str(&csv_row(&[
                row.scenario.to_owned(),
                row.true_distortion_count.to_string(),
                row.views.to_string(),
                row.added_pose.to_owned(),
                row.solve_ok.to_string(),
                row.h2_ok.to_string(),
                row.goal_met.to_string(),
                optional_value(row.rms_px),
                optional_value(row.fx),
                optional_value(row.fy),
                optional_value(row.cx),
                optional_value(row.cy),
                optional_value(row.fx_error_pct),
                optional_value(row.fy_error_pct),
                optional_value(row.cx_error_px),
                optional_value(row.cy_error_px),
                optional_value(row.k[0]),
                optional_value(row.k[1]),
                optional_value(row.k[2]),
                optional_value(row.k[3]),
                optional_value(row.k[4]),
                optional_value(row.k_error[0]),
                optional_value(row.k_error[1]),
                optional_value(row.k_error[2]),
                optional_value(row.k_error[3]),
                optional_value(row.k_error[4]),
                optional_value(row.cond_h),
                optional_value(row.focal_std_max_pct),
                optional_value(row.principal_std_max_px),
                optional_value(row.d5_edge_std_px),
                optional_value(row.d12_edge_std_px),
                optional_value(row.logdet_gain),
                row.hint.clone(),
            ]));
        }
    }
    std::fs::write(path, output)
}

fn write_corners_csv(path: &Path, data: &[ScenarioData]) -> std::io::Result<()> {
    let mut output = String::new();
    output.push_str("scenario,true_distortion_count,view,label,corner,x,y\n");
    for scenario in data {
        for corner in &scenario.corners {
            output.push_str(&csv_row(&[
                corner.scenario.to_owned(),
                corner.true_distortion_count.to_string(),
                corner.view.to_string(),
                corner.label.to_owned(),
                corner.corner.to_string(),
                corner.x.to_string(),
                corner.y.to_string(),
            ]));
        }
    }
    std::fs::write(path, output)
}

fn optional_value(value: Option<f64>) -> String {
    value
        .filter(|value| value.is_finite())
        .map(|value| format!("{value:.10}"))
        .unwrap_or_default()
}

fn csv_row(cells: &[String]) -> String {
    let mut row = cells
        .iter()
        .map(|cell| csv_cell(cell))
        .collect::<Vec<_>>()
        .join(",");
    row.push('\n');
    row
}

fn csv_cell(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn print_metric_row(row: &MetricRow) {
    println!(
        "{},{},{},solve={},h2={},goal={},rms={},fx_err={}%,fy_err={}%,cx_err={}px,cy_err={}px,cond={},d5_edge={},hint={}",
        row.scenario,
        row.true_distortion_count,
        row.views,
        row.solve_ok,
        row.h2_ok,
        row.goal_met,
        optional_value(row.rms_px),
        optional_value(row.fx_error_pct),
        optional_value(row.fy_error_pct),
        optional_value(row.cx_error_px),
        optional_value(row.cy_error_px),
        optional_value(row.cond_h),
        optional_value(row.d5_edge_std_px),
        row.hint
    );
}
