//! 节点间统一数据负载。
//!
//! 大负载（帧、检测结果、解）一律用 `Arc` 零拷贝传递；`Clone` 仅复制句柄。

use std::sync::Arc;

use camera_toolbox_core::{
    CalibrationImageSize, ChessboardDetection, Datum, PacketProvenance, StructuredPacket,
};
use serde::{Deserialize, Serialize};

use crate::platform::{
    DecodedVideoFrame, I2cExecutionReport, I2cReadReport, SourcePts, SshConnection,
    StreamFrameIdentity, StreamSessionId,
};

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

/// 标定板参数包的版本化 JSON 类型标识。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CalibrationBoardParamsKind {
    #[serde(rename = "calib.board.params.v1")]
    V1,
}

/// 当前受支持的标定板类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CalibrationBoardKind {
    #[serde(rename = "chessboard")]
    Chessboard,
}

/// 相机模型参数包的版本化 JSON 类型标识。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CameraModelParamsKind {
    #[serde(rename = "calib.camera.model.v1")]
    V1,
}

/// 当前受支持的相机投影模型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CameraModelKind {
    #[serde(rename = "pinhole")]
    Pinhole,
}

/// 畸变模型参数包的版本化 JSON 类型标识。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DistortionModelParamsKind {
    #[serde(rename = "calib.distortion.model.v1")]
    V1,
}

/// 当前受支持的镜头畸变模型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DistortionModelKind {
    #[serde(rename = "none")]
    None,
}

/// 检测姿态包的版本化 JSON 类型标识。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectionPoseKind {
    #[serde(rename = "calib.pose.v1")]
    V1,
}

/// 检测姿态的坐标变换约定。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectionPoseConvention {
    /// 将标定板坐标系中的点变换到相机坐标系。
    #[serde(rename = "T_camera_board")]
    TCameraBoard,
}

/// 三维向量；姿态平移使用米，Rodrigues 旋转向量使用弧度。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationVector3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl CalibrationVector3 {
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    #[must_use]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

/// 参数包不满足下游检测、PnP 或求解前置条件时的错误。
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum CalibrationParameterError {
    #[error("chessboard inner-corner dimensions must be within 2..=64, got {cols}x{rows}")]
    InvalidBoardDimensions { cols: u16, rows: u16 },
    #[error("{field} must be finite and positive, got {value}")]
    NonFiniteOrNonPositive { field: &'static str, value: f64 },
    #[error("{field} must be finite, got {value}")]
    NonFinite { field: &'static str, value: f64 },
    #[error("camera image size must be nonzero, got {width}x{height}")]
    InvalidImageSize { width: u32, height: u32 },
    #[error("the none distortion model requires zero coefficients, got {count}")]
    NoneDistortionCoefficients { count: usize },
    #[error("reprojection error must be finite and non-negative, got {value}")]
    InvalidReprojectionError { value: f64 },
}

/// 可复用的棋盘格标定板参数；`cols`/`rows` 表示内角点数。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationBoardParams {
    pub kind: CalibrationBoardParamsKind,
    pub board_kind: CalibrationBoardKind,
    pub cols: u16,
    pub rows: u16,
    pub square_size_mm: f64,
}

impl Default for CalibrationBoardParams {
    fn default() -> Self {
        Self {
            kind: CalibrationBoardParamsKind::V1,
            board_kind: CalibrationBoardKind::Chessboard,
            cols: 11,
            rows: 8,
            square_size_mm: 40.0,
        }
    }
}

impl CalibrationBoardParams {
    /// 构造当前唯一受支持的棋盘格参数包。
    ///
    /// # Errors
    ///
    /// 内角点尺寸不在 `2..=64` 或方格间距不是有限正数时返回错误。
    pub fn new(
        cols: u16,
        rows: u16,
        square_size_mm: f64,
    ) -> Result<Self, CalibrationParameterError> {
        let params = Self {
            cols,
            rows,
            square_size_mm,
            ..Self::default()
        };
        params.validate()?;
        Ok(params)
    }

    /// 返回 Pose/Solver 生成 object points 时必须使用的米单位间距。
    #[must_use]
    pub fn square_size_meters(&self) -> f64 {
        self.square_size_mm / 1_000.0
    }

    /// 验证当前棋盘格参数可用于检测和几何求解。
    ///
    /// # Errors
    ///
    /// 内角点尺寸不在 `2..=64` 或方格间距不是有限正数时返回错误。
    pub fn validate(&self) -> Result<(), CalibrationParameterError> {
        if !(2..=64).contains(&self.cols) || !(2..=64).contains(&self.rows) {
            return Err(CalibrationParameterError::InvalidBoardDimensions {
                cols: self.cols,
                rows: self.rows,
            });
        }
        validate_finite_positive("squareSizeMm", self.square_size_mm)
    }
}

/// 可复用的针孔相机初始内参；焦距和主点的单位均为像素。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraModelParams {
    pub kind: CameraModelParamsKind,
    pub model: CameraModelKind,
    pub fx: f64,
    pub fy: f64,
    pub cx: f64,
    pub cy: f64,
    pub image_size: Option<CalibrationImageSize>,
}

