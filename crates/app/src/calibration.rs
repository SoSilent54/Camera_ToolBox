//! 标定数据集会话与结果安装事务。

use std::{collections::HashMap, sync::Arc};

use camera_toolbox_core::{
    BoardSpec, CalibrationDataError, CalibrationImageSize, CalibrationRequest, CalibrationSolution,
    ChessboardDetection, ChessboardDetectionOutcome, InitialIntrinsics, PANGBOT_CALIBRATION_FLAGS,
    ViewCalibrationResult,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    FileRef, FileSystem, FileSystemError, FileVersion, FsControl, ReadRequest, SnapshotHash,
    StreamFrameIdentity, StreamSessionId,
};

/// `OpenCV` 标定至少需要多个不同姿态；UI readiness 使用这一保守下限。
pub const MIN_CALIBRATION_VIEWS: usize = 3;
/// 当前权威检测配方的 app 级 fingerprint；baseline 必须精确匹配。
pub const AUTO_CAPTURE_DETECTOR_FINGERPRINT: &str =
    "opencv-findChessboardCorners-subpix-authoritative/v1";
/// 当前自动准入 feature 公式版本；baseline 必须精确匹配。
pub const AUTO_CAPTURE_FEATURE_SCHEMA_VERSION: &str = "field-pnp-coverage/v3";

/// 直播帧进入标定数据集时的稳定身份；时间戳只保留在 provenance，不能参与幂等键。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StreamCaptureId {
    pub stream_id: StreamSessionId,
    pub channel: u16,
    pub frame_sequence: u64,
}

impl From<&StreamFrameIdentity> for StreamCaptureId {
    fn from(identity: &StreamFrameIdentity) -> Self {
        Self {
            stream_id: identity.stream_id.clone(),
            channel: identity.channel,
            frame_sequence: identity.frame_sequence,
        }
    }
}

/// 标定输入不等同于文件系统对象；直播快照不得伪造成 `FileRef`。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CalibrationInputKey {
    File(FileRef),
    StreamCapture(StreamCaptureId),
}

impl CalibrationInputKey {
    #[must_use]
    pub fn file_reference(&self) -> Option<&FileRef> {
        match self {
            Self::File(reference) => Some(reference),
            Self::StreamCapture(_) => None,
        }
    }
}

impl From<FileRef> for CalibrationInputKey {
    fn from(reference: FileRef) -> Self {
        Self::File(reference)
    }
}

/// 每次检测必须绑定不可变输入 revision，避免异步结果覆盖已替换的 source。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CalibrationInputRevision {
    File(FileVersion),
    EphemeralPng {
        content_sha256: String,
        encoded_bytes: u64,
    },
}

impl CalibrationInputRevision {
    #[must_use]
    pub const fn encoded_bytes(&self) -> u64 {
        match self {
            Self::File(version) => version.size,
            Self::EphemeralPng { encoded_bytes, .. } => *encoded_bytes,
        }
    }

    #[must_use]
    pub const fn file_version(&self) -> Option<FileVersion> {
        match self {
            Self::File(version) => Some(*version),
            Self::EphemeralPng { .. } => None,
        }
    }
}

impl From<FileVersion> for CalibrationInputRevision {
    fn from(version: FileVersion) -> Self {
        Self::File(version)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationEncodedPng {
    pub bytes: Arc<[u8]>,
    pub image_size: CalibrationImageSize,
    pub source_revision: CalibrationInputRevision,
}

/// 自动准入使用的图像方向；当前只允许未旋转的 OpenCV 图像坐标。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CalibrationImageOrientation {
    Upright,
}

/// 自动准入使用的像素坐标约定；检测点沿用 OpenCV 像素中心坐标。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelCoordinateConvention {
    OpenCvPixelCenters,
}

/// 自动准入绑定的源图像 ROI；`None` 表示完整帧。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CalibrationImageCrop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// 自动准入绑定的真实采集来源；必须同时绑定源、通道和几何 profile。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AutoCaptureAcquisitionKey {
    pub source_fingerprint: String,
    pub channel: u16,
    pub geometry_key: String,
}

impl AutoCaptureAcquisitionKey {
    /// 构造已类型化的采集来源 key；空 source/geometry 会使自动准入失效。
    pub fn new(
        source_fingerprint: impl Into<String>,
        channel: u16,
        geometry_key: impl Into<String>,
    ) -> Result<Self, CalibrationSessionError> {
        let key = Self {
            source_fingerprint: source_fingerprint.into(),
            channel,
            geometry_key: geometry_key.into(),
        };
        key.validate()?;
        Ok(key)
    }

    fn validate(&self) -> Result<(), CalibrationSessionError> {
        if self.source_fingerprint.trim().is_empty() {
            return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                "acquisition source fingerprint is empty".to_owned(),
            ));
        }
        if self.geometry_key.trim().is_empty() {
            return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                "acquisition geometry key is empty".to_owned(),
            ));
        }
        Ok(())
    }
}

/// PnP/feature 证据绑定的初始内参、图像几何与可选采集来源。
#[derive(Clone, Debug, PartialEq)]
pub struct InitialIntrinsicsBinding {
    pub initial_intrinsics: InitialIntrinsics,
    pub reference_image_size: CalibrationImageSize,
    pub orientation: CalibrationImageOrientation,
    pub crop: Option<CalibrationImageCrop>,
    pub pixel_convention: PixelCoordinateConvention,
    pub acquisition_key: AutoCaptureAcquisitionKey,
    pub digest: SnapshotHash,
}

impl InitialIntrinsicsBinding {
    /// 为未裁剪、未旋转的完整帧创建 source-bound 初始内参绑定。
    pub fn full_frame(
        initial_intrinsics: InitialIntrinsics,
        reference_image_size: CalibrationImageSize,
        acquisition_key: AutoCaptureAcquisitionKey,
    ) -> Result<Self, CalibrationSessionError> {
        initial_intrinsics.validate()?;
        acquisition_key.validate().map_err(|error| {
            CalibrationSessionError::InvalidInitialIntrinsicsBinding(error.to_string())
        })?;
        let mut binding = Self {
            initial_intrinsics,
            reference_image_size,
            orientation: CalibrationImageOrientation::Upright,
            crop: None,
            pixel_convention: PixelCoordinateConvention::OpenCvPixelCenters,
            acquisition_key,
            digest: SnapshotHash::digest_bytes(&[]),
        };
        binding.digest = binding.compute_digest();
        Ok(binding)
    }

    /// 为普通 Dataset 评估创建来源无关的完整帧初始内参绑定。
    ///
    /// 自动候选准入仍必须使用 `full_frame` 传入真实 acquisition key；该入口只用于
    /// 已进入 Dataset 的本地、SFTP、手动 RTSP 与自动 RTSP 图片的 PnP 证据绑定。
    pub fn dataset_full_frame(
        initial_intrinsics: InitialIntrinsics,
        reference_image_size: CalibrationImageSize,
    ) -> Result<Self, CalibrationSessionError> {
        let acquisition_key = AutoCaptureAcquisitionKey::new(
            "dataset-pnp",
            0,
            format!(
                "dataset;full-frame={}x{};orientation=upright;pixel=opencv-centers",
                reference_image_size.width, reference_image_size.height
            ),
        )?;
        Self::full_frame(initial_intrinsics, reference_image_size, acquisition_key)
    }

    fn validate(&self) -> Result<(), CalibrationSessionError> {
        self.initial_intrinsics.validate()?;
        self.acquisition_key.validate().map_err(|error| {
            CalibrationSessionError::InvalidInitialIntrinsicsBinding(error.to_string())
        })?;
        if self.compute_digest() != self.digest {
            return Err(CalibrationSessionError::InvalidInitialIntrinsicsBinding(
                "digest does not match binding fields".to_owned(),
            ));
        }
        Ok(())
    }

    fn compute_digest(&self) -> SnapshotHash {
        let mut hash = SnapshotHash::builder("camera-toolbox/initial-intrinsics-binding/v1");
        hash.u32(self.reference_image_size.width);
        hash.u32(self.reference_image_size.height);
        hash.u8(match self.orientation {
            CalibrationImageOrientation::Upright => 0,
        });
        match self.crop {
            Some(crop) => {
                hash.u8(1);
                hash.u32(crop.x);
                hash.u32(crop.y);
                hash.u32(crop.width);
                hash.u32(crop.height);
            }
            None => hash.u8(0),
        }
        hash.u8(match self.pixel_convention {
            PixelCoordinateConvention::OpenCvPixelCenters => 0,
        });
        hash.string(&self.acquisition_key.source_fingerprint);
        hash.u16(self.acquisition_key.channel);
        hash.string(&self.acquisition_key.geometry_key);
        for value in self.initial_intrinsics.camera_matrix {
            hash.bytes(&value.to_bits().to_be_bytes());
        }
        hash.u64(self.initial_intrinsics.distortion_coefficients.len() as u64);
        for value in &self.initial_intrinsics.distortion_coefficients {
            hash.bytes(&value.to_bits().to_be_bytes());
        }
        hash.finish()
    }
}

/// 与精确初始内参绑定的紧凑 PnP 证据；不持有重复的 projected-points 缓冲区。
#[derive(Clone, Debug, PartialEq)]
pub struct PnPObservation {
    pub binding_digest: SnapshotHash,
    pub rotation_vector: [f64; 3],
    pub translation_vector: [f64; 3],
    pub depth: f64,
    pub minimum_board_depth: f64,
    pub maximum_board_depth: f64,
    pub tilt_degrees: f64,
    pub azimuth_degrees: f64,
    pub reprojection_rmse: f64,
    pub max_reprojection_error: f64,
}

#[derive(Clone, Copy, Debug)]
struct PnPGeometry {
    depth: f64,
    minimum_board_depth: f64,
    maximum_board_depth: f64,
    tilt_degrees: f64,
    azimuth_degrees: f64,
}

impl PnPObservation {
    /// 从 OpenCV `board frame -> camera frame` 结果提取确定性验收特征。
    pub fn from_view_result(
        binding_digest: SnapshotHash,
        result: ViewCalibrationResult,
        board: BoardSpec,
    ) -> Result<Self, CalibrationSessionError> {
        validate_pnp_metrics(result.reprojection_rmse, result.max_reprojection_error)?;
        let geometry =
            derive_pnp_geometry(result.rotation_vector, result.translation_vector, board)?;
        Ok(Self {
            binding_digest,
            rotation_vector: result.rotation_vector,
            translation_vector: result.translation_vector,
            depth: geometry.depth,
            minimum_board_depth: geometry.minimum_board_depth,
            maximum_board_depth: geometry.maximum_board_depth,
            tilt_degrees: geometry.tilt_degrees,
            azimuth_degrees: geometry.azimuth_degrees,
            reprojection_rmse: result.reprojection_rmse,
            max_reprojection_error: result.max_reprojection_error,
        })
    }

    /// 从不可变向量重新推导特征，不信任可显示字段作为准入依据。
    fn geometry(&self, board: BoardSpec) -> Result<PnPGeometry, CalibrationSessionError> {
        validate_pnp_metrics(self.reprojection_rmse, self.max_reprojection_error)?;
        if !self.depth.is_finite()
            || !self.minimum_board_depth.is_finite()
            || !self.maximum_board_depth.is_finite()
            || !self.tilt_degrees.is_finite()
            || !self.azimuth_degrees.is_finite()
        {
            return Err(CalibrationSessionError::RejectInvalidPnP(
                "stored PnP evidence contains non-finite derived values".to_owned(),
            ));
        }
        derive_pnp_geometry(self.rotation_vector, self.translation_vector, board)
    }
}

fn validate_pnp_metrics(
    reprojection_rmse: f64,
    max_reprojection_error: f64,
) -> Result<(), CalibrationSessionError> {
    if !reprojection_rmse.is_finite()
        || reprojection_rmse < 0.0
        || !max_reprojection_error.is_finite()
        || max_reprojection_error < 0.0
    {
        return Err(CalibrationSessionError::RejectInvalidPnP(
            "reprojection metrics must be finite and non-negative".to_owned(),
        ));
    }
    Ok(())
}

fn derive_pnp_geometry(
    rotation_vector: [f64; 3],
    translation_vector: [f64; 3],
    board: BoardSpec,
) -> Result<PnPGeometry, CalibrationSessionError> {
    board.validate()?;
    if rotation_vector
        .iter()
        .chain(&translation_vector)
        .any(|value| !value.is_finite())
    {
        return Err(CalibrationSessionError::RejectInvalidPnP(
            "pose vectors must be finite".to_owned(),
        ));
    }
    let rotation = rodrigues_matrix(rotation_vector)?;
    // 平面棋盘上相机深度是 x/y 的线性函数；四个边界角即可覆盖全部内角点极值。
    let width = f64::from(board.inner_cols.saturating_sub(1)) * board.square_size;
    let height = f64::from(board.inner_rows.saturating_sub(1)) * board.square_size;
    let corner_depths = [[0.0, 0.0], [width, 0.0], [0.0, height], [width, height]]
        .into_iter()
        .map(|[x, y]| rotation[2][0] * x + rotation[2][1] * y + translation_vector[2]);
    let (minimum_board_depth, maximum_board_depth) = corner_depths.fold(
        (f64::INFINITY, f64::NEG_INFINITY),
        |(minimum, maximum), depth| (minimum.min(depth), maximum.max(depth)),
    );
    if !minimum_board_depth.is_finite() || minimum_board_depth <= 0.0 {
        return Err(CalibrationSessionError::RejectInvalidPnP(format!(
            "all board points must have positive camera depth, minimum is {minimum_board_depth:.6}"
        )));
    }
    let normal = [rotation[0][2], rotation[1][2], rotation[2][2]];
    let tilt_degrees = normal[0].hypot(normal[1]).atan2(normal[2]).to_degrees();
    let azimuth_degrees = normal[1].atan2(normal[0]).to_degrees().rem_euclid(360.0);
    if !tilt_degrees.is_finite() || !azimuth_degrees.is_finite() {
        return Err(CalibrationSessionError::RejectInvalidPnP(
            "derived board-normal angles are non-finite".to_owned(),
        ));
    }
    Ok(PnPGeometry {
        depth: translation_vector[2],
        minimum_board_depth,
        maximum_board_depth,
        tilt_degrees,
        azimuth_degrees,
    })
}

