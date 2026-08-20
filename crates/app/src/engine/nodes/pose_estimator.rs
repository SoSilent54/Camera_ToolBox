//! 单帧棋盘检测的 PnP 姿态节点。
//!
//! 输入必须来自同一套显式标定板、相机和畸变参数；在四项输入齐全前不产生猜测性姿态。

use std::sync::Arc;

use camera_toolbox_core::{BoardSpec, InitialIntrinsics};

use crate::{
    engine::{
        packet::{
            CalibrationBoardParams, CalibrationVector3, CameraModelParams, DetectionPacket,
            DetectionPose, DistortionModelParams,
        },
        DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime,
        NodeRuntimeState, NodeSpec,
    },
    ports::CalibrationCancellation,
};

const POSE_ESTIMATOR_KIND: &str = "poseEstimator";

/// PoseEstimator 节点工厂。
pub struct PoseEstimatorFactory;

impl NodeFactory for PoseEstimatorFactory {
    fn kind(&self) -> &'static str {
        POSE_ESTIMATOR_KIND
    }
    fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(PoseEstimatorNode {
            detection: None,
            board: None,
            camera_model: None,
            distortion_model: None,
        }))
    }
}

/// 以最近一次完整参数集为单帧检测计算 `T_camera_board`。
pub struct PoseEstimatorNode {
    detection: Option<Arc<DetectionPacket>>,
    board: Option<Arc<CalibrationBoardParams>>,
    camera_model: Option<Arc<CameraModelParams>>,
    distortion_model: Option<Arc<DistortionModelParams>>,
}