impl Default for CameraModelParams {
    fn default() -> Self {
        Self {
            kind: CameraModelParamsKind::V1,
            model: CameraModelKind::Pinhole,
            fx: 900.0,
            fy: 900.0,
            cx: 960.0,
            cy: 540.0,
            image_size: Some(CalibrationImageSize {
                width: 1_920,
                height: 1_080,
            }),
        }
    }
}

impl CameraModelParams {
    /// 构造当前唯一受支持的针孔相机内参包。
    ///
    /// # Errors
    ///
    /// 焦距不是有限正数、主点不是有限数，或可选图像尺寸为零时返回错误。
    pub fn new(
        fx: f64,
        fy: f64,
        cx: f64,
        cy: f64,
        image_size: Option<CalibrationImageSize>,
    ) -> Result<Self, CalibrationParameterError> {
        let params = Self {
            fx,
            fy,
            cx,
            cy,
            image_size,
            ..Self::default()
        };
        params.validate()?;
        Ok(params)
    }

    /// 验证针孔模型参数可安全转换为投影矩阵。
    ///
    /// # Errors
    ///
    /// 焦距不是有限正数、主点不是有限数，或可选图像尺寸为零时返回错误。
    pub fn validate(&self) -> Result<(), CalibrationParameterError> {
        validate_finite_positive("fx", self.fx)?;
        validate_finite_positive("fy", self.fy)?;
        validate_finite("cx", self.cx)?;
        validate_finite("cy", self.cy)?;
        if let Some(image_size) = self.image_size {
            if image_size.width == 0 || image_size.height == 0 {
                return Err(CalibrationParameterError::InvalidImageSize {
                    width: image_size.width,
                    height: image_size.height,
                });
            }
        }
        Ok(())
    }
}

/// 可复用的镜头畸变参数；当前只有无畸变模型。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DistortionModelParams {
    pub kind: DistortionModelParamsKind,
    pub model: DistortionModelKind,
    pub coefficients: Vec<f64>,
}

impl Default for DistortionModelParams {
    fn default() -> Self {
        Self {
            kind: DistortionModelParamsKind::V1,
            model: DistortionModelKind::None,
            coefficients: Vec::new(),
        }
    }
}

impl DistortionModelParams {
    /// 验证当前无畸变模型没有被携带 OpenCV 系数的调用方错误复用。
    ///
    /// # Errors
    ///
    /// `none` 模型携带任意系数时返回错误。
    pub fn validate(&self) -> Result<(), CalibrationParameterError> {
        if !self.coefficients.is_empty() {
            return Err(CalibrationParameterError::NoneDistortionCoefficients {
                count: self.coefficients.len(),
            });
        }
        Ok(())
    }
}

/// 单帧 PnP 输出的标定板姿态；始终保留产生检测的原始帧身份。
#[derive(Clone, Debug, PartialEq)]
pub struct DetectionPose {
    pub kind: DetectionPoseKind,
    pub frame_identity: ImageFrameIdentity,
    pub convention: DetectionPoseConvention,
    pub translation_m: CalibrationVector3,
    pub rotation_rodrigues: CalibrationVector3,
    pub reprojection_error_px: Option<f64>,
}

impl DetectionPose {
    /// 以固定的 `T_camera_board` 约定构造有效的 PnP 姿态。
    ///
    /// # Errors
    ///
    /// 平移/旋转存在非有限分量，或重投影误差为负数、NaN、无穷时返回错误。
    pub fn new(
        frame_identity: ImageFrameIdentity,
        translation_m: CalibrationVector3,
        rotation_rodrigues: CalibrationVector3,
        reprojection_error_px: Option<f64>,
    ) -> Result<Self, CalibrationParameterError> {
        let pose = Self {
            kind: DetectionPoseKind::V1,
            frame_identity,
            convention: DetectionPoseConvention::TCameraBoard,
            translation_m,
            rotation_rodrigues,
            reprojection_error_px,
        };
        pose.validate()?;
        Ok(pose)
    }