fn rodrigues_matrix(rotation_vector: [f64; 3]) -> Result<[[f64; 3]; 3], CalibrationSessionError> {
    let theta = rotation_vector[0]
        .hypot(rotation_vector[1])
        .hypot(rotation_vector[2]);
    if !theta.is_finite() {
        return Err(CalibrationSessionError::RejectInvalidPnP(
            "rotation magnitude is non-finite".to_owned(),
        ));
    }
    if theta <= 1.0e-12 {
        return Ok([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
    }
    let [x, y, z] = rotation_vector.map(|value| value / theta);
    let cosine = theta.cos();
    let sine = theta.sin();
    let one_minus_cosine = 1.0 - cosine;
    Ok([
        [
            cosine + x * x * one_minus_cosine,
            x * y * one_minus_cosine - z * sine,
            x * z * one_minus_cosine + y * sine,
        ],
        [
            y * x * one_minus_cosine + z * sine,
            cosine + y * y * one_minus_cosine,
            y * z * one_minus_cosine - x * sine,
        ],
        [
            z * x * one_minus_cosine - y * sine,
            z * y * one_minus_cosine + x * sine,
            cosine + z * z * one_minus_cosine,
        ],
    ])
}

/// Dataset Acceptance 阈值；GUI 可从 YAML 配置安装，运行时基线仍持有不可变快照。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutoCaptureAcceptanceCriteria {
    pub field_columns: usize,
    pub field_rows: usize,
    /// 每个 Field cell 需要累计的棋盘角点数；达到前每个角点都提供 Gain。
    pub field_target_per_cell: usize,
    pub min_adjacent_spacing_px: f32,
    pub pnp_depth_min: f64,
    pub pnp_depth_max: f64,
    pub pnp_depth_bins: usize,
    /// 每个 depth bin 需要累计的棋盘角点深度数。
    pub depth_target_per_bin: usize,
    pub pnp_tilt_deadband_deg: f64,
    pub pnp_tilt_max_deg: f64,
    pub pnp_tilt_bins: usize,
    pub pnp_azimuth_sectors: usize,
    /// 每个 pose bin 需要累计的合格 view 数。
    pub pose_target_per_bin: usize,
    pub pnp_max_rmse_px: f64,
    pub pnp_max_error_px: f64,
    /// 自动候选单张图的最小归一化总 Gain；低于该值不允许入库。
    pub minimum_auto_gain: f64,
}

impl AutoCaptureAcceptanceCriteria {
    fn validate(&self) -> Result<(), CalibrationSessionError> {
        if self.field_columns == 0
            || self.field_rows == 0
            || self.field_columns > 32
            || self.field_rows > 32
            || self.field_target_per_cell == 0
        {
            return Err(CalibrationSessionError::InvalidAutoCaptureBaseline(
                "field grid and per-cell target are invalid".to_owned(),
            ));
        }
        if self
            .field_columns
            .checked_mul(self.field_rows)
            .and_then(|regions| regions.checked_mul(self.field_target_per_cell))
            .is_none()
        {
            return Err(CalibrationSessionError::InvalidAutoCaptureBaseline(
                "field quota target overflows usize".to_owned(),
            ));
        }
        if !self.min_adjacent_spacing_px.is_finite() || self.min_adjacent_spacing_px <= 0.0 {
            return Err(CalibrationSessionError::InvalidAutoCaptureBaseline(
                "minimum adjacent spacing must be positive and finite".to_owned(),
            ));
        }
        if !self.pnp_depth_min.is_finite()
            || !self.pnp_depth_max.is_finite()
            || self.pnp_depth_min <= 0.0
            || self.pnp_depth_max <= self.pnp_depth_min
            || !(1..=32).contains(&self.pnp_depth_bins)
            || self.depth_target_per_bin == 0
        {
            return Err(CalibrationSessionError::InvalidAutoCaptureBaseline(
                "PnP depth range, bin count, or per-bin target is invalid".to_owned(),
            ));
        }
        if self
            .pnp_depth_bins
            .checked_mul(self.depth_target_per_bin)
            .is_none()
        {
            return Err(CalibrationSessionError::InvalidAutoCaptureBaseline(
                "depth quota target overflows usize".to_owned(),
            ));
        }
        let pose_capacity = pose_bin_capacity_for_values(
            self.pnp_tilt_bins,
            self.pnp_azimuth_sectors,
            self.pnp_tilt_deadband_deg,
        )
        .ok_or_else(|| {
            CalibrationSessionError::InvalidAutoCaptureBaseline(
                "PnP pose-bin capacity overflows usize".to_owned(),
            )
        })?;
        if !self.pnp_tilt_deadband_deg.is_finite()
            || !self.pnp_tilt_max_deg.is_finite()
            || self.pnp_tilt_deadband_deg < 0.0
            || self.pnp_tilt_max_deg <= self.pnp_tilt_deadband_deg
            || self.pnp_tilt_max_deg >= 90.0
            || !(1..=16).contains(&self.pnp_tilt_bins)
            || !(1..=32).contains(&self.pnp_azimuth_sectors)
            || pose_capacity == 0
            || self.pose_target_per_bin == 0
        {
            return Err(CalibrationSessionError::InvalidAutoCaptureBaseline(
                "PnP tilt/azimuth bins or per-pose target are invalid".to_owned(),
            ));
        }
        if pose_capacity
            .checked_mul(self.pose_target_per_bin)
            .is_none()
        {
            return Err(CalibrationSessionError::InvalidAutoCaptureBaseline(
                "pose quota target overflows usize".to_owned(),
            ));
        }
        if !self.pnp_max_rmse_px.is_finite()
            || !self.pnp_max_error_px.is_finite()
            || self.pnp_max_rmse_px < 0.0
            || self.pnp_max_error_px < self.pnp_max_rmse_px
        {
            return Err(CalibrationSessionError::InvalidAutoCaptureBaseline(
                "PnP reprojection gates are invalid".to_owned(),
            ));
        }
        if !self.minimum_auto_gain.is_finite()
            || self.minimum_auto_gain <= 0.0
            || self.minimum_auto_gain > 1.0
        {
            return Err(CalibrationSessionError::InvalidAutoCaptureBaseline(
                "minimum automatic Gain must be finite in (0, 1]".to_owned(),
            ));
        }
        Ok(())
    }
}

/// 当前 live source、几何、棋盘与运行时阈值的不可变自动准入基线。
#[derive(Clone, Debug, PartialEq)]
pub struct AutoCaptureBaseline {
    pub acquisition_key: AutoCaptureAcquisitionKey,
    pub image_size: CalibrationImageSize,
    pub board: BoardSpec,
    pub criteria: AutoCaptureAcceptanceCriteria,
    pub digest: SnapshotHash,
}

impl AutoCaptureBaseline {
    pub fn new(
        acquisition_key: AutoCaptureAcquisitionKey,
        image_size: CalibrationImageSize,
        board: BoardSpec,
        criteria: AutoCaptureAcceptanceCriteria,
    ) -> Result<Self, CalibrationSessionError> {
        let mut baseline = Self {
            acquisition_key,
            image_size,
            board,
            criteria,
            digest: SnapshotHash::digest_bytes(&[]),
        };
        baseline.digest = baseline.compute_digest();
        baseline.validate()?;
        Ok(baseline)
    }

    fn validate(&self) -> Result<(), CalibrationSessionError> {
        self.board.validate()?;
        self.acquisition_key.validate().map_err(|error| {
            CalibrationSessionError::InvalidAutoCaptureBaseline(error.to_string())
        })?;
        self.criteria.validate()?;
        if self.compute_digest() != self.digest {
            return Err(CalibrationSessionError::InvalidAutoCaptureBaseline(
                "digest does not match baseline fields".to_owned(),
            ));
        }
        Ok(())
    }

    fn compute_digest(&self) -> SnapshotHash {
        let mut hash = SnapshotHash::builder("camera-toolbox/runtime-auto-capture-baseline/v4");
        hash.string(AUTO_CAPTURE_DETECTOR_FINGERPRINT);
        hash.bytes(&PANGBOT_CALIBRATION_FLAGS.to_be_bytes());
        hash.string(AUTO_CAPTURE_FEATURE_SCHEMA_VERSION);
        hash.string(&self.acquisition_key.source_fingerprint);
        hash.u16(self.acquisition_key.channel);
        hash.string(&self.acquisition_key.geometry_key);
        hash.u32(self.image_size.width);
        hash.u32(self.image_size.height);
        hash.u16(self.board.inner_cols);
        hash.u16(self.board.inner_rows);
        hash.bytes(&self.board.square_size.to_bits().to_be_bytes());
        hash.u64(self.criteria.field_columns as u64);
        hash.u64(self.criteria.field_rows as u64);
        hash.u64(self.criteria.field_target_per_cell as u64);
        hash.u32(self.criteria.min_adjacent_spacing_px.to_bits());
        hash.bytes(&self.criteria.pnp_depth_min.to_bits().to_be_bytes());
        hash.bytes(&self.criteria.pnp_depth_max.to_bits().to_be_bytes());
        hash.u64(self.criteria.pnp_depth_bins as u64);
        hash.u64(self.criteria.depth_target_per_bin as u64);
        hash.bytes(&self.criteria.pnp_tilt_deadband_deg.to_bits().to_be_bytes());
        hash.bytes(&self.criteria.pnp_tilt_max_deg.to_bits().to_be_bytes());
        hash.u64(self.criteria.pnp_tilt_bins as u64);
        hash.u64(self.criteria.pnp_azimuth_sectors as u64);
        hash.u64(self.criteria.pose_target_per_bin as u64);
        hash.bytes(&self.criteria.pnp_max_rmse_px.to_bits().to_be_bytes());
        hash.bytes(&self.criteria.pnp_max_error_px.to_bits().to_be_bytes());
        hash.bytes(&self.criteria.minimum_auto_gain.to_bits().to_be_bytes());
        hash.finish()
    }
}

fn depth_bin_for_criteria(
    criteria: &AutoCaptureAcceptanceCriteria,
    depth: f64,
) -> Result<usize, CalibrationSessionError> {
    if !depth.is_finite() || !(criteria.pnp_depth_min..=criteria.pnp_depth_max).contains(&depth) {
        return Err(CalibrationSessionError::RejectInvalidPnP(format!(
            "PnP depth {depth:.6} is outside [{:.6}, {:.6}]",
            criteria.pnp_depth_min, criteria.pnp_depth_max
        )));
    }
    let normalized =
        (depth - criteria.pnp_depth_min) / (criteria.pnp_depth_max - criteria.pnp_depth_min);
    Ok(((normalized * criteria.pnp_depth_bins as f64) as usize).min(criteria.pnp_depth_bins - 1))
}

/// 将棋盘每个内角点的相机 Z 深度计入对应区间；深度覆盖不再按单张 view/中心深度计数。
fn depth_corner_counts_for_criteria(
    observation: &PnPObservation,
    criteria: &AutoCaptureAcceptanceCriteria,
    board: BoardSpec,
) -> Result<Vec<(usize, usize)>, CalibrationSessionError> {
    board.validate()?;
    let rotation = rodrigues_matrix(observation.rotation_vector)?;
    let mut counts = vec![0_usize; criteria.pnp_depth_bins];
    for row in 0..board.inner_rows {
        for column in 0..board.inner_cols {
            let x = f64::from(column) * board.square_size;
            let y = f64::from(row) * board.square_size;
            let depth = rotation[2][0] * x + rotation[2][1] * y + observation.translation_vector[2];
            if !depth.is_finite() || depth <= 0.0 {
                return Err(CalibrationSessionError::RejectInvalidPnP(format!(
                    "all board points must have positive camera depth, got {depth:.6}"
                )));
            }
            if (criteria.pnp_depth_min..=criteria.pnp_depth_max).contains(&depth) {
                let bin = depth_bin_for_criteria(criteria, depth)?;
                counts[bin] = counts[bin].saturating_add(1);
            }
        }
    }
    Ok(counts
        .into_iter()
        .enumerate()
        .filter_map(|(bin, count)| (count != 0).then_some((bin, count)))
        .collect())
}

fn pose_center_bin_enabled(criteria: &AutoCaptureAcceptanceCriteria) -> bool {
    criteria.pnp_tilt_deadband_deg > 0.0
}

fn pose_bin_capacity_for_values(
    tilt_bins: usize,
    azimuth_sectors: usize,
    tilt_deadband_deg: f64,
) -> Option<usize> {
    let sector_bins = tilt_bins.checked_mul(azimuth_sectors)?;
    sector_bins.checked_add(usize::from(tilt_deadband_deg > 0.0))
}

fn pose_bin_capacity_for_criteria(criteria: &AutoCaptureAcceptanceCriteria) -> Option<usize> {
    pose_bin_capacity_for_values(
        criteria.pnp_tilt_bins,
        criteria.pnp_azimuth_sectors,
        criteria.pnp_tilt_deadband_deg,
    )
}

fn pose_bin_for_criteria(
    criteria: &AutoCaptureAcceptanceCriteria,
    tilt: f64,
    azimuth: f64,
) -> Result<usize, CalibrationSessionError> {
    if !tilt.is_finite() || !azimuth.is_finite() || tilt < 0.0 || tilt > criteria.pnp_tilt_max_deg {
        return Err(CalibrationSessionError::RejectInvalidPnP(format!(
            "PnP tilt {tilt:.6}° is outside [0, {:.6}]°",
            criteria.pnp_tilt_max_deg
        )));
    }
    if pose_center_bin_enabled(criteria) && tilt < criteria.pnp_tilt_deadband_deg {
        return Ok(0);
    }
    let offset = usize::from(pose_center_bin_enabled(criteria));
    let effective_min_tilt = if pose_center_bin_enabled(criteria) {
        criteria.pnp_tilt_deadband_deg
    } else {
        0.0
    };
    let normalized_tilt =
        (tilt - effective_min_tilt) / (criteria.pnp_tilt_max_deg - effective_min_tilt);
    let tilt_bin = ((normalized_tilt * criteria.pnp_tilt_bins as f64) as usize)
        .min(criteria.pnp_tilt_bins - 1);
    let wrapped_azimuth = azimuth.rem_euclid(360.0);
    let azimuth_sector = ((wrapped_azimuth / 360.0 * criteria.pnp_azimuth_sectors as f64) as usize)
        .min(criteria.pnp_azimuth_sectors - 1);
    Ok(offset + tilt_bin * criteria.pnp_azimuth_sectors + azimuth_sector)
}

/// 单次自动候选提交绑定的准入配置。
#[derive(Clone, Debug, PartialEq)]
pub struct AutoCandidateAdmission {
    pub baseline: AutoCaptureBaseline,
    pub initial_intrinsics_binding: InitialIntrinsicsBinding,
}

impl AutoCandidateAdmission {
    fn validate(&self) -> Result<(), CalibrationSessionError> {
        self.baseline.validate()?;
        self.initial_intrinsics_binding.validate()
    }
}

/// Dataset 行在当前验收 binding 下的 PnP 可用状态；用于区分 PnP 失败和有效零增益。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoAdmissionPnpState {
    Valid,
    MissingBinding,
    MissingObservation,
    BindingGap(String),
    DepthGap(String),
    PoseGap(String),
    RmseReprojectionGap(String),
    MaxReprojectionGap(String),
    Invalid(String),
}

impl AutoAdmissionPnpState {
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        matches!(self, Self::Valid)
    }

    #[must_use]
    pub const fn is_blocked(&self) -> bool {
        !self.is_valid()
    }
}

/// Dataset 内单项对当前目标封顶覆盖的归属贡献。
#[derive(Clone, Debug, PartialEq)]
pub struct AutoAdmissionItemContribution {
    pub item_id: CalibrationItemId,
    pub field_gain: f64,
    pub depth_gain: f64,
    pub pose_gain: f64,
    pub constraint_gain: f64,
    pub pnp_state: AutoAdmissionPnpState,
    pub depth_covered: bool,
    pub pose_covered: bool,
}

/// Dataset 单张图的棋盘角点深度范围；仅用于可视化，不参与 Gain 计算。
#[derive(Clone, Debug, PartialEq)]
pub struct AutoAdmissionDepthRange {
    pub item_id: CalibrationItemId,
    pub minimum_depth: f64,
    pub maximum_depth: f64,
    pub pnp_state: AutoAdmissionPnpState,
    pub reprojection_rmse: f64,
    pub max_reprojection_error: f64,
}

/// Dataset 单张图在 Field/Pose 可视化中的命中区域；用于表格选中高亮。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoAdmissionItemVisualization {
    pub item_id: CalibrationItemId,
    pub field_cells: Vec<usize>,
    pub pose_bin: Option<usize>,
    pub pnp_state: AutoAdmissionPnpState,
}

