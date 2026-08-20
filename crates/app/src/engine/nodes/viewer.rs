//! Viewer 节点：终端节点，把收到的视频帧发布到引擎预分配的帧出口。
//!
//! 用「最后收帧时间戳 + 活性检测」实现数据流状态统一：上游断开后超时回落 idle。

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use crate::{
    engine::{
        DataPacket, FrameProvenance, ImageFrame, ImageFrameFormat, ImagePlane, NodeAction,
        NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec,
    },
    platform::{
        DecodedVideoFrame, SourcePts, StreamFrameIdentity, StreamSessionId, host_monotonic_time_ns,
    },
};

/// 帧超时阈值：超过此时间无新帧视为上游已停止。
const FRAME_STALL_TIMEOUT_NS: u64 = 1_000_000_000;

pub struct ViewerFactory;

impl NodeFactory for ViewerFactory {
    fn kind(&self) -> &'static str {
        crate::engine::node::kinds::VIEWER
    }

    fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(ViewerNode {
            last_frame_at: Arc::new(AtomicU64::new(0)),
            latest_image: Mutex::new(None),
        }))
    }
}

pub struct ViewerNode {
    /// 最近一次收帧的进程单调时间戳；0 表示「未收到帧」或「已回落 idle」。
    last_frame_at: Arc<AtomicU64>,
    /// 最后一次成功预览的原始图像；Trigger 必须原样输出它，不能输出显示转换后的 RGBA。
    latest_image: Mutex<Option<Arc<ImageFrame>>>,
}

impl NodeInstance for ViewerNode {
    fn kind(&self) -> &'static str {
        crate::engine::node::kinds::VIEWER
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for frames");
        let last_frame_at = Arc::clone(&self.last_frame_at);
        let reporter = rt.context().reporter.clone();
        let cancel = Arc::clone(&rt.context().cancel);
        rt.spawn("viewer-liveness", move |_ctx| {
            liveness_loop(last_frame_at, reporter, cancel);
        });
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if let ("overlay", DataPacket::Json(overlay)) = (port, &packet) {
            let kind = overlay
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("overlay");
            rt.report_event(format!("viewer overlay updated: {kind}"));
            return Ok(());
        }
        let frame = match packet {
            DataPacket::VideoFrame(frame) => {
                *self
                    .latest_image
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(Arc::new(ImageFrame::from(frame.as_ref())));
                frame
            }
            DataPacket::ImageFrame(image) => {
                let frame = match preview_image_frame(&image) {
                    Ok(frame) => frame,
                    Err(diagnostic) => {
                        rt.report_event(diagnostic.to_owned());
                        return Ok(());
                    }
                };
                // 显示用 RGBA 缓冲仅进入 viewer slot；图载荷本身仍以原格式、原身份保存。
                *self
                    .latest_image
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(image);
                Arc::new(frame)
            }
            _ => return Ok(()),
        };
        if let Some(slot) = rt.context().viewer_slot.as_ref() {
            // 帧数据零拷贝转移进槽位；仅当唯一引用时直接取出，否则克隆。
            slot.publish(Arc::unwrap_or_clone(frame));
        }
        let now = host_monotonic_time_ns();
        // 从 0（未收帧/已回落）变为非 0 时上报 running。
        if self.last_frame_at.swap(now, Ordering::Relaxed) == 0 {
            rt.report_state(NodeRuntimeState::Running, "receiving frames");
        }
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            // 捕获原始 `ImageFrame`，绝不把 Gray/NV12 的显示转换结果伪装成图载荷。
            NodeAction::Trigger => {
                let image = self
                    .latest_image
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                    .ok_or_else(|| {
                        NodeError::Precondition("viewer has no frame to capture".to_owned())
                    })?;
                let sequence = image.identity.frame_sequence;
                rt.emit("image", DataPacket::ImageFrame(image))?;
                rt.report_event(format!("captured latest frame (sequence {sequence})"));
                Ok(())
            }
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.latest_image
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 将统一图载荷转换为仅供浏览器 JPEG 出口使用的 RGBA 诊断图像。
///
/// 该转换不回写图载荷。Bayer RAW 缺少明确 demosaic 策略，必须经 Demosaic 节点处理。
fn preview_image_frame(image: &ImageFrame) -> Result<DecodedVideoFrame, &'static str> {
    if image.format == ImageFrameFormat::BayerRaw {
        return Err("viewer cannot preview BAYER_RAW; BayerRaw requires explicit Demosaic");
    }

