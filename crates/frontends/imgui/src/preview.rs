//! 双路 RTSP 预览与稳定检测自动采集。
//!
//! 预览：RTSP H.264 解码帧用于实时显示与棋盘检测；检测到棋盘后按帧间
//! 位姿抖动（jitter）做稳定判定，连续 hold 满 `HOLD_TARGET` 帧即触发采集。
//! dataset：真实 X5 采集在 hold 通过后按 RTSP SEI `timestamp_ns` 从 TCP 9073
//! 精确回查同源 NV12/YUV 原图；合成模式仅用于本机 UI 测试，保留 RGBA→luma fallback。
//! 无板验证：`PONGBOT_SYNTH=1` 用非棋盘合成帧（保证检测失败，验证采集链路）。
//! 采集质量：每张已采帧的全部内角点写入连续密度场；中心、四边和四角的
//! 密度证据与既有距离/倾斜 4 档共同决定完成。近重复位姿在 TCP 抓原图前拒绝。
use crate::guide_overlay::{DensityHeatmap, OverlayData, OverlayStatus};
use camera_toolbox_adapters::calibration::OpenCvCalibrationBackend;
use camera_toolbox_adapters::media::FfmpegRtspTransport;
use camera_toolbox_adapters::media::ffmpeg_rtsp::FfmpegRtspDecoder;
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 保留的距离（投影尺寸）和倾斜覆盖档数。
const DEPTH_COVERAGE_BINS: usize = 4;
const SKEW_COVERAGE_BINS: usize = 4;
const SPATIAL_EDGE_COUNT: usize = 4;
const SPATIAL_CORNER_COUNT: usize = 4;
/// 连续密度场的固定分辨率；只保存累计场，不保存角点历史。
const DENSITY_COLS: usize = 64;
const DENSITY_ROWS: usize = 36;
const DENSITY_KERNEL_SIGMA: f64 = 0.035;
/// 单个角点高斯核的峰值，也是空间区域的充分密度阈值。
const DENSITY_SUFFICIENT: f32 = 1.0;
/// 中心区域半径占图像半对角线的比例。
const CENTER_RADIUS: f64 = 0.30;
/// 边缘和四角区域只接受接近画面外围的角点。
const OUTER_RADIUS_THRESHOLD: f64 = 0.80;
/// 归一化图像坐标中的边缘/角落带宽。
const OUTER_REGION_BAND: f64 = 0.20;
/// 仅保留有限个已采视角摘要，保证去重累加器内存有上界。
const MAX_VIEW_SUMMARIES: usize = 64;
/// 小于该既有姿态 jitter 分数时，视为近重复视角。
const DUPLICATE_VIEW_JITTER_LIMIT: f64 = 0.40;
/// 稳定检测的 hold 连续帧数（满则触发采集）。
pub const HOLD_TARGET: u8 = 3;
const GUIDED_HOLD_JITTER_XYZ_LIMIT: f64 = 0.025;
const GUIDED_HOLD_JITTER_Z_LIMIT: f64 = 0.04;
const GUIDED_HOLD_JITTER_RPY_DEGREES: f64 = 2.0;
/// 采集 worker 检测节拍。
const DETECT_INTERVAL: Duration = Duration::from_millis(150);
/// 成功拍摄后角点 overlay 保留显示时长（确认触发瞬间采到的棋盘姿态）。
const CAPTURE_CORNERS_DISPLAY: Duration = Duration::from_millis(1500);

/// X5 TCP 控制端口；RTSP 只做引导，dataset 通过该端口抓同源 NV12 原图。
const X5_TCP_CONTROL_PORT: u16 = 9073;

/// 数据集质量的 UI 快照。
///
/// 空间证据来自全部已接受内角点的连续密度累加；距离和倾斜则保留原有四档
/// 覆盖语义。没有帧数、尺度比或姿态族的额外完成门限。
#[derive(Clone, Debug, PartialEq)]
pub struct DatasetQuality {
    /// 已接受且已写入 dataset 的帧数，仅用于引导文本与诊断，不是完成门限。
    pub accepted_frames: usize,
    pub heatmap: DensityHeatmap,
    /// 指定中心半径内的累计密度。
    pub center_density: f32,
    /// 左、右、上、下四个外缘区域的累计密度。
    pub edge_density: [f32; SPATIAL_EDGE_COUNT],
    /// 左上、右上、右下、左下四个外围角落的累计密度。
    pub corner_density: [f32; SPATIAL_CORNER_COUNT],
    /// 保留的投影尺寸（远→近）四档覆盖。
    pub depth: [bool; DEPTH_COVERAGE_BINS],
    /// 保留的板法线倾角（正视→大倾）四档覆盖。
    pub skew: [bool; SKEW_COVERAGE_BINS],
}

impl Default for DatasetQuality {
    fn default() -> Self {
        Self {
            accepted_frames: 0,
            heatmap: DensityHeatmap::zeroed(DENSITY_COLS, DENSITY_ROWS),
            center_density: 0.0,
            edge_density: [0.0; SPATIAL_EDGE_COUNT],
            corner_density: [0.0; SPATIAL_CORNER_COUNT],
            depth: [false; DEPTH_COVERAGE_BINS],
            skew: [false; SKEW_COVERAGE_BINS],
        }
    }
}

