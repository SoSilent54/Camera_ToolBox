//! 标定求解节点：手动触发的按钮节点（范式样板）。
//!
//! `on_action(Trigger)` 时用节点 config 构造 `CalibrationRequest`，经 `CalibrationBackend`
//! 求解并输出通用 `data.packet.v1`；业务 schema 仍由 StructuredPacket 内容严格表达。

use std::sync::Arc;

use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationRequest, CalibrationSolution,
    Datum, InitialIntrinsics, PacketProvenance, StructuredPacket, TypedValue,
};

use crate::{
    engine::{
        DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime,
        NodeRuntimeState, NodeSpec,
        packet::{CalibrationBoardParams, CameraModelParams, DistortionModelParams},
    },
    ports::CalibrationCancellation,
};

use super::composite::{CalibrationDataset, parse_calibration_dataset};

pub struct CalibrationSolverFactory;

impl NodeFactory for CalibrationSolverFactory {
    fn kind(&self) -> &'static str {
        "calibrationSolver"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(CalibrationSolverNode {
            spec,
            dataset: None,
            board: None,
            camera_model: None,
            distortion_model: None,
        }))
    }
}

pub struct CalibrationSolverNode {
    spec: NodeSpec,
    dataset: Option<CalibrationDataset>,
    board: Option<Arc<CalibrationBoardParams>>,
    camera_model: Option<Arc<CameraModelParams>>,
    distortion_model: Option<Arc<DistortionModelParams>>,
}