    let width = usize::try_from(image.width).map_err(|_| "viewer image width overflows host")?;
    let height = usize::try_from(image.height).map_err(|_| "viewer image height overflows host")?;
    let pixels = width
        .checked_mul(height)
        .ok_or("viewer image dimensions overflow host")?;
    let rgba = match image.format {
        ImageFrameFormat::Rgba8 => compact_rgba(image, width, height)?,
        ImageFrameFormat::Gray8 => gray8_rgba(image, width, height)?,
        ImageFrameFormat::Gray16Le => gray16le_rgba(image, width, height)?,
        ImageFrameFormat::Nv12 => nv12_rgba(image, width, height)?,
        ImageFrameFormat::BayerRaw => unreachable!("BayerRaw is rejected above"),
    };
    let expected_len = pixels
        .checked_mul(4)
        .ok_or("viewer RGBA size overflows host")?;
    if rgba.len() < expected_len {
        return Err("viewer produced an invalid RGBA preview extent");
    }
    Ok(DecodedVideoFrame {
        width: image.width,
        height: image.height,
        rgba,
        identity: display_identity(image),
    })
}

/// 原始流身份可直接复用；设备/文件帧只为显示出口创建临时 stream 标识，绝不进入图载荷。
fn display_identity(image: &ImageFrame) -> StreamFrameIdentity {
    image.identity.stream_identity().unwrap_or_else(|| {
        let channel = match &image.identity.provenance {
            FrameProvenance::Device { channel, .. } | FrameProvenance::Stream { channel, .. } => {
                *channel
            }
            FrameProvenance::File { .. } | FrameProvenance::Unknown { .. } => 0,
        };
        StreamFrameIdentity {
            stream_id: StreamSessionId::new("viewer-diagnostic")
                .expect("static viewer diagnostic stream id is valid"),
            channel,
            frame_sequence: image.identity.frame_sequence,
            source_pts: SourcePts::Unavailable {
                reason: "display-only identity for non-stream ImageFrame".to_owned(),
            },
            host_monotonic_time_ns: image.identity.host_monotonic_time_ns,
            device_timestamp_ns: image.identity.device_timestamp_ns(),
        }
    })
}

fn plane(image: &ImageFrame, index: usize) -> Result<&ImagePlane, &'static str> {
    image
        .planes
        .get(index)
        .ok_or("viewer image is missing a required plane")
}

fn row(plane: &ImagePlane, y: usize, width: usize) -> Result<&[u8], &'static str> {
    let start = usize::try_from(plane.stride_bytes)
        .map_err(|_| "viewer image stride overflows host")?
        .checked_mul(y)
        .ok_or("viewer image row offset overflows host")?;
    let end = start
        .checked_add(width)
        .ok_or("viewer image row extent overflows host")?;
    plane
        .bytes
        .get(start..end)
        .ok_or("viewer image plane is shorter than its declared stride")
}

fn compact_rgba(
    image: &ImageFrame,
    width: usize,
    height: usize,
) -> Result<Arc<[u8]>, &'static str> {
    let row_bytes = width
        .checked_mul(4)
        .ok_or("viewer RGBA row overflows host")?;
    let source = plane(image, 0)?;
    if usize::try_from(source.stride_bytes).map_err(|_| "viewer image stride overflows host")?
        == row_bytes
    {
        let extent = row_bytes
            .checked_mul(height)
            .ok_or("viewer RGBA extent overflows host")?;
        if source.bytes.len() < extent {
            return Err("viewer image plane is shorter than its declared stride");
        }
        return Ok(Arc::clone(&source.bytes));
    }
    let mut output = vec![
        0;
        row_bytes
            .checked_mul(height)
            .ok_or("viewer RGBA extent overflows host")?
    ];
    for y in 0..height {
        output[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(row(source, y, row_bytes)?);
    }
    Ok(Arc::from(output))
}

