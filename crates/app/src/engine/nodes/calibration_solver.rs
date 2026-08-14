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
