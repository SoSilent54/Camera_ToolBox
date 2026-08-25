//! 标定参数源节点：将经校验的轻量配置显式发射为强类型参数包。
//!
//! 参数只在 `Trigger` 时输出；运行时配置更新先校验再原子替换，避免无效参数污染后续标定链路。

use std::sync::Arc;

use camera_toolbox_core::CalibrationImageSize;
use serde::Deserialize;

use crate::engine::{
    DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState,
    NodeSpec,
    packet::{
        CalibrationBoardKind, CalibrationBoardParams, CameraModelKind, CameraModelParams,
        DistortionModelKind, DistortionModelParams,
    },
};

const CALIBRATION_BOARD_PARAMS_KIND: &str = "calibrationBoardParams";
const CAMERA_INITIAL_PARAMS_KIND: &str = "cameraInitialParams";

/// 标定板参数节点工厂。
pub struct CalibrationBoardParamsFactory;

impl NodeFactory for CalibrationBoardParamsFactory {
    fn kind(&self) -> &'static str {
        CALIBRATION_BOARD_PARAMS_KIND
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(CalibrationBoardParamsNode { spec }))
    }
}

/// 把棋盘格规格配置发射为 `calib.board.params` 包。
pub struct CalibrationBoardParamsNode {
    spec: NodeSpec,
}

impl CalibrationBoardParamsNode {
    fn params(&self) -> Result<CalibrationBoardParams, NodeError> {
        let config: CalibrationBoardConfig =
            parse_config(&self.spec, CALIBRATION_BOARD_PARAMS_KIND)?;
        let params = CalibrationBoardParams {
            board_kind: config.board_kind,
            cols: config.board_cols,
            rows: config.board_rows,
            square_size_mm: config.square_size_mm,
            ..CalibrationBoardParams::default()
        };
        params
            .validate()
            .map_err(|error| NodeError::Config(error.to_string()))?;
        Ok(params)
    }

    fn emit_params(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let params = self.params()?;
        rt.emit(
            "board",
            DataPacket::CalibrationBoardParams(Arc::new(params)),
        )?;
        rt.report_state(
            NodeRuntimeState::Ready,
            "calibration board parameters emitted",
        );
        Ok(())
    }
}

impl NodeInstance for CalibrationBoardParamsNode {
    fn kind(&self) -> &'static str {
        CALIBRATION_BOARD_PARAMS_KIND
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.params()?;
        rt.report_state(
            NodeRuntimeState::Ready,
            "trigger to emit calibration board parameters",
        );
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
            NodeAction::Trigger => self.emit_params(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_config_update(
        &mut self,
        config: serde_json::Value,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let candidate = Self {
            spec: NodeSpec {
                config,
                ..self.spec.clone()
            },
        };
        candidate.params()?;
        self.spec = candidate.spec;
        Ok(())
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 相机初始内参与镜头畸变参数节点工厂。
pub struct CameraInitialParamsFactory;

impl NodeFactory for CameraInitialParamsFactory {
    fn kind(&self) -> &'static str {
        CAMERA_INITIAL_PARAMS_KIND
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(CameraInitialParamsNode { spec }))
    }
}

/// 把相机内参与畸变配置分别发射为 `calib.camera.model` 和 `calib.distortion.model` 包。
pub struct CameraInitialParamsNode {
    spec: NodeSpec,
}

impl CameraInitialParamsNode {
    fn params(&self) -> Result<(CameraModelParams, DistortionModelParams), NodeError> {
        let config: CameraInitialConfig = parse_config(&self.spec, CAMERA_INITIAL_PARAMS_KIND)?;
        let image_size = CalibrationImageSize::new(config.image_width, config.image_height)
            .map_err(|error| NodeError::Config(error.to_string()))?;
        let camera_model = CameraModelParams {
            model: config.camera_model_kind,
            fx: config.fx,
            fy: config.fy,
            cx: config.cx,
            cy: config.cy,
            image_size: Some(image_size),
            ..CameraModelParams::default()
        };
        camera_model
            .validate()
            .map_err(|error| NodeError::Config(error.to_string()))?;
        let distortion_model = DistortionModelParams {
            model: config.distortion_kind,
            ..DistortionModelParams::default()
        };
        distortion_model
            .validate()
            .map_err(|error| NodeError::Config(error.to_string()))?;
        Ok((camera_model, distortion_model))
    }

