//! 棋盘格检测节点：视频/图像帧 → 棋盘角点与 overlay（转换节点范式）。
//!
//! `on_input` 收到 `VideoFrame`/`ImageFrame` 后，先把 RGBA 帧经 `RasterImageCodec::encode_png`
//! 编码为 PNG bytes，再交给 `CalibrationBackend::detect_png` 检测棋盘角点：
//! - `Found` → 输出 `calib.detection`（强类型 `Detection`）+ `overlay`（JSON）。
//! - `NotFound` → 只上报事件/状态，不输出 detection 或 overlay。
//!
//! 未注入 `image_codec` 或 `calibration` 时按前置条件失败（`NodeError::Precondition`），不 panic。

use std::sync::Arc;

use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, ChessboardDetection, ChessboardDetectionOutcome, Rgba8Frame,
};
use serde_json::json;

use crate::{
    engine::{
        DataPacket, DetectionPacket, ImageFrame, ImageFrameFormat, ImageFrameIdentity, NodeAction,
        NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec, PortSpec,
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
            return Ok(());
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
            return Ok(());
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
                let detection = Arc::new(detection);
                rt.emit(
                    "detection",
                    DataPacket::Detection(Arc::new(DetectionPacket {
                        detection: Arc::clone(&detection),
                        frame_identity: frame.identity.clone(),
                    })),
                )?;
                self.emit_found_overlay(rt, frame, detection.as_ref(), board)?;
                rt.report_state(NodeRuntimeState::Idle, "detected");
                Ok(())
            }
            ChessboardDetectionOutcome::NotFound { image_size } => {
                rt.report_event(format!("chessboard not found in {image_size:?} frame"));
                rt.report_state(NodeRuntimeState::Idle, "not found");
                Ok(())
            }
        }
    }

    /// 输出与原始图像像素坐标绑定的棋盘 overlay；只有检测到棋盘时才产生可视层。
    fn emit_found_overlay(
        &self,
        rt: &NodeRuntime,
        frame: &ImageFrame,
        detection: &ChessboardDetection,
        board: BoardSpec,
    ) -> Result<(), NodeError> {
        self.emit_overlay_payload(
            rt,
            json!({
                "kind": "calib.chessboard.overlay.v1",
                "schema": "viewer.layer.overlay.v1",
                "found": true,
                "status": "found",
                "coordinateSpace": "image_pixel",
                "frameSequence": frame.identity.frame_sequence,
                "frameIdentity": frame_identity_overlay(&frame.identity),
                "imageSize": {"width": detection.image_size.width, "height": detection.image_size.height},
                "board": {
                    "cols": board.inner_cols,
                    "rows": board.inner_rows,
                    "squareSizeMm": board.square_size,
                },
                "corners": detection.corners.iter().map(|corner| json!({"x": corner.x, "y": corner.y})).collect::<Vec<_>>(),
                "outline": chessboard_outline(detection, board),
            }),
        )
    }

    fn emit_overlay_payload(
        &self,
        rt: &NodeRuntime,
        overlay: serde_json::Value,
    ) -> Result<(), NodeError> {
        if has_output_port(&self.spec, "overlay") {
            rt.emit("overlay", DataPacket::Json(Arc::new(overlay)))?;
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

fn frame_identity_overlay(identity: &ImageFrameIdentity) -> serde_json::Value {
    json!({
        "frameSequence": identity.frame_sequence,
        "hostMonotonicTimeNs": identity.host_monotonic_time_ns,
        "deviceTimestampNs": identity.device_timestamp_ns,
    })
}

fn chessboard_outline(detection: &ChessboardDetection, board: BoardSpec) -> serde_json::Value {
    let cols = usize::from(board.inner_cols);
    let rows = usize::from(board.inner_rows);
    if cols == 0 || rows == 0 || detection.corners.len() < cols.saturating_mul(rows) {
        return serde_json::Value::Null;
    }
    let indexes = [
        0,
        cols - 1,
        rows.saturating_sub(1) * cols + cols - 1,
        rows.saturating_sub(1) * cols,
    ];
    json!(
        indexes
            .into_iter()
            .filter_map(|index| detection.corners.get(index))
            .map(|corner| json!({"x": corner.x, "y": corner.y}))
            .collect::<Vec<_>>()
    )
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
        EngineServices, FrameProvenance, ImageFrameIdentity, ImagePlane, NodeReporter,
        OutputRegistry, SpawnContext,
    };
    use crate::platform::StreamSessionId;
    use camera_toolbox_core::CalibrationPoint;

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
        let identity = ImageFrameIdentity {
            provenance: FrameProvenance::Stream {
                stream_id: StreamSessionId::new("detector-test").expect("session id"),
                channel: 0,
            },
            frame_sequence: 0,
            source_pts: crate::platform::SourcePts::Unavailable {
                reason: "test".to_owned(),
            },
            host_monotonic_time_ns: 123,
            device_timestamp_ns: None,
        };
        if width == 0 || height == 0 {
            return Arc::new(ImageFrame {
                width,
                height,
                format: ImageFrameFormat::Rgba8,
                planes: Vec::new(),
                identity,
                color: None,
                raw: None,
            });
        }
        let byte_len = width.saturating_mul(height).saturating_mul(4) as usize;
        Arc::new(
            ImageFrame::new(
                width,
                height,
                ImageFrameFormat::Rgba8,
                vec![ImagePlane::new(
                    Arc::from(vec![0u8; byte_len]),
                    width.saturating_mul(4),
                )],
                identity,
                None,
                None,
            )
            .expect("valid frame"),
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
    fn overlay_output_records_chessboard_drawing_payload() {
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
        let frame = frame(640, 480);
        let board = BoardSpec::new(2, 2, 30.0).expect("board");
        let detection = ChessboardDetection {
            image_size: CalibrationImageSize::new(640, 480).expect("image size"),
            corners: vec![
                CalibrationPoint::new(10.0, 20.0),
                CalibrationPoint::new(30.0, 20.0),
                CalibrationPoint::new(30.0, 40.0),
                CalibrationPoint::new(10.0, 40.0),
            ],
        };

        node.emit_found_overlay(&rt, frame.as_ref(), &detection, board)
            .expect("overlay emits");

        let guard = record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(guard.len(), 1);
        let DataPacket::Json(value) = &guard[0] else {
            panic!("overlay must be JSON");
        };
        assert_eq!(value["kind"], "calib.chessboard.overlay.v1");
        assert_eq!(value["schema"], "viewer.layer.overlay.v1");
        assert_eq!(value["found"], true);
        assert_eq!(value["status"], "found");
        assert_eq!(value["frameSequence"], 0);
        assert_eq!(value["imageSize"], json!({"width": 640, "height": 480}));
        assert_eq!(value["corners"].as_array().expect("corners").len(), 4);
        assert_eq!(value["outline"].as_array().expect("outline").len(), 4);
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
}
