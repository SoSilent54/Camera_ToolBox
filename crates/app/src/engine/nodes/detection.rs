//! 棋盘格检测节点：视频/图像帧 → 棋盘角点与 overlay（转换节点范式）。
//!
//! `on_input` 收到 `VideoFrame`/`ImageFrame` 后，先把 RGBA 帧经 `RasterImageCodec::encode_png`
//! 编码为 PNG bytes，再交给 `CalibrationBackend::detect_png` 检测棋盘角点：
//! - `Found` → 输出 `calib.detection`（强类型 `Detection`）+ `overlay`（JSON）。
//! - `NotFound` → 上报事件并输出 `overlay`（`found:false`），不输出 detection。
//!
//! 未注入 `image_codec` 或 `calibration` 时按前置条件失败（`NodeError::Precondition`），不 panic。

use std::sync::Arc;

use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, ChessboardDetectionOutcome, Rgba8Frame,
};

use crate::{
    engine::{
        DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime,
        NodeRuntimeState, NodeSpec, PortSpec,
    },
    ports::CalibrationCancellation,
};

/// 单帧解码峰值字节预算：OpenCV BGR + Gray 同时存活，每像素 4 bytes（与
/// `adapters/src/calibration.rs::ensure_decoded_budget` 的峰值口径一致）。
const DECODED_BYTES_PER_PIXEL: u64 = 4;

pub struct ChessboardDetectorFactory;

impl NodeFactory for ChessboardDetectorFactory {
    fn kind(&self) -> &'static str {
        "chessboardDetector"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(ChessboardDetectorNode { spec }))
    }
}

pub struct ChessboardDetectorNode {
    spec: NodeSpec,
}

impl NodeInstance for ChessboardDetectorNode {
    fn kind(&self) -> &'static str {
        "chessboardDetector"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for frames");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        // 只处理帧类输入；detection 等旁路输入忽略。
        let (("image", DataPacket::ImageFrame(frame)) | ("frames", DataPacket::VideoFrame(frame))) =
            (port, packet)
        else {
            return Ok(());
        };
        self.detect(&frame, rt)
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl ChessboardDetectorNode {
    fn detect(
        &self,
        frame: &crate::platform::DecodedVideoFrame,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        // 空帧（宽/高/数据任一为零或长度不匹配）直接按无结果处理，避免编码或检测 panic。
        if frame.width == 0
            || frame.height == 0
            || frame.rgba.is_empty()
            || frame.rgba.len() != rgba_len(frame.width, frame.height)?
        {
            rt.report_event("skipped empty or malformed frame");
            return self.emit_overlay(rt, false);
        }

        let image_codec = rt.services().image_codec()?;
        let backend = rt.services().calibration_backend()?;

        let expected_size = CalibrationImageSize::new(frame.width, frame.height)
            .map_err(|error| NodeError::Config(error.to_string()))?;
        let decoded_byte_limit = decoded_byte_limit(expected_size)?;

        // DecodedVideoFrame 已是紧密排列 RGBA，直接构造 Rgba8Frame 再编码 PNG。
        let rgba = Rgba8Frame::tight(frame.width, frame.height, frame.rgba.clone())
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let mut png = Vec::new();
        image_codec
            .encode_png(&rgba, &mut png)
            .map_err(|error| NodeError::Execution(error.to_string()))?;

        let board = self.board()?;
        rt.report_state(NodeRuntimeState::Running, "detecting chessboard");
        let outcome = backend
            .detect_png(
                &png,
                expected_size,
                decoded_byte_limit,
                board,
                &CalibrationCancellation::default(),
            )
            .map_err(|error| NodeError::Execution(error.to_string()))?;

        match outcome {
            ChessboardDetectionOutcome::Found(detection) => {
                rt.emit("detection", DataPacket::Detection(Arc::new(detection)))?;
                self.emit_overlay(rt, true)?;
                rt.report_state(NodeRuntimeState::Idle, "detected");
                Ok(())
            }
            ChessboardDetectionOutcome::NotFound { image_size } => {
                rt.report_event(format!("chessboard not found in {image_size:?} frame"));
                self.emit_overlay(rt, false)?;
                rt.report_state(NodeRuntimeState::Idle, "not found");
                Ok(())
            }
        }
    }

    /// 输出 `found` 状态 overlay；未声明 `overlay` 输出端口时跳过（emit 对未连接端口本就 no-op）。
    fn emit_overlay(&self, rt: &NodeRuntime, found: bool) -> Result<(), NodeError> {
        if has_output_port(&self.spec, "overlay") {
            rt.emit(
                "overlay",
                DataPacket::Json(Arc::new(serde_json::json!({
                    "kind": "overlay",
                    "found": found,
                }))),
            )?;
        }
        Ok(())
    }

    fn board(&self) -> Result<BoardSpec, NodeError> {
        BoardSpec::new(
            config_u16(&self.spec, "boardCols", 8),
            config_u16(&self.spec, "boardRows", 11),
            config_f64(&self.spec, "squareSizeMm", 30.0),
        )
        .map_err(|error| NodeError::Config(error.to_string()))
    }
}

/// 单帧解码峰值字节预算：`width * height * DECODED_BYTES_PER_PIXEL`（BGR + Gray 同存）。
fn decoded_byte_limit(size: CalibrationImageSize) -> Result<usize, NodeError> {
    let required = u64::from(size.width)
        .checked_mul(u64::from(size.height))
        .and_then(|pixels| pixels.checked_mul(DECODED_BYTES_PER_PIXEL))
        .ok_or_else(|| NodeError::Execution("image dimensions overflow byte budget".to_owned()))?;
    usize::try_from(required)
        .map_err(|_| NodeError::Execution("image byte budget exceeds usize".to_owned()))
}

/// 紧密排列 RGBA 帧的字节长度：`width * height * 4`。
fn rgba_len(width: u32, height: u32) -> Result<usize, NodeError> {
    let len = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| NodeError::Execution("frame dimensions overflow".to_owned()))?;
    usize::try_from(len).map_err(|_| NodeError::Execution("frame length exceeds usize".to_owned()))
}