fn gray8_rgba(image: &ImageFrame, width: usize, height: usize) -> Result<Arc<[u8]>, &'static str> {
    let source = plane(image, 0)?;
    let mut output = vec![
        0;
        width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .ok_or("viewer GRAY8 extent overflows host")?
    ];
    for y in 0..height {
        for (x, &value) in row(source, y, width)?.iter().enumerate() {
            let offset = (y * width + x) * 4;
            output[offset..offset + 3].fill(value);
            output[offset + 3] = u8::MAX;
        }
    }
    Ok(Arc::from(output))
}

fn gray16le_rgba(
    image: &ImageFrame,
    width: usize,
    height: usize,
) -> Result<Arc<[u8]>, &'static str> {
    let source = plane(image, 0)?;
    let row_bytes = width
        .checked_mul(2)
        .ok_or("viewer GRAY16LE row overflows host")?;
    let mut output = vec![
        0;
        width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .ok_or("viewer GRAY16LE extent overflows host")?
    ];
    for y in 0..height {
        for (x, sample) in row(source, y, row_bytes)?.chunks_exact(2).enumerate() {
            let value = sample[1]; // 16-bit little endian sample缩放到 8-bit display luma。
            let offset = (y * width + x) * 4;
            output[offset..offset + 3].fill(value);
            output[offset + 3] = u8::MAX;
        }
    }
    Ok(Arc::from(output))
}

fn nv12_rgba(image: &ImageFrame, width: usize, height: usize) -> Result<Arc<[u8]>, &'static str> {
    let luma = plane(image, 0)?;
    let chroma = plane(image, 1)?;
    let mut output = vec![
        0;
        width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .ok_or("viewer NV12 extent overflows host")?
    ];
    for y in 0..height {
        let y_row = row(luma, y, width)?;
        let uv_row = row(chroma, y / 2, width)?;
        for x in 0..width {
            let uv = (x / 2) * 2;
            let rgba = yuv_to_rgba(y_row[x], uv_row[uv], uv_row[uv + 1], image);
            let offset = (y * width + x) * 4;
            output[offset..offset + 4].copy_from_slice(&rgba);
        }
    }
    Ok(Arc::from(output))
}

/// 仅用于 NV12 诊断预览；缺失色彩元数据时采用 BT.601 limited-range 显示假设。
fn yuv_to_rgba(y: u8, u: u8, v: u8, image: &ImageFrame) -> [u8; 4] {
    let full_range = image
        .color
        .as_ref()
        .and_then(|color| color.full_range)
        .unwrap_or(false);
    let (red, green_u, green_v, blue) = match image.color.as_ref().map(|color| color.color_space) {
        Some(crate::engine::ColorSpace::Bt709) => (1.5748, 0.1873, 0.4681, 1.8556),
        Some(crate::engine::ColorSpace::Bt2020) => (1.4746, 0.1646, 0.5714, 1.8814),
        Some(crate::engine::ColorSpace::Bt601 | crate::engine::ColorSpace::Srgb) | None => {
            (1.402, 0.3441, 0.7141, 1.772)
        }
    };
    let luma = if full_range {
        f32::from(y)
    } else {
        (f32::from(y) - 16.0) * 1.164_383
    };
    let u = f32::from(u) - 128.0;
    let v = f32::from(v) - 128.0;
    [
        (luma + red * v).round().clamp(0.0, 255.0) as u8,
        (luma - green_u * u - green_v * v).round().clamp(0.0, 255.0) as u8,
        (luma + blue * u).round().clamp(0.0, 255.0) as u8,
        u8::MAX,
    ]
}

