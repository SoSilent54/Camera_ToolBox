//! 双路 RTSP 预览与自动采集：解码器管理 + 帧 → ImageTexture 上传 + dataset 采集。
//!
//! 复用 adapters 的 `FfmpegRtspDecoder`（后台线程解码，共享帧槽）；
//! 主线程 `pump` 轮询新帧并上传纹理，`tick_capture` 按间隔采样帧入 dataset。
//! 无板验证：`PONGBOT_SYNTH=1` 时用合成测试帧替代真实 RTSP。

use camera_toolbox_adapters::media::ffmpeg_rtsp::FfmpegRtspDecoder;
use camera_toolbox_adapters::media::FfmpegRtspTransport;
use camera_toolbox_app::platform::{
    DecodedVideoFrame, LatestDecodedFrameSlot, RtspLatencyMode, SourcePts, StreamCancellation,
    StreamFrameIdentity, StreamSessionId,
};
use godot::classes::image::Format;
use godot::classes::{Image, ImageTexture, TextureRect};
use godot::prelude::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 每路目标采集帧数（达标自动停止并完成 Step 2）。
pub const CAPTURE_TARGET: usize = 20;
/// 自动采集采样间隔。
pub const CAPTURE_INTERVAL: Duration = Duration::from_millis(600);

/// 单路 RTSP 流：解码器 + 帧槽 + 已上传纹理 + 采集队列。
pub struct RtspStream {
    decoder: Option<FfmpegRtspDecoder>,
    slot: Arc<LatestDecodedFrameSlot>,
    last: Option<Arc<DecodedVideoFrame>>,
    texture: Option<Gd<ImageTexture>>,
    /// 自动采集开关。
    pub capturing: bool,
    /// 已采集帧队列（有界，达标即停）。
    pub captured: Vec<Arc<DecodedVideoFrame>>,
    pub target: usize,
    last_sample: Option<Instant>,
    interval: Duration,
}

impl RtspStream {
    /// 启动真实 RTSP 解码（Tcp 传输，低延迟模式）。
    pub fn start(
        host: &str,
        rtsp_port: u16,
        channel: u16,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let cancellation = StreamCancellation::default();
        let session = StreamSessionId::new(format!("ch{channel}-{host}"))
            .map_err(|error| error.to_string())?;
        let url = format!("rtsp://{host}:{rtsp_port}/PRR");
        let decoder = FfmpegRtspDecoder::start(
            &url,
            FfmpegRtspTransport::Tcp,
            RtspLatencyMode::Low,
            width,
            height,
            session,
            channel,
            Arc::clone(&slot),
            Duration::from_secs(8),
            false,
            &cancellation,
        )
        .map_err(|error| format!("CH{channel} 解码器启动失败：{error}"))?;
        Ok(Self::new(Some(decoder), slot, channel))
    }