impl DatasetQuality {
    /// 连续空间、保留距离和保留倾斜证据全部充分时结束采集。
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.center_complete()
            && self.edges_complete()
            && self.corners_complete()
            && self.depth.iter().all(|covered| *covered)
            && self.skew.iter().all(|covered| *covered)
    }

    #[must_use]
    pub fn center_complete(&self) -> bool {
        self.center_density >= DENSITY_SUFFICIENT
    }

    #[must_use]
    pub fn edges_complete(&self) -> bool {
        self.edge_density
            .iter()
            .all(|density| *density >= DENSITY_SUFFICIENT)
    }

    #[must_use]
    pub fn corners_complete(&self) -> bool {
        self.corner_density
            .iter()
            .all(|density| *density >= DENSITY_SUFFICIENT)
    }

    #[must_use]
    pub fn center_progress(&self) -> f32 {
        density_progress(self.center_density)
    }

    #[must_use]
    pub fn edge_progress(&self) -> f32 {
        self.edge_density
            .iter()
            .map(|density| density_progress(*density))
            .sum::<f32>()
            / SPATIAL_EDGE_COUNT as f32
    }

    #[must_use]
    pub fn corner_progress(&self) -> f32 {
        self.corner_density
            .iter()
            .map(|density| density_progress(*density))
            .sum::<f32>()
            / SPATIAL_CORNER_COUNT as f32
    }

    #[must_use]
    pub fn covered_edges(&self) -> usize {
        self.edge_density
            .iter()
            .filter(|density| **density >= DENSITY_SUFFICIENT)
            .count()
    }

    #[must_use]
    pub fn covered_corners(&self) -> usize {
        self.corner_density
            .iter()
            .filter(|density| **density >= DENSITY_SUFFICIENT)
            .count()
    }
}

#[derive(Clone, Copy, Debug)]
struct AcceptedViewSummary {
    xyz: [f64; 3],
    rpy_degrees: [f64; 3],
}

/// 单路常量内存的角点质量累加器。
struct CornerDensityMap {
    field: Vec<f32>,
    center_density: f32,
    edge_density: [f32; SPATIAL_EDGE_COUNT],
    corner_density: [f32; SPATIAL_CORNER_COUNT],
    depth: [bool; DEPTH_COVERAGE_BINS],
    skew: [bool; SKEW_COVERAGE_BINS],
    views: Vec<AcceptedViewSummary>,
    accepted_frames: usize,
}

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

#[derive(Clone, Debug, Default)]
pub struct GuideState {
    /// overlay 引导文本（未检测到棋盘 / 空间与距离倾斜质量状态）。
    pub text: String,
    /// 已采帧数。
    pub captured_count: usize,
    /// 当前 hold 计数（0 = 未 hold）。
    pub hold: u8,
    /// 数据集质量快照（worker 每次接受帧后更新）。
    pub quality: DatasetQuality,
}

/// 单路 RTSP 流：解码器 + 帧槽 + guided 采集。
pub struct RtspStream {
    decoder: Option<FfmpegRtspDecoder>,
    slot: Arc<LatestDecodedFrameSlot>,
    last: Option<Arc<DecodedVideoFrame>>,
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
    detect_started: bool,
    /// RTSP 解码失败信息（pump 检查 completion 写入，worker 读取显示）。
    rtsp_error: Arc<Mutex<Option<String>>>,
    /// dataset 的权威抓帧来源；真实设备必须走 TCP YUV，RTSP 不直接入库。
    capture_source: DatasetCaptureSource,
}

impl RtspStream {
    /// 启动真实 RTSP 解码（Tcp 传输，稳定缓冲模式）。
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
            RtspLatencyMode::Stable,
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
            let (width, height) = (960u32, 540u32);
            let mut seed = u64::from(channel) + 1;
            let board_mode = std::env::var("PONGBOT_SYNTH").is_ok_and(|v| v == "board");
            let mut tick = 0u32;
            loop {
                let mut rgba = Vec::with_capacity((width * height * 4) as usize);
                if board_mode {
                    // 合成棋盘（12x9 格 = 11x8 内角点）：四阶段自动动作模拟标定覆盖，
                    // X 平移 → Y 平移 → 尺寸缩放 → 倾斜；每阶段 60 步（400ms/步 ≈ 24s），
                    // 四阶段约 96s 扫完 18 bin 全覆盖（无板验证全流程）。
                    let cols = 12i32;
                    let rows = 9i32;
                    let slow = tick / 4;
                    let stage_len = 60u32;
                    let phase = (slow / stage_len) % 4;
                    let t = (slow % stage_len) as f32 / (stage_len - 1) as f32;
                    let (center_x, center_y, cell, shear) = match phase {
                        // X 扫描：小棋盘（宽 192px）横向扫过 10%..90% → X 5 bin 全覆盖。
                        0 => (0.1 + 0.8 * t, 0.5, 16.0, 0.0),
                        // Y 扫描：棋盘高 144px，纵向扫过 15%..85% → Y 5 bin 全覆盖。
                        1 => (0.5, 0.15 + 0.7 * t, 16.0, 0.0),
                        // 尺寸：cell 13→54（棋盘占画面 0.16..0.68）→ Size 4 bin。
                        2 => (0.5, 0.5, 13.0 + 41.0 * t, 0.0),
                        // 倾斜：shear 0→0.75（板法线与光轴夹角 0..37°）→ Skew 4 bin。
                        _ => (0.5, 0.5, 32.0, 0.75 * t),
                    };
                    let cell = cell as i32;
                    let ox = (center_x * width as f32 - (cols * cell) as f32 * 0.5) as i32;
                    let oy = (center_y * height as f32 - (rows * cell) as f32 * 0.5) as i32;
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
            capturing: Arc::new(AtomicBool::new(false)),
            captured: Arc::new(Mutex::new(Vec::new())),
            poses: Arc::new(Mutex::new(Vec::new())),
            overlay: overlay_slot,
            guide_state: Arc::new(Mutex::new(GuideState::default())),
            detect_started: false,
            rtsp_error: Arc::new(Mutex::new(None)),
            capture_source,
        }
    }

