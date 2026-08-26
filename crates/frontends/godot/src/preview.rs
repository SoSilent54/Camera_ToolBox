//! 双路 RTSP 预览与 guided 自动采集。
//!
//! 预览/引导：RTSP H.264 解码帧用于实时显示、棋盘检测和 guide pose 评估。
//! dataset：真实 X5 采集在 hold 通过后按 RTSP SEI `timestamp_ns` 从 TCP 9073
//! 精确回查同源 NV12/YUV 原图；合成模式仅用于本机 UI 测试，保留 RGBA→luma fallback。
//! 无板验证：`PONGBOT_SYNTH=1` 用非棋盘合成帧（保证检测失败，验证引导路径）。

use crate::guide_overlay::{
    OverlayData, OverlayGridLine, OverlayPoseArrow, OverlayRotationArc, OverlayRotationRings,
    OverlayStatus,
};
use camera_toolbox_adapters::calibration::OpenCvCalibrationBackend;
use camera_toolbox_adapters::media::ffmpeg_rtsp::FfmpegRtspDecoder;
use camera_toolbox_adapters::media::FfmpegRtspTransport;
use camera_toolbox_adapters::x5_tcp_client::{self, X5YuvSnapshot};
use camera_toolbox_app::platform::{
    DecodedVideoFrame, LatestDecodedFrameSlot, RtspLatencyMode, SourcePts, StreamCancellation,
    StreamFrameIdentity, StreamSessionId,
};
use camera_toolbox_app::ports::calibration::{CalibrationBackend, CalibrationCancellation};
use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, ChessboardDetection,
    ChessboardDetectionOutcome, InitialIntrinsics, ViewCalibrationResult,
};
use godot::classes::image::Format;
use godot::classes::{Image, ImageTexture, TextureRect};
use godot::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 每路目标覆盖位姿数：15 张正视 raster + 8 张中距倾斜环绕 + 4 张远距四角。
pub const CAPTURE_TARGET: usize = 27;
/// guide auto_capture 的 hold 稳定帧数（黄 1/3 → 绿 3/3）。
pub const HOLD_TARGET: u8 = 3;
const GUIDED_HOLD_JITTER_XYZ_LIMIT: f64 = 0.025;
const GUIDED_HOLD_JITTER_Z_LIMIT: f64 = 0.04;
const GUIDED_HOLD_JITTER_RPY_DEGREES: f64 = 2.0;
const GUIDED_POSE_X_TOLERANCE: f64 = 0.10;
const GUIDED_POSE_Y_TOLERANCE: f64 = 0.10;
const GUIDED_POSE_Z_TOLERANCE: f64 = 0.24;
const GUIDED_POSE_ROLL_TOLERANCE_DEGREES: f64 = 10.0;
const GUIDED_POSE_PITCH_TOLERANCE_DEGREES: f64 = 10.0;
const GUIDED_POSE_YAW_TOLERANCE_DEGREES: f64 = 15.0;
const GUIDED_POSE_MATCH_SCORE_LIMIT: f64 = 1.0;
const GUIDED_POSE_OVERLAY_DEPTH_SOLVE_ITERS: usize = 12;
const GUIDED_POSE_RING_SEGMENTS: usize = 96;
const GUIDED_POSE_HALF_RING_SEGMENTS: usize = 48;
const GUIDED_POSE_RING_SMALL_ERROR_GAIN: f32 = 3.0;
const GUIDED_POSE_RING_SMALL_ERROR_DECAY_DEGREES: f32 = 8.0;
/// 采集 worker 检测节拍。
const DETECT_INTERVAL: Duration = Duration::from_millis(150);

/// X5 TCP 控制端口；RTSP 只做引导，dataset 通过该端口抓同源 NV12 原图。
const X5_TCP_CONTROL_PORT: u16 = 9073;