#[derive(Clone, Debug)]
struct DatasetAdmissionItemCoverage {
    item_id: CalibrationItemId,
    field_corner_counts: Vec<(usize, usize)>,
    depth_corner_counts: Vec<(usize, usize)>,
    pose_bin: Option<usize>,
    pnp_state: AutoAdmissionPnpState,
    depth_covered: bool,
    pose_covered: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AutoAdmissionAssessment {
    /// 生成本评估的有效 runtime 门限；无 active admission 时默认值为 `None`。
    pub active_criteria: Option<AutoCaptureAcceptanceCriteria>,
    pub field_columns: usize,
    pub field_rows: usize,
    /// 每个 Field 网格单元内的兼容棋盘角点数量；非零单元仍表示 occupied field cell。
    pub field_counts: Vec<usize>,
    pub field_cells: usize,
    pub required_field_cells: usize,
    pub field_quota_filled: usize,
    pub required_field_quota: usize,
    pub depth_bin_counts: Vec<usize>,
    pub depth_bins: usize,
    pub required_depth_bins: usize,
    pub depth_quota_filled: usize,
    pub required_depth_quota: usize,
    pub pose_bin_counts: Vec<usize>,
    pub pose_bins: usize,
    pub required_pose_bins: usize,
    pub pose_quota_filled: usize,
    pub required_pose_quota: usize,
    pub field_gain: f64,
    pub depth_gain: f64,
    pub pose_gain: f64,
    pub constraint_gain: f64,
    pub item_contributions: Vec<AutoAdmissionItemContribution>,
    pub depth_ranges: Vec<AutoAdmissionDepthRange>,
    pub item_visualizations: Vec<AutoAdmissionItemVisualization>,
    pub field_target_met: bool,
    pub depth_target_met: bool,
    pub pose_target_met: bool,
    pub collection_target_met: bool,
}

/// 当前 Dataset Score 使用归一化区域 quota 贡献：Field/Depth 按棋盘角点总数归一化，Pose 按单张 view 归一化。
fn target_capped_item_contributions(
    item_coverage: &[DatasetAdmissionItemCoverage],
    criteria: &AutoCaptureAcceptanceCriteria,
    pose_capacity: usize,
    board: BoardSpec,
) -> Result<Vec<AutoAdmissionItemContribution>, CalibrationSessionError> {
    let corner_count = board.corner_count()? as f64;
    let mut field_counts = vec![0_usize; criteria.field_columns * criteria.field_rows];
    let mut depth_counts = vec![0_usize; criteria.pnp_depth_bins];
    let mut pose_counts = vec![0_usize; pose_capacity];

    Ok(item_coverage
        .iter()
        .map(|coverage| {
            let raw_field_gain = target_capped_region_gain(
                &mut field_counts,
                criteria.field_target_per_cell,
                coverage.field_corner_counts.iter().copied(),
            );
            let raw_depth_gain = target_capped_region_gain(
                &mut depth_counts,
                criteria.depth_target_per_bin,
                coverage.depth_corner_counts.iter().copied(),
            );
            let raw_pose_gain = coverage.pose_bin.map_or(0, |pose_bin| {
                target_capped_region_gain(
                    &mut pose_counts,
                    criteria.pose_target_per_bin,
                    std::iter::once((pose_bin, 1)),
                )
            });
            let field_gain = corner_gain(raw_field_gain, corner_count);
            let depth_gain = corner_gain(raw_depth_gain, corner_count);
            let pose_gain = pose_gain(raw_pose_gain);
            AutoAdmissionItemContribution {
                item_id: coverage.item_id,
                field_gain,
                depth_gain,
                pose_gain,
                constraint_gain: constraint_gain(field_gain, depth_gain, pose_gain),
                pnp_state: coverage.pnp_state.clone(),
                depth_covered: coverage.depth_covered,
                pose_covered: coverage.pose_covered,
            }
        })
        .collect())
}

fn capped_region_score(counts: &[usize], target_per_region: usize) -> usize {
    saturating_sum(counts.iter().map(|count| (*count).min(target_per_region)))
}

fn saturating_sum(values: impl IntoIterator<Item = usize>) -> usize {
    values
        .into_iter()
        .fold(0_usize, |total, value| total.saturating_add(value))
}

fn corner_gain(raw_gain: usize, corner_count: f64) -> f64 {
    raw_gain as f64 / corner_count
}

fn pose_gain(raw_gain: usize) -> f64 {
    raw_gain as f64
}

fn sum_gain(values: impl IntoIterator<Item = f64>) -> f64 {
    values.into_iter().sum()
}

fn constraint_gain(field_gain: f64, depth_gain: f64, pose_gain: f64) -> f64 {
    (field_gain + depth_gain + pose_gain) / 3.0
}

fn capped_region_target(region_count: usize, target_per_region: usize) -> usize {
    region_count.saturating_mul(target_per_region)
}

fn target_capped_region_gain(
    counts: &mut [usize],
    target_per_region: usize,
    increments: impl IntoIterator<Item = (usize, usize)>,
) -> usize {
    let mut gain = 0_usize;
    for (region, increment) in increments {
        let before = counts[region].min(target_per_region);
        counts[region] = counts[region].saturating_add(increment);
        let after = counts[region].min(target_per_region);
        gain = gain.saturating_add(after.saturating_sub(before));
    }
    gain
}

/// 通过统一文件端口有界读取 `PNG`，并在进入 `OpenCV` 前解析 `IHDR` 尺寸。
///
/// # Errors
///
/// 源版本变化、读取越界或 PNG header 无效时返回错误。
pub fn read_calibration_png(
    file_system: &dyn FileSystem,
    reference: &FileRef,
    expected_version: FileVersion,
    max_encoded_bytes: u64,
    control: &FsControl,
) -> Result<CalibrationEncodedPng, CalibrationInputError> {
    let entry = file_system.stat(reference, control)?;
    if entry.version != expected_version {
        return Err(CalibrationInputError::SourceChanged {
            expected: expected_version,
            actual: entry.version,
        });
    }
    if entry.version.size > max_encoded_bytes {
        return Err(CalibrationInputError::EncodedImageTooLarge {
            size: entry.version.size,
            limit: max_encoded_bytes,
        });
    }
    let capacity = usize::try_from(entry.version.size).map_err(|_| {
        CalibrationInputError::EncodedImageTooLarge {
            size: entry.version.size,
            limit: usize::MAX as u64,
        }
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    let outcome = file_system.read(
        reference,
        ReadRequest {
            offset: 0,
            max_bytes: max_encoded_bytes,
        },
        control,
        &mut |chunk| {
            bytes.extend_from_slice(chunk);
            Ok(())
        },
    )?;
    if outcome.source_version != expected_version || outcome.bytes_read != entry.version.size {
        return Err(CalibrationInputError::SourceChanged {
            expected: expected_version,
            actual: outcome.source_version,
        });
    }
    let image_size = parse_png_dimensions(&bytes)?;
    Ok(CalibrationEncodedPng {
        bytes: Arc::from(bytes),
        image_size,
        source_revision: outcome.source_version.into(),
    })
}

fn parse_png_dimensions(bytes: &[u8]) -> Result<CalibrationImageSize, CalibrationInputError> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24
        || &bytes[..8] != PNG_SIGNATURE
        || u32::from_be_bytes(bytes[8..12].try_into().expect("fixed slice")) != 13
        || &bytes[12..16] != b"IHDR"
    {
        return Err(CalibrationInputError::InvalidPngHeader);
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("fixed slice"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("fixed slice"));
    CalibrationImageSize::new(width, height).map_err(CalibrationInputError::InvalidData)
}

#[derive(Debug, Error, PartialEq)]
pub enum CalibrationInputError {
    #[error(transparent)]
    FileSystem(#[from] FileSystemError),
    #[error("calibration source changed: expected {expected:?}, got {actual:?}")]
    SourceChanged {
        expected: FileVersion,
        actual: FileVersion,
    },
    #[error("encoded calibration image is {size} bytes, limit is {limit} bytes")]
    EncodedImageTooLarge { size: u64, limit: u64 },
    #[error("calibration detector accepts a PNG with a valid IHDR header only")]
    InvalidPngHeader,
    #[error(transparent)]
    InvalidData(#[from] CalibrationDataError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CalibrationItemId(u64);

impl CalibrationItemId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CalibrationItemStatus {
    Pending,
    ReadQueued,
    Reading,
    DetectQueued,
    Detecting,
    Found(ChessboardDetection),
    NotFound { image_size: CalibrationImageSize },
    Failed(String),
}

impl CalibrationItemStatus {
    #[must_use]
    pub const fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::ReadQueued | Self::Reading | Self::DetectQueued | Self::Detecting
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationDatasetItem {
    pub id: CalibrationItemId,
    pub input: CalibrationInputKey,
    pub revision: CalibrationInputRevision,
    pub display_name: String,
    pub enabled: bool,
    pub status: CalibrationItemStatus,
    pub acquisition_key: Option<AutoCaptureAcquisitionKey>,
    /// 普通 Dataset Acceptance / 预览使用的来源无关当前 K/D PnP 证据。
    pub pnp_observation: Option<PnPObservation>,
    /// 自动候选准入使用的精确 source-bound PnP 证据；不得被普通 Dataset 刷新覆盖。
    pub admission_pnp_observation: Option<PnPObservation>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationJobToken {
    pub item_id: CalibrationItemId,
    detection_epoch: u64,
    job_id: u64,
    source_revision: CalibrationInputRevision,
    board: BoardSpec,
}

impl CalibrationJobToken {
    #[must_use]
    pub const fn board(&self) -> BoardSpec {
        self.board
    }

    #[must_use]
    pub fn source_revision(&self) -> &CalibrationInputRevision {
        &self.source_revision
    }
}

/// 自动候选的会话内身份；它在 Dataset item 创建前存在。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AutoCandidateId(u64);

impl AutoCandidateId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// 自动候选从冻结 PNG 到最终提交都必须保持一致的不可变绑定。
#[derive(Clone, Debug, PartialEq)]
pub struct AutoCandidateToken {
    id: AutoCandidateId,
    input: CalibrationInputKey,
    source_revision: CalibrationInputRevision,
    display_name: String,
    frame_identity: StreamFrameIdentity,
    source_acquisition_key: Option<AutoCaptureAcquisitionKey>,
    board: BoardSpec,
    fit_manifest_revision: u64,
    admission_revision: u64,
}

impl AutoCandidateToken {
    #[must_use]
    pub const fn id(&self) -> AutoCandidateId {
        self.id
    }

    #[must_use]
    pub const fn board(&self) -> BoardSpec {
        self.board
    }

    #[must_use]
    pub fn source_revision(&self) -> &CalibrationInputRevision {
        &self.source_revision
    }

    #[must_use]
    pub fn frame_identity(&self) -> &StreamFrameIdentity {
        &self.frame_identity
    }

    #[must_use]
    pub const fn admission_revision(&self) -> u64 {
        self.admission_revision
    }
}

/// authoritative detection、PnP 证据与其原始候选 token 的单一提交对象。
#[derive(Clone, Debug, PartialEq)]
pub struct AutoCandidateCommit {
    token: AutoCandidateToken,
    observed_revision: CalibrationInputRevision,
    detection: ChessboardDetection,
    pnp_observation: PnPObservation,
}

impl AutoCandidateCommit {
    #[must_use]
    pub fn new(
        token: AutoCandidateToken,
        observed_revision: CalibrationInputRevision,
        detection: ChessboardDetection,
        pnp_observation: PnPObservation,
    ) -> Self {
        Self {
            token,
            observed_revision,
            detection,
            pnp_observation,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationSnapshot {
    pub item_ids: Vec<CalibrationItemId>,
    pub request: CalibrationRequest,
    solution_revision: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstalledCalibration {
    pub item_ids: Vec<CalibrationItemId>,
    pub request: CalibrationRequest,
    pub solution: CalibrationSolution,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddCalibrationItemOutcome {
    Added(CalibrationItemId),
    AlreadyPresent(CalibrationItemId),
    SourceChanged(CalibrationItemId),
}

#[derive(Clone, Debug)]
pub struct CalibrationSession {
    board: BoardSpec,
    items: Vec<CalibrationDatasetItem>,
    selected: Option<CalibrationItemId>,
    installed: Option<InstalledCalibration>,
    active_auto_baseline: Option<AutoCaptureBaseline>,
    active_initial_intrinsics_binding: Option<InitialIntrinsicsBinding>,
    solution_revision: u64,
    detection_epoch: u64,
    auto_admission_revision: u64,
    active_detection_jobs: HashMap<CalibrationItemId, u64>,
    next_detection_job_id: u64,
    next_id: u64,
}

impl CalibrationSession {
    #[must_use]
    pub fn new(board: BoardSpec) -> Self {
        Self {
            board,
            items: Vec::new(),
            selected: None,
            installed: None,
            active_auto_baseline: None,
            active_initial_intrinsics_binding: None,
            solution_revision: 1,
            detection_epoch: 1,
            auto_admission_revision: 1,
            active_detection_jobs: HashMap::new(),
            next_detection_job_id: 1,
            next_id: 1,
        }
    }

    #[must_use]
    pub const fn board(&self) -> BoardSpec {
        self.board
    }

    #[must_use]
    pub fn items(&self) -> &[CalibrationDatasetItem] {
        &self.items
    }

    #[must_use]
    pub const fn selected(&self) -> Option<CalibrationItemId> {
        self.selected
    }

    #[must_use]
    pub fn installed(&self) -> Option<&InstalledCalibration> {
        self.installed.as_ref()
    }

    /// 安装或清除运行时自动准入。相同的精确配置不推进 admission revision。
    ///
    /// # Errors
    ///
    /// baseline/binding 必须成对、有效且与当前 session 的棋盘、图像几何和 source 一致。
    pub fn configure_auto_admission(
        &mut self,
        baseline: Option<AutoCaptureBaseline>,
        binding: Option<InitialIntrinsicsBinding>,
    ) -> Result<(), CalibrationSessionError> {
        match (&baseline, &binding) {
            (Some(baseline), Some(binding)) => {
                baseline.validate()?;
                binding.validate()?;
                if baseline.board != self.board {
                    return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                        "baseline BoardSpec does not match active session board".to_owned(),
                    ));
                }
                if baseline.image_size != binding.reference_image_size {
                    return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                        "baseline image size does not match intrinsics binding reference image size"
                            .to_owned(),
                    ));
                }
                if baseline.acquisition_key != binding.acquisition_key {
                    return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                        "baseline acquisition source does not match intrinsics binding".to_owned(),
                    ));
                }
            }
            (None, None) => {}
            _ => {
                return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                    "baseline and InitialIntrinsicsBinding must be configured together".to_owned(),
                ));
            }
        }
        if self.active_auto_baseline == baseline
            && self.active_initial_intrinsics_binding == binding
        {
            return Ok(());
        }
        self.active_auto_baseline = baseline;
        self.active_initial_intrinsics_binding = binding;
        self.invalidate_auto_admission();
        Ok(())
    }

    #[must_use]
    pub const fn auto_capture_baseline(&self) -> Option<&AutoCaptureBaseline> {
        self.active_auto_baseline.as_ref()
    }

    #[must_use]
    pub const fn initial_intrinsics_binding(&self) -> Option<&InitialIntrinsicsBinding> {
        self.active_initial_intrinsics_binding.as_ref()
    }

    /// 用当前 app/session 契约评估自动准入进度；候选必须同时提供 source-bound PnP 证据。
    ///
    /// # Errors
    ///
    /// 缺 baseline/binding、几何不匹配或候选不满足硬门规时返回错误。
    pub fn assess_auto_admission(
        &self,
        candidate: Option<(&ChessboardDetection, &PnPObservation)>,
    ) -> Result<AutoAdmissionAssessment, CalibrationSessionError> {
        let admission = self.active_admission()?;
        self.assess_auto_admission_with(&admission, candidate)
    }
    /// 评估当前 Dataset 的验收进度，不以文件、SFTP 或 RTSP 的 acquisition key 过滤。
    ///
    /// 自动候选仍必须调用 `assess_auto_admission`；该入口只服务已进入 Dataset 的
    /// 来源无关进度和边际贡献展示。
    ///
    /// # Errors
    ///
    /// 阈值、PnP binding 或统一图像尺寸无效时返回错误。
    pub fn assess_dataset_acceptance(
        &self,
        image_size: CalibrationImageSize,
        criteria: &AutoCaptureAcceptanceCriteria,
        pnp_binding: Option<&InitialIntrinsicsBinding>,
    ) -> Result<AutoAdmissionAssessment, CalibrationSessionError> {
        criteria.validate()?;
        if let Some(binding) = pnp_binding {
            binding.validate()?;
            if binding.reference_image_size != image_size {
                return Err(CalibrationSessionError::RejectIncompatibleImageSize {
                    expected: binding.reference_image_size,
                    actual: image_size,
                });
            }
        }

        let pose_capacity = pose_bin_capacity_for_criteria(criteria).ok_or_else(|| {
            CalibrationSessionError::InvalidAutoCaptureBaseline(
                "PnP pose-bin capacity overflows usize".to_owned(),
            )
        })?;
        let mut field_counts = vec![0_usize; criteria.field_columns * criteria.field_rows];
        let mut depth_bin_counts = vec![0_usize; criteria.pnp_depth_bins];
        let mut pose_bin_counts = vec![0_usize; pose_capacity];
        let mut item_coverage = Vec::new();
        let mut depth_ranges = Vec::new();
        let mut item_visualizations = Vec::new();

        for item in self.items.iter().filter(|item| item.enabled) {
            let CalibrationItemStatus::Found(detection) = &item.status else {
                continue;
            };
            // 标定求解与验收共用一个图像几何；异尺寸项保留在 Dataset 中但不混入统计。
            if detection.image_size != image_size
                || Self::ensure_detection_inside_image(detection).is_err()
                || Self::ensure_min_adjacent_spacing(
                    detection,
                    self.board,
                    criteria.min_adjacent_spacing_px,
                )
                .is_err()
            {
                continue;
            }

            let field_corner_counts = Self::field_corner_counts(detection, criteria);
            for (cell, count) in &field_corner_counts {
                field_counts[*cell] = field_counts[*cell].saturating_add(*count);
            }

            // 无 PnP 的普通 Dataset 项仍能反映 Found/Field；Depth/Pose 必须有当前 K/D
            // binding 下的合格证据，绝不把历史或异几何结果当作当前姿态覆盖。
            let (depth_corner_counts, pose_bin, pnp_state) =
                match (pnp_binding, item.pnp_observation.as_ref()) {
                    (None, _) => (Vec::new(), None, AutoAdmissionPnpState::MissingBinding),
                    (Some(_), None) => {
                        (Vec::new(), None, AutoAdmissionPnpState::MissingObservation)
                    }
                    (Some(binding), Some(observation)) => match Self::pnp_coverage_for_dataset(
                        observation,
                        criteria,
                        binding,
                        self.board,
                    ) {
                        Ok((depth_corner_counts, pose_bin)) => (
                            depth_corner_counts,
                            Some(pose_bin),
                            AutoAdmissionPnpState::Valid,
                        ),
                        Err(error) => {
                            (Vec::new(), None, Self::pnp_state_from_coverage_error(error))
                        }
                    },
                };
            if let (Some(binding), Some(observation)) = (pnp_binding, item.pnp_observation.as_ref())
                && let Ok((minimum_depth, maximum_depth)) =
                    Self::pnp_depth_range_for_dataset(observation, binding, self.board)
            {
                depth_ranges.push(AutoAdmissionDepthRange {
                    item_id: item.id,
                    minimum_depth,
                    maximum_depth,
                    pnp_state: pnp_state.clone(),
                    reprojection_rmse: observation.reprojection_rmse,
                    max_reprojection_error: observation.max_reprojection_error,
                });
            }
            item_visualizations.push(AutoAdmissionItemVisualization {
                item_id: item.id,
                field_cells: field_corner_counts.iter().map(|(cell, _)| *cell).collect(),
                pose_bin,
                pnp_state: pnp_state.clone(),
            });
            for (depth_bin, count) in &depth_corner_counts {
                depth_bin_counts[*depth_bin] = depth_bin_counts[*depth_bin].saturating_add(*count);
            }
            if let Some(pose_bin) = pose_bin {
                pose_bin_counts[pose_bin] = pose_bin_counts[pose_bin].saturating_add(1);
            }
            let depth_covered = !depth_corner_counts.is_empty();
            let pose_covered = pose_bin.is_some();
            item_coverage.push(DatasetAdmissionItemCoverage {
                item_id: item.id,
                field_corner_counts,
                depth_corner_counts,
                pose_bin,
                pnp_state,
                depth_covered,
                pose_covered,
            });
        }

        let field_cells = field_counts.iter().filter(|count| **count != 0).count();
        let depth_bins = depth_bin_counts.iter().filter(|count| **count != 0).count();
        let pose_bins = pose_bin_counts.iter().filter(|count| **count != 0).count();
        let field_quota_filled = capped_region_score(&field_counts, criteria.field_target_per_cell);
        let required_field_quota =
            capped_region_target(field_counts.len(), criteria.field_target_per_cell);
        let depth_quota_filled =
            capped_region_score(&depth_bin_counts, criteria.depth_target_per_bin);
        let required_depth_quota =
            capped_region_target(depth_bin_counts.len(), criteria.depth_target_per_bin);
        let pose_quota_filled = capped_region_score(&pose_bin_counts, criteria.pose_target_per_bin);
        let required_pose_quota =
            capped_region_target(pose_bin_counts.len(), criteria.pose_target_per_bin);
        let item_contributions =
            target_capped_item_contributions(&item_coverage, criteria, pose_capacity, self.board)?;
        let field_gain = sum_gain(
            item_contributions
                .iter()
                .map(|contribution| contribution.field_gain),
        );
        let depth_gain = sum_gain(
            item_contributions
                .iter()
                .map(|contribution| contribution.depth_gain),
        );
        let pose_gain = sum_gain(
            item_contributions
                .iter()
                .map(|contribution| contribution.pose_gain),
        );
        let field_target_met = field_quota_filled >= required_field_quota;
        let depth_target_met = depth_quota_filled >= required_depth_quota;
        let pose_target_met = pose_quota_filled >= required_pose_quota;
        Ok(AutoAdmissionAssessment {
            active_criteria: Some(criteria.clone()),
            field_columns: criteria.field_columns,
            field_rows: criteria.field_rows,
            field_counts,
            field_cells,
            required_field_cells: criteria.field_columns * criteria.field_rows,
            field_quota_filled,
            required_field_quota,
            depth_bin_counts,
            depth_bins,
            required_depth_bins: criteria.pnp_depth_bins,
            depth_quota_filled,
            required_depth_quota,
            pose_bin_counts,
            pose_bins,
            required_pose_bins: pose_capacity,
            pose_quota_filled,
            required_pose_quota,
            field_gain,
            depth_gain,
            pose_gain,
            constraint_gain: constraint_gain(field_gain, depth_gain, pose_gain),
            item_contributions,
            depth_ranges,
            item_visualizations,
            field_target_met,
            depth_target_met,
            pose_target_met,
            collection_target_met: field_target_met && depth_target_met && pose_target_met,
        })
    }

    /// 更新棋盘定义。
    ///
    /// 内角点行列改变时，既有检测与新 pattern 不再匹配，全部重置为 `Pending`；
    /// 仅相邻角点物理尺寸改变时，保留像素检测结果，只使既有标定解失效。
    ///
    /// # Errors
    ///
    /// 棋盘参数无效时返回错误。
    pub fn set_board(&mut self, board: BoardSpec) -> Result<(), CalibrationSessionError> {
        board.validate()?;
        if self.board == board {
            return Ok(());
        }
        let corner_layout_changed =
            self.board.inner_cols != board.inner_cols || self.board.inner_rows != board.inner_rows;
        self.board = board;
        // BoardSpec 改变会改变 PnP object points；旧基线和 K/D 绑定不能跨棋盘复用。
        self.active_auto_baseline = None;
        self.active_initial_intrinsics_binding = None;
        self.invalidate_auto_admission();
        for item in &mut self.items {
            if corner_layout_changed || item.status.is_busy() {
                item.status = CalibrationItemStatus::Pending;
            }
            // PnP 坐标以 BoardSpec 单位定义；任何棋盘变化均使历史 pose 失效。
            item.pnp_observation = None;
            item.admission_pnp_observation = None;
        }
        self.invalidate_detection_epoch();
        Ok(())
    }

    /// 清空全部检测与标定结果，保留 Dataset、选择和 Use 状态。
    pub fn reset_detections(&mut self) {
        for item in &mut self.items {
            item.status = CalibrationItemStatus::Pending;
            item.pnp_observation = None;
            item.admission_pnp_observation = None;
        }
        self.invalidate_detection_epoch();
    }

    pub fn add_or_refresh(
        &mut self,
        input: impl Into<CalibrationInputKey>,
        revision: impl Into<CalibrationInputRevision>,
        display_name: String,
    ) -> AddCalibrationItemOutcome {
        self.add_or_refresh_with_acquisition_key(input, revision, display_name, None)
    }

    pub fn add_or_refresh_with_acquisition_key(
        &mut self,
        input: impl Into<CalibrationInputKey>,
        revision: impl Into<CalibrationInputRevision>,
        display_name: String,
        acquisition_key: Option<AutoCaptureAcquisitionKey>,
    ) -> AddCalibrationItemOutcome {
        let input = input.into();
        let revision = revision.into();
        if let Some(index) = self.items.iter().position(|item| item.input == input) {
            let item = &mut self.items[index];
            if item.revision == revision {
                return AddCalibrationItemOutcome::AlreadyPresent(item.id);
            }
            item.revision = revision;
            item.display_name = display_name;
            item.status = CalibrationItemStatus::Pending;
            item.acquisition_key = acquisition_key;
            item.pnp_observation = None;
            item.admission_pnp_observation = None;
            let id = item.id;
            self.invalidate_detection_epoch();
            return AddCalibrationItemOutcome::SourceChanged(id);
        }

        let id = CalibrationItemId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.items.push(CalibrationDatasetItem {
            id,
            input,
            revision,
            display_name,
            enabled: true,
            status: CalibrationItemStatus::Pending,
            acquisition_key,
            pnp_observation: None,
            admission_pnp_observation: None,
        });
        self.selected.get_or_insert(id);
        self.invalidate_detection_epoch();
        AddCalibrationItemOutcome::Added(id)
    }

    /// 为尚未入库的直播 PNG 创建不可变候选绑定，不修改 Dataset。
    ///
    /// # Errors
    ///
    /// 输入不是匹配 identity 的 stream PNG、输入已存在或 revision 无效时返回错误。
    pub fn bind_auto_candidate(
        &self,
        id: AutoCandidateId,
        frame_identity: StreamFrameIdentity,
        source_revision: CalibrationInputRevision,
        display_name: String,
        source_acquisition_key: Option<AutoCaptureAcquisitionKey>,
    ) -> Result<AutoCandidateToken, CalibrationSessionError> {
        let input = CalibrationInputKey::StreamCapture(StreamCaptureId::from(&frame_identity));
        if self.items.iter().any(|item| item.input == input) {
            return Err(CalibrationSessionError::AutoCandidateAlreadyPresent);
        }
        if !matches!(
            &source_revision,
            CalibrationInputRevision::EphemeralPng {
                content_sha256,
                encoded_bytes,
            } if !content_sha256.is_empty() && *encoded_bytes > 0
        ) {
            return Err(CalibrationSessionError::InvalidAutoCandidateRevision);
        }
        if let Some(source_acquisition_key) = &source_acquisition_key {
            source_acquisition_key.validate()?;
            if source_acquisition_key.channel != frame_identity.channel {
                return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                    format!(
                        "candidate source channel {} does not match frame channel {}",
                        source_acquisition_key.channel, frame_identity.channel
                    ),
                ));
            }
        }
        Ok(AutoCandidateToken {
            id,
            input,
            source_revision,
            display_name,
            frame_identity,
            source_acquisition_key,
            board: self.board,
            fit_manifest_revision: self.solution_revision,
            admission_revision: self.auto_admission_revision,
        })
    }

    /// 原子提交已通过 authoritative detection 和 PnP 硬门规的自动候选。
    ///
    /// # Errors
    ///
    /// 任一不可变 binding 已过期、输入已存在或检测/PnP 证据无效时返回错误，且 Dataset 不变。
    pub fn commit_auto_candidate(
        &mut self,
        commit: AutoCandidateCommit,
    ) -> Result<CalibrationItemId, CalibrationSessionError> {
        let AutoCandidateCommit {
            token,
            observed_revision,
            detection,
            pnp_observation,
        } = commit;
        if token.board != self.board
            || token.fit_manifest_revision != self.solution_revision
            || token.admission_revision != self.auto_admission_revision
        {
            return Err(CalibrationSessionError::StaleAutoCandidate);
        }
        if observed_revision != token.source_revision {
            return Err(CalibrationSessionError::InvalidAutoCandidateRevision);
        }
        let expected_input =
            CalibrationInputKey::StreamCapture(StreamCaptureId::from(&token.frame_identity));
        if token.input != expected_input {
            return Err(CalibrationSessionError::InvalidAutoCandidateIdentity);
        }
        if self.items.iter().any(|item| item.input == token.input) {
            return Err(CalibrationSessionError::AutoCandidateAlreadyPresent);
        }
        detection.validate(token.board)?;
        let admission = self.active_admission()?;
        let source_acquisition_key = token.source_acquisition_key.as_ref().ok_or_else(|| {
            CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                "automatic candidate token has no acquisition source key".to_owned(),
            )
        })?;
        if *source_acquisition_key != admission.baseline.acquisition_key {
            return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                format!(
                    "candidate source {:?} does not match active baseline source {:?}",
                    source_acquisition_key, admission.baseline.acquisition_key
                ),
            ));
        }
        let assessment =
            self.assess_auto_admission_with(&admission, Some((&detection, &pnp_observation)))?;
        if assessment.constraint_gain < admission.baseline.criteria.minimum_auto_gain {
            return Err(CalibrationSessionError::RejectInsufficientConstraintGain {
                actual: assessment.constraint_gain,
                minimum: admission.baseline.criteria.minimum_auto_gain,
            });
        }

        let dataset_binding = InitialIntrinsicsBinding::dataset_full_frame(
            admission
                .initial_intrinsics_binding
                .initial_intrinsics
                .clone(),
            detection.image_size,
        )?;
        let mut dataset_pnp_observation = pnp_observation.clone();
        dataset_pnp_observation.binding_digest = dataset_binding.digest;

        let id = CalibrationItemId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.items.push(CalibrationDatasetItem {
            id,
            input: token.input,
            revision: token.source_revision,
            display_name: token.display_name,
            enabled: true,
            status: CalibrationItemStatus::Found(detection),
            acquisition_key: Some(source_acquisition_key.clone()),
            pnp_observation: Some(dataset_pnp_observation),
            admission_pnp_observation: Some(pnp_observation),
        });
        self.selected = Some(id);
        // 自动候选只改变 solver 输入；不得使其他 Dataset 检测令牌过期。
        self.invalidate_solution();
        Ok(id)
    }
    /// 使所有尚未提交的自动候选失效；不影响 Dataset detection token。
    pub fn invalidate_auto_admission(&mut self) {
        self.auto_admission_revision = self.auto_admission_revision.wrapping_add(1).max(1);
    }