    /// 主线程调用：返回自上次调用以来的最新新帧；不触碰任何 UI/GPU 对象。
    pub fn poll_frame(&mut self) -> Option<Arc<DecodedVideoFrame>> {
        // 检查解码器终态：连接/解码失败时记录，供引导显示。
        if let Some(decoder) = self.decoder.as_ref() {
            if let Some(Err(error)) = decoder.completion() {
                if let Ok(mut slot) = self.rtsp_error.lock() {
                    *slot = Some(error.clone());
                }
            }
        }
        let frame = self.slot.latest()?;
        if self
            .last
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, &frame))
        {
            return None;
        }
        self.last = Some(frame.clone());
        Some(frame)
    }

    /// 读取当前 overlay 数据快照；绘制由前端完成。
    pub fn overlay(&self) -> Option<OverlayData> {
        self.overlay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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
        std::thread::spawn(move || {
            capture_loop(
                capturing,
                slot,
                captured,
                poses,
                guide,
                overlay,
                rtsp_error,
                capture_source,
                board,
            );
        });
    }

    /// 读取引导状态（主线程）：文本、已采帧数、hold、质量快照。
    pub fn guide(&self) -> (String, usize, u8, DatasetQuality) {
        let state = self
            .guide_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            state.text.clone(),
            state.captured_count,
            state.hold,
            state.quality.clone(),
        )
    }

    /// 是否满足连续空间与保留距离/倾斜覆盖质量。
    pub fn complete(&self) -> bool {
        let state = self
            .guide_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.quality.is_complete()
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
struct GuidedPose6Dof {
    /// 棋盘中心在相机坐标系下的 XYZ；单位继承 BoardSpec::square_size。
    xyz: [f64; 3],
    /// board->camera 旋转矩阵按 ZYX 分解得到的 roll/pitch/yaw，单位 degree；
    /// 语义对齐标定板坐标轴：roll 绕板 Z（法线）、pitch 绕板 X（横纹理）、yaw 绕板 Y（竖纹理）。
    rpy_degrees: [f64; 3],
    rotation: [[f64; 3]; 3],
    translation: [f64; 3],
    center_uv: [f32; 2],
}

#[derive(Clone, Debug, PartialEq)]
struct GuidedPoseMeasurement {
    pose: GuidedPose6Dof,
    board: BoardSpec,
    initial_intrinsics: InitialIntrinsics,
    image_size: CalibrationImageSize,
}

/// 稳定检测采集循环：hold 满后先拒绝无效/近重复视角，再抓 TCP 原图并更新连续质量。
#[allow(clippy::too_many_arguments)]
fn capture_loop(
    capturing: Arc<AtomicBool>,
    slot: Arc<LatestDecodedFrameSlot>,
    captured: Arc<Mutex<Vec<Arc<CapturedDatasetFrame>>>>,
    poses: Arc<Mutex<Vec<[f64; 3]>>>,
    guide: Arc<Mutex<GuideState>>,
    overlay: Arc<Mutex<Option<OverlayData>>>,
    rtsp_error: Arc<Mutex<Option<String>>>,
    capture_source: DatasetCaptureSource,
    board: BoardSpec,
) {
    let backend = OpenCvCalibrationBackend;
    let cancellation = CalibrationCancellation::default();
    let mut hold_frames: u8 = 0;
    let mut last_measurement: Option<GuidedPoseMeasurement> = None;
    let mut density = CornerDensityMap::new();
    let mut quality = DatasetQuality::default();
    let mut last_capture: Option<(std::time::Instant, Vec<[f32; 2]>)> = None;
    tracing::info!("稳定检测采集 worker 已启动（hold {HOLD_TARGET} 帧，连续空间 + 距离/倾斜质量）");
    loop {
        if quality.is_complete() {
            set_guide_quality(
                &guide,
                &format!(
                    "采集质量完成（已采 {} 张），采集结束",
                    quality.accepted_frames
                ),
                0,
                &quality,
            );
            break;
        }
        let Some(frame) = slot.latest() else {
            let error = rtsp_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(error) = error {
                set_guide_quality(
                    &guide,
                    &format!("RTSP 无帧：{error}（检查板端 DEMO233）"),
                    0,
                    &quality,
                );
            } else {
                set_guide_quality(&guide, "等待 RTSP 帧…", 0, &quality);
            }
            std::thread::sleep(DETECT_INTERVAL);
            continue;
        };

        let image_size = CalibrationImageSize {
            width: frame.width,
            height: frame.height,
        };
        let initial = default_initial_intrinsics(frame.width, frame.height);
        // 拍摄角点 overlay：拍摄后短暂保留显示，供确认触发瞬间的棋盘姿态。
        let captured_corners = last_capture
            .as_ref()
            .filter(|(time, _)| time.elapsed() < CAPTURE_CORNERS_DISPLAY)
            .map(|(_, corners)| {
                (
                    usize::from(board.inner_cols),
                    usize::from(board.inner_rows),
                    corners.clone(),
                )
            });

        let (detect_rgba, detect_w, detect_h, detect_scale) =
            resize_for_detect(&frame.rgba, frame.width, frame.height);
        let png = match encode_png(&detect_rgba, detect_w, detect_h) {
            Ok(png) => png,
            Err(error) => {
                set_guide_quality(&guide, &format!("PNG 编码失败：{error}"), 0, &quality);
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
                    .map(|corner| CalibrationPoint {
                        x: corner.x / detect_scale,
                        y: corner.y / detect_scale,
                    })
                    .collect();
                let detection = ChessboardDetection {
                    image_size,
                    corners,
                };
                let view = match backend.estimate_pose(&detection, &initial, board, &cancellation) {
                    Ok(view) => view,
                    Err(error) => {
                        hold_frames = 0;
                        last_measurement = None;
                        set_guide_quality(
                            &guide,
                            &format!("检测到棋盘，姿态估计失败：{error}"),
                            0,
                            &quality,
                        );
                        std::thread::sleep(DETECT_INTERVAL);
                        continue;
                    }
                };
                let measurement = match guided_pose_measurement(&view, board, &initial, image_size)
                {
                    Ok(measurement) => measurement,
                    Err(error) => {
                        hold_frames = 0;
                        last_measurement = None;
                        set_guide_quality(&guide, &format!("棋盘位姿无效：{error}"), 0, &quality);
                        std::thread::sleep(DETECT_INTERVAL);
                        continue;
                    }
                };
                publish_overlay(
                    &overlay,
                    frame.width,
                    frame.height,
                    Some(&measurement),
                    Some(hold_frames),
                    captured_corners.clone(),
                );

                if !capturing.load(Ordering::Acquire) {
                    set_guide_quality(&guide, "等待自动采集启动…", 0, &quality);
                    std::thread::sleep(DETECT_INTERVAL);
                    continue;
                }

                let jitter_score = last_measurement.as_ref().map_or(0.0, |previous| {
                    guided_hold_jitter_score(previous, &measurement)
                });
                if jitter_score > 1.0 {
                    hold_frames = 0;
                    last_measurement = None;
                    set_guide_quality(
                        &guide,
                        &format!("棋盘在移动（jitter {jitter_score:.2}），请保持稳定"),
                        0,
                        &quality,
                    );
                    std::thread::sleep(DETECT_INTERVAL);
                    continue;
                }
                hold_frames = hold_frames.saturating_add(1).min(HOLD_TARGET);
                last_measurement = Some(measurement.clone());
                if hold_frames < HOLD_TARGET {
                    set_guide_quality(
                        &guide,
                        &format!("已检测棋盘，保持稳定 {hold_frames}/{HOLD_TARGET}"),
                        hold_frames,
                        &quality,
                    );
                    std::thread::sleep(DETECT_INTERVAL);
                    continue;
                }
                if !density.has_usable_corners(&detection) {
                    hold_frames = 0;
                    last_measurement = None;
                    set_guide_quality(&guide, "检测角点无效，未抓取原图", 0, &quality);
                    std::thread::sleep(DETECT_INTERVAL);
                    continue;
                }
                if density.is_near_duplicate(&measurement) {
                    hold_frames = 0;
                    last_measurement = None;
                    set_guide_quality(
                        &guide,
                        "近重复视角已拒绝，请移动或转动棋盘后再保持稳定",
                        0,
                        &quality,
                    );
                    std::thread::sleep(DETECT_INTERVAL);
                    continue;
                }

                // 去重通过后才允许抓 TCP 原图，避免重复数据进入 solver dataset。
                let dataset_frame = match capture_dataset_frame(&capture_source, &frame) {
                    Ok(frame) => frame,
                    Err(error) => {
                        hold_frames = 0;
                        last_measurement = None;
                        set_guide_quality(
                            &guide,
                            &format!("TCP YUV 原图抓取失败：{error}"),
                            0,
                            &quality,
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
                    poses.push(view.rotation_vector);
                }
                let added = density.add_frame(&detection, &measurement);
                debug_assert!(added);
                quality = density.snapshot();

                // 记录本次拍摄的角点（image 像素坐标）并立即发布 overlay 确认。
                let corners_px: Vec<[f32; 2]> = detection
                    .corners
                    .iter()
                    .map(|corner| [corner.x, corner.y])
                    .collect();
                last_capture = Some((std::time::Instant::now(), corners_px));
                publish_overlay(
                    &overlay,
                    frame.width,
                    frame.height,
                    Some(&measurement),
                    Some(HOLD_TARGET),
                    last_capture.as_ref().map(|(_, corners)| {
                        (
                            usize::from(board.inner_cols),
                            usize::from(board.inner_rows),
                            corners.clone(),
                        )
                    }),
                );
                hold_frames = 0;
                last_measurement = None;
                if quality.is_complete() {
                    set_guide_quality(
                        &guide,
                        &format!(
                            "采集质量完成（已采 {} 张），采集结束",
                            quality.accepted_frames
                        ),
                        0,
                        &quality,
                    );
                    break;
                }
                set_guide_quality(
                    &guide,
                    &format!(
                        "已采集 {} 张 · {}",
                        quality.accepted_frames,
                        quality_missing_hint(&quality)
                    ),
                    0,
                    &quality,
                );
            }
            Ok(ChessboardDetectionOutcome::NotFound { .. }) => {
                hold_frames = 0;
                last_measurement = None;
                publish_overlay(
                    &overlay,
                    frame.width,
                    frame.height,
                    None,
                    None,
                    captured_corners.clone(),
                );
                set_guide_quality(
                    &guide,
                    "未检测到棋盘 · 请把 11×8 / 40mm 棋盘移入画面",
                    0,
                    &quality,
                );
            }
            Err(error) => {
                hold_frames = 0;
                last_measurement = None;
                publish_overlay(
                    &overlay,
                    frame.width,
                    frame.height,
                    None,
                    None,
                    captured_corners.clone(),
                );
                set_guide_quality(&guide, &format!("检测失败：{error}"), 0, &quality);
            }
        }
        std::thread::sleep(DETECT_INTERVAL);
    }
}

/// 发布当前帧的 overlay 绘制数据（检测外框 + hold 状态 + 拍摄角点）。
fn publish_overlay(
    overlay: &Arc<Mutex<Option<OverlayData>>>,
    width: u32,
    height: u32,
    measurement: Option<&GuidedPoseMeasurement>,
    hold_frames: Option<u8>,
    captured_corners_px: Option<(usize, usize, Vec<[f32; 2]>)>,
) {
    if let Ok(mut slot) = overlay.lock() {
        *slot = Some(OverlayData {
            image_width: width as f32,
            image_height: height as f32,
            detected_outline_px: measurement.map(detected_outline_pixels),
            status: hold_frames.map(|frames| OverlayStatus {
                hold_frames: frames.min(HOLD_TARGET),
                hold_target: HOLD_TARGET,
            }),
            captured_corners_px,
        });
    }
}

/// 将累计密度映射为 UI 进度；异常值一律不参与完成判断。
fn density_progress(density: f32) -> f32 {
    if density.is_finite() {
        (density / DENSITY_SUFFICIENT).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

impl CornerDensityMap {
    fn new() -> Self {
        debug_assert!(DENSITY_COLS >= 8 && DENSITY_ROWS >= 8);
        Self {
            field: vec![0.0; DENSITY_COLS * DENSITY_ROWS],
            center_density: 0.0,
            edge_density: [0.0; SPATIAL_EDGE_COUNT],
            corner_density: [0.0; SPATIAL_CORNER_COUNT],
            depth: [false; DEPTH_COVERAGE_BINS],
            skew: [false; SKEW_COVERAGE_BINS],
            views: Vec::with_capacity(MAX_VIEW_SUMMARIES),
            accepted_frames: 0,
        }
    }

    /// 只有至少一个位于图像内的有限角点，才允许进行 TCP 原图抓取。
    fn has_usable_corners(&self, detection: &ChessboardDetection) -> bool {
        detection
            .corners
            .iter()
            .any(|corner| normalized_corner(corner, detection.image_size).is_some())
    }

    /// 去重只检查紧凑的位姿摘要；此函数没有副作用，故可在 TCP 抓图前调用。
    fn is_near_duplicate(&self, measurement: &GuidedPoseMeasurement) -> bool {
        self.views.iter().any(|previous| {
            guided_pose_jitter_score(
                previous.xyz,
                previous.rpy_degrees,
                measurement.pose.xyz,
                measurement.pose.rpy_degrees,
            ) < DUPLICATE_VIEW_JITTER_LIMIT
        })
    }

    /// 成功抓取原图后一次性写入角点密度、空间区域和保留的距离/倾斜覆盖。
    /// 返回 `false` 表示没有任何有效内角点，因此不会改变累加器。
    fn add_frame(
        &mut self,
        detection: &ChessboardDetection,
        measurement: &GuidedPoseMeasurement,
    ) -> bool {
        if !self.has_usable_corners(detection) {
            return false;
        }
        let image_size = detection.image_size;
        let width = f64::from(image_size.width);
        let height = f64::from(image_size.height);
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for corner in &detection.corners {
            let Some([u, v]) = normalized_corner(corner, image_size) else {
                continue;
            };
            let x = f64::from(corner.x);
            let y = f64::from(corner.y);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            self.splat_density(u, v);
            let radius = normalized_image_radius(u, v, width, height);
            if radius <= CENTER_RADIUS {
                add_density(&mut self.center_density, 1.0);
            }
            if radius > OUTER_RADIUS_THRESHOLD {
                self.add_outer_region_density(u, v);
            }
        }
        let scale = ((max_x - min_x) / width).min((max_y - min_y) / height);
        self.depth[depth_coverage_bin(scale)] = true;
        // 板 Z 轴（法线）的 z 分量 = R[2][2]；保留原有倾角分档边界。
        let normal_angle = measurement.pose.rotation[2][2]
            .clamp(-1.0, 1.0)
            .abs()
            .acos()
            .to_degrees();
        let skew_index = if normal_angle.is_finite() {
            skew_coverage_bin(normal_angle)
        } else {
            SKEW_COVERAGE_BINS - 1
        };
        self.skew[skew_index] = true;
        self.remember_view(measurement.pose);
        self.accepted_frames = self.accepted_frames.saturating_add(1);
        true
    }

    fn splat_density(&mut self, u: f64, v: f64) {
        let radius_x = (DENSITY_KERNEL_SIGMA * DENSITY_COLS as f64 * 3.0).ceil() as isize;
        let radius_y = (DENSITY_KERNEL_SIGMA * DENSITY_ROWS as f64 * 3.0).ceil() as isize;
        let center_x = (u * DENSITY_COLS as f64 - 0.5).round() as isize;
        let center_y = (v * DENSITY_ROWS as f64 - 0.5).round() as isize;
        let min_x = (center_x - radius_x).max(0);
        let max_x = (center_x + radius_x).min(DENSITY_COLS as isize - 1);
        let min_y = (center_y - radius_y).max(0);
        let max_y = (center_y + radius_y).min(DENSITY_ROWS as isize - 1);
        for row in min_y..=max_y {
            let sample_v = (row as f64 + 0.5) / DENSITY_ROWS as f64;
            for col in min_x..=max_x {
                let sample_u = (col as f64 + 0.5) / DENSITY_COLS as f64;
                let squared_distance = (sample_u - u).powi(2) + (sample_v - v).powi(2);
                let weight =
                    (-squared_distance / (2.0 * DENSITY_KERNEL_SIGMA.powi(2))).exp() as f32;
                if weight.is_finite() {
                    let index = row as usize * DENSITY_COLS + col as usize;
                    add_density(&mut self.field[index], weight);
                }
            }
        }
    }

    fn add_outer_region_density(&mut self, u: f64, v: f64) {
        if u <= OUTER_REGION_BAND {
            add_density(&mut self.edge_density[0], 1.0);
        }
        if u >= 1.0 - OUTER_REGION_BAND {
            add_density(&mut self.edge_density[1], 1.0);
        }
        if v <= OUTER_REGION_BAND {
            add_density(&mut self.edge_density[2], 1.0);
        }
        if v >= 1.0 - OUTER_REGION_BAND {
            add_density(&mut self.edge_density[3], 1.0);
        }
        let corner = match (
            u <= OUTER_REGION_BAND,
            u >= 1.0 - OUTER_REGION_BAND,
            v <= OUTER_REGION_BAND,
            v >= 1.0 - OUTER_REGION_BAND,
        ) {
            (true, _, true, _) => Some(0),
            (_, true, true, _) => Some(1),
            (_, true, _, true) => Some(2),
            (true, _, _, true) => Some(3),
            _ => None,
        };
        if let Some(index) = corner {
            add_density(&mut self.corner_density[index], 1.0);
        }
    }

    fn remember_view(&mut self, pose: GuidedPose6Dof) {
        let summary = AcceptedViewSummary {
            xyz: pose.xyz,
            rpy_degrees: pose.rpy_degrees,
        };
        if self.views.len() < MAX_VIEW_SUMMARIES {
            self.views.push(summary);
        } else {
            let index = self.accepted_frames % MAX_VIEW_SUMMARIES;
            self.views[index] = summary;
        }
    }

    fn snapshot(&self) -> DatasetQuality {
        DatasetQuality {
            accepted_frames: self.accepted_frames,
            heatmap: DensityHeatmap {
                cols: DENSITY_COLS,
                rows: DENSITY_ROWS,
                samples: self.field.clone().into(),
            },
            center_density: self.center_density,
            edge_density: self.edge_density,
            corner_density: self.corner_density,
            depth: self.depth,
            skew: self.skew,
        }
    }
}

/// 归一化且位于图像内的角点；无效值绝不进入密度场。
fn normalized_corner(
    corner: &CalibrationPoint,
    image_size: CalibrationImageSize,
) -> Option<[f64; 2]> {
    let width = f64::from(image_size.width);
    let height = f64::from(image_size.height);
    let x = f64::from(corner.x);
    let y = f64::from(corner.y);
    if !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
        || !x.is_finite()
        || !y.is_finite()
    {
        return None;
    }
    let u = x / width;
    let v = y / height;
    (u.is_finite() && v.is_finite() && (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v))
        .then_some([u, v])
}

/// 角点到图像中心的连续物理半径，按图像半对角线归一化。
fn normalized_image_radius(u: f64, v: f64, width: f64, height: f64) -> f64 {
    ((u - 0.5) * width).hypot((v - 0.5) * height) / (width * 0.5).hypot(height * 0.5)
}

fn add_density(slot: &mut f32, contribution: f32) {
    *slot = (*slot + contribution).min(f32::MAX);
}

/// 保留的投影尺度分档（远 / 中远 / 中近 / 近）。
fn depth_coverage_bin(scale: f64) -> usize {
    if scale < 0.18 {
        0
    } else if scale < 0.28 {
        1
    } else if scale < 0.42 {
        2
    } else {
        3
    }
}

/// 保留的板法线与光轴夹角分档（正视 / 浅倾 / 中倾 / 大倾）。
fn skew_coverage_bin(normal_angle_degrees: f64) -> usize {
    if normal_angle_degrees < 10.0 {
        0
    } else if normal_angle_degrees < 22.0 {
        1
    } else if normal_angle_degrees < 35.0 {
        2
    } else {
        3
    }
}

fn quality_missing_hint(quality: &DatasetQuality) -> &'static str {
    if !quality.center_complete() {
        "请让棋盘内角点覆盖画面中心"
    } else if !quality.edges_complete() {
        "请将棋盘移到四个画面边缘"
    } else if !quality.corners_complete() {
        "请将棋盘移动到四个画面角落"
    } else if quality.depth.iter().any(|covered| !covered) {
        "请调整棋盘距离，覆盖远中近尺度"
    } else {
        "请改变棋盘倾斜角度"
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

/// board->camera 旋转矩阵按 R = Rz(yaw)·Ry(pitch)·Rx(roll)（ZYX）分解。
///
/// 返回的 [roll, pitch, yaw] 语义对齐标定板坐标轴（OpenCV x 右 / y 下 / z 前）：
/// roll 绕板 Z 轴（平面法线）、pitch 绕板 X 轴（横纹理）、yaw 绕板 Y 轴（竖纹理），
/// 单位 degree。符号沿用分解公式（右手系，从轴正端看正角为逆时针）。
fn guided_pose_rotation_to_rpy_degrees(rotation: [[f64; 3]; 3]) -> Option<[f64; 3]> {
    if rotation.iter().flatten().any(|value| !value.is_finite()) {
        return None;
    }
    // ZYX 分解：β 绕板 Y（竖）、α 绕板 X（横）、γ 绕板 Z（法线）。
    let beta = (-rotation[2][0]).clamp(-1.0, 1.0).asin();
    let cos_beta = beta.cos();
    let (gamma, alpha) = if cos_beta.abs() > 1.0e-9 {
        (
            rotation[1][0].atan2(rotation[0][0]),
            rotation[2][1].atan2(rotation[2][2]),
        )
    } else {
        ((-rotation[0][1]).atan2(rotation[1][1]), 0.0)
    };
    let rpy = [gamma.to_degrees(), alpha.to_degrees(), beta.to_degrees()];
    rpy.iter().all(|value| value.is_finite()).then_some(rpy)
}

fn signed_angle_distance_degrees(left: f64, right: f64) -> f64 {
    let delta = (left - right).rem_euclid(360.0);
    if delta > 180.0 { delta - 360.0 } else { delta }
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
    Some([
        signed_angle_distance_degrees(target_rpy_degrees[0], measurement_rpy_degrees[0]),
        signed_angle_distance_degrees(target_rpy_degrees[1], measurement_rpy_degrees[1]),
        signed_angle_distance_degrees(target_rpy_degrees[2], measurement_rpy_degrees[2]),
    ])
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
fn guided_hold_jitter_score(
    previous: &GuidedPoseMeasurement,
    current: &GuidedPoseMeasurement,
) -> f64 {
    guided_pose_jitter_score(
        previous.pose.xyz,
        previous.pose.rpy_degrees,
        current.pose.xyz,
        current.pose.rpy_degrees,
    )
}

/// 稳定 hold 与已采视角去重共用同一姿态距离，避免两个阈值模型漂移。
fn guided_pose_jitter_score(
    previous_xyz: [f64; 3],
    previous_rpy_degrees: [f64; 3],
    current_xyz: [f64; 3],
    current_rpy_degrees: [f64; 3],
) -> f64 {
    if previous_xyz
        .iter()
        .chain(previous_rpy_degrees.iter())
        .chain(current_xyz.iter())
        .chain(current_rpy_degrees.iter())
        .any(|value| !value.is_finite())
    {
        return f64::INFINITY;
    }
    let depth_scale = previous_xyz[2].abs().max(current_xyz[2].abs()).max(1.0);
    let xyz_score = ((previous_xyz[0] - current_xyz[0]).abs()
        / depth_scale
        / GUIDED_HOLD_JITTER_XYZ_LIMIT)
        .max((previous_xyz[1] - current_xyz[1]).abs() / depth_scale / GUIDED_HOLD_JITTER_XYZ_LIMIT)
        .max((previous_xyz[2] - current_xyz[2]).abs() / depth_scale / GUIDED_HOLD_JITTER_Z_LIMIT);
    let rpy_score =
        guided_pose_signed_rotation_error_components(previous_rpy_degrees, current_rpy_degrees)
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
fn detected_outline_pixels(measurement: &GuidedPoseMeasurement) -> [[f32; 2]; 4] {
    let board = measurement.board;
    let left = -1.0;
    let top = -1.0;
    let right = f64::from(board.inner_cols);
    let bottom = f64::from(board.inner_rows);
    let points = [
        guided_pose_board_point(board, left, top),
        guided_pose_board_point(board, right, top),
        guided_pose_board_point(board, right, bottom),
        guided_pose_board_point(board, left, bottom),
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

fn set_guide_quality(
    guide: &Arc<Mutex<GuideState>>,
    text: &str,
    hold: u8,
    quality: &DatasetQuality,
) {
    let mut state = guide
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.text = text.to_owned();
    state.captured_count = quality.accepted_frames;
    state.hold = hold;
    state.quality = quality.clone();
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
                    tracing::warn!("CH0 预览启动失败：{error}");
                    RtspStream::start_synth(0, ch0_slot)
                },
            );
            let ch3 = RtspStream::start(host, 557, 3, 1920, 1080, ch3_slot.clone()).unwrap_or_else(
                |error| {
                    tracing::warn!("CH3 预览启动失败：{error}");
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

    fn board() -> BoardSpec {
        BoardSpec::new(11, 8, 40.0).unwrap()
    }

    fn image_size() -> CalibrationImageSize {
        CalibrationImageSize::new(1920, 1080).unwrap()
    }

    fn intrinsics() -> InitialIntrinsics {
        InitialIntrinsics {
            camera_matrix: [900.0, 0.0, 980.0, 0.0, 900.0, 540.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 12],
        }
    }

    fn identity() -> [[f64; 3]; 3] {
        [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
    }

    fn view(rotation_vector: [f64; 3], translation_vector: [f64; 3]) -> ViewCalibrationResult {
        ViewCalibrationResult {
            rotation_vector,
            translation_vector,
            projected_points: Vec::new(),
            reprojection_rmse: 0.0,
            max_reprojection_error: 0.0,
        }
    }

    /// 生成一个中心在 (center_x, center_y)、尺寸 (box_w × box_h) 像素的角点云。
    fn corner_cloud(center_x: f64, center_y: f64, box_w: f64, box_h: f64) -> Vec<CalibrationPoint> {
        let mut corners = Vec::new();
        for row in 0..8 {
            for column in 0..11 {
                let x = (center_x + (column as f64 - 5.0) * box_w / 10.0) as f32;
                let y = (center_y + (row as f64 - 3.5) * box_h / 7.0) as f32;
                corners.push(CalibrationPoint { x, y });
            }
        }
        corners
    }

    fn detection_with(corners: Vec<CalibrationPoint>) -> ChessboardDetection {
        ChessboardDetection {
            image_size: image_size(),
            corners,
        }
    }

    fn measurement(
        rotation_vector: [f64; 3],
        translation_vector: [f64; 3],
    ) -> GuidedPoseMeasurement {
        let board = board();
        let initial = intrinsics();
        let size = image_size();
        guided_pose_measurement(
            &view(rotation_vector, translation_vector),
            board,
            &initial,
            size,
        )
        .expect("synthetic pose should be valid")
    }

    fn point(u: f64, v: f64) -> CalibrationPoint {
        CalibrationPoint {
            x: (u * f64::from(image_size().width)) as f32,
            y: (v * f64::from(image_size().height)) as f32,
        }
    }

    fn complete_quality() -> DatasetQuality {
        DatasetQuality {
            accepted_frames: 0,
            heatmap: DensityHeatmap::zeroed(DENSITY_COLS, DENSITY_ROWS),
            center_density: DENSITY_SUFFICIENT,
            edge_density: [DENSITY_SUFFICIENT; SPATIAL_EDGE_COUNT],
            corner_density: [DENSITY_SUFFICIENT; SPATIAL_CORNER_COUNT],
            depth: [true; DEPTH_COVERAGE_BINS],
            skew: [true; SKEW_COVERAGE_BINS],
        }
    }

    #[test]
    fn density_snapshot_is_zero_until_a_corner_reaches_sufficient_density() {
        let empty = DatasetQuality::default();
        assert!(empty.heatmap.is_valid());
        assert!(empty.heatmap.samples.iter().all(|density| *density == 0.0));

        let col = DENSITY_COLS / 2;
        let row = DENSITY_ROWS / 2;
        let corner = CalibrationPoint {
            x: ((col as f64 + 0.5) * f64::from(image_size().width) / DENSITY_COLS as f64) as f32,
            y: ((row as f64 + 0.5) * f64::from(image_size().height) / DENSITY_ROWS as f64) as f32,
        };
        let mut density = CornerDensityMap::new();
        assert!(density.add_frame(
            &detection_with(vec![corner]),
            &measurement([0.0; 3], [0.0, 0.0, 600.0]),
        ));
        let snapshot = density.snapshot();
        let center_index = row * DENSITY_COLS + col;
        assert!(snapshot.heatmap.samples[center_index] >= DENSITY_SUFFICIENT);
        assert_eq!(snapshot.heatmap.samples[0], 0.0);
    }

    #[test]
    fn invalid_corners_are_a_noop() {
        let mut density = CornerDensityMap::new();
        let invalid = detection_with(vec![CalibrationPoint {
            x: f32::NAN,
            y: 0.0,
        }]);
        assert!(!density.add_frame(&invalid, &measurement([0.0; 3], [0.0, 0.0, 600.0]),));
        let snapshot = density.snapshot();
        assert_eq!(snapshot.accepted_frames, 0);
        assert!(
            snapshot
                .heatmap
                .samples
                .iter()
                .all(|density| *density == 0.0)
        );
    }

    #[test]
    fn near_duplicate_view_is_rejected_before_capture_mutates_density() {
        let first_measurement = measurement([0.0; 3], [0.0, 0.0, 600.0]);
        let detection = detection_with(vec![point(0.5, 0.5)]);
        let mut density = CornerDensityMap::new();
        assert!(density.add_frame(&detection, &first_measurement));
        let before = density.snapshot();

        assert!(density.is_near_duplicate(&first_measurement));
        if !density.is_near_duplicate(&first_measurement) {
            assert!(density.add_frame(&detection, &first_measurement));
        }
        let after = density.snapshot();
        assert_eq!(after.accepted_frames, 1);
        assert_eq!(after.heatmap.samples, before.heatmap.samples);

        let novel = measurement([0.0; 3], [30.0, 0.0, 600.0]);
        assert!(!density.is_near_duplicate(&novel));
    }

    #[test]
    fn central_only_or_one_sided_observations_do_not_complete_spatial_quality() {
        let pose = measurement([0.0; 3], [0.0, 0.0, 600.0]);
        let mut center_density = CornerDensityMap::new();
        assert!(center_density.add_frame(&detection_with(vec![point(0.5, 0.5)]), &pose));
        let mut center_quality = center_density.snapshot();
        center_quality.depth = [true; DEPTH_COVERAGE_BINS];
        center_quality.skew = [true; SKEW_COVERAGE_BINS];
        assert!(center_quality.center_complete());
        assert!(!center_quality.edges_complete());
        assert!(!center_quality.is_complete());

        let mut left_density = CornerDensityMap::new();
        assert!(left_density.add_frame(&detection_with(vec![point(0.02, 0.5)]), &pose));
        let left_quality = left_density.snapshot();
        assert_eq!(left_quality.covered_edges(), 1);
        assert_eq!(left_quality.covered_corners(), 0);
    }

    #[test]
    fn all_outer_edges_and_corners_require_continuous_outer_density() {
        let pose = measurement([0.0; 3], [0.0, 0.0, 600.0]);
        let corners = vec![
            point(0.02, 0.02),
            point(0.98, 0.02),
            point(0.98, 0.98),
            point(0.02, 0.98),
        ];
        let mut density = CornerDensityMap::new();
        assert!(density.add_frame(&detection_with(corners), &pose));
        let quality = density.snapshot();
        assert!(quality.edges_complete());
        assert!(quality.corners_complete());
        assert_eq!(quality.covered_edges(), SPATIAL_EDGE_COUNT);
        assert_eq!(quality.covered_corners(), SPATIAL_CORNER_COUNT);
    }

    #[test]
    fn retained_depth_and_skew_bins_keep_their_existing_boundaries() {
        let depths = [
            (300.0, 168.0),
            (400.0, 216.0),
            (600.0, 324.0),
            (1000.0, 540.0),
        ];
        let tilts = [0.0_f64, 15.0, 30.0, 45.0];
        let mut density = CornerDensityMap::new();
        for ((width, height), tilt) in depths.into_iter().zip(tilts) {
            assert!(density.add_frame(
                &detection_with(corner_cloud(960.0, 540.0, width, height)),
                &measurement([tilt.to_radians(), 0.0, 0.0], [0.0, 0.0, 1000.0]),
            ));
        }
        let quality = density.snapshot();
        assert_eq!(quality.depth, [true; DEPTH_COVERAGE_BINS]);
        assert_eq!(quality.skew, [true; SKEW_COVERAGE_BINS]);
    }

    #[test]
    fn completion_has_no_frame_count_gate_but_requires_each_quality_dimension() {
        let quality = complete_quality();
        assert_eq!(quality.accepted_frames, 0);
        assert!(quality.is_complete());

        let mut missing_center = quality.clone();
        missing_center.center_density = f32::from_bits(DENSITY_SUFFICIENT.to_bits() - 1);
        assert!(!missing_center.is_complete());

        let mut missing_edge = quality.clone();
        missing_edge.edge_density[2] = f32::from_bits(DENSITY_SUFFICIENT.to_bits() - 1);
        assert!(!missing_edge.is_complete());

        let mut missing_corner = quality.clone();
        missing_corner.corner_density[3] = f32::from_bits(DENSITY_SUFFICIENT.to_bits() - 1);
        assert!(!missing_corner.is_complete());

        let mut missing_depth = quality.clone();
        missing_depth.depth[1] = false;
        assert!(!missing_depth.is_complete());

        let mut missing_skew = quality;
        missing_skew.skew[3] = false;
        assert!(!missing_skew.is_complete());
    }

    #[test]
    #[should_panic(expected = "密度场分辨率至少为 8×8")]
    fn heatmap_rejects_subminimum_resolution() {
        let _ = DensityHeatmap::zeroed(7, 8);
    }

    #[test]
    fn detected_outline_pixels_use_outer_board_frame() {
        let measurement = GuidedPoseMeasurement {
            pose: guided_pose_6dof_from_rotation_translation(
                board(),
                identity(),
                [0.0, 0.0, 600.0],
                &intrinsics(),
                image_size(),
            )
            .expect("fronto pose should be valid"),
            board: board(),
            initial_intrinsics: intrinsics(),
            image_size: image_size(),
        };
        let detected = detected_outline_pixels(&measurement);
        // 11×8 内角点、40mm：外框 12×9 格 → 480×360mm；fx=fy=900，z=600mm。
        let expected = [
            [
                (-1.0 * 40.0 * 900.0 / 600.0) + 980.0,
                (-1.0 * 40.0 * 900.0 / 600.0) + 540.0,
            ],
            [
                (11.0 * 40.0 * 900.0 / 600.0) + 980.0,
                (-1.0 * 40.0 * 900.0 / 600.0) + 540.0,
            ],
            [
                (11.0 * 40.0 * 900.0 / 600.0) + 980.0,
                (8.0 * 40.0 * 900.0 / 600.0) + 540.0,
            ],
            [
                (-1.0 * 40.0 * 900.0 / 600.0) + 980.0,
                (8.0 * 40.0 * 900.0 / 600.0) + 540.0,
            ],
        ];
        for (actual, expected) in detected.iter().zip(expected) {
            assert!((f64::from(actual[0]) - expected[0]).abs() <= 1.0e-3);
            assert!((f64::from(actual[1]) - expected[1]).abs() <= 1.0e-3);
        }
    }
}