impl NodeInstance for PoseEstimatorNode {
    fn kind(&self) -> &'static str {
        POSE_ESTIMATOR_KIND
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for detection and camera parameters");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        match (port, packet) {
            ("detection", DataPacket::Detection(detection)) => self.detection = Some(detection),
            ("board", DataPacket::CalibrationBoardParams(board)) => self.board = Some(board),
            ("cameraModel", DataPacket::CameraModelParams(camera_model)) => {
                self.camera_model = Some(camera_model)
            }
            ("distortionModel", DataPacket::DistortionModelParams(distortion_model)) => {
                self.distortion_model = Some(distortion_model)
            }
            ("detection", _) => {
                return Err(NodeError::Precondition(
                    "poseEstimator.detection requires calib.detection".to_owned(),
                ));
            }
            ("board", _) => {
                return Err(NodeError::Precondition(
                    "poseEstimator.board requires calib.board.params".to_owned(),
                ));
            }
            ("cameraModel", _) => {
                return Err(NodeError::Precondition(
                    "poseEstimator.cameraModel requires calib.camera.model".to_owned(),
                ));
            }
            ("distortionModel", _) => {
                return Err(NodeError::Precondition(
                    "poseEstimator.distortionModel requires calib.distortion.model".to_owned(),
                ));
            }
            _ => return Ok(()),
        }
        self.try_estimate(rt)
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl PoseEstimatorNode {
    fn try_estimate(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let (Some(detection), Some(board), Some(camera_model), Some(distortion_model)) = (
            self.detection.as_ref(),
            self.board.as_ref(),
            self.camera_model.as_ref(),
            self.distortion_model.as_ref(),
        ) else {
            rt.report_state(
                NodeRuntimeState::Ready,
                "waiting for detection, board, camera model, and distortion model",
            );
            return Ok(());
        };

        board
            .validate()
            .map_err(|error| NodeError::Precondition(format!("invalid board parameters: {error}")))?;
        camera_model
            .validate()
            .map_err(|error| NodeError::Precondition(format!("invalid camera parameters: {error}")))?;
        distortion_model.validate().map_err(|error| {
            NodeError::Precondition(format!("invalid distortion parameters: {error}"))
        })?;
        if let Some(expected_size) = camera_model.image_size {
            if expected_size != detection.detection.image_size {
                return Err(NodeError::Precondition(format!(
                    "detection image size {:?} does not match camera model {:?}",
                    detection.detection.image_size, expected_size
                )));
            }
        }

        // PnP object points 与输出平移统一用米，避免把 UI 的 mm 板规格泄漏进位姿语义。
        let board_spec = BoardSpec::new(
            board.cols,
            board.rows,
            board.square_size_meters(),
        )
        .map_err(|error| NodeError::Precondition(error.to_string()))?;
        detection
            .detection
            .validate(board_spec)
            .map_err(|error| NodeError::Precondition(format!("invalid detection: {error}")))?;
        let intrinsics = InitialIntrinsics {
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
            // `none` 是公开参数契约；OpenCV 调用边界仍需要显式零向量而不是空矩阵。
            distortion_coefficients: vec![0.0; 5],
        };
        intrinsics
            .validate()
            .map_err(|error| NodeError::Precondition(error.to_string()))?;

        rt.report_state(NodeRuntimeState::Running, "estimating T_camera_board");
        let result = rt
            .services()
            .calibration_backend()?
            .estimate_pose(
                detection.detection.as_ref(),
                &intrinsics,
                board_spec,
                &CalibrationCancellation::default(),
            )
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let pose = DetectionPose::new(
            detection.frame_identity.clone(),
            CalibrationVector3::new(
                result.translation_vector[0],
                result.translation_vector[1],
                result.translation_vector[2],
            ),
            CalibrationVector3::new(
                result.rotation_vector[0],
                result.rotation_vector[1],
                result.rotation_vector[2],
            ),
            Some(result.reprojection_rmse),
        )
        .map_err(|error| NodeError::Execution(format!("invalid PnP result: {error}")))?;
        rt.emit("pose", DataPacket::DetectionPose(Arc::new(pose)))?;
        rt.report_state(NodeRuntimeState::Idle, "pose estimated");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{atomic::AtomicBool, mpsc, Arc, Mutex};

    use camera_toolbox_core::{
        CalibrationImageSize, CalibrationPoint, CalibrationSolution, ChessboardDetection,
        ChessboardDetectionOutcome, ViewCalibrationResult,
    };

    use super::*;
    use crate::{
        engine::{EngineServices, FrameProvenance, ImageFrameIdentity, NodeReporter, OutputRegistry, SpawnContext},
        platform::SourcePts,
        ports::{CalibrationBackend, CalibrationBackendError},
    };

    fn node() -> PoseEstimatorNode {
        PoseEstimatorNode {
            detection: None,
            board: None,
            camera_model: None,
            distortion_model: None,
        }
    }

    fn runtime(recorded: Arc<Mutex<Vec<DataPacket>>>) -> NodeRuntime {
        let (state_tx, _state_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let mut outputs = OutputRegistry::default();
        outputs.set_record(Arc::new(move |packet| recorded.lock().expect("record lock").push(packet)));
        NodeRuntime::new(SpawnContext {
            outputs,
            reporter: NodeReporter::new("pose-test".to_owned(), state_tx, event_tx),
            services: Arc::new(EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        })
    }

    fn detection_packet() -> Arc<DetectionPacket> {
        Arc::new(DetectionPacket {
            detection: Arc::new(ChessboardDetection {
                image_size: CalibrationImageSize::new(1920, 1080).expect("image size"),
                corners: vec![CalibrationPoint::new(10.0, 20.0); 88],
            }),
            frame_identity: ImageFrameIdentity {
                provenance: FrameProvenance::Unknown { reason: "test".to_owned() },
                frame_sequence: 7,
                source_pts: SourcePts::Unavailable { reason: "test".to_owned() },
                host_monotonic_time_ns: 1,
                device_timestamp_ns: None,
            },
        })
    }

    #[test]
    fn incomplete_inputs_never_emit_placeholder_pose() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(Arc::clone(&recorded));
        let mut estimator = node();
        estimator
            .on_input("detection", DataPacket::Detection(detection_packet()), &mut rt)
            .expect("pending detection is accepted");
        estimator
            .on_input("board", DataPacket::CalibrationBoardParams(Arc::new(CalibrationBoardParams::default())), &mut rt)
            .expect("pending board is accepted");
        estimator
            .on_input("cameraModel", DataPacket::CameraModelParams(Arc::new(CameraModelParams::default())), &mut rt)
            .expect("pending camera model is accepted");
        assert!(recorded.lock().expect("record lock").is_empty());

        let error = estimator
            .on_input("distortionModel", DataPacket::DistortionModelParams(Arc::new(DistortionModelParams::default())), &mut rt)
            .expect_err("missing backend must not yield a fake pose");
        assert!(matches!(error, NodeError::Precondition(_)));
        assert!(recorded.lock().expect("record lock").is_empty());
    }

    struct PoseBackend {
        board: Arc<Mutex<Option<BoardSpec>>>,
    }

    impl CalibrationBackend for PoseBackend {
        fn build_information(&self) -> Result<String, CalibrationBackendError> {
            Ok("test".to_owned())
        }

        fn detect_png(
            &self,
            _encoded_png: &[u8],
            _expected_size: CalibrationImageSize,
            _decoded_byte_limit: usize,
            _board: BoardSpec,
            _cancellation: &CalibrationCancellation,
        ) -> Result<ChessboardDetectionOutcome, CalibrationBackendError> {
            unreachable!("not used by pose estimator")
        }

        fn estimate_pose(
            &self,
            _detection: &ChessboardDetection,
            _initial_intrinsics: &InitialIntrinsics,
            board: BoardSpec,
            _cancellation: &CalibrationCancellation,
        ) -> Result<ViewCalibrationResult, CalibrationBackendError> {
            *self.board.lock().expect("board lock") = Some(board);
            Ok(ViewCalibrationResult {
                rotation_vector: [0.1, 0.2, 0.3],
                translation_vector: [1.0, 2.0, 3.0],
                projected_points: Vec::new(),
                reprojection_rmse: 0.5,
                max_reprojection_error: 0.8,
            })
        }

        fn calibrate(
            &self,
            _request: &camera_toolbox_core::CalibrationRequest,
            _cancellation: &CalibrationCancellation,
        ) -> Result<CalibrationSolution, CalibrationBackendError> {
            unreachable!("not used by pose estimator")
        }
    }

    #[test]
    fn complete_inputs_emit_meter_t_camera_board_pose() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let board_seen = Arc::new(Mutex::new(None));
        let backend = Arc::new(PoseBackend { board: Arc::clone(&board_seen) });
        let (state_tx, _state_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let mut outputs = OutputRegistry::default();
        let sink = Arc::clone(&recorded);
        outputs.set_record(Arc::new(move |packet| sink.lock().expect("record lock").push(packet)));
        let mut rt = NodeRuntime::new(SpawnContext {
            outputs,
            reporter: NodeReporter::new("pose-test".to_owned(), state_tx, event_tx),
            services: Arc::new(EngineServices { calibration: Some(backend), ..EngineServices::default() }),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        });
        let mut estimator = node();
        for (port, packet) in [
            ("board", DataPacket::CalibrationBoardParams(Arc::new(CalibrationBoardParams::default()))),
            ("cameraModel", DataPacket::CameraModelParams(Arc::new(CameraModelParams::default()))),
            ("distortionModel", DataPacket::DistortionModelParams(Arc::new(DistortionModelParams::default()))),
            ("detection", DataPacket::Detection(detection_packet())),
        ] {
            estimator.on_input(port, packet, &mut rt).expect("valid input");
        }
        let recorded = recorded.lock().expect("record lock");
        let [DataPacket::DetectionPose(pose)] = recorded.as_slice() else {
            panic!("complete inputs must emit one pose");
        };
        assert_eq!(pose.convention, crate::engine::DetectionPoseConvention::TCameraBoard);
        assert_eq!(pose.translation_m, CalibrationVector3::new(1.0, 2.0, 3.0));
        assert_eq!(pose.rotation_rodrigues, CalibrationVector3::new(0.1, 0.2, 0.3));
        assert_eq!(pose.reprojection_error_px, Some(0.5));
        assert_eq!(board_seen.lock().expect("board lock").expect("board").square_size, 0.04);
    }
}