    /// # Errors
    ///
    /// `id` 不属于当前数据集时返回错误。
    pub fn set_selected(&mut self, id: CalibrationItemId) -> Result<(), CalibrationSessionError> {
        self.item(id)?;
        self.selected = Some(id);
        Ok(())
    }

    /// # Errors
    ///
    /// `id` 不属于当前数据集时返回错误。
    pub fn set_enabled(
        &mut self,
        id: CalibrationItemId,
        enabled: bool,
    ) -> Result<(), CalibrationSessionError> {
        let item = self.item_mut(id)?;
        if item.enabled != enabled {
            item.enabled = enabled;
            self.invalidate_detection_epoch();
        }
        Ok(())
    }

    /// # Errors
    ///
    /// `id` 不属于当前数据集时返回错误。
    pub fn remove(&mut self, id: CalibrationItemId) -> Result<(), CalibrationSessionError> {
        let index = self
            .items
            .iter()
            .position(|item| item.id == id)
            .ok_or(CalibrationSessionError::UnknownItem(id))?;
        self.items.remove(index);
        if self.selected == Some(id) {
            self.selected = self
                .items
                .get(index.min(self.items.len().saturating_sub(1)))
                .map(|item| item.id);
        }
        self.invalidate_detection_epoch();
        Ok(())
    }

    pub fn clear(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.items.clear();
        self.selected = None;
        self.invalidate_detection_epoch();
    }

    /// 为文件输入创建检测令牌；读取 worker 取到任务后才转为 `Reading`。
    ///
    /// # Errors
    ///
    /// 数据项不存在或已有任务在途时返回错误。
    pub fn begin_detection(
        &mut self,
        id: CalibrationItemId,
    ) -> Result<CalibrationJobToken, CalibrationSessionError> {
        self.begin_detection_with_status(id, CalibrationItemStatus::ReadQueued)
    }

    /// 为已冻结、已预检的 encoded PNG 直接进入 detection queue 创建令牌。
    ///
    /// 调用方必须紧接着将任务放入 `pending_loaded`；channel 满时该状态保持为
    /// `DetectQueued`，直到 worker 发出 `Started`。
    ///
    /// # Errors
    ///
    /// 数据项不存在或已有任务在途时返回错误。
    pub fn begin_encoded_detection(
        &mut self,
        id: CalibrationItemId,
    ) -> Result<CalibrationJobToken, CalibrationSessionError> {
        self.begin_detection_with_status(id, CalibrationItemStatus::DetectQueued)
    }

    fn begin_detection_with_status(
        &mut self,
        id: CalibrationItemId,
        initial_status: CalibrationItemStatus,
    ) -> Result<CalibrationJobToken, CalibrationSessionError> {
        if self.active_detection_jobs.contains_key(&id) {
            return Err(CalibrationSessionError::ItemBusy(id));
        }
        let board = self.board;
        let source_revision = {
            let item = self.item_mut(id)?;
            if item.status.is_busy() {
                return Err(CalibrationSessionError::ItemBusy(id));
            }
            item.status = initial_status;
            item.revision.clone()
        };
        let job_id = self.next_detection_job_id;
        self.next_detection_job_id = self.next_detection_job_id.wrapping_add(1).max(1);
        self.active_detection_jobs.insert(id, job_id);
        self.invalidate_solution();
        Ok(CalibrationJobToken {
            item_id: id,
            detection_epoch: self.detection_epoch,
            job_id,
            source_revision,
            board,
        })
    }

    /// 将已进入 I/O worker 的读取任务从排队状态切换为活动读取。
    ///
    /// # Errors
    ///
    /// 令牌已过期、数据项不存在或任务不在读取队列中时返回错误。
    pub fn mark_reading(
        &mut self,
        token: &CalibrationJobToken,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_active_token(token)?;
        if !matches!(
            self.item(token.item_id)?.status,
            CalibrationItemStatus::ReadQueued
        ) {
            return Err(CalibrationSessionError::StaleResult);
        }
        self.item_mut(token.item_id)?.status = CalibrationItemStatus::Reading;
        Ok(())
    }

    /// 读取成功后标记为待检测；任务仍在检测 worker 的队列中。
    ///
    /// # Errors
    ///
    /// 令牌已过期、数据项不存在或读取尚未完成时返回错误。
    pub fn mark_detect_queued(
        &mut self,
        token: &CalibrationJobToken,
    ) -> Result<(), CalibrationSessionError> {
        self.mark_detection_queued_from(token, CalibrationItemStatus::Reading)
    }

    fn mark_detection_queued_from(
        &mut self,
        token: &CalibrationJobToken,
        expected_status: CalibrationItemStatus,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_active_token(token)?;
        if self.item(token.item_id)?.status != expected_status {
            return Err(CalibrationSessionError::StaleResult);
        }
        self.item_mut(token.item_id)?.status = CalibrationItemStatus::DetectQueued;
        Ok(())
    }

    /// 检测 worker 取到任务时标记为活动检测。
    ///
    /// # Errors
    ///
    /// 令牌已过期、数据项不存在或任务仍未进入检测队列时返回错误。
    pub fn mark_detecting(
        &mut self,
        token: &CalibrationJobToken,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_active_token(token)?;
        if !matches!(
            self.item(token.item_id)?.status,
            CalibrationItemStatus::DetectQueued
        ) {
            return Err(CalibrationSessionError::StaleResult);
        }
        self.item_mut(token.item_id)?.status = CalibrationItemStatus::Detecting;
        Ok(())
    }

    /// 仅在令牌和读取后版本仍匹配时安装检测结果。
    ///
    /// 不带 PnP 的调用保持原有行为；普通 Dataset 的异步姿态证据请使用
    /// `install_detection_with_pnp`，以避免检测与姿态跨版本错配。
    ///
    /// # Errors
    ///
    /// 令牌过期、源变化或检测结果无效时返回错误。
    pub fn install_detection(
        &mut self,
        token: &CalibrationJobToken,
        observed_revision: impl Into<CalibrationInputRevision>,
        outcome: ChessboardDetectionOutcome,
    ) -> Result<(), CalibrationSessionError> {
        self.install_detection_with_pnp(token, observed_revision, outcome, None)
    }

    /// 原子安装同一检测任务产出的 Found 结果和可选 PnP 证据。
    ///
    /// PnP 的 current-K/D binding 在评估时再次校验；这里先验证几何，保证无效证据
    /// 不能残留在 Dataset item 上。
    ///
    /// # Errors
    ///
    /// 令牌、版本、检测或 PnP 几何无效时返回错误。
    pub fn install_detection_with_pnp(
        &mut self,
        token: &CalibrationJobToken,
        observed_revision: impl Into<CalibrationInputRevision>,
        outcome: ChessboardDetectionOutcome,
        pnp_observation: Option<PnPObservation>,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_active_token(token)?;
        if observed_revision.into() != token.source_revision {
            self.active_detection_jobs.remove(&token.item_id);
            let item = self.item_mut(token.item_id)?;
            item.status = CalibrationItemStatus::Pending;
            item.pnp_observation = None;
            item.admission_pnp_observation = None;
            self.invalidate_solution();
            return Err(CalibrationSessionError::SourceChanged(token.item_id));
        }
        let (status, pnp_observation) = match outcome {
            ChessboardDetectionOutcome::Found(detection) => {
                detection.validate(token.board)?;
                if let Some(observation) = &pnp_observation {
                    observation.geometry(token.board)?;
                }
                (CalibrationItemStatus::Found(detection), pnp_observation)
            }
            ChessboardDetectionOutcome::NotFound { image_size } => {
                if pnp_observation.is_some() {
                    return Err(CalibrationSessionError::RejectInvalidPnP(
                        "PnP evidence requires a Found chessboard detection".to_owned(),
                    ));
                }
                (CalibrationItemStatus::NotFound { image_size }, None)
            }
        };
        self.active_detection_jobs.remove(&token.item_id);
        let item = self.item_mut(token.item_id)?;
        item.status = status;
        item.pnp_observation = pnp_observation;
        item.admission_pnp_observation = None;
        self.invalidate_solution();
        Ok(())
    }

    /// 为既有 Found Dataset 项安装或清除当前 K/D 绑定下的 PnP 证据。
    ///
    /// 该路径只重算姿态，不重读文件、不重跑角点检测；用于 GUI K/D 编辑后刷新 Depth/Pose
    /// 可视化。调用者仍需在安装前确认 binding digest 属于当前 GUI 状态。
    ///
    /// # Errors
    ///
    /// 数据项不存在、当前不是 Found 状态或 PnP 几何无效时返回错误。
    pub fn install_dataset_pnp_observation(
        &mut self,
        item_id: CalibrationItemId,
        pnp_observation: Option<PnPObservation>,
    ) -> Result<(), CalibrationSessionError> {
        let board = self.board;
        if let Some(observation) = &pnp_observation {
            observation.geometry(board)?;
        }
        let item = self.item_mut(item_id)?;
        if !matches!(item.status, CalibrationItemStatus::Found(_)) {
            return Err(CalibrationSessionError::StaleResult);
        }
        item.pnp_observation = pnp_observation;
        Ok(())
    }

    /// # Errors
    ///
    /// 令牌已过期或数据项不存在时返回错误。
    pub fn install_failure(
        &mut self,
        token: &CalibrationJobToken,
        message: String,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_active_token(token)?;
        self.active_detection_jobs.remove(&token.item_id);
        self.item_mut(token.item_id)?.status = CalibrationItemStatus::Failed(message);
        self.invalidate_solution();
        Ok(())
    }