fn has_output_port(spec: &NodeSpec, id: &str) -> bool {
    spec.outputs.iter().any(|port: &PortSpec| port.id == id)
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
    use std::sync::{Arc, atomic::AtomicBool, mpsc};

    use super::*;
    use crate::engine::{EngineServices, NodeReporter, OutputRegistry, SpawnContext};
    use crate::platform::{DecodedVideoFrame, StreamFrameIdentity, StreamSessionId};

    fn spec() -> NodeSpec {
        NodeSpec {
            id: "detector-1".to_owned(),
            kind: "chessboardDetector".to_owned(),
            title: "Detector".to_owned(),
            inputs: vec![],
            outputs: vec![PortSpec {
                id: "overlay".to_owned(),
                label: "Overlay".to_owned(),
                kind: "viewer.layer.overlay.v1".to_owned(),
                cardinality: crate::engine::PortCardinality::One,
                required: false,
            }],
            config: serde_json::json!({}),
        }
    }

    fn runtime(services: EngineServices) -> (NodeRuntime, OutputRegistry) {
        let (status_tx, _status_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("detector-1".to_owned(), status_tx, event_tx);
        let outputs = OutputRegistry::default();
        let ctx = SpawnContext {
            outputs: outputs.clone(),
            reporter,
            services: Arc::new(services),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        (NodeRuntime::new(ctx), outputs)
    }

    fn frame(width: u32, height: u32) -> Arc<DecodedVideoFrame> {
        let len = (width as usize) * (height as usize) * 4;
        Arc::new(DecodedVideoFrame {
            width,
            height,
            rgba: vec![0u8; len].into(),
            identity: StreamFrameIdentity::unavailable(
                StreamSessionId::new("test").expect("valid session id"),
                0,
                0,
                "unavailable".to_owned(),
            ),
        })
    }

    #[test]
    fn factory_instantiates_with_expected_kind() {
        assert_eq!(ChessboardDetectorFactory.kind(), "chessboardDetector");
        let instance = ChessboardDetectorFactory
            .instantiate(spec())
            .expect("instantiate");
        assert_eq!(instance.kind(), "chessboardDetector");
    }

    #[test]
    fn missing_image_codec_is_precondition() {
        let input = spec();
        let mut node = ChessboardDetectorNode { spec: input };
        // 未注入任何服务；非空帧会先取 image_codec → Precondition。
        let (mut rt, _outputs) = runtime(EngineServices::default());
        let err = node
            .on_input("image", DataPacket::ImageFrame(frame(2, 2)), &mut rt)
            .expect_err("missing image_codec must be a precondition error");
        assert!(matches!(err, NodeError::Precondition(_)), "got {err:?}");
    }

    #[test]
    fn empty_frame_is_skipped_without_services() {
        let input = spec();
        let mut node = ChessboardDetectorNode { spec: input };
        // 空帧在取任何 service 之前即被跳过，因此无需注入服务也应成功返回。
        let (mut rt, _outputs) = runtime(EngineServices::default());
        assert!(
            node.on_input("image", DataPacket::ImageFrame(frame(0, 0)), &mut rt)
                .is_ok()
        );
    }
}
