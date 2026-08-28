//! 双路 RTSP 预览与稳定检测自动采集。
//!
//! 预览：RTSP H.264 解码帧用于实时显示与棋盘检测；检测到棋盘后按帧间
//! 位姿抖动（jitter）做稳定判定，连续 hold 满 `HOLD_TARGET` 帧即触发采集。
//! dataset：真实 X5 采集在 hold 通过后按 RTSP SEI `timestamp_ns` 从 TCP 9073
//! 精确回查同源 NV12/YUV 原图；合成模式仅用于本机 UI 测试，保留 RGBA→luma fallback。
//! 无板验证：`PONGBOT_SYNTH=1` 用非棋盘合成帧（保证检测失败，验证采集链路）。
//! 采集质量：每张已采帧的 raw 亚像素角点实时进入求解，并以数值可观测性
//! 作为 dataset goal；密度热力图只保留为画面 overlay，不再生成旧版覆盖/bin 指标。
use crate::guide_overlay::{DensityHeatmap, OverlayData, OverlayStatus};
use crate::observability::ObservabilityReport;
use crate::solve::{DetectedDatasetFrame, solve_channel_from_detections};
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
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// 连续密度场的固定分辨率；只保存累计场，不保存角点历史。
///
/// 128×72 使两个方向的格间距均小于核 σ（h_v/σ≈0.93），保证各向同性采样；
/// 64×36 下 v 方向格间距约 1.85σ，修复后核在 v 方向欠采样。
const DENSITY_COLS: usize = 128;
const DENSITY_ROWS: usize = 72;
/// 高斯核宽度（归一化图像坐标）。3σ≈0.09，略小于满幅棋盘角点间距（约 0.10），
/// 相邻角点核基本不融合：单帧角点处密度约 1.0、间隙约 0.5，保留局部分布细节，
/// 不会像 0.06 那样把密度扩散到无角点区域。
const DENSITY_KERNEL_SIGMA: f64 = 0.03;
/// 达到充分（绿）所需的等效角点观测数：单帧角点峰值计 1，
/// 因此至少六帧覆盖同一区域才变绿，红→黄→绿共六档渐进过渡，
/// 避免"一下全绿"。
const DENSITY_SUFFICIENT: f32 = 6.0;
/// 永不驱逐的紧凑位姿占用网格。一个单元等于既有 near-duplicate 完整容差，
/// 因此相邻单元查询覆盖恰好跨越量化边界的旧容差内姿态。
const DUPLICATE_VIEW_JITTER_LIMIT: f64 = 0.40;
const POSE_OCCUPANCY_DEPTH_FRACTION: f64 = GUIDED_HOLD_JITTER_Z_LIMIT * DUPLICATE_VIEW_JITTER_LIMIT;
const POSE_OCCUPANCY_LATERAL_STEP: f64 = GUIDED_HOLD_JITTER_XYZ_LIMIT * DUPLICATE_VIEW_JITTER_LIMIT;
/// `ln(z)` 的步长略大于 1.6% 相对深度容差的最大对数距离，保证 ±1 格足够。
const POSE_OCCUPANCY_LOG_DEPTH_STEP: f64 = 0.0162;
const POSE_OCCUPANCY_ANGLE_STEP_DEGREES: f64 =
    GUIDED_HOLD_JITTER_RPY_DEGREES * DUPLICATE_VIEW_JITTER_LIMIT;
const POSE_OCCUPANCY_ANGLE_BINS: i32 = 450;
const POSE_OCCUPANCY_NEIGHBOR_RADIUS: i32 = 1;
/// 稳定检测的 hold 连续帧数（满则触发采集）。
pub const HOLD_TARGET: u8 = 3;
const GUIDED_HOLD_JITTER_XYZ_LIMIT: f64 = 0.025;
const GUIDED_HOLD_JITTER_Z_LIMIT: f64 = 0.04;
const GUIDED_HOLD_JITTER_RPY_DEGREES: f64 = 2.0;
/// 无新帧时的状态回报检查周期（事件驱动下检测本身不受此节拍限制）。
const DETECT_INTERVAL: Duration = Duration::from_millis(150);
/// 成功拍摄后角点 overlay 保留显示时长（确认触发瞬间采到的棋盘姿态）。
const CAPTURE_CORNERS_DISPLAY: Duration = Duration::from_millis(1500);

/// X5 TCP 控制端口；RTSP 只做引导，dataset 通过该端口抓同源 NV12 原图。
const X5_TCP_CONTROL_PORT: u16 = 9073;

/// 数据集质量的 UI 快照。
///
/// 热力图只用于预览 overlay；dataset 是否完成只由 solver 最小视图数与最新数值可观测性决定。
/// 不再维护旧版中心/边缘/四角/距离/倾斜 bin 统计。
#[derive(Clone, Debug, PartialEq)]
pub struct DatasetQuality {
    /// 已接受、已 raw 重检且已写入 dataset 的帧数；同时代表 solver 可用视图数。
    pub accepted_frames: usize,
    pub heatmap: DensityHeatmap,
    /// 最新标定解附近的数值可观测性；唯一决定 dataset goal 是否达标。
    pub observability: Option<ObservabilityReport>,
}