    /// 用户取消检测时仅清除 busy 状态；后续到达的旧结果会被 active-token 校验拒绝。
    ///
    /// # Errors
    ///
    /// 令牌已过期或数据项不存在时返回错误。
    pub fn cancel_detection(
        &mut self,
        token: &CalibrationJobToken,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_active_token(token)?;
        self.active_detection_jobs.remove(&token.item_id);
        self.item_mut(token.item_id)?.status = CalibrationItemStatus::Pending;
        self.invalidate_solution();
        Ok(())
    }

    /// 快照所有 enabled 且检测成功的同尺寸图像。
    ///
    /// # Errors
    ///
    /// view 数量不足、尺寸不一致或初始内参无效时返回错误。
    pub fn calibration_snapshot(
        &self,
        initial_intrinsics: InitialIntrinsics,
    ) -> Result<CalibrationSnapshot, CalibrationSessionError> {
        let mut item_ids = Vec::new();
        let mut image_points = Vec::new();
        let mut image_size = None;
        for item in self.items.iter().filter(|item| item.enabled) {
            let CalibrationItemStatus::Found(detection) = &item.status else {
                continue;
            };
            if let Some(expected) = image_size {
                if expected != detection.image_size {
                    return Err(CalibrationSessionError::MixedImageSizes {
                        expected,
                        actual: detection.image_size,
                    });
                }
            } else {
                image_size = Some(detection.image_size);
            }
            item_ids.push(item.id);
            image_points.push(detection.corners.clone());
        }
        if item_ids.len() < MIN_CALIBRATION_VIEWS {
            return Err(CalibrationSessionError::NotEnoughViews {
                found: item_ids.len(),
                required: MIN_CALIBRATION_VIEWS,
            });
        }
        let image_size = image_size.ok_or(CalibrationSessionError::NotEnoughViews {
            found: 0,
            required: MIN_CALIBRATION_VIEWS,
        })?;
        let request = CalibrationRequest {
            image_size,
            board: self.board,
            image_points,
            initial_intrinsics,
        };
        request.validate()?;
        Ok(CalibrationSnapshot {
            item_ids,
            request,
            solution_revision: self.solution_revision,
        })
    }

    /// 仅在 session solution revision 未变化时安装并再次校验解算结果。
    ///
    /// # Errors
    ///
    /// 快照过期或解算结果不满足请求不变量时返回错误。
    pub fn install_solution(
        &mut self,
        snapshot: CalibrationSnapshot,
        solution: CalibrationSolution,
    ) -> Result<(), CalibrationSessionError> {
        if snapshot.solution_revision != self.solution_revision {
            return Err(CalibrationSessionError::StaleResult);
        }
        solution.validate_against(&snapshot.request)?;
        self.installed = Some(InstalledCalibration {
            item_ids: snapshot.item_ids,
            request: snapshot.request,
            solution,
        });
        Ok(())
    }

    fn validate_token(&self, token: &CalibrationJobToken) -> Result<(), CalibrationSessionError> {
        if token.detection_epoch != self.detection_epoch || token.board != self.board {
            return Err(CalibrationSessionError::StaleResult);
        }
        let item = self.item(token.item_id)?;
        if item.revision != token.source_revision {
            return Err(CalibrationSessionError::SourceChanged(token.item_id));
        }
        Ok(())
    }

    fn validate_active_token(
        &self,
        token: &CalibrationJobToken,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_token(token)?;
        if self.active_detection_jobs.get(&token.item_id) != Some(&token.job_id)
            || !self.item(token.item_id)?.status.is_busy()
        {
            return Err(CalibrationSessionError::StaleResult);
        }
        Ok(())
    }

    fn item(
        &self,
        id: CalibrationItemId,
    ) -> Result<&CalibrationDatasetItem, CalibrationSessionError> {
        self.items
            .iter()
            .find(|item| item.id == id)
            .ok_or(CalibrationSessionError::UnknownItem(id))
    }

    fn item_mut(
        &mut self,
        id: CalibrationItemId,
    ) -> Result<&mut CalibrationDatasetItem, CalibrationSessionError> {
        self.items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or(CalibrationSessionError::UnknownItem(id))
    }

