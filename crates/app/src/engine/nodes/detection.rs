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
        DataPacket, DetectionPacket, ImageFrame, ImageFrameFormat, NodeAction, NodeError,
        NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec, PortSpec,
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
        // 兼容仍以 DecodedVideoFrame 表示的 RTSP stream；ImageFrame 已显式保留格式与身份。
        match (port, packet) {
            ("image", DataPacket::ImageFrame(frame))
            | ("frames", DataPacket::ImageFrame(frame)) => self.detect(&frame, rt),
            ("frames", DataPacket::VideoFrame(frame)) => {
                self.detect(&ImageFrame::from(frame.as_ref()), rt)
            }
            _ => Ok(()),
        }
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
    fn detect(&self, frame: &ImageFrame, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        // 空帧先按无结果处理，避免 malformed carrier 在格式分派前触发 panic 或服务依赖。
        if frame.width == 0 || frame.height == 0 {
            rt.report_event("skipped empty or malformed frame");
            return self.emit_overlay(rt, false);
        }
        // 只有 RGB/Gray 格式可以安全交给检测器；NV12 与 Bayer 必须在图上显式转换。
        let rgba_pixels = match frame.format {
            ImageFrameFormat::Rgba8 => compact_plane_rgba(frame)?,
            ImageFrameFormat::Gray8 => gray8_to_rgba(frame)?,
            ImageFrameFormat::Gray16Le => gray16_to_rgba(frame)?,
            ImageFrameFormat::Nv12 => {
                return Err(NodeError::Precondition(
                    "chessboardDetector does not accept NV12; use Image Convert/luma first"
                        .to_owned(),
                ));
            }
            ImageFrameFormat::BayerRaw => {
                return Err(NodeError::Precondition(
                    "chessboardDetector does not accept BayerRaw; use Demosaic explicitly"
                        .to_owned(),
                ));
            }
        };
        if rgba_pixels.is_empty() {
            rt.report_event("skipped empty or malformed frame");
            return self.emit_overlay(rt, false);
        }

        let image_codec = rt.services().image_codec()?;
        let backend = rt.services().calibration_backend()?;

        let expected_size = CalibrationImageSize::new(frame.width, frame.height)
            .map_err(|error| NodeError::Config(error.to_string()))?;
        let decoded_byte_limit = decoded_byte_limit(expected_size)?;

        let rgba = Rgba8Frame::tight(frame.width, frame.height, rgba_pixels)
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
                rt.emit(
                    "detection",
                    DataPacket::Detection(Arc::new(DetectionPacket {
                        detection: Arc::new(detection),
                        frame_identity: frame.identity.clone(),
                    })),
                )?;
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

/// 复制去除 RGBA 行 padding；紧密排列时只复制一次像素句柄供后续 codec 使用。
fn compact_plane_rgba(frame: &ImageFrame) -> Result<Arc<[u8]>, NodeError> {
    let plane = frame.rgba8_plane().ok_or_else(|| {
        NodeError::Precondition("RGBA8 image is missing its pixel plane".to_owned())
    })?;
    let row_bytes = usize::try_from(frame.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| NodeError::Execution("frame dimensions overflow".to_owned()))?;
    if usize::try_from(plane.stride_bytes).ok() == Some(row_bytes) {
        return Ok(Arc::clone(&plane.bytes));
    }
    compact_rows(
        plane.bytes.as_ref(),
        plane.stride_bytes,
        frame.height,
        row_bytes,
    )
}

fn gray8_to_rgba(frame: &ImageFrame) -> Result<Arc<[u8]>, NodeError> {
    let plane = frame.planes.first().ok_or_else(|| {
        NodeError::Precondition("Gray8 image is missing its pixel plane".to_owned())
    })?;
    let values = compact_rows(
        plane.bytes.as_ref(),
        plane.stride_bytes,
        frame.height,
        usize::try_from(frame.width)
            .map_err(|_| NodeError::Execution("frame width overflow".to_owned()))?,
    )?;
    let mut rgba = Vec::with_capacity(
        values
            .len()
            .checked_mul(4)
            .ok_or_else(|| NodeError::Execution("frame dimensions overflow".to_owned()))?,
    );
    for value in values.iter().copied() {
        rgba.extend_from_slice(&[value, value, value, u8::MAX]);
    }
    Ok(rgba.into())
}

fn gray16_to_rgba(frame: &ImageFrame) -> Result<Arc<[u8]>, NodeError> {
    let plane = frame.planes.first().ok_or_else(|| {
        NodeError::Precondition("Gray16Le image is missing its pixel plane".to_owned())
    })?;
    let row_bytes = usize::try_from(frame.width)
        .ok()
        .and_then(|width| width.checked_mul(2))
        .ok_or_else(|| NodeError::Execution("frame dimensions overflow".to_owned()))?;
    let values = compact_rows(
        plane.bytes.as_ref(),
        plane.stride_bytes,
        frame.height,
        row_bytes,
    )?;
    let mut rgba = Vec::with_capacity(
        values
            .len()
            .checked_mul(2)
            .ok_or_else(|| NodeError::Execution("frame dimensions overflow".to_owned()))?,
    );
    for gray16 in values.chunks_exact(2) {
        let value = gray16[1];
        rgba.extend_from_slice(&[value, value, value, u8::MAX]);
    }
    Ok(rgba.into())
}

fn compact_rows(
    bytes: &[u8],
    stride_bytes: u32,
    height: u32,
    row_bytes: usize,
) -> Result<Arc<[u8]>, NodeError> {
    let stride = usize::try_from(stride_bytes)
        .map_err(|_| NodeError::Execution("frame stride overflow".to_owned()))?;
    let total = row_bytes
        .checked_mul(
            usize::try_from(height)
                .map_err(|_| NodeError::Execution("frame height overflow".to_owned()))?,
        )
        .ok_or_else(|| NodeError::Execution("frame dimensions overflow".to_owned()))?;
    let mut compact = Vec::with_capacity(total);
    for row in 0..usize::try_from(height)
        .map_err(|_| NodeError::Execution("frame height overflow".to_owned()))?
    {
        let start = row
            .checked_mul(stride)
            .ok_or_else(|| NodeError::Execution("frame stride overflow".to_owned()))?;
        let end = start
            .checked_add(row_bytes)
            .ok_or_else(|| NodeError::Execution("frame stride overflow".to_owned()))?;
        let row = bytes
            .get(start..end)
            .ok_or_else(|| NodeError::Execution("frame plane layout is inconsistent".to_owned()))?;
        compact.extend_from_slice(row);
    }
    Ok(compact.into())
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
    use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};

    use super::*;
    use crate::engine::{
        EngineServices, ImageFrameIdentity, NodeReporter, OutputRegistry, SpawnContext,
    };
    use crate::platform::{StreamFrameIdentity, StreamSessionId};

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

    fn frame(width: u32, height: u32) -> Arc<ImageFrame> {
        let identity = StreamFrameIdentity::unavailable(
            StreamSessionId::new("test").expect("valid session id"),
            0,
            0,
            "unavailable".to_owned(),
        );
        if width == 0 || height == 0 {
            return Arc::new(ImageFrame {
                width,
                height,
                format: ImageFrameFormat::Rgba8,
                planes: Vec::new(),
                identity: ImageFrameIdentity::from(&identity),
                color: None,
                raw: None,
            });
        }
        let len = (width as usize) * (height as usize) * 4;
        Arc::new(
            ImageFrame::rgba8(
                width,
                height,
                vec![0u8; len].into(),
                ImageFrameIdentity::from(&identity),
            )
            .expect("test frame layout is valid"),
        )
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

    #[test]
    fn rtsp_frames_image_frame_triggers_detection_path() {
        let input = spec();
        let mut node = ChessboardDetectorNode { spec: input };
        let (mut rt, _outputs) = runtime(EngineServices::default());
        let err = node
            .on_input("frames", DataPacket::ImageFrame(frame(2, 2)), &mut rt)
            .expect_err(
                "RTSP frames as ImageFrame must reach detection and fail on missing services",
            );
        assert!(matches!(err, NodeError::Precondition(_)), "got {err:?}");
    }

    #[test]
    fn overlay_output_records_found_state() {
        let record: Arc<Mutex<Vec<DataPacket>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&record);
        let mut outputs = OutputRegistry::default();
        outputs.set_record(Arc::new(move |packet| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(packet);
        }));
        let (status_tx, _status_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("detector-1".to_owned(), status_tx, event_tx);
        let rt = NodeRuntime::new(SpawnContext {
            outputs,
            reporter,
            services: Arc::new(EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        });
        let node = ChessboardDetectorNode { spec: spec() };

        node.emit_overlay(&rt, true).expect("overlay emits");

        let guard = record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(guard.len(), 1);
        let DataPacket::Json(value) = &guard[0] else {
            panic!("overlay must be JSON");
        };
        assert_eq!(value["kind"], "overlay");
        assert_eq!(value["found"], true);
    }
}
