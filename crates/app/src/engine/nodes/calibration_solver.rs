//! 标定求解节点：手动触发的按钮节点（范式样板）。
//!
//! `on_action(Trigger)` 时用节点 config 构造 `CalibrationRequest`，经 `CalibrationBackend`
//! 求解并输出 `calib.solution`。这是「按钮触发 → 后端计算 → 结果输出」的完整样板。

use std::sync::Arc;

use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationRequest, CalibrationSolution,
    InitialIntrinsics,
};

use crate::{
    engine::{DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec},
    ports::CalibrationCancellation,
};

pub struct CalibrationSolverFactory;

impl NodeFactory for CalibrationSolverFactory {
    fn kind(&self) -> &'static str {
        "calibrationSolver"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(CalibrationSolverNode { spec }))
    }
}

pub struct CalibrationSolverNode {
    spec: NodeSpec,
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
        _port: &str,
        _packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => self.solve(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl CalibrationSolverNode {
    fn solve(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let backend = rt.services().calibration_backend()?;
        let request = self.build_request()?;
        rt.report_state(NodeRuntimeState::Running, "solving calibration");
        let solution: CalibrationSolution = backend
            .calibrate(&request, &CalibrationCancellation::default())
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        rt.emit("solution", DataPacket::Solution(Arc::new(solution)))?;
        rt.report_state(NodeRuntimeState::Idle, "solved");
        Ok(())
    }

    fn build_request(&self) -> Result<CalibrationRequest, NodeError> {
        let image_size = CalibrationImageSize::new(
            config_u32(&self.spec, "imageWidth", 1920),
            config_u32(&self.spec, "imageHeight", 1080),
        )
        .map_err(|error| NodeError::Config(error.to_string()))?;
        let board = BoardSpec::new(
            config_u16(&self.spec, "boardCols", 8),
            config_u16(&self.spec, "boardRows", 11),
            config_f64(&self.spec, "squareSizeMm", 30.0),
        )
        .map_err(|error| NodeError::Config(error.to_string()))?;
        let fx = config_f64(&self.spec, "fx", 1234.56);
        let fy = config_f64(&self.spec, "fy", 1234.56);
        let cx = config_f64(&self.spec, "cx", 960.0);
        let cy = config_f64(&self.spec, "cy", 540.0);
        let camera_matrix = [fx, 0.0, cx, 0.0, fy, cy, 0.0, 0.0, 1.0];

        let distortion_coefficients: Vec<f64> = self
            .spec
            .config
            .get("distortionCoefficients")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_f64)
                    .collect()
            })
            .unwrap_or_default();
        let distortion_coefficients = if distortion_coefficients.is_empty() {
            vec![0.0; 12]
        } else {
            distortion_coefficients
        };

        let image_points: Vec<Vec<CalibrationPoint>> = self
            .spec
            .config
            .get("imagePoints")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default();