/// 活性检测：超过阈值无新帧时回落到 idle（抑制重复上报）。
fn liveness_loop(
    last_frame_at: Arc<AtomicU64>,
    reporter: crate::engine::NodeReporter,
    cancel: Arc<AtomicBool>,
) {
    while !cancel.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(500));
        let last = last_frame_at.load(Ordering::Relaxed);
        if last == 0 {
            continue;
        }
        let now = host_monotonic_time_ns();
        if now.saturating_sub(last) > FRAME_STALL_TIMEOUT_NS
            && last_frame_at.swap(0, Ordering::Relaxed) != 0
        {
            reporter.report_state(NodeRuntimeState::Idle, "no frames (upstream stopped)");
        }
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::sync::{Arc, atomic::AtomicBool, mpsc};

    use super::*;
    use crate::engine::{
        BayerPattern, FrameProvenance, ImageFrameIdentity, ImagePlane, NodeReporter,
        OutputRegistry, RawMetadata, SpawnContext,
    };
    use crate::platform::{
        DecodedVideoFrame, LatestDecodedFrameSlot, StreamFrameIdentity, StreamSessionId,
    };

    fn video_frame() -> DataPacket {
        let session = StreamSessionId::new("viewer-test").expect("session id");
        DataPacket::VideoFrame(Arc::new(DecodedVideoFrame {
            width: 1,
            height: 1,
            rgba: Arc::from(vec![0u8; 4]),
            identity: StreamFrameIdentity::unavailable(session, 0, 1, "test"),
        }))
    }

    fn runtime_with_slot(
        slot: Arc<LatestDecodedFrameSlot>,
        state_tx: mpsc::Sender<crate::engine::NodeStatusReport>,
    ) -> NodeRuntime {
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("viewer-1".to_owned(), state_tx, event_tx);
        let ctx = SpawnContext {
            outputs: OutputRegistry::default(),
            reporter,
            services: Arc::new(crate::engine::EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: Some(slot),
        };
        NodeRuntime::new(ctx)
    }

    fn runtime_with_slot_and_events(
        slot: Arc<LatestDecodedFrameSlot>,
        state_tx: mpsc::Sender<crate::engine::NodeStatusReport>,
        event_tx: mpsc::Sender<crate::engine::NodeEvent>,
    ) -> NodeRuntime {
        let reporter = NodeReporter::new("viewer-1".to_owned(), state_tx, event_tx);
        let ctx = SpawnContext {
            outputs: OutputRegistry::default(),
            reporter,
            services: Arc::new(crate::engine::EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: Some(slot),
        };
        NodeRuntime::new(ctx)
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

    #[test]
    fn factory_instantiates_with_expected_kind() {
        assert_eq!(ViewerFactory.kind(), "viewer");
        let spec = crate::engine::NodeSpec {
            id: "viewer-1".to_owned(),
            kind: "viewer".to_owned(),
            title: "Viewer".to_owned(),
            inputs: vec![],
            outputs: vec![],
            config: serde_json::json!({}),
        };
        let instance = ViewerFactory.instantiate(spec).expect("instantiate");
        assert_eq!(instance.kind(), "viewer");
    }

    #[test]
    fn on_start_reports_ready() {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime_with_slot(slot, state_tx);
        let mut node = ViewerFactory
            .instantiate(crate::engine::NodeSpec {
                id: "viewer-1".to_owned(),
                kind: "viewer".to_owned(),
                title: "Viewer".to_owned(),
                inputs: vec![],
                outputs: vec![],
                config: serde_json::json!({}),
            })
            .expect("instantiate");

        node.on_start(&mut rt).expect("on_start");
        rt.stop_background(); // 关闭 liveness 线程，避免测试泄漏
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Ready));
    }

    #[test]
    fn on_input_publishes_to_slot_and_reports_running_once() {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime_with_slot(Arc::clone(&slot), state_tx);
        let mut node = ViewerFactory
            .instantiate(crate::engine::NodeSpec {
                id: "viewer-1".to_owned(),
                kind: "viewer".to_owned(),
                title: "Viewer".to_owned(),
                inputs: vec![],
                outputs: vec![],
                config: serde_json::json!({}),
            })
            .expect("instantiate");

        node.on_input("video", video_frame(), &mut rt)
            .expect("on_input");
        assert!(
            slot.latest().is_some(),
            "frame should be published to viewer slot"
        );
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));

        // 第二帧不重复上报 running（状态仍是 running，但 try_recv 之后应无新报告）
        node.on_input("video", video_frame(), &mut rt)
            .expect("on_input");
        let mut extra = None;
        while let Ok(report) = state_rx.try_recv() {
            extra = Some(report.state);
        }
        assert_eq!(
            extra, None,
            "second frame must not emit a fresh running report"
        );
    }

    #[test]
    fn preview_image_frame_supports_rgba_gray_and_nv12() {
        let DataPacket::VideoFrame(source) = video_frame() else {
            panic!("test fixture must be a video frame");
        };
        let identity = ImageFrameIdentity::from(&source.identity);
        let rgba = ImageFrame::new(
            2,
            2,
            ImageFrameFormat::Rgba8,
            vec![ImagePlane::new(
                Arc::from(vec![1, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255]),
                8,
            )],
            identity.clone(),
            None,
            None,
        )
        .expect("valid RGBA8 fixture");
        let gray8 = ImageFrame::new(
            2,
            2,
            ImageFrameFormat::Gray8,
            vec![ImagePlane::new(Arc::from(vec![16, 32, 64, 128]), 2)],
            identity.clone(),
            None,
            None,
        )
        .expect("valid GRAY8 fixture");
        let gray16 = ImageFrame::new(
            2,
            2,
            ImageFrameFormat::Gray16Le,
            vec![ImagePlane::new(
                Arc::from(vec![0, 16, 0, 32, 0, 64, 0, 128]),
                4,
            )],
            identity.clone(),
            None,
            None,
        )
        .expect("valid GRAY16LE fixture");
        let nv12 = ImageFrame::new(
            2,
            2,
            ImageFrameFormat::Nv12,
            vec![
                ImagePlane::new(Arc::from(vec![235; 4]), 2),
                ImagePlane::new(Arc::from(vec![128; 2]), 2),
            ],
            identity,
            None,
            None,
        )
        .expect("valid NV12 fixture");

        assert_eq!(
            preview_image_frame(&rgba)
                .expect("RGBA preview")
                .rgba
                .as_ref(),
            &[1, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255],
        );
        assert_eq!(
            preview_image_frame(&gray8)
                .expect("GRAY8 preview")
                .rgba
                .len(),
            16
        );
        assert_eq!(
            preview_image_frame(&gray16).expect("GRAY16LE preview").rgba[3],
            255
        );
        assert_eq!(
            preview_image_frame(&nv12)
                .expect("NV12 preview")
                .rgba
                .as_ref(),
            &[255; 16],
        );
    }

    #[test]
    fn viewer_reports_bayer_raw_requires_explicit_demosaic() {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let (state_tx, _state_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let mut rt = runtime_with_slot_and_events(Arc::clone(&slot), state_tx, event_tx);
        let DataPacket::VideoFrame(source) = video_frame() else {
            panic!("test fixture must be a video frame");
        };
        let raw = Arc::new(
            ImageFrame::new(
                2,
                2,
                ImageFrameFormat::BayerRaw,
                vec![ImagePlane::new(Arc::from(vec![0; 8]), 4)],
                ImageFrameIdentity::from(&source.identity),
                None,
                Some(RawMetadata {
                    bayer_pattern: BayerPattern::Rggb,
                    bits_per_sample: 12,
                    black_level: None,
                    white_level: None,
                }),
            )
            .expect("valid BayerRaw fixture"),
        );
        let mut node = ViewerFactory
            .instantiate(crate::engine::NodeSpec {
                id: "viewer-1".to_owned(),
                kind: "viewer".to_owned(),
                title: "Viewer".to_owned(),
                inputs: vec![],
                outputs: vec![],
                config: serde_json::json!({}),
            })
            .expect("instantiate");

        node.on_input("image", DataPacket::ImageFrame(raw), &mut rt)
            .expect("viewer reports unsupported RAW without faulting graph");
        assert!(slot.latest().is_none());
        assert!(
            event_rx
                .try_recv()
                .expect("BayerRaw diagnostic")
                .message
                .contains("explicit Demosaic")
        );
    }

    #[test]
    fn trigger_emits_latest_frame_as_image_frame() {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let mut outputs = OutputRegistry::default();
        let record = Arc::clone(&emitted);
        outputs.set_record(Arc::new(move |packet| record.lock().push(packet)));
        let (state_tx, _state_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let ctx = SpawnContext {
            outputs,
            reporter: NodeReporter::new("viewer-1".to_owned(), state_tx, event_tx),
            services: Arc::new(crate::engine::EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: Some(Arc::clone(&slot)),
        };
        let mut rt = NodeRuntime::new(ctx);
        let mut node = ViewerFactory
            .instantiate(crate::engine::NodeSpec {
                id: "viewer-1".to_owned(),
                kind: "viewer".to_owned(),
                title: "Viewer".to_owned(),
                inputs: vec![],
                outputs: vec![],
                config: serde_json::json!({}),
            })
            .expect("instantiate");

        let image = Arc::new(
            ImageFrame::new(
                2,
                2,
                ImageFrameFormat::Gray16Le,
                vec![ImagePlane::new(
                    Arc::from(vec![0, 16, 0, 32, 0, 64, 0, 128]),
                    4,
                )],
                ImageFrameIdentity {
                    provenance: FrameProvenance::Device {
                        driver: "X5_233".to_owned(),
                        channel: 3,
                        camera: Some(0),
                        timestamp_ns: 987_654,
                    },
                    frame_sequence: 42,
                    source_pts: SourcePts::Unavailable {
                        reason: "device snapshot has no RTSP PTS".to_owned(),
                    },
                    host_monotonic_time_ns: 123,
                    device_timestamp_ns: Some(987_654),
                },
                None,
                None,
            )
            .expect("valid device GRAY16LE fixture"),
        );
        node.on_input("image", DataPacket::ImageFrame(image), &mut rt)
            .expect("image frame accepted");
        node.on_action(NodeAction::Trigger, &mut rt)
            .expect("capture current frame");
        let captured = emitted.lock();

        assert_eq!(captured.len(), 1);
        let DataPacket::ImageFrame(frame) = &captured[0] else {
            panic!("viewer capture must emit image.frame");
        };
        assert_eq!(frame.format, ImageFrameFormat::Gray16Le);
        assert_eq!(frame.identity.frame_sequence, 42);
        assert!(matches!(
            frame.identity.provenance,
            FrameProvenance::Device {
                channel: 3,
                camera: Some(0),
                ..
            }
        ));
    }

    #[test]
    fn on_input_ignores_non_video_packets() {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime_with_slot(Arc::clone(&slot), state_tx);
        let mut node = ViewerFactory
            .instantiate(crate::engine::NodeSpec {
                id: "viewer-1".to_owned(),
                kind: "viewer".to_owned(),
                title: "Viewer".to_owned(),
                inputs: vec![],
                outputs: vec![],
                config: serde_json::json!({}),
            })
            .expect("instantiate");

        node.on_input(
            "video",
            DataPacket::Json(Arc::new(serde_json::json!({}))),
            &mut rt,
        )
        .expect("on_input");
        assert!(slot.latest().is_none(), "non-video packet must not publish");
        assert_eq!(last_state(&state_rx), None);
    }

    #[test]
    fn overlay_input_is_absorbed_by_viewer_without_publishing_frame() {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let (state_tx, state_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let mut rt = runtime_with_slot_and_events(Arc::clone(&slot), state_tx, event_tx);
        let mut node = ViewerFactory
            .instantiate(crate::engine::NodeSpec {
                id: "viewer-1".to_owned(),
                kind: "viewer".to_owned(),
                title: "Viewer".to_owned(),
                inputs: vec![],
                outputs: vec![],
                config: serde_json::json!({}),
            })
            .expect("instantiate");

        node.on_input(
            "overlay",
            DataPacket::Json(Arc::new(serde_json::json!({"kind": "overlay"}))),
            &mut rt,
        )
        .expect("overlay input");

        assert!(
            slot.latest().is_none(),
            "overlay must not replace current frame"
        );
        assert_eq!(last_state(&state_rx), None);
        let event = event_rx.try_recv().expect("overlay event");
        assert!(event.message.contains("viewer overlay updated"));
    }

    #[test]
    fn trigger_without_frame_is_rejected_and_other_actions_stay_unsupported() {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime_with_slot(slot, state_tx);
        let mut node = ViewerFactory
            .instantiate(crate::engine::NodeSpec {
                id: "viewer-1".to_owned(),
                kind: "viewer".to_owned(),
                title: "Viewer".to_owned(),
                inputs: vec![],
                outputs: vec![],
                config: serde_json::json!({}),
            })
            .expect("instantiate");

        node.on_stop(&mut rt).expect("on_stop");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Idle));

        let err = node
            .on_action(NodeAction::Trigger, &mut rt)
            .expect_err("no frame");
        assert!(matches!(err, NodeError::Precondition(_)));
        let err = node
            .on_action(NodeAction::Connect, &mut rt)
            .expect_err("unsupported");
        assert!(matches!(err, NodeError::UnsupportedAction(_)));
    }
}