/// 进入标定 dataset 的权威帧：真实设备为 TCP NV12 的 Y plane，合成模式为 RGBA 转 luma。
#[derive(Clone, Debug, PartialEq)]
pub struct CapturedDatasetFrame {
    pub channel: u16,
    pub width: u32,
    pub height: u32,
    pub luma: Arc<[u8]>,
    pub source: CapturedDatasetSource,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CapturedDatasetSource {
    X5TcpYuv { frame_id: u64, timestamp_ns: u64 },
    SyntheticRgba { frame_sequence: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DatasetCaptureSource {
    X5TcpYuv {
        host: String,
        tcp_port: u16,
        channel: u16,
    },
    Synthetic {
        channel: u16,
    },
}

/// worker → 主线程的引导状态。
#[derive(Default)]
pub struct GuideState {
    /// overlay 引导文本（未检测到棋盘 / 姿态重复 / 已覆盖 N 位姿）。
    pub text: String,
    /// 已采位姿数。
    pub captured_count: usize,
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
    /// 已采 dataset 帧（真实设备为 TCP NV12/Y plane；worker 写，solve 读）。
    captured: Arc<Mutex<Vec<Arc<CapturedDatasetFrame>>>>,
    /// 已采姿态（rvec，与 captured 一一对应）。
    poses: Arc<Mutex<Vec<[f64; 3]>>>,
    /// 检测绘制数据（worker 写，GuideOverlay draw 读）。
    overlay: Arc<Mutex<Option<OverlayData>>>,
    /// 引导状态（worker 写，主线程读）。
    guide_state: Arc<Mutex<GuideState>>,
    target: usize,
    detect_started: bool,
    /// RTSP 解码失败信息（pump 检查 completion 写入，worker 读取显示）。
    rtsp_error: Arc<Mutex<Option<String>>>,
    /// dataset 的权威抓帧来源；真实设备必须走 TCP YUV，RTSP 不直接入库。
    capture_source: DatasetCaptureSource,
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
        let capture_source = DatasetCaptureSource::X5TcpYuv {
            host: host.to_owned(),
            tcp_port: X5_TCP_CONTROL_PORT,
            channel,
        };
        Ok(Self::new(Some(decoder), slot, overlay_slot, capture_source))
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
                            let v = if white {
                                235u8
                            } else if inside {
                                30u8
                            } else {
                                60u8
                            };
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
        Self::new(
            None,
            slot,
            overlay_slot,
            DatasetCaptureSource::Synthetic { channel },
        )
    }

    fn new(
        decoder: Option<FfmpegRtspDecoder>,
        slot: Arc<LatestDecodedFrameSlot>,
        overlay_slot: Arc<Mutex<Option<OverlayData>>>,
        capture_source: DatasetCaptureSource,
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
            target: CAPTURE_TARGET,
            detect_started: false,
            rtsp_error: Arc::new(Mutex::new(None)),
            capture_source,
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
        if self
            .last
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, &frame))
        {
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

    /// 启动 guide auto_capture（RTSP 预览连接后自动开启）。
    pub fn start_capture(&self) {
        self.capturing.store(true, Ordering::Release);
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
        let capture_source = self.capture_source.clone();
        let target = self.target;
        std::thread::spawn(move || {
            guided_capture_loop(
                capturing,
                slot,
                captured,
                poses,
                guide,
                overlay,
                rtsp_error,
                capture_source,
                board,
                target,
            );
        });
    }

    /// 读取引导状态（主线程）。
    pub fn guide(&self) -> (String, usize, u8) {
        let state = self
            .guide_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.text.clone(), state.captured_count, state.hold)
    }

    /// 是否达到目标位姿数。
    pub fn complete(&self) -> bool {
        let poses = self
            .poses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        poses.len() >= self.target
    }

    /// 取已采 dataset 帧（solve 用；真实设备为 TCP NV12/Y plane）。
    pub fn captured_frames(&self) -> Vec<Arc<CapturedDatasetFrame>> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct GuidedPoseTolerance {
    x: f64,
    y: f64,
    z: f64,
    roll_degrees: f64,
    pitch_degrees: f64,
    yaw_degrees: f64,
}

impl Default for GuidedPoseTolerance {
    fn default() -> Self {
        Self {
            x: GUIDED_POSE_X_TOLERANCE,
            y: GUIDED_POSE_Y_TOLERANCE,
            z: GUIDED_POSE_Z_TOLERANCE,
            roll_degrees: GUIDED_POSE_ROLL_TOLERANCE_DEGREES,
            pitch_degrees: GUIDED_POSE_PITCH_TOLERANCE_DEGREES,
            yaw_degrees: GUIDED_POSE_YAW_TOLERANCE_DEGREES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct GuidedPose6Dof {
    /// 棋盘中心在相机坐标系下的 XYZ；单位继承 BoardSpec::square_size。
    xyz: [f64; 3],
    /// board->camera 旋转矩阵按 ZYX 分解得到的 roll/pitch/yaw，单位 degree。
    rpy_degrees: [f64; 3],
    rotation: [[f64; 3]; 3],
    translation: [f64; 3],
    center_uv: [f32; 2],
}

#[derive(Clone, Debug, PartialEq)]
struct GuidedPoseTarget {
    label: &'static str,
    pose: GuidedPose6Dof,
    tolerance: GuidedPoseTolerance,
    outline_uv: [[f32; 2]; 4],
    grid_lines: Vec<OverlayGridLine>,
}

#[derive(Clone, Debug, PartialEq)]
struct GuidedPoseMeasurement {
    pose: GuidedPose6Dof,
    board: BoardSpec,
    initial_intrinsics: InitialIntrinsics,
    image_size: CalibrationImageSize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct GuidedPoseError {
    x: f64,
    y: f64,
    z: f64,
    roll_degrees: f64,
    pitch_degrees: f64,
    yaw_degrees: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct GuidedPoseAssessment {
    step_index: usize,
    target_label: &'static str,
    measurement: GuidedPoseMeasurement,
    error: GuidedPoseError,
    signed_rotation_error_degrees: [f64; 3],
    pose_error_score: f64,
    matched: bool,
    reason: Option<String>,
}

struct GuidedHoldSample {
    frame: Arc<DecodedVideoFrame>,
    pose_vector: [f64; 3],
    stability_score: f64,
}

struct GuidedCaptureRuntime {
    plan: Vec<GuidedPoseTarget>,
    current_step: usize,
    hold_frames: u8,
    last_hold_measurement: Option<GuidedPoseMeasurement>,
    best_hold_sample: Option<GuidedHoldSample>,
}

impl GuidedCaptureRuntime {
    fn standard_27(
        board: BoardSpec,
        initial_intrinsics: &InitialIntrinsics,
        image_size: CalibrationImageSize,
    ) -> Result<Self, String> {
        Ok(Self {
            plan: standard_guided_pose_plan(board, initial_intrinsics, image_size)?,
            current_step: 0,
            hold_frames: 0,
            last_hold_measurement: None,
            best_hold_sample: None,
        })
    }

    fn current_target(&self) -> Option<&GuidedPoseTarget> {
        self.plan.get(self.current_step)
    }

    fn is_complete(&self) -> bool {
        self.current_step >= self.plan.len()
    }

    fn current_step_label(&self) -> String {
        match self.current_target() {
            Some(target) => format!(
                "动作 {} / {} · {}",
                self.current_step + 1,
                self.plan.len(),
                target.label
            ),
            None => "guide auto_capture 完成".to_owned(),
        }
    }

    fn update_hold(
        &mut self,
        mut assessment: GuidedPoseAssessment,
        sample: GuidedHoldSample,
    ) -> Option<GuidedHoldSample> {
        if assessment.matched {
            let jitter_score = self.last_hold_measurement.as_ref().map_or(0.0, |previous| {
                guided_hold_jitter_score(previous, &assessment.measurement)
            });
            if jitter_score > 1.0 {
                assessment.matched = false;
                assessment.reason = Some(format!(
                    "hold jitter {:.2} exceeds stability limit",
                    jitter_score
                ));
                self.reset_hold();
                return None;
            }
            self.hold_frames = self.hold_frames.saturating_add(1).min(HOLD_TARGET);
            self.last_hold_measurement = Some(assessment.measurement.clone());
            let mut sample = sample;
            sample.stability_score = sample
                .stability_score
                .min(assessment.pose_error_score + jitter_score);
            let replace_best = self
                .best_hold_sample
                .as_ref()
                .is_none_or(|best| sample.stability_score < best.stability_score);
            if replace_best {
                self.best_hold_sample = Some(sample);
            }
            if self.hold_frames >= HOLD_TARGET {
                return self.best_hold_sample.take();
            }
        } else {
            self.reset_hold();
        }
        None
    }

    fn advance_after_commit(&mut self) {
        self.current_step = self.current_step.saturating_add(1);
        self.reset_hold();
    }

    fn reset_hold(&mut self) {
        self.hold_frames = 0;
        self.last_hold_measurement = None;
        self.best_hold_sample = None;
    }
}

/// guided 采集循环（worker 线程）：投影目标框 → 检测 → 姿态误差 → hold 选择最稳帧 → 下个动作。
#[allow(clippy::too_many_arguments)]
fn guided_capture_loop(
    capturing: Arc<AtomicBool>,
    slot: Arc<LatestDecodedFrameSlot>,
    captured: Arc<Mutex<Vec<Arc<CapturedDatasetFrame>>>>,
    poses: Arc<Mutex<Vec<[f64; 3]>>>,
    guide: Arc<Mutex<GuideState>>,
    overlay: Arc<Mutex<Option<OverlayData>>>,
    rtsp_error: Arc<Mutex<Option<String>>>,
    capture_source: DatasetCaptureSource,
    board: BoardSpec,
    target: usize,
) {
    let backend = OpenCvCalibrationBackend;
    let cancellation = CalibrationCancellation::default();
    let mut runtime: Option<GuidedCaptureRuntime> = None;
    godot_print!("guide auto_capture worker 已启动（27 动作：15 正视 + 8 倾斜 + 4 远距，hold {HOLD_TARGET} 帧）");
    loop {
        if runtime
            .as_ref()
            .is_some_and(GuidedCaptureRuntime::is_complete)
        {
            let count = runtime.as_ref().map_or(target, |rt| rt.plan.len());
            set_guide(
                &guide,
                &format!("已完成 {count}/{count} guide 动作，采集完成"),
                count,
                0,
            );
            break;
        }
        let Some(frame) = slot.latest() else {
            let error = rtsp_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(error) = error {
                set_guide(
                    &guide,
                    &format!("RTSP 无帧：{error}（检查板端 DEMO233）"),
                    0,
                    0,
                );
            } else {
                set_guide(&guide, "等待 RTSP 帧…", 0, 0);
            }
            std::thread::sleep(DETECT_INTERVAL);
            continue;
        };

        let image_size = CalibrationImageSize {
            width: frame.width,
            height: frame.height,
        };
        let initial = default_initial_intrinsics(frame.width, frame.height);
        if runtime.is_none() {
            match GuidedCaptureRuntime::standard_27(board, &initial, image_size) {
                Ok(rt) => runtime = Some(rt),
                Err(error) => {
                    set_guide(&guide, &format!("guide 目标投影失败：{error}"), 0, 0);
                    std::thread::sleep(DETECT_INTERVAL);
                    continue;
                }
            }
        }
        let rt = runtime.as_mut().expect("runtime initialized");
        let Some(target_pose) = rt.current_target().cloned() else {
            continue;
        };
        publish_target_overlay(
            &overlay,
            frame.width,
            frame.height,
            &target_pose,
            None,
            None,
        );

        let (detect_rgba, detect_w, detect_h, detect_scale) =
            resize_for_detect(&frame.rgba, frame.width, frame.height);
        let png = match encode_png(&detect_rgba, detect_w, detect_h) {
            Ok(png) => png,
            Err(error) => {
                set_guide(
                    &guide,
                    &format!("PNG 编码失败：{error}"),
                    rt.current_step,
                    0,
                );
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
                let corners: Vec<CalibrationPoint> = detection
                    .corners
                    .iter()
                    .map(|c| CalibrationPoint {
                        x: c.x / detect_scale,
                        y: c.y / detect_scale,
                    })
                    .collect();
                let detection = ChessboardDetection {
                    image_size,
                    corners,
                };
                let view = match backend.estimate_pose(&detection, &initial, board, &cancellation) {
                    Ok(view) => view,
                    Err(error) => {
                        set_guide(
                            &guide,
                            &format!("检测到棋盘，姿态估计失败：{error}"),
                            rt.current_step,
                            0,
                        );
                        std::thread::sleep(DETECT_INTERVAL);
                        continue;
                    }
                };
                let assessment = match assess_guided_pose(
                    rt.current_step,
                    &target_pose,
                    &detection,
                    &view,
                    board,
                    &initial,
                    image_size,
                ) {
                    Ok(assessment) => assessment,
                    Err(error) => {
                        set_guide(
                            &guide,
                            &format!("guide 姿态评估失败：{error}"),
                            rt.current_step,
                            0,
                        );
                        std::thread::sleep(DETECT_INTERVAL);
                        continue;
                    }
                };
                let status = guided_pose_status_overlay(&assessment, rt.hold_frames);
                publish_target_overlay(
                    &overlay,
                    frame.width,
                    frame.height,
                    &target_pose,
                    Some(&assessment),
                    Some(status.clone()),
                );

                let step_label = rt.current_step_label();
                if !capturing.load(Ordering::Acquire) {
                    set_guide(
                        &guide,
                        &format!("{step_label} · 等待自动采集启动"),
                        rt.current_step,
                        0,
                    );
                    std::thread::sleep(DETECT_INTERVAL);
                    continue;
                }
                let sample = GuidedHoldSample {
                    frame: frame.clone(),
                    pose_vector: view.rotation_vector,
                    stability_score: assessment.pose_error_score,
                };
                let matched = assessment.matched;
                let reason = assessment.reason.clone();
                let error = assessment.error;
                let captured_sample = rt.update_hold(assessment, sample);
                if let Some(sample) = captured_sample {
                    let dataset_frame = match capture_dataset_frame(&capture_source, &sample.frame)
                    {
                        Ok(frame) => frame,
                        Err(error) => {
                            rt.reset_hold();
                            set_guide(
                                &guide,
                                &format!("{step_label} · TCP YUV 原图抓取失败：{error}"),
                                rt.current_step,
                                0,
                            );
                            std::thread::sleep(DETECT_INTERVAL);
                            continue;
                        }
                    };
                    {
                        let mut captured = captured
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        captured.push(Arc::new(dataset_frame));
                    }
                    {
                        let mut poses = poses
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        poses.push(sample.pose_vector);
                    }
                    rt.advance_after_commit();
                    let done = rt.current_step;
                    if rt.is_complete() {
                        set_guide(
                            &guide,
                            &format!("已完成 {done}/{done} guide 动作，采集完成"),
                            done,
                            0,
                        );
                    } else {
                        set_guide(
                            &guide,
                            &format!(
                                "已保存最稳帧 {done}/{} · 下一动作：{}",
                                rt.plan.len(),
                                rt.current_step_label()
                            ),
                            done,
                            0,
                        );
                    }
                } else if matched {
                    set_guide(
                        &guide,
                        &format!(
                            "{step_label} · 对齐完成，请保持稳定 {}/{}",
                            rt.hold_frames, HOLD_TARGET
                        ),
                        rt.current_step,
                        rt.hold_frames,
                    );
                } else {
                    let reason =
                        reason.unwrap_or_else(|| guided_pose_error_reason(&error, &target_pose));
                    set_guide(
                        &guide,
                        &format!("{step_label} · {reason}"),
                        rt.current_step,
                        0,
                    );
                }
            }
            Ok(ChessboardDetectionOutcome::NotFound { .. }) => {
                rt.reset_hold();
                set_guide(
                    &guide,
                    &format!(
                        "{} · 未检测到棋盘，请把 11×8 / 40mm 棋盘移入黄框",
                        rt.current_step_label()
                    ),
                    rt.current_step,
                    0,
                );
            }
            Err(error) => {
                rt.reset_hold();
                set_guide(&guide, &format!("检测失败：{error}"), rt.current_step, 0);
            }
        }
        std::thread::sleep(DETECT_INTERVAL);
    }
}

fn publish_target_overlay(
    overlay: &Arc<Mutex<Option<OverlayData>>>,
    width: u32,
    height: u32,
    target: &GuidedPoseTarget,
    assessment: Option<&GuidedPoseAssessment>,
    status: Option<OverlayStatus>,
) {
    if let Ok(mut slot) = overlay.lock() {
        *slot = Some(OverlayData {
            image_width: width as f32,
            image_height: height as f32,
            detected_outline_px: assessment
                .map(|assessment| detected_outline_pixels(&assessment.measurement)),
            target_center_uv: Some(target.pose.center_uv),
            target_outline_uv: Some(target.outline_uv),
            target_grid_lines: target.grid_lines.clone(),
            target_matched: assessment.is_some_and(|assessment| assessment.matched),
            rotation_rings: assessment
                .map(|assessment| guided_pose_rotation_rings_overlay(assessment, target)),
            pose_arrow: assessment.map(|assessment| OverlayPoseArrow {
                start_uv: assessment.measurement.pose.center_uv,
                end_uv: target.pose.center_uv,
            }),
            status,
        });
    }
}

fn standard_guided_pose_plan(
    board: BoardSpec,
    initial_intrinsics: &InitialIntrinsics,
    image_size: CalibrationImageSize,
) -> Result<Vec<GuidedPoseTarget>, String> {
    const FRONTO_SCALE: f64 = 0.62;
    const MID_TILT_SCALE: f64 = 0.43;
    const FAR_CORNER_SCALE: f64 = 0.22;
    const MID_TILT_DEGREES: f64 = 32.0;
    let tolerance = GuidedPoseTolerance::default();
    let mut plan = Vec::with_capacity(CAPTURE_TARGET);
    let mut push = |label: &'static str,
                    center_uv: [f64; 2],
                    scale: f64,
                    tilt_degrees: f64,
                    azimuth_degrees: f64|
     -> Result<(), String> {
        let projection = guided_pose_grid_projection(
            board,
            center_uv,
            scale,
            tilt_degrees,
            azimuth_degrees,
            initial_intrinsics,
            image_size,
        )
        .ok_or_else(|| format!("guided target '{label}' cannot be projected with current K/D12"))?;
        plan.push(GuidedPoseTarget {
            label,
            pose: projection.pose,
            tolerance,
            outline_uv: projection.outline_uv,
            grid_lines: projection.grid_lines,
        });
        Ok(())
    };
    // Phase A: 近距正视 3x5 蛇形 raster，先用高像素尺度扫完整画面覆盖。
    push(
        "F01 Fronto upper left",
        [0.33, 0.31],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "F02 Fronto upper mid-left",
        [0.415, 0.31],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "F03 Fronto upper center",
        [0.50, 0.31],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "F04 Fronto upper mid-right",
        [0.585, 0.31],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "F05 Fronto upper right",
        [0.67, 0.31],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "F06 Fronto middle right",
        [0.67, 0.50],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "F07 Fronto middle mid-right",
        [0.585, 0.50],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push("F08 Fronto center", [0.50, 0.50], FRONTO_SCALE, 0.0, 0.0)?;
    push(
        "F09 Fronto middle mid-left",
        [0.415, 0.50],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "F10 Fronto middle left",
        [0.33, 0.50],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "F11 Fronto lower left",
        [0.33, 0.69],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "F12 Fronto lower mid-left",
        [0.415, 0.69],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "F13 Fronto lower center",
        [0.50, 0.69],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "F14 Fronto lower mid-right",
        [0.585, 0.69],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "F15 Fronto lower right",
        [0.67, 0.69],
        FRONTO_SCALE,
        0.0,
        0.0,
    )?;

    // Phase B: 中距 8 点大倾斜圆弧；相机位置小幅移动，主要通过姿态变化获得透视激励。
    push(
        "T01 Tilt lower right",
        [0.60, 0.61],
        MID_TILT_SCALE,
        MID_TILT_DEGREES,
        315.0,
    )?;
    push(
        "T02 Tilt right",
        [0.63, 0.50],
        MID_TILT_SCALE,
        MID_TILT_DEGREES,
        0.0,
    )?;
    push(
        "T03 Tilt upper right",
        [0.60, 0.39],
        MID_TILT_SCALE,
        MID_TILT_DEGREES,
        45.0,
    )?;
    push(
        "T04 Tilt top",
        [0.50, 0.36],
        MID_TILT_SCALE,
        MID_TILT_DEGREES,
        90.0,
    )?;
    push(
        "T05 Tilt upper left",
        [0.40, 0.39],
        MID_TILT_SCALE,
        MID_TILT_DEGREES,
        135.0,
    )?;
    push(
        "T06 Tilt left",
        [0.37, 0.50],
        MID_TILT_SCALE,
        MID_TILT_DEGREES,
        180.0,
    )?;
    push(
        "T07 Tilt lower left",
        [0.40, 0.61],
        MID_TILT_SCALE,
        MID_TILT_DEGREES,
        225.0,
    )?;
    push(
        "T08 Tilt bottom",
        [0.50, 0.64],
        MID_TILT_SCALE,
        MID_TILT_DEGREES,
        270.0,
    )?;

    // Phase C: 远距四角 off-axis；保持相机中心在圆柱空间内，靠朝向把小棋盘送到四角。
    push(
        "C01 Far lower right",
        [0.86, 0.82],
        FAR_CORNER_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "C02 Far upper right",
        [0.86, 0.18],
        FAR_CORNER_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "C03 Far upper left",
        [0.14, 0.18],
        FAR_CORNER_SCALE,
        0.0,
        0.0,
    )?;
    push(
        "C04 Far lower left",
        [0.14, 0.82],
        FAR_CORNER_SCALE,
        0.0,
        0.0,
    )?;
    Ok(plan)
}

struct GuidedPoseGridProjection {
    pose: GuidedPose6Dof,
    outline_uv: [[f32; 2]; 4],
    grid_lines: Vec<OverlayGridLine>,
}

fn guided_pose_grid_projection(
    board: BoardSpec,
    center_uv: [f64; 2],
    scale: f64,
    tilt_degrees: f64,
    azimuth_degrees: f64,
    initial_intrinsics: &InitialIntrinsics,
    image_size: CalibrationImageSize,
) -> Option<GuidedPoseGridProjection> {
    if center_uv.iter().any(|value| !value.is_finite()) || !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let rotation = guided_pose_rotation(tilt_degrees, azimuth_degrees);
    let translation = guided_pose_target_translation(
        board,
        center_uv,
        scale,
        rotation,
        initial_intrinsics,
        image_size,
    )?;
    let left = -1.0;
    let top = -1.0;
    let right = f64::from(board.inner_cols);
    let bottom = f64::from(board.inner_rows);
    let mut grid_lines = Vec::with_capacity(usize::from(board.inner_cols + board.inner_rows) + 4);
    for column in 0..=usize::from(board.inner_cols) + 1 {
        let x = column as f64 - 1.0;
        grid_lines.push(OverlayGridLine {
            start_uv: guided_pose_project_board_uv(
                board,
                rotation,
                translation,
                x,
                top,
                initial_intrinsics,
                image_size,
            )?,
            end_uv: guided_pose_project_board_uv(
                board,
                rotation,
                translation,
                x,
                bottom,
                initial_intrinsics,
                image_size,
            )?,
        });
    }
    for row in 0..=usize::from(board.inner_rows) + 1 {
        let y = row as f64 - 1.0;
        grid_lines.push(OverlayGridLine {
            start_uv: guided_pose_project_board_uv(
                board,
                rotation,
                translation,
                left,
                y,
                initial_intrinsics,
                image_size,
            )?,
            end_uv: guided_pose_project_board_uv(
                board,
                rotation,
                translation,
                right,
                y,
                initial_intrinsics,
                image_size,
            )?,
        });
    }
    let pose = guided_pose_6dof_from_rotation_translation(
        board,
        rotation,
        translation,
        initial_intrinsics,
        image_size,
    )?;
    Some(GuidedPoseGridProjection {
        pose,
        outline_uv: [
            guided_pose_project_board_uv(
                board,
                rotation,
                translation,
                left,
                top,
                initial_intrinsics,
                image_size,
            )?,
            guided_pose_project_board_uv(
                board,
                rotation,
                translation,
                right,
                top,
                initial_intrinsics,
                image_size,
            )?,
            guided_pose_project_board_uv(
                board,
                rotation,
                translation,
                right,
                bottom,
                initial_intrinsics,
                image_size,
            )?,
            guided_pose_project_board_uv(
                board,
                rotation,
                translation,
                left,
                bottom,
                initial_intrinsics,
                image_size,
            )?,
        ],
        grid_lines,
    })
}

fn guided_pose_target_translation(
    board: BoardSpec,
    center_uv: [f64; 2],
    target_scale: f64,
    rotation: [[f64; 3]; 3],
    initial_intrinsics: &InitialIntrinsics,
    image_size: CalibrationImageSize,
) -> Option<[f64; 3]> {
    let target_pixel = [
        center_uv[0] * f64::from(image_size.width),
        center_uv[1] * f64::from(image_size.height),
    ];
    let center_ray = undistort_image_pixel_to_normalized(target_pixel, initial_intrinsics)?;
    let inner_center = guided_pose_inner_center_point(board);
    let rotated_center = rotate_guided_pose_point(rotation, inner_center);
    let minimum_depth = guided_pose_minimum_center_depth(board, rotation, inner_center);
    let mut center_depth =
        guided_pose_initial_center_depth(board, target_scale, initial_intrinsics, image_size)?
            .max(minimum_depth);
    let mut last_translation = None;
    for _ in 0..GUIDED_POSE_OVERLAY_DEPTH_SOLVE_ITERS {
        let translation = guided_pose_translation_at_depth(
            board,
            rotation,
            rotated_center,
            center_ray,
            target_pixel,
            center_depth,
            initial_intrinsics,
        )?;
        let current_scale = guided_pose_projected_inner_scale(
            board,
            rotation,
            translation,
            initial_intrinsics,
            image_size,
        )?;
        last_translation = Some(translation);
        let scale_ratio = current_scale / target_scale;
        if !scale_ratio.is_finite() || scale_ratio <= 0.0 {
            return last_translation;
        }
        if (current_scale - target_scale).abs() <= target_scale * 1.0e-4 {
            return last_translation;
        }
        let next_depth = (center_depth * scale_ratio).max(minimum_depth);
        if (next_depth - center_depth).abs() <= center_depth * 1.0e-5 {
            return last_translation;
        }
        center_depth = next_depth;
    }
    last_translation
}

fn guided_pose_initial_center_depth(
    board: BoardSpec,
    target_scale: f64,
    initial_intrinsics: &InitialIntrinsics,
    image_size: CalibrationImageSize,
) -> Option<f64> {
    if !target_scale.is_finite() || target_scale <= 0.0 {
        return None;
    }
    let short_side = f64::from(image_size.width.min(image_size.height));
    let inner_width = f64::from(board.inner_cols.saturating_sub(1)) * board.square_size;
    let inner_height = f64::from(board.inner_rows.saturating_sub(1)) * board.square_size;
    let matrix = initial_intrinsics.camera_matrix;
    let depth =
        (inner_width * matrix[0]).max(inner_height * matrix[4]) / (target_scale * short_side);
    depth
        .is_finite()
        .then_some(depth.max(board.square_size.max(1.0)))
}

fn guided_pose_translation_at_depth(
    board: BoardSpec,
    rotation: [[f64; 3]; 3],
    rotated_center: [f64; 3],
    center_ray: [f64; 2],
    target_pixel: [f64; 2],
    center_depth: f64,
    initial_intrinsics: &InitialIntrinsics,
) -> Option<[f64; 3]> {
    if !center_depth.is_finite() || center_depth <= 0.0 {
        return None;
    }
    let matrix = initial_intrinsics.camera_matrix;
    let mut translation = [
        center_ray[0] * center_depth - rotated_center[0],
        center_ray[1] * center_depth - rotated_center[1],
        center_depth - rotated_center[2],
    ];
    for _ in 0..8 {
        let (minimum, maximum) = guided_pose_projected_inner_pixel_bounds(
            board,
            rotation,
            translation,
            initial_intrinsics,
        )?;
        let current_center = [
            (minimum[0] + maximum[0]) * 0.5,
            (minimum[1] + maximum[1]) * 0.5,
        ];
        let error = [
            target_pixel[0] - current_center[0],
            target_pixel[1] - current_center[1],
        ];
        if error[0].abs().max(error[1].abs()) <= 1.0e-3 {
            break;
        }
        translation[0] += error[0] / matrix[0] * center_depth;
        translation[1] += error[1] / matrix[4] * center_depth;
        if translation.iter().any(|value| !value.is_finite()) {
            return None;
        }
    }
    Some(translation)
}

fn guided_pose_projected_inner_scale(
    board: BoardSpec,
    rotation: [[f64; 3]; 3],
    translation: [f64; 3],
    initial_intrinsics: &InitialIntrinsics,
    image_size: CalibrationImageSize,
) -> Option<f64> {
    let (minimum, maximum) =
        guided_pose_projected_inner_pixel_bounds(board, rotation, translation, initial_intrinsics)?;
    let short_side = f64::from(image_size.width.min(image_size.height));
    let scale = (maximum[0] - minimum[0]).max(maximum[1] - minimum[1]) / short_side;
    scale.is_finite().then_some(scale)
}

fn guided_pose_projected_inner_pixel_bounds(
    board: BoardSpec,
    rotation: [[f64; 3]; 3],
    translation: [f64; 3],
    initial_intrinsics: &InitialIntrinsics,
) -> Option<([f64; 2], [f64; 2])> {
    let right = f64::from(board.inner_cols.saturating_sub(1));
    let bottom = f64::from(board.inner_rows.saturating_sub(1));
    let corners = [[0.0, 0.0], [right, 0.0], [right, bottom], [0.0, bottom]];
    let mut minimum = [f64::INFINITY, f64::INFINITY];
    let mut maximum = [f64::NEG_INFINITY, f64::NEG_INFINITY];
    for [x, y] in corners {
        let point = project_board_point_image(
            rotation,
            translation,
            guided_pose_board_point(board, x, y),
            initial_intrinsics,
        )?;
        let image = [f64::from(point.x), f64::from(point.y)];
        minimum[0] = minimum[0].min(image[0]);
        minimum[1] = minimum[1].min(image[1]);
        maximum[0] = maximum[0].max(image[0]);
        maximum[1] = maximum[1].max(image[1]);
    }
    Some((minimum, maximum))
}

fn guided_pose_minimum_center_depth(
    board: BoardSpec,
    rotation: [[f64; 3]; 3],
    inner_center: [f64; 3],
) -> f64 {
    let center_z = rotate_guided_pose_point(rotation, inner_center)[2];
    let right = f64::from(board.inner_cols);
    let bottom = f64::from(board.inner_rows);
    let outline = [[-1.0, -1.0], [right, -1.0], [right, bottom], [-1.0, bottom]];
    let min_delta = outline.iter().fold(f64::INFINITY, |minimum, [x, y]| {
        let z = rotate_guided_pose_point(rotation, guided_pose_board_point(board, *x, *y))[2];
        minimum.min(z - center_z)
    });
    let margin = board.square_size.max(1.0) * 0.05;
    if min_delta < 0.0 {
        -min_delta + margin
    } else {
        margin
    }
}

fn guided_pose_inner_center_point(board: BoardSpec) -> [f64; 3] {
    guided_pose_board_point(
        board,
        f64::from(board.inner_cols.saturating_sub(1)) * 0.5,
        f64::from(board.inner_rows.saturating_sub(1)) * 0.5,
    )
}

fn guided_pose_board_point(board: BoardSpec, x: f64, y: f64) -> [f64; 3] {
    [x * board.square_size, y * board.square_size, 0.0]
}

fn guided_pose_project_board_uv(
    board: BoardSpec,
    rotation: [[f64; 3]; 3],
    translation: [f64; 3],
    x: f64,
    y: f64,
    initial_intrinsics: &InitialIntrinsics,
    image_size: CalibrationImageSize,
) -> Option<[f32; 2]> {
    let point = project_board_point_image(
        rotation,
        translation,
        guided_pose_board_point(board, x, y),
        initial_intrinsics,
    )?;
    Some([
        point.x / image_size.width as f32,
        point.y / image_size.height as f32,
    ])
}

fn guided_pose_6dof_from_rotation_translation(
    board: BoardSpec,
    rotation: [[f64; 3]; 3],
    translation: [f64; 3],
    initial_intrinsics: &InitialIntrinsics,
    image_size: CalibrationImageSize,
) -> Option<GuidedPose6Dof> {
    let center_point = guided_pose_inner_center_point(board);
    let rotated_center = rotate_guided_pose_point(rotation, center_point);
    let xyz = [
        rotated_center[0] + translation[0],
        rotated_center[1] + translation[1],
        rotated_center[2] + translation[2],
    ];
    let center_image =
        project_board_point_image(rotation, translation, center_point, initial_intrinsics)?;
    let center_uv = [
        center_image.x / image_size.width as f32,
        center_image.y / image_size.height as f32,
    ];
    let rpy_degrees = guided_pose_rotation_to_rpy_degrees(rotation)?;
    let pose = GuidedPose6Dof {
        xyz,
        rpy_degrees,
        rotation,
        translation,
        center_uv,
    };
    guided_pose_6dof_is_finite(&pose).then_some(pose)
}

fn guided_pose_6dof_is_finite(pose: &GuidedPose6Dof) -> bool {
    pose.xyz.iter().all(|value| value.is_finite())
        && pose.rpy_degrees.iter().all(|value| value.is_finite())
        && pose
            .rotation
            .iter()
            .flatten()
            .all(|value| value.is_finite())
        && pose.translation.iter().all(|value| value.is_finite())
        && pose.center_uv.iter().all(|value| value.is_finite())
}

fn guided_pose_rotation_to_rpy_degrees(rotation: [[f64; 3]; 3]) -> Option<[f64; 3]> {
    if rotation.iter().flatten().any(|value| !value.is_finite()) {
        return None;
    }
    let pitch = (-rotation[2][0]).clamp(-1.0, 1.0).asin();
    let cos_pitch = pitch.cos();
    let (roll, yaw) = if cos_pitch.abs() > 1.0e-9 {
        (
            rotation[2][1].atan2(rotation[2][2]),
            rotation[1][0].atan2(rotation[0][0]),
        )
    } else {
        (0.0, (-rotation[0][1]).atan2(rotation[1][1]))
    };
    let rpy = [roll.to_degrees(), pitch.to_degrees(), yaw.to_degrees()];
    rpy.iter().all(|value| value.is_finite()).then_some(rpy)
}

fn mat3_mul(left: [[f64; 3]; 3], right: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut output = [[0.0; 3]; 3];
    for row in 0..3 {
        for column in 0..3 {
            output[row][column] = left[row][0] * right[0][column]
                + left[row][1] * right[1][column]
                + left[row][2] * right[2][column];
        }
    }
    output
}

fn signed_angle_distance_degrees(left: f64, right: f64) -> f64 {
    let delta = (left - right).rem_euclid(360.0);
    if delta > 180.0 {
        delta - 360.0
    } else {
        delta
    }
}

fn guided_pose_signed_rotation_error_components(
    measurement_rpy_degrees: [f64; 3],
    target_rpy_degrees: [f64; 3],
) -> Option<[f64; 3]> {
    if measurement_rpy_degrees
        .iter()
        .chain(&target_rpy_degrees)
        .any(|value| !value.is_finite())
    {
        return None;
    }
    let raw_zyx = [
        signed_angle_distance_degrees(target_rpy_degrees[0], measurement_rpy_degrees[0]),
        signed_angle_distance_degrees(target_rpy_degrees[1], measurement_rpy_degrees[1]),
        signed_angle_distance_degrees(target_rpy_degrees[2], measurement_rpy_degrees[2]),
    ];
    Some([raw_zyx[2], raw_zyx[0], raw_zyx[1]])
}

fn guided_pose_rotation_error_score(components: [f64; 3], tolerance: GuidedPoseTolerance) -> f64 {
    (components[0].abs() / tolerance.roll_degrees)
        .max(components[1].abs() / tolerance.pitch_degrees)
        .max(components[2].abs() / tolerance.yaw_degrees)
}

fn guided_pose_rotation_error_degrees(
    measurement: &GuidedPose6Dof,
    target: &GuidedPose6Dof,
    tolerance: GuidedPoseTolerance,
) -> Option<[f64; 3]> {
    let direct =
        guided_pose_signed_rotation_error_components(measurement.rpy_degrees, target.rpy_degrees)?;
    let board_half_turn = [[-1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0]];
    let symmetric_measurement = mat3_mul(measurement.rotation, board_half_turn);
    let symmetric_rpy = guided_pose_rotation_to_rpy_degrees(symmetric_measurement)?;
    let symmetric =
        guided_pose_signed_rotation_error_components(symmetric_rpy, target.rpy_degrees)?;
    let direct_score = guided_pose_rotation_error_score(direct, tolerance);
    let symmetric_score = guided_pose_rotation_error_score(symmetric, tolerance);
    Some(if symmetric_score < direct_score {
        symmetric
    } else {
        direct
    })
}

fn undistort_image_pixel_to_normalized(
    pixel: [f64; 2],
    initial_intrinsics: &InitialIntrinsics,
) -> Option<[f64; 2]> {
    let matrix = initial_intrinsics.camera_matrix;
    if matrix[0] <= 0.0 || matrix[4] <= 0.0 || pixel.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let distorted = [
        (pixel[0] - matrix[2]) / matrix[0],
        (pixel[1] - matrix[5]) / matrix[4],
    ];
    if distorted.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let mut undistorted = distorted;
    for _ in 0..12 {
        let projected = distort_normalized_point(
            undistorted[0],
            undistorted[1],
            &initial_intrinsics.distortion_coefficients,
        )?;
        let error = [projected[0] - distorted[0], projected[1] - distorted[1]];
        undistorted[0] -= error[0];
        undistorted[1] -= error[1];
        if error[0].abs().max(error[1].abs()) <= 1.0e-12 {
            break;
        }
    }
    undistorted
        .iter()
        .all(|value| value.is_finite())
        .then_some(undistorted)
}

fn distort_normalized_point(x: f64, y: f64, distortion: &[f64]) -> Option<[f64; 2]> {
    let coefficient = |index: usize| distortion.get(index).copied().unwrap_or(0.0);
    let r2 = x * x + y * y;
    let r4 = r2 * r2;
    let r6 = r4 * r2;
    let numerator = 1.0 + coefficient(0) * r2 + coefficient(1) * r4 + coefficient(4) * r6;
    let denominator = 1.0 + coefficient(5) * r2 + coefficient(6) * r4 + coefficient(7) * r6;
    if !denominator.is_finite() || denominator.abs() <= f64::EPSILON {
        return None;
    }
    let radial = numerator / denominator;
    let distorted = [
        x * radial
            + 2.0 * coefficient(2) * x * y
            + coefficient(3) * (r2 + 2.0 * x * x)
            + coefficient(8) * r2
            + coefficient(9) * r4,
        y * radial
            + coefficient(2) * (r2 + 2.0 * y * y)
            + 2.0 * coefficient(3) * x * y
            + coefficient(10) * r2
            + coefficient(11) * r4,
    ];
    distorted
        .iter()
        .all(|value| value.is_finite())
        .then_some(distorted)
}

fn guided_pose_rotation(tilt_degrees: f64, azimuth_degrees: f64) -> [[f64; 3]; 3] {
    let tilt = tilt_degrees.to_radians();
    if tilt.abs() <= f64::EPSILON {
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    }
    let azimuth = azimuth_degrees.to_radians();
    let axis = [-azimuth.sin(), azimuth.cos(), 0.0];
    let (sin_theta, cos_theta) = tilt.sin_cos();
    let one_minus_cos = 1.0 - cos_theta;
    let [x, y, z] = axis;
    [
        [
            cos_theta + x * x * one_minus_cos,
            x * y * one_minus_cos - z * sin_theta,
            x * z * one_minus_cos + y * sin_theta,
        ],
        [
            y * x * one_minus_cos + z * sin_theta,
            cos_theta + y * y * one_minus_cos,
            y * z * one_minus_cos - x * sin_theta,
        ],
        [
            z * x * one_minus_cos - y * sin_theta,
            z * y * one_minus_cos + x * sin_theta,
            cos_theta + z * z * one_minus_cos,
        ],
    ]
}

fn guided_hold_jitter_score(
    previous: &GuidedPoseMeasurement,
    current: &GuidedPoseMeasurement,
) -> f64 {
    let depth_scale = previous.pose.xyz[2]
        .abs()
        .max(current.pose.xyz[2].abs())
        .max(1.0);
    let xyz_score = ((previous.pose.xyz[0] - current.pose.xyz[0]).abs()
        / depth_scale
        / GUIDED_HOLD_JITTER_XYZ_LIMIT)
        .max(
            (previous.pose.xyz[1] - current.pose.xyz[1]).abs()
                / depth_scale
                / GUIDED_HOLD_JITTER_XYZ_LIMIT,
        )
        .max(
            (previous.pose.xyz[2] - current.pose.xyz[2]).abs()
                / depth_scale
                / GUIDED_HOLD_JITTER_Z_LIMIT,
        );
    let rpy_score = guided_pose_signed_rotation_error_components(
        previous.pose.rpy_degrees,
        current.pose.rpy_degrees,
    )
    .map(|components| {
        components
            .into_iter()
            .map(|component| component.abs() / GUIDED_HOLD_JITTER_RPY_DEGREES)
            .fold(0.0_f64, f64::max)
    })
    .unwrap_or(f64::INFINITY);
    xyz_score.max(rpy_score)
}

fn rotate_guided_pose_point(rotation: [[f64; 3]; 3], point: [f64; 3]) -> [f64; 3] {
    [
        rotation[0][0] * point[0] + rotation[0][1] * point[1] + rotation[0][2] * point[2],
        rotation[1][0] * point[0] + rotation[1][1] * point[1] + rotation[1][2] * point[2],
        rotation[2][0] * point[0] + rotation[2][1] * point[1] + rotation[2][2] * point[2],
    ]
}

fn guided_pose_measurement(
    view: &ViewCalibrationResult,
    board: BoardSpec,
    initial_intrinsics: &InitialIntrinsics,
    image_size: CalibrationImageSize,
) -> Result<GuidedPoseMeasurement, String> {
    let rotation = rodrigues_matrix_for_preview(view.rotation_vector)
        .ok_or_else(|| "guided pose rotation is not finite".to_owned())?;
    let pose = guided_pose_6dof_from_rotation_translation(
        board,
        rotation,
        view.translation_vector,
        initial_intrinsics,
        image_size,
    )
    .ok_or_else(|| "guided pose 6DoF projection is invalid".to_owned())?;
    if pose.xyz[2] <= 0.0 {
        return Err("guided pose measurement contains non-positive depth".to_owned());
    }
    Ok(GuidedPoseMeasurement {
        pose,
        board,
        initial_intrinsics: initial_intrinsics.clone(),
        image_size,
    })
}

fn assess_guided_pose(
    step_index: usize,
    target: &GuidedPoseTarget,
    detection: &ChessboardDetection,
    view: &ViewCalibrationResult,
    board: BoardSpec,
    initial_intrinsics: &InitialIntrinsics,
    image_size: CalibrationImageSize,
) -> Result<GuidedPoseAssessment, String> {
    if detection.corners.is_empty() {
        return Err("guided pose requires detected board corners".to_owned());
    }
    if detection.image_size != image_size {
        return Err("guided pose detection image size does not match target binding".to_owned());
    }
    let measurement = guided_pose_measurement(view, board, initial_intrinsics, image_size)?;
    let depth_scale = target.pose.xyz[2]
        .abs()
        .max(measurement.pose.xyz[2].abs())
        .max(board.square_size.max(1.0));
    let signed_rotation_error_degrees =
        guided_pose_rotation_error_degrees(&measurement.pose, &target.pose, target.tolerance)
            .ok_or_else(|| "guided pose rotation error is not finite".to_owned())?;
    let [signed_roll_degrees, signed_pitch_degrees, signed_yaw_degrees] =
        signed_rotation_error_degrees;
    let error = GuidedPoseError {
        x: (measurement.pose.xyz[0] - target.pose.xyz[0]).abs() / depth_scale,
        y: (measurement.pose.xyz[1] - target.pose.xyz[1]).abs() / depth_scale,
        z: (measurement.pose.xyz[2] - target.pose.xyz[2]).abs() / depth_scale,
        roll_degrees: signed_roll_degrees.abs(),
        pitch_degrees: signed_pitch_degrees.abs(),
        yaw_degrees: signed_yaw_degrees.abs(),
    };
    let pose_error_score = (error.x / target.tolerance.x)
        .max(error.y / target.tolerance.y)
        .max(error.z / target.tolerance.z)
        .max(error.roll_degrees / target.tolerance.roll_degrees)
        .max(error.pitch_degrees / target.tolerance.pitch_degrees)
        .max(error.yaw_degrees / target.tolerance.yaw_degrees);
    if !pose_error_score.is_finite() {
        return Err("guided pose score is not finite".to_owned());
    }
    let matched = pose_error_score <= GUIDED_POSE_MATCH_SCORE_LIMIT;
    let reason = (!matched).then(|| guided_pose_error_reason(&error, target));
    Ok(GuidedPoseAssessment {
        step_index,
        target_label: target.label,
        measurement,
        error,
        signed_rotation_error_degrees,
        pose_error_score,
        matched,
        reason,
    })
}

fn guided_pose_error_reason(error: &GuidedPoseError, target: &GuidedPoseTarget) -> String {
    let values = [
        (
            "横向",
            error.x / target.tolerance.x,
            error.x,
            target.tolerance.x,
        ),
        (
            "纵向",
            error.y / target.tolerance.y,
            error.y,
            target.tolerance.y,
        ),
        (
            "距离",
            error.z / target.tolerance.z,
            error.z,
            target.tolerance.z,
        ),
        (
            "roll",
            error.roll_degrees / target.tolerance.roll_degrees,
            error.roll_degrees,
            target.tolerance.roll_degrees,
        ),
        (
            "pitch",
            error.pitch_degrees / target.tolerance.pitch_degrees,
            error.pitch_degrees,
            target.tolerance.pitch_degrees,
        ),
        (
            "yaw",
            error.yaw_degrees / target.tolerance.yaw_degrees,
            error.yaw_degrees,
            target.tolerance.yaw_degrees,
        ),
    ];
    let (label, _, value, limit) = values
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap();
    format!("对齐黄框：{label} 误差 {value:.2}/{limit:.2}")
}

fn guided_pose_status_overlay(assessment: &GuidedPoseAssessment, hold_frames: u8) -> OverlayStatus {
    OverlayStatus {
        hold_frames: hold_frames.min(HOLD_TARGET),
        hold_target: HOLD_TARGET,
        detail_label: "pose error".to_owned(),
        detail_value: assessment.pose_error_score,
        detail_limit: GUIDED_POSE_MATCH_SCORE_LIMIT,
        matched: assessment.matched,
    }
}

#[derive(Clone, Copy, Debug)]
enum GuidedPoseRotationRingPlane {
    RollXy,
    PitchYzNegativeZ,
    YawXzNegativeZ,
}

fn detected_outline_pixels(measurement: &GuidedPoseMeasurement) -> [[f32; 2]; 4] {
    let board = measurement.board;
    let last_col = f64::from(board.inner_cols.saturating_sub(1));
    let last_row = f64::from(board.inner_rows.saturating_sub(1));
    let points = [
        guided_pose_board_point(board, 0.0, 0.0),
        guided_pose_board_point(board, last_col, 0.0),
        guided_pose_board_point(board, last_col, last_row),
        guided_pose_board_point(board, 0.0, last_row),
    ];
    points.map(|point| {
        project_board_point_image(
            measurement.pose.rotation,
            measurement.pose.translation,
            point,
            &measurement.initial_intrinsics,
        )
        .map(|p| [p.x, p.y])
        .unwrap_or([
            measurement.pose.center_uv[0] * measurement.image_size.width as f32,
            measurement.pose.center_uv[1] * measurement.image_size.height as f32,
        ])
    })
}

fn guided_pose_rotation_ring_visual_sweep_degrees(error_degrees: f64) -> Option<f32> {
    if !error_degrees.is_finite()
        || error_degrees < f64::from(f32::MIN)
        || error_degrees > f64::from(f32::MAX)
    {
        return None;
    }
    let signed_error = error_degrees as f32;
    let error_abs = signed_error.abs();
    if error_abs <= f32::EPSILON {
        return Some(0.0);
    }
    let emphasis = GUIDED_POSE_RING_SMALL_ERROR_GAIN
        * (-error_abs / GUIDED_POSE_RING_SMALL_ERROR_DECAY_DEGREES).exp();
    Some(signed_error.signum() * error_abs.min(180.0) * (1.0 + emphasis))
}

fn guided_pose_rotation_ring_radius(board: BoardSpec) -> f64 {
    let width = f64::from(board.inner_cols.saturating_sub(1)) * board.square_size;
    let height = f64::from(board.inner_rows.saturating_sub(1)) * board.square_size;
    width.min(height).max(board.square_size) * 0.34
}

fn guided_pose_rotation_ring_local_point(
    center: [f64; 3],
    radius: f64,
    plane: GuidedPoseRotationRingPlane,
    angle: f32,
) -> [f64; 3] {
    let cos = f64::from(angle.cos());
    let sin = f64::from(angle.sin());
    match plane {
        GuidedPoseRotationRingPlane::RollXy => [
            center[0] + radius * cos,
            center[1] + radius * sin,
            center[2],
        ],
        GuidedPoseRotationRingPlane::PitchYzNegativeZ => [
            center[0],
            center[1] + radius * cos,
            center[2] - radius * sin,
        ],
        GuidedPoseRotationRingPlane::YawXzNegativeZ => [
            center[0] + radius * cos,
            center[1],
            center[2] - radius * sin,
        ],
    }
}

fn guided_pose_project_local_uv(
    measurement: &GuidedPoseMeasurement,
    point: [f64; 3],
) -> Option<[f32; 2]> {
    let image = project_board_point_image(
        measurement.pose.rotation,
        measurement.pose.translation,
        point,
        &measurement.initial_intrinsics,
    )?;
    Some([
        image.x / measurement.image_size.width as f32,
        image.y / measurement.image_size.height as f32,
    ])
}

fn guided_pose_project_rotation_ring_points(
    measurement: &GuidedPoseMeasurement,
    plane: GuidedPoseRotationRingPlane,
    start_angle: f32,
    sweep: f32,
    segments: usize,
) -> Vec<[f32; 2]> {
    let center = guided_pose_inner_center_point(measurement.board);
    let radius = guided_pose_rotation_ring_radius(measurement.board);
    let steps = segments.max(1);
    let mut points = Vec::with_capacity(steps + 1);
    for index in 0..=steps {
        let t = index as f32 / steps as f32;
        let point =
            guided_pose_rotation_ring_local_point(center, radius, plane, start_angle + sweep * t);
        if let Some(uv) = guided_pose_project_local_uv(measurement, point) {
            points.push(uv);
        }
    }
    points
}

fn guided_pose_project_rotation_ring_point(
    measurement: &GuidedPoseMeasurement,
    plane: GuidedPoseRotationRingPlane,
    angle: f32,
) -> [f32; 2] {
    let center = guided_pose_inner_center_point(measurement.board);
    let radius = guided_pose_rotation_ring_radius(measurement.board);
    guided_pose_project_local_uv(
        measurement,
        guided_pose_rotation_ring_local_point(center, radius, plane, angle),
    )
    .unwrap_or(measurement.pose.center_uv)
}

fn guided_pose_rotation_arc_overlay(
    measurement: &GuidedPoseMeasurement,
    error_degrees: f64,
    plane: GuidedPoseRotationRingPlane,
    base_start_angle: f32,
    base_sweep: f32,
    arc_start_angle: f32,
    arc_sweep_limit: f32,
) -> OverlayRotationArc {
    let base_segments = if base_sweep.abs() >= std::f32::consts::TAU - 1.0e-6 {
        GUIDED_POSE_RING_SEGMENTS
    } else {
        GUIDED_POSE_HALF_RING_SEGMENTS
    };
    let visual_sweep = guided_pose_rotation_ring_visual_sweep_degrees(error_degrees)
        .unwrap_or(0.0)
        .to_radians()
        .clamp(-arc_sweep_limit.abs(), arc_sweep_limit.abs());
    let arc_uv = if visual_sweep.abs() > 0.5_f32.to_radians() {
        guided_pose_project_rotation_ring_points(
            measurement,
            plane,
            arc_start_angle,
            visual_sweep,
            GUIDED_POSE_HALF_RING_SEGMENTS,
        )
    } else {
        Vec::new()
    };
    OverlayRotationArc {
        base_uv: guided_pose_project_rotation_ring_points(
            measurement,
            plane,
            base_start_angle,
            base_sweep,
            base_segments,
        ),
        arc_uv,
        tick_uv: guided_pose_project_rotation_ring_point(measurement, plane, arc_start_angle),
    }
}

fn guided_pose_rotation_rings_overlay(
    assessment: &GuidedPoseAssessment,
    _target: &GuidedPoseTarget,
) -> OverlayRotationRings {
    let [roll, pitch, yaw] = assessment.signed_rotation_error_degrees;
    let measurement = &assessment.measurement;
    OverlayRotationRings {
        center_uv: measurement.pose.center_uv,
        roll: guided_pose_rotation_arc_overlay(
            measurement,
            roll,
            GuidedPoseRotationRingPlane::RollXy,
            0.0,
            std::f32::consts::TAU,
            -90.0_f32.to_radians(),
            std::f32::consts::PI,
        ),
        pitch: guided_pose_rotation_arc_overlay(
            measurement,
            pitch,
            GuidedPoseRotationRingPlane::PitchYzNegativeZ,
            0.0,
            std::f32::consts::PI,
            90.0_f32.to_radians(),
            90.0_f32.to_radians(),
        ),
        yaw: guided_pose_rotation_arc_overlay(
            measurement,
            yaw,
            GuidedPoseRotationRingPlane::YawXzNegativeZ,
            0.0,
            std::f32::consts::PI,
            90.0_f32.to_radians(),
            90.0_f32.to_radians(),
        ),
    }
}

fn project_board_point_image(
    rotation: [[f64; 3]; 3],
    translation: [f64; 3],
    point: [f64; 3],
    intrinsics: &InitialIntrinsics,
) -> Option<CalibrationPoint> {
    let camera = [
        rotation[0][0] * point[0]
            + rotation[0][1] * point[1]
            + rotation[0][2] * point[2]
            + translation[0],
        rotation[1][0] * point[0]
            + rotation[1][1] * point[1]
            + rotation[1][2] * point[2]
            + translation[1],
        rotation[2][0] * point[0]
            + rotation[2][1] * point[1]
            + rotation[2][2] * point[2]
            + translation[2],
    ];
    if camera.iter().any(|value| !value.is_finite()) || camera[2] <= 0.0 {
        return None;
    }
    let x = camera[0] / camera[2];
    let y = camera[1] / camera[2];
    let [x_distorted, y_distorted] =
        distort_normalized_point(x, y, &intrinsics.distortion_coefficients)?;
    let matrix = intrinsics.camera_matrix;
    let image_x = matrix[0] * x_distorted + matrix[2];
    let image_y = matrix[4] * y_distorted + matrix[5];
    if !image_x.is_finite() || !image_y.is_finite() {
        return None;
    }
    Some(CalibrationPoint {
        x: image_x as f32,
        y: image_y as f32,
    })
}

fn rodrigues_matrix_for_preview(rotation_vector: [f64; 3]) -> Option<[[f64; 3]; 3]> {
    if rotation_vector.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let theta = rotation_vector[0]
        .hypot(rotation_vector[1])
        .hypot(rotation_vector[2]);
    if theta <= f64::EPSILON {
        return Some([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
    }
    let axis = [
        rotation_vector[0] / theta,
        rotation_vector[1] / theta,
        rotation_vector[2] / theta,
    ];
    let (sin_theta, cos_theta) = theta.sin_cos();
    let one_minus_cos = 1.0 - cos_theta;
    let [x, y, z] = axis;
    Some([
        [
            cos_theta + x * x * one_minus_cos,
            x * y * one_minus_cos - z * sin_theta,
            x * z * one_minus_cos + y * sin_theta,
        ],
        [
            y * x * one_minus_cos + z * sin_theta,
            cos_theta + y * y * one_minus_cos,
            y * z * one_minus_cos - x * sin_theta,
        ],
        [
            z * x * one_minus_cos - y * sin_theta,
            z * y * one_minus_cos + x * sin_theta,
            cos_theta + z * z * one_minus_cos,
        ],
    ])
}

fn capture_dataset_frame(
    source: &DatasetCaptureSource,
    guide_frame: &DecodedVideoFrame,
) -> Result<CapturedDatasetFrame, String> {
    match source {
        DatasetCaptureSource::X5TcpYuv {
            host,
            tcp_port,
            channel,
        } => {
            if guide_frame.identity.channel != *channel {
                return Err(format!(
                    "RTSP 通道 {} 与 TCP 抓图通道 {channel} 不一致",
                    guide_frame.identity.channel
                ));
            }
            let timestamp_ns = guide_frame.identity.device_timestamp_ns.ok_or_else(|| {
                "RTSP 帧缺少 X5 SEI timestamp_ns，无法精确回查 TCP NV12 原图".to_owned()
            })?;
            let snapshot = x5_tcp_client::capture_yuv_snapshot_by_timestamp_ns(
                host,
                *tcp_port,
                *channel,
                timestamp_ns,
            )?;
            dataset_frame_from_yuv_snapshot(snapshot)
        }
        DatasetCaptureSource::Synthetic { channel } => dataset_frame_from_decoded_rgba(
            *channel,
            guide_frame.width,
            guide_frame.height,
            guide_frame.identity.frame_sequence,
            &guide_frame.rgba,
        ),
    }
}

fn dataset_frame_from_yuv_snapshot(
    snapshot: X5YuvSnapshot,
) -> Result<CapturedDatasetFrame, String> {
    let expected_y_len = usize::try_from(snapshot.width)
        .ok()
        .and_then(|width| {
            usize::try_from(snapshot.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| "TCP NV12 尺寸溢出".to_owned())?;
    if snapshot.y_len != expected_y_len {
        return Err(format!(
            "TCP NV12 Y plane 非紧密排列：expected {expected_y_len}, got {}",
            snapshot.y_len
        ));
    }
    if snapshot.payload.len() < snapshot.y_len {
        return Err(format!(
            "TCP NV12 payload 太短：y_len={}, payload={}",
            snapshot.y_len,
            snapshot.payload.len()
        ));
    }
    Ok(CapturedDatasetFrame {
        channel: snapshot.channel,
        width: snapshot.width,
        height: snapshot.height,
        luma: snapshot.payload[..snapshot.y_len].to_vec().into(),
        source: CapturedDatasetSource::X5TcpYuv {
            frame_id: snapshot.frame_id,
            timestamp_ns: snapshot.timestamp_ns,
        },
    })
}

fn dataset_frame_from_decoded_rgba(
    channel: u16,
    width: u32,
    height: u32,
    frame_sequence: u64,
    rgba: &[u8],
) -> Result<CapturedDatasetFrame, String> {
    let pixel_count = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| "合成帧尺寸溢出".to_owned())?;
    if rgba.len() != pixel_count.saturating_mul(4) {
        return Err(format!(
            "合成 RGBA 长度不匹配：expected {}, got {}",
            pixel_count.saturating_mul(4),
            rgba.len()
        ));
    }
    let mut luma = Vec::with_capacity(pixel_count);
    for pixel in rgba.chunks_exact(4) {
        let value = (77_u16 * u16::from(pixel[0])
            + 150_u16 * u16::from(pixel[1])
            + 29_u16 * u16::from(pixel[2]))
            >> 8;
        luma.push(value as u8);
    }
    Ok(CapturedDatasetFrame {
        channel,
        width,
        height,
        luma: luma.into(),
        source: CapturedDatasetSource::SyntheticRgba { frame_sequence },
    })
}

fn set_guide(guide: &Arc<Mutex<GuideState>>, text: &str, count: usize, hold: u8) {
    let mut state = guide
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let ch0 = RtspStream::start(host, 554, 0, 1920, 1080, ch0_slot.clone()).unwrap_or_else(
                |error| {
                    godot_print!("CH0 预览启动失败：{error}");
                    RtspStream::start_synth(0, ch0_slot)
                },
            );
            let ch3 = RtspStream::start(host, 557, 3, 1920, 1080, ch3_slot.clone()).unwrap_or_else(
                |error| {
                    godot_print!("CH3 预览启动失败：{error}");
                    RtspStream::start_synth(3, ch3_slot)
                },
            );
            Self { ch0, ch3 }
        }
    }

    /// 两路是否都达到目标位姿数。
    pub fn both_complete(&self) -> bool {
        self.ch0.complete() && self.ch3.complete()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_plan() -> Vec<GuidedPoseTarget> {
        let board = BoardSpec::new(11, 8, 40.0).unwrap();
        let image_size = CalibrationImageSize::new(1920, 1080).unwrap();
        let intrinsics = InitialIntrinsics {
            camera_matrix: [900.0, 0.0, 980.0, 0.0, 900.0, 540.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 12],
        };
        standard_guided_pose_plan(board, &intrinsics, image_size).unwrap()
    }

    #[test]
    fn guided_plan_has_structured_27_pose_phases() {
        let plan = reference_plan();
        assert_eq!(plan.len(), CAPTURE_TARGET);
        assert_eq!(CAPTURE_TARGET, 27);
        assert!(plan[..15]
            .iter()
            .all(|target| target.label.starts_with('F')));
        assert!(plan[15..23]
            .iter()
            .all(|target| target.label.starts_with('T')));
        assert!(plan[23..]
            .iter()
            .all(|target| target.label.starts_with('C')));
        assert_eq!(plan[0].label, "F01 Fronto upper left");
        assert_eq!(plan[14].label, "F15 Fronto lower right");
        assert_eq!(plan[22].label, "T08 Tilt bottom");
        assert_eq!(plan[26].label, "C04 Far lower left");
    }

    #[test]
    fn guided_plan_depth_ratio_and_fronto_motion_match_constraints() {
        let plan = reference_plan();
        let min_z = plan
            .iter()
            .map(|target| target.pose.xyz[2])
            .fold(f64::INFINITY, f64::min);
        let max_z = plan
            .iter()
            .map(|target| target.pose.xyz[2])
            .fold(0.0_f64, f64::max);
        let ratio = max_z / min_z;
        assert!(
            (2.0..=5.0).contains(&ratio),
            "depth ratio {ratio:.3} out of range"
        );

        for target in &plan[..15] {
            let radius = target.pose.xyz[0].hypot(target.pose.xyz[1]);
            assert!(
                radius <= 255.0,
                "{} radius {radius:.1}mm exceeds fronto motion budget",
                target.label
            );
            assert!(target
                .pose
                .rpy_degrees
                .iter()
                .all(|angle| angle.abs() <= 1.0e-6));
        }
    }

    #[test]
    fn guided_plan_far_corner_centers_cover_four_image_corners() {
        let plan = reference_plan();
        let centers = plan[23..]
            .iter()
            .map(|target| target.pose.center_uv)
            .collect::<Vec<_>>();
        let expected = [[0.86, 0.82], [0.86, 0.18], [0.14, 0.18], [0.14, 0.82]];
        for (actual, expected) in centers.iter().zip(expected) {
            assert!((f64::from(actual[0]) - expected[0]).abs() <= 0.005);
            assert!((f64::from(actual[1]) - expected[1]).abs() <= 0.005);
        }
    }
}