impl Default for DatasetQuality {
    fn default() -> Self {
        Self {
            accepted_frames: 0,
            heatmap: DensityHeatmap::zeroed(DENSITY_COLS, DENSITY_ROWS),
            observability: None,
        }
    }
}

impl DatasetQuality {
    /// dataset goal 只由最新数值可观测性决定。
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.solver_input_ready()
            && self
                .observability
                .as_ref()
                .is_some_and(ObservabilityReport::goal_met)
    }

    #[must_use]
    pub fn solver_input_ready(&self) -> bool {
        self.accepted_frames >= crate::solve::MIN_USABLE_CALIBRATION_VIEWS
    }
}

/// 紧凑的量化姿态占用 key；只保存一个 6D 单元，不保留原始姿态或逐帧历史。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct QuantizedViewCell([i32; 6]);

/// 单路常量内存的角点质量累加器。
struct CornerDensityMap {
    field: Vec<f32>,
    /// 所有接受视角的持久量化占用；绝不按数量淘汰旧单元。
    occupied_views: HashSet<QuantizedViewCell>,
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
    /// overlay 引导文本（未检测到棋盘 / hold 状态 / 数值可观测性状态）。
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
    /// 已 raw 重检并完成亚像素细化的检测缓存；实时求解复用，避免重复检测历史帧。
    detections: Arc<Mutex<Vec<DetectedDatasetFrame>>>,
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
                    // 合成棋盘（12×9 格 = 11×8 内角点）：用于本机验证检测、采集、热力图 overlay
                    // 与 H2 实时可观测性分析；姿态仍偏合成化，可能触发未满秩提示。
                    let cols = 12i32;
                    let rows = 9i32;
                    let slow = tick / 4;
                    let stage_len = 60u32;
                    let phase = (slow / stage_len) % 6;
                    let t = (slow % stage_len) as f32 / (stage_len - 1) as f32;
                    let (center_x, center_y, cell, shear) = match phase {
                        // 中心的远→近扫描保留四档距离覆盖，并激励中心半径。
                        0 => (0.5, 0.5, 13.0 + 41.0 * t, 0.0),
                        // 四个外围角落分别激励相邻边缘和对应角落的连续密度。
                        1 => (0.18, 0.18, 16.0, 0.0),
                        2 => (0.82, 0.18, 16.0, 0.0),
                        3 => (0.82, 0.82, 16.0, 0.0),
                        4 => (0.18, 0.82, 16.0, 0.0),
                        // 斜切会使已有姿态估计跨过保留的倾斜档位。
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
            detections: Arc::new(Mutex::new(Vec::new())),
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
        let (analysis_tx, analysis_rx) = mpsc::channel();
        let analysis_pending = Arc::new(AtomicBool::new(false));
        let captured_overlay: CapturedCornerOverlaySlot = Arc::new(Mutex::new(None));

        let analysis_capturing = Arc::clone(&self.capturing);
        let analysis_pending_for_worker = Arc::clone(&analysis_pending);
        let analysis_captured = Arc::clone(&self.captured);
        let analysis_poses = Arc::clone(&self.poses);
        let analysis_detections = Arc::clone(&self.detections);
        let analysis_overlay = Arc::clone(&self.overlay);
        let analysis_guide = Arc::clone(&self.guide_state);
        let analysis_captured_overlay = Arc::clone(&captured_overlay);
        std::thread::spawn(move || {
            dataset_analysis_loop(
                analysis_capturing,
                analysis_pending_for_worker,
                analysis_rx,
                analysis_captured,
                analysis_poses,
                analysis_detections,
                analysis_guide,
                analysis_overlay,
                analysis_captured_overlay,
                board,
            );
        });

        let capturing = Arc::clone(&self.capturing);
        let slot = Arc::clone(&self.slot);
        let overlay = Arc::clone(&self.overlay);
        let guide = Arc::clone(&self.guide_state);
        let rtsp_error = Arc::clone(&self.rtsp_error);
        let capture_source = self.capture_source.clone();
        std::thread::spawn(move || {
            capture_loop(
                capturing,
                slot,
                analysis_tx,
                analysis_pending,
                guide,
                overlay,
                captured_overlay,
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

    /// 是否满足数值可观测性 dataset goal。
    pub fn complete(&self) -> bool {
        let state = self
            .guide_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.quality.is_complete()
    }

    /// 取已采 dataset 帧（兼容旧求解入口；真实设备为 TCP NV12/Y plane）。
    pub fn captured_frames(&self) -> Vec<Arc<CapturedDatasetFrame>> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 取已 raw 重检的 dataset 角点缓存；最终求解与实时可观测性共用。
    pub fn captured_detections(&self) -> Vec<DetectedDatasetFrame> {
        self.detections
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

struct PendingDatasetAnalysis {
    dataset_frame: CapturedDatasetFrame,
    display_width: u32,
    display_height: u32,
}

type CapturedCornerOverlaySlot = Arc<Mutex<Option<(std::time::Instant, Vec<[f32; 2]>)>>>;

fn current_guide_quality(guide: &Arc<Mutex<GuideState>>) -> DatasetQuality {
    guide
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .quality
        .clone()
}

fn captured_corners_snapshot(
    captured_overlay: &CapturedCornerOverlaySlot,
    board: BoardSpec,
) -> Option<(usize, usize, Vec<[f32; 2]>)> {
    captured_overlay
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|(time, _)| time.elapsed() < CAPTURE_CORNERS_DISPLAY)
        .map(|(_, corners)| {
            (
                usize::from(board.inner_cols),
                usize::from(board.inner_rows),
                corners.clone(),
            )
        })
}

/// 稳定检测采集循环：只负责 RTSP 检测、hold 判定和及时触发 TCP raw 抓帧。
/// raw 重检、亚像素角点、入库去重和实时求解由 `dataset_analysis_loop` 独立处理。
#[allow(clippy::too_many_arguments)]
fn capture_loop(
    capturing: Arc<AtomicBool>,
    slot: Arc<LatestDecodedFrameSlot>,
    analysis_tx: mpsc::Sender<PendingDatasetAnalysis>,
    analysis_pending: Arc<AtomicBool>,
    guide: Arc<Mutex<GuideState>>,
    overlay: Arc<Mutex<Option<OverlayData>>>,
    captured_overlay: CapturedCornerOverlaySlot,
    rtsp_error: Arc<Mutex<Option<String>>>,
    capture_source: DatasetCaptureSource,
    board: BoardSpec,
) {
    let backend = OpenCvCalibrationBackend;
    let cancellation = CalibrationCancellation::default();
    let mut hold_frames: u8 = 0;
    let mut last_measurement: Option<GuidedPoseMeasurement> = None;
    tracing::info!(
        "稳定检测采集 worker 已启动（hold {HOLD_TARGET} 帧；raw/subpixel 与实时求解在独立线程）"
    );
    // 上一轮已处理帧：非消费等待下一新帧，避免重复处理同一帧。
    let mut last_processed: Option<Arc<DecodedVideoFrame>> = None;
    loop {
        let quality = current_guide_quality(&guide);
        if quality.is_complete() {
            set_guide_quality(
                &guide,
                &format!(
                    "数值可观测性完成（已采 {} 张），采集结束",
                    quality.accepted_frames
                ),
                0,
                &quality,
            );
            break;
        }
        // 事件驱动取最新帧：非消费等待「新帧到达或首帧」，不抢走显示路径的帧。
        let frame = match slot.wait_until_changed_timeout(last_processed.as_ref(), DETECT_INTERVAL)
        {
            Some(frame) => frame,
            None => {
                let quality = current_guide_quality(&guide);
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
                continue;
            }
        };
        last_processed = Some(frame.clone());

        let image_size = CalibrationImageSize {
            width: frame.width,
            height: frame.height,
        };
        let initial = default_initial_intrinsics(frame.width, frame.height);
        let captured_corners = captured_corners_snapshot(&captured_overlay, board);
        let png = match encode_png(&frame.rgba, frame.width, frame.height) {
            Ok(png) => png,
            Err(error) => {
                let quality = current_guide_quality(&guide);
                set_guide_quality(&guide, &format!("PNG 编码失败：{error}"), 0, &quality);
                continue;
            }
        };
        match backend.detect_png(&png, image_size, 256 * 1024 * 1024, board, &cancellation) {
            Ok(ChessboardDetectionOutcome::Found(detection)) => {
                let view = match backend.estimate_pose(&detection, &initial, board, &cancellation) {
                    Ok(view) => view,
                    Err(error) => {
                        hold_frames = 0;
                        last_measurement = None;
                        let quality = current_guide_quality(&guide);
                        set_guide_quality(
                            &guide,
                            &format!("检测到棋盘，姿态估计失败：{error}"),
                            0,
                            &quality,
                        );
                        continue;
                    }
                };
                let measurement = match guided_pose_measurement(&view, board, &initial, image_size)
                {
                    Ok(measurement) => measurement,
                    Err(error) => {
                        hold_frames = 0;
                        last_measurement = None;
                        let quality = current_guide_quality(&guide);
                        set_guide_quality(&guide, &format!("棋盘位姿无效：{error}"), 0, &quality);
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

                let quality = current_guide_quality(&guide);
                if !capturing.load(Ordering::Acquire) {
                    set_guide_quality(&guide, "等待自动采集启动…", 0, &quality);
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
                    continue;
                }
                if !has_usable_corners(&detection) {
                    hold_frames = 0;
                    last_measurement = None;
                    set_guide_quality(&guide, "检测角点无效，未抓取原图", 0, &quality);
                    continue;
                }
                if analysis_pending
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    hold_frames = 0;
                    last_measurement = None;
                    set_guide_quality(
                        &guide,
                        "上一张 raw/subpixel 与可观测性分析仍在后台进行，请稍候",
                        0,
                        &quality,
                    );
                    continue;
                }

                let dataset_frame = match capture_dataset_frame(&capture_source, &frame) {
                    Ok(frame) => frame,
                    Err(error) => {
                        analysis_pending.store(false, Ordering::Release);
                        hold_frames = 0;
                        last_measurement = None;
                        set_guide_quality(
                            &guide,
                            &format!("TCP YUV 原图抓取失败：{error}"),
                            0,
                            &quality,
                        );
                        continue;
                    }
                };
                let pending = PendingDatasetAnalysis {
                    dataset_frame,
                    display_width: frame.width,
                    display_height: frame.height,
                };
                if analysis_tx.send(pending).is_err() {
                    analysis_pending.store(false, Ordering::Release);
                    break;
                }
                hold_frames = 0;
                last_measurement = None;
                set_guide_quality(
                    &guide,
                    "已抓取 raw 原图，后台进行亚像素检测和可观测性分析…",
                    0,
                    &quality,
                );
            }
            Ok(ChessboardDetectionOutcome::NotFound { .. }) => {
                hold_frames = 0;
                last_measurement = None;
                let quality = current_guide_quality(&guide);
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
                let quality = current_guide_quality(&guide);
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
    }
}

/// dataset 分析线程：串行处理每路 raw 帧的亚像素重检、去重、实时求解和可观测性。
#[allow(clippy::too_many_arguments)]
fn dataset_analysis_loop(
    capturing: Arc<AtomicBool>,
    analysis_pending: Arc<AtomicBool>,
    analysis_rx: mpsc::Receiver<PendingDatasetAnalysis>,
    captured: Arc<Mutex<Vec<Arc<CapturedDatasetFrame>>>>,
    poses: Arc<Mutex<Vec<[f64; 3]>>>,
    detections: Arc<Mutex<Vec<DetectedDatasetFrame>>>,
    guide: Arc<Mutex<GuideState>>,
    overlay: Arc<Mutex<Option<OverlayData>>>,
    captured_overlay: CapturedCornerOverlaySlot,
    board: BoardSpec,
) {
    let backend = OpenCvCalibrationBackend;
    let cancellation = CalibrationCancellation::default();
    let mut density = CornerDensityMap::new();
    let mut quality = DatasetQuality::default();
    let mut last_observability: Option<ObservabilityReport> = None;
    tracing::info!("dataset analysis worker 已启动（raw/subpixel + realtime solve）");

    while let Ok(pending) = analysis_rx.recv() {
        let channel = pending.dataset_frame.channel;
        let validated =
            match validate_dataset_frame(&backend, pending.dataset_frame, board, &cancellation) {
                Ok(validated) => validated,
                Err(error) => {
                    set_guide_quality(
                        &guide,
                        &format!("TCP 原图棋盘重检未通过，未入库：{error}"),
                        0,
                        &quality,
                    );
                    analysis_pending.store(false, Ordering::Release);
                    continue;
                }
            };
        let measurement = validated.measurement.clone();
        let raw_detection = {
            let mut captured = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut poses = poses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut detections = detections
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match commit_solver_validated_capture(
                &mut density,
                &mut captured,
                &mut poses,
                &mut detections,
                validated,
            ) {
                Ok(Some(detection)) => detection,
                Ok(None) => {
                    set_guide_quality(
                        &guide,
                        "TCP 原图近重复视角已拒绝，请移动或转动棋盘后再保持稳定",
                        0,
                        &quality,
                    );
                    analysis_pending.store(false, Ordering::Release);
                    continue;
                }
                Err(error) => {
                    set_guide_quality(
                        &guide,
                        &format!("TCP 原图校验失败，未入库：{error}"),
                        0,
                        &quality,
                    );
                    analysis_pending.store(false, Ordering::Release);
                    continue;
                }
            }
        };
        quality = density.snapshot();
        let cached_detections = detections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if cached_detections.len() >= crate::solve::MIN_USABLE_CALIBRATION_VIEWS {
            match solve_channel_from_detections(
                channel,
                &cached_detections,
                board,
                last_observability.as_ref(),
            ) {
                Ok(result) => {
                    quality.observability = result.observability.clone();
                    last_observability = result.observability;
                }
                Err(error) => {
                    quality.observability = last_observability.clone();
                    tracing::warn!(channel, "实时标定/可观测性求解失败：{error}");
                }
            }
        }

        let corners_px = captured_corners_for_overlay(
            &raw_detection,
            pending.display_width,
            pending.display_height,
        );
        {
            let mut slot = captured_overlay
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = Some((std::time::Instant::now(), corners_px.clone()));
        }
        publish_overlay(
            &overlay,
            pending.display_width,
            pending.display_height,
            Some(&measurement),
            Some(HOLD_TARGET),
            Some((
                usize::from(board.inner_cols),
                usize::from(board.inner_rows),
                corners_px,
            )),
        );
        if quality.is_complete() {
            set_guide_quality(
                &guide,
                &format!(
                    "数值可观测性达标（已采 {} 张），采集结束",
                    quality.accepted_frames
                ),
                0,
                &quality,
            );
            capturing.store(false, Ordering::Release);
        } else {
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
        analysis_pending.store(false, Ordering::Release);
    }
}

fn has_usable_corners(detection: &ChessboardDetection) -> bool {
    detection
        .corners
        .iter()
        .any(|corner| normalized_corner(corner, detection.image_size).is_some())
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

fn quantize_linear(value: f64, step: f64) -> Option<i32> {
    if !value.is_finite() || !step.is_finite() || step <= 0.0 {
        return None;
    }
    let cell = (value / step).floor();
    (cell.is_finite() && cell >= f64::from(i32::MIN) && cell <= f64::from(i32::MAX))
        .then_some(cell as i32)
}

fn quantize_angle_degrees(angle: f64) -> Option<i32> {
    if !angle.is_finite() {
        return None;
    }
    let cell = (angle.rem_euclid(360.0) / POSE_OCCUPANCY_ANGLE_STEP_DEGREES).floor();
    (cell.is_finite() && cell >= 0.0 && cell < f64::from(POSE_OCCUPANCY_ANGLE_BINS))
        .then_some(cell as i32)
}

fn wrap_occupancy_angle_bin(value: i32) -> i32 {
    value.rem_euclid(POSE_OCCUPANCY_ANGLE_BINS)
}

/// 将旧 `Δx/max(z, 1)` 容差保守地投影到量化横向轴，覆盖相邻深度格引起的漂移。
fn occupancy_lateral_neighbor_radius(lateral: f64) -> Option<i32> {
    if !lateral.is_finite() {
        return None;
    }
    let min_depth_ratio = 1.0 - POSE_OCCUPANCY_DEPTH_FRACTION;
    if !min_depth_ratio.is_finite() || min_depth_ratio <= 0.0 {
        return None;
    }
    let depth_ratio_inverse = 1.0 / min_depth_ratio;
    let maximum_lateral_delta = POSE_OCCUPANCY_LATERAL_STEP * depth_ratio_inverse
        + lateral.abs() * (depth_ratio_inverse - 1.0);
    let radius = (maximum_lateral_delta / POSE_OCCUPANCY_LATERAL_STEP).ceil() + 1.0;
    (radius.is_finite() && radius >= 0.0 && radius <= f64::from(i32::MAX)).then_some(radius as i32)
}

fn quantized_view_cell(pose: &GuidedPose6Dof) -> Option<QuantizedViewCell> {
    let depth = pose.xyz[2];
    if !depth.is_finite() || depth <= 0.0 {
        return None;
    }
    let depth_scale = depth.max(1.0);
    Some(QuantizedViewCell([
        quantize_linear(pose.xyz[0] / depth_scale, POSE_OCCUPANCY_LATERAL_STEP)?,
        quantize_linear(pose.xyz[1] / depth_scale, POSE_OCCUPANCY_LATERAL_STEP)?,
        quantize_linear(depth_scale.ln(), POSE_OCCUPANCY_LOG_DEPTH_STEP)?,
        quantize_angle_degrees(pose.rpy_degrees[0])?,
        quantize_angle_degrees(pose.rpy_degrees[1])?,
        quantize_angle_degrees(pose.rpy_degrees[2])?,
    ]))
}

impl CornerDensityMap {
    fn new() -> Self {
        debug_assert!(DENSITY_COLS >= 8 && DENSITY_ROWS >= 8);
        Self {
            field: vec![0.0; DENSITY_COLS * DENSITY_ROWS],
            occupied_views: HashSet::new(),
            accepted_frames: 0,
        }
    }

    /// 只有至少一个位于图像内的有限角点，才允许进行 TCP 原图抓取。
    fn has_usable_corners(&self, detection: &ChessboardDetection) -> bool {
        has_usable_corners(detection)
    }

    /// 持久量化占用查询保持旧 jitter 同义：深度变化会按 `Δx/max(z, 1)` 放宽横向邻域。
    fn is_near_duplicate(&self, measurement: &GuidedPoseMeasurement) -> bool {
        quantized_view_cell(&measurement.pose)
            .is_none_or(|cell| self.occupancy_neighborhood_contains(cell, &measurement.pose))
    }

    fn occupancy_neighborhood_contains(
        &self,
        cell: QuantizedViewCell,
        pose: &GuidedPose6Dof,
    ) -> bool {
        let pose_depth_scale = pose.xyz[2].max(1.0);
        let (Some(lateral_x_radius), Some(lateral_y_radius)) = (
            occupancy_lateral_neighbor_radius(pose.xyz[0] / pose_depth_scale),
            occupancy_lateral_neighbor_radius(pose.xyz[1] / pose_depth_scale),
        ) else {
            return true;
        };
        let [x, y, depth_cell, roll, pitch, yaw] = cell.0;
        for delta_x in -lateral_x_radius..=lateral_x_radius {
            for delta_y in -lateral_y_radius..=lateral_y_radius {
                for delta_depth in -POSE_OCCUPANCY_NEIGHBOR_RADIUS..=POSE_OCCUPANCY_NEIGHBOR_RADIUS
                {
                    for delta_roll in
                        -POSE_OCCUPANCY_NEIGHBOR_RADIUS..=POSE_OCCUPANCY_NEIGHBOR_RADIUS
                    {
                        for delta_pitch in
                            -POSE_OCCUPANCY_NEIGHBOR_RADIUS..=POSE_OCCUPANCY_NEIGHBOR_RADIUS
                        {
                            for delta_yaw in
                                -POSE_OCCUPANCY_NEIGHBOR_RADIUS..=POSE_OCCUPANCY_NEIGHBOR_RADIUS
                            {
                                let candidate = QuantizedViewCell([
                                    x.saturating_add(delta_x),
                                    y.saturating_add(delta_y),
                                    depth_cell.saturating_add(delta_depth),
                                    wrap_occupancy_angle_bin(roll.saturating_add(delta_roll)),
                                    wrap_occupancy_angle_bin(pitch.saturating_add(delta_pitch)),
                                    wrap_occupancy_angle_bin(yaw.saturating_add(delta_yaw)),
                                ]);
                                if self.occupied_views.contains(&candidate) {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// solver-valid raw 帧一次性写入热力图 overlay 与持久位姿占用。
    /// 返回 `false` 表示无效或重复输入，累加器不发生改变。
    fn add_frame(
        &mut self,
        detection: &ChessboardDetection,
        measurement: &GuidedPoseMeasurement,
    ) -> bool {
        if !self.has_usable_corners(detection) {
            return false;
        }
        let Some(occupancy) = quantized_view_cell(&measurement.pose) else {
            return false;
        };
        if self.occupancy_neighborhood_contains(occupancy, &measurement.pose) {
            return false;
        }
        let image_size = detection.image_size;
        let mut has_valid_corner = false;
        for corner in &detection.corners {
            let Some([u, v]) = normalized_corner(corner, image_size) else {
                continue;
            };
            has_valid_corner = true;
            self.splat_density(u, v);
        }
        if !has_valid_corner {
            return false;
        }
        self.occupied_views.insert(occupancy);
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

    fn snapshot(&self) -> DatasetQuality {
        let heatmap = DensityHeatmap {
            cols: DENSITY_COLS,
            rows: DENSITY_ROWS,
            samples: self.field.clone().into(),
            sufficient_level: DENSITY_SUFFICIENT,
        };
        DatasetQuality {
            accepted_frames: self.accepted_frames,
            heatmap,
            observability: None,
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

fn add_density(slot: &mut f32, contribution: f32) {
    *slot = (*slot + contribution).min(f32::MAX);
}

fn quality_missing_hint(quality: &DatasetQuality) -> &'static str {
    if !quality.solver_input_ready() {
        "请继续采集，达到实时求解的最小有效视图数"
    } else if let Some(report) = &quality.observability {
        report.missing_hint()
    } else {
        "等待最新 dataset 标定与可观测性分析"
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

/// 经 solver 同规则 raw 棋盘检测与位姿估计验证、但尚未提交的 dataset 帧。
struct SolverValidatedCapture {
    dataset_frame: CapturedDatasetFrame,
    detection: ChessboardDetection,
    measurement: GuidedPoseMeasurement,
    rotation_vector: [f64; 3],
}

/// 将 TCP raw dataset 帧用 `solve_channel` 复用的检测路径重新验证，并从该结果导出质量位姿。
fn validate_dataset_frame(
    backend: &OpenCvCalibrationBackend,
    dataset_frame: CapturedDatasetFrame,
    board: BoardSpec,
    cancellation: &CalibrationCancellation,
) -> Result<SolverValidatedCapture, String> {
    let detection =
        match crate::solve::detect_dataset_frame(backend, &dataset_frame, board, cancellation)? {
            ChessboardDetectionOutcome::Found(detection) => detection,
            ChessboardDetectionOutcome::NotFound { .. } => {
                return Err("未找到完整棋盘".to_owned());
            }
        };
    let initial = default_initial_intrinsics(dataset_frame.width, dataset_frame.height);
    let view = backend
        .estimate_pose(&detection, &initial, board, cancellation)
        .map_err(|error| format!("raw 棋盘位姿估计失败：{error}"))?;
    let measurement = guided_pose_measurement(&view, board, &initial, detection.image_size)
        .map_err(|error| format!("raw 棋盘位姿无效：{error}"))?;
    Ok(SolverValidatedCapture {
        dataset_frame,
        detection,
        measurement,
        rotation_vector: view.rotation_vector,
    })
}

/// 提交已验证 raw 帧；质量、dataset、pose 占用只在同一个成功分支中一起改变。
///
/// 返回 `Ok(None)` 表示 raw 位姿与已有持久占用重复，所有状态保持不变。
fn commit_solver_validated_capture(
    density: &mut CornerDensityMap,
    captured: &mut Vec<Arc<CapturedDatasetFrame>>,
    poses: &mut Vec<[f64; 3]>,
    detections: &mut Vec<DetectedDatasetFrame>,
    validated: SolverValidatedCapture,
) -> Result<Option<ChessboardDetection>, String> {
    let SolverValidatedCapture {
        dataset_frame,
        detection,
        measurement,
        rotation_vector,
    } = validated;
    if density.is_near_duplicate(&measurement) {
        return Ok(None);
    }
    if !density.add_frame(&detection, &measurement) {
        return Err("raw 重检结果没有可提交的有限角点或位姿".to_owned());
    }
    let frame = Arc::new(dataset_frame);
    captured.push(Arc::clone(&frame));
    poses.push(rotation_vector);
    detections.push(DetectedDatasetFrame {
        frame,
        detection: detection.clone(),
    });
    Ok(Some(detection))
}

/// 将 raw 检测角点映射到当前 RTSP 视频尺寸，保证入库确认 overlay 与视频对齐。
fn captured_corners_for_overlay(
    detection: &ChessboardDetection,
    display_width: u32,
    display_height: u32,
) -> Vec<[f32; 2]> {
    let source_width = detection.image_size.width.max(1) as f32;
    let source_height = detection.image_size.height.max(1) as f32;
    let scale_x = display_width as f32 / source_width;
    let scale_y = display_height as f32 / source_height;
    detection
        .corners
        .iter()
        .filter(|corner| corner.x.is_finite() && corner.y.is_finite())
        .map(|corner| [corner.x * scale_x, corner.y * scale_y])
        .collect()
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
            accepted_frames: crate::solve::MIN_USABLE_CALIBRATION_VIEWS,
            heatmap: DensityHeatmap::zeroed(DENSITY_COLS, DENSITY_ROWS),
            observability: Some(ObservabilityReport {
                view_count: crate::solve::MIN_USABLE_CALIBRATION_VIEWS,
                point_count: 100,
                rms_error: 0.1,
                max_view_rmse: 0.1,
                condition_number: 1.0e3,
                log_det_information: 0.0,
                last_info_gain: Some(1.0),
                focal_relative_stddev: [0.001, 0.001],
                principal_point_stddev_px: [0.5, 0.5],
                distortion_edge_stddev_px: vec![0.5; 12],
                distortion_names: vec![
                    "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
                ],
            }),
        }
    }

    #[test]
    fn single_capture_is_below_sufficient_and_six_overlapping_captures_qualify() {
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
        // 单帧单角点 = 1 个等效观测，低于 6 帧充分阈值：不绿。
        let single = snapshot.heatmap.samples[center_index];
        assert!(single >= 0.8 && single < DENSITY_SUFFICIENT);
        assert!(snapshot.heatmap.sufficient_fraction(0.5, 0.5) < 1.0);
        assert_eq!(snapshot.heatmap.samples[0], 0.0);

        // 同一角点位置再观测五帧：等效观测达到 6 帧充分阈值。
        let corner_u = (col as f64 + 0.5) / DENSITY_COLS as f64;
        let corner_v = (row as f64 + 0.5) / DENSITY_ROWS as f64;
        density.splat_density(corner_u, corner_v);
        density.splat_density(corner_u, corner_v);
        density.splat_density(corner_u, corner_v);
        density.splat_density(corner_u, corner_v);
        density.splat_density(corner_u, corner_v);
        let doubled = density.snapshot().heatmap.samples[center_index];
        assert!(doubled >= DENSITY_SUFFICIENT - 1e-4);
        assert!(doubled > single);
    }

    #[test]
    fn full_frame_board_single_capture_never_completes() {
        // 10×7 内角点铺满 0.05..0.95：模拟一张占满画幅的棋盘。
        let mut corners = Vec::new();
        for row in 0..7 {
            for col in 0..10 {
                corners.push(point(
                    0.05 + 0.1 * col as f64,
                    0.05 + (0.9 / 6.0) * row as f64,
                ));
            }
        }
        assert_eq!(corners.len(), 70);
        let mut density = CornerDensityMap::new();
        assert!(density.add_frame(
            &detection_with(corners),
            &measurement([0.0; 3], [0.0, 0.0, 600.0]),
        ));
        let quality = density.snapshot();
        assert_eq!(quality.accepted_frames, 1);
        // 单帧所有角点峰值仅 1 个等效观测：渲染侧峰值归一化不到 1。
        assert!(quality.heatmap.sufficient_fraction(0.5, 0.5) < 1.0);
        assert!(!quality.is_complete());
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
    fn persistent_occupancy_rejects_depth_coupled_legacy_repeat_after_more_than_64_novel_views() {
        let detection = detection_with(vec![point(0.5, 0.5)]);
        let mut first = measurement([0.0; 3], [0.0, 0.0, 600.0]);
        first.pose.xyz = [612.06, 0.0, 600.0];
        let mut repeated = first.clone();
        repeated.pose.xyz[2] = 609.0;
        let first_cell = quantized_view_cell(&first.pose).expect("finite pose should quantize");
        let repeated_cell =
            quantized_view_cell(&repeated.pose).expect("finite pose should quantize");
        assert_eq!(first_cell.0[0], 102);
        assert_eq!(repeated_cell.0[0], 100);
        assert_eq!((first_cell.0[0] - repeated_cell.0[0]).abs(), 2);
        assert!(
            guided_pose_jitter_score(
                first.pose.xyz,
                first.pose.rpy_degrees,
                repeated.pose.xyz,
                repeated.pose.rpy_degrees,
            ) < DUPLICATE_VIEW_JITTER_LIMIT
        );

        let mut density = CornerDensityMap::new();
        assert!(density.add_frame(&detection, &first));
        for index in 1..=64 {
            let mut novel = first.clone();
            novel.pose.rpy_degrees[0] += f64::from(index) * 5.0;
            assert!(
                !density.is_near_duplicate(&novel),
                "view {index} should be novel"
            );
            assert!(density.add_frame(&detection, &novel));
        }
        assert_eq!(density.snapshot().accepted_frames, 65);
        assert!(density.is_near_duplicate(&repeated));
        assert!(!density.add_frame(&detection, &repeated));
        assert_eq!(density.snapshot().accepted_frames, 65);
    }

    #[test]
    fn occupancy_rejects_a_legacy_tolerance_pose_across_an_adjacent_quantization_boundary() {
        let detection = detection_with(vec![point(0.5, 0.5)]);
        let mut first = measurement([0.0; 3], [0.0, 0.0, 600.0]);
        let depth = first.pose.xyz[2];
        let raw_lateral = first.pose.xyz[0] / depth;
        let next_boundary =
            f64::from(quantize_linear(raw_lateral, POSE_OCCUPANCY_LATERAL_STEP).unwrap() + 1)
                * POSE_OCCUPANCY_LATERAL_STEP;
        first.pose.xyz[0] = (next_boundary - POSE_OCCUPANCY_LATERAL_STEP * 0.1) * depth;
        let first_cell = quantized_view_cell(&first.pose).expect("finite pose should quantize");
        let mut adjacent = first.clone();
        adjacent.pose.xyz[0] = (next_boundary + POSE_OCCUPANCY_LATERAL_STEP * 0.1) * depth;
        let adjacent_cell =
            quantized_view_cell(&adjacent.pose).expect("adjacent pose should quantize");
        assert_eq!(adjacent_cell.0[0], first_cell.0[0] + 1);
        assert!(
            guided_pose_jitter_score(
                first.pose.xyz,
                first.pose.rpy_degrees,
                adjacent.pose.xyz,
                adjacent.pose.rpy_degrees,
            ) < DUPLICATE_VIEW_JITTER_LIMIT
        );

        let mut density = CornerDensityMap::new();
        assert!(density.add_frame(&detection, &first));
        assert!(density.is_near_duplicate(&adjacent));
    }

    #[test]
    fn raw_redetection_failure_never_commits_dataset_quality_or_occupancy() {
        let frame = CapturedDatasetFrame {
            channel: 0,
            width: 64,
            height: 64,
            luma: vec![0; 64 * 64].into(),
            source: CapturedDatasetSource::X5TcpYuv {
                frame_id: 1,
                timestamp_ns: 1,
            },
        };
        let backend = OpenCvCalibrationBackend;
        let cancellation = CalibrationCancellation::default();
        let mut density = CornerDensityMap::new();
        let mut captured: Vec<Arc<CapturedDatasetFrame>> = Vec::new();
        let mut poses = Vec::new();
        let mut detections = Vec::new();
        let admission =
            validate_dataset_frame(&backend, frame, board(), &cancellation).and_then(|validated| {
                commit_solver_validated_capture(
                    &mut density,
                    &mut captured,
                    &mut poses,
                    &mut detections,
                    validated,
                )
            });

        assert!(admission.is_err());
        assert!(captured.is_empty());
        assert!(poses.is_empty());
        assert!(detections.is_empty());
        assert_eq!(density.snapshot().accepted_frames, 0);
        assert!(!density.is_near_duplicate(&measurement([0.0; 3], [0.0, 0.0, 600.0])));
    }

    #[test]
    fn completion_requires_solver_minimum_and_observability_goal_only() {
        let quality = complete_quality();
        assert_eq!(
            quality.accepted_frames,
            crate::solve::MIN_USABLE_CALIBRATION_VIEWS
        );
        assert!(quality.solver_input_ready());
        assert!(quality.is_complete());

        let mut insufficient_views = quality.clone();
        insufficient_views.accepted_frames = crate::solve::MIN_USABLE_CALIBRATION_VIEWS - 1;
        assert!(!insufficient_views.solver_input_ready());
        assert!(!insufficient_views.is_complete());

        let mut missing_observability = quality.clone();
        missing_observability.observability = None;
        assert!(!missing_observability.is_complete());

        let mut poor_focal = quality;
        poor_focal
            .observability
            .as_mut()
            .expect("test report")
            .focal_relative_stddev[0] = crate::observability::FOCAL_REL_STDDEV_TARGET * 2.0;
        assert!(!poor_focal.is_complete());
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
