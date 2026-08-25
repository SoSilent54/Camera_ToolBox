//! 双路 RTSP 预览与 guided 自动采集。
//!
//! 预览：复用 `FfmpegRtspDecoder`（后台解码，共享帧槽），主线程上传纹理。
//! 采集：worker 线程按帧检测棋盘 → `estimate_pose` 计算姿态 → 与已采姿态
//! 比较（最小角差阈值），只有"新姿态"才采入 dataset；overlay 实时给出
//! 引导文本（未检测到棋盘 / 姿态重复 / 已覆盖 N 位姿）。
//! 无板验证：`PONGBOT_SYNTH=1` 用非棋盘合成帧（保证检测失败，验证引导路径）。

use camera_toolbox_adapters::calibration::OpenCvCalibrationBackend;
use camera_toolbox_adapters::media::ffmpeg_rtsp::FfmpegRtspDecoder;
use camera_toolbox_adapters::media::FfmpegRtspTransport;
use camera_toolbox_app::platform::{
    DecodedVideoFrame, LatestDecodedFrameSlot, RtspLatencyMode, SourcePts, StreamCancellation,
    StreamFrameIdentity, StreamSessionId,
};
use camera_toolbox_app::ports::calibration::{
    CalibrationBackend, CalibrationCancellation,
};
use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, ChessboardDetectionOutcome, InitialIntrinsics,
};
use godot::classes::image::Format;
use godot::classes::{Image, ImageTexture, TextureRect};
use godot::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 每路目标覆盖位姿数（达标自动停止）。
pub const CAPTURE_TARGET: usize = 12;
/// 最小位姿角差（度）：小于该差异视为重复姿态，不入 dataset。
pub const MIN_POSE_ANGLE_DEG: f64 = 10.0;
/// 采集 worker 检测节拍。
const DETECT_INTERVAL: Duration = Duration::from_millis(150);

/// worker → 主线程的引导状态。
#[derive(Default)]
pub struct GuideState {
    /// overlay 引导文本（未检测到棋盘 / 姿态重复 / 已覆盖 N 位姿）。
    pub text: String,
    /// 已采位姿数。
    pub captured_count: usize,
    /// 累计检测成功次数。
    pub found_count: u64,
}

