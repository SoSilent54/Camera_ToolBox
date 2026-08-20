//! 节点间统一数据负载。
//!
//! 大负载（帧、检测结果、解）一律用 `Arc` 零拷贝传递；`Clone` 仅复制句柄。

use std::sync::Arc;

use camera_toolbox_core::{CalibrationSolution, ChessboardDetection};

#[cfg(test)]
use camera_toolbox_core::CalibrationImageSize;

use crate::platform::{DecodedVideoFrame, SourcePts, StreamFrameIdentity, StreamSessionId};

/// 图像平面：数据与每行物理步长分开保存，不能假设所有像素格式都紧密排列。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePlane {
    pub bytes: Arc<[u8]>,
    pub stride_bytes: u32,
}

impl ImagePlane {
    #[must_use]
    pub const fn new(bytes: Arc<[u8]>, stride_bytes: u32) -> Self {
        Self {
            bytes,
            stride_bytes,
        }
    }
}

/// 图像字节的显式布局。RAW 不会隐式 demosaic。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFrameFormat {
    Rgba8,
    Gray8,
    Gray16Le,
    Nv12,
    BayerRaw,
}

impl std::fmt::Display for ImageFrameFormat {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Rgba8 => "RGBA8",
            Self::Gray8 => "GRAY8",
            Self::Gray16Le => "GRAY16LE",
            Self::Nv12 => "NV12",
            Self::BayerRaw => "BAYER_RAW",
        })
    }
}

/// RAW Bayer 的 CFA 排列。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BayerPattern {
    Rggb,
    Bggr,
    Grbg,
    Gbrg,
}

/// RAW 解释所需的元数据；不携带时不得把 Bayer 数据当作彩色图像。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawMetadata {
    pub bayer_pattern: BayerPattern,
    pub bits_per_sample: u8,
    pub black_level: Option<u16>,
    pub white_level: Option<u16>,
}

/// 已知颜色空间和量化范围。缺失信息必须保持为 `None`，不能猜测。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSpace {
    Srgb,
    Bt601,
    Bt709,
    Bt2020,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorMetadata {
    pub color_space: ColorSpace,
    pub full_range: Option<bool>,
}

/// 帧来源；流来源保留稳定 session/channel，设备和文件来源保留其原始标识。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameProvenance {
    Stream {
        stream_id: StreamSessionId,
        channel: u16,
    },
    Device {
        driver: String,
        channel: u16,
        camera: Option<u16>,
        /// 驱动采集时间戳；不是本机单调时钟。
        timestamp_ns: u64,
    },
    File {
        source: String,
    },
    Unknown {
        reason: String,
    },
}

/// 跨图像、检测与采集请求传递的不可变帧身份和时钟信息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageFrameIdentity {
    pub provenance: FrameProvenance,
    pub frame_sequence: u64,
    pub source_pts: SourcePts,
    pub host_monotonic_time_ns: u64,
    pub device_timestamp_ns: Option<u64>,
}

impl From<&StreamFrameIdentity> for ImageFrameIdentity {
    fn from(identity: &StreamFrameIdentity) -> Self {
        Self {
            provenance: FrameProvenance::Stream {
                stream_id: identity.stream_id.clone(),
                channel: identity.channel,
            },
            frame_sequence: identity.frame_sequence,
            source_pts: identity.source_pts.clone(),
            host_monotonic_time_ns: identity.host_monotonic_time_ns,
            device_timestamp_ns: identity.device_timestamp_ns,
        }
    }
}

impl ImageFrameIdentity {
    /// 在来源确为流会话时恢复原始 stream 身份；其他来源不能伪装成 RTSP 帧。
    #[must_use]
    pub fn stream_identity(&self) -> Option<StreamFrameIdentity> {
        let FrameProvenance::Stream { stream_id, channel } = &self.provenance else {
            return None;
        };
        Some(StreamFrameIdentity {
            stream_id: stream_id.clone(),
            channel: *channel,
            frame_sequence: self.frame_sequence,
            source_pts: self.source_pts.clone(),
            host_monotonic_time_ns: self.host_monotonic_time_ns,
            device_timestamp_ns: self.device_timestamp_ns,
        })
    }

