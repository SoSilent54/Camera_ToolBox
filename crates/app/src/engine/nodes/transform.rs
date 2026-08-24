//! 帧变换节点：由数据输入触发的转换节点（范式样板）。
//!
//! 引擎语义下 `rtspSource` 已把「连接 + 解码」合并，因此 `rtspDecoder` 是 pass-through；
//! `demosaic` 是显式 RAW Bayer → RGBA/Gray 转换；`frameSampler` 按时间降采样；`videoLayer`/`imageLayer` 是可见性标记的 pass-through。
//!
//! 这是「转换节点」的完整样板：`on_input` 收到上游帧 → 变换 → `emit` 到输出端口。

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use crate::{
    engine::{
        BayerPattern, DataPacket, ImageFrame, ImageFrameFormat, ImagePlane, NodeAction, NodeError,
        NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec,
    },
    platform::host_monotonic_time_ns,
};

/// 帧超时阈值：超过此时间无新帧视为上游数据流已停止。
const FRAME_STALL_TIMEOUT_NS: u64 = 1_000_000_000;

/// RTSP 解码节点：解码已在 `rtspSource` 内完成，这里原样转发视频帧。
pub struct RtspDecoderFactory;

impl NodeFactory for RtspDecoderFactory {
    fn kind(&self) -> &'static str {
        "rtspDecoder"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(PassThroughNode {
            kind: "rtspDecoder",
            output_port: output_port(&spec),
            last_frame_at: Arc::new(AtomicU64::new(0)),
        }))
    }
}

/// 视频图层节点：可见性标记 + 帧转发。
pub struct VideoLayerFactory;

impl NodeFactory for VideoLayerFactory {
    fn kind(&self) -> &'static str {
        "videoLayer"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(PassThroughNode {
            kind: "videoLayer",
            output_port: output_port(&spec),
            last_frame_at: Arc::new(AtomicU64::new(0)),
        }))
    }
}

/// 图像图层节点：可见性标记 + 帧转发。
pub struct ImageLayerFactory;

impl NodeFactory for ImageLayerFactory {
    fn kind(&self) -> &'static str {
        "imageLayer"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(PassThroughNode {
            kind: "imageLayer",
            output_port: output_port(&spec),
            last_frame_at: Arc::new(AtomicU64::new(0)),
        }))
    }
}

/// 显式 Bayer RAW 去马赛克节点；Viewer/Detector/ImageLayer 不会隐式执行该转换。
pub struct DemosaicFactory;

impl NodeFactory for DemosaicFactory {
    fn kind(&self) -> &'static str {
        "demosaic"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(DemosaicNode { spec }))
    }
}

pub struct DemosaicNode {
    spec: NodeSpec,
}