/// 单路 RTSP 流：解码器 + 帧槽 + 已上传纹理 + guided 采集。
pub struct RtspStream {
    decoder: Option<FfmpegRtspDecoder>,
    slot: Arc<LatestDecodedFrameSlot>,
    last: Option<Arc<DecodedVideoFrame>>,
    texture: Option<Gd<ImageTexture>>,
    /// 采集开关（worker 与主线程共享）。
    capturing: Arc<AtomicBool>,
    /// 已采 dataset 帧（worker 写，solve 读；主线程只读计数）。
    captured: Arc<Mutex<Vec<Arc<DecodedVideoFrame>>>>,
    /// 已采姿态（rvec，与 captured 一一对应）。
    poses: Arc<Mutex<Vec<[f64; 3]>>>,
    /// 引导状态（worker 写，主线程读）。
    guide: Arc<Mutex<GuideState>>,
    worker: Option<std::thread::JoinHandle<()>>,
    target: usize,
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
        Ok(Self::new(Some(decoder), slot))
    }

    /// 合成测试帧模式（无板验证纹理与引导链路）；图案为非棋盘噪点。
    pub fn start_synth(channel: u16) -> Self {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let worker_slot = Arc::clone(&slot);
        std::thread::spawn(move || {
            let (width, height) = (640u32, 360u32);
            let mut seed = u64::from(channel) + 1;
            loop {
                let mut rgba = Vec::with_capacity((width * height * 4) as usize);
                for _ in 0..width * height {
                    // xorshift 伪随机噪点：无棋盘结构，保证检测失败（验证引导路径）。
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let v = (seed & 0xff) as u8;
                    let (r, g, b) = (v, v.wrapping_mul(2), 255 - v);
                    rgba.extend_from_slice(&[r, g, b, 255]);
                }
                worker_slot.publish(DecodedVideoFrame {
                    width,
                    height,
                    rgba: rgba.into(),
                    identity: StreamFrameIdentity {
                        stream_id: StreamSessionId::new(format!("synth-ch{channel}"))
                            .expect("合成会话 id"),
                        channel,
                        frame_sequence: seed,
                        source_pts: SourcePts::Unavailable {
                            reason: "synthetic".to_owned(),
                        },
                        host_monotonic_time_ns: 0,
                        device_timestamp_ns: Some(seed),
                    },
                });
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Self::new(None, slot)
    }

    fn new(decoder: Option<FfmpegRtspDecoder>, slot: Arc<LatestDecodedFrameSlot>) -> Self {
        Self {
            decoder,
            slot,
            last: None,
            texture: None,
            capturing: Arc::new(AtomicBool::new(false)),
            captured: Arc::new(Mutex::new(Vec::with_capacity(CAPTURE_TARGET))),
            poses: Arc::new(Mutex::new(Vec::with_capacity(CAPTURE_TARGET))),
            guide: Arc::new(Mutex::new(GuideState::default())),
            worker: None,
            target: CAPTURE_TARGET,
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

    /// 切换 guided 采集；启动/停止 worker 线程。
    pub fn toggle_capture(&mut self, board: BoardSpec) -> bool {
        let on = !self.capturing.load(Ordering::Acquire);
        self.capturing.store(on, Ordering::Release);
        if on {
            let capturing = Arc::clone(&self.capturing);
            let slot = Arc::clone(&self.slot);
            let captured = Arc::clone(&self.captured);
            let poses = Arc::clone(&self.poses);
            let guide = Arc::clone(&self.guide);
            let target = self.target;
            self.worker = Some(std::thread::spawn(move || {
                guided_capture_loop(
                    capturing, slot, captured, poses, guide, board, target,
                );
            }));
        } else if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        on
    }

    /// 读取引导状态（主线程）。
    pub fn guide(&self) -> (String, usize) {
        let guide = self.guide.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (guide.text.clone(), guide.captured_count)
    }

    /// 采集是否进行中。
    pub fn is_capturing(&self) -> bool {
        self.capturing.load(Ordering::Acquire)
    }

    /// 是否达到目标位姿数。
    pub fn complete(&self) -> bool {
        let poses = self.poses.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        poses.len() >= self.target
    }

    /// 取已采 dataset 帧（solve 用）。
    pub fn captured_frames(&self) -> Vec<Arc<DecodedVideoFrame>> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// guided 采集循环（worker 线程）：检测 → 姿态 → 去重入队 → 引导文本。
#[allow(clippy::too_many_arguments)]
fn guided_capture_loop(
    capturing: Arc<AtomicBool>,
    slot: Arc<LatestDecodedFrameSlot>,
    captured: Arc<Mutex<Vec<Arc<DecodedVideoFrame>>>>,
    poses: Arc<Mutex<Vec<[f64; 3]>>>,
    guide: Arc<Mutex<GuideState>>,
    board: BoardSpec,
    target: usize,
) {
    let backend = OpenCvCalibrationBackend;
    let cancellation = CalibrationCancellation::default();
    let initial = default_initial_intrinsics();
    while capturing.load(Ordering::Acquire) {
        let count = {
            let poses = poses.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            poses.len()
        };
        if count >= target {
            set_guide(&guide, &format!("已覆盖 {count}/{target} 位姿，采集完成"), count);
            break;
        }
        let Some(frame) = slot.latest() else {
            std::thread::sleep(DETECT_INTERVAL);
            continue;
        };
        let png = match encode_png(&frame.rgba, frame.width, frame.height) {
            Ok(png) => png,
            Err(_) => {
                std::thread::sleep(DETECT_INTERVAL);
                continue;
            }
        };
        let expected = CalibrationImageSize {
            width: frame.width,
            height: frame.height,
        };
        match backend.detect_png(&png, expected, 256 * 1024 * 1024, board, &cancellation) {
            Ok(ChessboardDetectionOutcome::Found(detection)) => {
                let pose = match backend.estimate_pose(&detection, &initial, board, &cancellation)
                {
                    Ok(view) => view.rotation_vector,
                    Err(_) => {
                        set_guide(&guide, "检测到棋盘，姿态估计失败，请稍作调整", count);
                        std::thread::sleep(DETECT_INTERVAL);
                        continue;
                    }
                };
                let mut poses = poses.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let is_new = poses
                    .iter()
                    .all(|existing| pose_angle_deg(existing, &pose) >= MIN_POSE_ANGLE_DEG);
                if is_new {
                    poses.push(pose);
                    let mut captured =
                        captured.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    captured.push(frame);
                    let count = poses.len();
                    drop(captured);
                    drop(poses);
                    if count >= target {
                        set_guide(&guide, &format!("已覆盖 {count}/{target} 位姿，采集完成"), count);
                    } else {
                        set_guide(
                            &guide,
                            &format!("已采 {count}/{target} 位姿，请变换棋盘姿态（旋转/俯仰/平移）"),
                            count,
                        );
                    }
                } else {
                    drop(poses);
                    set_guide(&guide, "姿态重复，请变换棋盘位姿", count);
                }
            }
            Ok(ChessboardDetectionOutcome::NotFound { .. }) => {
                set_guide(&guide, "未检测到棋盘，请将棋盘置于画面中并调整角度", count);
            }
            Err(error) => {
                set_guide(&guide, &format!("检测失败：{error}"), count);
            }
        }
        std::thread::sleep(DETECT_INTERVAL);
    }
}

/// 两姿态（rvec）夹角（度）。
fn pose_angle_deg(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    let na = norm(a);
    let nb = norm(b);
    if na < 1e-9 || nb < 1e-9 {
        return 180.0;
    }
    let cos = (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]) / (na * nb);
    cos.clamp(-1.0, 1.0).acos().to_degrees()
}

fn norm(v: &[f64; 3]) -> f64 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn set_guide(guide: &Arc<Mutex<GuideState>>, text: &str, count: usize) {
    let mut state = guide.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    state.text = text.to_owned();
    state.captured_count = count;
}

/// 默认初始内参（fx=fy=900，主点居中）。
fn default_initial_intrinsics() -> InitialIntrinsics {
    InitialIntrinsics {
        camera_matrix: [900.0, 0.0, 320.0, 0.0, 900.0, 180.0, 0.0, 0.0, 1.0],
        distortion_coefficients: vec![0.0, 0.0, 0.0, 0.0, 0.0],
    }
}

/// RGBA 帧编码为 PNG 字节。
fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec()).ok_or("帧尺寸非法")?;
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|error| format!("PNG 编码失败：{error}"))?;
    Ok(buf)
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

    /// 两路是否都达到目标位姿数。
    pub fn both_complete(&self) -> bool {
        self.ch0.complete() && self.ch3.complete()
    }
}