    /// 返回可用于 X5 TCP exact snapshot 的设备时间戳；禁止退回本机单调时钟。
    #[must_use]
    pub fn device_timestamp_ns(&self) -> Option<u64> {
        self.device_timestamp_ns.or_else(|| match &self.provenance {
            FrameProvenance::Device { timestamp_ns, .. } => Some(*timestamp_ns),
            _ => None,
        })
    }
}

/// 捕获目标必须显式区分 ISP/VSE NV12 与 VIN Bayer RAW，禁止根据端口名称猜测格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureTarget {
    Yuv { channel: u16 },
    Raw { camera: u16 },
}

/// X5_233 ring 的强匹配条件；`timestamp_ns` 必须来自设备侧 metadata。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    Latest,
    FrameId(u64),
    TimestampNs(u64),
}

/// `command.capture.request.v1` 的进程内载荷；可选来源身份随检测/评分链路原样携带。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureRequest {
    pub target: CaptureTarget,
    pub mode: CaptureMode,
    pub source_identity: Option<ImageFrameIdentity>,
}

/// 检测结果连同产生它的原始图像身份；后续评分和采集请求必须只转发该身份，不得重建。
#[derive(Clone, Debug, PartialEq)]
pub struct DetectionPacket {
    pub detection: Arc<ChessboardDetection>,
    pub frame_identity: ImageFrameIdentity,
}

/// 归一化标定帧质量评分；`score` 由 `CalibrationFrameScorer` 保证为有限且落在 `[0, 1]`。
///
/// 来源帧身份必须沿检测、阈值、连续保持和采集请求链路原样传递，不能由下游重建。
#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationFrameScore {
    pub score: f64,
    pub frame_identity: ImageFrameIdentity,
}

/// 阈值门的逐帧判定；拒绝也要向下游传递，才能让连续保持门正确重置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureSignal {
    pub accepted: bool,
    pub frame_identity: ImageFrameIdentity,
}

/// 连续保持条件满足后产生的无歧义抓帧触发。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureTrigger {
    pub frame_identity: ImageFrameIdentity,
}

/// 带显式格式、步长、元数据与来源身份的单帧图像载荷。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageFrame {
    pub width: u32,
    pub height: u32,
    pub format: ImageFrameFormat,
    pub planes: Vec<ImagePlane>,
    pub identity: ImageFrameIdentity,
    pub color: Option<ColorMetadata>,
    pub raw: Option<RawMetadata>,
}

impl ImageFrame {
    /// 构造并校验平面数、步长与最小字节长度，防止下游按错误布局读取图像。
    pub fn new(
        width: u32,
        height: u32,
        format: ImageFrameFormat,
        planes: Vec<ImagePlane>,
        identity: ImageFrameIdentity,
        color: Option<ColorMetadata>,
        raw: Option<RawMetadata>,
    ) -> Result<Self, ImageFrameError> {
        validate_layout(width, height, format, &planes, raw.as_ref())?;
        Ok(Self {
            width,
            height,
            format,
            planes,
            identity,
            color,
            raw,
        })
    }

    /// 从紧密排列 RGBA8 数据构造图像帧。
    pub fn rgba8(
        width: u32,
        height: u32,
        pixels: Arc<[u8]>,
        identity: ImageFrameIdentity,
    ) -> Result<Self, ImageFrameError> {
        let stride_bytes = width
            .checked_mul(4)
            .ok_or(ImageFrameError::DimensionOverflow)?;
        Self::new(
            width,
            height,
            ImageFrameFormat::Rgba8,
            vec![ImagePlane::new(pixels, stride_bytes)],
            identity,
            Some(ColorMetadata {
                color_space: ColorSpace::Srgb,
                full_range: Some(true),
            }),
            None,
        )
    }