    /// 合成测试帧模式（无板验证纹理与采集链路）。
    pub fn start_synth(channel: u16) -> Self {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let worker_slot = Arc::clone(&slot);
        std::thread::spawn(move || {
            let (width, height) = (640u32, 360u32);
            let mut tick = 0u8;
            loop {
                let mut rgba = Vec::with_capacity((width * height * 4) as usize);
                for y in 0..height {
                    for x in 0..width {
                        // 渐变背景 + 运动方块，便于肉眼/截图确认帧更新。
                        let r = ((x as u8).wrapping_mul(3)).wrapping_add(tick);
                        let g = ((y as u8).wrapping_mul(3)).wrapping_add(tick);
                        let b = (x as u8 ^ y as u8).wrapping_add(tick);
                        let moving = (x as i32 / 60) % 2 == (y as i32 / 40) % 2;
                        let (r, g, b) = if moving {
                            (r.wrapping_add(60), g.wrapping_add(60), b.wrapping_add(60))
                        } else {
                            (r, g, b)
                        };
                        rgba.extend_from_slice(&[r, g, b, 255]);
                    }
                }
                worker_slot.publish(DecodedVideoFrame {
                    width,
                    height,
                    rgba: rgba.into(),
                    identity: StreamFrameIdentity {
                        stream_id: StreamSessionId::new(format!("synth-ch{channel}"))
                            .expect("合成会话 id"),
                        channel,
                        frame_sequence: tick as u64,
                        source_pts: SourcePts::Unavailable {
                            reason: "synthetic".to_owned(),
                        },
                        host_monotonic_time_ns: 0,
                        device_timestamp_ns: Some(tick as u64),
                    },
                });
                tick = tick.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Self::new(None, slot, channel)
    }

    fn new(decoder: Option<FfmpegRtspDecoder>, slot: Arc<LatestDecodedFrameSlot>, _channel: u16) -> Self {
        Self {
            decoder,
            slot,
            last: None,
            texture: None,
            capturing: false,
            captured: Vec::with_capacity(CAPTURE_TARGET),
            target: CAPTURE_TARGET,
            last_sample: None,
            interval: CAPTURE_INTERVAL,
        }
    }

    /// 主线程调用：有且仅在上传新帧时返回 `true`。
    pub fn pump(&mut self, target: &mut TextureRect) -> bool {
        let Some(frame) = self.slot.latest() else {
            return false;
        };
        if self.last.as_ref().is_some_and(|old| Arc::ptr_eq(old, &frame)) {
            return false;
        }
        self.last = Some(frame.clone());
        let Some(image) = Image::create_from_data(
            frame.width as i32,
            frame.height as i32,
            false,
            Format::RGBA8,
            &PackedByteArray::from(&frame.rgba[..]),
        ) else {
            return false;
        };
        let Some(texture) = ImageTexture::create_from_image(&image) else {
            return false;
        };
        self.texture = Some(texture.clone());
        target.set_texture(&texture);
        true
    }

    /// 切换采集开关；返回切换后的状态。
    pub fn toggle_capture(&mut self) -> bool {
        self.capturing = !self.capturing;
        if self.capturing {
            self.last_sample = None;
        }
        self.capturing
    }

    /// 采集节拍：开启且到间隔时采样最新帧；返回本次是否采到新帧。
    pub fn tick_capture(&mut self, now: Instant) -> bool {
        if !self.capturing || self.captured.len() >= self.target {
            if self.capturing && self.captured.len() >= self.target {
                self.capturing = false;
            }
            return false;
        }
        let due = self
            .last_sample
            .is_none_or(|last| now.duration_since(last) >= self.interval);
        if !due {
            return false;
        }
        let Some(frame) = self.slot.latest() else {
            return false;
        };
        self.last = Some(frame.clone());
        self.captured.push(frame);
        self.last_sample = Some(now);
        if self.captured.len() >= self.target {
            self.capturing = false;
        }
        true
    }
}

/// 双路预览状态。
pub struct StreamState {
    pub ch0: RtspStream,
    pub ch3: RtspStream,
}

impl StreamState {
    /// 按环境启动：`PONGBOT_SYNTH=1` 用合成帧，否则真实 RTSP（CH0:554 / CH3:557）。
    pub fn start(host: &str) -> Self {
        let synth = std::env::var("PONGBOT_SYNTH").is_ok_and(|v| v == "1");
        if synth {
            Self {
                ch0: RtspStream::start_synth(0),
                ch3: RtspStream::start_synth(3),
            }
        } else {
            let ch0 = RtspStream::start(host, 554, 0, 1920, 1080)
                .unwrap_or_else(|error| {
                    godot_print!("CH0 预览启动失败：{error}");
                    RtspStream::start_synth(0)
                });
            let ch3 = RtspStream::start(host, 557, 3, 1920, 1080)
                .unwrap_or_else(|error| {
                    godot_print!("CH3 预览启动失败：{error}");
                    RtspStream::start_synth(3)
                });
            Self { ch0, ch3 }
        }
    }

    /// 两路是否都达到目标帧数。
    pub fn both_complete(&self) -> bool {
        self.ch0.captured.len() >= self.ch0.target && self.ch3.captured.len() >= self.ch3.target
    }
}