    /// 验证姿态数值可安全发送到运行时输出或下游几何节点。
    ///
    /// # Errors
    ///
    /// 平移/旋转存在非有限分量，或重投影误差为负数、NaN、无穷时返回错误。
    pub fn validate(&self) -> Result<(), CalibrationParameterError> {
        for (field, value) in [
            ("translationM.x", self.translation_m.x),
            ("translationM.y", self.translation_m.y),
            ("translationM.z", self.translation_m.z),
            ("rotationRodrigues.x", self.rotation_rodrigues.x),
            ("rotationRodrigues.y", self.rotation_rodrigues.y),
            ("rotationRodrigues.z", self.rotation_rodrigues.z),
        ] {
            validate_finite(field, value)?;
        }
        if let Some(error) = self.reprojection_error_px {
            if !error.is_finite() || error < 0.0 {
                return Err(CalibrationParameterError::InvalidReprojectionError { value: error });
            }
        }
        Ok(())
    }
}

fn validate_finite_positive(
    field: &'static str,
    value: f64,
) -> Result<(), CalibrationParameterError> {
    if !value.is_finite() || value <= 0.0 {
        return Err(CalibrationParameterError::NonFiniteOrNonPositive { field, value });
    }
    Ok(())
}

fn validate_finite(field: &'static str, value: f64) -> Result<(), CalibrationParameterError> {
    if !value.is_finite() {
        return Err(CalibrationParameterError::NonFinite { field, value });
    }
    Ok(())
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

/// 从结构化包保留到 typed-field 的不可变来源元数据。
///
/// 每个字段独立携带其 schema、完整 provenance 和相机模型标识；下游不得把不同字段
/// 的来源摘要当作一致性前提，但可以逐字段执行 map 的来源合同。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedFieldSource {
    pub schema: String,
    pub provenance: PacketProvenance,
    /// 无相机模型的通用 structured packet 保持为 None；要求模型的消费者必须拒绝它。
    pub model_id: Option<String>,
}

impl TypedFieldSource {
    #[must_use]
    pub fn new(
        schema: impl Into<String>,
        provenance: PacketProvenance,
        model_id: Option<String>,
    ) -> Self {
        Self {
            schema: schema.into(),
            provenance,
            model_id,
        }
    }
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
    /// `calib.board.params`：棋盘格标定板参数。
    CalibrationBoardParams(Arc<CalibrationBoardParams>),
    /// `calib.camera.model`：针孔相机初始内参。
    CameraModelParams(Arc<CameraModelParams>),
    /// `calib.distortion.model`：镜头畸变模型参数。
    DistortionModelParams(Arc<DistortionModelParams>),
    /// `calib.pose`：检测帧中标定板的 `T_camera_board` 姿态。
    DetectionPose(Arc<DetectionPose>),
    /// `data.packet.v1`：可交换、可审计的结构化业务数据包。
    StructuredPacket(Arc<StructuredPacket>),
    /// `data.packet.v1`：通用 JSON packet；I²C task 等严格子合同在消费者处解析。
    PacketData(Arc<serde_json::Value>),
    /// `data.field.v1`：从一个结构化包提取的完整 datum 及其来源。
    /// generation 在 extractor 的一次输入内相同，供多端口 fan-in 原子组装。
    TypedField {
        datum: Arc<Datum>,
        generation: u64,
        source: Arc<TypedFieldSource>,
    },
    /// `ssh.connection.v1`：进程内 SSH 会话句柄；不含 credential material，不能持久化。
    SshConnection(Arc<SshConnection>),
    /// `i2c.read-report.v1`：一次原子 read 的设备状态报告。
    I2cReadReport(Arc<I2cReadReport>),
    /// `i2c.execution-report.v1`：目标端 guarded write 的逐页 readback 报告。
    I2cExecutionReport(Arc<I2cExecutionReport>),
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
            Self::CalibrationBoardParams(_) => "calib.board.params",
            Self::CameraModelParams(_) => "calib.camera.model",
            Self::DistortionModelParams(_) => "calib.distortion.model",
            Self::DetectionPose(_) => "calib.pose",
            Self::StructuredPacket(_) | Self::PacketData(_) => "data.packet.v1",
            Self::TypedField { .. } => "data.field.v1",
            Self::SshConnection(_) => "ssh.connection.v1",
            Self::I2cReadReport(_) => "i2c.read-report.v1",
            Self::I2cExecutionReport(_) => "i2c.execution-report.v1",
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