    /// RGBA8 数据的唯一平面；其他格式或 malformed carrier 不可直接读取。
    #[must_use]
    pub fn rgba8_plane(&self) -> Option<&ImagePlane> {
        (self.format == ImageFrameFormat::Rgba8)
            .then(|| self.planes.first())
            .flatten()
    }

    /// 当且仅当图像是紧密 RGBA8 且来自流会话时，以零像素拷贝恢复旧 Viewer 帧。
    #[must_use]
    pub fn decoded_rgba_frame(&self) -> Option<DecodedVideoFrame> {
        let plane = self.rgba8_plane()?;
        let stride = self.width.checked_mul(4)?;
        let identity = self.identity.stream_identity()?;
        (plane.stride_bytes == stride).then(|| DecodedVideoFrame {
            width: self.width,
            height: self.height,
            rgba: Arc::clone(&plane.bytes),
            identity,
        })
    }
}

impl From<&DecodedVideoFrame> for ImageFrame {
    fn from(frame: &DecodedVideoFrame) -> Self {
        Self::rgba8(
            frame.width,
            frame.height,
            Arc::clone(&frame.rgba),
            ImageFrameIdentity::from(&frame.identity),
        )
        .expect("DecodedVideoFrame always has a representable RGBA8 layout")
    }
}

/// 图像布局不满足其格式约束时的构造错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImageFrameError {
    #[error("image dimensions must be non-zero")]
    EmptyDimensions,
    #[error("image dimensions overflow byte layout")]
    DimensionOverflow,
    #[error("{format} requires {expected} planes, got {actual}")]
    PlaneCount {
        format: ImageFrameFormat,
        expected: usize,
        actual: usize,
    },
    #[error("plane {plane} stride {actual} is smaller than required {required}")]
    StrideTooSmall {
        plane: usize,
        actual: u32,
        required: u32,
    },
    #[error("plane {plane} contains {actual} bytes but requires at least {required}")]
    PlaneTooShort {
        plane: usize,
        actual: usize,
        required: usize,
    },
    #[error("NV12 requires even width and height")]
    InvalidNv12Dimensions,
    #[error("BayerRaw requires RAW metadata with a 1..=16 bit sample depth")]
    InvalidRawMetadata,
    #[error("RAW metadata is only valid for BayerRaw images")]
    RawMetadataForNonRaw,
}

fn validate_layout(
    width: u32,
    height: u32,
    format: ImageFrameFormat,
    planes: &[ImagePlane],
    raw: Option<&RawMetadata>,
) -> Result<(), ImageFrameError> {
    if width == 0 || height == 0 {
        return Err(ImageFrameError::EmptyDimensions);
    }
    if format == ImageFrameFormat::Nv12 && (width % 2 != 0 || height % 2 != 0) {
        return Err(ImageFrameError::InvalidNv12Dimensions);
    }
    match (format, raw) {
        (ImageFrameFormat::BayerRaw, Some(raw)) if (1..=16).contains(&raw.bits_per_sample) => {}
        (ImageFrameFormat::BayerRaw, _) => return Err(ImageFrameError::InvalidRawMetadata),
        (_, Some(_)) => return Err(ImageFrameError::RawMetadataForNonRaw),
        (_, None) => {}
    }

    let layouts: &[(u32, u32)] = match format {
        ImageFrameFormat::Rgba8 => &[(
            width
                .checked_mul(4)
                .ok_or(ImageFrameError::DimensionOverflow)?,
            height,
        )],
        ImageFrameFormat::Gray8 => &[(width, height)],
        ImageFrameFormat::Gray16Le => &[(
            width
                .checked_mul(2)
                .ok_or(ImageFrameError::DimensionOverflow)?,
            height,
        )],
        ImageFrameFormat::Nv12 => &[(width, height), (width, height / 2)],
        ImageFrameFormat::BayerRaw => &[(
            width
                .checked_mul(2)
                .ok_or(ImageFrameError::DimensionOverflow)?,
            height,
        )],
    };
    if planes.len() != layouts.len() {
        return Err(ImageFrameError::PlaneCount {
            format,
            expected: layouts.len(),
            actual: planes.len(),
        });
    }
    for (index, (plane, &(minimum_stride, rows))) in planes.iter().zip(layouts).enumerate() {
        if plane.stride_bytes < minimum_stride {
            return Err(ImageFrameError::StrideTooSmall {
                plane: index,
                actual: plane.stride_bytes,
                required: minimum_stride,
            });
        }
        let required = usize::try_from(u64::from(plane.stride_bytes) * u64::from(rows))
            .map_err(|_| ImageFrameError::DimensionOverflow)?;
        if plane.bytes.len() < required {
            return Err(ImageFrameError::PlaneTooShort {
                plane: index,
                actual: plane.bytes.len(),
                required,
            });
        }
    }
    Ok(())
}

