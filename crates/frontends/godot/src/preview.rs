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
    BoardSpec, CalibrationImageSize, CalibrationPoint, ChessboardDetection,
    ChessboardDetectionOutcome, InitialIntrinsics,
};
use godot::classes::image::Format;
use godot::classes::{Image, ImageTexture, TextureRect};
use godot::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use crate::guide_overlay::OverlayData;

/// 每路目标覆盖位姿数（达标自动停止）。
pub const CAPTURE_TARGET: usize = 12;
/// 最小位姿角差（度）：小于该差异视为重复姿态，不入 dataset。
pub const MIN_POSE_ANGLE_DEG: f64 = 10.0;
/// hold 稳定帧数：新姿态需连续保持该帧数才采集（黄 1/4 → 绿 4/4）。
pub const HOLD_TARGET: u8 = 4;
/// hold 期间姿态抖动容忍（度）。
const HOLD_TOLERANCE_DEG: f64 = 3.0;
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
    /// 当前 hold 计数（0 = 未 hold）。
    pub hold: u8,
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
    /// 检测绘制数据（worker 写，GuideOverlay draw 读）。
    overlay: Arc<Mutex<Option<OverlayData>>>,
    /// 引导状态（worker 写，主线程读）。
    guide_state: Arc<Mutex<GuideState>>,
    worker: Option<std::thread::JoinHandle<()>>,
    target: usize,
    detect_started: bool,
    /// RTSP 解码失败信息（pump 检查 completion 写入，worker 读取显示）。
    rtsp_error: Arc<Mutex<Option<String>>>,
}

impl RtspStream {
    /// 启动真实 RTSP 解码（Tcp 传输，低延迟模式）。
    pub fn start(
        host: &str,
        rtsp_port: u16,
        channel: u16,
        width: u32,
        height: u32,
        overlay_slot: Arc<Mutex<Option<OverlayData>>>,
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
        Ok(Self::new(Some(decoder), slot, overlay_slot))
    }