    fn active_admission(&self) -> Result<AutoCandidateAdmission, CalibrationSessionError> {
        let baseline = self
            .active_auto_baseline
            .clone()
            .ok_or(CalibrationSessionError::RejectMissingBaseline)?;
        let initial_intrinsics_binding = self
            .active_initial_intrinsics_binding
            .clone()
            .ok_or(CalibrationSessionError::RejectMissingInitialIntrinsicsBinding)?;
        baseline.validate()?;
        initial_intrinsics_binding.validate()?;
        if baseline.board != self.board {
            return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                "baseline BoardSpec does not match active session board".to_owned(),
            ));
        }
        if baseline.image_size != initial_intrinsics_binding.reference_image_size {
            return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                "baseline image size does not match intrinsics binding reference image size"
                    .to_owned(),
            ));
        }
        if baseline.acquisition_key != initial_intrinsics_binding.acquisition_key {
            return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                "baseline acquisition source does not match intrinsics binding".to_owned(),
            ));
        }
        Ok(AutoCandidateAdmission {
            baseline,
            initial_intrinsics_binding,
        })
    }

    fn assess_auto_admission_with(
        &self,
        admission: &AutoCandidateAdmission,
        candidate: Option<(&ChessboardDetection, &PnPObservation)>,
    ) -> Result<AutoAdmissionAssessment, CalibrationSessionError> {
        admission.validate()?;
        let baseline = &admission.baseline;
        let binding = &admission.initial_intrinsics_binding;
        let criteria = &baseline.criteria;
        let pose_capacity = pose_bin_capacity_for_criteria(criteria).ok_or_else(|| {
            CalibrationSessionError::InvalidAutoCaptureBaseline(
                "PnP pose-bin capacity overflows usize".to_owned(),
            )
        })?;
        let mut field_counts = vec![0_usize; criteria.field_columns * criteria.field_rows];
        let mut depth_bin_counts = vec![0_usize; criteria.pnp_depth_bins];
        let mut pose_bin_counts = vec![0_usize; pose_capacity];

        let mut item_coverage = Vec::new();
        for item in self.items.iter().filter(|item| {
            item.enabled && item.acquisition_key.as_ref() == Some(&baseline.acquisition_key)
        }) {
            let CalibrationItemStatus::Found(detection) = &item.status else {
                continue;
            };
            // 历史尺寸不匹配项不能污染当前 source-bound 统计。
            if detection.image_size != binding.reference_image_size {
                continue;
            }
            // 既有项也必须满足当前几何门限；阈值调整后不能保留过期覆盖贡献。
            if Self::ensure_detection_inside_image(detection).is_err()
                || Self::ensure_min_adjacent_spacing(
                    detection,
                    self.board,
                    criteria.min_adjacent_spacing_px,
                )
                .is_err()
            {
                continue;
            }
            // PnP 是 active admission 的硬门限；缺失、过期或损坏证据均不能贡献任何指标。
            let Some(observation) = item.admission_pnp_observation.as_ref() else {
                continue;
            };
            let Ok((depth_corner_counts, pose_bin)) =
                Self::pnp_coverage_for_admission(observation, baseline, binding)
            else {
                continue;
            };
            let field_corner_counts = Self::field_corner_counts(detection, criteria);
            for (cell, count) in &field_corner_counts {
                field_counts[*cell] = field_counts[*cell].saturating_add(*count);
            }
            for (depth_bin, count) in &depth_corner_counts {
                depth_bin_counts[*depth_bin] = depth_bin_counts[*depth_bin].saturating_add(*count);
            }
            pose_bin_counts[pose_bin] = pose_bin_counts[pose_bin].saturating_add(1);
            let depth_covered = !depth_corner_counts.is_empty();
            item_coverage.push(DatasetAdmissionItemCoverage {
                item_id: item.id,
                field_corner_counts,
                depth_corner_counts,
                pose_bin: Some(pose_bin),
                pnp_state: AutoAdmissionPnpState::Valid,
                depth_covered,
                pose_covered: true,
            });
        }

        let item_contributions = target_capped_item_contributions(
            &item_coverage,
            criteria,
            pose_capacity,
            baseline.board,
        )?;
        let existing_field_gain = sum_gain(
            item_contributions
                .iter()
                .map(|contribution| contribution.field_gain),
        );
        let existing_depth_gain = sum_gain(
            item_contributions
                .iter()
                .map(|contribution| contribution.depth_gain),
        );
        let existing_pose_gain = sum_gain(
            item_contributions
                .iter()
                .map(|contribution| contribution.pose_gain),
        );
        let corner_count = baseline.board.corner_count()? as f64;

        let (field_gain, depth_gain, pose_gain) = if let Some((candidate, observation)) = candidate
        {
            candidate.validate(self.board)?;
            if candidate.image_size != binding.reference_image_size {
                return Err(CalibrationSessionError::RejectIncompatibleImageSize {
                    expected: binding.reference_image_size,
                    actual: candidate.image_size,
                });
            }
            Self::ensure_detection_inside_image(candidate)?;
            Self::ensure_min_adjacent_spacing(
                candidate,
                self.board,
                criteria.min_adjacent_spacing_px,
            )?;
            let (candidate_depth, pose_bin) =
                Self::pnp_coverage_for_admission(observation, baseline, binding)?;
            let candidate_field = Self::field_corner_counts(candidate, criteria);
            let raw_field_gain = target_capped_region_gain(
                &mut field_counts,
                criteria.field_target_per_cell,
                candidate_field.iter().copied(),
            );
            let raw_depth_gain = target_capped_region_gain(
                &mut depth_bin_counts,
                criteria.depth_target_per_bin,
                candidate_depth.iter().copied(),
            );
            let raw_pose_gain = target_capped_region_gain(
                &mut pose_bin_counts,
                criteria.pose_target_per_bin,
                std::iter::once((pose_bin, 1)),
            );
            let field_gain = corner_gain(raw_field_gain, corner_count);
            let depth_gain = corner_gain(raw_depth_gain, corner_count);
            let pose_gain = pose_gain(raw_pose_gain);
            (field_gain, depth_gain, pose_gain)
        } else {
            (existing_field_gain, existing_depth_gain, existing_pose_gain)
        };

        let field_cells = field_counts.iter().filter(|count| **count != 0).count();
        let depth_bins = depth_bin_counts.iter().filter(|count| **count != 0).count();
        let pose_bins = pose_bin_counts.iter().filter(|count| **count != 0).count();
        let field_quota_filled = capped_region_score(&field_counts, criteria.field_target_per_cell);
        let required_field_quota =
            capped_region_target(field_counts.len(), criteria.field_target_per_cell);
        let depth_quota_filled =
            capped_region_score(&depth_bin_counts, criteria.depth_target_per_bin);
        let required_depth_quota =
            capped_region_target(depth_bin_counts.len(), criteria.depth_target_per_bin);
        let pose_quota_filled = capped_region_score(&pose_bin_counts, criteria.pose_target_per_bin);
        let required_pose_quota =
            capped_region_target(pose_bin_counts.len(), criteria.pose_target_per_bin);
        let field_target_met = field_quota_filled >= required_field_quota;
        let depth_target_met = depth_quota_filled >= required_depth_quota;
        let pose_target_met = pose_quota_filled >= required_pose_quota;
        Ok(AutoAdmissionAssessment {
            active_criteria: Some(criteria.clone()),
            field_columns: criteria.field_columns,
            field_rows: criteria.field_rows,
            field_counts,
            field_cells,
            required_field_cells: criteria.field_columns * criteria.field_rows,
            field_quota_filled,
            required_field_quota,
            depth_bin_counts,
            depth_bins,
            required_depth_bins: criteria.pnp_depth_bins,
            depth_quota_filled,
            required_depth_quota,
            pose_bin_counts,
            pose_bins,
            required_pose_bins: pose_capacity,
            pose_quota_filled,
            required_pose_quota,
            item_contributions,
            depth_ranges: Vec::new(),
            item_visualizations: Vec::new(),
            field_gain,
            depth_gain,
            pose_gain,
            constraint_gain: constraint_gain(field_gain, depth_gain, pose_gain),
            field_target_met,
            depth_target_met,
            pose_target_met,
            collection_target_met: field_target_met && depth_target_met && pose_target_met,
        })
    }

    fn pnp_state_from_coverage_error(error: CalibrationSessionError) -> AutoAdmissionPnpState {
        match error {
            CalibrationSessionError::RejectAutoAdmissionBindingMismatch(reason) => {
                AutoAdmissionPnpState::BindingGap(reason)
            }
            CalibrationSessionError::RejectInvalidPnP(reason) => {
                let lower = reason.to_ascii_lowercase();
                if lower.contains("rmse") {
                    AutoAdmissionPnpState::RmseReprojectionGap(reason)
                } else if lower.contains("maximum reprojection") || lower.contains("max") {
                    AutoAdmissionPnpState::MaxReprojectionGap(reason)
                } else if lower.contains("depth") {
                    AutoAdmissionPnpState::DepthGap(reason)
                } else if lower.contains("tilt")
                    || lower.contains("azimuth")
                    || lower.contains("angle")
                {
                    AutoAdmissionPnpState::PoseGap(reason)
                } else {
                    AutoAdmissionPnpState::Invalid(reason)
                }
            }
            other => AutoAdmissionPnpState::Invalid(other.to_string()),
        }
    }
    fn pnp_coverage_for_admission(
        observation: &PnPObservation,
        baseline: &AutoCaptureBaseline,
        binding: &InitialIntrinsicsBinding,
    ) -> Result<(Vec<(usize, usize)>, usize), CalibrationSessionError> {
        Self::pnp_coverage_for_criteria(observation, &baseline.criteria, binding, baseline.board)
    }

    fn pnp_coverage_for_dataset(
        observation: &PnPObservation,
        criteria: &AutoCaptureAcceptanceCriteria,
        binding: &InitialIntrinsicsBinding,
        board: BoardSpec,
    ) -> Result<(Vec<(usize, usize)>, usize), CalibrationSessionError> {
        Self::pnp_coverage_for_criteria(observation, criteria, binding, board)
    }

    fn pnp_depth_range_for_dataset(
        observation: &PnPObservation,
        binding: &InitialIntrinsicsBinding,
        board: BoardSpec,
    ) -> Result<(f64, f64), CalibrationSessionError> {
        if observation.binding_digest != binding.digest {
            return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                "PnP evidence InitialIntrinsicsBinding digest does not match the active K/D binding"
                    .to_owned(),
            ));
        }
        let geometry = observation.geometry(board)?;
        Ok((geometry.minimum_board_depth, geometry.maximum_board_depth))
    }

    fn pnp_coverage_for_criteria(
        observation: &PnPObservation,
        criteria: &AutoCaptureAcceptanceCriteria,
        binding: &InitialIntrinsicsBinding,
        board: BoardSpec,
    ) -> Result<(Vec<(usize, usize)>, usize), CalibrationSessionError> {
        if observation.binding_digest != binding.digest {
            return Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(
                "PnP evidence InitialIntrinsicsBinding digest does not match the active K/D binding"
                    .to_owned(),
            ));
        }
        let geometry = observation.geometry(board)?;
        if observation.reprojection_rmse > criteria.pnp_max_rmse_px {
            return Err(CalibrationSessionError::RejectInvalidPnP(format!(
                "PnP reprojection RMSE {:.6} px exceeds {:.6} px",
                observation.reprojection_rmse, criteria.pnp_max_rmse_px
            )));
        }
        if observation.max_reprojection_error > criteria.pnp_max_error_px {
            return Err(CalibrationSessionError::RejectInvalidPnP(format!(
                "PnP maximum reprojection error {:.6} px exceeds {:.6} px",
                observation.max_reprojection_error, criteria.pnp_max_error_px
            )));
        }
        let depth_corner_counts = depth_corner_counts_for_criteria(observation, criteria, board)?;
        Ok((
            depth_corner_counts,
            pose_bin_for_criteria(criteria, geometry.tilt_degrees, geometry.azimuth_degrees)?,
        ))
    }

    fn invalidate_solution(&mut self) {
        self.solution_revision = self.solution_revision.wrapping_add(1);
        self.installed = None;
    }

    fn invalidate_detection_epoch(&mut self) {
        self.detection_epoch = self.detection_epoch.wrapping_add(1);
        self.active_detection_jobs.clear();
        for item in &mut self.items {
            if item.status.is_busy() {
                item.status = CalibrationItemStatus::Pending;
            }
        }
        self.invalidate_solution();
    }
    fn ensure_detection_inside_image(
        detection: &ChessboardDetection,
    ) -> Result<(), CalibrationSessionError> {
        let width = detection.image_size.width as f32;
        let height = detection.image_size.height as f32;
        if detection.corners.iter().all(|point| {
            point.x.is_finite()
                && point.y.is_finite()
                && point.x >= 0.0
                && point.y >= 0.0
                && point.x < width
                && point.y < height
        }) {
            Ok(())
        } else {
            Err(CalibrationSessionError::RejectInvalidCandidateGeometry(
                "checkerboard corners must be finite and inside the image".to_owned(),
            ))
        }
    }

    fn ensure_min_adjacent_spacing(
        detection: &ChessboardDetection,
        board: BoardSpec,
        min_spacing_px: f32,
    ) -> Result<(), CalibrationSessionError> {
        let cols = usize::from(board.inner_cols);
        let rows = usize::from(board.inner_rows);
        let mut minimum = f32::INFINITY;
        for row in 0..rows {
            for col in 0..cols {
                let index = row * cols + col;
                if col + 1 < cols {
                    let next = row * cols + col + 1;
                    minimum = minimum.min(Self::point_distance(
                        detection.corners[index],
                        detection.corners[next],
                    ));
                }
                if row + 1 < rows {
                    let next = (row + 1) * cols + col;
                    minimum = minimum.min(Self::point_distance(
                        detection.corners[index],
                        detection.corners[next],
                    ));
                }
            }
        }
        if minimum.is_finite() && minimum >= min_spacing_px {
            Ok(())
        } else {
            Err(CalibrationSessionError::RejectInvalidCandidateGeometry(
                format!(
                    "minimum adjacent corner spacing {minimum:.3} px is below {min_spacing_px:.3} px"
                ),
            ))
        }
    }

    fn point_distance(
        a: camera_toolbox_core::CalibrationPoint,
        b: camera_toolbox_core::CalibrationPoint,
    ) -> f32 {
        (a.x - b.x).hypot(a.y - b.y)
    }

    /// 将一帧已检测到的棋盘角点映射到 Field coverage 单元；只返回有角点的单元索引。
    #[must_use]
    pub fn detection_field_cells(
        detection: &ChessboardDetection,
        criteria: &AutoCaptureAcceptanceCriteria,
    ) -> Vec<usize> {
        Self::field_corner_counts(detection, criteria)
            .into_iter()
            .map(|(cell, _)| cell)
            .collect()
    }

    fn field_corner_counts(
        detection: &ChessboardDetection,
        criteria: &AutoCaptureAcceptanceCriteria,
    ) -> Vec<(usize, usize)> {
        let field_capacity = criteria.field_columns * criteria.field_rows;
        let mut counts = vec![0_usize; field_capacity];
        for point in &detection.corners {
            let normalized_x =
                ((point.x + 0.5) / detection.image_size.width as f32).clamp(0.0, 1.0);
            let normalized_y =
                ((point.y + 0.5) / detection.image_size.height as f32).clamp(0.0, 1.0);
            let column = ((normalized_x * criteria.field_columns as f32) as usize)
                .min(criteria.field_columns - 1);
            let row =
                ((normalized_y * criteria.field_rows as f32) as usize).min(criteria.field_rows - 1);
            counts[row * criteria.field_columns + column] =
                counts[row * criteria.field_columns + column].saturating_add(1);
        }
        counts
            .into_iter()
            .enumerate()
            .filter_map(|(cell, count)| (count != 0).then_some((cell, count)))
            .collect()
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum CalibrationSessionError {
    #[error("unknown calibration dataset item {0:?}")]
    UnknownItem(CalibrationItemId),
    #[error("calibration dataset item {0:?} is busy")]
    ItemBusy(CalibrationItemId),
    #[error("calibration source changed for item {0:?}")]
    SourceChanged(CalibrationItemId),
    #[error("calibration result is stale")]
    StaleResult,
    #[error("automatic candidate input already exists in the dataset")]
    AutoCandidateAlreadyPresent,
    #[error("automatic candidate binding is stale")]
    StaleAutoCandidate,
    #[error("automatic candidate PNG revision is invalid or mismatched")]
    InvalidAutoCandidateRevision,
    #[error("automatic candidate identity does not match its stream frame")]
    InvalidAutoCandidateIdentity,
    #[error("automatic admission requires an active AutoCaptureBaseline")]
    RejectMissingBaseline,
    #[error("automatic admission requires a source-bound InitialIntrinsicsBinding")]
    RejectMissingInitialIntrinsicsBinding,
    #[error("automatic admission binding mismatch: {0}")]
    RejectAutoAdmissionBindingMismatch(String),
    #[error("automatic candidate image size mismatch: expected {expected:?}, got {actual:?}")]
    RejectIncompatibleImageSize {
        expected: CalibrationImageSize,
        actual: CalibrationImageSize,
    },
    #[error("automatic candidate Gain {actual:.3} is below minimum {minimum:.3}")]
    RejectInsufficientConstraintGain { actual: f64, minimum: f64 },
    #[error("automatic candidate geometry is invalid: {0}")]
    RejectInvalidCandidateGeometry(String),
    #[error("automatic candidate PnP evidence is invalid: {0}")]
    RejectInvalidPnP(String),
    #[error("invalid AutoCaptureBaseline: {0}")]
    InvalidAutoCaptureBaseline(String),
    #[error("invalid InitialIntrinsicsBinding: {0}")]
    InvalidInitialIntrinsicsBinding(String),
    #[error("calibration needs at least {required} detected views, found {found}")]
    NotEnoughViews { found: usize, required: usize },
    #[error("calibration images must share one size: expected {expected:?}, got {actual:?}")]
    MixedImageSizes {
        expected: CalibrationImageSize,
        actual: CalibrationImageSize,
    },
    #[error(transparent)]
    InvalidData(#[from] CalibrationDataError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSourceId, SourcePath};
    use camera_toolbox_core::{CalibrationPoint, PANGBOT_CALIBRATION_FLAGS, ViewCalibrationResult};

    fn board() -> BoardSpec {
        BoardSpec::new(2, 2, 20.0).unwrap()
    }

    fn reference(name: &str) -> FileRef {
        FileRef::new(
            FileSourceId::new("local").unwrap(),
            SourcePath::new(name).unwrap(),
        )
    }

    fn version(size: u64) -> FileVersion {
        FileVersion {
            size,
            modified_millis: Some(size),
        }
    }

    fn detection() -> ChessboardDetectionOutcome {
        ChessboardDetectionOutcome::Found(ChessboardDetection {
            image_size: CalibrationImageSize::new(640, 480).unwrap(),
            corners: vec![
                CalibrationPoint::new(10.0, 10.0),
                CalibrationPoint::new(20.0, 10.0),
                CalibrationPoint::new(10.0, 20.0),
                CalibrationPoint::new(20.0, 20.0),
            ],
        })
    }

    fn intrinsics() -> InitialIntrinsics {
        InitialIntrinsics {
            camera_matrix: [500.0, 0.0, 320.0, 0.0, 500.0, 240.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 12],
        }
    }

    fn test_acquisition_key(channel: u16) -> AutoCaptureAcquisitionKey {
        AutoCaptureAcquisitionKey::new("auto-capture-test", channel, "test-geometry").unwrap()
    }

    fn test_criteria() -> AutoCaptureAcceptanceCriteria {
        AutoCaptureAcceptanceCriteria {
            field_columns: 2,
            field_rows: 2,
            field_target_per_cell: 1,
            min_adjacent_spacing_px: 1.0,
            pnp_depth_min: 0.1,
            pnp_depth_max: 2_000.0,
            pnp_depth_bins: 4,
            depth_target_per_bin: 1,
            pnp_tilt_deadband_deg: 5.0,
            pnp_tilt_max_deg: 65.0,
            pnp_tilt_bins: 3,
            pnp_azimuth_sectors: 8,
            pose_target_per_bin: 1,
            pnp_max_rmse_px: 1.5,
            pnp_max_error_px: 4.0,
            minimum_auto_gain: 0.3,
        }
    }

    fn test_baseline() -> AutoCaptureBaseline {
        AutoCaptureBaseline::new(
            test_acquisition_key(0),
            CalibrationImageSize::new(640, 480).unwrap(),
            board(),
            test_criteria(),
        )
        .unwrap()
    }

    fn test_binding() -> InitialIntrinsicsBinding {
        InitialIntrinsicsBinding::full_frame(
            intrinsics(),
            CalibrationImageSize::new(640, 480).unwrap(),
            test_acquisition_key(0),
        )
        .unwrap()
    }

    fn test_pnp_observation() -> PnPObservation {
        PnPObservation::from_view_result(
            test_binding().digest,
            ViewCalibrationResult {
                rotation_vector: [0.0, 0.0, 0.0],
                translation_vector: [0.0, 0.0, 1_000.0],
                projected_points: Vec::new(),
                reprojection_rmse: 0.1,
                max_reprojection_error: 0.2,
            },
            board(),
        )
        .unwrap()
    }

    fn assert_gain_eq(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1.0e-12,
            "expected gain {expected}, got {actual}"
        );
    }

    fn configure_test_auto_admission(session: &mut CalibrationSession) {
        session
            .configure_auto_admission(Some(test_baseline()), Some(test_binding()))
            .unwrap();
    }

    fn solution(snapshot: &CalibrationSnapshot) -> CalibrationSolution {
        CalibrationSolution {
            image_size: snapshot.request.image_size,
            camera_matrix: snapshot.request.initial_intrinsics.camera_matrix,
            distortion_coefficients: vec![0.0; 12],
            rms_error: 0.1,
            calibration_flags: PANGBOT_CALIBRATION_FLAGS,
            views: snapshot
                .request
                .image_points
                .iter()
                .map(|points| ViewCalibrationResult {
                    rotation_vector: [0.0; 3],
                    translation_vector: [0.0, 0.0, 1.0],
                    projected_points: points.clone(),
                    reprojection_rmse: 0.1,
                    max_reprojection_error: 0.2,
                })
                .collect(),
        }
    }

    fn add_found(session: &mut CalibrationSession, name: &str, size: u64) -> CalibrationItemId {
        let AddCalibrationItemOutcome::Added(id) =
            session.add_or_refresh(reference(name), version(size), name.to_owned())
        else {
            panic!("expected added item");
        };
        let token = session.begin_detection(id).unwrap();
        session.mark_reading(&token).unwrap();

        session.mark_detect_queued(&token).unwrap();
        session.mark_detecting(&token).unwrap();
        session
            .install_detection(&token, version(size), detection())
            .unwrap();
        id
    }

    fn add_found_with_acquisition_key(
        session: &mut CalibrationSession,
        name: &str,
        size: u64,
        acquisition_key: AutoCaptureAcquisitionKey,
    ) -> CalibrationItemId {
        let AddCalibrationItemOutcome::Added(id) = session.add_or_refresh_with_acquisition_key(
            reference(name),
            version(size),
            name.to_owned(),
            Some(acquisition_key),
        ) else {
            panic!("expected added item");
        };
        let token = session.begin_detection(id).unwrap();
        session.mark_reading(&token).unwrap();
        session.mark_detect_queued(&token).unwrap();
        session.mark_detecting(&token).unwrap();
        session
            .install_detection(&token, version(size), detection())
            .unwrap();
        id
    }

    fn add_stream_found_with_acquisition_key(
        session: &mut CalibrationSession,
        frame_sequence: u64,
        acquisition_key: AutoCaptureAcquisitionKey,
    ) -> CalibrationItemId {
        let input = CalibrationInputKey::StreamCapture(StreamCaptureId {
            stream_id: StreamSessionId::new("manual-stream").unwrap(),
            channel: acquisition_key.channel,
            frame_sequence,
        });
        let revision = CalibrationInputRevision::EphemeralPng {
            content_sha256: format!("{frame_sequence:064x}"),
            encoded_bytes: 128,
        };
        let AddCalibrationItemOutcome::Added(id) = session.add_or_refresh_with_acquisition_key(
            input,
            revision.clone(),
            format!("RTSP ch{} #{frame_sequence}", acquisition_key.channel),
            Some(acquisition_key),
        ) else {
            panic!("expected added stream item");
        };
        let token = session.begin_encoded_detection(id).unwrap();
        session.mark_detecting(&token).unwrap();
        session
            .install_detection(&token, revision, detection())
            .unwrap();
        id
    }

    #[test]
    fn encoded_input_queues_directly_without_a_fake_read_stage() {
        let mut session = CalibrationSession::new(board());
        let input = CalibrationInputKey::StreamCapture(StreamCaptureId {
            stream_id: StreamSessionId::new("stream-test").unwrap(),
            channel: 0,
            frame_sequence: 7,
        });
        let revision = CalibrationInputRevision::EphemeralPng {
            content_sha256: "a".repeat(64),
            encoded_bytes: 128,
        };
        let AddCalibrationItemOutcome::Added(id) =
            session.add_or_refresh(input, revision, "RTSP ch0 #7".to_owned())
        else {
            panic!();
        };

        let token = session.begin_encoded_detection(id).unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::DetectQueued
        ));
        session.mark_detecting(&token).unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::Detecting
        ));
    }

    #[test]
    fn detection_status_distinguishes_read_and_detect_queues() {
        let mut session = CalibrationSession::new(board());
        let AddCalibrationItemOutcome::Added(id) =
            session.add_or_refresh(reference("a.png"), version(10), "a.png".into())
        else {
            panic!();
        };
        let token = session.begin_detection(id).unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::ReadQueued
        ));
        assert_eq!(
            session.mark_detect_queued(&token),
            Err(CalibrationSessionError::StaleResult)
        );
        session.mark_reading(&token).unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::Reading
        ));
        session.mark_detect_queued(&token).unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::DetectQueued
        ));
        session.mark_detecting(&token).unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::Detecting
        ));
    }

    #[test]
    fn png_preflight_reads_ihdr_dimensions() {
        let mut bytes = vec![0_u8; 24];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes[8..12].copy_from_slice(&13_u32.to_be_bytes());
        bytes[12..16].copy_from_slice(b"IHDR");
        bytes[16..20].copy_from_slice(&640_u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&480_u32.to_be_bytes());
        assert_eq!(
            parse_png_dimensions(&bytes).unwrap(),
            CalibrationImageSize::new(640, 480).unwrap()
        );

        bytes[0] = 0;
        assert_eq!(
            parse_png_dimensions(&bytes),
            Err(CalibrationInputError::InvalidPngHeader)
        );
    }

    #[test]
    fn duplicate_refresh_invalidates_detection_and_solution_generation() {
        let mut session = CalibrationSession::new(board());
        let id = add_found(&mut session, "a.png", 10);
        assert_eq!(
            session.add_or_refresh(reference("a.png"), version(10), "a.png".into()),
            AddCalibrationItemOutcome::AlreadyPresent(id)
        );
        assert_eq!(
            session.add_or_refresh(reference("a.png"), version(11), "a.png".into()),
            AddCalibrationItemOutcome::SourceChanged(id)
        );
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::Pending
        ));
    }

    #[test]
    fn board_corner_layout_change_marks_all_items_pending() {
        let mut session = CalibrationSession::new(board());
        add_found(&mut session, "a.png", 10);
        session
            .set_board(BoardSpec::new(3, 2, 10.0).unwrap())
            .unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::Pending
        ));
    }

    #[test]
    fn square_size_change_preserves_detections_and_invalidates_solution() {
        let mut session = CalibrationSession::new(board());
        add_found(&mut session, "a.png", 10);
        add_found(&mut session, "b.png", 20);
        add_found(&mut session, "c.png", 30);
        let snapshot = session.calibration_snapshot(intrinsics()).unwrap();
        let solution = solution(&snapshot);
        session.install_solution(snapshot, solution).unwrap();

        session
            .set_board(BoardSpec::new(2, 2, 40.0).unwrap())
            .unwrap();

        assert!(
            session
                .items()
                .iter()
                .all(|item| matches!(item.status, CalibrationItemStatus::Found(_)))
        );
        assert!(session.installed().is_none());
        assert_eq!(session.board().square_size, 40.0);
        assert_eq!(
            session
                .calibration_snapshot(intrinsics())
                .unwrap()
                .request
                .board
                .square_size,
            40.0
        );
    }

    #[test]
    fn reset_detections_marks_every_item_pending() {
        let mut session = CalibrationSession::new(board());
        add_found(&mut session, "a.png", 10);
        let disabled = add_found(&mut session, "b.png", 20);
        session.set_enabled(disabled, false).unwrap();

        session.reset_detections();

        assert!(
            session
                .items()
                .iter()
                .all(|item| matches!(item.status, CalibrationItemStatus::Pending))
        );
        assert!(!session.items()[1].enabled);
        assert!(matches!(
            session.calibration_snapshot(intrinsics()),
            Err(CalibrationSessionError::NotEnoughViews { found: 0, .. })
        ));
    }

    #[test]
    fn stale_detection_result_is_rejected_after_dataset_change() {
        let mut session = CalibrationSession::new(board());
        let AddCalibrationItemOutcome::Added(id) =
            session.add_or_refresh(reference("a.png"), version(10), "a.png".into())
        else {
            panic!();
        };
        let token = session.begin_detection(id).unwrap();
        session.add_or_refresh(reference("b.png"), version(20), "b.png".into());
        assert_eq!(
            session.install_detection(&token, version(10), detection()),
            Err(CalibrationSessionError::StaleResult)
        );
    }

    #[test]
    fn concurrent_detection_tokens_install_in_reverse_order() {
        let mut session = CalibrationSession::new(board());
        let AddCalibrationItemOutcome::Added(first) =
            session.add_or_refresh(reference("a.png"), version(10), "a.png".into())
        else {
            panic!();
        };
        let AddCalibrationItemOutcome::Added(second) =
            session.add_or_refresh(reference("b.png"), version(20), "b.png".into())
        else {
            panic!();
        };
        let first_token = session.begin_detection(first).unwrap();
        let second_token = session.begin_detection(second).unwrap();
        session.mark_reading(&first_token).unwrap();
        session.mark_reading(&second_token).unwrap();
        session.mark_detect_queued(&first_token).unwrap();
        session.mark_detect_queued(&second_token).unwrap();
        session.mark_detecting(&first_token).unwrap();
        session.mark_detecting(&second_token).unwrap();

        session
            .install_detection(&second_token, version(20), detection())
            .unwrap();
        session
            .install_detection(&first_token, version(10), detection())
            .unwrap();

        assert!(
            session
                .items()
                .iter()
                .all(|item| matches!(item.status, CalibrationItemStatus::Found(_)))
        );
    }

    #[test]
    fn restarted_detection_rejects_result_from_cancelled_job() {
        let mut session = CalibrationSession::new(board());
        let AddCalibrationItemOutcome::Added(id) =
            session.add_or_refresh(reference("a.png"), version(10), "a.png".into())
        else {
            panic!();
        };
        let cancelled_token = session.begin_detection(id).unwrap();
        session.cancel_detection(&cancelled_token).unwrap();
        let active_token = session.begin_detection(id).unwrap();
        session.mark_reading(&active_token).unwrap();
        session.mark_detect_queued(&active_token).unwrap();
        session.mark_detecting(&active_token).unwrap();

        assert_eq!(
            session.install_detection(&cancelled_token, version(10), detection()),
            Err(CalibrationSessionError::StaleResult)
        );
        session
            .install_detection(&active_token, version(10), detection())
            .unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::Found(_)
        ));
    }

    #[test]
    fn snapshot_requires_three_same_size_found_views() {
        let mut session = CalibrationSession::new(board());
        add_found(&mut session, "a.png", 10);
        add_found(&mut session, "b.png", 20);
        assert!(matches!(
            session.calibration_snapshot(intrinsics()),
            Err(CalibrationSessionError::NotEnoughViews { found: 2, .. })
        ));
        add_found(&mut session, "c.png", 30);
        let snapshot = session.calibration_snapshot(intrinsics()).unwrap();
        assert_eq!(snapshot.item_ids.len(), 3);
    }

    #[test]
    fn solution_install_is_transactional() {
        let mut session = CalibrationSession::new(board());
        add_found(&mut session, "a.png", 10);
        add_found(&mut session, "b.png", 20);
        add_found(&mut session, "c.png", 30);
        let snapshot = session.calibration_snapshot(intrinsics()).unwrap();
        let solution = solution(&snapshot);
        session.install_solution(snapshot, solution).unwrap();
        assert!(session.installed().is_some());
        session.set_enabled(session.items()[0].id, false).unwrap();
        assert!(session.installed().is_none());
    }

    fn auto_candidate_fixture_with_source(
        session: &CalibrationSession,
        candidate_id: u64,
        sequence: u64,
        stream_session: &str,
        source_acquisition_key: Option<AutoCaptureAcquisitionKey>,
    ) -> (AutoCandidateToken, CalibrationInputRevision) {
        let channel = source_acquisition_key.as_ref().map_or(0, |key| key.channel);
        let identity = StreamFrameIdentity::unavailable(
            StreamSessionId::new(stream_session).unwrap(),
            channel,
            sequence,
            "test fixture",
        );
        let revision = CalibrationInputRevision::EphemeralPng {
            content_sha256: format!("{sequence:064x}"),
            encoded_bytes: 128,
        };
        let token = session
            .bind_auto_candidate(
                AutoCandidateId::new(candidate_id),
                identity,
                revision.clone(),
                format!("candidate-{sequence}"),
                source_acquisition_key,
            )
            .unwrap();
        (token, revision)
    }

    fn auto_candidate_fixture_with_channel(
        session: &CalibrationSession,
        candidate_id: u64,
        sequence: u64,
        channel: u16,
    ) -> (AutoCandidateToken, CalibrationInputRevision) {
        auto_candidate_fixture_with_source(
            session,
            candidate_id,
            sequence,
            "stream-new-session",
            Some(test_acquisition_key(channel)),
        )
    }

    fn auto_candidate_fixture(
        session: &CalibrationSession,
        candidate_id: u64,
        sequence: u64,
    ) -> (AutoCandidateToken, CalibrationInputRevision) {
        auto_candidate_fixture_with_source(
            session,
            candidate_id,
            sequence,
            "stream-new-session",
            Some(test_acquisition_key(0)),
        )
    }

    #[test]
    fn stale_auto_admission_revision_cannot_mutate_dataset() {
        let mut session = CalibrationSession::new(board());
        let (token, revision) = auto_candidate_fixture(&session, 1, 7);
        session.invalidate_auto_admission();
        let ChessboardDetectionOutcome::Found(found) = detection() else {
            panic!("fixture must contain a found detection");
        };

        assert_eq!(
            session.commit_auto_candidate(AutoCandidateCommit::new(
                token,
                revision,
                found,
                test_pnp_observation(),
            )),
            Err(CalibrationSessionError::StaleAutoCandidate)
        );
        assert!(session.items().is_empty());
        assert_eq!(session.selected(), None);
    }

    #[test]
    fn auto_candidate_commit_requires_matching_baseline_and_binding() {
        let mut session = CalibrationSession::new(board());
        let (token, revision) = auto_candidate_fixture(&session, 1, 9);
        let ChessboardDetectionOutcome::Found(found) = detection() else {
            panic!("fixture must contain a found detection");
        };

        assert_eq!(
            session.commit_auto_candidate(AutoCandidateCommit::new(
                token,
                revision,
                found,
                test_pnp_observation(),
            )),
            Err(CalibrationSessionError::RejectMissingBaseline)
        );
        assert!(session.items().is_empty());
    }

    #[test]
    fn auto_candidate_commit_rejects_wrong_stream_channel_without_mutating_dataset() {
        let mut session = CalibrationSession::new(board());
        configure_test_auto_admission(&mut session);
        let (token, revision) = auto_candidate_fixture_with_channel(&session, 1, 10, 3);
        let ChessboardDetectionOutcome::Found(found) = detection() else {
            panic!("fixture must contain a found detection");
        };

        let result = session.commit_auto_candidate(AutoCandidateCommit::new(
            token,
            revision,
            found,
            test_pnp_observation(),
        ));
        assert!(
            matches!(
                &result,
                Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(message))
                    if message.contains("candidate source")
            ),
            "unexpected result: {result:?}"
        );
        assert!(session.items().is_empty());
        assert_eq!(session.selected(), None);
    }

    #[test]
    fn auto_candidate_bind_rejects_source_key_channel_mismatching_frame_identity() {
        let session = CalibrationSession::new(board());
        let identity = StreamFrameIdentity::unavailable(
            StreamSessionId::new("stream-channel-mismatch").unwrap(),
            3,
            13,
            "test fixture",
        );
        let revision = CalibrationInputRevision::EphemeralPng {
            content_sha256: "d".repeat(64),
            encoded_bytes: 128,
        };

        let result = session.bind_auto_candidate(
            AutoCandidateId::new(4),
            identity,
            revision,
            "candidate-13".to_owned(),
            Some(test_acquisition_key(0)),
        );
        assert!(
            matches!(
                &result,
                Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(message))
                    if message.contains("source channel")
            ),
            "unexpected result: {result:?}"
        );
        assert!(session.items().is_empty());
    }

    #[test]
    fn auto_candidate_commit_accepts_same_source_new_stream_session() {
        let mut session = CalibrationSession::new(board());
        configure_test_auto_admission(&mut session);
        let (token, revision) = auto_candidate_fixture_with_source(
            &session,
            2,
            11,
            "stream-2",
            Some(test_acquisition_key(0)),
        );
        let ChessboardDetectionOutcome::Found(found) = detection() else {
            panic!("fixture must contain a found detection");
        };

        let item_id = session
            .commit_auto_candidate(AutoCandidateCommit::new(
                token,
                revision,
                found,
                test_pnp_observation(),
            ))
            .unwrap();
        assert_eq!(session.items().len(), 1);
        assert_eq!(session.items()[0].id, item_id);
        assert_eq!(
            session.items()[0].acquisition_key,
            Some(test_acquisition_key(0))
        );
    }

    #[test]
    fn auto_admission_assessment_ignores_other_source_dataset_items() {
        let mut session = CalibrationSession::new(board());
        add_found_with_acquisition_key(
            &mut session,
            "other-source.png",
            10,
            AutoCaptureAcquisitionKey::new("other-source", 0, "test-geometry").unwrap(),
        );
        configure_test_auto_admission(&mut session);

        let assessment = session.assess_auto_admission(None).unwrap();
        assert_eq!(assessment.field_cells, 0);
        assert!(assessment.item_contributions.is_empty());
    }

    #[test]
    fn dataset_acceptance_aggregates_file_manual_stream_and_auto_items() {
        let mut session = CalibrationSession::new(board());
        let local_file = add_found(&mut session, "local.png", 10);
        let manual_stream = add_stream_found_with_acquisition_key(
            &mut session,
            7,
            AutoCaptureAcquisitionKey::new("manual-rtsp", 0, "test-geometry").unwrap(),
        );
        let auto_item =
            add_found_with_acquisition_key(&mut session, "auto.png", 11, test_acquisition_key(0));
        for id in [local_file, manual_stream, auto_item] {
            session.item_mut(id).unwrap().pnp_observation = Some(test_pnp_observation());
        }
        session
            .item_mut(auto_item)
            .unwrap()
            .admission_pnp_observation = Some(test_pnp_observation());
        configure_test_auto_admission(&mut session);
        let criteria = test_criteria();
        let binding = test_binding();

        let dataset = session
            .assess_dataset_acceptance(binding.reference_image_size, &criteria, Some(&binding))
            .unwrap();
        assert_eq!(dataset.item_contributions.len(), 3);
        assert_eq!(dataset.depth_bins, 1);
        assert_eq!(dataset.pose_bins, 1);
        assert!(session.items().iter().any(|item| item.id == manual_stream
            && matches!(item.input, CalibrationInputKey::StreamCapture(_))));

        let automatic = session.assess_auto_admission(None).unwrap();
        assert_eq!(automatic.item_contributions.len(), 1);
        assert_eq!(automatic.item_contributions[0].item_id, auto_item);
    }

    #[test]
    fn dataset_acceptance_counts_file_found_and_field_before_pnp() {
        let mut session = CalibrationSession::new(board());
        let local_file = add_found(&mut session, "local.png", 10);
        configure_test_auto_admission(&mut session);
        let criteria = test_criteria();
        let binding = test_binding();

        let assessment = session
            .assess_dataset_acceptance(binding.reference_image_size, &criteria, Some(&binding))
            .unwrap();
        assert_eq!(assessment.field_cells, 1);
        assert_eq!(assessment.depth_bins, 0);
        assert_eq!(assessment.pose_bins, 0);
        let [contribution] = assessment.item_contributions.as_slice() else {
            panic!("expected field-only contribution");
        };
        assert_eq!(contribution.item_id, local_file);
        assert_gain_eq(contribution.depth_gain, 0.0);
        assert_gain_eq(contribution.pose_gain, 0.0);
        assert_eq!(
            contribution.pnp_state,
            AutoAdmissionPnpState::MissingObservation
        );
        assert_gain_eq(contribution.field_gain, 0.25);
        assert_gain_eq(contribution.constraint_gain, 0.25 / 3.0);
    }

    #[test]
    fn dataset_acceptance_reports_gate_gaps_separately_from_zero_gain() {
        let binding = test_binding();

        let mut rmse_session = CalibrationSession::new(board());
        let rmse_item = add_found(&mut rmse_session, "rmse-gap.png", 10);
        let mut rmse_observation = test_pnp_observation();
        rmse_observation.reprojection_rmse = test_criteria().pnp_max_rmse_px + 0.1;
        rmse_session.item_mut(rmse_item).unwrap().pnp_observation = Some(rmse_observation);
        let rmse_assessment = rmse_session
            .assess_dataset_acceptance(
                binding.reference_image_size,
                &test_criteria(),
                Some(&binding),
            )
            .unwrap();
        assert!(matches!(
            rmse_assessment.item_contributions[0].pnp_state,
            AutoAdmissionPnpState::RmseReprojectionGap(_)
        ));

        let mut max_error_session = CalibrationSession::new(board());
        let max_error_item = add_found(&mut max_error_session, "max-error-gap.png", 10);
        let mut max_error_observation = test_pnp_observation();
        max_error_observation.max_reprojection_error = test_criteria().pnp_max_error_px + 0.1;
        max_error_session
            .item_mut(max_error_item)
            .unwrap()
            .pnp_observation = Some(max_error_observation);
        let max_error_assessment = max_error_session
            .assess_dataset_acceptance(
                binding.reference_image_size,
                &test_criteria(),
                Some(&binding),
            )
            .unwrap();
        assert!(matches!(
            max_error_assessment.item_contributions[0].pnp_state,
            AutoAdmissionPnpState::MaxReprojectionGap(_)
        ));

        let mut depth_criteria = test_criteria();
        depth_criteria.pnp_depth_min = 1_500.0;
        depth_criteria.pnp_depth_max = 2_000.0;
        depth_criteria.validate().unwrap();
        let mut depth_session = CalibrationSession::new(board());
        let depth_item = add_found(&mut depth_session, "depth-gap.png", 10);
        depth_session.item_mut(depth_item).unwrap().pnp_observation = Some(test_pnp_observation());
        let depth_assessment = depth_session
            .assess_dataset_acceptance(
                binding.reference_image_size,
                &depth_criteria,
                Some(&binding),
            )
            .unwrap();
        let depth_contribution = &depth_assessment.item_contributions[0];
        assert_eq!(depth_contribution.pnp_state, AutoAdmissionPnpState::Valid);
        assert!(!depth_contribution.depth_covered);
        assert!(depth_contribution.pose_covered);
        assert_gain_eq(depth_contribution.depth_gain, 0.0);

        let mut redundant_session = CalibrationSession::new(board());
        let criteria = test_criteria();
        for index in 0..4 {
            let item = add_found(
                &mut redundant_session,
                &format!("redundant-{index}.png"),
                10 + index as u64,
            );
            redundant_session.item_mut(item).unwrap().pnp_observation =
                Some(test_pnp_observation());
        }
        let redundant_assessment = redundant_session
            .assess_dataset_acceptance(binding.reference_image_size, &criteria, Some(&binding))
            .unwrap();
        let total_gain = redundant_assessment
            .item_contributions
            .iter()
            .map(|contribution| contribution.constraint_gain)
            .sum::<f64>();
        assert_gain_eq(redundant_assessment.constraint_gain, total_gain);
        assert_gain_eq(
            redundant_assessment.constraint_gain,
            constraint_gain(
                redundant_assessment.field_gain,
                redundant_assessment.depth_gain,
                redundant_assessment.pose_gain,
            ),
        );
        assert_eq!(
            redundant_assessment
                .item_contributions
                .iter()
                .filter(|contribution| contribution.constraint_gain == 0.0)
                .count(),
            3
        );
        assert!(
            redundant_assessment
                .item_contributions
                .iter()
                .any(|contribution| {
                    contribution.pnp_state == AutoAdmissionPnpState::Valid
                        && contribution.depth_covered
                        && contribution.pose_covered
                        && contribution.constraint_gain > 0.0
                })
        );
    }

    #[test]
    fn field_coverage_counts_corners_per_region_not_views() {
        let mut session = CalibrationSession::new(board());
        let first = add_found(&mut session, "first.png", 10);
        let second = add_found(&mut session, "second.png", 11);
        let criteria = test_criteria();
        let binding = test_binding();

        let assessment = session
            .assess_dataset_acceptance(binding.reference_image_size, &criteria, Some(&binding))
            .unwrap();
        assert_eq!(assessment.field_cells, 1);
        assert_eq!(assessment.field_counts[0], 8);
        assert_eq!(assessment.field_counts.iter().sum::<usize>(), 8);
        assert_eq!(assessment.item_contributions.len(), 2);
        let [first_contribution, second_contribution] = assessment.item_contributions.as_slice()
        else {
            panic!("expected two compatible field items");
        };
        assert_eq!(first_contribution.item_id, first);
        assert_gain_eq(first_contribution.field_gain, 0.25);
        assert_eq!(second_contribution.item_id, second);
        assert_gain_eq(second_contribution.field_gain, 0.0);

        session.set_enabled(second, false).unwrap();
        let assessment = session
            .assess_dataset_acceptance(binding.reference_image_size, &criteria, Some(&binding))
            .unwrap();
        assert_eq!(assessment.field_counts[0], 4);
        assert_eq!(assessment.field_counts.iter().sum::<usize>(), 4);
        let [contribution] = assessment.item_contributions.as_slice() else {
            panic!("expected the only enabled compatible item");
        };
        assert_eq!(contribution.item_id, first);
        assert_gain_eq(contribution.field_gain, 0.25);
    }

    #[test]
    fn auto_admission_item_contributions_are_target_capped_and_recompute() {
        let mut session = CalibrationSession::new(board());
        let first =
            add_found_with_acquisition_key(&mut session, "first.png", 10, test_acquisition_key(0));
        session.item_mut(first).unwrap().admission_pnp_observation = Some(test_pnp_observation());
        let second =
            add_found_with_acquisition_key(&mut session, "second.png", 11, test_acquisition_key(0));
        session.item_mut(second).unwrap().admission_pnp_observation = Some(test_pnp_observation());
        configure_test_auto_admission(&mut session);

        let assessment = session.assess_auto_admission(None).unwrap();
        assert_eq!(assessment.item_contributions.len(), 2);
        let [first_contribution, second_contribution] = assessment.item_contributions.as_slice()
        else {
            panic!("expected two source-bound items");
        };
        assert_eq!(first_contribution.item_id, first);
        assert_gain_eq(first_contribution.field_gain, 0.25);
        assert_gain_eq(first_contribution.depth_gain, 0.25);
        assert_gain_eq(first_contribution.pose_gain, 1.0);
        assert_gain_eq(first_contribution.constraint_gain, 0.5);
        assert_eq!(second_contribution.item_id, second);
        assert_gain_eq(second_contribution.field_gain, 0.0);
        assert_gain_eq(second_contribution.depth_gain, 0.0);
        assert_gain_eq(second_contribution.pose_gain, 0.0);
        assert_gain_eq(second_contribution.constraint_gain, 0.0);
        assert_gain_eq(assessment.constraint_gain, 0.5);

        session.set_enabled(second, false).unwrap();
        let assessment = session.assess_auto_admission(None).unwrap();
        let [contribution] = assessment.item_contributions.as_slice() else {
            panic!("expected the only enabled compatible item");
        };
        assert_eq!(contribution.item_id, first);
        assert_gain_eq(contribution.field_gain, 0.25);
        assert_gain_eq(contribution.depth_gain, 0.25);
        assert_gain_eq(contribution.pose_gain, 1.0);
        assert_gain_eq(contribution.constraint_gain, 0.5);

        session.set_enabled(first, false).unwrap();
        assert!(
            session
                .assess_auto_admission(None)
                .unwrap()
                .item_contributions
                .is_empty()
        );
    }

    #[test]
    fn auto_admission_excludes_items_without_valid_pnp_evidence() {
        let mut session = CalibrationSession::new(board());
        add_found_with_acquisition_key(
            &mut session,
            "missing-pnp.png",
            10,
            test_acquisition_key(0),
        );
        let stale_binding = add_found_with_acquisition_key(
            &mut session,
            "stale-binding.png",
            11,
            test_acquisition_key(0),
        );
        let mut observation = test_pnp_observation();
        let mut other_intrinsics = intrinsics();
        other_intrinsics.camera_matrix[0] = 501.0;
        observation.binding_digest = InitialIntrinsicsBinding::full_frame(
            other_intrinsics,
            CalibrationImageSize::new(640, 480).unwrap(),
            test_acquisition_key(0),
        )
        .unwrap()
        .digest;
        session
            .item_mut(stale_binding)
            .unwrap()
            .admission_pnp_observation = Some(observation);
        configure_test_auto_admission(&mut session);

        let assessment = session.assess_auto_admission(None).unwrap();
        assert_eq!(assessment.field_cells, 0);
        assert_eq!(assessment.depth_bins, 0);
        assert_eq!(assessment.pose_bins, 0);
        assert!(assessment.item_contributions.is_empty());
    }

    #[test]
    fn auto_admission_excludes_existing_item_after_spacing_gate_tightens() {
        let mut session = CalibrationSession::new(board());
        let item = add_found_with_acquisition_key(
            &mut session,
            "spacing-gated.png",
            10,
            test_acquisition_key(0),
        );
        session.item_mut(item).unwrap().admission_pnp_observation = Some(test_pnp_observation());
        configure_test_auto_admission(&mut session);
        let before = session.assess_auto_admission(None).unwrap();
        assert_eq!(before.item_contributions.len(), 1);

        let mut tightened_criteria = test_criteria();
        tightened_criteria.min_adjacent_spacing_px = 11.0;
        let tightened_baseline = AutoCaptureBaseline::new(
            test_acquisition_key(0),
            CalibrationImageSize::new(640, 480).unwrap(),
            board(),
            tightened_criteria,
        )
        .unwrap();
        session
            .configure_auto_admission(Some(tightened_baseline), Some(test_binding()))
            .unwrap();
        let after = session.assess_auto_admission(None).unwrap();
        assert_eq!(after.field_cells, 0);
        assert_eq!(after.depth_bins, 0);
        assert_eq!(after.pose_bins, 0);
        assert!(after.item_contributions.is_empty());
    }

    #[test]
    fn target_capped_region_gain_accumulates_until_region_quota() {
        let mut counts = vec![0; 4];
        assert_eq!(
            target_capped_region_gain(&mut counts, 2, [(0, 1), (2, 1)]),
            2
        );
        assert_eq!(counts, vec![1, 0, 1, 0]);
        assert_eq!(
            target_capped_region_gain(&mut counts, 2, [(0, 1), (1, 1)]),
            2
        );
        assert_eq!(counts, vec![2, 1, 1, 0]);
        assert_eq!(
            target_capped_region_gain(&mut counts, 2, [(0, 1), (3, 5)]),
            2
        );
        assert_eq!(counts, vec![3, 1, 1, 5]);
    }

    #[test]
    fn quota_scoring_saturates_and_validation_rejects_overflow_targets() {
        assert_eq!(
            capped_region_score(&[usize::MAX, usize::MAX], usize::MAX),
            usize::MAX
        );
        let mut counts = vec![usize::MAX - 1];
        assert_eq!(
            target_capped_region_gain(&mut counts, usize::MAX, [(0, 10)]),
            1
        );
        assert_eq!(counts, vec![usize::MAX]);

        let mut criteria = test_criteria();
        criteria.field_target_per_cell = usize::MAX;
        assert!(matches!(
            criteria.validate(),
            Err(CalibrationSessionError::InvalidAutoCaptureBaseline(message))
                if message.contains("field quota target overflows")
        ));

        let mut criteria = test_criteria();
        criteria.depth_target_per_bin = usize::MAX;
        assert!(matches!(
            criteria.validate(),
            Err(CalibrationSessionError::InvalidAutoCaptureBaseline(message))
                if message.contains("depth quota target overflows")
        ));

        let mut criteria = test_criteria();
        criteria.pose_target_per_bin = usize::MAX;
        assert!(matches!(
            criteria.validate(),
            Err(CalibrationSessionError::InvalidAutoCaptureBaseline(message))
                if message.contains("pose quota target overflows")
        ));
    }

    #[test]
    fn minimum_auto_gain_rejects_candidate_below_threshold() {
        let mut session = CalibrationSession::new(board());
        let mut criteria = test_criteria();
        criteria.minimum_auto_gain = 0.75;
        let baseline = AutoCaptureBaseline::new(
            test_acquisition_key(0),
            CalibrationImageSize::new(640, 480).unwrap(),
            board(),
            criteria,
        )
        .unwrap();
        session
            .configure_auto_admission(Some(baseline), Some(test_binding()))
            .unwrap();
        let (token, revision) = auto_candidate_fixture(&session, 1, 31);
        let ChessboardDetectionOutcome::Found(found) = detection() else {
            panic!("fixture must contain a found detection");
        };

        let result = session.commit_auto_candidate(AutoCandidateCommit::new(
            token,
            revision,
            found,
            test_pnp_observation(),
        ));

        match result {
            Err(CalibrationSessionError::RejectInsufficientConstraintGain { actual, minimum }) => {
                assert_gain_eq(actual, 0.5);
                assert_gain_eq(minimum, 0.75);
            }
            other => panic!("expected insufficient gain rejection, got {other:?}"),
        }
        assert!(session.items().is_empty());
    }

    #[test]
    fn admission_digest_changes_when_quota_or_minimum_gain_changes() {
        let base = test_baseline();

        let mut criteria = test_criteria();
        criteria.field_target_per_cell = 2;
        let field_digest = AutoCaptureBaseline::new(
            test_acquisition_key(0),
            CalibrationImageSize::new(640, 480).unwrap(),
            board(),
            criteria,
        )
        .unwrap()
        .digest;
        assert_ne!(base.digest, field_digest);

        let mut criteria = test_criteria();
        criteria.minimum_auto_gain = 0.6;
        let threshold_digest = AutoCaptureBaseline::new(
            test_acquisition_key(0),
            CalibrationImageSize::new(640, 480).unwrap(),
            board(),
            criteria,
        )
        .unwrap()
        .digest;
        assert_ne!(base.digest, threshold_digest);
    }

    #[test]
    fn pose_zero_deadband_has_no_center_bin() {
        let mut criteria = test_criteria();
        criteria.pnp_tilt_deadband_deg = 0.0;
        criteria.pnp_tilt_bins = 2;
        criteria.pnp_azimuth_sectors = 4;
        criteria.validate().unwrap();

        assert_eq!(pose_bin_capacity_for_criteria(&criteria), Some(8));
        assert_eq!(pose_bin_for_criteria(&criteria, 0.0, 0.0).unwrap(), 0);
        assert_eq!(pose_bin_for_criteria(&criteria, 0.0, 90.0).unwrap(), 1);
    }

    #[test]
    fn depth_coverage_counts_corners_per_bin_not_views() {
        let mut criteria = test_criteria();
        criteria.pnp_depth_min = 100.0;
        criteria.pnp_depth_max = 130.0;
        criteria.pnp_depth_bins = 3;
        let binding = test_binding();
        let observation = PnPObservation::from_view_result(
            binding.digest.clone(),
            ViewCalibrationResult {
                rotation_vector: [0.0, 0.7, 0.0],
                translation_vector: [0.0, 0.0, 122.0],
                projected_points: Vec::new(),
                reprojection_rmse: 0.1,
                max_reprojection_error: 0.2,
            },
            board(),
        )
        .unwrap();
        assert!(observation.maximum_board_depth > observation.minimum_board_depth);
        let observed_minimum_depth = observation.minimum_board_depth;
        let observed_maximum_depth = observation.maximum_board_depth;

        let corner_counts =
            depth_corner_counts_for_criteria(&observation, &criteria, board()).unwrap();
        assert_eq!(
            corner_counts.iter().map(|(_, count)| *count).sum::<usize>(),
            4
        );
        assert_eq!(corner_counts, vec![(0, 2), (2, 2)]);

        let mut session = CalibrationSession::new(board());
        let first = add_found(&mut session, "first.png", 10);
        let second = add_found(&mut session, "second.png", 11);
        session.item_mut(first).unwrap().pnp_observation = Some(observation.clone());
        session.item_mut(second).unwrap().pnp_observation = Some(observation);
        let assessment = session
            .assess_dataset_acceptance(binding.reference_image_size, &criteria, Some(&binding))
            .unwrap();
        assert_eq!(assessment.depth_bins, 2);
        assert_eq!(assessment.depth_bin_counts.iter().sum::<usize>(), 8);
        assert_eq!(assessment.depth_ranges.len(), 2);
        assert_eq!(assessment.depth_ranges[0].item_id, first);
        assert_eq!(
            assessment.depth_ranges[0].minimum_depth,
            observed_minimum_depth
        );
        assert_eq!(
            assessment.depth_ranges[0].maximum_depth,
            observed_maximum_depth
        );
        assert_eq!(assessment.item_visualizations.len(), 2);
        assert_eq!(assessment.item_visualizations[0].item_id, first);
        assert_eq!(assessment.item_visualizations[0].field_cells, vec![0]);
        assert!(assessment.item_visualizations[0].pose_bin.is_some());
        let [first_contribution, second_contribution] = assessment.item_contributions.as_slice()
        else {
            panic!("expected two depth-compatible items");
        };
        assert_eq!(first_contribution.item_id, first);
        assert_gain_eq(first_contribution.depth_gain, 0.5);
        assert_eq!(second_contribution.item_id, second);
        assert_gain_eq(second_contribution.depth_gain, 0.0);
        assert!(
            assessment
                .item_contributions
                .iter()
                .all(|contribution| contribution.pnp_state == AutoAdmissionPnpState::Valid)
        );

        session.set_enabled(second, false).unwrap();
        let assessment = session
            .assess_dataset_acceptance(binding.reference_image_size, &criteria, Some(&binding))
            .unwrap();
        let [contribution] = assessment.item_contributions.as_slice() else {
            panic!("expected one enabled compatible item");
        };
        assert_eq!(assessment.depth_bin_counts.iter().sum::<usize>(), 4);
        assert_gain_eq(contribution.depth_gain, 0.5);
        assert_gain_eq(assessment.constraint_gain, contribution.constraint_gain);
        assert_gain_eq(
            assessment.constraint_gain,
            constraint_gain(
                assessment.field_gain,
                assessment.depth_gain,
                assessment.pose_gain,
            ),
        );
    }

    #[test]
    fn auto_candidate_commit_rejects_same_size_wrong_source_without_mutating_dataset() {
        let mut session = CalibrationSession::new(board());
        configure_test_auto_admission(&mut session);
        let (token, revision) = auto_candidate_fixture_with_source(
            &session,
            3,
            12,
            "stream-3",
            Some(AutoCaptureAcquisitionKey::new("other-source", 0, "test-geometry").unwrap()),
        );
        let ChessboardDetectionOutcome::Found(found) = detection() else {
            panic!("fixture must contain a found detection");
        };

        let result = session.commit_auto_candidate(AutoCandidateCommit::new(
            token,
            revision,
            found,
            test_pnp_observation(),
        ));
        assert!(
            matches!(
                &result,
                Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(message))
                    if message.contains("candidate source")
            ),
            "unexpected result: {result:?}"
        );
        assert!(session.items().is_empty());
        assert_eq!(session.selected(), None);
    }

    #[test]
    fn auto_candidate_commit_preserves_active_dataset_detection_token() {
        let mut session = CalibrationSession::new(board());
        let AddCalibrationItemOutcome::Added(dataset_id) = session.add_or_refresh(
            reference("dataset.png"),
            version(10),
            "dataset.png".to_owned(),
        ) else {
            panic!("expected dataset item");
        };
        let dataset_token = session.begin_detection(dataset_id).unwrap();
        configure_test_auto_admission(&mut session);
        let (candidate_token, candidate_revision) = auto_candidate_fixture(&session, 1, 8);
        let ChessboardDetectionOutcome::Found(candidate_detection) = detection() else {
            panic!("fixture must contain a found detection");
        };
        session
            .commit_auto_candidate(AutoCandidateCommit::new(
                candidate_token,
                candidate_revision,
                candidate_detection,
                test_pnp_observation(),
            ))
            .unwrap();

        session.mark_reading(&dataset_token).unwrap();
        session.mark_detect_queued(&dataset_token).unwrap();
        session.mark_detecting(&dataset_token).unwrap();
        session
            .install_detection(&dataset_token, version(10), detection())
            .unwrap();
        assert!(matches!(
            session.item(dataset_id).unwrap().status,
            CalibrationItemStatus::Found(_)
        ));
        assert_eq!(session.items().len(), 2);
    }
    #[test]
    fn pnp_combined_gain_counts_new_depth_and_pose_bins() {
        let mut session = CalibrationSession::new(board());
        let mut criteria = test_criteria();
        let baseline = AutoCaptureBaseline::new(
            test_acquisition_key(0),
            CalibrationImageSize::new(640, 480).unwrap(),
            board(),
            criteria,
        )
        .unwrap();
        session
            .configure_auto_admission(Some(baseline), Some(test_binding()))
            .unwrap();
        let ChessboardDetectionOutcome::Found(found) = detection() else {
            panic!("fixture must contain a found detection");
        };
        let observation = |rotation_vector, depth| {
            PnPObservation::from_view_result(
                test_binding().digest,
                ViewCalibrationResult {
                    rotation_vector,
                    translation_vector: [0.0, 0.0, depth],
                    projected_points: Vec::new(),
                    reprojection_rmse: 0.1,
                    max_reprojection_error: 0.2,
                },
                board(),
            )
            .unwrap()
        };

        let (first_token, first_revision) = auto_candidate_fixture(&session, 1, 21);
        let first_pnp = observation([0.0, 0.0, 0.0], 100.0);
        let first_assessment = session
            .assess_auto_admission(Some((&found, &first_pnp)))
            .unwrap();
        assert_gain_eq(first_assessment.field_gain, 0.25);
        assert_gain_eq(first_assessment.depth_gain, 0.25);
        assert_gain_eq(first_assessment.pose_gain, 1.0);
        assert_gain_eq(first_assessment.constraint_gain, 0.5);
        session
            .commit_auto_candidate(AutoCandidateCommit::new(
                first_token,
                first_revision,
                found.clone(),
                first_pnp,
            ))
            .unwrap();

        let (second_token, second_revision) = auto_candidate_fixture(&session, 2, 22);
        let second_pnp = observation([0.0, 0.5, 0.0], 1_000.0);
        let second_assessment = session
            .assess_auto_admission(Some((&found, &second_pnp)))
            .unwrap();
        assert_gain_eq(second_assessment.field_gain, 0.0);
        assert_gain_eq(second_assessment.depth_gain, 0.25);
        assert_gain_eq(second_assessment.pose_gain, 1.0);
        assert_gain_eq(second_assessment.constraint_gain, 1.25 / 3.0);
        assert_eq!(second_assessment.depth_bins, 2);
        assert_eq!(second_assessment.pose_bins, 2);
        session
            .commit_auto_candidate(AutoCandidateCommit::new(
                second_token,
                second_revision,
                found,
                second_pnp,
            ))
            .unwrap();
        let source_digest = test_binding().digest;
        let dataset_digest = InitialIntrinsicsBinding::dataset_full_frame(
            intrinsics(),
            CalibrationImageSize::new(640, 480).unwrap(),
        )
        .unwrap()
        .digest;
        assert!(session.items().iter().all(|item| {
            item.pnp_observation
                .as_ref()
                .is_some_and(|observation| observation.binding_digest == dataset_digest)
                && item
                    .admission_pnp_observation
                    .as_ref()
                    .is_some_and(|observation| observation.binding_digest == source_digest)
        }));
    }

    #[test]
    fn auto_candidate_rejects_pnp_evidence_bound_to_other_intrinsics() {
        let mut session = CalibrationSession::new(board());
        configure_test_auto_admission(&mut session);
        let (token, revision) = auto_candidate_fixture(&session, 1, 23);
        let ChessboardDetectionOutcome::Found(found) = detection() else {
            panic!("fixture must contain a found detection");
        };
        let mut observation = test_pnp_observation();
        let mut other_intrinsics = intrinsics();
        other_intrinsics.camera_matrix[0] = 501.0;
        observation.binding_digest = InitialIntrinsicsBinding::full_frame(
            other_intrinsics,
            CalibrationImageSize::new(640, 480).unwrap(),
            test_acquisition_key(0),
        )
        .unwrap()
        .digest;

        let result = session.commit_auto_candidate(AutoCandidateCommit::new(
            token,
            revision,
            found,
            observation,
        ));
        assert!(matches!(
            result,
            Err(CalibrationSessionError::RejectAutoAdmissionBindingMismatch(message))
                if message.contains("PnP evidence")
        ));
        assert!(session.items().is_empty());
    }

    #[test]
    fn auto_admission_rejects_pnp_reprojection_metrics_beyond_active_limits() {
        let mut session = CalibrationSession::new(board());
        configure_test_auto_admission(&mut session);
        let ChessboardDetectionOutcome::Found(found) = detection() else {
            panic!("fixture must contain a found detection");
        };
        let mut high_rmse = test_pnp_observation();
        high_rmse.reprojection_rmse = 1.6;
        assert!(matches!(
            session.assess_auto_admission(Some((&found, &high_rmse))),
            Err(CalibrationSessionError::RejectInvalidPnP(message))
                if message.contains("RMSE")
        ));

        let mut high_maximum = test_pnp_observation();
        high_maximum.max_reprojection_error = 4.1;
        assert!(matches!(
            session.assess_auto_admission(Some((&found, &high_maximum))),
            Err(CalibrationSessionError::RejectInvalidPnP(message))
                if message.contains("maximum reprojection")
        ));
    }

    #[test]
    fn pnp_observation_rejects_nonpositive_board_point_depth() {
        let result = PnPObservation::from_view_result(
            test_binding().digest,
            ViewCalibrationResult {
                rotation_vector: [0.0, 0.0, 0.0],
                translation_vector: [0.0, 0.0, -1.0],
                projected_points: Vec::new(),
                reprojection_rmse: 0.1,
                max_reprojection_error: 0.2,
            },
            board(),
        );
        assert!(matches!(
            result,
            Err(CalibrationSessionError::RejectInvalidPnP(message))
                if message.contains("positive camera depth")
        ));
    }
}