        Ok(CalibrationRequest {
            image_size,
            board,
            image_points,
            initial_intrinsics: InitialIntrinsics {
                camera_matrix,
                distortion_coefficients,
            },
        })
    }
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
    use camera_toolbox_core::ChessboardDetectionOutcome;
    use crate::engine::{
        EngineServices, NodeReporter, OutputRegistry, SpawnContext,
    };
    use crate::ports::{CalibrationBackend, CalibrationBackendError};

    fn spec() -> NodeSpec {
        NodeSpec {
            id: "calib-1".to_owned(),
            kind: "calibrationSolver".to_owned(),
            title: "Calibration Solver".to_owned(),
            inputs: vec![],
            outputs: vec![crate::engine::PortSpec {
                id: "solution".to_owned(),
                label: "Solution".to_owned(),
                kind: "calib.solution".to_owned(),
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

    fn runtime(services: EngineServices, state_tx: mpsc::Sender<crate::engine::NodeStatusReport>) -> NodeRuntime {
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
            *self.called.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
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

    fn last_state(rx: &mpsc::Receiver<crate::engine::NodeStatusReport>) -> Option<NodeRuntimeState> {
        let mut last = None;
        while let Ok(report) = rx.try_recv() {
            last = Some(report.state);
        }
        last
    }

    #[test]
    fn factory_instantiates_with_expected_kind() {
        assert_eq!(CalibrationSolverFactory.kind(), "calibrationSolver");
        let instance = CalibrationSolverFactory.instantiate(spec()).expect("instantiate");
        assert_eq!(instance.kind(), "calibrationSolver");
    }

    #[test]
    fn on_start_reports_ready() {
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = CalibrationSolverFactory.instantiate(spec()).expect("instantiate");
        node.on_start(&mut rt).expect("on_start");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Ready));
    }

    #[test]
    fn build_request_reads_config_defaults_and_distortion() {
        let node = CalibrationSolverNode { spec: spec() };
        let request = node.build_request().expect("build request");
        assert_eq!(request.image_size.width, 1920);
        assert_eq!(request.image_size.height, 1080);
        assert_eq!(request.board.inner_cols, 8);
        assert_eq!(request.board.inner_rows, 11);
        // 未提供 distortionCoefficients → 默认 12 个 0
        assert_eq!(request.initial_intrinsics.distortion_coefficients, vec![0.0; 12]);
    }

    #[test]
    fn build_request_parses_distortion_coefficients() {
        let mut s = spec();
        s.config["distortionCoefficients"] = serde_json::json!([0.1, 0.2, 0.3]);
        let node = CalibrationSolverNode { spec: s };
        let request = node.build_request().expect("build request");
        assert_eq!(request.initial_intrinsics.distortion_coefficients, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn trigger_without_backend_is_precondition() {
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = CalibrationSolverFactory.instantiate(spec()).expect("instantiate");
        let err = node.on_action(NodeAction::Trigger, &mut rt).expect_err("no backend");
        assert!(matches!(err, NodeError::Precondition(_)), "got {err:?}");
    }

    #[test]
    fn trigger_solves_and_emits_solution() {
        let size = CalibrationImageSize::new(1920, 1080).expect("size");
        let backend = Arc::new(RecordingBackend {
            called: Arc::new(Mutex::new(0)),
            solution: solution_for(size),
        });
        let services = EngineServices {
            calibration: Some(backend.clone()),
            ..EngineServices::default()
        };
        let (state_tx, state_rx) = mpsc::channel();
        let mut outputs = OutputRegistry::default();
        let emitted: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let sink = Arc::clone(&emitted);
        outputs.set_record(Arc::new(move |_| *sink.lock().unwrap() += 1));

        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("calib-1".to_owned(), state_tx, event_tx);
        let ctx = SpawnContext {
            outputs: outputs.clone(),
            reporter,
            services: Arc::new(services),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        let mut rt = NodeRuntime::new(ctx);

        let mut node = CalibrationSolverFactory.instantiate(spec()).expect("instantiate");
        node.on_action(NodeAction::Trigger, &mut rt).expect("trigger");

        assert_eq!(*emitted.lock().unwrap(), 1, "solution must be emitted once");
        assert_eq!(*backend.called.lock().unwrap(), 1, "backend.calibrate must be called once");
        // 求解后回落到 idle
        assert_eq!(
            last_state(&state_rx).filter(|s| *s == NodeRuntimeState::Idle),
            Some(NodeRuntimeState::Idle)
        );
    }

    #[test]
    fn unsupported_action_is_error() {
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = CalibrationSolverFactory.instantiate(spec()).expect("instantiate");
        let err = node.on_action(NodeAction::Connect, &mut rt).expect_err("unsupported");
        assert!(matches!(err, NodeError::UnsupportedAction(_)));
    }

    #[test]
    fn on_stop_reports_idle() {
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(EngineServices::default(), state_tx);
        let mut node = CalibrationSolverFactory.instantiate(spec()).expect("instantiate");
        node.on_stop(&mut rt).expect("on_stop");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Idle));
    }
}