    /// 合成测试帧模式（无板验证纹理与引导链路）；图案为非棋盘噪点。
    pub fn start_synth(channel: u16, overlay_slot: Arc<Mutex<Option<OverlayData>>>) -> Self {
        let slot = Arc::new(LatestDecodedFrameSlot::default());
        let worker_slot = Arc::clone(&slot);
        std::thread::spawn(move || {
            let (width, height) = (640u32, 360u32);
            let mut seed = u64::from(channel) + 1;
            let board_mode = std::env::var("PONGBOT_SYNTH").is_ok_and(|v| v == "board");
            let mut tick = 0u32;
            loop {
                let mut rgba = Vec::with_capacity((width * height * 4) as usize);
                if board_mode {
                    // 合成棋盘（12x9 格 = 11x8 内角点），随 tick 平移模拟位姿变化。
                    let cell = 32i32;
                    let cols = 12i32;
                    let rows = 9i32;
                    // 姿态变化放慢（每 4 tick 一次 ≈ 1.2s 周期），hold 窗口内保持稳定。
                    let slow = tick / 4;
                    let ox = ((width as i32 - cols * cell) / 2) + ((slow % 60) as i32 - 30);
                    let oy = ((height as i32 - rows * cell) / 2) + ((slow % 40) as i32 - 20);
                    // 错切模拟旋转：改变棋盘几何 → rvec 变化 → 新姿态。
                    let shear = ((slow % 30) as i32 - 15) as f32 / 60.0;
                    for y in 0..height as i32 {
                        for x in 0..width as i32 {
                            let sx = (x as f32 + shear * (y - oy) as f32) as i32;
                            let gx = (sx - ox) / cell;
                            let gy = (y - oy) / cell;
                            let inside = gx >= 0 && gx < cols && gy >= 0 && gy < rows;
                            let white = inside && ((gx + gy) % 2 == 0);
                            let v = if white { 235u8 } else if inside { 30u8 } else { 60u8 };
                            rgba.extend_from_slice(&[v, v, v, 255]);
                        }
                    }
                } else {
                    for _ in 0..width * height {
                        // xorshift 伪随机噪点：无棋盘结构，保证检测失败（验证引导路径）。
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        let v = (seed & 0xff) as u8;
                        let (r, g, b) = (v, v.wrapping_mul(2), 255 - v);
                        rgba.extend_from_slice(&[r, g, b, 255]);
                    }
                }
                tick = tick.wrapping_add(1);
                worker_slot.publish(DecodedVideoFrame {
                    width,
                    height,
                    rgba: rgba.into(),
                    identity: StreamFrameIdentity {
                        stream_id: StreamSessionId::new(format!("synth-ch{channel}"))
                            .expect("合成会话 id"),
                        channel,
                        frame_sequence: u64::from(tick),
                        source_pts: SourcePts::Unavailable {
                            reason: "synthetic".to_owned(),
                        },
                        host_monotonic_time_ns: 0,
                        device_timestamp_ns: Some(u64::from(tick)),
                    },
                });
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Self::new(None, slot, overlay_slot)
    }

    fn new(
        decoder: Option<FfmpegRtspDecoder>,
        slot: Arc<LatestDecodedFrameSlot>,
        overlay_slot: Arc<Mutex<Option<OverlayData>>>,
    ) -> Self {
        Self {
            decoder,
            slot,
            last: None,
            texture: None,
            capturing: Arc::new(AtomicBool::new(false)),
            captured: Arc::new(Mutex::new(Vec::with_capacity(CAPTURE_TARGET))),
            poses: Arc::new(Mutex::new(Vec::with_capacity(CAPTURE_TARGET))),
            overlay: overlay_slot,
            guide_state: Arc::new(Mutex::new(GuideState::default())),
            worker: None,
            target: CAPTURE_TARGET,
            detect_started: false,
            rtsp_error: Arc::new(Mutex::new(None)),
        }
    }

    /// 主线程调用：有且仅在上传新帧时返回 `true`。
    pub fn pump(&mut self, target: &mut TextureRect) -> bool {
        // 检查解码器终态：连接/解码失败时记录，供引导显示。
        if let Some(decoder) = self.decoder.as_ref() {
            if let Some(Err(error)) = decoder.completion() {
                if let Ok(mut slot) = self.rtsp_error.lock() {
                    *slot = Some(error.clone());
                }
            }
        }
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

    /// 切换 guided 采集（检测 worker 常驻，仅切换 hold/采集阶段）。
    pub fn toggle_capture(&mut self, _board: BoardSpec) -> bool {
        let on = !self.capturing.load(Ordering::Acquire);
        self.capturing.store(on, Ordering::Release);
        if on {
            // 采集开始：重置 hold（worker 检测到 capturing 翻转会重置）。
        }
        on
    }

    /// 启动常驻检测 worker（预览开启即调用；持续检测 + 更新 overlay/引导）。
    pub fn start_detect(&mut self, board: BoardSpec) {
        if self.detect_started {
            return;
        }
        self.detect_started = true;
        let capturing = Arc::clone(&self.capturing);
        let slot = Arc::clone(&self.slot);
        let captured = Arc::clone(&self.captured);
        let poses = Arc::clone(&self.poses);
        let overlay = Arc::clone(&self.overlay);
        let guide = Arc::clone(&self.guide_state);
        let rtsp_error = Arc::clone(&self.rtsp_error);
        let target = self.target;
        std::thread::spawn(move || {
            guided_capture_loop(
                capturing, slot, captured, poses, guide, overlay, rtsp_error, board, target,
            );
        });
    }

    /// 检测绘制数据槽（GuideOverlay attach 用）。
    pub fn overlay_slot(&self) -> Arc<Mutex<Option<OverlayData>>> {
        Arc::clone(&self.overlay)
    }

    /// 读取引导状态（主线程）。
    pub fn guide(&self) -> (String, usize, u8) {
        let state = self.guide_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.text.clone(), state.captured_count, state.hold)
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
    overlay: Arc<Mutex<Option<OverlayData>>>,
    rtsp_error: Arc<Mutex<Option<String>>>,
    board: BoardSpec,
    target: usize,
) {
    let backend = OpenCvCalibrationBackend;
    let cancellation = CalibrationCancellation::default();
    let mut hold_pose: Option<[f64; 3]> = None;
    let mut hold_count: u8 = 0;
    godot_print!("检测 worker 已启动（目标 {target} 位姿，hold {HOLD_TARGET} 帧）");
    loop {
        let count = {
            let poses = poses.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            poses.len()
        };
        if count >= target {
            set_guide(&guide, &format!("已覆盖 {count}/{target} 位姿，采集完成"), count, 0);
            break;
        }
        let Some(frame) = slot.latest() else {
            // 无帧：显示 RTSP 连接/解码状态（不再静默空转）。
            let error = rtsp_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(error) = error {
                set_guide(&guide, &format!("RTSP 无帧：{error}（检查板端 DEMO233）"), 0, 0);
            } else {
                set_guide(&guide, "等待 RTSP 帧…", 0, 0);
            }
            std::thread::sleep(DETECT_INTERVAL);
            continue;
        };
        // 降采样检测：1080p 下棋盘格子易过大导致 findChessboardCorners 失败，
        // 缩放到最大边 960px 后检测更鲁棒；角点坐标映射回原图。
        let (detect_rgba, detect_w, detect_h, detect_scale) = resize_for_detect(&frame.rgba, frame.width, frame.height);
        let png = match encode_png(&detect_rgba, detect_w, detect_h) {
            Ok(png) => png,
            Err(_) => {
                std::thread::sleep(DETECT_INTERVAL);
                continue;
            }
        };
        let expected = CalibrationImageSize {
            width: detect_w,
            height: detect_h,
        };
        match backend.detect_png(&png, expected, 256 * 1024 * 1024, board, &cancellation) {
            Ok(ChessboardDetectionOutcome::Found(detection)) => {
                // 角点映射回原图坐标（检测/绘制/姿态估计统一用原图）。
                let scaled_corners: Vec<CalibrationPoint> = detection
                    .corners
                    .iter()
                    .map(|c| CalibrationPoint {
                        x: c.x / detect_scale,
                        y: c.y / detect_scale,
                    })
                    .collect();
                let detection = ChessboardDetection {
                    image_size: CalibrationImageSize {
                        width: frame.width,
                        height: frame.height,
                    },
                    corners: scaled_corners,
                };
                // 初始内参随帧尺寸生成（1080p 主点不再是 640x360 默认）。
                let initial = default_initial_intrinsics(frame.width, frame.height);
                let pose = match backend.estimate_pose(&detection, &initial, board, &cancellation)
                {
                    Ok(view) => view.rotation_vector,
                    Err(_) => {
                        set_guide(&guide, "检测到棋盘，姿态估计失败，请稍作调整", count, 0);
                        std::thread::sleep(DETECT_INTERVAL);
                        continue;
                    }
                };
                // 回传绘制数据（角点 + 姿态）。
                if let Ok(mut slot) = overlay.lock() {
                    *slot = Some(OverlayData {
                        found: true,
                        corners: detection.corners.iter().map(|p| (p.x, p.y)).collect(),
                        image_width: frame.width as f32,
                        image_height: frame.height as f32,
                        rotation_deg: (
                            pose[0].to_degrees() as f32,
                            pose[1].to_degrees() as f32,
                            pose[2].to_degrees() as f32,
                        ),
                    });
                }
                // hold 状态机：与当前 hold 姿态比较，稳定保持 HOLD_TARGET 帧才采集。
                if !capturing.load(Ordering::Acquire) {
                    // 预览阶段：只显示检测状态，不进入 hold/采集。
                    set_guide(&guide, "棋盘已检测 ✓（点「开始采集」进入引导）", 0, 0);
                    std::thread::sleep(DETECT_INTERVAL);
                    continue;
                }
                let mut poses = poses.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let is_new = poses
                    .iter()
                    .all(|existing| pose_angle_deg(existing, &pose) >= MIN_POSE_ANGLE_DEG);
                let hold_matches = hold_pose
                    .as_ref()
                    .is_some_and(|hp| pose_angle_deg(hp, &pose) <= HOLD_TOLERANCE_DEG);
                if hold_matches {
                    hold_count += 1;
                    if hold_count >= HOLD_TARGET {
                        // hold 达标：采集（仅新姿态），黄 → 绿。
                        hold_pose = None;
                        hold_count = 0;
                        if is_new {
                            poses.push(pose);
                            let mut captured =
                                captured.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            captured.push(frame);
                            drop(captured);
                            let count = poses.len();
                            drop(poses);
                            godot_print!("hold 达标采集：已采 {count}/{target}");
                            if count >= target {
                                set_guide(
                                    &guide,
                                    &format!("已覆盖 {count}/{target} 位姿，采集完成"),
                                    count,
                                    0,
                                );
                            } else {
                                set_guide(
                                    &guide,
                                    &format!("已采 {count}/{target} 位姿（绿 4/4），请变换棋盘姿态"),
                                    count,
                                    0,
                                );
                            }
                        } else {
                            drop(poses);
                            set_guide(&guide, "姿态重复，请变换棋盘位姿", count, 0);
                        }
                    } else {
                        // 黄 N/4：保持中。
                        godot_print!("hold 推进：{hold_count}/{HOLD_TARGET}");
                        drop(poses);
                        set_guide(
                            &guide,
                            &format!("保持当前姿态 · {hold_count}/{HOLD_TARGET}"),
                            count,
                            hold_count,
                        );
                    }
                } else {
                    // 姿态变化：开始新一轮 hold。
                    godot_print!("新姿态，hold 1/{HOLD_TARGET}（已采 {}）", count);
                    hold_pose = Some(pose);
                    hold_count = 1;
                    drop(poses);
                    set_guide(
                        &guide,
                        &format!("检测到新姿态 · 1/{HOLD_TARGET}（请保持稳定）"),
                        count,
                        1,
                    );
                }
            }
            Ok(ChessboardDetectionOutcome::NotFound { .. }) => {
                hold_pose = None;
                hold_count = 0;
                set_guide(&guide, "未检测到棋盘，请将棋盘置于画面中并调整角度", count, 0);
            }
            Err(error) => {
                godot_print!("检测出错：{error}");
                hold_pose = None;
                hold_count = 0;
                set_guide(&guide, &format!("检测失败：{error}"), count, 0);
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

fn set_guide(guide: &Arc<Mutex<GuideState>>, text: &str, count: usize, hold: u8) {
    let mut state = guide.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    state.text = text.to_owned();
    state.captured_count = count;
    state.hold = hold;
}

/// 默认初始内参（fx=fy=900，主点按帧尺寸居中）。
fn default_initial_intrinsics(width: u32, height: u32) -> InitialIntrinsics {
    InitialIntrinsics {
        camera_matrix: [
            900.0,
            0.0,
            f64::from(width) / 2.0,
            0.0,
            900.0,
            f64::from(height) / 2.0,
            0.0,
            0.0,
            1.0,
        ],
        distortion_coefficients: vec![0.0, 0.0, 0.0, 0.0, 0.0],
    }
}

/// 降采样到最大边 960px（超过才缩）；返回 (缩后 RGBA, w, h, 缩放比例)。
fn resize_for_detect(rgba: &[u8], width: u32, height: u32) -> (Vec<u8>, u32, u32, f32) {
    const MAX_DETECT_WIDTH: u32 = 960;
    if width <= MAX_DETECT_WIDTH {
        return (rgba.to_vec(), width, height, 1.0);
    }
    let scale = MAX_DETECT_WIDTH as f32 / width as f32;
    let new_height = ((height as f32) * scale).round().max(1.0) as u32;
    let img = match image::RgbaImage::from_raw(width, height, rgba.to_vec()) {
        Some(img) => img,
        None => return (rgba.to_vec(), width, height, 1.0),
    };
    let resized = image::imageops::resize(
        &img,
        MAX_DETECT_WIDTH,
        new_height,
        image::imageops::FilterType::Triangle,
    );
    (resized.into_raw(), MAX_DETECT_WIDTH, new_height, scale)
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
    pub fn start(
        host: &str,
        ch0_slot: Arc<Mutex<Option<OverlayData>>>,
        ch3_slot: Arc<Mutex<Option<OverlayData>>>,
    ) -> Self {
        let synth = std::env::var("PONGBOT_SYNTH").is_ok_and(|v| v == "1" || v == "board");
        if synth {
            Self {
                ch0: RtspStream::start_synth(0, ch0_slot),
                ch3: RtspStream::start_synth(3, ch3_slot),
            }
        } else {
            let ch0 = RtspStream::start(host, 554, 0, 1920, 1080, ch0_slot.clone())
                .unwrap_or_else(|error| {
                    godot_print!("CH0 预览启动失败：{error}");
                    RtspStream::start_synth(0, ch0_slot)
                });
            let ch3 = RtspStream::start(host, 557, 3, 1920, 1080, ch3_slot.clone())
                .unwrap_or_else(|error| {
                    godot_print!("CH3 预览启动失败：{error}");
                    RtspStream::start_synth(3, ch3_slot)
                });
            Self { ch0, ch3 }
        }
    }

    /// 两路是否都达到目标位姿数。
    pub fn both_complete(&self) -> bool {
        self.ch0.complete() && self.ch3.complete()
    }
}