    fn emit_params(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let (camera_model, distortion_model) = self.params()?;
        rt.emit(
            "cameraModel",
            DataPacket::CameraModelParams(Arc::new(camera_model)),
        )?;
        rt.emit(
            "distortionModel",
            DataPacket::DistortionModelParams(Arc::new(distortion_model)),
        )?;
        rt.report_state(NodeRuntimeState::Ready, "initial camera parameters emitted");
        Ok(())
    }
}

impl NodeInstance for CameraInitialParamsNode {
    fn kind(&self) -> &'static str {
        CAMERA_INITIAL_PARAMS_KIND
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.params()?;
        rt.report_state(
            NodeRuntimeState::Ready,
            "trigger to emit initial camera parameters",
        );
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
            NodeAction::Trigger => self.emit_params(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_config_update(
        &mut self,
        config: serde_json::Value,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let candidate = Self {
            spec: NodeSpec {
                config,
                ..self.spec.clone()
            },
        };
        candidate.params()?;
        self.spec = candidate.spec;
        Ok(())
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalibrationBoardConfig {
    #[serde(default = "default_board_kind")]
    board_kind: CalibrationBoardKind,
    #[serde(default = "default_board_cols")]
    board_cols: u16,
    #[serde(default = "default_board_rows")]
    board_rows: u16,
    #[serde(default = "default_square_size_mm")]
    square_size_mm: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CameraInitialConfig {
    #[serde(default = "default_camera_model_kind")]
    camera_model_kind: CameraModelKind,
    #[serde(default = "default_fx")]
    fx: f64,
    #[serde(default = "default_fy")]
    fy: f64,
    #[serde(default = "default_cx")]
    cx: f64,
    #[serde(default = "default_cy")]
    cy: f64,
    #[serde(default = "default_image_width")]
    image_width: u32,
    #[serde(default = "default_image_height")]
    image_height: u32,
    #[serde(default = "default_distortion_kind")]
    distortion_kind: DistortionModelKind,
}

fn parse_config<T>(spec: &NodeSpec, node_kind: &str) -> Result<T, NodeError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(spec.config.clone())
        .map_err(|error| NodeError::Config(format!("{node_kind} config is invalid: {error}")))
}

const fn default_board_kind() -> CalibrationBoardKind {
    CalibrationBoardKind::Chessboard
}

const fn default_board_cols() -> u16 {
    11
}

const fn default_board_rows() -> u16 {
    8
}

const fn default_square_size_mm() -> f64 {
    40.0
}

const fn default_camera_model_kind() -> CameraModelKind {
    CameraModelKind::Pinhole
}

const fn default_fx() -> f64 {
    900.0
}

const fn default_fy() -> f64 {
    900.0
}

const fn default_cx() -> f64 {
    960.0
}

const fn default_cy() -> f64 {
    540.0
}

const fn default_image_width() -> u32 {
    1_920
}

const fn default_image_height() -> u32 {
    1_080
}

const fn default_distortion_kind() -> DistortionModelKind {
    DistortionModelKind::None
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};

    use super::*;
    use crate::engine::{
        EngineServices, NodeReporter, OutputRegistry, PortCardinality, PortSpec, SpawnContext,
    };

    fn spec(kind: &str, outputs: &[(&str, &str)], config: serde_json::Value) -> NodeSpec {
        NodeSpec {
            id: format!("{kind}-1"),
            kind: kind.to_owned(),
            title: kind.to_owned(),
            inputs: Vec::new(),
            outputs: outputs
                .iter()
                .map(|(id, packet_kind)| PortSpec {
                    id: (*id).to_owned(),
                    label: (*id).to_owned(),
                    kind: (*packet_kind).to_owned(),
                    cardinality: PortCardinality::One,
                    required: true,
                })
                .collect(),
            config,
        }
    }

    fn runtime(recorded: Arc<Mutex<Vec<DataPacket>>>) -> NodeRuntime {
        let (state_tx, _state_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("parameter-node-test".to_owned(), state_tx, event_tx);
        let mut outputs = OutputRegistry::default();
        outputs.set_record(Arc::new(move |packet| {
            recorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(packet);
        }));
        NodeRuntime::new(SpawnContext {
            outputs,
            reporter,
            services: Arc::new(EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        })
    }

    #[test]
    fn board_node_emits_valid_default_packet_on_trigger() {
        let mut node = CalibrationBoardParamsNode {
            spec: spec(
                CALIBRATION_BOARD_PARAMS_KIND,
                &[("board", "calib.board.params")],
                serde_json::json!({}),
            ),
        };
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&recorded));

        node.on_start(&mut runtime)
            .expect("default config is valid");
        node.on_action(NodeAction::Trigger, &mut runtime)
            .expect("trigger emits board params");

        let recorded = recorded.lock().expect("record lock");
        let [DataPacket::CalibrationBoardParams(params)] = recorded.as_slice() else {
            panic!("expected a board parameters packet");
        };
        assert_eq!(params.board_kind, CalibrationBoardKind::Chessboard);
        assert_eq!(params.cols, 11);
        assert_eq!(params.rows, 8);
        assert_eq!(params.square_size_mm, 40.0);
    }

    #[test]
    fn camera_node_emits_pinhole_and_none_distortion_defaults_on_trigger() {
        let mut node = CameraInitialParamsNode {
            spec: spec(
                CAMERA_INITIAL_PARAMS_KIND,
                &[
                    ("cameraModel", "calib.camera.model"),
                    ("distortionModel", "calib.distortion.model"),
                ],
                serde_json::json!({}),
            ),
        };
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&recorded));

        node.on_start(&mut runtime)
            .expect("default config is valid");
        node.on_action(NodeAction::Trigger, &mut runtime)
            .expect("trigger emits camera parameters");

        let recorded = recorded.lock().expect("record lock");
        let [
            DataPacket::CameraModelParams(camera),
            DataPacket::DistortionModelParams(distortion),
        ] = recorded.as_slice()
        else {
            panic!("expected camera and distortion parameter packets");
        };
        assert_eq!(camera.model, CameraModelKind::Pinhole);
        assert_eq!(
            (camera.fx, camera.fy, camera.cx, camera.cy),
            (900.0, 900.0, 960.0, 540.0)
        );
        assert_eq!(
            camera.image_size,
            Some(CalibrationImageSize::new(1_920, 1_080).expect("valid default size"))
        );
        assert_eq!(distortion.model, DistortionModelKind::None);
        assert!(distortion.coefficients.is_empty());
    }

    #[test]
    fn invalid_config_update_keeps_last_valid_board_config() {
        let mut node = CalibrationBoardParamsNode {
            spec: spec(
                CALIBRATION_BOARD_PARAMS_KIND,
                &[("board", "calib.board.params")],
                serde_json::json!({"boardCols": 11}),
            ),
        };
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&recorded));

        let error = node.on_config_update(serde_json::json!({"boardCols": 1}), &mut runtime);
        assert!(matches!(error, Err(NodeError::Config(_))));
        node.on_action(NodeAction::Trigger, &mut runtime)
            .expect("old valid config remains active");

        let recorded = recorded.lock().expect("record lock");
        let [DataPacket::CalibrationBoardParams(params)] = recorded.as_slice() else {
            panic!("expected a board parameters packet");
        };
        assert_eq!(params.cols, 11);
    }
}