    /// 高频帧派生流走 mailbox 的受限 lane；计划、配置和 typed-field 必须可靠投递。
    #[must_use]
    pub const fn is_realtime_stream(&self) -> bool {
        matches!(
            self,
            Self::VideoFrame(_)
                | Self::ImageFrame(_)
                | Self::Detection(_)
                | Self::DetectionPose(_)
                | Self::Score(_)
                | Self::CaptureSignal(_)
                | Self::CaptureTrigger(_)
        )
    }

    /// 返回可用于流动动画去重/标注的来源帧序号；非帧派生负载返回 None。
    #[must_use]
    pub fn flow_sequence(&self) -> Option<u64> {
        match self {
            Self::VideoFrame(frame) => Some(frame.identity.frame_sequence),
            Self::ImageFrame(frame) => Some(frame.identity.frame_sequence),
            Self::Detection(detection) => Some(detection.frame_identity.frame_sequence),
            Self::DetectionPose(pose) => Some(pose.frame_identity.frame_sequence),
            Self::Score(score) => Some(score.frame_identity.frame_sequence),
            Self::CaptureSignal(signal) => Some(signal.frame_identity.frame_sequence),
            Self::CaptureTrigger(trigger) => Some(trigger.frame_identity.frame_sequence),
            Self::CaptureRequest(request) => request
                .source_identity
                .as_ref()
                .map(|identity| identity.frame_sequence),
            Self::CalibrationBoardParams(_)
            | Self::CameraModelParams(_)
            | Self::DistortionModelParams(_)
            | Self::StructuredPacket(_)
            | Self::PacketData(_)
            | Self::TypedField { .. }
            | Self::SshConnection(_)
            | Self::I2cReadReport(_)
            | Self::I2cExecutionReport(_)
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
            Self::CalibrationBoardParams(_) => f.write_str("CalibrationBoardParams(..)"),
            Self::CameraModelParams(_) => f.write_str("CameraModelParams(..)"),
            Self::DistortionModelParams(_) => f.write_str("DistortionModelParams(..)"),
            Self::DetectionPose(pose) => f
                .debug_struct("DetectionPose")
                .field("sequence", &pose.frame_identity.frame_sequence)
                .finish(),
            Self::StructuredPacket(packet) => f
                .debug_struct("StructuredPacket")
                .field("schema", &packet.schema)
                .field("fields", &packet.fields.len())
                .finish(),
            Self::PacketData(packet) => f
                .debug_struct("PacketData")
                .field("schema", &packet.get("schema"))
                .finish(),
            Self::TypedField {
                datum: field,
                generation,
                source,
            } => f
                .debug_struct("TypedField")
                .field("name", &field.name)
                .field("type", &field.primitive_type())
                .field("generation", generation)
                .field("schema", &source.schema)
                .field("model_id", &source.model_id)
                .finish(),
            Self::SshConnection(connection) => f
                .debug_struct("SshConnection")
                .field("id", &connection.id())
                .finish(),
            Self::I2cReadReport(report) => f
                .debug_struct("I2cReadReport")
                .field("map_id", &report.map_id)
                .field("valid", &report.valid)
                .finish(),
            Self::I2cExecutionReport(report) => f
                .debug_struct("I2cExecutionReport")
                .field("final_verified", &report.final_verified)
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
    use camera_toolbox_core::TypedValue;

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
    fn calibration_parameter_defaults_round_trip_as_v1_contracts() {
        let board = CalibrationBoardParams::default();
        let board_json = serde_json::to_value(&board).expect("board params serialize");
        assert_eq!(
            board_json,
            serde_json::json!({
                "kind": "calib.board.params.v1",
                "boardKind": "chessboard",
                "cols": 11,
                "rows": 8,
                "squareSizeMm": 40.0,
            })
        );
        assert_eq!(
            serde_json::from_value::<CalibrationBoardParams>(board_json)
                .expect("board params deserialize"),
            board
        );
        assert!((board.square_size_meters() - 0.04).abs() < f64::EPSILON);

        let camera = CameraModelParams::default();
        let camera_json = serde_json::to_value(&camera).expect("camera params serialize");
        assert_eq!(
            camera_json,
            serde_json::json!({
                "kind": "calib.camera.model.v1",
                "model": "pinhole",
                "fx": 900.0,
                "fy": 900.0,
                "cx": 960.0,
                "cy": 540.0,
                "imageSize": {"width": 1920, "height": 1080},
            })
        );
        assert_eq!(
            serde_json::from_value::<CameraModelParams>(camera_json)
                .expect("camera params deserialize"),
            camera
        );

        let distortion = DistortionModelParams::default();
        let distortion_json =
            serde_json::to_value(&distortion).expect("distortion params serialize");
        assert_eq!(
            distortion_json,
            serde_json::json!({
                "kind": "calib.distortion.model.v1",
                "model": "none",
                "coefficients": [],
            })
        );
        assert_eq!(
            serde_json::from_value::<DistortionModelParams>(distortion_json)
                .expect("distortion params deserialize"),
            distortion
        );
    }

    #[test]
    fn calibration_parameter_validation_and_packet_kinds_are_explicit() {
        assert!(CalibrationBoardParams::new(1, 8, 40.0).is_err());
        assert!(CameraModelParams::new(0.0, 900.0, 960.0, 540.0, None).is_err());
        assert!(
            DistortionModelParams {
                coefficients: vec![0.0],
                ..DistortionModelParams::default()
            }
            .validate()
            .is_err()
        );

        let identity = ImageFrameIdentity::from(&stream_identity());
        let pose = DetectionPose::new(
            identity.clone(),
            CalibrationVector3::new(0.0, 0.0, 1.0),
            CalibrationVector3::new(0.0, 0.0, 0.0),
            None,
        )
        .expect("finite pose");
        assert_eq!(pose.frame_identity, identity);
        assert_eq!(
            DataPacket::CalibrationBoardParams(Arc::new(CalibrationBoardParams::default()))
                .port_kind(),
            "calib.board.params"
        );
        assert_eq!(
            DataPacket::CameraModelParams(Arc::new(CameraModelParams::default())).port_kind(),
            "calib.camera.model"
        );
        assert_eq!(
            DataPacket::DistortionModelParams(Arc::new(DistortionModelParams::default()))
                .port_kind(),
            "calib.distortion.model"
        );
        let pose_packet = DataPacket::DetectionPose(Arc::new(pose));
        assert_eq!(pose_packet.port_kind(), "calib.pose");
        assert_eq!(pose_packet.flow_sequence(), Some(42));
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
        let structured = DataPacket::StructuredPacket(Arc::new(
            StructuredPacket::new("example.packet.v1", Default::default(), vec![])
                .expect("valid structured packet"),
        ));
        assert_eq!(coverage.port_kind(), "calib.coverage");
        assert_eq!(dataset.port_kind(), "calib.dataset");
        assert_eq!(report.port_kind(), "calib.report");
        assert_eq!(score.port_kind(), "capture.score");
        assert_eq!(signal.port_kind(), "capture.signal");
        assert_eq!(trigger.port_kind(), "capture.trigger");
        assert_eq!(target.port_kind(), "capture.target");
        assert_eq!(structured.port_kind(), "data.packet.v1");
        assert_eq!(structured.flow_sequence(), None);
    }

    #[test]
    fn typed_field_packets_use_generic_field_port_kind() {
        for value in [
            TypedValue::Bool(true),
            TypedValue::U8(1),
            TypedValue::I8(-1),
            TypedValue::U16(1),
            TypedValue::I16(-1),
            TypedValue::U32(1),
            TypedValue::I32(-1),
            TypedValue::U64(1),
            TypedValue::I64(-1),
            TypedValue::F32(1.0),
            TypedValue::F64(1.0),
            TypedValue::Str("value".to_owned()),
            TypedValue::Bytes(vec![0x12]),
        ] {
            let field = DataPacket::TypedField {
                datum: Arc::new(Datum::new("example.field", value)),
                generation: 1,
                source: Arc::new(TypedFieldSource::new(
                    "example.packet.v1",
                    PacketProvenance::default(),
                    Some("example.model.v1".to_owned()),
                )),
            };
            assert_eq!(field.port_kind(), "data.field.v1");
            assert_eq!(field.flow_sequence(), None);
        }
    }

    #[test]
    fn existing_variants_keep_their_port_kinds() {
        let json = DataPacket::Json(Arc::new(serde_json::json!({})));
        assert_eq!(json.port_kind(), "status.metrics");
    }
}