/// 节点间流动的统一数据包。
///
/// 变体与端口 `kind` 对应；骨架阶段先落地帧与解，其余负载类型随对应节点补齐。
#[derive(Clone)]
pub enum DataPacket {
    /// `stream.video-frame`：视频帧流。
    VideoFrame(Arc<DecodedVideoFrame>),
    /// `image.frame`：单张图片帧。
    ImageFrame(Arc<ImageFrame>),
    /// `calib.detection`：棋盘格检测结果及其来源帧身份。
    Detection(Arc<DetectionPacket>),
    /// `calib.solution`：标定求解结果。
    Solution(Arc<CalibrationSolution>),
    /// `calib.coverage`：标定数据覆盖度（弱类型，`Arc<Value>` 承载）。
    Coverage(Arc<serde_json::Value>),
    /// `calib.dataset`：标定数据集（弱类型）。
    Dataset(Arc<serde_json::Value>),
    /// `calib.report`：标定报告（弱类型）。
    Report(Arc<serde_json::Value>),
    /// `capture.score`：带来源帧身份的归一化标定帧质量评分。
    Score(Arc<CalibrationFrameScore>),
    /// `capture.signal`：阈值门对每个评分帧的接受/拒绝判定。
    CaptureSignal(Arc<CaptureSignal>),
    /// `capture.trigger`：连续保持门完成后的抓帧触发。
    CaptureTrigger(Arc<CaptureTrigger>),
    /// `capture.target`：采集目标位姿（弱类型）。
    Target(Arc<serde_json::Value>),
    /// `command.capture.request.v1`：以明确目标和匹配条件驱动设备抓帧。
    CaptureRequest(Arc<CaptureRequest>),
    /// 通用 JSON 负载：控制结果、指标、命令等未强类型化的数据。
    Json(Arc<serde_json::Value>),
}

impl DataPacket {
    /// 负载的端口类型标识，用于引擎在接线时做一致性校验。
    #[must_use]
    pub fn port_kind(&self) -> &'static str {
        match self {
            Self::VideoFrame(_) => "stream.video-frame",
            Self::ImageFrame(_) => "image.frame",
            Self::Detection(_) => "calib.detection",
            Self::Solution(_) => "calib.solution",
            Self::Coverage(_) => "calib.coverage",
            Self::Dataset(_) => "calib.dataset",
            Self::Report(_) => "calib.report",
            Self::Score(_) => "capture.score",
            Self::CaptureSignal(_) => "capture.signal",
            Self::CaptureTrigger(_) => "capture.trigger",
            Self::Target(_) => "capture.target",
            Self::CaptureRequest(_) => "command.capture.request.v1",
            Self::Json(_) => "status.metrics",
        }
    }

    /// 返回可用于流动动画去重/标注的来源帧序号；非帧派生负载返回 None。
    #[must_use]
    pub fn flow_sequence(&self) -> Option<u64> {
        match self {
            Self::VideoFrame(frame) => Some(frame.identity.frame_sequence),
            Self::ImageFrame(frame) => Some(frame.identity.frame_sequence),
            Self::Detection(detection) => Some(detection.frame_identity.frame_sequence),
            Self::Score(score) => Some(score.frame_identity.frame_sequence),
            Self::CaptureSignal(signal) => Some(signal.frame_identity.frame_sequence),
            Self::CaptureTrigger(trigger) => Some(trigger.frame_identity.frame_sequence),
            Self::CaptureRequest(request) => request
                .source_identity
                .as_ref()
                .map(|identity| identity.frame_sequence),
            Self::Solution(_)
            | Self::Coverage(_)
            | Self::Dataset(_)
            | Self::Report(_)
            | Self::Target(_)
            | Self::Json(_) => None,
        }
    }
}