impl NodeInstance for DemosaicNode {
    fn kind(&self) -> &'static str {
        "demosaic"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for BayerRaw image");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if port != "raw" {
            return Ok(());
        }
        let DataPacket::ImageFrame(frame) = packet else {
            return Err(NodeError::Precondition(
                "demosaic.raw requires image.frame.v1 BayerRaw".to_owned(),
            ));
        };
        if frame.format != ImageFrameFormat::BayerRaw {
            return Err(NodeError::Precondition(
                "demosaic.raw requires BayerRaw input".to_owned(),
            ));
        }
        let algorithm = self
            .spec
            .config
            .get("algorithm")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("bilinear");
        if algorithm != "bilinear" {
            return Err(NodeError::Config(format!(
                "unsupported demosaic algorithm `{algorithm}`; only bilinear is implemented"
            )));
        }

        let raw = self.raw_metadata(frame.raw.as_ref())?;
        let output_format = self.output_format()?;
        let mut frame = frame.as_ref().clone();
        frame.raw = Some(raw);

        let image = match output_format {
            DemosaicOutputFormat::Rgba8 => {
                let rgba = demosaic_bayer_bilinear(&frame)?;
                ImageFrame::rgba8(frame.width, frame.height, rgba, frame.identity.clone()).map_err(
                    |error| NodeError::Execution(format!("invalid demosaic output: {error}")),
                )?
            }
            DemosaicOutputFormat::Gray8 => {
                let plane = frame.planes.first().ok_or_else(|| {
                    NodeError::Precondition("BayerRaw image is missing pixel plane".to_owned())
                })?;
                let width = usize::try_from(frame.width).map_err(|_| {
                    NodeError::Execution("BayerRaw width overflows host".to_owned())
                })?;
                let height = usize::try_from(frame.height).map_err(|_| {
                    NodeError::Execution("BayerRaw height overflows host".to_owned())
                })?;
                let raw = frame.raw.as_ref().ok_or_else(|| {
                    NodeError::Precondition("BayerRaw image is missing RAW metadata".to_owned())
                })?;
                let luma = compact_raw_luma(plane, width, height, raw)?;
                ImageFrame::new(
                    frame.width,
                    frame.height,
                    ImageFrameFormat::Gray8,
                    vec![ImagePlane::new(Arc::from(luma), frame.width)],
                    frame.identity.clone(),
                    None,
                    None,
                )
                .map_err(|error| {
                    NodeError::Execution(format!("invalid demosaic output: {error}"))
                })?
            }
            DemosaicOutputFormat::Gray16Le => {
                let plane = frame.planes.first().ok_or_else(|| {
                    NodeError::Precondition("BayerRaw image is missing pixel plane".to_owned())
                })?;
                let width = usize::try_from(frame.width).map_err(|_| {
                    NodeError::Execution("BayerRaw width overflows host".to_owned())
                })?;
                let height = usize::try_from(frame.height).map_err(|_| {
                    NodeError::Execution("BayerRaw height overflows host".to_owned())
                })?;
                let raw = frame.raw.as_ref().ok_or_else(|| {
                    NodeError::Precondition("BayerRaw image is missing RAW metadata".to_owned())
                })?;
                let luma = compact_raw_luma(plane, width, height, raw)?;
                let mut bytes = Vec::with_capacity(luma.len() * 2);
                for value in luma {
                    let sample = u16::from(value) * 257;
                    bytes.extend_from_slice(&sample.to_le_bytes());
                }
                let stride_bytes = frame.width.checked_mul(2).ok_or_else(|| {
                    NodeError::Execution("BayerRaw output stride overflows host".to_owned())
                })?;
                ImageFrame::new(
                    frame.width,
                    frame.height,
                    ImageFrameFormat::Gray16Le,
                    vec![ImagePlane::new(Arc::from(bytes), stride_bytes)],
                    frame.identity.clone(),
                    None,
                    None,
                )
                .map_err(|error| {
                    NodeError::Execution(format!("invalid demosaic output: {error}"))
                })?
            }
        };
        rt.emit("image", DataPacket::ImageFrame(Arc::new(image)))?;
        rt.report_state(NodeRuntimeState::Running, "demosaic frame emitted");
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl DemosaicNode {
    fn raw_metadata(
        &self,
        _frame_raw: Option<&crate::engine::RawMetadata>,
    ) -> Result<crate::engine::RawMetadata, NodeError> {
        let bayer = config_string(&self.spec, "bayer")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| NodeError::Config("demosaic requires explicit bayer".to_owned()))?;
        let bayer_pattern = match bayer.as_str() {
            "rggb" => BayerPattern::Rggb,
            "bggr" => BayerPattern::Bggr,
            "grbg" => BayerPattern::Grbg,
            "gbrg" => BayerPattern::Gbrg,
            value => {
                return Err(NodeError::Config(format!(
                    "unsupported demosaic Bayer pattern `{value}`"
                )));
            }
        };
        let bits_per_sample = config_u64(&self.spec, "bitsPerSample")
            .and_then(|value| u8::try_from(value).ok())
            .filter(|value| (1..=16).contains(value))
            .ok_or_else(|| {
                NodeError::Config(
                    "demosaic bitsPerSample must be a non-negative integer in 1..=16".to_owned(),
                )
            })?;
        let black_level = u16::try_from(config_u64(&self.spec, "blackLevel").ok_or_else(|| {
            NodeError::Config("demosaic requires explicit blackLevel".to_owned())
        })?)
        .map_err(|_| NodeError::Config("demosaic blackLevel must fit in u16".to_owned()))?;
        Ok(crate::engine::RawMetadata {
            bayer_pattern,
            bits_per_sample,
            black_level: Some(black_level),
            white_level: None,
        })
    }

    fn output_format(&self) -> Result<DemosaicOutputFormat, NodeError> {
        match config_string(&self.spec, "outputFormat")
            .as_deref()
            .unwrap_or("rgba")
        {
            "rgba" => Ok(DemosaicOutputFormat::Rgba8),
            "gray8" => Ok(DemosaicOutputFormat::Gray8),
            "gray16le" => Ok(DemosaicOutputFormat::Gray16Le),
            value => Err(NodeError::Config(format!(
                "unsupported demosaic outputFormat `{value}`"
            ))),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BayerChannel {
    Red,
    Green,
    Blue,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DemosaicOutputFormat {
    Rgba8,
    Gray8,
    Gray16Le,
}

fn config_string(spec: &NodeSpec, key: &str) -> Option<String> {
    let value = spec.config.get(key)?;
    value
        .as_str()
        .map(|text| text.trim().to_owned())
        .or_else(|| value.as_u64().map(|number| number.to_string()))
}

fn config_u64(spec: &NodeSpec, key: &str) -> Option<u64> {
    let value = spec.config.get(key)?;
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    value.as_str()?.trim().parse::<u64>().ok()
}

fn demosaic_bayer_bilinear(frame: &ImageFrame) -> Result<Arc<[u8]>, NodeError> {
    let raw = frame.raw.as_ref().ok_or_else(|| {
        NodeError::Precondition("BayerRaw image is missing RAW metadata".to_owned())
    })?;
    let plane = frame.planes.first().ok_or_else(|| {
        NodeError::Precondition("BayerRaw image is missing pixel plane".to_owned())
    })?;
    let width = usize::try_from(frame.width)
        .map_err(|_| NodeError::Execution("BayerRaw width overflows host".to_owned()))?;
    let height = usize::try_from(frame.height)
        .map_err(|_| NodeError::Execution("BayerRaw height overflows host".to_owned()))?;
    let luma = compact_raw_luma(plane, width, height, raw)?;
    let mut rgba = Vec::with_capacity(
        width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| {
                NodeError::Execution("demosaic output size overflows host".to_owned())
            })?,
    );
    for y in 0..height {
        for x in 0..width {
            let fallback = luma[y * width + x];
            let r = average_channel(
                &luma,
                width,
                height,
                raw.bayer_pattern,
                x,
                y,
                BayerChannel::Red,
            )
            .unwrap_or(fallback);
            let g = average_channel(
                &luma,
                width,
                height,
                raw.bayer_pattern,
                x,
                y,
                BayerChannel::Green,
            )
            .unwrap_or(fallback);
            let b = average_channel(
                &luma,
                width,
                height,
                raw.bayer_pattern,
                x,
                y,
                BayerChannel::Blue,
            )
            .unwrap_or(fallback);
            rgba.extend_from_slice(&[r, g, b, u8::MAX]);
        }
    }
    Ok(Arc::from(rgba))
}

fn compact_raw_luma(
    plane: &ImagePlane,
    width: usize,
    height: usize,
    raw: &crate::engine::RawMetadata,
) -> Result<Vec<u8>, NodeError> {
    let stride = usize::try_from(plane.stride_bytes)
        .map_err(|_| NodeError::Execution("BayerRaw stride overflows host".to_owned()))?;
    let row_bytes = width
        .checked_mul(2)
        .ok_or_else(|| NodeError::Execution("BayerRaw row size overflows host".to_owned()))?;
    let white_default = (1u32 << u32::from(raw.bits_per_sample)).saturating_sub(1);
    let black = u32::from(raw.black_level.unwrap_or(0));
    let white = u32::from(
        raw.white_level
            .unwrap_or(u16::try_from(white_default).unwrap_or(u16::MAX)),
    );
    let span = white.saturating_sub(black).max(1);
    let mut luma = Vec::with_capacity(
        width
            .checked_mul(height)
            .ok_or_else(|| NodeError::Execution("BayerRaw luma size overflows host".to_owned()))?,
    );
    for y in 0..height {
        let start = stride
            .checked_mul(y)
            .ok_or_else(|| NodeError::Execution("BayerRaw row offset overflows host".to_owned()))?;
        let end = start
            .checked_add(row_bytes)
            .ok_or_else(|| NodeError::Execution("BayerRaw row extent overflows host".to_owned()))?;
        let row = plane.bytes.get(start..end).ok_or_else(|| {
            NodeError::Execution("BayerRaw plane is shorter than declared stride".to_owned())
        })?;
        for sample in row.chunks_exact(2) {
            let value = u32::from(u16::from_le_bytes([sample[0], sample[1]]));
            let normalized = value.saturating_sub(black).min(span);
            luma.push(((normalized * 255 + span / 2) / span) as u8);
        }
    }
    Ok(luma)
}

fn average_channel(
    luma: &[u8],
    width: usize,
    height: usize,
    pattern: BayerPattern,
    x: usize,
    y: usize,
    channel: BayerChannel,
) -> Option<u8> {
    let mut sum = 0u32;
    let mut count = 0u32;
    let y_min = y.saturating_sub(1);
    let y_max = (y + 1).min(height.saturating_sub(1));
    let x_min = x.saturating_sub(1);
    let x_max = (x + 1).min(width.saturating_sub(1));
    for yy in y_min..=y_max {
        for xx in x_min..=x_max {
            if bayer_channel(pattern, xx, yy) == channel {
                sum += u32::from(luma[yy * width + xx]);
                count += 1;
            }
        }
    }
    (count > 0).then(|| ((sum + count / 2) / count) as u8)
}

fn bayer_channel(pattern: BayerPattern, x: usize, y: usize) -> BayerChannel {
    let even_x = x % 2 == 0;
    let even_y = y % 2 == 0;
    match pattern {
        BayerPattern::Rggb => match (even_y, even_x) {
            (true, true) => BayerChannel::Red,
            (true, false) | (false, true) => BayerChannel::Green,
            (false, false) => BayerChannel::Blue,
        },
        BayerPattern::Bggr => match (even_y, even_x) {
            (true, true) => BayerChannel::Blue,
            (true, false) | (false, true) => BayerChannel::Green,
            (false, false) => BayerChannel::Red,
        },
        BayerPattern::Grbg => match (even_y, even_x) {
            (true, true) | (false, false) => BayerChannel::Green,
            (true, false) => BayerChannel::Red,
            (false, true) => BayerChannel::Blue,
        },
        BayerPattern::Gbrg => match (even_y, even_x) {
            (true, true) | (false, false) => BayerChannel::Green,
            (true, false) => BayerChannel::Blue,
            (false, true) => BayerChannel::Red,
        },
    }
}

/// pass-through 转换节点：原样转发视频/图像帧。
pub struct PassThroughNode {
    kind: &'static str,
    output_port: String,
    /// 最近一次收帧的进程单调时间戳；0 表示「未收到帧」或「已回落 idle」。
    last_frame_at: Arc<AtomicU64>,
}

/// 输出端口 id 跟随 web 图，避免硬编码导致接线断裂。
fn output_port(spec: &NodeSpec) -> String {
    spec.outputs
        .first()
        .map(|port| port.id.clone())
        .unwrap_or_else(|| "frames".to_owned())
}

impl NodeInstance for PassThroughNode {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for frames");
        let last_frame_at = Arc::clone(&self.last_frame_at);
        let reporter = rt.context().reporter.clone();
        let cancel = Arc::clone(&rt.context().cancel);
        let kind = self.kind;
        rt.spawn(format!("{kind}-liveness"), move |_ctx| {
            liveness_loop(last_frame_at, reporter, cancel);
        });
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if self.kind == "imageLayer"
            && matches!(&packet, DataPacket::ImageFrame(frame) if frame.format == ImageFrameFormat::BayerRaw)
        {
            return Err(NodeError::Precondition(
                "imageLayer rejects BayerRaw; connect an explicit Demosaic node first".to_owned(),
            ));
        }
        match packet {
            DataPacket::VideoFrame(_) | DataPacket::ImageFrame(_) => {
                // 收到第一帧即进入 running；无帧超时后 last_frame_at 会回 0，下一帧可重新上报。
                let now = host_monotonic_time_ns();
                if self.last_frame_at.swap(now, Ordering::Relaxed) == 0 {
                    rt.report_state(NodeRuntimeState::Running, "relaying frames");
                }
                let _ = rt.emit(&self.output_port, packet);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.last_frame_at.store(0, Ordering::Relaxed);
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 活性检测：pass-through 节点本身没有 stop 信号输入，靠收帧间隔回落 idle。
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

/// 帧降采样节点：按目标 fps 丢弃中间帧。
pub struct FrameSamplerFactory;

impl NodeFactory for FrameSamplerFactory {
    fn kind(&self) -> &'static str {
        "frameSampler"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(FrameSamplerNode {
            spec,
            last_emit_ns: None,
            active: false,
        }))
    }
}

pub struct FrameSamplerNode {
    spec: NodeSpec,
    last_emit_ns: Option<u64>,
    /// 是否已上报过 running；避免每帧重复上报，stop 后复位。
    active: bool,
}

impl NodeInstance for FrameSamplerNode {
    fn kind(&self) -> &'static str {
        "frameSampler"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let fps_limit = config_f64(&self.spec, "fpsLimit", 30.0).max(1.0);
        rt.report_state(
            NodeRuntimeState::Ready,
            format!("downsampling to {fps_limit:.0} fps"),
        );
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let DataPacket::VideoFrame(frame) = packet else {
            return Ok(());
        };
        let now = host_monotonic_time_ns();
        let interval_ns = fps_interval_ns(&self.spec);
        if self
            .last_emit_ns
            .is_none_or(|last| now.saturating_sub(last) >= interval_ns)
        {
            self.last_emit_ns = Some(now);
            if !self.active {
                self.active = true;
                rt.report_state(NodeRuntimeState::Running, "downsampling frames");
            }
            let _ = rt.emit("frames", DataPacket::VideoFrame(frame));
        }
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.active = false;
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

fn fps_interval_ns(spec: &NodeSpec) -> u64 {
    let fps = config_f64(spec, "fpsLimit", 30.0).clamp(1.0, 240.0);
    (1_000_000_000.0 / fps) as u64
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
    use crate::engine::{NodeReporter, OutputRegistry, PortCardinality, PortSpec, SpawnContext};
    use crate::platform::{DecodedVideoFrame, StreamFrameIdentity, StreamSessionId};

    fn out_port(id: &str) -> PortSpec {
        PortSpec {
            id: id.to_owned(),
            label: id.to_owned(),
            kind: "stream.video-frame".to_owned(),
            cardinality: PortCardinality::One,
            required: false,
        }
    }

    fn pt_spec(kind: &str, output_id: &str) -> NodeSpec {
        NodeSpec {
            id: "n".to_owned(),
            kind: kind.to_owned(),
            title: kind.to_owned(),
            inputs: vec![],
            outputs: vec![out_port(output_id)],
            config: serde_json::json!({}),
        }
    }

    fn demosaic_spec(config: serde_json::Value) -> NodeSpec {
        let mut spec = pt_spec("demosaic", "image");
        spec.config = config;
        spec
    }

    /// 构造一个带 `record` 回调的 runtime：emit 无下游时也会把 packet 送进 record sink。
    fn runtime(
        outputs: OutputRegistry,
        state_tx: mpsc::Sender<crate::engine::NodeStatusReport>,
    ) -> NodeRuntime {
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("n".to_owned(), state_tx, event_tx);
        let ctx = SpawnContext {
            outputs,
            reporter,
            services: Arc::new(crate::engine::EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        NodeRuntime::new(ctx)
    }

    fn video_frame(seq: u64) -> DataPacket {
        let session = StreamSessionId::new("test-stream").expect("session id");
        DataPacket::VideoFrame(Arc::new(DecodedVideoFrame {
            width: 2,
            height: 2,
            rgba: Arc::from(vec![0u8; 16]),
            identity: StreamFrameIdentity::unavailable(session, 0, seq, "test"),
        }))
    }

    fn bayer_raw_frame(
        source: &DecodedVideoFrame,
        width: u32,
        height: u32,
        samples: &[u16],
        bayer_pattern: BayerPattern,
        bits_per_sample: u8,
        black_level: Option<u16>,
    ) -> Arc<ImageFrame> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        Arc::new(
            ImageFrame::new(
                width,
                height,
                ImageFrameFormat::BayerRaw,
                vec![ImagePlane::new(Arc::from(bytes), width * 2)],
                crate::engine::ImageFrameIdentity::from(&source.identity),
                None,
                Some(crate::engine::RawMetadata {
                    bayer_pattern,
                    bits_per_sample,
                    black_level,
                    white_level: None,
                }),
            )
            .expect("valid BayerRaw fixture"),
        )
    }

    fn last_state(
        rx: &mpsc::Receiver<crate::engine::NodeStatusReport>,
    ) -> Option<crate::engine::NodeRuntimeState> {
        let mut last = None;
        while let Ok(report) = rx.try_recv() {
            last = Some(report.state);
        }
        last
    }

    #[test]
    fn pass_through_factories_instantiate_with_expected_kinds() {
        let cases: [(&dyn NodeFactory, &str, &str); 4] = [
            (&RtspDecoderFactory, "rtspDecoder", "frames"),
            (&DemosaicFactory, "demosaic", "image"),
            (&VideoLayerFactory, "videoLayer", "layer"),
            (&ImageLayerFactory, "imageLayer", "layer"),
        ];
        for (factory, kind, output_id) in cases {
            assert_eq!(factory.kind(), kind);
            let instance = factory
                .instantiate(pt_spec(kind, output_id))
                .expect("instantiate");
            assert_eq!(instance.kind(), kind);
        }
    }

    #[test]
    fn pass_through_reports_running_on_first_frame_and_relays() {
        let mut outputs = OutputRegistry::default();
        let relayed: Arc<Mutex<Vec<DataPacket>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |packet| sink.lock().unwrap().push(packet)));

        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let mut node = PassThroughNode {
            kind: "videoLayer",
            output_port: "layer".to_owned(),
            last_frame_at: Arc::new(AtomicU64::new(0)),
        };

        // on_start → ready
        node.on_start(&mut rt).expect("on_start");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Ready));

        // 第一帧 → running + relay
        node.on_input("video", video_frame(1), &mut rt)
            .expect("on_input");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));
        assert_eq!(relayed.lock().unwrap().len(), 1);

        // 第二帧 → 不重复上报状态（无新状态报告），但继续 relay
        node.on_input("video", video_frame(2), &mut rt)
            .expect("on_input");
        assert_eq!(last_state(&state_rx), None);
        assert_eq!(relayed.lock().unwrap().len(), 2);
    }

    #[test]
    fn image_layer_relays_image_frames_without_retyping_them() {
        let mut outputs = OutputRegistry::default();
        let relayed = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |packet| sink.lock().push(packet)));
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let mut node = PassThroughNode {
            kind: "imageLayer",
            output_port: "layer".to_owned(),
            last_frame_at: Arc::new(AtomicU64::new(0)),
        };
        let DataPacket::VideoFrame(frame) = video_frame(7) else {
            panic!("test fixture must be a video frame");
        };
        let image = Arc::new(crate::engine::ImageFrame::from(frame.as_ref()));

        node.on_input("image", DataPacket::ImageFrame(image), &mut rt)
            .expect("image layer accepts image.frame");

        let relayed = relayed.lock();
        assert_eq!(relayed.len(), 1);
        let DataPacket::ImageFrame(frame) = &relayed[0] else {
            panic!("image layer must retain image.frame type");
        };
        assert_eq!(frame.identity.frame_sequence, 7);
    }

    #[test]
    fn image_layer_rejects_bayer_raw_with_demosaic_guidance() {
        let mut outputs = OutputRegistry::default();
        let relayed = Arc::new(parking_lot::Mutex::new(0usize));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |_| *sink.lock() += 1));
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let DataPacket::VideoFrame(source) = video_frame(9) else {
            panic!("test fixture must be a video frame");
        };
        let raw = Arc::new(
            crate::engine::ImageFrame::new(
                2,
                2,
                ImageFrameFormat::BayerRaw,
                vec![crate::engine::ImagePlane::new(Arc::from(vec![0; 8]), 4)],
                crate::engine::ImageFrameIdentity::from(&source.identity),
                None,
                Some(crate::engine::RawMetadata {
                    bayer_pattern: crate::engine::BayerPattern::Rggb,
                    bits_per_sample: 12,
                    black_level: None,
                    white_level: None,
                }),
            )
            .expect("valid BayerRaw fixture"),
        );
        let mut node = PassThroughNode {
            kind: "imageLayer",
            output_port: "layer".to_owned(),
            last_frame_at: Arc::new(AtomicU64::new(0)),
        };

        let error = node
            .on_input("image", DataPacket::ImageFrame(raw), &mut rt)
            .expect_err("BayerRaw must require explicit Demosaic");
        assert!(error.to_string().contains("Demosaic"));
        assert_eq!(*relayed.lock(), 0);
    }

    #[test]
    fn demosaic_converts_bayer_raw_to_rgba_image_with_same_identity() {
        let mut outputs = OutputRegistry::default();
        let relayed = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |packet| sink.lock().push(packet)));
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let DataPacket::VideoFrame(source) = video_frame(11) else {
            panic!("test fixture must be a video frame");
        };
        let raw = bayer_raw_frame(
            &source,
            2,
            2,
            &[4095, 0, 0, 4095],
            BayerPattern::Rggb,
            12,
            None,
        );
        let mut node = DemosaicNode {
            spec: demosaic_spec(serde_json::json!({
                "algorithm": "bilinear",
                "outputFormat": "rgba",
                "bayer": "rggb",
                "bitsPerSample": 12,
                "blackLevel": 0,
            })),
        };

        node.on_input("raw", DataPacket::ImageFrame(raw), &mut rt)
            .expect("BayerRaw demosaic succeeds");

        let relayed = relayed.lock();
        assert_eq!(relayed.len(), 1);
        let DataPacket::ImageFrame(image) = &relayed[0] else {
            panic!("demosaic must emit image.frame");
        };
        assert_eq!(image.format, ImageFrameFormat::Rgba8);
        assert_eq!(image.identity.frame_sequence, 11);
        let rgba = image.rgba8_plane().expect("RGBA plane");
        assert_eq!(rgba.bytes.len(), 16);
        assert!(rgba.bytes.iter().any(|byte| *byte > 0));
    }

    #[test]
    fn demosaic_gray8_applies_black_level_and_config_bits() {
        let mut outputs = OutputRegistry::default();
        let relayed = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |packet| sink.lock().push(packet)));
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let DataPacket::VideoFrame(source) = video_frame(13) else {
            panic!("test fixture must be a video frame");
        };
        let raw = bayer_raw_frame(&source, 2, 1, &[0, 4095], BayerPattern::Rggb, 12, None);
        let mut node = DemosaicNode {
            spec: demosaic_spec(serde_json::json!({
                "algorithm": "bilinear",
                "outputFormat": "gray8",
                "bayer": "rggb",
                "bitsPerSample": 12,
                "blackLevel": 2048,
            })),
        };

        node.on_input("raw", DataPacket::ImageFrame(raw), &mut rt)
            .expect("gray8 demosaic succeeds");

        let relayed = relayed.lock();
        assert_eq!(relayed.len(), 1);
        let DataPacket::ImageFrame(image) = &relayed[0] else {
            panic!("demosaic must emit image.frame");
        };
        assert_eq!(image.format, ImageFrameFormat::Gray8);
        assert_eq!(image.planes[0].bytes.as_ref(), &[0, 255]);
    }

    #[test]
    fn demosaic_gray16le_uses_configured_output_format() {
        let mut outputs = OutputRegistry::default();
        let relayed = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |packet| sink.lock().push(packet)));
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let DataPacket::VideoFrame(source) = video_frame(14) else {
            panic!("test fixture must be a video frame");
        };
        let raw = bayer_raw_frame(&source, 2, 1, &[0, 4095], BayerPattern::Rggb, 12, None);
        let mut node = DemosaicNode {
            spec: demosaic_spec(serde_json::json!({
                "algorithm": "bilinear",
                "outputFormat": "gray16le",
                "bayer": "rggb",
                "bitsPerSample": 12,
                "blackLevel": 0,
            })),
        };

        node.on_input("raw", DataPacket::ImageFrame(raw), &mut rt)
            .expect("gray16le demosaic succeeds");

        let relayed = relayed.lock();
        assert_eq!(relayed.len(), 1);
        let DataPacket::ImageFrame(image) = &relayed[0] else {
            panic!("demosaic must emit image.frame");
        };
        assert_eq!(image.format, ImageFrameFormat::Gray16Le);
        assert_eq!(image.planes[0].stride_bytes, 4);
        assert_eq!(image.planes[0].bytes.as_ref(), &[0, 0, 255, 255]);
    }

    #[test]
    fn demosaic_bayer_override_changes_channel_interpretation() {
        let mut outputs = OutputRegistry::default();
        let relayed = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |packet| sink.lock().push(packet)));
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let DataPacket::VideoFrame(source) = video_frame(15) else {
            panic!("test fixture must be a video frame");
        };
        let raw = bayer_raw_frame(
            &source,
            2,
            2,
            &[4095, 0, 0, 0],
            BayerPattern::Rggb,
            12,
            None,
        );
        let mut node = DemosaicNode {
            spec: demosaic_spec(serde_json::json!({
                "algorithm": "bilinear",
                "outputFormat": "rgba",
                "bayer": "bggr",
                "bitsPerSample": 12,
                "blackLevel": 0,
            })),
        };

        node.on_input("raw", DataPacket::ImageFrame(raw), &mut rt)
            .expect("Bayer override succeeds");

        let relayed = relayed.lock();
        assert_eq!(relayed.len(), 1);
        let DataPacket::ImageFrame(image) = &relayed[0] else {
            panic!("demosaic must emit image.frame");
        };
        let rgba = image.rgba8_plane().expect("RGBA plane");
        assert_eq!(&rgba.bytes[0..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn demosaic_rejects_missing_explicit_raw_config_even_when_frame_has_metadata() {
        let outputs = OutputRegistry::default();
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let DataPacket::VideoFrame(source) = video_frame(16) else {
            panic!("test fixture must be a video frame");
        };
        let raw = bayer_raw_frame(&source, 2, 1, &[0, 4095], BayerPattern::Rggb, 12, None);
        let mut node = DemosaicNode {
            spec: demosaic_spec(serde_json::json!({
                "algorithm": "bilinear",
                "outputFormat": "rgba",
                "bitsPerSample": 12,
                "blackLevel": 0,
            })),
        };

        let error = node
            .on_input("raw", DataPacket::ImageFrame(raw), &mut rt)
            .expect_err("Demosaic must not fall back to frame RAW metadata");
        assert!(error.to_string().contains("explicit bayer"));
    }

    #[test]
    fn demosaic_rejects_non_bayer_input() {
        let outputs = OutputRegistry::default();
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let DataPacket::VideoFrame(source) = video_frame(12) else {
            panic!("test fixture must be a video frame");
        };
        let image = Arc::new(ImageFrame::from(source.as_ref()));
        let mut node = DemosaicNode {
            spec: pt_spec("demosaic", "image"),
        };

        let error = node
            .on_input("raw", DataPacket::ImageFrame(image), &mut rt)
            .expect_err("only BayerRaw can enter Demosaic");
        assert!(error.to_string().contains("BayerRaw"));
    }

    #[test]
    fn pass_through_ignores_non_frame_packets() {
        let mut outputs = OutputRegistry::default();
        let relayed: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |_| *sink.lock().unwrap() += 1));

        let mut node = PassThroughNode {
            kind: "videoLayer",
            output_port: "layer".to_owned(),
            last_frame_at: Arc::new(AtomicU64::new(0)),
        };
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);

        node.on_input(
            "video",
            DataPacket::Json(Arc::new(serde_json::json!({}))),
            &mut rt,
        )
        .expect("on_input");
        assert_eq!(*relayed.lock().unwrap(), 0);
        assert_eq!(last_state(&state_rx), None); // 无帧，不进入 running
    }

    #[test]
    fn pass_through_stop_resets_to_idle_and_rearms() {
        let outputs = OutputRegistry::default();
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let mut node = PassThroughNode {
            kind: "videoLayer",
            output_port: "layer".to_owned(),
            last_frame_at: Arc::new(AtomicU64::new(0)),
        };

        node.on_input("video", video_frame(1), &mut rt)
            .expect("on_input");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));

        node.on_stop(&mut rt).expect("on_stop");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Idle));

        // stop 后 active 复位，再次收帧应重新上报 running
        node.on_input("video", video_frame(2), &mut rt)
            .expect("on_input");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));
    }

    #[test]
    fn pass_through_liveness_returns_idle_after_frame_stall() {
        let outputs = OutputRegistry::default();
        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let mut node = PassThroughNode {
            kind: "videoLayer",
            output_port: "layer".to_owned(),
            last_frame_at: Arc::new(AtomicU64::new(0)),
        };

        node.on_start(&mut rt).expect("on_start");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Ready));
        node.on_input("video", video_frame(1), &mut rt)
            .expect("on_input");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));

        let report = state_rx
            .recv_timeout(Duration::from_millis(1_700))
            .expect("stall should report idle");
        assert_eq!(report.state, NodeRuntimeState::Idle);
        rt.stop_background();
    }

    #[test]
    fn pass_through_rejects_actions() {
        let outputs = OutputRegistry::default();
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);
        let mut node = PassThroughNode {
            kind: "videoLayer",
            output_port: "layer".to_owned(),
            last_frame_at: Arc::new(AtomicU64::new(0)),
        };
        let err = node
            .on_action(NodeAction::Trigger, &mut rt)
            .expect_err("unsupported");
        assert!(matches!(err, NodeError::UnsupportedAction(_)));
    }

    #[test]
    fn frame_sampler_rate_limits_by_fps_limit() {
        let mut outputs = OutputRegistry::default();
        let relayed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |packet| {
            if let DataPacket::VideoFrame(frame) = packet {
                sink.lock().unwrap().push(frame.identity.frame_sequence);
            }
        }));

        let mut spec = pt_spec("frameSampler", "frames");
        // 超低 fpsLimit=1 → 间隔 ~1s，连续两帧只应发射第一帧。
        spec.config = serde_json::json!({"fpsLimit": 1.0});
        let mut node = FrameSamplerNode {
            spec,
            last_emit_ns: None,
            active: false,
        };

        let (state_tx, state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);

        node.on_start(&mut rt).expect("on_start");
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Ready));

        node.on_input("video", video_frame(1), &mut rt)
            .expect("on_input");
        node.on_input("video", video_frame(2), &mut rt)
            .expect("on_input");
        node.on_input("video", video_frame(3), &mut rt)
            .expect("on_input");

        let seqs = relayed.lock().unwrap().clone();
        assert_eq!(seqs, vec![1]);
        assert_eq!(last_state(&state_rx), Some(NodeRuntimeState::Running));
    }

    #[test]
    fn frame_sampler_ignores_non_video_packets() {
        let mut outputs = OutputRegistry::default();
        let relayed: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let sink = Arc::clone(&relayed);
        outputs.set_record(Arc::new(move |_| *sink.lock().unwrap() += 1));

        let spec = pt_spec("frameSampler", "frames");
        let mut node = FrameSamplerNode {
            spec,
            last_emit_ns: None,
            active: false,
        };
        let (state_tx, _state_rx) = mpsc::channel();
        let mut rt = runtime(outputs, state_tx);

        node.on_input(
            "video",
            DataPacket::Json(Arc::new(serde_json::json!({}))),
            &mut rt,
        )
        .expect("on_input");
        assert_eq!(*relayed.lock().unwrap(), 0);
    }

    #[test]
    fn fps_interval_is_clamped() {
        let spec = pt_spec("frameSampler", "frames");
        assert_eq!(fps_interval_ns(&spec), 1_000_000_000 / 30);

        let mut spec = pt_spec("frameSampler", "frames");
        spec.config = serde_json::json!({"fpsLimit": 0.0});
        // 0 → clamp 到 1.0，间隔 1s
        assert_eq!(fps_interval_ns(&spec), 1_000_000_000);

        let mut spec = pt_spec("frameSampler", "frames");
        spec.config = serde_json::json!({"fpsLimit": 1000.0});
        // 1000 → clamp 到 240，间隔 1e9/240
        assert_eq!(fps_interval_ns(&spec), 1_000_000_000 / 240);
    }
}