impl NodeInstance for CalibrationSolverNode {
    fn kind(&self) -> &'static str {
        "calibrationSolver"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "trigger to solve");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        match (port, packet) {
            ("dataset", DataPacket::Dataset(dataset)) => {
                self.dataset = Some(parse_calibration_dataset(&dataset)?);
            }
            ("board", DataPacket::CalibrationBoardParams(board)) => self.board = Some(board),
            ("cameraModel", DataPacket::CameraModelParams(camera_model)) => {
                self.camera_model = Some(camera_model);
            }
            ("distortionModel", DataPacket::DistortionModelParams(distortion_model)) => {
                self.distortion_model = Some(distortion_model);
            }
            ("dataset", _) => {
                return Err(NodeError::Precondition(
                    "calibrationSolver.dataset requires calib.dataset".to_owned(),
                ));
            }
            ("board", _) => {
                return Err(NodeError::Precondition(
                    "calibrationSolver.board requires calib.board.params".to_owned(),
                ));
            }
            ("cameraModel", _) => {
                return Err(NodeError::Precondition(
                    "calibrationSolver.cameraModel requires calib.camera.model".to_owned(),
                ));
            }
            ("distortionModel", _) => {
                return Err(NodeError::Precondition(
                    "calibrationSolver.distortionModel requires calib.distortion.model".to_owned(),
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => self.solve(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_config_update(
        &mut self,
        config: serde_json::Value,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let next = NodeSpec {
            config,
            ..self.spec.clone()
        };
        Self::validate_config(&next)?;
        self.spec = next;
        Ok(())
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl CalibrationSolverNode {
    fn validate_config(spec: &NodeSpec) -> Result<(), NodeError> {
        CalibrationImageSize::new(
            config_u32(spec, "imageWidth", 1920),
            config_u32(spec, "imageHeight", 1080),
        )
        .map_err(|error| NodeError::Config(error.to_string()))?;
        BoardSpec::new(
            config_u16(spec, "boardCols", 8),
            config_u16(spec, "boardRows", 11),
            config_f64(spec, "squareSizeMm", 30.0),
        )
        .map_err(|error| NodeError::Config(error.to_string()))?;
        InitialIntrinsics {
            camera_matrix: [
                config_f64(spec, "fx", 1234.56),
                0.0,
                config_f64(spec, "cx", 960.0),
                0.0,
                config_f64(spec, "fy", 1234.56),
                config_f64(spec, "cy", 540.0),
                0.0,
                0.0,
                1.0,
            ],
            distortion_coefficients: vec![0.0; 12],
        }
        .validate()
        .map_err(|error| NodeError::Config(error.to_string()))?;
        Ok(())
    }

    fn solve(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let backend = rt.services().calibration_backend()?;
        let request = self.build_request()?;
        rt.report_state(NodeRuntimeState::Running, "solving calibration");
        let solution = backend
            .calibrate(&request, &CalibrationCancellation::default())
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let packet = Self::solution_packet(&solution)?;
        rt.emit("packet", DataPacket::StructuredPacket(Arc::new(packet)))?;
        rt.report_state(NodeRuntimeState::Idle, "solved");
        Ok(())
    }

    /// 将后端标定解投影为无领域运行时依赖的可审计字段包。
    fn solution_packet(solution: &CalibrationSolution) -> Result<StructuredPacket, NodeError> {
        const DISTORTION_NAMES: [&str; 12] = [
            "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
        ];

        if solution.distortion_coefficients.len() != DISTORTION_NAMES.len() {
            return Err(NodeError::Execution(format!(
                "calibration solution has {} distortion coefficients; {} are required for pinhole.rational-thin-prism.v1",
                solution.distortion_coefficients.len(),
                DISTORTION_NAMES.len()
            )));
        }

        let view_count = u32::try_from(solution.views.len()).map_err(|_| {
            NodeError::Execution("calibration solution view count exceeds u32".to_owned())
        })?;
        let mut fields = vec![
            Datum::new(
                "camera.model.id",
                TypedValue::Str("pinhole.rational-thin-prism.v1".to_owned()),
            )
            .with_semantic_type("camera.model-id"),
            Datum::new(
                "camera.image.width",
                TypedValue::U32(solution.image_size.width),
            )
            .with_unit("px")
            .with_semantic_type("image.width"),
            Datum::new(
                "camera.image.height",
                TypedValue::U32(solution.image_size.height),
            )
            .with_unit("px")
            .with_semantic_type("image.height"),
            Datum::new(
                "camera.intrinsics.fx",
                TypedValue::F64(solution.camera_matrix[0]),
            )
            .with_unit("px")
            .with_semantic_type("camera.focal-length"),
            Datum::new(
                "camera.intrinsics.fy",
                TypedValue::F64(solution.camera_matrix[4]),
            )
            .with_unit("px")
            .with_semantic_type("camera.focal-length"),
            Datum::new(
                "camera.intrinsics.cx",
                TypedValue::F64(solution.camera_matrix[2]),
            )
            .with_unit("px")
            .with_semantic_type("camera.principal-point"),
            Datum::new(
                "camera.intrinsics.cy",
                TypedValue::F64(solution.camera_matrix[5]),
            )
            .with_unit("px")
            .with_semantic_type("camera.principal-point"),
            Datum::new(
                "calibration.quality.rms_error",
                TypedValue::F64(solution.rms_error),
            )
            .with_unit("px")
            .with_semantic_type("calibration.reprojection-rmse"),
            Datum::new(
                "calibration.flags",
                TypedValue::I32(solution.calibration_flags),
            )
            .with_semantic_type("calibration.flags"),
            Datum::new("views.count", TypedValue::U32(view_count))
                .with_semantic_type("calibration.view-count"),
        ];

        for (index, value) in solution.camera_matrix.iter().copied().enumerate() {
            fields.push(
                Datum::new(
                    format!("camera.matrix.r{}{}", index / 3, index % 3),
                    TypedValue::F64(value),
                )
                .with_semantic_type("camera.intrinsic-matrix-element"),
            );
        }
        for (name, value) in DISTORTION_NAMES
            .iter()
            .zip(solution.distortion_coefficients.iter().copied())
        {
            fields.push(
                Datum::new(format!("distortion.{name}"), TypedValue::F64(value))
                    .with_unit("dimensionless")
                    .with_semantic_type("camera.distortion-coefficient"),
            );
        }
        for (view_index, view) in solution.views.iter().enumerate() {
            for (axis, value) in ["x", "y", "z"].iter().zip(view.rotation_vector) {
                fields.push(
                    Datum::new(
                        format!("views[{view_index}].rotation_vector.{axis}"),
                        TypedValue::F64(value),
                    )
                    .with_unit("rad")
                    .with_semantic_type("camera.rodrigues-angle"),
                );
            }
            for (axis, value) in ["x", "y", "z"].iter().zip(view.translation_vector) {
                fields.push(
                    Datum::new(
                        format!("views[{view_index}].translation_vector.{axis}"),
                        TypedValue::F64(value),
                    )
                    .with_unit("mm")
                    .with_semantic_type("camera.translation"),
                );
            }
            fields.push(
                Datum::new(
                    format!("views[{view_index}].reprojection.rmse"),
                    TypedValue::F64(view.reprojection_rmse),
                )
                .with_unit("px")
                .with_semantic_type("calibration.reprojection-rmse"),
            );
            fields.push(
                Datum::new(
                    format!("views[{view_index}].reprojection.max_error"),
                    TypedValue::F64(view.max_reprojection_error),
                )
                .with_unit("px")
                .with_semantic_type("calibration.reprojection-max-error"),
            );
            for (point_index, point) in view.projected_points.iter().enumerate() {
                fields.push(
                    Datum::new(
                        format!("views[{view_index}].projected_points[{point_index}].x"),
                        TypedValue::F32(point.x),
                    )
                    .with_unit("px")
                    .with_semantic_type("image.coordinate-x"),
                );
                fields.push(
                    Datum::new(
                        format!("views[{view_index}].projected_points[{point_index}].y"),
                        TypedValue::F32(point.y),
                    )
                    .with_unit("px")
                    .with_semantic_type("image.coordinate-y"),
                );
            }
        }

        StructuredPacket::new(
            "camera-toolbox.calib.solution.v1",
            PacketProvenance {
                source_port: Some("calibrationSolver.packet".to_owned()),
                ..PacketProvenance::default()
            },
            fields,
        )
        .map_err(|error| {
            NodeError::Execution(format!(
                "cannot project calibration solution packet: {error}"
            ))
        })
    }
    fn build_request(&self) -> Result<CalibrationRequest, NodeError> {
        let fallback_image_size = CalibrationImageSize::new(
            config_u32(&self.spec, "imageWidth", 1920),
            config_u32(&self.spec, "imageHeight", 1080),
        )
        .map_err(|error| NodeError::Config(error.to_string()))?;
        let image_size = self
            .camera_model
            .as_ref()
            .and_then(|params| params.image_size)
            .unwrap_or(fallback_image_size);
        let board = match self.board.as_deref() {
            Some(params) => board_spec_from_params(params)?,
            None => BoardSpec::new(
                config_u16(&self.spec, "boardCols", 8),
                config_u16(&self.spec, "boardRows", 11),
                config_f64(&self.spec, "squareSizeMm", 30.0),
            )
            .map_err(|error| NodeError::Config(error.to_string()))?,
        };

        let image_points: Vec<Vec<CalibrationPoint>> = if let Some(dataset) = &self.dataset {
            let mut points = Vec::with_capacity(dataset.samples.len());
            for sample in dataset.accepted_enabled_samples() {
                let detection = &sample.detection;
                if detection.image_size != image_size {
                    return Err(NodeError::Precondition(format!(
                        "dataset sample `{}` image size {:?} does not match active {:?}",
                        sample.id, detection.image_size, image_size
                    )));
                }
                detection.validate(board).map_err(|error| {
                    NodeError::Precondition(format!(
                        "dataset sample `{}` is invalid for active board: {error}",
                        sample.id
                    ))
                })?;
                points.push(detection.corners.clone());
            }
            if points.is_empty() {
                return Err(NodeError::Precondition(
                    "calibration dataset has no accepted/enabled samples".to_owned(),
                ));
            }
            points
        } else {
            self.spec
                .config
                .get("imagePoints")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default()
        };

        let initial_intrinsics = match (
            self.camera_model.as_deref(),
            self.distortion_model.as_deref(),
        ) {
            (Some(camera_model), Some(distortion_model)) => {
                initial_intrinsics_from_params(camera_model, distortion_model)?
            }
            (Some(camera_model), None) => {
                initial_intrinsics_from_params(camera_model, &DistortionModelParams::default())?
            }
            (None, _) => InitialIntrinsics {
                camera_matrix: [
                    config_f64(&self.spec, "fx", 1234.56),
                    0.0,
                    config_f64(&self.spec, "cx", 960.0),
                    0.0,
                    config_f64(&self.spec, "fy", 1234.56),
                    config_f64(&self.spec, "cy", 540.0),
                    0.0,
                    0.0,
                    1.0,
                ],
                distortion_coefficients: vec![0.0; 12],
            },
        };
        initial_intrinsics.validate().map_err(|error| {
            NodeError::Precondition(format!("invalid initial intrinsics: {error}"))
        })?;

        Ok(CalibrationRequest {
            image_size,
            board,
            image_points,
            initial_intrinsics,
        })
    }
}

fn board_spec_from_params(params: &CalibrationBoardParams) -> Result<BoardSpec, NodeError> {
    params
        .validate()
        .map_err(|error| NodeError::Precondition(format!("invalid board parameters: {error}")))?;
    BoardSpec::new(params.cols, params.rows, params.square_size_mm)
        .map_err(|error| NodeError::Precondition(error.to_string()))
}

fn initial_intrinsics_from_params(
    camera_model: &CameraModelParams,
    distortion_model: &DistortionModelParams,
) -> Result<InitialIntrinsics, NodeError> {
    camera_model
        .validate()
        .map_err(|error| NodeError::Precondition(format!("invalid camera parameters: {error}")))?;
    distortion_model.validate().map_err(|error| {
        NodeError::Precondition(format!("invalid distortion parameters: {error}"))
    })?;
    Ok(InitialIntrinsics {
        camera_matrix: [
            camera_model.fx,
            0.0,
            camera_model.cx,
            0.0,
            camera_model.fy,
            camera_model.cy,
            0.0,
            0.0,
            1.0,
        ],
        // 当前 only-none 公共模型在 OpenCV 调用边界以显式零向量表达。
        distortion_coefficients: vec![0.0; 5],
    })
}

fn config_u32(spec: &NodeSpec, key: &str, fallback: u32) -> u32 {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(fallback)
}

fn config_u16(spec: &NodeSpec, key: &str, fallback: u16) -> u16 {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(fallback)
}

fn config_f64(spec: &NodeSpec, key: &str, fallback: f64) -> f64 {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};

    use super::*;
    use crate::engine::{EngineServices, NodeReporter, OutputRegistry, SpawnContext};
    use crate::ports::{CalibrationBackend, CalibrationBackendError};
    use camera_toolbox_core::{ChessboardDetection, ChessboardDetectionOutcome};

    fn spec() -> NodeSpec {
        NodeSpec {
            id: "calib-1".to_owned(),
            kind: "calibrationSolver".to_owned(),
            title: "Calibration Solver".to_owned(),
            inputs: vec![],
            outputs: vec![crate::engine::PortSpec {
                id: "packet".to_owned(),
                label: "Calibration packet".to_owned(),
                kind: "data.packet.v1".to_owned(),
                cardinality: crate::engine::PortCardinality::One,
                required: false,
            }],
            config: serde_json::json!({
                "imageWidth": 1920,
                "imageHeight": 1080,
                "boardCols": 8,
                "boardRows": 11,
                "squareSizeMm": 30.0,
                "fx": 1234.56,
                "fy": 1234.56,
                "cx": 960.0,
                "cy": 540.0,
            }),
        }
    }

    fn runtime(
        services: EngineServices,
        state_tx: mpsc::Sender<crate::engine::NodeStatusReport>,
    ) -> NodeRuntime {
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("calib-1".to_owned(), state_tx, event_tx);
        let ctx = SpawnContext {
            outputs: OutputRegistry::default(),
            reporter,
            services: Arc::new(services),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        NodeRuntime::new(ctx)
    }

    struct RecordingBackend {
        called: Arc<Mutex<usize>>,
        solution: CalibrationSolution,
    }

    impl CalibrationBackend for RecordingBackend {
        fn build_information(&self) -> Result<String, CalibrationBackendError> {
            Ok("mock".to_owned())
        }

        fn detect_png(
            &self,
            _encoded_png: &[u8],
            _expected_size: CalibrationImageSize,
            _decoded_byte_limit: usize,
            _board: BoardSpec,
            _cancellation: &CalibrationCancellation,
        ) -> Result<ChessboardDetectionOutcome, CalibrationBackendError> {
            unreachable!("not exercised in solver tests")
        }

        fn estimate_pose(
            &self,
            _detection: &camera_toolbox_core::ChessboardDetection,
            _initial_intrinsics: &InitialIntrinsics,
            _board: BoardSpec,
            _cancellation: &CalibrationCancellation,
        ) -> Result<camera_toolbox_core::ViewCalibrationResult, CalibrationBackendError> {
            unreachable!("not exercised in solver tests")
        }

        fn calibrate(
            &self,
            _request: &CalibrationRequest,
            _cancellation: &CalibrationCancellation,
        ) -> Result<CalibrationSolution, CalibrationBackendError> {
            *self
                .called
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            Ok(self.solution.clone())
        }
    }

    fn solution_for(size: CalibrationImageSize) -> CalibrationSolution {
        CalibrationSolution {
            image_size: size,
            camera_matrix: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 12],
            rms_error: 0.0,
            calibration_flags: 0,
            views: vec![],
        }
    }

    fn last_state(
        rx: &mpsc::Receiver<crate::engine::NodeStatusReport>,
    ) -> Option<NodeRuntimeState> {
        let mut last = None;
        while let Ok(report) = rx.try_recv() {
            last = Some(report.state);
        }
        last
    }

    fn rich_dataset_sample(
        id: &str,
        detection: ChessboardDetection,
        accepted: bool,
        enabled: bool,
    ) -> serde_json::Value {
        let width = detection.image_size.width;
        let height = detection.image_size.height;
        serde_json::json!({
            "id": id,
            "imageRef": {
                "ref": format!("runtime://frame/{id}"),
                "width": width,
                "height": height,
                "format": "GRAY8",
            },
            "detection": detection,
            "score": {"score": 0.5, "frameSequence": 1},
            "acceptance": {"accepted": accepted, "enabled": enabled},
            "provenance": {
                "source": {"kind": "test"},
                "frameIdentity": {"frameSequence": 1},
            },
        })
    }

    #[test]
    fn factory_instantiates_with_expected_kind() {
        assert_eq!(CalibrationSolverFactory.kind(), "calibrationSolver");
        let instance = CalibrationSolverFactory
            .instantiate(spec())
            .expect("instantiate");
        assert_eq!(instance.kind(), "calibrationSolver");
    }

    #[test]
    fn on_start_reports_ready() {
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = CalibrationSolverFactory
            .instantiate(spec())
            .expect("instantiate");
        node.on_start(&mut rt).expect("on_start");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Ready));
    }

    #[test]
    fn build_request_reads_scalar_config_defaults() {
        let node = CalibrationSolverNode {
            spec: spec(),
            dataset: None,
            board: None,
            camera_model: None,
            distortion_model: None,
        };
        let request = node.build_request().expect("build request");
        assert_eq!(request.image_size.width, 1920);
        assert_eq!(request.image_size.height, 1080);
        assert_eq!(request.board.inner_cols, 8);
        assert_eq!(request.board.inner_rows, 11);
        assert_eq!(
            request.initial_intrinsics.distortion_coefficients,
            vec![0.0; 12]
        );
    }

    #[test]
    fn config_update_changes_solver_request_contract() {
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = CalibrationSolverNode {
            spec: spec(),
            dataset: None,
            board: None,
            camera_model: None,
            distortion_model: None,
        };
        node.on_config_update(
            serde_json::json!({
                "imageWidth": 1280,
                "imageHeight": 720,
                "boardCols": 7,
                "boardRows": 6,
                "squareSizeMm": 24.0,
                "fx": 500.0,
                "fy": 510.0,
                "cx": 640.0,
                "cy": 360.0,
            }),
            &mut rt,
        )
        .expect("update solver config");
        let request = node.build_request().expect("build updated request");
        assert_eq!(request.image_size.width, 1280);
        assert_eq!(request.image_size.height, 720);
        assert_eq!(request.board.inner_cols, 7);
        assert_eq!(request.board.inner_rows, 6);
        assert_eq!(request.initial_intrinsics.camera_matrix[0], 500.0);
        assert_eq!(request.initial_intrinsics.camera_matrix[2], 640.0);
    }

    #[test]
    fn parameter_packets_override_legacy_solver_config() {
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = CalibrationSolverNode {
            spec: spec(),
            dataset: None,
            board: None,
            camera_model: None,
            distortion_model: None,
        };
        node.on_input(
            "board",
            DataPacket::CalibrationBoardParams(Arc::new(CalibrationBoardParams::default())),
            &mut rt,
        )
        .expect("board input");
        node.on_input(
            "cameraModel",
            DataPacket::CameraModelParams(Arc::new(CameraModelParams::default())),
            &mut rt,
        )
        .expect("camera input");
        node.on_input(
            "distortionModel",
            DataPacket::DistortionModelParams(Arc::new(DistortionModelParams::default())),
            &mut rt,
        )
        .expect("distortion input");
        let request = node
            .build_request()
            .expect("request from parameter packets");
        assert_eq!(
            (
                request.board.inner_cols,
                request.board.inner_rows,
                request.board.square_size
            ),
            (11, 8, 40.0)
        );
        assert_eq!(request.initial_intrinsics.camera_matrix[0], 900.0);
        assert_eq!(
            request.initial_intrinsics.distortion_coefficients,
            vec![0.0; 5]
        );
    }

    #[test]
    fn rich_dataset_only_accepted_enabled_samples_become_calibration_image_points() {
        let active = ChessboardDetection {
            image_size: CalibrationImageSize::new(1920, 1080).expect("image size"),
            corners: vec![CalibrationPoint::new(12.0, 24.0); 88],
        };
        let rejected = ChessboardDetection {
            image_size: CalibrationImageSize::new(1920, 1080).expect("image size"),
            corners: vec![CalibrationPoint::new(48.0, 96.0); 88],
        };
        let disabled = ChessboardDetection {
            image_size: CalibrationImageSize::new(1920, 1080).expect("image size"),
            corners: vec![CalibrationPoint::new(120.0, 240.0); 88],
        };
        let dataset = serde_json::json!({
            "kind": "calib.dataset.v1",
            "board": null,
            "samples": [
                rich_dataset_sample("active", active, true, true),
                rich_dataset_sample("rejected", rejected, false, true),
                rich_dataset_sample("disabled", disabled, true, false),
            ],
            "count": 3,
        });
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = CalibrationSolverNode {
            spec: spec(),
            dataset: None,
            board: None,
            camera_model: None,
            distortion_model: None,
        };
        node.on_input("dataset", DataPacket::Dataset(Arc::new(dataset)), &mut rt)
            .expect("accept rich dataset");
        let request = node.build_request().expect("build request");
        assert_eq!(
            request.image_points,
            vec![vec![CalibrationPoint::new(12.0, 24.0); 88]]
        );
    }

    #[test]
    fn dataset_without_accepted_enabled_samples_is_rejected() {
        let detection = ChessboardDetection {
            image_size: CalibrationImageSize::new(1920, 1080).expect("image size"),
            corners: vec![CalibrationPoint::new(12.0, 24.0); 88],
        };
        let dataset = serde_json::json!({
            "kind": "calib.dataset.v1",
            "board": null,
            "samples": [rich_dataset_sample("rejected", detection, false, true)],
            "count": 1,
        });
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = CalibrationSolverNode {
            spec: spec(),
            dataset: None,
            board: None,
            camera_model: None,
            distortion_model: None,
        };
        node.on_input("dataset", DataPacket::Dataset(Arc::new(dataset)), &mut rt)
            .expect("accept rich dataset");
        let error = node
            .build_request()
            .expect_err("no accepted/enabled samples");
        assert!(matches!(error, NodeError::Precondition(_)));
    }

    #[test]
    fn trigger_without_backend_is_precondition() {
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = CalibrationSolverFactory
            .instantiate(spec())
            .expect("instantiate");
        let err = node
            .on_action(NodeAction::Trigger, &mut rt)
            .expect_err("no backend");
        assert!(matches!(err, NodeError::Precondition(_)), "got {err:?}");
    }

    #[test]
    fn trigger_solves_and_emits_structured_packet() {
        let size = CalibrationImageSize::new(1920, 1080).expect("size");
        let mut solution = solution_for(size);
        solution.distortion_coefficients = vec![0.1; 12];
        solution.views = vec![camera_toolbox_core::ViewCalibrationResult {
            rotation_vector: [0.1, 0.2, 0.3],
            translation_vector: [10.0, 20.0, 30.0],
            projected_points: vec![CalibrationPoint::new(12.0, 24.0)],
            reprojection_rmse: 0.25,
            max_reprojection_error: 0.5,
        }];
        let backend = Arc::new(RecordingBackend {
            called: Arc::new(Mutex::new(0)),
            solution,
        });
        let services = EngineServices {
            calibration: Some(backend.clone()),
            ..EngineServices::default()
        };
        let (state_tx, state_rx) = mpsc::channel();
        let mut outputs = OutputRegistry::default();
        let emitted = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = Arc::clone(&emitted);
        outputs.set_record(Arc::new(move |packet| sink.lock().push(packet)));

        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("calib-1".to_owned(), state_tx, event_tx);
        let ctx = SpawnContext {
            outputs,
            reporter,
            services: Arc::new(services),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        let mut rt = NodeRuntime::new(ctx);

        let mut node = CalibrationSolverFactory
            .instantiate(spec())
            .expect("instantiate");
        node.on_action(NodeAction::Trigger, &mut rt)
            .expect("trigger");

        let packets = emitted.lock();
        assert_eq!(packets.len(), 1, "packet must be emitted once");
        let DataPacket::StructuredPacket(packet) = &packets[0] else {
            panic!("solver must emit a structured packet");
        };
        assert_eq!(packet.schema, "camera-toolbox.calib.solution.v1");
        assert_eq!(
            packet.provenance.source_port.as_deref(),
            Some("calibrationSolver.packet")
        );
        assert_eq!(
            DataPacket::StructuredPacket(Arc::clone(packet)).port_kind(),
            "data.packet.v1"
        );
        assert!(packet.fields.iter().any(|field| {
            field.name == "camera.intrinsics.fx"
                && field.value == TypedValue::F64(1.0)
                && field.unit.as_deref() == Some("px")
                && field.semantic_type.as_deref() == Some("camera.focal-length")
        }));
        assert!(packet.fields.iter().any(|field| {
            field.name == "camera.model.id"
                && field.value == TypedValue::Str("pinhole.rational-thin-prism.v1".to_owned())
                && field.semantic_type.as_deref() == Some("camera.model-id")
        }));
        assert!(packet.fields.iter().any(|field| {
            field.name == "distortion.k1"
                && field.value == TypedValue::F64(0.1)
                && field.unit.as_deref() == Some("dimensionless")
        }));
        assert!(packet.fields.iter().any(|field| {
            field.name == "views[0].translation_vector.z"
                && field.value == TypedValue::F64(30.0)
                && field.unit.as_deref() == Some("mm")
        }));
        assert!(packet.fields.iter().any(|field| {
            field.name == "views[0].projected_points[0].y"
                && field.value == TypedValue::F32(24.0)
                && field.unit.as_deref() == Some("px")
        }));
        assert_eq!(
            *backend.called.lock().unwrap(),
            1,
            "backend.calibrate must be called once"
        );
        assert_eq!(
            last_state(&state_rx).filter(|state| *state == NodeRuntimeState::Idle),
            Some(NodeRuntimeState::Idle)
        );
    }

    #[test]
    fn unsupported_action_is_error() {
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = CalibrationSolverFactory
            .instantiate(spec())
            .expect("instantiate");
        let err = node
            .on_action(NodeAction::Connect, &mut rt)
            .expect_err("unsupported");
        assert!(matches!(err, NodeError::UnsupportedAction(_)));
    }

    #[test]
    fn on_stop_reports_idle() {
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = CalibrationSolverFactory
            .instantiate(spec())
            .expect("instantiate");
        node.on_stop(&mut rt).expect("on_stop");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Idle));
    }
}