impl std::fmt::Debug for DataPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VideoFrame(frame) => f
                .debug_struct("VideoFrame")
                .field("width", &frame.width)
                .field("height", &frame.height)
                .field("sequence", &frame.identity.frame_sequence)
                .finish(),
            Self::ImageFrame(frame) => f
                .debug_struct("ImageFrame")
                .field("width", &frame.width)
                .field("height", &frame.height)
                .field("format", &frame.format)
                .field("sequence", &frame.identity.frame_sequence)
                .finish(),
            Self::Detection(_) => f.write_str("Detection(..)"),
            Self::Solution(solution) => f
                .debug_struct("Solution")
                .field("views", &solution.views.len())
                .field("rms", &solution.rms_error)
                .finish(),
            Self::Coverage(_) => f.write_str("Coverage(..)"),
            Self::Dataset(_) => f.write_str("Dataset(..)"),
            Self::Report(_) => f.write_str("Report(..)"),
            Self::Score(_) => f.write_str("Score(..)"),
            Self::CaptureSignal(_) => f.write_str("CaptureSignal(..)"),
            Self::CaptureTrigger(_) => f.write_str("CaptureTrigger(..)"),
            Self::Target(_) => f.write_str("Target(..)"),
            Self::CaptureRequest(_) => f.write_str("CaptureRequest(..)"),
            Self::Json(_) => f.write_str("Json(..)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{SourcePtsProvenance, StreamSessionId};

    fn stream_identity() -> StreamFrameIdentity {
        StreamFrameIdentity::known_at_with_device_timestamp(
            StreamSessionId::new("rtsp-camera-0").expect("valid stream id"),
            3,
            42,
            SourcePts::Known {
                ticks: 9_000,
                time_base_numerator: 1,
                time_base_denominator: 90_000,
                provenance: SourcePtsProvenance::FfmpegDecodedFrame,
            },
            123_456,
            Some(987_654),
        )
    }

    #[test]
    fn decoded_video_frame_conversion_is_rgba_zero_copy_and_preserves_identity() {
        let decoded = DecodedVideoFrame {
            width: 2,
            height: 1,
            rgba: Arc::from(vec![1; 8]),
            identity: stream_identity(),
        };
        let image = ImageFrame::from(&decoded);

        assert_eq!(image.format, ImageFrameFormat::Rgba8);
        assert!(Arc::ptr_eq(&decoded.rgba, &image.planes[0].bytes));
        assert_eq!(image.identity.frame_sequence, 42);
        assert_eq!(image.identity.source_pts, decoded.identity.source_pts);
        assert_eq!(image.identity.host_monotonic_time_ns, 123_456);
        assert_eq!(image.identity.device_timestamp_ns(), Some(987_654));
        assert_eq!(
            image.identity.provenance,
            FrameProvenance::Stream {
                stream_id: decoded.identity.stream_id.clone(),
                channel: 3,
            }
        );
    }

    #[test]
    fn typed_capture_chain_retains_the_same_frame_identity() {
        let identity = ImageFrameIdentity::from(&stream_identity());
        let detection = DetectionPacket {
            detection: Arc::new(ChessboardDetection {
                image_size: CalibrationImageSize::new(2, 2).expect("non-empty image"),
                corners: Vec::new(),
            }),
            frame_identity: identity.clone(),
        };
        let score = CalibrationFrameScore {
            score: 0.5,
            frame_identity: detection.frame_identity.clone(),
        };
        let signal = CaptureSignal {
            accepted: true,
            frame_identity: score.frame_identity.clone(),
        };
        let trigger = CaptureTrigger {
            frame_identity: signal.frame_identity.clone(),
        };
        let request = CaptureRequest {
            target: CaptureTarget::Yuv { channel: 3 },
            mode: CaptureMode::FrameId(trigger.frame_identity.frame_sequence),
            source_identity: Some(trigger.frame_identity.clone()),
        };

        assert_eq!(score.frame_identity, identity);
        assert_eq!(signal.frame_identity, identity);
        assert_eq!(trigger.frame_identity, identity);
        assert_eq!(request.source_identity.as_ref(), Some(&identity));
        assert_eq!(
            DataPacket::Detection(Arc::new(detection)).port_kind(),
            "calib.detection"
        );
        assert_eq!(
            DataPacket::Score(Arc::new(score)).port_kind(),
            "capture.score"
        );
        assert_eq!(
            DataPacket::CaptureSignal(Arc::new(signal)).port_kind(),
            "capture.signal"
        );
        assert_eq!(
            DataPacket::CaptureTrigger(Arc::new(trigger)).port_kind(),
            "capture.trigger"
        );
        assert_eq!(
            DataPacket::CaptureRequest(Arc::new(request)).flow_sequence(),
            Some(42)
        );
    }

    #[test]
    fn nv12_requires_two_planes_and_even_dimensions() {
        let identity = ImageFrameIdentity::from(&stream_identity());
        let error = ImageFrame::new(
            3,
            2,
            ImageFrameFormat::Nv12,
            vec![ImagePlane::new(Arc::from(vec![0; 6]), 3)],
            identity,
            None,
            None,
        )
        .expect_err("odd NV12 width must be rejected");
        assert_eq!(error, ImageFrameError::InvalidNv12Dimensions);
    }

    #[test]
    fn new_variants_map_to_declared_port_kinds() {
        let coverage = DataPacket::Coverage(Arc::new(serde_json::json!({})));
        let dataset = DataPacket::Dataset(Arc::new(serde_json::json!({})));
        let report = DataPacket::Report(Arc::new(serde_json::json!({})));
        let identity = ImageFrameIdentity::from(&stream_identity());
        let score = DataPacket::Score(Arc::new(CalibrationFrameScore {
            score: 0.5,
            frame_identity: identity.clone(),
        }));
        let signal = DataPacket::CaptureSignal(Arc::new(CaptureSignal {
            accepted: true,
            frame_identity: identity.clone(),
        }));
        let trigger = DataPacket::CaptureTrigger(Arc::new(CaptureTrigger {
            frame_identity: identity,
        }));
        let target = DataPacket::Target(Arc::new(serde_json::json!({})));
        assert_eq!(coverage.port_kind(), "calib.coverage");
        assert_eq!(dataset.port_kind(), "calib.dataset");
        assert_eq!(report.port_kind(), "calib.report");
        assert_eq!(score.port_kind(), "capture.score");
        assert_eq!(signal.port_kind(), "capture.signal");
        assert_eq!(trigger.port_kind(), "capture.trigger");
        assert_eq!(target.port_kind(), "capture.target");
    }

    #[test]
    fn existing_variants_keep_their_port_kinds() {
        let json = DataPacket::Json(Arc::new(serde_json::json!({})));
        assert_eq!(json.port_kind(), "status.metrics");
    }
}
