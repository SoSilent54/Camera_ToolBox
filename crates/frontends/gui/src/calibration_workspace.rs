//! 同窗标定工作区；GUI 只编排 app 端口，文件读取和 OpenCV 均在后台执行。

#[cfg(test)]
use std::time::Duration;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError, TrySendError},
    },
    thread,
};

use camera_toolbox_adapters::{ImageRasterCodec, OpenCvCalibrationBackend};
use camera_toolbox_app::{
    AddCalibrationItemOutcome, AutoAdmissionAssessment, AutoAdmissionItemContribution,
    AutoAdmissionPnpState, AutoCandidateCommit, AutoCandidateId, AutoCandidateToken,
    AutoCaptureAcceptanceCriteria, AutoCaptureAcquisitionKey, AutoCaptureBaseline,
    CalibrationBackend, CalibrationCancellation, CalibrationEncodedPng, CalibrationInputKey,
    CalibrationInputRevision, CalibrationItemId, CalibrationItemStatus, CalibrationJobToken,
    CalibrationSession, CalibrationSnapshot, CaptureStore, DecodedVideoFrame, EepromInspectResult,
    EepromWriteResult, EntryName, ExportDestination, ExportReceipt, ExportService, FileSourceId,
    FileSystem, FileSystemError, FsCancellation, FsControl, InitialIntrinsicsBinding, OperationId,
    PnPObservation, RasterImageCodec, SnapshotHash, StreamCaptureId, StreamFrameIdentity,
    StreamSessionId, host_monotonic_time_ns,
};
use camera_toolbox_core::{
    AssetId, BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationSolution,
    CaptureMetadata, ChessboardDetection, ChromaOrder, EphemeralAsset, InitialIntrinsics,
    IntegrityState, MediaFormat, OwnedMediaPayload, Rgba8Frame, ViewCalibrationResult,
    YgStereoModuleCode, YgStereoSerialIdInput, Yuv420SpFrame, Yuv420SpSpec, YuvMatrix, YuvRange,
    parse_opencv_pinhole_radtan_yaml, write_opencv_pinhole_radtan_yaml,
    yuv420sp_to_rgba8_with_cancel,
};
use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::calibration_acceptance::{
    DEFAULT_DATASET_ACCEPTANCE_CONFIG, DEFAULT_DATASET_ACCEPTANCE_CONFIG_FILE_NAME,
    DatasetAcceptanceConfigAction, DatasetAcceptanceDraft, DatasetAcceptancePanelState,
    DatasetAcceptanceProgress, render_dataset_acceptance,
};
use crate::calibration_eeprom::{CalibrationEepromState, CalibrationProvisionIntent};
use crate::calibration_pipeline::{
    CalibrationDetectionPipeline, DatasetPoseEstimationSeed, DetectionProduct, DetectionStageEvent,
    DetectionStageResult, EncodedDetectionRequest, LoadedDetectionJob, MAX_ENCODED_PNG_BYTES,
    MAX_INFLIGHT_ENCODED_BYTES, PipelineStageError, PoseEstimationRequest, ReadJob, ReadSource,
    ReadStageEvent, ReadStageResult,
};
use crate::{
    explorer::CalibrationImportCandidate,
    viewer::{pixel_inspection_texture_options, viewer_texture_uv},
    workspace::{LiveAuthoritativeCapture, LiveStreamSource},
    x5_tcp_client,
};

const MAX_DATASET_ITEMS: usize = 256;
const REMOTE_READS_PER_SOURCE: usize = 8;
const AUTO_CAPTURE_ANALYSIS_INTERVAL_NS: u64 = 200_000_000;
const AUTO_CAPTURE_ACCEPT_COOLDOWN_NS: u64 = 750_000_000;
const LIVE_AUTO_CANDIDATE_CAPACITY: usize = 4;
const GUIDED_CAPTURE_HOLD_FRAMES: u8 = 4;
const GUIDED_HOLD_JITTER_XYZ_LIMIT: f64 = 0.025;
const GUIDED_HOLD_JITTER_Z_LIMIT: f64 = 0.04;
const GUIDED_HOLD_JITTER_RPY_DEGREES: f64 = 2.0;
const X5_RTSP_PTS_BRIDGE_TOLERANCE_90K: u64 = 3_000;
const X5_RTSP_PTS_BRIDGE_MAX_AGE_NS: u64 = 2_000_000_000;
const GUIDED_POSE_X_TOLERANCE: f64 = 0.10;
const GUIDED_POSE_Y_TOLERANCE: f64 = 0.10;
const GUIDED_POSE_Z_TOLERANCE: f64 = 0.24;
const GUIDED_POSE_ROLL_TOLERANCE_DEGREES: f64 = 10.0;
const GUIDED_POSE_PITCH_TOLERANCE_DEGREES: f64 = 10.0;
const GUIDED_POSE_YAW_TOLERANCE_DEGREES: f64 = 15.0;
const GUIDED_POSE_MATCH_SCORE_LIMIT: f64 = 1.0;
// 透视引导网格由目标 pose + 当前 K/D12 投影；目标 depth 由 bbox scale 迭代反解。
const GUIDED_POSE_OVERLAY_DEPTH_SOLVE_ITERS: usize = 12;
// 检测结果异步返回；Acceptance live 标记保留 1 秒，不再把实时角点画到主 Viewer。
const LIVE_DETECTION_MARKER_TTL_NS: u64 = 1_000_000_000;
const LATEST_DATASET_OVERLAY_TTL_NS: u64 = 1_000_000_000;
const COVERAGE_WIDTH: usize = 192;
const COVERAGE_GAUSSIAN_SIGMA: f32 = 42.0 / 1920.0 * COVERAGE_WIDTH as f32;
const MIN_PREVIEW_ZOOM: f32 = 0.05;
const MAX_PREVIEW_ZOOM: f32 = 64.0;
const OBSERVED_POINT_COLOR: egui::Color32 = egui::Color32::from_rgb(120, 230, 140);
const REPROJECTED_POINT_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 96, 96);
const CURRENT_GUI_REPROJECTED_POINT_COLOR: egui::Color32 = egui::Color32::from_rgb(80, 170, 255);
const INITIAL_DISTORTION_NAMES: [&str; 12] = [
    "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
];
const ZERO_DISTORTION_COEFFICIENTS: [f64; 12] = [0.0; 12];
const RMSE_TEXT_ON_FILL: egui::Color32 = egui::Color32::from_rgb(12, 32, 45);
const RMSE_TEXT_ON_TRACK: egui::Color32 = egui::Color32::WHITE;
const REPROJECTION_ARROW_WIDTH: f32 = 1.25;
const REPROJECTION_ARROW_HEAD_LENGTH: f32 = 5.0;
const REPROJECTION_ARROW_HEAD_HALF_WIDTH: f32 = 2.5;
const POSE_AXIS_STROKE_WIDTH: f32 = 2.25;
const POSE_AXIS_ENDPOINT_RADIUS: f32 = 3.0;
const POSE_AXIS_ORIGIN_RADIUS: f32 = 4.0;
const POSE_AXIS_X_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 80, 80);
const POSE_AXIS_Y_COLOR: egui::Color32 = egui::Color32::from_rgb(80, 220, 120);
const POSE_AXIS_Z_COLOR: egui::Color32 = egui::Color32::from_rgb(80, 150, 255);
const STALE_CALIBRATION_RESULT_REASON: &str = "Dataset selection changed after this result; re-run Calibrate before export or EEPROM provisioning.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CalibrationJobKind {
    Detect,
    Calibrate,
    DatasetPnpRefresh,
}

/// 手工内参的数值输入只在提交后触发重算，避免键入过程中抢占焦点。
fn observe_intrinsics_value_response(
    response: egui::Response,
    changed: &mut bool,
    editing: &mut bool,
) {
    let has_focus = response.has_focus();
    *editing |= has_focus;
    *changed |= response.changed() && !has_focus;
}

/// 标定页面支持的格式化导出；所有变体都通过同一 Explorer destination 保存。
pub(crate) enum CalibrationExport {
    Json(serde_json::Value),
    Yaml(CalibrationSolution),
}

impl CalibrationExport {
    #[must_use]
    pub(crate) const fn suggested_name(&self) -> &'static str {
        match self {
            Self::Json(_) => "camera_intrinsics.json",
            Self::Yaml(_) => "camera_intrinsics.yaml",
        }
    }

    #[must_use]
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Json(_) => "calibration JSON",
            Self::Yaml(_) => "calibration YAML",
        }
    }

    pub(crate) fn save_new(
        &self,
        destination: &ExportDestination,
        name: &EntryName,
        control: &FsControl,
    ) -> Result<ExportReceipt, FileSystemError> {
        ExportService.save_new_with(destination, name, control, &mut |writer| {
            self.write_to(writer)
        })
    }

    fn write_to(&self, writer: &mut dyn Write) -> Result<(), FileSystemError> {
        match self {
            Self::Json(document) => {
                serde_json::to_writer_pretty(&mut *writer, document)
                    .map_err(FileSystemError::io)?;
                writer.write_all(b"\n").map_err(FileSystemError::io)
            }
            Self::Yaml(solution) => write_opencv_pinhole_radtan_yaml(writer, solution)
                .map_err(|error| FileSystemError::Io(error.to_string())),
        }
    }
}

/// 外部 YAML 标定结果只携带 EEPROM 写入所需的 K/D12/尺寸，不绑定当前 Dataset。
struct LoadedCalibrationResult {
    source: String,
    solution: CalibrationSolution,
}

#[derive(Clone)]
struct CalibrationSource {
    display_name: String,
    kind: CalibrationSourceKind,
    preview: Option<CalibrationPreview>,
}

#[derive(Clone)]
enum CalibrationSourceKind {
    File {
        file_system: Arc<dyn FileSystem>,
        remote: bool,
    },
    Stream(StreamCalibrationSource),
}

#[derive(Clone)]
struct StreamCalibrationSource {
    store: CaptureStore,
    asset: Option<Arc<EphemeralAsset>>,
    analysis_asset: Option<Arc<EphemeralAsset>>,
    identity: StreamFrameIdentity,
    image_size: CalibrationImageSize,
    acquisition_key: camera_toolbox_app::AutoCaptureAcquisitionKey,
    authoritative_capture: Option<LiveAuthoritativeCapture>,
}

impl CalibrationSource {
    fn file(display_path: PathBuf, file_system: Arc<dyn FileSystem>, remote: bool) -> Self {
        Self {
            display_name: display_path.display().to_string(),
            kind: CalibrationSourceKind::File {
                file_system,
                remote,
            },
            preview: None,
        }
    }

    fn stream(
        store: CaptureStore,
        asset: Arc<EphemeralAsset>,
        identity: StreamFrameIdentity,
        image_size: CalibrationImageSize,
        acquisition_key: camera_toolbox_app::AutoCaptureAcquisitionKey,
    ) -> Self {
        Self::stream_with_analysis(
            store,
            asset,
            None,
            identity,
            image_size,
            acquisition_key,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_with_analysis(
        store: CaptureStore,
        asset: Arc<EphemeralAsset>,
        analysis_asset: Option<Arc<EphemeralAsset>>,
        identity: StreamFrameIdentity,
        image_size: CalibrationImageSize,
        acquisition_key: camera_toolbox_app::AutoCaptureAcquisitionKey,
        authoritative_capture: Option<LiveAuthoritativeCapture>,
    ) -> Self {
        let display_name = match asset.metadata.format {
            MediaFormat::Yuv420Sp { .. } => {
                format!(
                    "X5 YUV ch{} frame {}",
                    identity.channel, identity.frame_sequence
                )
            }
            _ => format!(
                "RTSP ch{} frame {}",
                identity.channel, identity.frame_sequence
            ),
        };
        Self {
            display_name,
            kind: CalibrationSourceKind::Stream(StreamCalibrationSource {
                store,
                asset: Some(asset),
                analysis_asset,
                identity,
                image_size,
                acquisition_key,
                authoritative_capture,
            }),
            preview: None,
        }
    }

    fn remote(&self) -> bool {
        matches!(self.kind, CalibrationSourceKind::File { remote: true, .. })
    }

    fn file_binding(&self) -> Option<(Arc<dyn FileSystem>, bool)> {
        match &self.kind {
            CalibrationSourceKind::File {
                file_system,
                remote,
            } => Some((Arc::clone(file_system), *remote)),
            CalibrationSourceKind::Stream(_) => None,
        }
    }

    fn encoded_png(
        &self,
        source_revision: &CalibrationInputRevision,
    ) -> Result<Option<CalibrationEncodedPng>, String> {
        let CalibrationSourceKind::Stream(stream) = &self.kind else {
            return Ok(None);
        };
        let _retained_acquisition_key = &stream.acquisition_key;
        let Some(asset) = stream.analysis_asset.as_ref().or(stream.asset.as_ref()) else {
            return Err("stream calibration asset was released".to_owned());
        };
        if asset.metadata.format != MediaFormat::Png {
            return Err("stream calibration analysis source is not a PNG asset".to_owned());
        }
        let OwnedMediaPayload::Bytes(bytes) = &asset.source else {
            return Err("stream calibration PNG must use one contiguous payload".to_owned());
        };
        Ok(Some(CalibrationEncodedPng {
            bytes: Arc::clone(bytes),
            image_size: stream.image_size,
            source_revision: source_revision.clone(),
        }))
    }
}

impl Drop for StreamCalibrationSource {
    fn drop(&mut self) {
        for asset in [self.analysis_asset.take(), self.asset.take()]
            .into_iter()
            .flatten()
        {
            let id = asset.id.clone();
            drop(asset);
            if let Err(error) = self.store.release(&id) {
                tracing::warn!(asset_id = %id, %error, "stream calibration asset release deferred by external ownership");
            }
        }
    }
}

struct FrozenStreamInput {
    source: CalibrationSource,
    encoded: CalibrationEncodedPng,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RtspPtsBridgeKey {
    stream_id: StreamSessionId,
    channel: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RtspPtsBridgeSample {
    source_pts_90k: u64,
    driver_rtsp_pts_90k: u64,
    offset_90k: i128,
    sampled_frame_sequence: u64,
    updated_at_host_ns: u64,
}

impl RtspPtsBridgeSample {
    fn target_rtsp_pts_90k(&self, source_pts_90k: u64) -> Result<u64, String> {
        let target = i128::from(source_pts_90k) + self.offset_90k;
        u64::try_from(target).map_err(|_| {
            format!(
                "RTSP PTS bridge target is outside u64: source_pts_90k={source_pts_90k}, offset_90k={}",
                self.offset_90k
            )
        })
    }
}

// RTSP 当前未携带显式 board metadata；PTS bridge 是过渡方案，只匹配同一 RTP 90k 时间轴。
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum AuthoritativeYuvLookup {
    FrameId(u64),
    TimestampNs(u64),
    RtspPts90k {
        pts_90k: u64,
        tolerance_90k: u64,
        source_pts_90k: u64,
        bridge_offset_90k: i128,
    },
}

impl AuthoritativeYuvLookup {
    fn from_rtsp_identity(identity: &StreamFrameIdentity) -> Result<Self, String> {
        match &identity.source_pts {
            camera_toolbox_app::SourcePts::Known { provenance, .. } => Err(format!(
                "RTSP frame PTS ({provenance:?}) is presentation timing only; direct X5 authoritative YUV lookup requires explicit frame_id/timestamp_ns metadata from RTSP SEI or RTP header extension"
            )),
            camera_toolbox_app::SourcePts::Unavailable { reason } => Err(format!(
                "RTSP frame has no board frame_id/timestamp_ns metadata: {reason}"
            )),
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::FrameId(_) => "frame_id",
            Self::TimestampNs(_) => "timestamp_ns",
            Self::RtspPts90k { .. } => "rtsp_pts_90k",
        }
    }

    const fn value(&self) -> u64 {
        match self {
            Self::FrameId(value) | Self::TimestampNs(value) => *value,
            Self::RtspPts90k { pts_90k, .. } => *pts_90k,
        }
    }
}

fn source_pts_to_90k(source_pts: &camera_toolbox_app::SourcePts) -> Result<u64, String> {
    let camera_toolbox_app::SourcePts::Known {
        ticks,
        time_base_numerator,
        time_base_denominator,
        provenance,
    } = source_pts
    else {
        return Err(format!("RTSP frame has no PTS: {source_pts:?}"));
    };
    if *ticks < 0 || *time_base_numerator == 0 || *time_base_denominator == 0 {
        return Err(format!(
            "RTSP frame PTS is invalid for bridge: ticks={ticks}, time_base={time_base_numerator}/{time_base_denominator}, provenance={provenance:?}"
        ));
    }
    let scaled = i128::from(*ticks)
        .checked_mul(i128::from(*time_base_numerator))
        .and_then(|value| value.checked_mul(90_000))
        .ok_or_else(|| format!("RTSP frame PTS overflows 90k bridge: ticks={ticks}"))?
        / i128::from(*time_base_denominator);
    u64::try_from(scaled).map_err(|_| {
        format!(
            "RTSP frame PTS cannot be represented as u64 90k ticks: ticks={ticks}, time_base={time_base_numerator}/{time_base_denominator}"
        )
    })
}

fn x5_ring_status_range(valid: u16, min: u64, max: u64) -> String {
    if valid == 0 {
        "—".to_owned()
    } else if min == max {
        min.to_string()
    } else {
        format!("{min}..{max}")
    }
}

fn format_x5_authoritative_yuv_ring_diagnostics(
    status: &x5_tcp_client::X5DriverStatus,
    channel: u16,
) -> String {
    let Some(ring) = status.rings.iter().find(|ring| ring.channel == channel) else {
        let channels = status
            .rings
            .iter()
            .map(|ring| format!("CH{}", ring.channel))
            .collect::<Vec<_>>()
            .join(",");
        return format!(
            "ring_status=missing_channel_CH{channel}, available_rings={}",
            if channels.is_empty() {
                "—"
            } else {
                &channels
            }
        );
    };
    format!(
        "ring_channel=CH{}, ring_valid={}/{}, ring_frame_id={}, ring_timestamp_ns={}, ring_rtsp_pts_90k={}, ring_last_rtsp_pts_90k={}, ring_retention_ns={}, ring_evicted={}, ring_dropped={}",
        ring.channel,
        ring.valid,
        ring.depth,
        x5_ring_status_range(ring.valid, ring.min_frame_id, ring.max_frame_id),
        x5_ring_status_range(ring.valid, ring.min_timestamp_ns, ring.max_timestamp_ns),
        x5_ring_status_range(ring.valid, ring.min_rtsp_pts_90k, ring.max_rtsp_pts_90k),
        ring.last_rtsp_pts_90k,
        ring.retention_ns,
        ring.evicted,
        ring.dropped
    )
}

fn query_x5_authoritative_yuv_ring_diagnostics(host: &str, tcp_port: u16, channel: u16) -> String {
    match x5_tcp_client::status(host, tcp_port) {
        Ok(status) => format_x5_authoritative_yuv_ring_diagnostics(&status, channel),
        Err(error) => format!("ring_status_error={error}"),
    }
}

fn freeze_stream_input(
    frame: &Arc<DecodedVideoFrame>,
    store: CaptureStore,
    acquisition_key: camera_toolbox_app::AutoCaptureAcquisitionKey,
    authoritative_capture: Option<LiveAuthoritativeCapture>,
) -> Result<FrozenStreamInput, String> {
    let rgba = Rgba8Frame::tight(frame.width, frame.height, Arc::clone(&frame.rgba))
        .map_err(|error| format!("Cannot freeze displayed stream frame: {error}"))?;
    let image_size = CalibrationImageSize::new(frame.width, frame.height)
        .map_err(|error| format!("Cannot capture displayed stream frame: {error}"))?;
    let mut encoded = Vec::new();
    ImageRasterCodec
        .encode_png(&rgba, &mut encoded)
        .map_err(|error| format!("Cannot encode displayed stream frame as PNG: {error}"))?;
    if encoded.len() > MAX_ENCODED_PNG_BYTES as usize {
        return Err(format!(
            "Encoded stream frame is {} bytes, limit is {} bytes.",
            encoded.len(),
            MAX_ENCODED_PNG_BYTES
        ));
    }

    let content_sha256 = SnapshotHash::digest_bytes(&encoded).to_hex();
    let asset_id = AssetId::new(format!(
        "calibration-stream-{}-{}-{}-{content_sha256}",
        frame.identity.stream_id.as_str(),
        frame.identity.channel,
        frame.identity.frame_sequence,
    ))
    .map_err(|error| format!("Cannot identify captured stream frame: {error}"))?;
    let operation_id = OperationId::new(format!("capture-{}", asset_id.as_str()))
        .map_err(|error| format!("Cannot reserve captured stream frame: {error}"))?;
    let bytes: Arc<[u8]> = Arc::from(encoded);
    let reservation = store
        .reserve(operation_id, bytes.len())
        .map_err(|error| format!("Cannot reserve memory for captured stream frame: {error}"))?;
    let mut attributes = BTreeMap::new();
    attributes.insert(
        "stream_id".to_owned(),
        frame.identity.stream_id.as_str().to_owned(),
    );
    attributes.insert("channel".to_owned(), frame.identity.channel.to_string());
    attributes.insert(
        "frame_sequence".to_owned(),
        frame.identity.frame_sequence.to_string(),
    );
    attributes.insert(
        "host_monotonic_time_ns".to_owned(),
        frame.identity.host_monotonic_time_ns.to_string(),
    );
    attributes.insert(
        "source_pts".to_owned(),
        format!("{:?}", frame.identity.source_pts),
    );
    attributes.insert("width".to_owned(), frame.width.to_string());
    attributes.insert("height".to_owned(), frame.height.to_string());
    attributes.insert(
        "acquisition_source_fingerprint".to_owned(),
        acquisition_key.source_fingerprint.clone(),
    );
    attributes.insert(
        "acquisition_geometry_key".to_owned(),
        acquisition_key.geometry_key.clone(),
    );
    let asset = EphemeralAsset::new(
        asset_id,
        OwnedMediaPayload::Bytes(Arc::clone(&bytes)),
        CaptureMetadata {
            format: MediaFormat::Png,
            source_name: format!(
                "RTSP ch{} frame {}",
                frame.identity.channel, frame.identity.frame_sequence
            ),
            attributes,
        },
        IntegrityState::Verified {
            algorithm: "sha256".to_owned(),
            digest: content_sha256.clone(),
        },
    );
    let asset = store
        .publish_validated(reservation, asset)
        .map_err(|error| format!("Cannot publish captured stream frame: {error}"))?;
    let source_revision = CalibrationInputRevision::EphemeralPng {
        content_sha256,
        encoded_bytes: asset.byte_len().unwrap_or_default() as u64,
    };
    Ok(FrozenStreamInput {
        source: CalibrationSource::stream_with_analysis(
            store,
            asset,
            None,
            frame.identity.clone(),
            image_size,
            acquisition_key,
            authoritative_capture,
        ),
        encoded: CalibrationEncodedPng {
            bytes,
            image_size,
            source_revision,
        },
    })
}

fn x5_yuv_snapshot_spec(snapshot: &x5_tcp_client::X5YuvSnapshot) -> Result<Yuv420SpSpec, String> {
    let height = usize::try_from(snapshot.height)
        .map_err(|_| "X5 YUV height does not fit host usize".to_owned())?;
    if height == 0 || snapshot.y_len % height != 0 {
        return Err(format!(
            "X5 YUV y_len {} is not divisible by height {}",
            snapshot.y_len, snapshot.height
        ));
    }
    let chroma_rows = height / 2;
    if chroma_rows == 0 || snapshot.uv_len % chroma_rows != 0 {
        return Err(format!(
            "X5 YUV uv_len {} is not divisible by chroma rows {chroma_rows}",
            snapshot.uv_len
        ));
    }
    let spec = Yuv420SpSpec {
        width: snapshot.width,
        height: snapshot.height,
        y_stride: snapshot.y_len / height,
        chroma_stride: snapshot.uv_len / chroma_rows,
        chroma_order: ChromaOrder::Uv,
        matrix: YuvMatrix::Bt601,
        range: YuvRange::Limited,
    };
    spec.validate()
        .map_err(|error| format!("X5 YUV metadata is invalid: {error}"))?;
    Ok(spec)
}

fn freeze_authoritative_yuv_input(
    snapshot: x5_tcp_client::X5YuvSnapshot,
    store: CaptureStore,
    acquisition_key: camera_toolbox_app::AutoCaptureAcquisitionKey,
    rtsp_identity: &StreamFrameIdentity,
    lookup: &AuthoritativeYuvLookup,
) -> Result<FrozenStreamInput, String> {
    let spec = x5_yuv_snapshot_spec(&snapshot)?;
    let image_size = CalibrationImageSize::new(snapshot.width, snapshot.height)
        .map_err(|error| format!("Cannot capture X5 YUV frame: {error}"))?;
    let payload_len = snapshot.payload.len();
    if payload_len != snapshot.y_len.saturating_add(snapshot.uv_len) {
        return Err(format!(
            "X5 YUV payload length mismatch: y_len + uv_len = {}, got {payload_len}",
            snapshot.y_len.saturating_add(snapshot.uv_len)
        ));
    }

    let primary_sha256 = SnapshotHash::digest_bytes(&snapshot.payload).to_hex();
    let yuv_frame = Yuv420SpFrame::from_contiguous(spec, Arc::new(snapshot.payload.clone()))
        .map_err(|error| format!("Cannot decode X5 YUV snapshot: {error}"))?;
    let rgba = yuv420sp_to_rgba8_with_cancel(&yuv_frame, || false).map_err(|error| {
        format!("Cannot derive calibration analysis image from X5 YUV: {error}")
    })?;
    let mut analysis_png = Vec::new();
    ImageRasterCodec
        .encode_png(&rgba, &mut analysis_png)
        .map_err(|error| format!("Cannot encode X5 YUV analysis PNG: {error}"))?;
    if analysis_png.len() > MAX_ENCODED_PNG_BYTES as usize {
        return Err(format!(
            "Encoded X5 YUV analysis image is {} bytes, limit is {} bytes.",
            analysis_png.len(),
            MAX_ENCODED_PNG_BYTES
        ));
    }
    let analysis_sha256 = SnapshotHash::digest_bytes(&analysis_png).to_hex();
    let captured_at_ns = host_monotonic_time_ns();

    let mut primary_attributes = BTreeMap::new();
    primary_attributes.insert(
        "source".to_owned(),
        "x5_233_tcp_authoritative_yuv".to_owned(),
    );
    primary_attributes.insert("channel".to_owned(), snapshot.channel.to_string());
    primary_attributes.insert("width".to_owned(), snapshot.width.to_string());
    primary_attributes.insert("height".to_owned(), snapshot.height.to_string());
    primary_attributes.insert("y_stride".to_owned(), spec.y_stride.to_string());
    primary_attributes.insert("chroma_stride".to_owned(), spec.chroma_stride.to_string());
    primary_attributes.insert("y_len".to_owned(), snapshot.y_len.to_string());
    primary_attributes.insert("uv_len".to_owned(), snapshot.uv_len.to_string());
    primary_attributes.insert(
        "rtsp_timestamp_us".to_owned(),
        snapshot.rtsp_timestamp_us.to_string(),
    );
    primary_attributes.insert("rtsp_pts_90k".to_owned(), snapshot.rtsp_pts_90k.to_string());
    if let Some(delta_90k) = snapshot.match_rtsp_pts_delta_90k {
        primary_attributes.insert("match_rtsp_pts_delta_90k".to_owned(), delta_90k.to_string());
    }
    if let AuthoritativeYuvLookup::RtspPts90k {
        source_pts_90k,
        bridge_offset_90k,
        tolerance_90k,
        ..
    } = lookup
    {
        primary_attributes.insert(
            "rtsp_bridge_source_pts_90k".to_owned(),
            source_pts_90k.to_string(),
        );
        primary_attributes.insert(
            "rtsp_bridge_offset_90k".to_owned(),
            bridge_offset_90k.to_string(),
        );
        primary_attributes.insert(
            "rtsp_bridge_tolerance_90k".to_owned(),
            tolerance_90k.to_string(),
        );
    }
    primary_attributes.insert("frame_id".to_owned(), snapshot.frame_id.to_string());
    primary_attributes.insert("timestamp_ns".to_owned(), snapshot.timestamp_ns.to_string());
    primary_attributes.insert(
        "match_mode".to_owned(),
        snapshot
            .match_mode
            .as_deref()
            .unwrap_or(lookup.label())
            .to_owned(),
    );
    primary_attributes.insert(
        "rtsp_precheck_stream_id".to_owned(),
        rtsp_identity.stream_id.as_str().to_owned(),
    );
    primary_attributes.insert(
        "rtsp_precheck_frame_sequence".to_owned(),
        rtsp_identity.frame_sequence.to_string(),
    );
    primary_attributes.insert(
        "rtsp_precheck_host_monotonic_time_ns".to_owned(),
        rtsp_identity.host_monotonic_time_ns.to_string(),
    );
    primary_attributes.insert(
        "rtsp_precheck_source_pts".to_owned(),
        format!("{:?}", rtsp_identity.source_pts),
    );
    primary_attributes.insert(
        "captured_at_host_monotonic_ns".to_owned(),
        captured_at_ns.to_string(),
    );
    primary_attributes.insert(
        "acquisition_source_fingerprint".to_owned(),
        acquisition_key.source_fingerprint.clone(),
    );
    primary_attributes.insert(
        "acquisition_geometry_key".to_owned(),
        acquisition_key.geometry_key.clone(),
    );

    let primary_asset_id = AssetId::new(format!(
        "calibration-x5-yuv-ch{}-frame{}-{captured_at_ns}-{}",
        snapshot.channel,
        snapshot.frame_id,
        &primary_sha256[..16]
    ))
    .map_err(|error| format!("Cannot identify X5 YUV frame: {error}"))?;
    let primary_operation_id = OperationId::new(format!("capture-{}", primary_asset_id.as_str()))
        .map_err(|error| format!("Cannot reserve X5 YUV frame: {error}"))?;
    let primary_bytes = Arc::<[u8]>::from(snapshot.payload);
    let primary_reservation = store
        .reserve(primary_operation_id, primary_bytes.len())
        .map_err(|error| format!("Cannot reserve memory for X5 YUV frame: {error}"))?;
    let primary_asset = EphemeralAsset::new(
        primary_asset_id,
        OwnedMediaPayload::Bytes(Arc::clone(&primary_bytes)),
        CaptureMetadata {
            format: MediaFormat::Yuv420Sp {
                chroma_order: ChromaOrder::Uv,
            },
            source_name: format!(
                "x5-233-ch{}-frame{}.nv12",
                snapshot.channel, snapshot.frame_id
            ),
            attributes: primary_attributes,
        },
        IntegrityState::Verified {
            algorithm: "sha256".to_owned(),
            digest: primary_sha256.clone(),
        },
    );
    let primary_asset = store
        .publish_validated(primary_reservation, primary_asset)
        .map_err(|error| format!("Cannot publish X5 YUV frame: {error}"))?;

    let analysis_bytes: Arc<[u8]> = Arc::from(analysis_png);
    let analysis_asset_id = AssetId::new(format!(
        "calibration-x5-yuv-analysis-ch{}-frame{}-{captured_at_ns}-{}",
        snapshot.channel,
        snapshot.frame_id,
        &analysis_sha256[..16]
    ))
    .map_err(|error| format!("Cannot identify X5 YUV analysis image: {error}"))?;
    let analysis_operation_id = OperationId::new(format!("capture-{}", analysis_asset_id.as_str()))
        .map_err(|error| format!("Cannot reserve X5 YUV analysis image: {error}"))?;
    let analysis_reservation = store
        .reserve(analysis_operation_id, analysis_bytes.len())
        .map_err(|error| format!("Cannot reserve memory for X5 YUV analysis image: {error}"))?;
    let mut analysis_attributes = BTreeMap::new();
    analysis_attributes.insert(
        "source".to_owned(),
        "x5_233_tcp_authoritative_yuv_analysis".to_owned(),
    );
    analysis_attributes.insert("primary_sha256".to_owned(), primary_sha256.clone());
    analysis_attributes.insert("primary_format".to_owned(), "nv12".to_owned());
    analysis_attributes.insert("frame_id".to_owned(), snapshot.frame_id.to_string());
    analysis_attributes.insert("timestamp_ns".to_owned(), snapshot.timestamp_ns.to_string());
    let analysis_asset = EphemeralAsset::new(
        analysis_asset_id,
        OwnedMediaPayload::Bytes(Arc::clone(&analysis_bytes)),
        CaptureMetadata {
            format: MediaFormat::Png,
            source_name: format!(
                "x5-233-ch{}-frame{}-analysis.png",
                snapshot.channel, snapshot.frame_id
            ),
            attributes: analysis_attributes,
        },
        IntegrityState::Verified {
            algorithm: "sha256".to_owned(),
            digest: analysis_sha256.clone(),
        },
    );
    let analysis_asset = store
        .publish_validated(analysis_reservation, analysis_asset)
        .map_err(|error| format!("Cannot publish X5 YUV analysis image: {error}"))?;

    let yuv_identity = StreamFrameIdentity::known_at(
        rtsp_identity.stream_id.clone(),
        snapshot.channel,
        snapshot.frame_id,
        camera_toolbox_app::SourcePts::Unavailable {
            reason: format!(
                "X5_233 TCP SNAPSHOT matched RTSP precheck by {}; timestamp_ns={}",
                lookup.label(),
                snapshot.timestamp_ns
            ),
        },
        captured_at_ns,
    );
    let source_revision = CalibrationInputRevision::EphemeralRaster {
        primary_sha256,
        primary_bytes: u64::try_from(primary_bytes.len()).unwrap_or(u64::MAX),
        primary_format: "nv12".to_owned(),
        analysis_sha256,
        analysis_encoded_bytes: u64::try_from(analysis_bytes.len()).unwrap_or(u64::MAX),
    };
    Ok(FrozenStreamInput {
        source: CalibrationSource::stream_with_analysis(
            store,
            primary_asset,
            Some(analysis_asset),
            yuv_identity,
            image_size,
            acquisition_key,
            None,
        ),
        encoded: CalibrationEncodedPng {
            bytes: analysis_bytes,
            image_size,
            source_revision,
        },
    })
}

#[derive(Clone)]
struct CalibrationPreview {
    frame: Arc<Rgba8Frame>,
    texture: egui::TextureHandle,
}

#[derive(Default)]
struct CalibrationPreviewViewport {
    item_id: Option<CalibrationItemId>,
    zoom: f32,
    pan: egui::Vec2,
    fit_on_next_frame: bool,
    horizontal_flip: bool,
}

impl CalibrationPreviewViewport {
    fn reset_for(&mut self, item_id: CalibrationItemId) {
        if self.item_id != Some(item_id) {
            self.item_id = Some(item_id);
            self.fit_on_next_frame = true;
        }
    }

    fn fit_to_rect(&mut self, rect: egui::Rect, image_size: egui::Vec2) {
        self.zoom =
            contain_fit_scale(rect.size(), image_size).clamp(MIN_PREVIEW_ZOOM, MAX_PREVIEW_ZOOM);
        self.pan = rect.center().to_vec2() - image_size * self.zoom * 0.5 - rect.min.to_vec2();
        self.fit_on_next_frame = false;
    }

    fn zoom_by(&mut self, factor: f32, anchor: egui::Pos2, viewport: egui::Rect) {
        let old_zoom = self.zoom;
        self.zoom = (self.zoom * factor).clamp(MIN_PREVIEW_ZOOM, MAX_PREVIEW_ZOOM);
        let scale = self.zoom / old_zoom;
        let local_anchor = anchor - viewport.min;
        self.pan = local_anchor + (self.pan - local_anchor) * scale;
    }

    fn interact(
        &mut self,
        ui: &egui::Ui,
        response: &egui::Response,
        viewport: egui::Rect,
        image_size: egui::Vec2,
    ) -> egui::Rect {
        if self.fit_on_next_frame || self.zoom <= 0.0 {
            self.fit_to_rect(viewport, image_size);
        }
        if response.dragged_by(egui::PointerButton::Primary) {
            self.pan += response.drag_delta();
        }
        if response.hovered() {
            let scroll_y = ui.input(|input| input.smooth_scroll_delta().y);
            if scroll_y.abs() > f32::EPSILON
                && let Some(anchor) = response.hover_pos()
            {
                self.zoom_by((scroll_y * 0.0015).exp(), anchor, viewport);
            }
        }
        egui::Rect::from_min_size(viewport.min + self.pan, image_size * self.zoom)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CalibrationDisplayLayer {
    #[default]
    LiveStream,
    DatasetImage,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CalibrationPreviewMode {
    Heatmap,
    Overlay,
    #[default]
    InputImage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreviewLayers {
    input: bool,
    heatmap_alpha: Option<u8>,
}

const fn preview_layers(mode: CalibrationPreviewMode, heatmap_available: bool) -> PreviewLayers {
    match (mode, heatmap_available) {
        (CalibrationPreviewMode::Heatmap, true) => PreviewLayers {
            input: false,
            heatmap_alpha: Some(255),
        },
        (CalibrationPreviewMode::Overlay, true) => PreviewLayers {
            input: true,
            heatmap_alpha: Some(150),
        },
        (CalibrationPreviewMode::InputImage, _) | (_, false) => PreviewLayers {
            input: true,
            heatmap_alpha: None,
        },
    }
}
struct CoverageVisualization {
    density: egui::TextureHandle,
    enabled_views: usize,
}

#[derive(Clone)]
struct IdentityBoundDetection {
    identity: StreamFrameIdentity,
    acquisition_key: AutoCaptureAcquisitionKey,
    detection: ChessboardDetection,
    pnp_observation: Option<PnPObservation>,
    completed_at_ns: u64,
}

#[derive(Clone)]
struct DatasetDetectionOverlay {
    item_id: CalibrationItemId,
    detection: ChessboardDetection,
    acquisition_key: AutoCaptureAcquisitionKey,
    pnp_observation: Option<PnPObservation>,
    committed_at_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ViewerPoseAxisOverlay {
    pub(crate) origin: CalibrationPoint,
    pub(crate) x_axis: CalibrationPoint,
    pub(crate) y_axis: CalibrationPoint,
    pub(crate) z_axis: CalibrationPoint,
}

#[derive(Clone)]
pub(crate) struct ViewerDetectionOverlay {
    pub(crate) image_size: CalibrationImageSize,
    pub(crate) corners: Vec<CalibrationPoint>,
    pub(crate) pose_axis: Option<ViewerPoseAxisOverlay>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ViewerGuidedPoseGridLine {
    pub(crate) start_uv: [f32; 2],
    pub(crate) end_uv: [f32; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ViewerGuidedPoseRotationArcOverlay {
    pub(crate) label: &'static str,
    pub(crate) error_degrees: f64,
    pub(crate) tolerance_degrees: f64,
    pub(crate) base_uv: Arc<[[f32; 2]]>,
    pub(crate) arc_uv: Arc<[[f32; 2]]>,
    pub(crate) tick_uv: [f32; 2],
    pub(crate) label_uv: [f32; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ViewerGuidedPoseRotationRingsOverlay {
    pub(crate) center_uv: [f32; 2],
    pub(crate) roll: ViewerGuidedPoseRotationArcOverlay,
    pub(crate) pitch: ViewerGuidedPoseRotationArcOverlay,
    pub(crate) yaw: ViewerGuidedPoseRotationArcOverlay,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ViewerGuidedPoseArrowOverlay {
    pub(crate) start_uv: [f32; 2],
    pub(crate) end_uv: [f32; 2],
    pub(crate) start_xyz: [f64; 3],
    pub(crate) end_xyz: [f64; 3],
    pub(crate) z_delta: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ViewerGuidedPoseInstructionOverlay {
    pub(crate) primary: &'static str,
    pub(crate) secondary: String,
    pub(crate) score: f64,
    pub(crate) matched: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ViewerGuidedPoseOverlay {
    pub(crate) center_uv: [f32; 2],
    pub(crate) outline_uv: [[f32; 2]; 4],
    pub(crate) grid_lines: Arc<[ViewerGuidedPoseGridLine]>,
    pub(crate) rotation_rings: Option<ViewerGuidedPoseRotationRingsOverlay>,
    pub(crate) pose_arrow: Option<ViewerGuidedPoseArrowOverlay>,
    pub(crate) instruction: Option<ViewerGuidedPoseInstructionOverlay>,
    pub(crate) matched: bool,
}

#[derive(Clone, Default)]
pub(crate) struct CalibrationViewerOverlay {
    /// Live Viewer 只叠加最近入库 Dataset 的短时角点提示，不替换实时视频底图。
    pub(crate) persistent: Option<ViewerDetectionOverlay>,
    /// 实时检测到的当前棋盘坐标轴；Guided mode 用旋转圆环替代坐标轴显示。
    pub(crate) realtime_detection: Option<ViewerDetectionOverlay>,
    /// 引导式自动快门的当前目标位置提示；只表达操作目标，不代表已入库数据。
    pub(crate) guided_target: Option<ViewerGuidedPoseOverlay>,
}

#[derive(Clone)]
pub(crate) struct CalibrationViewerPresentation {
    pub(crate) item_id: Option<CalibrationItemId>,
    pub(crate) overlay: CalibrationViewerOverlay,
}

struct ActiveCancellation {
    token: CalibrationJobToken,
    file_system: FsCancellation,
    calibration: CalibrationCancellation,
}

impl ActiveCancellation {
    fn cancel(&self) {
        self.file_system.cancel();
        self.calibration.cancel();
    }
}

struct DetectionBatch {
    id: u64,
    total: usize,
    completed: usize,
    reserved_encoded_bytes: u64,
    cancel_requested: bool,
    terminal_status: Option<String>,
    cancellations: HashMap<CalibrationItemId, ActiveCancellation>,
    active_remote_sources: HashMap<FileSourceId, usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoCandidateState {
    Queued,
    Submitted,
    Detecting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidateIntent {
    PreviewOnly,
    AutoCommit,
    GuidedMeasure,
    GuidedCapture,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum AutoCaptureTriggerMode {
    #[default]
    DatasetGain,
    GuidedPresetPose,
}

impl AutoCaptureTriggerMode {
    const ALL: [Self; 2] = [Self::DatasetGain, Self::GuidedPresetPose];

    const fn label(self) -> &'static str {
        match self {
            Self::DatasetGain => "Dataset gain",
            Self::GuidedPresetPose => "Guided preset pose",
        }
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
    grid_lines: Arc<[ViewerGuidedPoseGridLine]>,
}

#[derive(Clone, Debug, PartialEq)]
struct GuidedPoseMeasurement {
    pose: GuidedPose6Dof,
    board: BoardSpec,
    initial_intrinsics: InitialIntrinsics,
    image_size: CalibrationImageSize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GuidedPoseInstructionComponent {
    X,
    Y,
    Z,
    Roll,
    Pitch,
    Yaw,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GuidedCaptureState {
    Running,
    Paused,
    Complete,
}

#[derive(Clone, Debug, PartialEq)]
struct GuidedCaptureBinding {
    source: LiveStreamSource,
    acquisition_key: AutoCaptureAcquisitionKey,
    image_size: CalibrationImageSize,
    board: BoardSpec,
    initial_intrinsics: InitialIntrinsics,
    intrinsics_digest: SnapshotHash,
}

struct GuidedHoldSample {
    token: AutoCandidateToken,
    source: CalibrationSource,
    pose_request: Option<PoseEstimationRequest>,
    guided_step_index: Option<usize>,
    stability_score: f64,
}

struct GuidedHoldUpdate {
    capture_sample: Option<GuidedHoldSample>,
}

struct GuidedCaptureRuntime {
    plan: Vec<GuidedPoseTarget>,
    current_step: usize,
    state: GuidedCaptureState,
    binding: GuidedCaptureBinding,
    hold_frames: u8,
    capture_requested: bool,
    last_assessment: Option<GuidedPoseAssessment>,
    last_hold_measurement: Option<GuidedPoseMeasurement>,
    best_hold_sample: Option<GuidedHoldSample>,
}

impl GuidedCaptureRuntime {
    fn standard_25(binding: GuidedCaptureBinding) -> Result<Self, String> {
        let plan = standard_guided_pose_plan(
            binding.board,
            &binding.initial_intrinsics,
            binding.image_size,
        )?;
        Ok(Self {
            plan,
            current_step: 0,
            state: GuidedCaptureState::Running,
            binding,
            hold_frames: 0,
            capture_requested: false,
            last_assessment: None,
            last_hold_measurement: None,
            best_hold_sample: None,
        })
    }

    fn current_target(&self) -> Option<&GuidedPoseTarget> {
        self.plan.get(self.current_step)
    }

    fn is_running(&self) -> bool {
        self.state == GuidedCaptureState::Running
    }

    fn current_step_label(&self) -> String {
        match self.current_target() {
            Some(target) => format!(
                "Step {} / {} · {}",
                self.current_step + 1,
                self.plan.len(),
                target.label
            ),
            None => "Standard guided complete".to_owned(),
        }
    }

    fn update_hold(
        &mut self,
        mut assessment: GuidedPoseAssessment,
        sample: Option<GuidedHoldSample>,
    ) -> GuidedHoldUpdate {
        let mut capture_sample = None;
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
            } else {
                self.hold_frames = self
                    .hold_frames
                    .saturating_add(1)
                    .min(GUIDED_CAPTURE_HOLD_FRAMES);
                self.last_hold_measurement = Some(assessment.measurement.clone());
                if let Some(mut sample) = sample {
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
                }
                if self.hold_frames >= GUIDED_CAPTURE_HOLD_FRAMES {
                    self.capture_requested = true;
                    capture_sample = self.best_hold_sample.take();
                }
            }
        } else {
            self.reset_hold();
        }
        self.last_assessment = Some(assessment);
        GuidedHoldUpdate { capture_sample }
    }

    fn clear_hold_state(&mut self) {
        self.hold_frames = 0;
        self.capture_requested = false;
        self.last_hold_measurement = None;
        self.best_hold_sample = None;
        self.last_assessment = None;
    }

    fn advance_after_commit(&mut self) {
        self.current_step = self.current_step.saturating_add(1);
        self.clear_hold_state();
        if self.current_step >= self.plan.len() {
            self.state = GuidedCaptureState::Complete;
        }
    }

    fn reset_hold(&mut self) {
        self.hold_frames = 0;
        self.capture_requested = false;
        self.last_hold_measurement = None;
        self.best_hold_sample = None;
    }
}

fn standard_guided_pose_plan(
    board: BoardSpec,
    initial_intrinsics: &InitialIntrinsics,
    image_size: CalibrationImageSize,
) -> Result<Vec<GuidedPoseTarget>, String> {
    const FAR_TILTED: f64 = 0.56;
    const MID: f64 = 0.64;
    const NEAR: f64 = 0.72;
    const FAR_MIDDLE: f64 = 0.42;
    const CLOSE_CORNER: f64 = 0.68;
    const OUTER_CORNER: f64 = 0.64;
    const LOW_MIDDLE: f64 = 0.56;
    const LOW_EDGE: f64 = 0.64;
    const LOW_TILT: f64 = 12.0;
    const MID_TILT: f64 = 20.0;
    const HIGH_TILT: f64 = 28.0;

    let tolerance = GuidedPoseTolerance::default();
    let mut plan = Vec::with_capacity(45);
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

    push("Center · mid · flat", [0.50, 0.50], MID, 0.0, 0.0)?;
    push(
        "Lower right · mid tilt",
        [0.578, 0.578],
        NEAR,
        MID_TILT,
        315.0,
    )?;
    push("Right · mid tilt", [0.60, 0.50], NEAR, MID_TILT, 0.0)?;
    push(
        "Upper right · mid tilt",
        [0.578, 0.422],
        NEAR,
        MID_TILT,
        45.0,
    )?;
    push(
        "Upper right corner · low tilt",
        [0.62, 0.38],
        LOW_EDGE,
        LOW_TILT,
        45.0,
    )?;
    push(
        "Upper right · low tilt",
        [0.57, 0.43],
        LOW_MIDDLE,
        LOW_TILT,
        45.0,
    )?;
    push(
        "Upper right · high tilt",
        [0.559, 0.441],
        FAR_TILTED,
        HIGH_TILT,
        45.0,
    )?;
    push(
        "Top · high tilt",
        [0.50, 0.425],
        FAR_TILTED,
        HIGH_TILT,
        90.0,
    )?;
    push("Top · low tilt", [0.50, 0.41], LOW_MIDDLE, LOW_TILT, 90.0)?;
    push(
        "Top edge · low tilt",
        [0.50, 0.34],
        LOW_EDGE,
        LOW_TILT,
        90.0,
    )?;
    push("Top · mid tilt", [0.50, 0.40], NEAR, MID_TILT, 90.0)?;
    push(
        "Upper left · mid tilt",
        [0.422, 0.422],
        NEAR,
        MID_TILT,
        135.0,
    )?;
    push("Left · mid tilt", [0.40, 0.50], NEAR, MID_TILT, 180.0)?;
    push(
        "Lower left · mid tilt",
        [0.422, 0.578],
        NEAR,
        MID_TILT,
        225.0,
    )?;
    push("Bottom · mid tilt", [0.50, 0.60], NEAR, MID_TILT, 270.0)?;
    push(
        "Bottom edge · low tilt",
        [0.50, 0.66],
        LOW_EDGE,
        LOW_TILT,
        270.0,
    )?;
    push(
        "Bottom · low tilt",
        [0.50, 0.59],
        LOW_MIDDLE,
        LOW_TILT,
        270.0,
    )?;
    push(
        "Bottom · high tilt",
        [0.50, 0.575],
        FAR_TILTED,
        HIGH_TILT,
        270.0,
    )?;
    push(
        "Lower left · high tilt",
        [0.441, 0.559],
        FAR_TILTED,
        HIGH_TILT,
        225.0,
    )?;
    push(
        "Lower left · low tilt",
        [0.43, 0.57],
        LOW_MIDDLE,
        LOW_TILT,
        225.0,
    )?;
    push("Left · low tilt", [0.41, 0.50], LOW_MIDDLE, LOW_TILT, 180.0)?;
    push(
        "Left · high tilt",
        [0.425, 0.50],
        FAR_TILTED,
        HIGH_TILT,
        180.0,
    )?;
    push(
        "Lower left corner · low tilt",
        [0.38, 0.62],
        LOW_EDGE,
        LOW_TILT,
        225.0,
    )?;
    push(
        "Close lower left corner · fronto",
        [0.30, 0.70],
        CLOSE_CORNER,
        0.0,
        0.0,
    )?;
    push(
        "Outer lower left corner · fronto",
        [0.28, 0.72],
        OUTER_CORNER,
        0.0,
        0.0,
    )?;
    push(
        "Left edge · low tilt",
        [0.34, 0.50],
        LOW_EDGE,
        LOW_TILT,
        180.0,
    )?;
    push(
        "Outer upper left corner · fronto",
        [0.28, 0.28],
        OUTER_CORNER,
        0.0,
        0.0,
    )?;
    push(
        "Close upper left corner · fronto",
        [0.30, 0.30],
        CLOSE_CORNER,
        0.0,
        0.0,
    )?;
    push(
        "Upper left corner · low tilt",
        [0.38, 0.38],
        LOW_EDGE,
        LOW_TILT,
        135.0,
    )?;
    push(
        "Upper left · low tilt",
        [0.43, 0.43],
        LOW_MIDDLE,
        LOW_TILT,
        135.0,
    )?;
    push(
        "Upper left · high tilt",
        [0.441, 0.441],
        FAR_TILTED,
        HIGH_TILT,
        135.0,
    )?;
    push(
        "Far left field · fronto",
        [0.37, 0.50],
        FAR_MIDDLE,
        0.0,
        0.0,
    )?;
    push(
        "Far bottom field · fronto",
        [0.50, 0.63],
        FAR_MIDDLE,
        0.0,
        0.0,
    )?;
    push("Far top field · fronto", [0.50, 0.37], FAR_MIDDLE, 0.0, 0.0)?;
    push(
        "Far right field · fronto",
        [0.63, 0.50],
        FAR_MIDDLE,
        0.0,
        0.0,
    )?;
    push("Right · low tilt", [0.59, 0.50], LOW_MIDDLE, LOW_TILT, 0.0)?;
    push(
        "Right · high tilt",
        [0.575, 0.50],
        FAR_TILTED,
        HIGH_TILT,
        0.0,
    )?;
    push(
        "Lower right · high tilt",
        [0.559, 0.559],
        FAR_TILTED,
        HIGH_TILT,
        315.0,
    )?;
    push(
        "Lower right · low tilt",
        [0.57, 0.57],
        LOW_MIDDLE,
        LOW_TILT,
        315.0,
    )?;
    push(
        "Lower right corner · low tilt",
        [0.62, 0.62],
        LOW_EDGE,
        LOW_TILT,
        315.0,
    )?;
    push(
        "Close lower right corner · fronto",
        [0.70, 0.70],
        CLOSE_CORNER,
        0.0,
        0.0,
    )?;
    push(
        "Outer lower right corner · fronto",
        [0.72, 0.72],
        OUTER_CORNER,
        0.0,
        0.0,
    )?;
    push(
        "Right edge · low tilt",
        [0.66, 0.50],
        LOW_EDGE,
        LOW_TILT,
        0.0,
    )?;
    push(
        "Close upper right corner · fronto",
        [0.70, 0.30],
        CLOSE_CORNER,
        0.0,
        0.0,
    )?;
    push(
        "Outer upper right corner · fronto",
        [0.72, 0.28],
        OUTER_CORNER,
        0.0,
        0.0,
    )?;
    Ok(plan)
}

struct GuidedPoseGridProjection {
    pose: GuidedPose6Dof,
    outline_uv: [[f32; 2]; 4],
    grid_lines: Arc<[ViewerGuidedPoseGridLine]>,
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
        grid_lines.push(ViewerGuidedPoseGridLine {
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
        grid_lines.push(ViewerGuidedPoseGridLine {
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
        grid_lines: Arc::from(grid_lines),
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
    // 操作提示采用使用者视角：roll=视线/光轴，pitch=点头抬头横轴，yaw=重力/竖直轴。
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
    // 普通棋盘没有方向标记，OpenCV/PnP 可能返回绕棋盘法线 180° 翻转的等价坐标系；
    // 物理姿态接近时不能把这个不可观测翻转记成 180° yaw error。
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
    detection: &ChessboardDetection,
    pnp_observation: &PnPObservation,
    board: BoardSpec,
    initial_intrinsics: &InitialIntrinsics,
    image_size: CalibrationImageSize,
) -> Result<GuidedPoseMeasurement, String> {
    if detection.corners.is_empty() {
        return Err("guided pose requires detected board corners".to_owned());
    }
    if detection.image_size != image_size {
        return Err("guided pose detection image size does not match target binding".to_owned());
    }
    for point in &detection.corners {
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err("guided pose contains non-finite board corners".to_owned());
        }
    }
    let rotation = rodrigues_matrix_for_preview(pnp_observation.rotation_vector)
        .ok_or_else(|| "guided pose rotation is not finite".to_owned())?;
    let pose = guided_pose_6dof_from_rotation_translation(
        board,
        rotation,
        pnp_observation.translation_vector,
        initial_intrinsics,
        image_size,
    )
    .ok_or_else(|| "guided pose 6DoF projection is invalid".to_owned())?;
    let measurement = GuidedPoseMeasurement {
        pose,
        board,
        initial_intrinsics: initial_intrinsics.clone(),
        image_size,
    };
    if measurement.pose.xyz[2] <= 0.0 {
        return Err("guided pose measurement contains non-positive depth".to_owned());
    }
    Ok(measurement)
}

fn assess_guided_pose(
    step_index: usize,
    target: &GuidedPoseTarget,
    detection: &ChessboardDetection,
    pnp_observation: &PnPObservation,
    board: BoardSpec,
    initial_intrinsics: &InitialIntrinsics,
    image_size: CalibrationImageSize,
) -> Result<GuidedPoseAssessment, String> {
    let measurement = guided_pose_measurement(
        detection,
        pnp_observation,
        board,
        initial_intrinsics,
        image_size,
    )?;
    let depth_scale = target.pose.xyz[2]
        .abs()
        .max(measurement.pose.xyz[2].abs())
        .max(board.square_size.max(1.0));
    let signed_rotation_error_degrees =
        guided_pose_rotation_error_degrees(&measurement.pose, &target.pose, target.tolerance)
            .ok_or_else(|| "guided pose rotation error is not finite".to_owned())?;
    let [
        signed_roll_degrees,
        signed_pitch_degrees,
        signed_yaw_degrees,
    ] = signed_rotation_error_degrees;
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
    let reason = if matched {
        None
    } else {
        Some(guided_pose_error_reason(&error, target))
    };
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

fn guided_pose_instruction_overlay(
    assessment: &GuidedPoseAssessment,
    target: &GuidedPoseTarget,
    hold_frames: u8,
) -> ViewerGuidedPoseInstructionOverlay {
    if assessment.matched {
        return ViewerGuidedPoseInstructionOverlay {
            primary: "HOLD STILL",
            secondary: format!(
                "locked · hold {}/{} · pose error {:.2}/{:.2}",
                hold_frames.min(GUIDED_CAPTURE_HOLD_FRAMES),
                GUIDED_CAPTURE_HOLD_FRAMES,
                assessment.pose_error_score,
                GUIDED_POSE_MATCH_SCORE_LIMIT
            ),
            score: assessment.pose_error_score,
            matched: true,
        };
    }

    let candidates = [
        (
            GuidedPoseInstructionComponent::X,
            assessment.error.x,
            target.tolerance.x,
            "x",
            "",
        ),
        (
            GuidedPoseInstructionComponent::Y,
            assessment.error.y,
            target.tolerance.y,
            "y",
            "",
        ),
        (
            GuidedPoseInstructionComponent::Z,
            assessment.error.z,
            target.tolerance.z,
            "z",
            "",
        ),
        (
            GuidedPoseInstructionComponent::Roll,
            assessment.error.roll_degrees,
            target.tolerance.roll_degrees,
            "roll",
            "°",
        ),
        (
            GuidedPoseInstructionComponent::Pitch,
            assessment.error.pitch_degrees,
            target.tolerance.pitch_degrees,
            "pitch",
            "°",
        ),
        (
            GuidedPoseInstructionComponent::Yaw,
            assessment.error.yaw_degrees,
            target.tolerance.yaw_degrees,
            "yaw",
            "°",
        ),
    ];
    let (component, actual, limit, label, unit, component_score) = candidates
        .into_iter()
        .map(|(component, actual, limit, label, unit)| {
            (component, actual, limit, label, unit, actual / limit)
        })
        .max_by(|left, right| left.5.total_cmp(&right.5))
        .unwrap_or((
            GuidedPoseInstructionComponent::Z,
            assessment.error.z,
            target.tolerance.z,
            "pose",
            "",
            assessment.pose_error_score,
        ));
    let primary = guided_pose_instruction_primary(component, assessment, target);
    let secondary = if unit.is_empty() {
        format!(
            "{label} {:.0}% of limit · pose error {:.2}/{:.2}",
            component_score * 100.0,
            assessment.pose_error_score,
            GUIDED_POSE_MATCH_SCORE_LIMIT
        )
    } else {
        format!(
            "{label} {actual:.1}{unit}/{limit:.1}{unit} · pose error {:.2}/{:.2}",
            assessment.pose_error_score, GUIDED_POSE_MATCH_SCORE_LIMIT
        )
    };
    ViewerGuidedPoseInstructionOverlay {
        primary,
        secondary,
        score: assessment.pose_error_score,
        matched: false,
    }
}

fn guided_pose_instruction_primary(
    component: GuidedPoseInstructionComponent,
    assessment: &GuidedPoseAssessment,
    target: &GuidedPoseTarget,
) -> &'static str {
    match component {
        GuidedPoseInstructionComponent::X => {
            if target.pose.center_uv[0] >= assessment.measurement.pose.center_uv[0] {
                "MOVE BOARD RIGHT"
            } else {
                "MOVE BOARD LEFT"
            }
        }
        GuidedPoseInstructionComponent::Y => {
            if target.pose.center_uv[1] >= assessment.measurement.pose.center_uv[1] {
                "MOVE BOARD DOWN"
            } else {
                "MOVE BOARD UP"
            }
        }
        GuidedPoseInstructionComponent::Z => {
            if target.pose.xyz[2] >= assessment.measurement.pose.xyz[2] {
                "MOVE BOARD FARTHER"
            } else {
                "MOVE BOARD CLOSER"
            }
        }
        GuidedPoseInstructionComponent::Roll => "ROLL BOARD INTO GHOST",
        GuidedPoseInstructionComponent::Pitch => "TILT BOARD INTO GHOST",
        GuidedPoseInstructionComponent::Yaw => "ROTATE BOARD INTO GHOST",
    }
}

const GUIDED_POSE_RING_SEGMENTS: usize = 96;
const GUIDED_POSE_HALF_RING_SEGMENTS: usize = 48;
const GUIDED_POSE_RING_SMALL_ERROR_GAIN: f32 = 3.0;
const GUIDED_POSE_RING_SMALL_ERROR_DECAY_DEGREES: f32 = 8.0;

#[derive(Clone, Copy, Debug)]
enum GuidedPoseRotationRingPlane {
    RollXy,
    PitchYzNegativeZ,
    YawXzNegativeZ,
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
) -> Arc<[[f32; 2]]> {
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
    Arc::from(points)
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
    label: &'static str,
    error_degrees: f64,
    tolerance_degrees: f64,
    plane: GuidedPoseRotationRingPlane,
    base_start_angle: f32,
    base_sweep: f32,
    arc_start_angle: f32,
    arc_sweep_limit: f32,
) -> ViewerGuidedPoseRotationArcOverlay {
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
        Arc::from([])
    };
    ViewerGuidedPoseRotationArcOverlay {
        label,
        error_degrees,
        tolerance_degrees,
        base_uv: guided_pose_project_rotation_ring_points(
            measurement,
            plane,
            base_start_angle,
            base_sweep,
            base_segments,
        ),
        arc_uv,
        tick_uv: guided_pose_project_rotation_ring_point(measurement, plane, arc_start_angle),
        label_uv: guided_pose_project_rotation_ring_point(
            measurement,
            plane,
            arc_start_angle + visual_sweep,
        ),
    }
}

fn guided_pose_rotation_rings_overlay(
    assessment: &GuidedPoseAssessment,
    target: &GuidedPoseTarget,
) -> ViewerGuidedPoseRotationRingsOverlay {
    let [roll, pitch, yaw] = assessment.signed_rotation_error_degrees;
    let measurement = &assessment.measurement;
    ViewerGuidedPoseRotationRingsOverlay {
        center_uv: measurement.pose.center_uv,
        roll: guided_pose_rotation_arc_overlay(
            measurement,
            "ROLL",
            roll,
            target.tolerance.roll_degrees,
            GuidedPoseRotationRingPlane::RollXy,
            0.0,
            std::f32::consts::TAU,
            -90.0_f32.to_radians(),
            std::f32::consts::PI,
        ),
        pitch: guided_pose_rotation_arc_overlay(
            measurement,
            "PITCH",
            pitch,
            target.tolerance.pitch_degrees,
            GuidedPoseRotationRingPlane::PitchYzNegativeZ,
            0.0,
            std::f32::consts::PI,
            90.0_f32.to_radians(),
            90.0_f32.to_radians(),
        ),
        yaw: guided_pose_rotation_arc_overlay(
            measurement,
            "YAW",
            yaw,
            target.tolerance.yaw_degrees,
            GuidedPoseRotationRingPlane::YawXzNegativeZ,
            0.0,
            std::f32::consts::PI,
            90.0_f32.to_radians(),
            90.0_f32.to_radians(),
        ),
    }
}

fn guided_pose_error_reason(error: &GuidedPoseError, target: &GuidedPoseTarget) -> String {
    let x_score = error.x / target.tolerance.x;
    let y_score = error.y / target.tolerance.y;
    let z_score = error.z / target.tolerance.z;
    let roll_score = error.roll_degrees / target.tolerance.roll_degrees;
    let pitch_score = error.pitch_degrees / target.tolerance.pitch_degrees;
    let yaw_score = error.yaw_degrees / target.tolerance.yaw_degrees;
    let (label, actual, limit, unit) = [
        ("x", error.x, target.tolerance.x, ""),
        ("y", error.y, target.tolerance.y, ""),
        ("z", error.z, target.tolerance.z, ""),
        (
            "roll",
            error.roll_degrees,
            target.tolerance.roll_degrees,
            "°",
        ),
        (
            "pitch",
            error.pitch_degrees,
            target.tolerance.pitch_degrees,
            "°",
        ),
        ("yaw", error.yaw_degrees, target.tolerance.yaw_degrees, "°"),
    ]
    .into_iter()
    .zip([
        x_score,
        y_score,
        z_score,
        roll_score,
        pitch_score,
        yaw_score,
    ])
    .max_by(|(_, left), (_, right)| left.total_cmp(right))
    .map(|(component, _)| component)
    .unwrap_or(("pose", 0.0, 0.0, ""));
    format!("{label} error {actual:.3}{unit} exceeds {limit:.3}{unit}")
}

fn signed_angle_distance_degrees(left: f64, right: f64) -> f64 {
    let delta = (left - right).rem_euclid(360.0);
    if delta > 180.0 { delta - 360.0 } else { delta }
}

fn candidate_operation_label(intent: CandidateIntent) -> &'static str {
    match intent {
        CandidateIntent::PreviewOnly => "Board preview",
        CandidateIntent::AutoCommit => "Automatic capture",
        CandidateIntent::GuidedMeasure => "Guided pose measurement",
        CandidateIntent::GuidedCapture => "Guided capture",
    }
}

struct PendingCandidate {
    token: AutoCandidateToken,
    intent: CandidateIntent,
    source: CalibrationSource,
    encoded: Option<CalibrationEncodedPng>,
    cancellation: CalibrationCancellation,
    pose_request: Option<PoseEstimationRequest>,
    guided_step_index: Option<usize>,
    state: AutoCandidateState,
}

#[derive(Default)]
struct AutoCaptureSession {
    pending: VecDeque<PendingCandidate>,
    completed: Vec<(AutoCandidateId, CandidateTerminal)>,
    last_observed: Option<StreamCaptureId>,
    last_observed_at_ns: u64,
    latest_detection: Option<IdentityBoundDetection>,
    last_dataset_overlay: Option<DatasetDetectionOverlay>,
    last_accepted_at_ns: u64,
    next_candidate_id: u64,
    last_assessment: Option<AutoAdmissionAssessment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LiveAdmissionContext {
    source: LiveStreamSource,
    acquisition_key: AutoCaptureAcquisitionKey,
    image_size: CalibrationImageSize,
}

enum CandidateTerminal {
    Detection(Result<DetectionProduct, PipelineStageError>),
    Discard(String),
}

struct DatasetPnpRefreshItem {
    item_id: CalibrationItemId,
    detection: ChessboardDetection,
    request: PoseEstimationRequest,
}

struct DatasetPnpRefreshBatch {
    board: BoardSpec,
    cancellation: CalibrationCancellation,
    items: Vec<DatasetPnpRefreshItem>,
}

struct DatasetPnpRefreshItemResult {
    item_id: CalibrationItemId,
    detection: ChessboardDetection,
    binding_digest: SnapshotHash,
    result: Result<PnPObservation, String>,
}

struct DatasetPnpRefreshBatchResult {
    board: BoardSpec,
    results: Vec<DatasetPnpRefreshItemResult>,
}

enum WorkerCommand {
    Calibrate {
        snapshot: CalibrationSnapshot,
        cancellation: CalibrationCancellation,
    },
    RefreshDatasetPnp(DatasetPnpRefreshBatch),
    Shutdown,
}

enum WorkerEvent {
    Calibration {
        snapshot: CalibrationSnapshot,
        result: Result<CalibrationSolution, String>,
    },
    DatasetPnpRefresh(DatasetPnpRefreshBatchResult),
}

struct CalibrationWorker {
    sender: Sender<WorkerCommand>,
    receiver: Receiver<WorkerEvent>,
}

impl CalibrationWorker {
    fn new(context: &egui::Context) -> std::io::Result<Self> {
        let (command_sender, command_receiver) = mpsc::channel();
        let (event_sender, event_receiver) = mpsc::channel();
        let repaint = context.clone();
        thread::Builder::new()
            .name("camera-toolbox-calibration-solve".to_owned())
            .spawn(move || {
                let backend = OpenCvCalibrationBackend;
                while let Ok(command) = command_receiver.recv() {
                    let event = match command {
                        WorkerCommand::Calibrate {
                            snapshot,
                            cancellation,
                        } => {
                            let result = backend
                                .calibrate(&snapshot.request, &cancellation)
                                .map_err(|error| error.to_string());
                            WorkerEvent::Calibration { snapshot, result }
                        }
                        WorkerCommand::RefreshDatasetPnp(batch) => {
                            WorkerEvent::DatasetPnpRefresh(run_dataset_pnp_refresh(&backend, batch))
                        }
                        WorkerCommand::Shutdown => break,
                    };
                    if event_sender.send(event).is_err() {
                        break;
                    }
                    repaint.request_repaint();
                }
            })?;
        Ok(Self {
            sender: command_sender,
            receiver: event_receiver,
        })
    }

    fn send(&self, command: WorkerCommand) -> Result<(), String> {
        self.sender
            .send(command)
            .map_err(|_| "calibration worker stopped".to_owned())
    }
}

impl Drop for CalibrationWorker {
    fn drop(&mut self) {
        let _ = self.sender.send(WorkerCommand::Shutdown);
    }
}

fn run_dataset_pnp_refresh(
    backend: &dyn CalibrationBackend,
    batch: DatasetPnpRefreshBatch,
) -> DatasetPnpRefreshBatchResult {
    let DatasetPnpRefreshBatch {
        board,
        cancellation,
        items,
    } = batch;
    let results = items
        .into_iter()
        .map(|item| {
            let binding_digest = item.request.binding_digest.clone();
            let result = backend
                .estimate_pose(
                    &item.detection,
                    &item.request.initial_intrinsics,
                    board,
                    &cancellation,
                )
                .map_err(|error| error.to_string())
                .and_then(|pose| {
                    PnPObservation::from_view_result(binding_digest.clone(), pose, board)
                        .map_err(|error| error.to_string())
                });
            DatasetPnpRefreshItemResult {
                item_id: item.item_id,
                detection: item.detection,
                binding_digest,
                result,
            }
        })
        .collect();
    DatasetPnpRefreshBatchResult { board, results }
}

#[derive(Clone, Debug)]
struct CalibrationSnidDraft {
    module: YgStereoModuleCode,
    year: String,
    month: String,
    day: String,
    optical_axis_class: u8,
    sequence: String,
}

impl Default for CalibrationSnidDraft {
    fn default() -> Self {
        Self {
            module: YgStereoModuleCode::Model233,
            year: String::new(),
            month: String::new(),
            day: String::new(),
            optical_axis_class: 0,
            sequence: "1".to_owned(),
        }
    }
}

impl CalibrationSnidDraft {
    /// 将 GUI 文本字段转换为 EEPROM SNID；错误直接作为写入禁用原因展示。
    fn serial_number(&self) -> Result<String, String> {
        let input = YgStereoSerialIdInput::new(
            self.module,
            parse_two_digit_year(&self.year)?,
            parse_decimal_field("Month", &self.month)?,
            parse_decimal_field("Day", &self.day)?,
            self.optical_axis_class,
            parse_decimal_field("Sequence", &self.sequence)?,
        );
        input.serial_number().map_err(|error| error.to_string())
    }
}

fn parse_two_digit_year(text: &str) -> Result<u16, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("Year is required for SNID generation.".to_owned());
    }
    if trimmed.len() != 2 || !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("Year must be exactly two decimal digits, e.g. 26.".to_owned());
    }
    trimmed
        .parse::<u16>()
        .map_err(|_| "Year must be exactly two decimal digits, e.g. 26.".to_owned())
}

fn parse_decimal_field<T>(label: &str, text: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(format!("{label} is required for SNID generation."));
    }
    trimmed
        .parse::<T>()
        .map_err(|_| format!("{label} must be a decimal number."))
}

fn optical_axis_class_label(value: u8) -> &'static str {
    match value {
        0 => "0 - unclassified",
        1 => "1 - L0",
        2 => "2 - L1",
        3 => "3 - R0",
        4 => "4 - R1",
        _ => "invalid",
    }
}

pub(crate) struct CalibrationWorkspace {
    session: CalibrationSession,
    sources: HashMap<CalibrationItemId, CalibrationSource>,
    worker: CalibrationWorker,
    detection_pipeline: CalibrationDetectionPipeline,
    pending_reads: VecDeque<ReadJob>,
    pending_dataset_loaded: VecDeque<LoadedDetectionJob>,
    pending_imports: VecDeque<CalibrationImportCandidate>,
    active_job: Option<CalibrationJobKind>,
    active_detection_batch: Option<DetectionBatch>,
    calibration_cancellation: Option<CalibrationCancellation>,
    auto_capture: AutoCaptureSession,
    next_detection_batch_id: u64,
    status: String,
    snid_draft: CalibrationSnidDraft,
    pending_export: Option<CalibrationExport>,
    loaded_result: Option<LoadedCalibrationResult>,
    eeprom: CalibrationEepromState,
    board_cols: u16,
    board_rows: u16,
    square_size: f64,
    auto_intrinsics: bool,
    fx: f64,
    fy: f64,
    cx: f64,
    cy: f64,
    initial_distortion_coefficients: [f64; 12],
    intrinsics_value_editing: bool,
    pending_dataset_pnp_refresh: bool,
    display_layer: CalibrationDisplayLayer,
    preview_viewport: CalibrationPreviewViewport,
    preview_mode: CalibrationPreviewMode,
    pts_bridge_cache: BTreeMap<RtspPtsBridgeKey, RtspPtsBridgeSample>,
    coverage: Option<CoverageVisualization>,
    coverage_dirty: bool,
    auto_capture_enabled: bool,
    auto_capture_trigger_mode: AutoCaptureTriggerMode,
    guided_capture: Option<GuidedCaptureRuntime>,
    dataset_sidebar_expanded: bool,
    dataset_acceptance_expanded: bool,
    dataset_table_expanded: bool,
    dataset_split_ratio: f32,
    acceptance_draft: DatasetAcceptanceDraft,
    acceptance_last_valid_criteria: AutoCaptureAcceptanceCriteria,
    live_admission_context: Option<LiveAdmissionContext>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CalibrationWorkspaceKey(String);

impl CalibrationWorkspaceKey {
    fn manual() -> Self {
        Self("manual".to_owned())
    }

    fn for_live_source(source: &LiveStreamSource) -> Self {
        match source {
            LiveStreamSource::Cv610 {
                profile_id,
                channel,
                source_fingerprint,
                geometry_key,
                ..
            } => Self(format!(
                "cv610:{profile_id}:ch{channel}:{source_fingerprint}:{geometry_key}"
            )),
            LiveStreamSource::Rtsp {
                channel,
                source_fingerprint,
                geometry_key,
                ..
            } => Self(format!(
                "rtsp:ch{channel}:{source_fingerprint}:{geometry_key}"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CalibrationWorkspaceKind {
    ManualFiles,
    LiveStream,
}

impl CalibrationWorkspaceKind {
    const fn allows_live_inspection(self) -> bool {
        matches!(self, Self::LiveStream)
    }
}

struct CalibrationWorkspaceEntry {
    kind: CalibrationWorkspaceKind,
    label: String,
    workspace: CalibrationWorkspace,
}

/// 多路内参标定编排器；每个 live source 独立持有 Dataset、自动采集和求解状态。
pub(crate) struct CalibrationWorkspaceManager {
    context: egui::Context,
    active: CalibrationWorkspaceKey,
    entries: BTreeMap<CalibrationWorkspaceKey, CalibrationWorkspaceEntry>,
}

impl CalibrationWorkspaceManager {
    pub(crate) fn new(context: &egui::Context) -> std::io::Result<Self> {
        let manual = CalibrationWorkspaceKey::manual();
        let mut entries = BTreeMap::new();
        entries.insert(
            manual.clone(),
            CalibrationWorkspaceEntry {
                kind: CalibrationWorkspaceKind::ManualFiles,
                label: "Manual / Files".to_owned(),
                workspace: CalibrationWorkspace::new(context)?,
            },
        );
        Ok(Self {
            context: context.clone(),
            active: manual,
            entries,
        })
    }

    fn active_workspace(&self) -> &CalibrationWorkspace {
        &self
            .entries
            .get(&self.active)
            .expect("active calibration workspace exists")
            .workspace
    }

    fn active_workspace_mut(&mut self) -> &mut CalibrationWorkspace {
        &mut self
            .entries
            .get_mut(&self.active)
            .expect("active calibration workspace exists")
            .workspace
    }

    #[cfg(test)]
    pub(crate) fn workspace_count_for_test(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn active_label_for_test(&self) -> &str {
        self.entries
            .get(&self.active)
            .map(|entry| entry.label.as_str())
            .expect("active calibration workspace exists")
    }
    fn manual_workspace_mut(&mut self) -> &mut CalibrationWorkspace {
        &mut self
            .entries
            .get_mut(&CalibrationWorkspaceKey::manual())
            .expect("manual calibration workspace exists")
            .workspace
    }

    fn activate_manual_workspace(&mut self) {
        self.active = CalibrationWorkspaceKey::manual();
    }

    fn live_source_label(source: &LiveStreamSource) -> String {
        match source {
            LiveStreamSource::Cv610 {
                profile_label,
                channel,
                ..
            } => format!("{profile_label} CH{channel}"),
            LiveStreamSource::Rtsp { label, channel, .. } => format!("{label} CH{channel}"),
        }
    }

    fn ensure_live_workspace(
        &mut self,
        source: &LiveStreamSource,
    ) -> Option<&mut CalibrationWorkspace> {
        let key = CalibrationWorkspaceKey::for_live_source(source);
        if !self.entries.contains_key(&key) {
            let label = Self::live_source_label(source);
            let workspace = match CalibrationWorkspace::new(&self.context) {
                Ok(workspace) => workspace,
                Err(error) => {
                    self.active_workspace_mut().status =
                        format!("Cannot create calibration session for {label}: {error}");
                    return None;
                }
            };
            self.entries.insert(
                key.clone(),
                CalibrationWorkspaceEntry {
                    kind: CalibrationWorkspaceKind::LiveStream,
                    label,
                    workspace,
                },
            );
        }
        self.entries.get_mut(&key).map(|entry| &mut entry.workspace)
    }

    fn workspace_for_live_source(
        &self,
        source: &LiveStreamSource,
    ) -> Option<&CalibrationWorkspace> {
        let key = CalibrationWorkspaceKey::for_live_source(source);
        self.entries.get(&key).map(|entry| &entry.workspace)
    }

    fn activate_live_source(&mut self, source: &LiveStreamSource) {
        self.active = CalibrationWorkspaceKey::for_live_source(source);
    }
    fn close_session(&mut self, key: &CalibrationWorkspaceKey) -> bool {
        if *key == CalibrationWorkspaceKey::manual() {
            return false;
        }
        if self.entries.remove(key).is_none() {
            return false;
        }
        if self.active == *key {
            self.activate_manual_workspace();
        }
        true
    }

    pub(crate) fn activate_live_source_session(&mut self, source: &LiveStreamSource) {
        if self.ensure_live_workspace(source).is_some() {
            self.activate_live_source(source);
        }
    }

    pub(crate) fn import(&mut self, candidates: Vec<CalibrationImportCandidate>) {
        self.activate_manual_workspace();
        self.manual_workspace_mut().import(candidates);
    }

    pub(crate) fn reject_import(&mut self, display_path: &std::path::Path) {
        self.activate_manual_workspace();
        self.manual_workspace_mut().reject_import(display_path);
    }

    pub(crate) fn capture_displayed_stream_frame(
        &mut self,
        frame: Arc<DecodedVideoFrame>,
        live_source: LiveStreamSource,
        store: CaptureStore,
    ) {
        if self.ensure_live_workspace(&live_source).is_some() {
            self.activate_live_source(&live_source);
            self.active_workspace_mut()
                .capture_displayed_stream_frame(frame, live_source, store);
        }
    }
    pub(crate) fn active_accepts_live_source(&self, source: Option<&LiveStreamSource>) -> bool {
        let Some(source) = source else {
            return false;
        };
        let key = CalibrationWorkspaceKey::for_live_source(source);
        self.active == key
            && self
                .entries
                .get(&self.active)
                .is_some_and(|entry| entry.kind.allows_live_inspection())
    }
    #[cfg(test)]
    pub(crate) fn ensure_live_source_for_test(&mut self, source: &LiveStreamSource) {
        self.activate_live_source_session(source);
    }

    pub(crate) fn observe_live_frame(
        &mut self,
        frame: Arc<DecodedVideoFrame>,
        live_source: LiveStreamSource,
        store: CaptureStore,
        preview_requested: bool,
    ) {
        let key = CalibrationWorkspaceKey::for_live_source(&live_source);
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        entry
            .workspace
            .observe_live_frame(frame, live_source, store, preview_requested);
    }

    pub(crate) fn stream_disconnected(&mut self, session_id: &StreamSessionId) {
        for entry in self.entries.values_mut() {
            entry.workspace.stream_disconnected(session_id);
        }
    }

    pub(crate) fn take_export(&mut self) -> Option<CalibrationExport> {
        if let Some(export) = self.active_workspace_mut().take_export() {
            return Some(export);
        }
        for entry in self.entries.values_mut() {
            if let Some(export) = entry.workspace.take_export() {
                return Some(export);
            }
        }
        None
    }

    pub(crate) fn take_provision_intent(&mut self) -> Option<CalibrationProvisionIntent> {
        if let Some(intent) = self.active_workspace_mut().take_provision_intent() {
            return Some(intent);
        }
        for entry in self.entries.values_mut() {
            if let Some(intent) = entry.workspace.take_provision_intent() {
                return Some(intent);
            }
        }
        None
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_target_configured(&mut self, label: &str) {
        self.active_workspace_mut().report_target_configured(label);
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_target_configuration_failed(&mut self, message: impl Into<String>) {
        self.active_workspace_mut()
            .report_target_configuration_failed(message);
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_target_invalidated(&mut self, message: impl Into<String>) {
        self.active_workspace_mut()
            .report_target_invalidated(message);
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_bus_discovery_failed(&mut self, message: impl Into<String>) {
        self.active_workspace_mut()
            .report_bus_discovery_failed(message);
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_bus_discovery(&mut self, buses: Vec<camera_toolbox_app::I2cBusInfo>) {
        self.active_workspace_mut().report_bus_discovery(buses);
    }

    pub(crate) fn report_provision_error(&mut self, message: impl Into<String>) {
        self.active_workspace_mut().report_provision_error(message);
    }

    pub(crate) fn report_eeprom_provision_unknown(&mut self, message: impl Into<String>) {
        self.active_workspace_mut()
            .report_eeprom_provision_unknown(message);
    }

    pub(crate) fn report_eeprom_inspect(
        &mut self,
        target_label: String,
        result: EepromInspectResult,
    ) {
        self.active_workspace_mut()
            .report_eeprom_inspect(target_label, result);
    }

    pub(crate) fn report_eeprom_provision(
        &mut self,
        target_label: String,
        result: &EepromWriteResult,
        audit_file: String,
    ) {
        self.active_workspace_mut()
            .report_eeprom_provision(target_label, result, audit_file);
    }

    pub(crate) fn report_eeprom_provision_audit_error(
        &mut self,
        target_label: String,
        result: &EepromWriteResult,
        error: &str,
    ) {
        self.active_workspace_mut()
            .report_eeprom_provision_audit_error(target_label, result, error);
    }

    pub(crate) fn report_export_started(&mut self, label: &str, target_label: &str) {
        self.active_workspace_mut()
            .report_export_started(label, target_label);
    }

    pub(crate) fn report_export_finished(
        &mut self,
        label: &str,
        target_label: &str,
        result: Result<u64, &str>,
    ) {
        self.active_workspace_mut()
            .report_export_finished(label, target_label, result);
    }

    pub(crate) fn tick(&mut self, context: &egui::Context) {
        for entry in self.entries.values_mut() {
            entry.workspace.tick(context);
        }
    }

    pub(crate) fn live_viewer_presentation(
        &self,
        live_frame: Option<&DecodedVideoFrame>,
        live_source: Option<&LiveStreamSource>,
    ) -> Option<CalibrationViewerPresentation> {
        if let Some(source) = live_source {
            return self
                .workspace_for_live_source(source)
                .and_then(|workspace| workspace.live_viewer_presentation(live_frame, live_source));
        }
        self.active_workspace()
            .live_viewer_presentation(live_frame, live_source)
    }

    pub(crate) fn render(
        &mut self,
        context: &egui::Context,
        ui: &mut egui::Ui,
        export_enabled: bool,
        export_reason: Option<&str>,
        sftp_source: Result<&str, &str>,
        provision_target: Result<&str, &str>,
        has_live_inspection: bool,
        render_live_inspection: impl FnMut(&mut egui::Ui) -> Option<Arc<DecodedVideoFrame>>,
    ) -> (egui::Rect, Option<Arc<DecodedVideoFrame>>) {
        let tabs: Vec<_> = self
            .entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.label.clone(), entry.kind))
            .collect();
        if tabs.len() > 1 {
            let mut close_key = None;
            ui.horizontal_wrapped(|ui| {
                ui.label("Calibration session:");
                for (key, label, kind) in tabs {
                    if ui.selectable_label(self.active == key, &label).clicked() {
                        self.active = key.clone();
                    }
                    if self.active == key
                        && kind.allows_live_inspection()
                        && ui
                            .small_button("Close session")
                            .on_hover_text("Remove this Calibration session; the RTSP live document stays open.")
                            .clicked()
                    {
                        close_key = Some(key);
                    }
                }
            });
            if let Some(key) = close_key {
                self.close_session(&key);
            }
            ui.separator();
        }
        let has_live_inspection = has_live_inspection
            && self
                .entries
                .get(&self.active)
                .is_some_and(|entry| entry.kind.allows_live_inspection());
        self.active_workspace_mut().render(
            context,
            ui,
            export_enabled,
            export_reason,
            sftp_source,
            provision_target,
            has_live_inspection,
            render_live_inspection,
        )
    }

    pub(crate) fn render_status(&self, ui: &mut egui::Ui) {
        if self.entries.len() > 1
            && let Some(entry) = self.entries.get(&self.active)
        {
            ui.label(format!("{}:", entry.label));
        }
        self.active_workspace().render_status(ui);
    }
}

impl CalibrationWorkspace {
    pub(crate) fn new(context: &egui::Context) -> std::io::Result<Self> {
        let board = BoardSpec::new(11, 8, 40.0).expect("default board is valid");
        let acceptance_draft = DatasetAcceptanceDraft::default();
        let acceptance_last_valid_criteria = acceptance_draft
            .parse()
            .expect("default Dataset Acceptance thresholds are valid");

        Ok(Self {
            session: CalibrationSession::new(board),
            sources: HashMap::new(),
            worker: CalibrationWorker::new(context)?,
            detection_pipeline: CalibrationDetectionPipeline::new(context)?,
            pending_reads: VecDeque::new(),
            pending_dataset_loaded: VecDeque::new(),
            pending_imports: VecDeque::new(),
            active_job: None,
            active_detection_batch: None,
            calibration_cancellation: None,
            next_detection_batch_id: 1,
            status: "Add original PNG calibration images from Workspace Explorer.".to_owned(),
            snid_draft: CalibrationSnidDraft::default(),
            pending_export: None,
            loaded_result: None,
            eeprom: CalibrationEepromState::default(),
            board_cols: board.inner_cols,
            board_rows: board.inner_rows,
            square_size: board.square_size,
            auto_intrinsics: true,
            fx: 900.0,
            fy: 900.0,
            cx: 0.0,
            cy: 0.0,
            initial_distortion_coefficients: ZERO_DISTORTION_COEFFICIENTS,
            intrinsics_value_editing: false,
            pending_dataset_pnp_refresh: false,
            display_layer: CalibrationDisplayLayer::default(),
            preview_viewport: CalibrationPreviewViewport::default(),
            preview_mode: CalibrationPreviewMode::default(),
            pts_bridge_cache: BTreeMap::new(),
            coverage: None,
            coverage_dirty: true,
            auto_capture_enabled: false,
            auto_capture_trigger_mode: AutoCaptureTriggerMode::default(),
            guided_capture: None,
            dataset_sidebar_expanded: true,
            dataset_acceptance_expanded: true,
            dataset_table_expanded: true,
            dataset_split_ratio: 0.5,
            acceptance_draft,
            acceptance_last_valid_criteria,
            live_admission_context: None,
            auto_capture: AutoCaptureSession {
                next_candidate_id: 1,
                ..AutoCaptureSession::default()
            },
        })
    }

    fn load_calibration_result_from_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Calibration Result YAML", &["yaml", "yml"])
            .pick_file()
        else {
            return;
        };
        let source = path.display().to_string();
        match std::fs::read_to_string(&path) {
            Ok(yaml) => self.load_calibration_result_from_yaml_str(&yaml, &source),
            Err(error) => {
                self.loaded_result = None;
                self.status =
                    format!("Failed to read Calibration Result YAML from {source}: {error}");
            }
        }
    }

    fn load_calibration_result_from_yaml_str(&mut self, yaml: &str, source: &str) {
        match parse_opencv_pinhole_radtan_yaml(yaml) {
            Ok(solution) => {
                let width = solution.image_size.width;
                let height = solution.image_size.height;
                self.loaded_result = Some(LoadedCalibrationResult {
                    source: source.to_owned(),
                    solution,
                });
                self.status = format!(
                    "Loaded Calibration Result YAML from {source} ({width}×{height}); EEPROM writes can use it without Calibrate."
                );
            }
            Err(error) => {
                self.loaded_result = None;
                self.status = format!("Calibration Result YAML from {source} is invalid: {error}");
            }
        }
    }

    fn active_calibration_solution(&self) -> Option<&CalibrationSolution> {
        self.loaded_result
            .as_ref()
            .map(|loaded| &loaded.solution)
            .or_else(|| {
                self.session
                    .installed()
                    .map(|installed| &installed.solution)
            })
    }

    pub(crate) fn import(&mut self, candidates: Vec<CalibrationImportCandidate>) {
        match self.active_job {
            Some(CalibrationJobKind::Calibrate) => {
                self.status = "Cancel the active calibration before importing files.".to_owned();
            }
            Some(CalibrationJobKind::Detect) => {
                let queued = candidates.len();
                self.pending_imports.extend(candidates);
                self.status = format!("Queued {queued} image(s) for detection.");
            }
            Some(CalibrationJobKind::DatasetPnpRefresh) => {
                self.status = "Wait for Dataset PnP refresh before importing files.".to_owned();
            }
            None => self.import_candidates(candidates, true),
        }
    }

    fn import_candidates(
        &mut self,
        candidates: Vec<CalibrationImportCandidate>,
        auto_detect: bool,
    ) {
        let available = MAX_DATASET_ITEMS.saturating_sub(self.session.items().len());
        let offered = candidates.len();
        let mut added = 0_usize;
        let mut refreshed = 0_usize;
        let mut skipped = offered.saturating_sub(available);
        let mut detection_ids = Vec::new();

        for candidate in candidates.into_iter().take(available) {
            let name = candidate.entry.name.as_str().to_owned();
            let outcome = self.session.add_or_refresh(
                candidate.entry.reference.clone(),
                candidate.entry.version,
                name,
            );
            let id = match outcome {
                AddCalibrationItemOutcome::Added(id) => {
                    added += 1;
                    id
                }
                AddCalibrationItemOutcome::SourceChanged(id) => {
                    refreshed += 1;
                    id
                }
                AddCalibrationItemOutcome::AlreadyPresent(id) => {
                    skipped += 1;
                    id
                }
            };
            self.sources.insert(
                id,
                CalibrationSource::file(
                    candidate.display_path,
                    candidate.file_system,
                    candidate.remote,
                ),
            );
            if auto_detect && !detection_ids.contains(&id) {
                detection_ids.push(id);
            }
        }
        if added > 0 || refreshed > 0 {
            self.coverage_dirty = true;
        }
        self.status =
            format!("Dataset updated: {added} added, {refreshed} refreshed, {skipped} unchanged.");
        if auto_detect && self.active_job.is_none() && !detection_ids.is_empty() {
            self.start_detection_items(detection_ids);
        }
    }

    pub(crate) fn reject_import(&mut self, display_path: &std::path::Path) {
        self.status = format!(
            "Cannot add {}: PangbotCompatible calibration accepts original PNG files only.",
            display_path.display()
        );
    }

    /// 固化当前已展示的 RTSP 帧为会话内 PNG，并直接送入检测队列。
    ///
    /// 只接受 `LiveDocument::displayed_frame` 的不可变 frame，禁止回读 live slot，
    /// 以免用户点击的画面与提交给标定的数据不一致。
    pub(crate) fn capture_displayed_stream_frame(
        &mut self,
        frame: Arc<DecodedVideoFrame>,
        live_source: LiveStreamSource,
        store: CaptureStore,
    ) {
        if self.active_job.is_some() {
            self.status =
                "Wait for the active calibration operation before capturing a stream frame."
                    .to_owned();
            return;
        }
        if self.session.items().len() >= MAX_DATASET_ITEMS {
            self.status = format!("Dataset is limited to {MAX_DATASET_ITEMS} images.");
            return;
        }

        let input = CalibrationInputKey::StreamCapture(StreamCaptureId::from(&frame.identity));
        if let Some(existing_id) = self
            .session
            .items()
            .iter()
            .find(|item| item.input == input)
            .map(|item| item.id)
        {
            let _ = self.session.set_selected(existing_id);
            self.status =
                "This displayed stream frame is already in the calibration dataset.".to_owned();
            return;
        }
        if self
            .auto_capture
            .pending
            .iter()
            .any(|candidate| candidate.token.frame_identity() == &frame.identity)
        {
            self.status =
                "This displayed stream frame is already an automatic candidate.".to_owned();
            return;
        }

        let source_acquisition_key = match live_source.acquisition_key_for_frame(&frame) {
            Ok(key) => key,
            Err(error) => {
                self.status =
                    format!("Cannot bind displayed stream frame to calibration source: {error}");
                return;
            }
        };

        let FrozenStreamInput { source, encoded } =
            match freeze_stream_input(&frame, store, source_acquisition_key.clone(), None) {
                Ok(frozen) => frozen,
                Err(error) => {
                    self.status = error;
                    return;
                }
            };
        let outcome = self.session.add_or_refresh_with_acquisition_key(
            input,
            encoded.source_revision,
            source.display_name.clone(),
            Some(source_acquisition_key),
        );
        let id = match outcome {
            AddCalibrationItemOutcome::Added(id) | AddCalibrationItemOutcome::SourceChanged(id) => {
                id
            }
            AddCalibrationItemOutcome::AlreadyPresent(id) => {
                let _ = self.session.set_selected(id);
                self.status =
                    "This displayed stream frame is already in the calibration dataset.".to_owned();
                return;
            }
        };
        self.sources.insert(id, source);
        let _ = self.session.set_selected(id);
        self.coverage_dirty = true;
        self.status =
            "Captured displayed stream frame; submitting authoritative detection.".to_owned();
        self.start_detection_items(vec![id]);
    }

    /// 观察 Viewer 已安装的不可变帧；预览和自动提交共用权威检测 worker。
    pub(crate) fn observe_live_frame(
        &mut self,
        frame: Arc<DecodedVideoFrame>,
        live_source: LiveStreamSource,
        store: CaptureStore,
        preview_requested: bool,
    ) {
        let source_acquisition_key = match self.sync_live_admission(frame.as_ref(), &live_source) {
            Ok(key) => key,
            Err(error) => {
                self.status = format!("Cannot bind live frame to auto-capture admission: {error}");
                return;
            }
        };
        let frame_image_size = match CalibrationImageSize::new(frame.width, frame.height) {
            Ok(image_size) => image_size,
            Err(error) => {
                self.status = format!("Live frame has invalid geometry: {error}");
                return;
            }
        };

        let mut guided_step_index = None;
        let intent = if self.auto_capture_enabled && self.active_live_admission() {
            match self.auto_capture_trigger_mode {
                AutoCaptureTriggerMode::DatasetGain => CandidateIntent::AutoCommit,
                AutoCaptureTriggerMode::GuidedPresetPose => {
                    let Some(runtime) = self.guided_capture.as_ref() else {
                        self.status =
                            "Guided Auto Capture is selected; press Start guided first.".to_owned();
                        return;
                    };
                    if !runtime.is_running() {
                        if preview_requested {
                            CandidateIntent::PreviewOnly
                        } else {
                            return;
                        }
                    } else if runtime.binding.source != live_source
                        || runtime.binding.acquisition_key != source_acquisition_key
                        || runtime.binding.image_size != frame_image_size
                    {
                        self.stop_guided_capture(
                            "Guided Auto Capture stopped because the live source binding changed.",
                        );
                        return;
                    } else {
                        guided_step_index = Some(runtime.current_step);
                        CandidateIntent::GuidedMeasure
                    }
                }
            }
        } else if preview_requested {
            CandidateIntent::PreviewOnly
        } else {
            return;
        };
        let commits_dataset = matches!(
            intent,
            CandidateIntent::AutoCommit | CandidateIntent::GuidedCapture
        );
        if self.active_job.is_some()
            || self.auto_capture.pending.len() >= LIVE_AUTO_CANDIDATE_CAPACITY
            || (commits_dataset && self.session.items().len() >= MAX_DATASET_ITEMS)
        {
            return;
        }
        let capture_id = StreamCaptureId::from(&frame.identity);
        let repeated_observation = self.auto_capture.last_observed.as_ref() == Some(&capture_id);
        if (repeated_observation && intent != CandidateIntent::GuidedCapture)
            || self
                .auto_capture
                .pending
                .iter()
                .any(|candidate| candidate.token.frame_identity() == &frame.identity)
            || (commits_dataset
                && self.session.items().iter().any(|item| {
                    item.input == CalibrationInputKey::StreamCapture(capture_id.clone())
                }))
        {
            return;
        }
        let now_ns = host_monotonic_time_ns();
        if (self.auto_capture.last_observed.is_some()
            && now_ns.saturating_sub(self.auto_capture.last_observed_at_ns)
                < AUTO_CAPTURE_ANALYSIS_INTERVAL_NS
            && intent != CandidateIntent::GuidedCapture)
            || (commits_dataset
                && self.auto_capture.last_accepted_at_ns != 0
                && now_ns.saturating_sub(self.auto_capture.last_accepted_at_ns)
                    < AUTO_CAPTURE_ACCEPT_COOLDOWN_NS)
        {
            return;
        }
        let pose_request = self
            .session
            .initial_intrinsics_binding()
            .filter(|binding| binding.reference_image_size == frame_image_size)
            .map(|binding| PoseEstimationRequest {
                initial_intrinsics: binding.initial_intrinsics.clone(),
                reference_image_size: binding.reference_image_size,
                binding_digest: binding.digest.clone(),
            });
        if matches!(
            intent,
            CandidateIntent::AutoCommit
                | CandidateIntent::GuidedMeasure
                | CandidateIntent::GuidedCapture
        ) && pose_request.is_none()
        {
            self.status = "Automatic capture waits for a valid current K/D12 binding.".to_owned();
            return;
        }
        self.auto_capture.last_observed = Some(capture_id);
        self.auto_capture.last_observed_at_ns = now_ns;

        let FrozenStreamInput { source, encoded } = match freeze_stream_input(
            &frame,
            store,
            source_acquisition_key.clone(),
            live_source.authoritative_capture().cloned(),
        ) {
            Ok(frozen) => frozen,
            Err(error) => {
                self.status = format!(
                    "{} rejected before detection: {error}",
                    candidate_operation_label(intent)
                );
                return;
            }
        };
        let candidate_id = AutoCandidateId::new(self.auto_capture.next_candidate_id);
        self.auto_capture.next_candidate_id =
            self.auto_capture.next_candidate_id.wrapping_add(1).max(1);
        let token = match self.session.bind_auto_candidate(
            candidate_id,
            frame.identity.clone(),
            encoded.source_revision.clone(),
            source.display_name.clone(),
            Some(source_acquisition_key),
        ) {
            Ok(token) => token,
            Err(error) => {
                self.status = format!(
                    "{} candidate rejected: {error}",
                    candidate_operation_label(intent)
                );
                return;
            }
        };
        self.auto_capture.pending.push_back(PendingCandidate {
            token,
            intent,
            source,
            encoded: Some(encoded),
            cancellation: CalibrationCancellation::default(),
            pose_request,
            guided_step_index,
            state: AutoCandidateState::Queued,
        });
        self.status = match intent {
            CandidateIntent::PreviewOnly => format!(
                "Live board preview candidate {} queued for detection.",
                candidate_id.get()
            ),
            CandidateIntent::AutoCommit => format!(
                "Automatic candidate {} queued for authoritative detection and PnP.",
                candidate_id.get()
            ),
            CandidateIntent::GuidedMeasure => format!(
                "Guided pose measurement {} queued for step {}.",
                candidate_id.get(),
                guided_step_index.map_or(0, |step| step + 1)
            ),
            CandidateIntent::GuidedCapture => format!(
                "Guided capture {} queued for step {}.",
                candidate_id.get(),
                guided_step_index.map_or(0, |step| step + 1)
            ),
        };
    }

    /// 从当前显示帧刷新 source-bound runtime admission；不读取或写入 profile 文件。
    fn sync_live_admission(
        &mut self,
        frame: &DecodedVideoFrame,
        live_source: &LiveStreamSource,
    ) -> Result<AutoCaptureAcquisitionKey, String> {
        let image_size = CalibrationImageSize::new(frame.width, frame.height)
            .map_err(|error| error.to_string())?;
        let acquisition_key = if let Some(context) = self.live_admission_context.as_ref()
            && context.source == *live_source
            && context.image_size == image_size
        {
            context.acquisition_key.clone()
        } else {
            live_source.acquisition_key_for_frame(frame)?
        };
        let context_changed = self.live_admission_context.as_ref().is_none_or(|context| {
            context.source != *live_source
                || context.image_size != image_size
                || context.acquisition_key != acquisition_key
        });
        if context_changed {
            if !self.auto_capture.pending.is_empty() {
                self.cancel_auto_candidates_matching(
                    "Live detection candidate cancelled because source or image geometry changed.",
                    |_| true,
                );
            }
            if self.guided_capture.is_some() {
                self.stop_guided_capture(
                    "Guided Auto Capture stopped because source or image geometry changed.",
                );
            }
            self.session
                .configure_auto_admission(None, None)
                .map_err(|error| error.to_string())?;
            self.live_admission_context = Some(LiveAdmissionContext {
                source: live_source.clone(),
                acquisition_key: acquisition_key.clone(),
                image_size,
            });
            self.pts_bridge_cache.clear();
            self.auto_capture.latest_detection = None;
            self.auto_capture.last_assessment = None;
        }
        self.refresh_runtime_auto_admission();
        Ok(acquisition_key)
    }

    /// 仅在所有文本阈值和当前 K/D12 都有效时，替换当前 live context 的 admission。
    fn refresh_runtime_auto_admission(&mut self) {
        let criteria = match self.acceptance_draft.parse() {
            Ok(criteria) => {
                self.acceptance_last_valid_criteria = criteria.clone();
                criteria
            }
            Err(error) => {
                self.acceptance_draft.error = Some(error);
                return;
            }
        };
        let Some(context) = self.live_admission_context.clone() else {
            self.acceptance_draft.error = None;
            return;
        };
        self.refresh_auto_intrinsics_fields();
        let initial_intrinsics = match self.initial_intrinsics_for_image(context.image_size) {
            Ok(initial_intrinsics) => initial_intrinsics,
            Err(error) => {
                self.acceptance_draft.error = Some(error);
                return;
            }
        };
        let baseline = match AutoCaptureBaseline::new(
            context.acquisition_key.clone(),
            context.image_size,
            self.session.board(),
            criteria,
        ) {
            Ok(baseline) => baseline,
            Err(error) => {
                self.acceptance_draft.error = Some(error.to_string());
                return;
            }
        };
        let binding = match InitialIntrinsicsBinding::full_frame(
            initial_intrinsics,
            context.image_size,
            context.acquisition_key,
        ) {
            Ok(binding) => binding,
            Err(error) => {
                self.acceptance_draft.error = Some(error.to_string());
                return;
            }
        };
        let binding_changed = self.session.initial_intrinsics_binding() != Some(&binding);
        let reconfigure =
            self.session.auto_capture_baseline() != Some(&baseline) || binding_changed;
        if reconfigure {
            if !self.auto_capture.pending.is_empty() {
                self.cancel_auto_candidates_matching(
                    "Live detection candidate cancelled because acceptance settings or K/D12 changed.",
                    |_| true,
                );
            }
            if let Err(error) = self
                .session
                .configure_auto_admission(Some(baseline), Some(binding))
            {
                self.acceptance_draft.error = Some(error.to_string());
                return;
            }
            self.auto_capture.last_assessment = self.session.assess_auto_admission(None).ok();
            if binding_changed && self.guided_capture.is_some() {
                self.stop_guided_capture(
                    "Guided Auto Capture stopped because K/D12 binding changed.",
                );
            }
        }
        self.acceptance_draft.error = None;
    }

    fn active_live_admission(&self) -> bool {
        let (Some(context), Some(baseline), Some(binding)) = (
            self.live_admission_context.as_ref(),
            self.session.auto_capture_baseline(),
            self.session.initial_intrinsics_binding(),
        ) else {
            return false;
        };
        baseline.acquisition_key == context.acquisition_key
            && baseline.image_size == context.image_size
            && baseline.board == self.session.board()
            && binding.acquisition_key == context.acquisition_key
            && binding.reference_image_size == context.image_size
    }

    fn start_guided_capture(&mut self) {
        if !self.auto_capture.pending.is_empty() {
            if self
                .auto_capture
                .pending
                .iter()
                .all(|candidate| candidate.intent == CandidateIntent::PreviewOnly)
            {
                self.cancel_auto_candidates_matching(
                    "Live board preview cancelled because Guided Auto Capture started.",
                    |_| true,
                );
            } else {
                self.status = "Wait for the current auto-capture candidate before starting Guided Auto Capture."
                    .to_owned();
                return;
            }
        }
        self.refresh_runtime_auto_admission();
        if !self.active_live_admission() {
            self.status =
                "Guided Auto Capture waits for one displayed live frame and valid K/D12 inputs."
                    .to_owned();
            return;
        }
        let (Some(context), Some(binding)) = (
            self.live_admission_context.clone(),
            self.session.initial_intrinsics_binding().cloned(),
        ) else {
            self.status =
                "Guided Auto Capture cannot start without a live admission binding.".to_owned();
            return;
        };
        let guided_binding = GuidedCaptureBinding {
            source: context.source,
            acquisition_key: context.acquisition_key,
            image_size: context.image_size,
            board: self.session.board(),
            initial_intrinsics: binding.initial_intrinsics,
            intrinsics_digest: binding.digest,
        };
        let runtime = match GuidedCaptureRuntime::standard_25(guided_binding) {
            Ok(runtime) => runtime,
            Err(error) => {
                self.status = format!("Guided Auto Capture cannot project target grid: {error}");
                return;
            }
        };
        self.auto_capture_trigger_mode = AutoCaptureTriggerMode::GuidedPresetPose;
        self.auto_capture_enabled = true;
        self.auto_capture.last_observed = None;
        self.guided_capture = Some(runtime);
        self.status =
            "Guided Auto Capture started with the Standard guided preset pose plan.".to_owned();
    }

    fn pause_guided_capture(&mut self) {
        if let Some(runtime) = self.guided_capture.as_mut()
            && runtime.state == GuidedCaptureState::Running
        {
            runtime.state = GuidedCaptureState::Paused;
            runtime.reset_hold();
            self.status = "Guided Auto Capture paused.".to_owned();
        }
    }

    fn resume_guided_capture(&mut self) {
        if let Some(runtime) = self.guided_capture.as_mut()
            && runtime.state == GuidedCaptureState::Paused
        {
            runtime.state = GuidedCaptureState::Running;
            runtime.reset_hold();
            self.auto_capture_enabled = true;
            self.status = runtime.current_step_label();
        }
    }
    fn stop_guided_capture(&mut self, message: impl Into<String>) {
        if self.auto_capture.pending.iter().any(|candidate| {
            matches!(
                candidate.intent,
                CandidateIntent::GuidedMeasure | CandidateIntent::GuidedCapture
            )
        }) {
            self.cancel_auto_candidates_matching(
                "Guided Auto Capture candidate cancelled.",
                |candidate| {
                    matches!(
                        candidate.intent,
                        CandidateIntent::GuidedMeasure | CandidateIntent::GuidedCapture
                    )
                },
            );
        }
        self.guided_capture = None;
        if self.auto_capture_trigger_mode == AutoCaptureTriggerMode::GuidedPresetPose {
            self.auto_capture_enabled = false;
        }
        self.status = message.into();
    }

    /// 普通 Dataset PnP 绑定当前 GUI K/D12 和各自图片尺寸，不继承 live source acquisition key。
    fn dataset_pose_seed(&self) -> Option<DatasetPoseEstimationSeed> {
        if self.auto_intrinsics {
            return Some(DatasetPoseEstimationSeed::AutoCentered);
        }
        let initial_intrinsics = InitialIntrinsics {
            camera_matrix: [self.fx, 0.0, self.cx, 0.0, self.fy, self.cy, 0.0, 0.0, 1.0],
            distortion_coefficients: self.initial_distortion_coefficients.to_vec(),
        };
        initial_intrinsics.validate().ok()?;
        Some(DatasetPoseEstimationSeed::Fixed(initial_intrinsics))
    }

    fn dataset_pnp_binding(
        &self,
        image_size: CalibrationImageSize,
    ) -> Option<InitialIntrinsicsBinding> {
        let initial_intrinsics = self.initial_intrinsics_for_image(image_size).ok()?;
        InitialIntrinsicsBinding::dataset_full_frame(initial_intrinsics, image_size).ok()
    }

    fn dataset_pose_request_for_image(
        &self,
        image_size: CalibrationImageSize,
    ) -> Option<PoseEstimationRequest> {
        let binding = self.dataset_pnp_binding(image_size)?;
        Some(PoseEstimationRequest {
            initial_intrinsics: binding.initial_intrinsics,
            reference_image_size: binding.reference_image_size,
            binding_digest: binding.digest,
        })
    }

    pub(crate) fn stream_disconnected(&mut self, session_id: &StreamSessionId) {
        self.pts_bridge_cache
            .retain(|key, _| key.stream_id != *session_id);
        let pending_matches = self
            .auto_capture
            .pending
            .iter()
            .any(|candidate| candidate.token.frame_identity().stream_id == *session_id);
        if pending_matches {
            self.cancel_auto_candidates_matching(
                "Live detection candidate cancelled because its stream disconnected.",
                |candidate| candidate.token.frame_identity().stream_id == *session_id,
            );
            return;
        }
        if self
            .auto_capture
            .latest_detection
            .as_ref()
            .is_some_and(|latest| latest.identity.stream_id == *session_id)
        {
            self.auto_capture.latest_detection = None;
        }
    }

    fn dispatch_auto_candidate(&mut self, context: &egui::Context) {
        for index in 0..self.auto_capture.pending.len() {
            let Some(candidate) = self.auto_capture.pending.get_mut(index) else {
                break;
            };
            if candidate.state != AutoCandidateState::Queued {
                continue;
            }
            let Some(encoded) = candidate.encoded.take() else {
                continue;
            };
            let candidate_id = candidate.token.id();
            let job = LoadedDetectionJob::from_encoded(
                candidate_id.get(),
                EncodedDetectionRequest::Candidate(candidate.token.clone()),
                encoded,
                candidate.cancellation.clone(),
                candidate.pose_request.clone(),
            );
            match self.detection_pipeline.try_submit_detection(job) {
                Ok(()) => candidate.state = AutoCandidateState::Submitted,
                Err(TrySendError::Full(job)) => {
                    candidate.encoded = Some(job.encoded);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.complete_auto_candidate(
                        Some(context),
                        candidate_id,
                        CandidateTerminal::Discard(
                            "Calibration detection workers stopped unexpectedly.".to_owned(),
                        ),
                    );
                    break;
                }
            }
        }
    }

    fn authoritative_yuv_lookup_for_rtsp_identity(
        &mut self,
        host: &str,
        tcp_port: u16,
        rtsp_identity: &StreamFrameIdentity,
    ) -> Result<AuthoritativeYuvLookup, String> {
        if let Ok(lookup) = AuthoritativeYuvLookup::from_rtsp_identity(rtsp_identity) {
            return Ok(lookup);
        }
        let source_pts_90k = source_pts_to_90k(&rtsp_identity.source_pts)?;
        let key = RtspPtsBridgeKey {
            stream_id: rtsp_identity.stream_id.clone(),
            channel: rtsp_identity.channel,
        };
        let now_ns = host_monotonic_time_ns();
        let sample = self
            .pts_bridge_cache
            .get(&key)
            .filter(|sample| {
                now_ns.saturating_sub(sample.updated_at_host_ns) <= X5_RTSP_PTS_BRIDGE_MAX_AGE_NS
            })
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| {
                self.refresh_rtsp_pts_bridge_sample(host, tcp_port, rtsp_identity, source_pts_90k)
            })?;
        let target_pts_90k = sample.target_rtsp_pts_90k(source_pts_90k)?;
        tracing::warn!(
            operation = "x5_rtsp_pts_bridge",
            channel = rtsp_identity.channel,
            frame_sequence = rtsp_identity.frame_sequence,
            source_pts_90k,
            target_rtsp_pts_90k = target_pts_90k,
            bridge_offset_90k = sample.offset_90k,
            sampled_frame_sequence = sample.sampled_frame_sequence,
            sampled_source_pts_90k = sample.source_pts_90k,
            sampled_driver_rtsp_pts_90k = sample.driver_rtsp_pts_90k,
            tolerance_90k = X5_RTSP_PTS_BRIDGE_TOLERANCE_90K,
            "X5 authoritative YUV using experimental RTSP PTS bridge"
        );
        Ok(AuthoritativeYuvLookup::RtspPts90k {
            pts_90k: target_pts_90k,
            tolerance_90k: X5_RTSP_PTS_BRIDGE_TOLERANCE_90K,
            source_pts_90k,
            bridge_offset_90k: sample.offset_90k,
        })
    }

    fn refresh_rtsp_pts_bridge_sample(
        &mut self,
        host: &str,
        tcp_port: u16,
        rtsp_identity: &StreamFrameIdentity,
        source_pts_90k: u64,
    ) -> Result<RtspPtsBridgeSample, String> {
        let status = x5_tcp_client::status(host, tcp_port)
            .map_err(|error| format!("RTSP PTS bridge status query failed: {error}"))?;
        let ring = status
            .rings
            .iter()
            .find(|ring| ring.channel == rtsp_identity.channel)
            .ok_or_else(|| {
                format!(
                    "RTSP PTS bridge missing CH{} ring status",
                    rtsp_identity.channel
                )
            })?;
        if ring.valid == 0 || ring.last_rtsp_pts_90k == 0 {
            return Err(format!(
                "RTSP PTS bridge needs driver rtsp_pts_90k status, got CH{} valid={} last_rtsp_pts_90k={}",
                rtsp_identity.channel, ring.valid, ring.last_rtsp_pts_90k
            ));
        }
        let sample = RtspPtsBridgeSample {
            source_pts_90k,
            driver_rtsp_pts_90k: ring.last_rtsp_pts_90k,
            offset_90k: i128::from(ring.last_rtsp_pts_90k) - i128::from(source_pts_90k),
            sampled_frame_sequence: rtsp_identity.frame_sequence,
            updated_at_host_ns: host_monotonic_time_ns(),
        };
        self.pts_bridge_cache.insert(
            RtspPtsBridgeKey {
                stream_id: rtsp_identity.stream_id.clone(),
                channel: rtsp_identity.channel,
            },
            sample.clone(),
        );
        tracing::warn!(
            operation = "x5_rtsp_pts_bridge",
            channel = rtsp_identity.channel,
            frame_sequence = rtsp_identity.frame_sequence,
            source_pts_90k,
            driver_rtsp_pts_90k = sample.driver_rtsp_pts_90k,
            bridge_offset_90k = sample.offset_90k,
            ring_valid = ring.valid,
            ring_depth = ring.depth,
            "X5 RTSP PTS bridge sample refreshed from TCP ring tail"
        );
        Ok(sample)
    }

    fn remember_successful_rtsp_pts_bridge(
        &mut self,
        rtsp_identity: &StreamFrameIdentity,
        source_pts_90k: u64,
        driver_rtsp_pts_90k: u64,
    ) {
        if driver_rtsp_pts_90k == 0 {
            return;
        }
        let sample = RtspPtsBridgeSample {
            source_pts_90k,
            driver_rtsp_pts_90k,
            offset_90k: i128::from(driver_rtsp_pts_90k) - i128::from(source_pts_90k),
            sampled_frame_sequence: rtsp_identity.frame_sequence,
            updated_at_host_ns: host_monotonic_time_ns(),
        };
        self.pts_bridge_cache.insert(
            RtspPtsBridgeKey {
                stream_id: rtsp_identity.stream_id.clone(),
                channel: rtsp_identity.channel,
            },
            sample,
        );
    }

    fn queue_authoritative_yuv_candidate(
        &mut self,
        source: &CalibrationSource,
        rtsp_identity: &StreamFrameIdentity,
        intent: CandidateIntent,
        guided_step_index: Option<usize>,
        pose_request: Option<PoseEstimationRequest>,
    ) -> Result<AutoCandidateId, String> {
        let (store, acquisition_key, image_size, authoritative_capture) = match &source.kind {
            CalibrationSourceKind::Stream(stream) => (
                stream.store.clone(),
                stream.acquisition_key.clone(),
                stream.image_size,
                stream.authoritative_capture.clone(),
            ),
            CalibrationSourceKind::File { .. } => {
                return Err("RTSP precheck source is not stream-backed".to_owned());
            }
        };
        let Some(authoritative_capture) = authoritative_capture else {
            return Err("live source has no authoritative YUV capture provider".to_owned());
        };
        let LiveAuthoritativeCapture::X5233TcpYuv { host, tcp_port } = authoritative_capture;
        let lookup = self
            .authoritative_yuv_lookup_for_rtsp_identity(&host, tcp_port, rtsp_identity)
            .map_err(|error| {
                tracing::warn!(
                    operation = "x5_authoritative_yuv_lookup",
                    channel = rtsp_identity.channel,
                    frame_sequence = rtsp_identity.frame_sequence,
                    source_pts = ?rtsp_identity.source_pts,
                    error = %error,
                    "X5 authoritative YUV lookup could not derive frame identity"
                );
                error
            })?;
        let lookup_label = lookup.label();
        let lookup_value = lookup.value();
        let snapshot = match &lookup {
            AuthoritativeYuvLookup::FrameId(frame_id) => {
                x5_tcp_client::capture_yuv_snapshot_by_frame_id(
                    &host,
                    tcp_port,
                    rtsp_identity.channel,
                    *frame_id,
                )
            }
            AuthoritativeYuvLookup::TimestampNs(timestamp_ns) => {
                x5_tcp_client::capture_yuv_snapshot_by_timestamp_ns(
                    &host,
                    tcp_port,
                    rtsp_identity.channel,
                    *timestamp_ns,
                )
            }
            AuthoritativeYuvLookup::RtspPts90k {
                pts_90k,
                tolerance_90k,
                ..
            } => x5_tcp_client::capture_yuv_snapshot_by_rtsp_pts_90k(
                &host,
                tcp_port,
                rtsp_identity.channel,
                *pts_90k,
                *tolerance_90k,
            ),
        }
        .map_err(|error| {
            let ring_diagnostics =
                query_x5_authoritative_yuv_ring_diagnostics(&host, tcp_port, rtsp_identity.channel);
            let message = format!(
                "X5 authoritative YUV lookup by {lookup_label} failed: {error}; current {ring_diagnostics}"
            );
            tracing::warn!(
                operation = "x5_authoritative_yuv_lookup",
                host = %host,
                tcp_port,
                channel = rtsp_identity.channel,
                lookup = lookup_label,
                lookup_value,
                error = %error,
                ring = %ring_diagnostics,
                "X5 authoritative YUV lookup failed"
            );
            message
        })?;
        if snapshot.channel != rtsp_identity.channel {
            return Err(format!(
                "X5 authoritative YUV channel {} does not match RTSP channel {}",
                snapshot.channel, rtsp_identity.channel
            ));
        }
        if snapshot.width != image_size.width || snapshot.height != image_size.height {
            return Err(format!(
                "X5 authoritative YUV geometry {}x{} does not match RTSP precheck {}x{}",
                snapshot.width, snapshot.height, image_size.width, image_size.height
            ));
        }
        if let AuthoritativeYuvLookup::RtspPts90k { source_pts_90k, .. } = &lookup {
            self.remember_successful_rtsp_pts_bridge(
                rtsp_identity,
                *source_pts_90k,
                snapshot.rtsp_pts_90k,
            );
        }

        let FrozenStreamInput { source, encoded } = freeze_authoritative_yuv_input(
            snapshot,
            store,
            acquisition_key.clone(),
            rtsp_identity,
            &lookup,
        )?;
        let frame_identity = match &source.kind {
            CalibrationSourceKind::Stream(stream) => stream.identity.clone(),
            CalibrationSourceKind::File { .. } => {
                return Err("X5 authoritative YUV candidate is not stream-backed".to_owned());
            }
        };
        let candidate_id = AutoCandidateId::new(self.auto_capture.next_candidate_id);
        self.auto_capture.next_candidate_id =
            self.auto_capture.next_candidate_id.wrapping_add(1).max(1);
        let token = self
            .session
            .bind_auto_candidate(
                candidate_id,
                frame_identity,
                encoded.source_revision.clone(),
                source.display_name.clone(),
                Some(acquisition_key),
            )
            .map_err(|error| format!("X5 authoritative YUV candidate rejected: {error}"))?;
        self.auto_capture.pending.push_back(PendingCandidate {
            token,
            intent,
            source,
            encoded: Some(encoded),
            cancellation: CalibrationCancellation::default(),
            pose_request,
            guided_step_index,
            state: AutoCandidateState::Queued,
        });
        Ok(candidate_id)
    }

    fn complete_auto_candidate(
        &mut self,
        context: Option<&egui::Context>,
        candidate_id: AutoCandidateId,
        terminal: CandidateTerminal,
    ) {
        if !self
            .auto_capture
            .pending
            .iter()
            .any(|candidate| candidate.token.id() == candidate_id)
        {
            return;
        }
        self.auto_capture.completed.push((candidate_id, terminal));
        self.finalize_completed_auto_candidates(context);
    }

    fn finalize_completed_auto_candidates(&mut self, context: Option<&egui::Context>) {
        loop {
            let Some(front_id) = self
                .auto_capture
                .pending
                .front()
                .map(|candidate| candidate.token.id())
            else {
                break;
            };
            let Some(done_index) = self
                .auto_capture
                .completed
                .iter()
                .position(|(candidate_id, _)| *candidate_id == front_id)
            else {
                break;
            };
            let (_, terminal) = self.auto_capture.completed.remove(done_index);
            let candidate = self
                .auto_capture
                .pending
                .pop_front()
                .expect("front candidate was checked");
            self.finalize_candidate_entry(context, candidate, terminal);
        }
    }

    fn cancel_auto_candidates_matching(
        &mut self,
        message: impl Into<String>,
        mut predicate: impl FnMut(&PendingCandidate) -> bool,
    ) {
        let message = message.into();
        let mut cancelled = false;
        self.auto_capture.pending.retain(|candidate| {
            if predicate(candidate) {
                candidate.cancellation.cancel();
                cancelled = true;
                false
            } else {
                true
            }
        });
        self.auto_capture.completed.retain(|(candidate_id, _)| {
            self.auto_capture
                .pending
                .iter()
                .any(|candidate| candidate.token.id() == *candidate_id)
        });
        if cancelled {
            self.status = message;
        }
    }

    fn enqueue_guided_capture_sample(&mut self, sample: GuidedHoldSample) {
        if self.auto_capture.pending.len() >= LIVE_AUTO_CANDIDATE_CAPACITY
            || self
                .auto_capture
                .pending
                .iter()
                .any(|candidate| candidate.token.id() == sample.token.id())
        {
            return;
        }
        let encoded = match sample.source.encoded_png(sample.token.source_revision()) {
            Ok(Some(encoded)) => encoded,
            Ok(None) => {
                self.status = "Guided capture sample is not stream-backed.".to_owned();
                return;
            }
            Err(error) => {
                self.status = format!("Guided capture sample is unavailable: {error}");
                return;
            }
        };
        let step = sample.guided_step_index;
        self.auto_capture.pending.push_back(PendingCandidate {
            token: sample.token,
            intent: CandidateIntent::GuidedCapture,
            source: sample.source,
            encoded: Some(encoded),
            cancellation: CalibrationCancellation::default(),
            pose_request: sample.pose_request,
            guided_step_index: step,
            state: AutoCandidateState::Queued,
        });
        self.status = format!(
            "Guided hold stable; queued best frame for step {} capture.",
            step.map_or(0, |step| step + 1)
        );
    }

    fn finalize_candidate_entry(
        &mut self,
        context: Option<&egui::Context>,
        candidate: PendingCandidate,
        terminal: CandidateTerminal,
    ) {
        let PendingCandidate {
            token,
            intent,
            source,
            cancellation,
            pose_request,
            guided_step_index,
            ..
        } = candidate;
        cancellation.cancel();
        match terminal {
            CandidateTerminal::Detection(Ok(DetectionProduct {
                source_revision,
                outcome: camera_toolbox_core::ChessboardDetectionOutcome::Found(detection),
                pnp_observation,
                preview,
            })) => {
                if source_revision != *token.source_revision() {
                    self.auto_capture.latest_detection = None;
                    self.status = "Live detection source revision changed.".to_owned();
                    return;
                }
                self.auto_capture.latest_detection = Some(IdentityBoundDetection {
                    identity: token.frame_identity().clone(),
                    acquisition_key: match &source.kind {
                        CalibrationSourceKind::Stream(stream) => stream.acquisition_key.clone(),
                        CalibrationSourceKind::File { .. } => {
                            self.auto_capture.latest_detection = None;
                            self.status = "Live detection source was not stream-backed.".to_owned();
                            return;
                        }
                    },
                    detection: detection.clone(),
                    pnp_observation: pnp_observation.clone(),
                    completed_at_ns: host_monotonic_time_ns(),
                });
                match intent {
                    CandidateIntent::PreviewOnly => {
                        self.auto_capture.last_assessment =
                            self.session.assess_auto_admission(None).ok();
                        self.status = format!(
                            "Board preview detected on stream frame {}.",
                            token.frame_identity().frame_sequence
                        );
                    }
                    CandidateIntent::AutoCommit => {
                        let Some(pnp_observation) = pnp_observation else {
                            self.status =
                                "Automatic candidate rejected: PnP evidence was not produced."
                                    .to_owned();
                            return;
                        };
                        let assessment = match self
                            .session
                            .assess_auto_admission(Some((&detection, &pnp_observation)))
                        {
                            Ok(assessment) => assessment,
                            Err(error) => {
                                self.status = format!("Automatic candidate rejected: {error}");
                                return;
                            }
                        };
                        let gain = assessment.constraint_gain;
                        if let Some(baseline) = self.session.auto_capture_baseline()
                            && assessment.constraint_gain < baseline.criteria.minimum_auto_gain
                        {
                            self.auto_capture.last_assessment =
                                self.session.assess_auto_admission(None).ok();
                            self.status = format!(
                                "Automatic RTSP precheck rejected: candidate gain {} is below minimum {}.",
                                format_gain(gain),
                                format_gain(baseline.criteria.minimum_auto_gain)
                            );
                            return;
                        }
                        if matches!(
                            &source.kind,
                            CalibrationSourceKind::Stream(stream)
                                if stream.authoritative_capture.is_some()
                        ) {
                            match self.queue_authoritative_yuv_candidate(
                                &source,
                                token.frame_identity(),
                                intent,
                                guided_step_index,
                                pose_request.clone(),
                            ) {
                                Ok(yuv_candidate_id) => {
                                    self.auto_capture.last_assessment = Some(assessment.clone());
                                    self.status = format!(
                                        "RTSP precheck passed (gain {}); queued X5 YUV candidate {} for same-frame validation.",
                                        format_gain(gain),
                                        yuv_candidate_id.get()
                                    );
                                }
                                Err(error) => {
                                    self.auto_capture.last_assessment = Some(assessment.clone());
                                    self.status = format!(
                                        "Automatic RTSP precheck passed, but X5 YUV confirmation was not queued: {error}"
                                    );
                                }
                            }
                            return;
                        }
                        let commit = AutoCandidateCommit::new(
                            token,
                            source_revision,
                            detection.clone(),
                            pnp_observation.clone(),
                        );
                        match self.session.commit_auto_candidate(commit) {
                            Ok(item_id) => {
                                let overlay_acquisition_key = match &source.kind {
                                    CalibrationSourceKind::Stream(stream) => {
                                        Some(stream.acquisition_key.clone())
                                    }
                                    CalibrationSourceKind::File { .. } => None,
                                };
                                self.sources.insert(item_id, source);
                                if let (Some(context), Some(frame)) = (context, preview) {
                                    self.install_preview(context, item_id, frame);
                                }
                                let committed_at_ns = host_monotonic_time_ns();
                                if let Some(acquisition_key) = overlay_acquisition_key {
                                    self.auto_capture.last_dataset_overlay =
                                        Some(DatasetDetectionOverlay {
                                            item_id,
                                            detection,
                                            acquisition_key,
                                            pnp_observation: Some(pnp_observation),
                                            committed_at_ns,
                                        });
                                }
                                self.auto_capture.last_assessment =
                                    self.session.assess_auto_admission(None).ok();
                                self.auto_capture.last_accepted_at_ns = committed_at_ns;
                                self.coverage_dirty = true;
                                self.status = format!(
                                    "Automatic capture committed as dataset item {} (candidate gain {}).",
                                    item_id.get(),
                                    format_gain(gain)
                                );
                            }
                            Err(error) => {
                                self.auto_capture.last_assessment =
                                    self.session.assess_auto_admission(None).ok();
                                self.status =
                                    format!("Automatic candidate commit rejected: {error}");
                            }
                        }
                    }
                    CandidateIntent::GuidedMeasure => {
                        let Some(pnp_observation) = pnp_observation else {
                            if let Some(runtime) = self.guided_capture.as_mut() {
                                runtime.reset_hold();
                            }
                            self.status =
                                "Guided pose rejected: PnP evidence was not produced.".to_owned();
                            return;
                        };
                        let Some(runtime) = self.guided_capture.as_ref() else {
                            self.status =
                                "Guided pose result ignored because the guided session stopped."
                                    .to_owned();
                            return;
                        };
                        let expected_step = runtime.current_step;
                        if guided_step_index != Some(expected_step) {
                            self.status =
                                "Guided pose result ignored because its step is stale.".to_owned();
                            return;
                        }
                        let Some(target) = runtime.current_target().cloned() else {
                            self.status =
                                "Guided pose result ignored because the preset is complete."
                                    .to_owned();
                            return;
                        };
                        let assessment = match assess_guided_pose(
                            expected_step,
                            &target,
                            &detection,
                            &pnp_observation,
                            runtime.binding.board,
                            &runtime.binding.initial_intrinsics,
                            runtime.binding.image_size,
                        ) {
                            Ok(assessment) => assessment,
                            Err(error) => {
                                if let Some(runtime) = self.guided_capture.as_mut() {
                                    runtime.reset_hold();
                                }
                                self.status = format!("Guided pose rejected: {error}");
                                return;
                            }
                        };
                        let matched = assessment.matched;
                        let score = assessment.pose_error_score;
                        let reason = assessment.reason.clone();
                        let hold_sample = GuidedHoldSample {
                            token: token.clone(),
                            source: source.clone(),
                            pose_request: pose_request.clone(),
                            guided_step_index,
                            stability_score: score,
                        };
                        let mut capture_sample = None;
                        let mut hold_frames = 0;
                        if let Some(runtime) = self.guided_capture.as_mut() {
                            let update = runtime.update_hold(assessment, Some(hold_sample));
                            capture_sample = update.capture_sample;
                            hold_frames = runtime.hold_frames;
                        }
                        if let Some(sample) = capture_sample {
                            self.enqueue_guided_capture_sample(sample);
                        } else if matched {
                            self.status = format!(
                                "Guided pose matched for step {} (error {:.2}); hold {}/{}.",
                                expected_step + 1,
                                score,
                                hold_frames,
                                GUIDED_CAPTURE_HOLD_FRAMES
                            );
                        } else {
                            self.status = format!(
                                "Guided pose waiting for step {}: {}.",
                                expected_step + 1,
                                reason.unwrap_or_else(|| "pose error above threshold".to_owned())
                            );
                        }
                    }
                    CandidateIntent::GuidedCapture => {
                        let Some(pnp_observation) = pnp_observation else {
                            if let Some(runtime) = self.guided_capture.as_mut() {
                                runtime.reset_hold();
                            }
                            self.status = "Guided capture rejected: PnP evidence was not produced."
                                .to_owned();
                            return;
                        };
                        let Some(runtime) = self.guided_capture.as_ref() else {
                            self.status =
                                "Guided capture ignored because the guided session stopped."
                                    .to_owned();
                            return;
                        };
                        let expected_step = runtime.current_step;
                        if guided_step_index != Some(expected_step) {
                            self.status =
                                "Guided capture ignored because its step is stale.".to_owned();
                            return;
                        }
                        let Some(target) = runtime.current_target().cloned() else {
                            self.status =
                                "Guided capture ignored because the preset is complete.".to_owned();
                            return;
                        };
                        let assessment = match assess_guided_pose(
                            expected_step,
                            &target,
                            &detection,
                            &pnp_observation,
                            runtime.binding.board,
                            &runtime.binding.initial_intrinsics,
                            runtime.binding.image_size,
                        ) {
                            Ok(assessment) => assessment,
                            Err(error) => {
                                if let Some(runtime) = self.guided_capture.as_mut() {
                                    runtime.reset_hold();
                                }
                                self.status = format!("Guided capture rejected: {error}");
                                return;
                            }
                        };
                        if !assessment.matched {
                            let reason = assessment
                                .reason
                                .clone()
                                .unwrap_or_else(|| "pose error above threshold".to_owned());
                            if let Some(runtime) = self.guided_capture.as_mut() {
                                runtime.last_assessment = Some(assessment);
                                runtime.reset_hold();
                            }
                            self.status = format!(
                                "Guided capture rejected for step {}: {reason}.",
                                expected_step + 1
                            );
                            return;
                        }
                        if matches!(
                            &source.kind,
                            CalibrationSourceKind::Stream(stream)
                                if stream.authoritative_capture.is_some()
                        ) {
                            match self.queue_authoritative_yuv_candidate(
                                &source,
                                token.frame_identity(),
                                intent,
                                guided_step_index,
                                pose_request.clone(),
                            ) {
                                Ok(yuv_candidate_id) => {
                                    self.status = format!(
                                        "Guided RTSP precheck matched step {}; queued X5 YUV candidate {} for same-frame validation.",
                                        expected_step + 1,
                                        yuv_candidate_id.get()
                                    );
                                }
                                Err(error) => {
                                    self.status = format!(
                                        "Guided RTSP precheck matched step {}, but X5 YUV confirmation was not queued: {error}",
                                        expected_step + 1
                                    );
                                }
                            }
                            return;
                        }
                        let commit = AutoCandidateCommit::new(
                            token,
                            source_revision,
                            detection.clone(),
                            pnp_observation.clone(),
                        );
                        match self.session.commit_guided_auto_candidate(commit) {
                            Ok(item_id) => {
                                let overlay_acquisition_key = match &source.kind {
                                    CalibrationSourceKind::Stream(stream) => {
                                        Some(stream.acquisition_key.clone())
                                    }
                                    CalibrationSourceKind::File { .. } => None,
                                };
                                self.sources.insert(item_id, source);
                                if let (Some(context), Some(frame)) = (context, preview) {
                                    self.install_preview(context, item_id, frame);
                                }
                                let committed_at_ns = host_monotonic_time_ns();
                                if let Some(acquisition_key) = overlay_acquisition_key {
                                    self.auto_capture.last_dataset_overlay =
                                        Some(DatasetDetectionOverlay {
                                            item_id,
                                            detection,
                                            acquisition_key,
                                            pnp_observation: Some(pnp_observation),
                                            committed_at_ns,
                                        });
                                }
                                self.auto_capture.last_assessment =
                                    self.session.assess_auto_admission(None).ok();
                                self.auto_capture.last_accepted_at_ns = committed_at_ns;
                                self.coverage_dirty = true;
                                if let Some(runtime) = self.guided_capture.as_mut() {
                                    runtime.advance_after_commit();
                                    self.status = if runtime.state == GuidedCaptureState::Complete {
                                        format!(
                                            "Guided Auto Capture complete; committed dataset item {} as step {}/{}.",
                                            item_id.get(),
                                            expected_step + 1,
                                            runtime.plan.len()
                                        )
                                    } else {
                                        format!(
                                            "Guided capture committed dataset item {} as step {}; next: {}.",
                                            item_id.get(),
                                            expected_step + 1,
                                            runtime.current_step_label()
                                        )
                                    };
                                }
                            }
                            Err(error) => {
                                if let Some(runtime) = self.guided_capture.as_mut() {
                                    runtime.reset_hold();
                                }
                                self.auto_capture.last_assessment =
                                    self.session.assess_auto_admission(None).ok();
                                self.status = format!("Guided capture commit rejected: {error}");
                            }
                        }
                    }
                }
            }
            CandidateTerminal::Detection(Ok(DetectionProduct {
                outcome: camera_toolbox_core::ChessboardDetectionOutcome::NotFound { .. },
                ..
            })) => {
                self.auto_capture.latest_detection = None;
                self.status = match intent {
                    CandidateIntent::PreviewOnly => {
                        "Board preview: chessboard not found.".to_owned()
                    }
                    CandidateIntent::AutoCommit => {
                        "Automatic candidate rejected: chessboard not found.".to_owned()
                    }
                    CandidateIntent::GuidedMeasure => {
                        if let Some(runtime) = self.guided_capture.as_mut() {
                            runtime.reset_hold();
                        }
                        "Guided pose rejected: chessboard not found.".to_owned()
                    }
                    CandidateIntent::GuidedCapture => {
                        if let Some(runtime) = self.guided_capture.as_mut() {
                            runtime.reset_hold();
                        }
                        "Guided capture rejected: chessboard not found.".to_owned()
                    }
                };
            }
            CandidateTerminal::Detection(Err(error)) => {
                self.auto_capture.latest_detection = None;
                self.status = match (intent, error.is_cancelled()) {
                    (CandidateIntent::PreviewOnly, true) => {
                        "Board preview detection cancelled.".to_owned()
                    }
                    (CandidateIntent::PreviewOnly, false) => {
                        format!("Board preview detection failed: {error}")
                    }
                    (CandidateIntent::AutoCommit, true) => {
                        "Automatic candidate cancelled.".to_owned()
                    }
                    (CandidateIntent::AutoCommit, false) => {
                        format!("Automatic candidate detection or PnP failed: {error}")
                    }
                    (CandidateIntent::GuidedMeasure, true) => {
                        if let Some(runtime) = self.guided_capture.as_mut() {
                            runtime.reset_hold();
                        }
                        "Guided pose detection cancelled.".to_owned()
                    }
                    (CandidateIntent::GuidedMeasure, false) => {
                        if let Some(runtime) = self.guided_capture.as_mut() {
                            runtime.reset_hold();
                        }
                        format!("Guided pose detection or PnP failed: {error}")
                    }
                    (CandidateIntent::GuidedCapture, true) => {
                        if let Some(runtime) = self.guided_capture.as_mut() {
                            runtime.reset_hold();
                        }
                        "Guided capture cancelled.".to_owned()
                    }
                    (CandidateIntent::GuidedCapture, false) => {
                        if let Some(runtime) = self.guided_capture.as_mut() {
                            runtime.reset_hold();
                        }
                        format!("Guided capture detection or PnP failed: {error}")
                    }
                };
            }
            CandidateTerminal::Discard(message) => {
                self.auto_capture.latest_detection = None;
                self.status = message;
            }
        }
    }

    fn cancel_auto_candidate(&mut self, message: impl Into<String>) {
        self.cancel_auto_candidates_matching(message, |_| true);
    }

    fn remember_stream_detection(
        &mut self,
        item_id: CalibrationItemId,
        outcome: &camera_toolbox_core::ChessboardDetectionOutcome,
        pnp_observation: Option<PnPObservation>,
    ) {
        let Some(source) = self.sources.get(&item_id) else {
            return;
        };
        let CalibrationSourceKind::Stream(stream) = &source.kind else {
            return;
        };
        self.auto_capture.latest_detection = match outcome {
            camera_toolbox_core::ChessboardDetectionOutcome::Found(detection) => {
                self.auto_capture.last_dataset_overlay = Some(DatasetDetectionOverlay {
                    item_id,
                    acquisition_key: stream.acquisition_key.clone(),
                    detection: detection.clone(),
                    pnp_observation: pnp_observation.clone(),
                    committed_at_ns: host_monotonic_time_ns(),
                });
                Some(IdentityBoundDetection {
                    identity: stream.identity.clone(),
                    acquisition_key: stream.acquisition_key.clone(),
                    detection: detection.clone(),
                    pnp_observation,
                    completed_at_ns: host_monotonic_time_ns(),
                })
            }
            camera_toolbox_core::ChessboardDetectionOutcome::NotFound { .. } => None,
        };
    }

    pub(crate) fn take_export(&mut self) -> Option<CalibrationExport> {
        self.pending_export.take()
    }

    pub(crate) fn take_provision_intent(&mut self) -> Option<CalibrationProvisionIntent> {
        self.eeprom.take_intent()
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_target_configured(&mut self, label: &str) {
        self.eeprom.report_target_configured(label);
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_target_configuration_failed(&mut self, message: impl Into<String>) {
        self.eeprom.report_target_configuration_failed(message);
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_target_invalidated(&mut self, message: impl Into<String>) {
        self.eeprom.report_target_invalidated(message);
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_bus_discovery_failed(&mut self, message: impl Into<String>) {
        self.eeprom.report_bus_discovery_failed(message);
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_bus_discovery(&mut self, buses: Vec<camera_toolbox_app::I2cBusInfo>) {
        self.eeprom.report_bus_discovery(buses);
    }

    pub(crate) fn report_provision_error(&mut self, message: impl Into<String>) {
        self.eeprom.report_error(message);
    }

    pub(crate) fn report_eeprom_provision_unknown(&mut self, message: impl Into<String>) {
        self.eeprom.report_provision_unknown(message);
    }

    pub(crate) fn report_eeprom_inspect(
        &mut self,
        target_label: String,
        result: EepromInspectResult,
    ) {
        self.eeprom.report_inspect(target_label, result);
    }

    pub(crate) fn report_eeprom_provision(
        &mut self,
        target_label: String,
        result: &EepromWriteResult,
        audit_file: String,
    ) {
        self.eeprom
            .report_provision(target_label, result, audit_file);
    }

    pub(crate) fn report_eeprom_provision_audit_error(
        &mut self,
        target_label: String,
        result: &EepromWriteResult,
        error: &str,
    ) {
        self.eeprom
            .report_provision_audit_error(target_label, result, error);
    }

    pub(crate) fn report_export_started(&mut self, label: &str, target_label: &str) {
        self.status = format!("Exporting {label} to {target_label}.");
    }

    pub(crate) fn report_export_finished(
        &mut self,
        label: &str,
        target_label: &str,
        result: Result<u64, &str>,
    ) {
        self.status = match result {
            Ok(bytes_written) => {
                format!("Exported {label} ({bytes_written} B) to {target_label}.")
            }
            Err(error) => format!("Export {label} failed: {error}"),
        };
    }

    /// 推进所有后台队列；必须由应用主循环每帧调用，不依赖当前可见 workspace。
    pub(crate) fn tick(&mut self, context: &egui::Context) {
        self.poll_worker(context);
    }

    fn live_detection_for_context_at(&self, now_ns: u64) -> Option<&IdentityBoundDetection> {
        let latest = self.auto_capture.latest_detection.as_ref()?;
        let context = self.live_admission_context.as_ref()?;
        (latest.acquisition_key == context.acquisition_key
            && latest.detection.image_size == context.image_size
            && now_ns.saturating_sub(latest.completed_at_ns) <= LIVE_DETECTION_MARKER_TTL_NS)
            .then_some(latest)
    }

    fn live_detection_for_context(&self) -> Option<&IdentityBoundDetection> {
        self.live_detection_for_context_at(host_monotonic_time_ns())
    }

    fn live_field_cells_at(
        &self,
        criteria: &AutoCaptureAcceptanceCriteria,
        now_ns: u64,
    ) -> Vec<usize> {
        self.live_detection_for_context_at(now_ns)
            .map(|latest| CalibrationSession::detection_field_cells(&latest.detection, criteria))
            .unwrap_or_default()
    }

    fn live_field_cells(&self, criteria: &AutoCaptureAcceptanceCriteria) -> Vec<usize> {
        self.live_field_cells_at(criteria, host_monotonic_time_ns())
    }

    fn live_acceptance_marker_observation(&self) -> Option<&PnPObservation> {
        let latest = self.live_detection_for_context()?;
        let context = self.live_admission_context.as_ref()?;
        let binding = self.session.initial_intrinsics_binding()?;
        let observation = latest.pnp_observation.as_ref()?;
        (binding.acquisition_key == context.acquisition_key
            && binding.reference_image_size == context.image_size
            && observation.binding_digest == binding.digest)
            .then_some(observation)
    }

    /// Live Viewer 始终显示当前 RTSP/live texture；这里仅返回最新入库项的 1 秒角点 overlay。
    pub(crate) fn viewer_overlay(
        &self,
        frame: &DecodedVideoFrame,
        live_source: &LiveStreamSource,
    ) -> CalibrationViewerOverlay {
        self.live_viewer_presentation(Some(frame), Some(live_source))
            .map(|presentation| presentation.overlay)
            .unwrap_or_default()
    }

    pub(crate) fn live_viewer_presentation(
        &self,
        live_frame: Option<&DecodedVideoFrame>,
        live_source: Option<&LiveStreamSource>,
    ) -> Option<CalibrationViewerPresentation> {
        self.live_viewer_presentation_at(live_frame, live_source, host_monotonic_time_ns())
    }

    fn live_viewer_presentation_at(
        &self,
        live_frame: Option<&DecodedVideoFrame>,
        live_source: Option<&LiveStreamSource>,
        now_ns: u64,
    ) -> Option<CalibrationViewerPresentation> {
        let (frame, live_source) = live_frame.zip(live_source)?;
        let image_size = CalibrationImageSize::new(frame.width, frame.height).ok()?;
        let acquisition_key = live_source.acquisition_key_for_frame(frame).ok()?;
        let guided_target = self.guided_capture.as_ref().and_then(|runtime| {
            (self.auto_capture_trigger_mode == AutoCaptureTriggerMode::GuidedPresetPose
                && runtime.binding.source == *live_source
                && runtime.binding.acquisition_key == acquisition_key
                && runtime.binding.image_size == image_size
                && matches!(
                    runtime.state,
                    GuidedCaptureState::Running | GuidedCaptureState::Paused
                ))
            .then(|| {
                runtime.current_target().map(|target| {
                    let current_assessment = runtime
                        .last_assessment
                        .as_ref()
                        .filter(|assessment| assessment.step_index == runtime.current_step);
                    let pose_arrow =
                        current_assessment.map(|assessment| ViewerGuidedPoseArrowOverlay {
                            start_uv: assessment.measurement.pose.center_uv,
                            end_uv: target.pose.center_uv,
                            start_xyz: assessment.measurement.pose.xyz,
                            end_xyz: target.pose.xyz,
                            z_delta: target.pose.xyz[2] - assessment.measurement.pose.xyz[2],
                        });
                    let instruction = current_assessment.map(|assessment| {
                        guided_pose_instruction_overlay(assessment, target, runtime.hold_frames)
                    });
                    let rotation_rings = current_assessment
                        .map(|assessment| guided_pose_rotation_rings_overlay(assessment, target));
                    ViewerGuidedPoseOverlay {
                        center_uv: target.pose.center_uv,
                        outline_uv: target.outline_uv,
                        grid_lines: Arc::clone(&target.grid_lines),
                        rotation_rings,
                        pose_arrow,
                        instruction,
                        matched: current_assessment.is_some_and(|assessment| assessment.matched),
                    }
                })
            })
            .flatten()
        });
        let latest = self
            .auto_capture
            .last_dataset_overlay
            .as_ref()
            .filter(|latest| {
                latest.acquisition_key == acquisition_key
                    && latest.detection.image_size == image_size
                    && now_ns.saturating_sub(latest.committed_at_ns)
                        <= LATEST_DATASET_OVERLAY_TTL_NS
            });
        let realtime = self
            .auto_capture
            .latest_detection
            .as_ref()
            .filter(|latest| {
                latest.acquisition_key == acquisition_key
                    && latest.detection.image_size == image_size
                    && now_ns.saturating_sub(latest.completed_at_ns) <= LIVE_DETECTION_MARKER_TTL_NS
            });
        if latest.is_none() && guided_target.is_none() && realtime.is_none() {
            return None;
        }
        let realtime_detection = realtime.and_then(|latest| {
            let observation = latest.pnp_observation.as_ref()?;
            let binding = self
                .live_admission_context
                .as_ref()
                .filter(|context| {
                    context.acquisition_key == acquisition_key && context.image_size == image_size
                })
                .and_then(|_| self.session.initial_intrinsics_binding());
            Some(self.viewer_detection_overlay(&latest.detection, Some(observation), binding))
        });
        Some(CalibrationViewerPresentation {
            item_id: latest.map(|latest| latest.item_id),
            overlay: CalibrationViewerOverlay {
                persistent: latest.map(|latest| {
                    self.viewer_detection_overlay(
                        &latest.detection,
                        latest.pnp_observation.as_ref(),
                        self.dataset_pnp_binding(image_size).as_ref(),
                    )
                }),
                realtime_detection,
                guided_target,
            },
        })
    }

    fn viewer_detection_overlay(
        &self,
        detection: &ChessboardDetection,
        pnp_observation: Option<&PnPObservation>,
        binding: Option<&InitialIntrinsicsBinding>,
    ) -> ViewerDetectionOverlay {
        let pose_axis = binding.and_then(|binding| {
            pnp_observation
                .filter(|observation| {
                    observation.binding_digest == binding.digest
                        && binding.reference_image_size == detection.image_size
                })
                .and_then(|observation| {
                    pose_axis_image_projection(
                        observation,
                        &binding.initial_intrinsics,
                        self.session.board(),
                    )
                })
        });
        ViewerDetectionOverlay {
            image_size: detection.image_size,
            corners: detection.corners.clone(),
            pose_axis,
        }
    }

    pub(crate) fn render(
        &mut self,
        context: &egui::Context,
        ui: &mut egui::Ui,
        export_enabled: bool,
        export_reason: Option<&str>,
        sftp_source: Result<&str, &str>,
        provision_target: Result<&str, &str>,
        has_live_inspection: bool,
        mut render_live_inspection: impl FnMut(&mut egui::Ui) -> Option<Arc<DecodedVideoFrame>>,
    ) -> (egui::Rect, Option<Arc<DecodedVideoFrame>>) {
        self.sync_coverage(context);
        let rect = ui.available_rect_before_wrap();
        self.render_controls(ui);
        ui.separator();
        let mut dataset_sidebar_expanded = self.dataset_sidebar_expanded;
        let mut requested_sidebar_state = None;
        egui::Panel::show_switched(
            ui,
            &mut dataset_sidebar_expanded,
            egui::Panel::right("calibration_dataset_sidebar_collapsed")
                .resizable(true)
                .exact_size(32.0),
            egui::Panel::right("calibration_dataset_sidebar_expanded")
                .resizable(true)
                .default_size(440.0)
                .min_size(300.0),
            |ui, expanded| {
                let idle = self.active_job.is_none();
                let can_clear = idle && !self.session.items().is_empty();
                if expanded {
                    ui.horizontal(|ui| {
                        ui.heading(format!("Dataset ({})", self.session.items().len()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add_enabled(can_clear, egui::Button::new("Clear"))
                                .clicked()
                            {
                                self.session.clear();
                                self.sources.clear();
                                self.coverage_dirty = true;
                            }
                            if ui
                                .small_button("»")
                                .on_hover_text("Collapse Dataset")
                                .clicked()
                            {
                                requested_sidebar_state = Some(false);
                            }
                        });
                    });
                    ui.separator();
                    let sidebar_height = ui.available_height().max(0.0);
                    let acceptance_height = self.dataset_acceptance_body_height(sidebar_height);
                    self.render_dataset_acceptance_panel(ui, acceptance_height);
                    if self.dataset_acceptance_expanded && self.dataset_table_expanded {
                        self.render_dataset_splitter(ui, sidebar_height);
                    } else {
                        ui.separator();
                    }
                    self.render_dataset_table_panel(ui);
                } else if ui.button("«").on_hover_text("Expand Dataset").clicked() {
                    requested_sidebar_state = Some(true);
                }
            },
        );
        self.dataset_sidebar_expanded = requested_sidebar_state.unwrap_or(dataset_sidebar_expanded);
        if !has_live_inspection && self.display_layer == CalibrationDisplayLayer::LiveStream {
            self.display_layer = CalibrationDisplayLayer::DatasetImage;
        }
        ui.horizontal(|ui| {
            ui.add_enabled_ui(has_live_inspection, |ui| {
                ui.selectable_value(
                    &mut self.display_layer,
                    CalibrationDisplayLayer::LiveStream,
                    "Live Stream",
                );
            });
            ui.selectable_value(
                &mut self.display_layer,
                CalibrationDisplayLayer::DatasetImage,
                "Dataset Image",
            );
        });
        ui.separator();
        let available_height = ui.available_height();
        // Reserve bottom portion for calibration result so it is never hidden.
        let result_reserve = 148.0;
        let viewer_height = (available_height - result_reserve - 16.0).max(200.0);
        let mut capture_request = None;
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), viewer_height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                if has_live_inspection && self.display_layer == CalibrationDisplayLayer::LiveStream
                {
                    capture_request = render_live_inspection(ui);
                } else {
                    self.render_inspection(ui);
                }
            },
        );
        ui.add_space(8.0);
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("calibration_metrics")
            .show(ui, |ui| {
                self.render_calibration_result_panel(
                    context,
                    ui,
                    export_enabled,
                    export_reason,
                    sftp_source,
                    provision_target,
                );
            });
        (rect, capture_request)
    }

    pub(crate) fn render_status(&self, ui: &mut egui::Ui) {
        ui.set_min_height(22.0);
        ui.horizontal(|ui| {
            if self.active_job.is_some() || !self.auto_capture.pending.is_empty() {
                ui.spinner();
                ui.separator();
            }
            ui.label(&self.status);
            if let Some(installed) = self.session.latest_installed() {
                ui.separator();
                let response = ui.monospace(format!("RMS {:.4} px", installed.solution.rms_error));
                if !self.session.latest_installed_is_current() {
                    response.on_hover_text(STALE_CALIBRATION_RESULT_REASON);
                    ui.weak("stale");
                }
            }
        });
    }

    fn render_dataset_assessment(&self, ui: &mut egui::Ui, assessment: &AutoAdmissionAssessment) {
        ui.horizontal_wrapped(|ui| {
            ui.monospace(format!(
                "Field quota {}/{} · Depth quota {}/{} · Pose quota {}/{}",
                assessment.field_quota_filled,
                assessment.required_field_quota,
                assessment.depth_quota_filled,
                assessment.required_depth_quota,
                assessment.pose_quota_filled,
                assessment.required_pose_quota,
            ));
            ui.monospace(format!(
                "Occupied Field {} · Depth {} · Pose {}",
                assessment.field_cells, assessment.depth_bins, assessment.pose_bins,
            ));
            ui.monospace(format!(
                "Score {} · Gain Field {} Depth {} Pose {}",
                format_gain(assessment.constraint_gain),
                format_gain(assessment.field_gain),
                format_gain(assessment.depth_gain),
                format_gain(assessment.pose_gain)
            ));
        });
    }

    fn render_eeprom_snid_editor(
        &mut self,
        ui: &mut egui::Ui,
        snid_preview: Result<&str, &String>,
    ) {
        ui.label("YgStereo SNID");
        ui.horizontal_wrapped(|ui| {
            ui.weak("Fixed: resolution=2/FHD, vendor=T/SmartSens, algorithm=0, reserved=0");
        });
        egui::Grid::new("calibration_eeprom_snid_grid")
            .num_columns(2)
            .spacing(egui::vec2(12.0, 6.0))
            .show(ui, |ui| {
                ui.label("Module");
                egui::ComboBox::from_id_salt("calibration_eeprom_snid_module")
                    .selected_text(self.snid_draft.module.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.snid_draft.module,
                            YgStereoModuleCode::Model233,
                            "233",
                        );
                        ui.selectable_value(
                            &mut self.snid_draft.module,
                            YgStereoModuleCode::Model235,
                            "235",
                        );
                    });
                ui.end_row();

                ui.label("Ship date");
                ui.horizontal_wrapped(|ui| {
                    ui.label("Year");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.snid_draft.year)
                            .desired_width(56.0)
                            .hint_text("26"),
                    );
                    ui.label("Month");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.snid_draft.month)
                            .desired_width(36.0)
                            .hint_text("1-12"),
                    );
                    ui.label("Day");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.snid_draft.day)
                            .desired_width(36.0)
                            .hint_text("1-31"),
                    );
                });
                ui.end_row();

                ui.label("Optical axis class");
                egui::ComboBox::from_id_salt("calibration_eeprom_snid_axis_class")
                    .selected_text(optical_axis_class_label(self.snid_draft.optical_axis_class))
                    .show_ui(ui, |ui| {
                        for (value, label) in [
                            (0, "0 - unclassified"),
                            (1, "1 - L0"),
                            (2, "2 - L1"),
                            (3, "3 - R0"),
                            (4, "4 - R1"),
                        ] {
                            ui.selectable_value(
                                &mut self.snid_draft.optical_axis_class,
                                value,
                                label,
                            );
                        }
                    });
                ui.end_row();

                ui.label("Sequence");
                ui.horizontal_wrapped(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.snid_draft.sequence)
                            .desired_width(72.0)
                            .hint_text("1-3844"),
                    );
                    ui.weak("decimal input; encoded as base-62 high/low bytes");
                });
                ui.end_row();
            });
        match snid_preview {
            Ok(value) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Converted SNID");
                    ui.monospace(value);
                });
            }
            Err(error) => {
                ui.colored_label(egui::Color32::YELLOW, format!("SNID incomplete: {error}"));
            }
        }
    }

    fn render_calibration_result_panel(
        &mut self,
        context: &egui::Context,
        ui: &mut egui::Ui,
        export_enabled: bool,
        export_reason: Option<&str>,
        sftp_source: Result<&str, &str>,
        provision_target: Result<&str, &str>,
    ) {
        let idle = self.active_job.is_none();
        let current_result_installed = self.session.installed().is_some();
        let loaded_result_installed = self.loaded_result.is_some();
        let active_result_available = current_result_installed || loaded_result_installed;
        let latest_result_installed = self.session.latest_installed().is_some();
        let latest_result_current = self.session.latest_installed_is_current();
        let stale_result =
            latest_result_installed && !latest_result_current && !loaded_result_installed;
        let json_export_enabled =
            idle && current_result_installed && !loaded_result_installed && export_enabled;
        let yaml_export_enabled =
            idle && active_result_available && !stale_result && export_enabled;
        let export_disabled_reason = if stale_result {
            STALE_CALIBRATION_RESULT_REASON
        } else {
            export_reason.unwrap_or(
                "A current or loaded calibration result and writable Explorer directory are required.",
            )
        };
        let json_disabled_reason = if loaded_result_installed {
            "Loaded YAML result is active and has no Dataset provenance; export YAML instead."
        } else {
            export_disabled_reason
        };
        let result_source_label = self
            .loaded_result
            .as_ref()
            .map(|loaded| format!("Loaded YAML: {}", loaded.source));
        let snid_result = self.snid_draft.serial_number();
        let serial_number = snid_result.as_deref().unwrap_or("");

        let mut load_clicked = false;
        let mut json_export_clicked = false;
        let mut yaml_export_clicked = false;
        egui::CollapsingHeader::new("Calibration result")
            .id_salt("calibration_result_foldout")
            .default_open(true)
            .show(ui, |ui| {
                render_calibration_result(
                    ui,
                    self.active_calibration_solution().or_else(|| {
                        self.session
                            .latest_installed()
                            .map(|installed| &installed.solution)
                    }),
                    stale_result,
                    result_source_label.as_deref(),
                    loaded_result_installed,
                );
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    load_clicked = ui.button("Load Result").clicked();
                    json_export_clicked = ui
                        .add_enabled(json_export_enabled, egui::Button::new("Export JSON"))
                        .on_disabled_hover_text(json_disabled_reason)
                        .clicked();
                    yaml_export_clicked = ui
                        .add_enabled(yaml_export_enabled, egui::Button::new("Export YAML Result"))
                        .on_disabled_hover_text(export_disabled_reason)
                        .clicked();
                });
                if active_result_available && !export_enabled {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        export_reason
                            .unwrap_or("Select a writable Explorer directory before exporting."),
                    );
                }
            });

        if load_clicked {
            self.load_calibration_result_from_dialog();
        }
        if json_export_clicked {
            self.pending_export = self.json_export();
        }
        if yaml_export_clicked && let Some(solution) = self.active_calibration_solution().cloned() {
            self.pending_export = Some(CalibrationExport::Yaml(solution));
        }

        let eeprom_solution = self.active_calibration_solution().cloned();
        let eeprom_stale_result =
            latest_result_installed && !latest_result_current && eeprom_solution.is_none();
        let snid_error = snid_result.as_ref().err().map(String::as_str);
        let eeprom_disabled_reason = if eeprom_stale_result {
            Some(STALE_CALIBRATION_RESULT_REASON)
        } else {
            snid_error
        };

        ui.collapsing("EEPROM Provisioning", |ui| {
            self.render_eeprom_snid_editor(ui, snid_result.as_ref().map(String::as_str));
            if let Some(reason) = eeprom_disabled_reason {
                ui.colored_label(egui::Color32::YELLOW, reason);
            }
            self.eeprom.render_body(
                context,
                ui,
                eeprom_solution.as_ref(),
                serial_number,
                sftp_source,
                provision_target,
                eeprom_disabled_reason,
            );
        });
        self.eeprom.render_confirmation(
            context,
            provision_target,
            eeprom_solution.as_ref(),
            serial_number,
        );
    }

    fn render_controls(&mut self, ui: &mut egui::Ui) {
        if !self.intrinsics_value_editing {
            self.refresh_auto_intrinsics_fields();
        }
        let idle = self.active_job.is_none();
        let current_result_available = self.session.installed().is_some();
        let latest_result_stale =
            self.session.latest_installed().is_some() && !current_result_available;
        let setup_editable = !matches!(
            self.active_job,
            Some(CalibrationJobKind::Detect | CalibrationJobKind::Calibrate)
        );
        ui.horizontal_wrapped(|ui| {
            ui.heading("Intrinsic Calibration");
            ui.add_enabled_ui(setup_editable, |ui| {
                ui.separator();
                ui.label("Inner corners");
                ui.add(egui::DragValue::new(&mut self.board_cols).range(2..=256));
                ui.label("×");
                ui.add(egui::DragValue::new(&mut self.board_rows).range(2..=256));
                ui.label("Square size (mm)");
                ui.add(
                    egui::DragValue::new(&mut self.square_size)
                        .speed(0.1)
                        .range(0.001..=1.0e6),
                );
                if ui.button("Apply board").clicked() {
                    let previous = self.session.board();
                    if self.apply_board() {
                        let current = self.session.board();
                        let corner_layout_changed = previous.inner_cols != current.inner_cols
                            || previous.inner_rows != current.inner_rows;
                        if previous != current && !corner_layout_changed {
                            self.request_dataset_pnp_refresh();
                        }
                    }
                }
            });
        });
        ui.collapsing("Auto Capture", |ui| {
            let previous_mode = self.auto_capture_trigger_mode;
            ui.horizontal_wrapped(|ui| {
                ui.label("Trigger mode");
                for mode in AutoCaptureTriggerMode::ALL {
                    ui.selectable_value(&mut self.auto_capture_trigger_mode, mode, mode.label());
                }
            });
            if previous_mode != self.auto_capture_trigger_mode {
                match previous_mode {
                    AutoCaptureTriggerMode::DatasetGain => self.auto_capture_enabled = false,
                    AutoCaptureTriggerMode::GuidedPresetPose => {
                        self.stop_guided_capture(
                            "Guided Auto Capture stopped because the trigger mode changed.",
                        );
                        self.auto_capture_enabled = false;
                    }
                }
            }
            match self.auto_capture_trigger_mode {
                AutoCaptureTriggerMode::DatasetGain => {
                    let auto_capture_changed = ui
                        .checkbox(&mut self.auto_capture_enabled, "Enable Dataset-gain auto capture")
                        .on_hover_text(
                            "Preserves the existing flow: captures candidates whose Dataset Acceptance Gain exceeds Minimum auto Gain.",
                        )
                        .changed();
                    if auto_capture_changed {
                        self.refresh_runtime_auto_admission();
                        if self.auto_capture_enabled && !self.active_live_admission() {
                            self.status = "Auto Capture waits for one displayed frame and complete valid acceptance inputs."
                                .to_owned();
                        }
                    }
                    ui.weak(
                        "Dataset gain mode uses Minimum auto Gain from Dataset Acceptance. It does not follow preset poses.",
                    );
                    if let Some(assessment) = self.auto_capture.last_assessment.as_ref() {
                        self.render_dataset_assessment(ui, assessment);
                    } else {
                        ui.weak("Display a live frame to initialize runtime Dataset Acceptance.");
                    }
                }
                AutoCaptureTriggerMode::GuidedPresetPose => {
                    ui.weak(
                        "Guided mode triggers only when the detected board pose matches the current preset pose threshold; Minimum auto Gain is ignored.",
                    );
                    let pending_blocks_guided_start = self
                        .auto_capture
                        .pending
                        .iter()
                        .any(|candidate| candidate.intent != CandidateIntent::PreviewOnly);
                    let start_enabled = idle
                        && self.active_live_admission()
                        && !pending_blocks_guided_start
                        && self.session.items().len() < MAX_DATASET_ITEMS;
                    let start_reason = if !idle {
                        "Wait for the active calibration operation."
                    } else if pending_blocks_guided_start {
                        "Wait for the current auto-capture candidate. Live preview detection will be cancelled automatically when Guided starts."
                    } else if self.session.items().len() >= MAX_DATASET_ITEMS {
                        "Dataset is full."
                    } else {
                        "Display one live frame with valid K/D12 inputs before starting."
                    };
                    match self.guided_capture.as_ref().map(|runtime| runtime.state) {
                        None | Some(GuidedCaptureState::Complete) => {
                            if ui
                                .add_enabled(start_enabled, egui::Button::new("Start guided"))
                                .on_disabled_hover_text(start_reason)
                                .clicked()
                            {
                                self.start_guided_capture();
                            } else if !start_enabled {
                                ui.weak(start_reason);
                            }
                        }
                        Some(GuidedCaptureState::Running) => {
                            ui.horizontal_wrapped(|ui| {
                                if ui.button("Pause").clicked() {
                                    self.pause_guided_capture();
                                }
                                if ui.button("Stop").clicked() {
                                    self.stop_guided_capture("Guided Auto Capture stopped by user.");
                                }
                            });
                        }
                        Some(GuidedCaptureState::Paused) => {
                            ui.horizontal_wrapped(|ui| {
                                if ui.button("Resume").clicked() {
                                    self.resume_guided_capture();
                                }
                                if ui.button("Stop").clicked() {
                                    self.stop_guided_capture("Guided Auto Capture stopped by user.");
                                }
                            });
                        }
                    }
                    if let Some(runtime) = self.guided_capture.as_ref() {
                        ui.monospace(runtime.current_step_label());
                        if let Some(assessment) = runtime.last_assessment.as_ref() {
                            ui.monospace(format!(
                                "Pose error {:.2}/{:.2} · hold {}/{}",
                                assessment.pose_error_score,
                                GUIDED_POSE_MATCH_SCORE_LIMIT,
                                runtime.hold_frames,
                                GUIDED_CAPTURE_HOLD_FRAMES
                            ));
                            ui.monospace(format!(
                                "XYZ {:.3}/{:.3} {:.3}/{:.3} {:.3}/{:.3} · RPY {:.1}°/{:.1}° {:.1}°/{:.1}° {:.1}°/{:.1}°",
                                assessment.error.x,
                                GUIDED_POSE_X_TOLERANCE,
                                assessment.error.y,
                                GUIDED_POSE_Y_TOLERANCE,
                                assessment.error.z,
                                GUIDED_POSE_Z_TOLERANCE,
                                assessment.error.roll_degrees,
                                GUIDED_POSE_ROLL_TOLERANCE_DEGREES,
                                assessment.error.pitch_degrees,
                                GUIDED_POSE_PITCH_TOLERANCE_DEGREES,
                                assessment.error.yaw_degrees,
                                GUIDED_POSE_YAW_TOLERANCE_DEGREES
                            ));
                            if let Some(reason) = assessment.reason.as_deref() {
                                ui.weak(reason);
                            }
                        } else {
                            ui.weak("Start guided capture, then move the board into the displayed target pose.");
                        }
                    }
                    if let Some(assessment) = self.auto_capture.last_assessment.as_ref() {
                        ui.collapsing("Advanced Dataset diagnostics", |ui| {
                            self.render_dataset_assessment(ui, assessment);
                        });
                    }
                }
            }
        });
        let mut intrinsics_changed = false;
        let mut intrinsics_editing = false;
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(setup_editable, |ui| {
                intrinsics_changed |= ui
                    .checkbox(&mut self.auto_intrinsics, "Auto initial intrinsics")
                    .on_hover_text("Auto: fx=fy=900 px, cx=width/2, cy=height/2, D12=0")
                    .changed();
                ui.label("fx");
                let response = ui.add_enabled(
                    !self.auto_intrinsics,
                    egui::DragValue::new(&mut self.fx)
                        .speed(1.0)
                        .update_while_editing(false),
                );
                observe_intrinsics_value_response(
                    response,
                    &mut intrinsics_changed,
                    &mut intrinsics_editing,
                );
                ui.label("fy");
                let response = ui.add_enabled(
                    !self.auto_intrinsics,
                    egui::DragValue::new(&mut self.fy)
                        .speed(1.0)
                        .update_while_editing(false),
                );
                observe_intrinsics_value_response(
                    response,
                    &mut intrinsics_changed,
                    &mut intrinsics_editing,
                );
                ui.label("cx");
                let response = ui.add_enabled(
                    !self.auto_intrinsics,
                    egui::DragValue::new(&mut self.cx)
                        .speed(1.0)
                        .update_while_editing(false),
                );
                observe_intrinsics_value_response(
                    response,
                    &mut intrinsics_changed,
                    &mut intrinsics_editing,
                );
                ui.label("cy");
                let response = ui.add_enabled(
                    !self.auto_intrinsics,
                    egui::DragValue::new(&mut self.cy)
                        .speed(1.0)
                        .update_while_editing(false),
                );
                observe_intrinsics_value_response(
                    response,
                    &mut intrinsics_changed,
                    &mut intrinsics_editing,
                );
            });
            ui.separator();
            if ui
                .add_enabled(
                    idle && self.session.items().iter().any(|item| item.enabled),
                    egui::Button::new("Detect"),
                )
                .clicked()
            {
                self.start_detection();
            }
            let readiness = self.initial_intrinsics().and_then(|initial| {
                self.session
                    .calibration_snapshot(initial)
                    .map_err(|error| error.to_string())
            });
            if ui
                .add_enabled(idle && readiness.is_ok(), egui::Button::new("Calibrate"))
                .on_disabled_hover_text(readiness.as_ref().err().cloned().unwrap_or_default())
                .clicked()
            {
                self.start_calibration();
            }
            if ui
                .add_enabled(
                    idle && self.auto_capture.pending.is_empty() && current_result_available,
                    egui::Button::new("Use result as initial K+D12"),
                )
                .on_disabled_hover_text(if latest_result_stale {
                    STALE_CALIBRATION_RESULT_REASON
                } else {
                    "Requires an installed result and no active calibration or live candidate."
                })
                .clicked()
            {
                self.use_installed_result_as_initial_intrinsics();
            }
            if ui
                .add_enabled(self.active_job.is_some(), egui::Button::new("Cancel"))
                .clicked()
            {
                self.cancel_active_job();
            }
        });
        ui.collapsing("Initial distortion D12", |ui| {
            ui.weak(
                "Manual values seed Calibrate and Dataset PnP. Auto initial intrinsics uses D12=0.",
            );
            let distortion_enabled = setup_editable && !self.auto_intrinsics;
            egui::Grid::new("calibration_initial_distortion")
                .num_columns(8)
                .striped(true)
                .show(ui, |ui| {
                    for (index, name) in INITIAL_DISTORTION_NAMES.iter().enumerate() {
                        ui.label(*name);
                        let response = ui.add_enabled(
                            distortion_enabled,
                            egui::DragValue::new(&mut self.initial_distortion_coefficients[index])
                                .speed(0.0001)
                                .update_while_editing(false),
                        );
                        observe_intrinsics_value_response(
                            response,
                            &mut intrinsics_changed,
                            &mut intrinsics_editing,
                        );
                        if (index + 1) % 4 == 0 {
                            ui.end_row();
                        }
                    }
                });
        });
        self.intrinsics_value_editing = intrinsics_editing;
        if intrinsics_changed {
            self.refresh_runtime_auto_admission();
            self.request_dataset_pnp_refresh();
        }
    }

    // Dataset Acceptance 与表格通过中间分割线分配侧栏高度；任一折叠后另一项铺满。
    const DATASET_ACCEPTANCE_MIN_VIEWPORT_HEIGHT: f32 = 96.0;
    const DATASET_ACCEPTANCE_MAX_VIEWPORT_HEIGHT: f32 = 420.0;

    /// 选择当前统一标定几何：优先 Dataset 中首个启用 Found 项，空 Dataset 再退回 live binding。
    fn dataset_acceptance_image_size(&self) -> Option<CalibrationImageSize> {
        self.session
            .items()
            .iter()
            .filter(|item| item.enabled)
            .find_map(|item| match &item.status {
                CalibrationItemStatus::Found(detection) => Some(detection.image_size),
                _ => None,
            })
            .or_else(|| {
                self.session
                    .initial_intrinsics_binding()
                    .map(|binding| binding.reference_image_size)
            })
    }

    /// Dataset 进度不按 provenance 筛选；只有 PnP 仍要求当前 K/D 与统一图像尺寸一致。
    fn dataset_acceptance_assessment(&self) -> Option<AutoAdmissionAssessment> {
        let criteria = self
            .acceptance_draft
            .parse()
            .unwrap_or_else(|_| self.acceptance_last_valid_criteria.clone());
        let image_size = self.dataset_acceptance_image_size()?;
        let pnp_binding = self.dataset_pnp_binding(image_size);
        self.session
            .assess_dataset_acceptance(image_size, &criteria, pnp_binding.as_ref())
            .ok()
    }

    fn select_dataset_item_for_preview(&mut self, id: CalibrationItemId) {
        if self.session.set_selected(id).is_ok() {
            self.display_layer = CalibrationDisplayLayer::DatasetImage;
        }
    }

    fn render_dataset_acceptance_panel(&mut self, ui: &mut egui::Ui, default_body_height: f32) {
        let fallback_criteria = self.acceptance_last_valid_criteria.clone();
        let mut progress = self.dataset_acceptance_assessment().map_or_else(
            || DatasetAcceptanceProgress::empty(&fallback_criteria),
            |assessment| DatasetAcceptanceProgress::from_assessment(&assessment),
        );
        progress.selected_item = self.session.selected();
        if let Some(observation) = self.live_acceptance_marker_observation() {
            progress.live_depth_range = Some((
                observation.minimum_board_depth,
                observation.maximum_board_depth,
            ));
            progress.live_pose_angles =
                Some((observation.tilt_degrees, observation.azimuth_degrees));
        }
        progress.live_field_cells = self.live_field_cells(&fallback_criteria);
        let state = DatasetAcceptancePanelState {
            has_live_context: self.live_admission_context.is_some(),
            admission_active: self.active_live_admission(),
            auto_capture_enabled: self.auto_capture_enabled,
        };
        let acceptance_viewport_height =
            default_body_height.max(Self::DATASET_ACCEPTANCE_MIN_VIEWPORT_HEIGHT);
        let render_result = render_dataset_acceptance(
            ui,
            &mut self.acceptance_draft,
            &progress,
            state,
            acceptance_viewport_height,
        );
        if let Some(action) = render_result.config_action {
            self.handle_acceptance_config_action(action);
        }
        if let Some(id) = render_result.selected_item {
            self.select_dataset_item_for_preview(id);
        }
        self.apply_acceptance_render_result(render_result.changed, render_result.editing);
        self.dataset_acceptance_expanded = render_result.expanded;
    }

    fn dataset_acceptance_body_height(&self, sidebar_height: f32) -> f32 {
        let upper = sidebar_height
            .max(Self::DATASET_ACCEPTANCE_MAX_VIEWPORT_HEIGHT)
            .max(Self::DATASET_ACCEPTANCE_MIN_VIEWPORT_HEIGHT);
        if self.dataset_acceptance_expanded && self.dataset_table_expanded {
            (sidebar_height * self.dataset_split_ratio)
                .clamp(Self::DATASET_ACCEPTANCE_MIN_VIEWPORT_HEIGHT, upper)
        } else {
            upper
        }
    }

    fn render_dataset_splitter(&mut self, ui: &mut egui::Ui, sidebar_height: f32) {
        let width = ui.available_width().max(1.0);
        let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 8.0), egui::Sense::drag());
        let stroke = ui.style().interact(&response).fg_stroke;
        ui.painter().line_segment(
            [
                egui::pos2(rect.left(), rect.center().y),
                egui::pos2(rect.right(), rect.center().y),
            ],
            stroke,
        );
        if response.hovered() || response.dragged() {
            ui.output_mut(|output| output.cursor_icon = egui::CursorIcon::ResizeVertical);
        }
        if response.dragged() && sidebar_height > 1.0 {
            let delta_y = ui.input(|input| input.pointer.delta().y);
            self.dataset_split_ratio =
                (self.dataset_split_ratio + delta_y / sidebar_height).clamp(0.12, 0.88);
            ui.ctx().request_repaint();
        }
    }

    fn render_dataset_table_panel(&mut self, ui: &mut egui::Ui) {
        let foldout = egui::CollapsingHeader::new("Dataset table")
            .id_salt("calibration_dataset_table_panel")
            .default_open(self.dataset_table_expanded)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("calibration_dataset_sidebar")
                    .max_height(ui.available_height())
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.render_dataset(ui, false));
            });
        self.dataset_table_expanded = !foldout.fully_closed();
    }

    fn apply_acceptance_render_result(&mut self, changed: bool, editing: bool) {
        if changed {
            match self.acceptance_draft.parse() {
                Ok(criteria) => {
                    self.acceptance_last_valid_criteria = criteria;
                    self.refresh_runtime_auto_admission();
                }
                Err(error) if !editing => {
                    self.acceptance_draft.error = Some(error);
                }
                Err(_) => {}
            }
        } else if !editing && self.acceptance_draft.error.is_none() {
            if let Err(error) = self.acceptance_draft.parse() {
                self.acceptance_draft.error = Some(error);
            }
        }
    }

    fn handle_acceptance_config_action(&mut self, action: DatasetAcceptanceConfigAction) {
        match action {
            DatasetAcceptanceConfigAction::LoadYaml => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Dataset Acceptance YAML", &["yaml", "yml"])
                    .pick_file()
                else {
                    return;
                };
                match std::fs::read_to_string(&path) {
                    Ok(yaml) => {
                        let source = path.display().to_string();
                        self.load_acceptance_config_from_str(&yaml, &source);
                    }
                    Err(error) => self.report_acceptance_config_error(format!(
                        "Failed to read Dataset Acceptance YAML from {}: {error}",
                        path.display()
                    )),
                }
            }
            DatasetAcceptanceConfigAction::SaveYaml => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Dataset Acceptance YAML", &["yaml", "yml"])
                    .set_file_name(DEFAULT_DATASET_ACCEPTANCE_CONFIG_FILE_NAME)
                    .save_file()
                else {
                    return;
                };
                self.save_acceptance_config_to_path(&path);
            }
            DatasetAcceptanceConfigAction::LoadDefault => {
                self.load_acceptance_config_from_str(
                    DEFAULT_DATASET_ACCEPTANCE_CONFIG,
                    "packaged default",
                );
            }
        }
    }

    fn load_acceptance_config_from_str(&mut self, yaml: &str, source: &str) {
        match DatasetAcceptanceDraft::from_yaml_str(yaml) {
            Ok(draft) => {
                let criteria = draft
                    .parse()
                    .expect("validated Dataset Acceptance YAML draft must parse");
                self.acceptance_draft = draft;
                self.acceptance_last_valid_criteria = criteria;
                self.refresh_runtime_auto_admission();
                self.status = format!("Loaded Dataset Acceptance YAML from {source}.");
            }
            Err(error) => self.report_acceptance_config_error(error),
        }
    }

    fn save_acceptance_config_to_path(&mut self, path: &std::path::Path) {
        match self.acceptance_draft.to_yaml_string() {
            Ok(yaml) => match std::fs::write(path, yaml) {
                Ok(()) => {
                    self.acceptance_draft.error = None;
                    self.status = format!("Saved Dataset Acceptance YAML to {}.", path.display());
                }
                Err(error) => self.report_acceptance_config_error(format!(
                    "Failed to save Dataset Acceptance YAML to {}: {error}",
                    path.display()
                )),
            },
            Err(error) => self.report_acceptance_config_error(error),
        }
    }

    fn report_acceptance_config_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.acceptance_draft.error = Some(message.clone());
        self.status = message;
    }

    fn render_dataset(&mut self, ui: &mut egui::Ui, show_heading: bool) {
        if show_heading {
            ui.heading(format!("Dataset ({})", self.session.items().len()));
        }
        let mut toggle = None;
        let mut select = None;
        let installed = self.session.latest_installed();
        let selected = self.session.selected();
        let items = self.session.items();
        let assessment = self.dataset_acceptance_assessment();
        let contributions = assessment
            .as_ref()
            .map(|assessment| {
                assessment
                    .item_contributions
                    .iter()
                    .map(|contribution| (contribution.item_id, contribution))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let idle = self.active_job.is_none();
        let max_rmse = installed
            .map(|installed| {
                installed
                    .solution
                    .views
                    .iter()
                    .map(|view| view.reprojection_rmse)
                    .fold(0.0_f64, f64::max)
            })
            .unwrap_or(0.0)
            .max(1e-9);

        egui::ScrollArea::horizontal()
            .id_salt("calibration_dataset_hscroll")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                TableBuilder::new(ui)
                    .id_salt("calibration_dataset_table")
                    .striped(true)
                    .resizable(true)
                    .auto_shrink([false, false])
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::initial(42.0).at_least(36.0).clip(true))
                    .column(Column::initial(70.0).at_least(52.0).clip(true))
                    .column(Column::initial(118.0).at_least(96.0).clip(true))
                    .column(Column::initial(120.0).at_least(72.0).clip(true))
                    .column(Column::initial(58.0).at_least(48.0).clip(true))
                    .column(Column::initial(88.0).at_least(72.0).clip(true))
                    .column(Column::initial(62.0).at_least(52.0).clip(true))
                    .column(Column::initial(80.0).at_least(54.0).clip(true))
                    .column(Column::initial(76.0).at_least(58.0).clip(true))
                    .column(Column::initial(76.0).at_least(58.0).clip(true))
                    .column(Column::initial(58.0).at_least(48.0).clip(true))
                    .column(Column::initial(58.0).at_least(48.0).clip(true))
                    .column(Column::initial(58.0).at_least(48.0).clip(true))
                    .column(Column::initial(58.0).at_least(48.0).clip(true))
                    .column(Column::initial(58.0).at_least(48.0).clip(true))
                    .column(Column::initial(140.0).at_least(80.0).clip(true))
                    .header(24.0, |mut header| {
                        for heading in [
                            "Use",
                            "Status",
                            "Acceptance",
                            "Name",
                            "Source",
                            "Resolution",
                            "Corners",
                            "RMSE",
                            "Depth",
                            "Angle dir",
                            "Angle",
                            "Field Δ",
                            "Depth Δ",
                            "Pose Δ",
                            "Gain",
                            "Reason",
                        ] {
                            header.col(|ui| {
                                ui.strong(heading);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(26.0, items.len(), |mut row| {
                            let item = &items[row.index()];
                            let contribution = contributions.get(&item.id).copied();
                            row.col(|ui| {
                                let mut enabled = item.enabled;
                                if ui
                                    .add_enabled(idle, egui::Checkbox::new(&mut enabled, ""))
                                    .changed()
                                {
                                    toggle = Some((item.id, enabled));
                                }
                            });
                            row.col(|ui| {
                                let mut label = egui::RichText::new(status_label(&item.status));
                                if let Some(color) = status_color(&item.status) {
                                    label = label.color(color);
                                }
                                let mut response =
                                    ui.selectable_label(selected == Some(item.id), label);
                                if let CalibrationItemStatus::Failed(reason) = &item.status {
                                    response = response.on_hover_text(reason);
                                }
                                if response.clicked() {
                                    select = Some(item.id);
                                }
                            });
                            row.col(|ui| {
                                render_acceptance_status_cell(
                                    ui,
                                    &item.status,
                                    item.enabled,
                                    contribution,
                                    assessment.is_some(),
                                );
                            });
                            row.col(|ui| {
                                if ui
                                    .selectable_label(selected == Some(item.id), &item.display_name)
                                    .clicked()
                                {
                                    select = Some(item.id);
                                }
                            });
                            row.col(|ui| {
                                let label = match &item.input {
                                    CalibrationInputKey::File(_) => {
                                        self.sources.get(&item.id).map_or("Local", |source| {
                                            if source.remote() { "SFTP" } else { "Local" }
                                        })
                                    }
                                    CalibrationInputKey::StreamCapture(_) => "RTSP",
                                };
                                ui.label(label);
                            });
                            row.col(|ui| {
                                ui.monospace(detection_size(&item.status).map_or_else(
                                    || "—".to_owned(),
                                    |size| format!("{}×{}", size.width, size.height),
                                ));
                            });
                            row.col(|ui| {
                                ui.monospace(match &item.status {
                                    CalibrationItemStatus::Found(detection) => {
                                        detection.corners.len().to_string()
                                    }
                                    _ => "—".to_owned(),
                                });
                            });
                            row.col(|ui| {
                                let metric = calibration_metric(installed, item.id);
                                render_rmse_cell(ui, metric, max_rmse);
                            });
                            row.col(|ui| {
                                render_pnp_depth_cell(
                                    ui,
                                    item.pnp_observation.as_ref(),
                                    contribution.map(|value| &value.pnp_state),
                                );
                            });
                            row.col(|ui| {
                                render_pnp_direction_cell(
                                    ui,
                                    item.pnp_observation.as_ref(),
                                    contribution.map(|value| &value.pnp_state),
                                );
                            });
                            row.col(|ui| {
                                render_pnp_angle_cell(
                                    ui,
                                    item.pnp_observation.as_ref(),
                                    contribution.map(|value| &value.pnp_state),
                                );
                            });
                            row.col(|ui| {
                                render_admission_delta_cell(
                                    ui,
                                    contribution,
                                    item.enabled,
                                    "field cells",
                                    |_| false,
                                    |value| value.field_gain,
                                );
                            });
                            row.col(|ui| {
                                render_admission_delta_cell(
                                    ui,
                                    contribution,
                                    item.enabled,
                                    "depth bins",
                                    |value| value.pnp_state.is_blocked() || !value.depth_covered,
                                    |value| value.depth_gain,
                                );
                            });
                            row.col(|ui| {
                                render_admission_delta_cell(
                                    ui,
                                    contribution,
                                    item.enabled,
                                    "pose bins",
                                    |value| value.pnp_state.is_blocked() || !value.pose_covered,
                                    |value| value.pose_gain,
                                );
                            });
                            row.col(|ui| {
                                render_total_gain_cell(ui, contribution, item.enabled);
                            });
                            row.col(|ui| {
                                let reason = match &item.status {
                                    CalibrationItemStatus::Failed(reason) => reason.as_str(),
                                    CalibrationItemStatus::NotFound { .. } => {
                                        "Chessboard not found"
                                    }
                                    _ => "—",
                                };
                                ui.label(reason).on_hover_text(reason);
                            });
                        });
                    });
            });

        if let Some((id, enabled)) = toggle {
            if self.session.set_enabled(id, enabled).is_ok() {
                self.coverage_dirty = true;
            }
        }
        if let Some(id) = select {
            self.select_dataset_item_for_preview(id);
        }
    }

    fn render_inspection(&mut self, ui: &mut egui::Ui) {
        ui.heading("Preview and constraints");
        if let Some(id) = self.session.selected() {
            if let Some(source) = self.sources.get(&id) {
                ui.monospace(&source.display_name);
            }
            ui.horizontal_wrapped(|ui| {
                let has_heatmap = self.coverage.is_some();
                ui.add_enabled_ui(has_heatmap, |ui| {
                    ui.selectable_value(
                        &mut self.preview_mode,
                        CalibrationPreviewMode::Heatmap,
                        "Heatmap",
                    );
                    ui.selectable_value(
                        &mut self.preview_mode,
                        CalibrationPreviewMode::Overlay,
                        "Overlay",
                    );
                });
                ui.selectable_value(
                    &mut self.preview_mode,
                    CalibrationPreviewMode::InputImage,
                    "Input image",
                );
                if let Some(coverage) = &self.coverage {
                    ui.monospace(format!("{} views", coverage.enabled_views));
                }
                ui.separator();
                ui.monospace(format!("{:.0}%", self.preview_viewport.zoom * 100.0));
                if ui.small_button("Fit").clicked() {
                    self.preview_viewport.fit_on_next_frame = true;
                }
                ui.checkbox(&mut self.preview_viewport.horizontal_flip, "Flip X")
                    .on_hover_text(
                        "Mirror the Dataset preview image and all preview overlays horizontally.",
                    );
            });
            if self.preview_mode != CalibrationPreviewMode::Heatmap {
                ui.weak(
                    "Overlay legend: green detected corners, red installed-result reprojection, blue current GUI K+D12 PnP reprojection.",
                );
            }
            let image_size = self
                .sources
                .get(&id)
                .and_then(|source| source.preview.as_ref())
                .map(|preview| egui::vec2(preview.frame.width as f32, preview.frame.height as f32))
                .or_else(|| {
                    self.session
                        .items()
                        .iter()
                        .find(|item| item.id == id)
                        .and_then(|item| detection_size(&item.status))
                        .map(|size| egui::vec2(size.width as f32, size.height as f32))
                })
                .unwrap_or_else(|| egui::vec2(16.0, 9.0));
            let preview_size = contain_fit_size(ui.available_size(), image_size);
            let (rect, response) =
                ui.allocate_exact_size(preview_size, egui::Sense::click_and_drag());
            self.paint_preview(ui, &response, rect, id);
        } else {
            ui.weak("Select a dataset item to inspect its image and residuals.");
        }
    }

    fn paint_preview(
        &mut self,
        ui: &egui::Ui,
        response: &egui::Response,
        rect: egui::Rect,
        id: CalibrationItemId,
    ) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, egui::Color32::from_gray(20));
        let Some(source) = self.sources.get(&id) else {
            return;
        };
        let Some(preview) = &source.preview else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Run Detect to load preview",
                egui::FontId::proportional(14.0),
                egui::Color32::GRAY,
            );
            return;
        };
        let input_texture_id = preview.texture.id();
        let heatmap_texture_id = self.coverage.as_ref().map(|coverage| coverage.density.id());
        let width = preview.frame.width;
        let height = preview.frame.height;
        let image_size = egui::vec2(width as f32, height as f32);
        self.preview_viewport.reset_for(id);
        let image_rect = self
            .preview_viewport
            .interact(ui, response, rect, image_size);
        let texture_uv = viewer_texture_uv(self.preview_viewport.horizontal_flip);
        let layers = preview_layers(self.preview_mode, heatmap_texture_id.is_some());
        if layers.input {
            painter.image(
                input_texture_id,
                image_rect,
                texture_uv,
                egui::Color32::WHITE,
            );
        }
        if let (Some(texture_id), Some(alpha)) = (heatmap_texture_id, layers.heatmap_alpha) {
            painter.image(
                texture_id,
                image_rect,
                texture_uv,
                egui::Color32::from_white_alpha(alpha),
            );
            paint_heatmap_guides(&painter, image_rect);
        }
        if !layers.input {
            return;
        }
        let Some(item) = self.session.items().iter().find(|item| item.id == id) else {
            return;
        };
        let CalibrationItemStatus::Found(detection) = &item.status else {
            return;
        };
        let horizontal_flip = self.preview_viewport.horizontal_flip;
        let map = |point| image_point_to_preview(point, image_rect, width, height, horizontal_flip);
        let projected = calibration_view(self.session.latest_installed(), id)
            .map(|view| view.projected_points.as_slice());
        let current_dataset_pnp = item.pnp_observation.as_ref().and_then(|observation| {
            let binding = self.dataset_pnp_binding(detection.image_size)?;
            (observation.binding_digest == binding.digest).then_some((observation, binding))
        });
        let current_gui_projected =
            current_dataset_pnp
                .as_ref()
                .and_then(|(observation, binding)| {
                    projected_board_corners_for_preview(
                        observation,
                        &binding.initial_intrinsics,
                        self.session.board(),
                        image_rect,
                        width,
                        height,
                        horizontal_flip,
                    )
                });
        for (index, observed) in detection.corners.iter().copied().enumerate() {
            let observed_position = map(observed);
            if let Some(projected) = projected.and_then(|points| points.get(index)).copied() {
                paint_reprojection_vector(&painter, observed_position, map(projected));
            }
            if let Some(Some(position)) = current_gui_projected
                .as_ref()
                .and_then(|points| points.get(index))
            {
                paint_current_gui_reprojection_point(&painter, *position);
            }
            painter.circle_stroke(
                observed_position,
                3.0,
                egui::Stroke::new(1.25, OBSERVED_POINT_COLOR),
            );
        }
        if let Some((observation, binding)) = current_dataset_pnp.as_ref()
            && let Some(projection) = pose_axis_projection(
                observation,
                &binding.initial_intrinsics,
                self.session.board(),
                image_rect,
                width,
                height,
                horizontal_flip,
            )
        {
            paint_pose_axis_overlay(&painter, projection);
        }
    }

    fn apply_board(&mut self) -> bool {
        let board = match BoardSpec::new(self.board_cols, self.board_rows, self.square_size) {
            Ok(board) => board,
            Err(error) => {
                self.status = format!("Invalid board: {error}");
                return false;
            }
        };
        let previous = self.session.board();
        let corner_layout_changed =
            previous.inner_cols != board.inner_cols || previous.inner_rows != board.inner_rows;
        if let Err(error) = self.session.set_board(board) {
            self.status = format!("Invalid board: {error}");
            return false;
        }
        if previous != board {
            if !self.auto_capture.pending.is_empty() {
                self.cancel_auto_candidates_matching(
                    "Live detection candidate cancelled because the board specification changed.",
                    |_| true,
                );
            }
            if self.guided_capture.is_some() {
                self.stop_guided_capture(
                    "Guided Auto Capture stopped because the board specification changed.",
                );
            }
            self.refresh_runtime_auto_admission();
        }
        if corner_layout_changed {
            self.coverage_dirty = true;
            self.status =
                "Inner-corner layout applied; existing detections were invalidated.".to_owned();
        } else if previous != board {
            self.status =
                "Square size applied in mm; detections were preserved and calibration was invalidated."
                    .to_owned();
        } else {
            self.status = "Board unchanged; detections were preserved.".to_owned();
        }
        true
    }

    fn start_detection(&mut self) {
        if !self.apply_board() {
            return;
        }
        self.session.reset_detections();
        self.coverage = None;
        self.coverage_dirty = false;
        let ids = self
            .session
            .items()
            .iter()
            .filter(|item| item.enabled)
            .map(|item| item.id)
            .collect();
        self.start_detection_items(ids);
    }

    fn start_detection_items(&mut self, ids: Vec<CalibrationItemId>) {
        if ids.is_empty() {
            self.status = "No calibration images are enabled for detection.".to_owned();
            return;
        }
        debug_assert!(self.active_detection_batch.is_none());
        debug_assert!(self.pending_reads.is_empty());
        debug_assert!(self.pending_dataset_loaded.is_empty());

        let batch_id = self.next_detection_batch_id;
        self.next_detection_batch_id = self.next_detection_batch_id.wrapping_add(1).max(1);
        let calibration_cancellation = CalibrationCancellation::default();
        let dataset_pose_seed = self.dataset_pose_seed();
        let mut batch = DetectionBatch {
            id: batch_id,
            total: 0,
            completed: 0,
            reserved_encoded_bytes: 0,
            cancel_requested: false,
            terminal_status: None,
            cancellations: HashMap::new(),
            active_remote_sources: HashMap::new(),
        };
        let mut seen = HashSet::new();

        for id in ids.into_iter().filter(|id| seen.insert(*id)) {
            let input = self
                .session
                .items()
                .iter()
                .find(|item| item.id == id)
                .map(|item| item.input.clone());
            let is_stream = matches!(input, Some(CalibrationInputKey::StreamCapture(_)));
            let reference = input.and_then(|input| input.file_reference().cloned());
            let source = self
                .sources
                .get(&id)
                .and_then(CalibrationSource::file_binding);
            let token = match if is_stream {
                self.session.begin_encoded_detection(id)
            } else {
                self.session.begin_detection(id)
            } {
                Ok(token) => token,
                Err(error) => {
                    self.status = error.to_string();
                    continue;
                }
            };
            batch.total += 1;
            if token.source_revision().encoded_bytes() > MAX_ENCODED_PNG_BYTES {
                let message = format!(
                    "Encoded calibration image is {} bytes, limit is {} bytes.",
                    token.source_revision().encoded_bytes(),
                    MAX_ENCODED_PNG_BYTES
                );
                let _ = self.session.install_failure(&token, message.clone());
                batch.completed += 1;
                self.status = message;
                continue;
            }
            let file_cancellation = FsCancellation::default();
            batch.cancellations.insert(
                id,
                ActiveCancellation {
                    token: token.clone(),
                    file_system: file_cancellation.clone(),
                    calibration: calibration_cancellation.clone(),
                },
            );
            if is_stream {
                let direct = self
                    .sources
                    .get(&id)
                    .map(|source| source.encoded_png(token.source_revision()));
                match direct {
                    Some(Ok(Some(encoded))) => {
                        let pose_request = self.dataset_pose_request_for_image(encoded.image_size);
                        self.pending_dataset_loaded
                            .push_back(LoadedDetectionJob::from_encoded(
                                batch_id,
                                EncodedDetectionRequest::Dataset(token),
                                encoded,
                                calibration_cancellation.clone(),
                                pose_request,
                            ))
                    }
                    Some(Ok(None)) | None => {
                        let message = "Stream dataset source binding is unavailable.".to_owned();
                        let _ = self.session.install_failure(&token, message.clone());
                        batch.completed += 1;
                        self.status = message;
                    }
                    Some(Err(message)) => {
                        let _ = self.session.install_failure(&token, message.clone());
                        batch.completed += 1;
                        self.status = message;
                    }
                }
                continue;
            }
            let (Some(reference), Some((file_system, remote))) = (reference, source) else {
                let message = "Dataset source binding is unavailable.".to_owned();
                let _ = self.session.install_failure(&token, message.clone());
                batch.completed += 1;
                self.status = message;
                continue;
            };
            self.pending_reads.push_back(ReadJob::new(
                batch_id,
                token,
                ReadSource {
                    source_id: reference.source_id.clone(),
                    remote,
                },
                file_system,
                reference,
                file_cancellation,
                calibration_cancellation.clone(),
                dataset_pose_seed.clone(),
            ));
        }

        if batch.total == 0 {
            self.status = "No calibration images could be queued for detection.".to_owned();
            return;
        }
        self.active_job = Some(CalibrationJobKind::Detect);
        self.active_detection_batch = Some(batch);
        self.dispatch_detection_pipeline();
        self.finish_detection_batch_if_ready();
    }

    fn dispatch_detection_pipeline(&mut self) {
        self.dispatch_loaded_detections();
        self.dispatch_pending_reads();
    }

    fn dispatch_pending_reads(&mut self) {
        let attempts = self.pending_reads.len();
        for _ in 0..attempts {
            let Some(job) = self.pending_reads.pop_front() else {
                break;
            };
            let Some(batch) = self.active_detection_batch.as_ref() else {
                break;
            };
            if batch.cancel_requested || batch.id != job.batch_id {
                self.pending_reads.push_front(job);
                break;
            }
            let source_blocked = job.source.remote
                && batch
                    .active_remote_sources
                    .get(&job.source.source_id)
                    .copied()
                    .unwrap_or(0)
                    >= REMOTE_READS_PER_SOURCE;
            let budget_blocked = batch
                .reserved_encoded_bytes
                .saturating_add(job.reserved_bytes)
                > MAX_INFLIGHT_ENCODED_BYTES;
            if source_blocked || budget_blocked {
                self.pending_reads.push_back(job);
                continue;
            }
            let source = job.source.clone();
            let reserved_bytes = job.reserved_bytes;
            match self.detection_pipeline.try_submit_read(job) {
                Ok(()) => {
                    let batch = self
                        .active_detection_batch
                        .as_mut()
                        .expect("active batch was checked");
                    batch.reserved_encoded_bytes =
                        batch.reserved_encoded_bytes.saturating_add(reserved_bytes);
                    if source.remote {
                        let active = batch
                            .active_remote_sources
                            .entry(source.source_id)
                            .or_insert(0);
                        *active = active.saturating_add(1);
                    }
                }
                Err(TrySendError::Full(job)) => {
                    self.pending_reads.push_front(job);
                    break;
                }
                Err(TrySendError::Disconnected(job)) => {
                    let message = "Calibration read workers stopped unexpectedly.".to_owned();
                    let _ = self.session.install_failure(&job.token, message.clone());
                    self.complete_detection_item(job.batch_id, job.token.item_id, 0);
                    self.abort_detection_batch(message);
                    break;
                }
            }
        }
    }

    fn dispatch_loaded_detections(&mut self) {
        loop {
            let Some(job) = self.pending_dataset_loaded.pop_front() else {
                break;
            };
            match self.detection_pipeline.try_submit_detection(job) {
                Ok(()) => {}
                Err(TrySendError::Full(job)) => {
                    self.pending_dataset_loaded.push_front(job);
                    break;
                }
                Err(TrySendError::Disconnected(job)) => {
                    let message = "Calibration detection workers stopped unexpectedly.".to_owned();
                    match &job.request {
                        EncodedDetectionRequest::Dataset(token) => {
                            let _ = self.session.install_failure(token, message.clone());
                            self.complete_detection_item(
                                job.batch_id,
                                token.item_id,
                                job.reserved_bytes,
                            );
                            self.abort_detection_batch(message);
                        }
                        EncodedDetectionRequest::Candidate(_) => {
                            self.status = message;
                        }
                    }
                    break;
                }
            }
        }
    }

    fn poll_detection_pipeline(&mut self, context: &egui::Context) {
        loop {
            match self.detection_pipeline.try_detection_event() {
                Ok(event) => self.handle_detection_event(context, event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if matches!(self.active_job, Some(CalibrationJobKind::Detect)) {
                        self.abort_detection_batch(
                            "Calibration detection workers stopped unexpectedly.".to_owned(),
                        );
                    }
                    if !self.auto_capture.pending.is_empty() {
                        self.cancel_auto_candidates_matching(
                            "Calibration detection workers stopped unexpectedly.",
                            |_| true,
                        );
                    }
                    break;
                }
            }
        }
        self.dispatch_loaded_detections();

        loop {
            match self.detection_pipeline.try_read_event() {
                Ok(event) => {
                    self.handle_read_event(event);
                    self.dispatch_loaded_detections();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if matches!(self.active_job, Some(CalibrationJobKind::Detect)) {
                        self.abort_detection_batch(
                            "Calibration read workers stopped unexpectedly.".to_owned(),
                        );
                    }
                    break;
                }
            }
        }

        self.dispatch_pending_reads();
        self.finish_detection_batch_if_ready();
        self.dispatch_auto_candidate(context);
    }
    fn handle_read_event(&mut self, event: ReadStageEvent) {
        match event {
            ReadStageEvent::Started { batch_id, token } => {
                let Some(batch) = self.active_detection_batch.as_ref() else {
                    return;
                };
                if batch.id != batch_id || batch.cancel_requested {
                    return;
                }
                if let Err(error) = self.session.mark_reading(&token) {
                    self.status = error.to_string();
                }
            }
            ReadStageEvent::Finished(result) => self.handle_read_result(result),
        }
    }

    fn handle_read_result(&mut self, result: ReadStageResult) {
        let ReadStageResult {
            batch_id,
            token,
            source,
            reserved_bytes,
            result,
        } = result;
        let Some(batch) = self.active_detection_batch.as_mut() else {
            return;
        };
        if batch.id != batch_id {
            return;
        }
        if source.remote {
            let remove_source = batch
                .active_remote_sources
                .get_mut(&source.source_id)
                .is_some_and(|active| {
                    *active = active.saturating_sub(1);
                    *active == 0
                });
            if remove_source {
                batch.active_remote_sources.remove(&source.source_id);
            }
        }
        let cancelled = batch.cancel_requested;
        match result {
            Ok(job) if !cancelled => match self.session.mark_detect_queued(&token) {
                Ok(()) => self.pending_dataset_loaded.push_back(job),
                Err(error) => {
                    self.status = error.to_string();
                    self.complete_detection_item(batch_id, token.item_id, reserved_bytes);
                }
            },
            Ok(_) => {
                let _ = self.session.cancel_detection(&token);
                self.complete_detection_item(batch_id, token.item_id, reserved_bytes);
            }
            Err(error) => {
                if cancelled || error.is_cancelled() {
                    let _ = self.session.cancel_detection(&token);
                } else {
                    let message = error.to_string();
                    let _ = self.session.install_failure(&token, message.clone());
                    self.status = message;
                }
                self.complete_detection_item(batch_id, token.item_id, reserved_bytes);
            }
        }
    }

    fn handle_detection_event(&mut self, context: &egui::Context, event: DetectionStageEvent) {
        match event {
            DetectionStageEvent::Started {
                batch_id,
                request: EncodedDetectionRequest::Dataset(token),
            } => {
                let Some(batch) = self.active_detection_batch.as_ref() else {
                    return;
                };
                if batch.id != batch_id || batch.cancel_requested {
                    return;
                }
                if let Err(error) = self.session.mark_detecting(&token) {
                    self.status = error.to_string();
                }
            }
            DetectionStageEvent::Started {
                request: EncodedDetectionRequest::Candidate(token),
                ..
            } => {
                if let Some(candidate) = self
                    .auto_capture
                    .pending
                    .iter_mut()
                    .find(|candidate| candidate.token == token)
                {
                    candidate.state = AutoCandidateState::Detecting;
                    self.status = match candidate.intent {
                        CandidateIntent::PreviewOnly => {
                            format!("Detecting board preview candidate {}.", token.id().get())
                        }
                        CandidateIntent::AutoCommit => {
                            format!("Detecting automatic candidate {}.", token.id().get())
                        }
                        CandidateIntent::GuidedMeasure => {
                            format!("Detecting guided pose measurement {}.", token.id().get())
                        }
                        CandidateIntent::GuidedCapture => {
                            format!("Detecting guided capture {}.", token.id().get())
                        }
                    };
                }
            }
            DetectionStageEvent::Finished(result) => self.handle_detection_result(context, result),
        }
    }

    fn handle_detection_result(&mut self, context: &egui::Context, result: DetectionStageResult) {
        let DetectionStageResult {
            batch_id,
            request,
            reserved_bytes,
            result,
        } = result;
        let token = match request {
            EncodedDetectionRequest::Candidate(token) => {
                self.complete_auto_candidate(
                    Some(context),
                    token.id(),
                    CandidateTerminal::Detection(result),
                );
                return;
            }
            EncodedDetectionRequest::Dataset(token) => token,
        };
        let Some(batch) = self.active_detection_batch.as_ref() else {
            return;
        };
        if batch.id != batch_id {
            return;
        }
        let cancelled = batch.cancel_requested;
        if cancelled {
            let _ = self.session.cancel_detection(&token);
        } else {
            match result {
                Ok(DetectionProduct {
                    source_revision,
                    outcome,
                    pnp_observation,
                    preview,
                }) => {
                    let found = matches!(
                        &outcome,
                        camera_toolbox_core::ChessboardDetectionOutcome::Found(_)
                    );
                    let pnp_observation = pnp_observation.filter(|observation| {
                        matches!(
                            &outcome,
                            camera_toolbox_core::ChessboardDetectionOutcome::Found(detection)
                                if self
                                    .dataset_pnp_binding(detection.image_size)
                                    .is_some_and(|binding| observation.binding_digest == binding.digest)
                        )
                    });
                    self.remember_stream_detection(
                        token.item_id,
                        &outcome,
                        pnp_observation.clone(),
                    );
                    match self.session.install_detection_with_pnp(
                        &token,
                        source_revision,
                        outcome,
                        pnp_observation,
                    ) {
                        Ok(()) => {
                            // 多 worker 的完成顺序不等于导入顺序；每个刚 Found 的图像立即成为预览目标。
                            if found {
                                let _ = self.session.set_selected(token.item_id);
                            }
                            if let Some(frame) = preview {
                                self.install_preview(context, token.item_id, frame);
                            }
                        }
                        Err(error) => {
                            let message = error.to_string();
                            let _ = self.session.install_failure(&token, message.clone());
                            self.status = message;
                        }
                    }
                }
                Err(error) if error.is_cancelled() => {
                    let _ = self.session.cancel_detection(&token);
                }
                Err(error) => {
                    let message = error.to_string();
                    let _ = self.session.install_failure(&token, message.clone());
                    self.status = message;
                }
            }
        }
        self.coverage_dirty = true;
        self.complete_detection_item(batch_id, token.item_id, reserved_bytes);
    }

    fn complete_detection_item(
        &mut self,
        batch_id: u64,
        item_id: CalibrationItemId,
        reserved_bytes: u64,
    ) {
        let Some(batch) = self.active_detection_batch.as_mut() else {
            return;
        };
        if batch.id != batch_id {
            return;
        }
        batch.reserved_encoded_bytes = batch.reserved_encoded_bytes.saturating_sub(reserved_bytes);
        if batch.cancellations.remove(&item_id).is_some() {
            batch.completed = batch.completed.saturating_add(1);
        }
    }

    fn cancel_active_job(&mut self) {
        match self.active_job {
            Some(CalibrationJobKind::Detect) => {
                self.request_detection_cancel("Detection cancelled.".to_owned());
            }
            Some(CalibrationJobKind::Calibrate) => {
                if let Some(cancellation) = &self.calibration_cancellation {
                    cancellation.cancel();
                }
                self.status =
                    "Cancel requested; waiting for the current OpenCV call boundary.".to_owned();
            }
            Some(CalibrationJobKind::DatasetPnpRefresh) => {
                if let Some(cancellation) = &self.calibration_cancellation {
                    cancellation.cancel();
                }
                self.status =
                    "Cancel requested; waiting for the current Dataset PnP refresh boundary."
                        .to_owned();
            }
            None => {}
        }
    }

    fn request_detection_cancel(&mut self, terminal_status: String) {
        let Some(batch) = self.active_detection_batch.as_mut() else {
            return;
        };
        batch.cancel_requested = true;
        batch.terminal_status = Some(terminal_status);
        for cancellation in batch.cancellations.values() {
            cancellation.cancel();
        }

        let pending_reads: Vec<_> = self.pending_reads.drain(..).collect();
        for job in pending_reads {
            let _ = self.session.cancel_detection(&job.token);
            self.complete_detection_item(job.batch_id, job.token.item_id, 0);
        }
        let pending_loaded: Vec<_> = self.pending_dataset_loaded.drain(..).collect();
        for job in pending_loaded {
            if let EncodedDetectionRequest::Dataset(token) = job.request {
                let _ = self.session.cancel_detection(&token);
                self.complete_detection_item(job.batch_id, token.item_id, job.reserved_bytes);
            }
        }
        self.status = "Cancel requested; waiting for active file/OpenCV operations.".to_owned();
        self.finish_detection_batch_if_ready();
    }

    fn abort_detection_batch(&mut self, status: String) {
        let Some(batch) = self.active_detection_batch.take() else {
            return;
        };
        for cancellation in batch.cancellations.values() {
            cancellation.cancel();
            let _ = self.session.cancel_detection(&cancellation.token);
        }
        self.pending_reads.clear();
        self.pending_dataset_loaded.clear();
        self.active_job = None;
        let pending: Vec<_> = self.pending_imports.drain(..).collect();
        if !pending.is_empty() {
            self.import_candidates(pending, false);
        }
        self.status = status;
    }

    fn finish_detection_batch_if_ready(&mut self) {
        let Some(batch) = self.active_detection_batch.as_ref() else {
            return;
        };
        if batch.completed < batch.total
            || !self.pending_reads.is_empty()
            || !self.pending_dataset_loaded.is_empty()
        {
            if !batch.cancel_requested {
                self.status = format!(
                    "Detecting calibration images: {}/{} completed…",
                    batch.completed, batch.total
                );
            }
            return;
        }

        let batch = self
            .active_detection_batch
            .take()
            .expect("batch was checked");
        self.active_job = None;
        let final_status = batch.terminal_status.unwrap_or_else(|| {
            format!(
                "Detection completed: {}/{} processed.",
                batch.completed, batch.total
            )
        });
        let pending: Vec<_> = self.pending_imports.drain(..).collect();
        if pending.is_empty() {
            self.status = final_status;
        } else {
            self.import_candidates(pending, !batch.cancel_requested);
        }
    }

    fn start_calibration(&mut self) {
        let Ok(initial) = self.initial_intrinsics() else {
            return;
        };
        let snapshot = match self.session.calibration_snapshot(initial) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.status = error.to_string();
                return;
            }
        };
        let cancellation = CalibrationCancellation::default();
        if let Err(error) = self.worker.send(WorkerCommand::Calibrate {
            snapshot,
            cancellation: cancellation.clone(),
        }) {
            self.status = error;
            return;
        }
        self.active_job = Some(CalibrationJobKind::Calibrate);
        self.calibration_cancellation = Some(cancellation);
        self.status = "Running Pangbot-compatible calibration…".to_owned();
    }

    fn request_dataset_pnp_refresh(&mut self) {
        if self.active_job.is_some() {
            self.pending_dataset_pnp_refresh = true;
            return;
        }
        self.pending_dataset_pnp_refresh = false;
        self.start_dataset_pnp_refresh();
    }

    fn drain_pending_dataset_pnp_refresh(&mut self) {
        if !self.pending_dataset_pnp_refresh || self.active_job.is_some() {
            return;
        }
        self.pending_dataset_pnp_refresh = false;
        self.start_dataset_pnp_refresh();
    }

    fn start_dataset_pnp_refresh(&mut self) {
        if self.active_job.is_some() {
            return;
        }
        let items =
            self.session
                .items()
                .iter()
                .filter_map(|item| {
                    let CalibrationItemStatus::Found(detection) = &item.status else {
                        return None;
                    };
                    let request = self.dataset_pose_request_for_image(detection.image_size)?;
                    if item.pnp_observation.as_ref().is_some_and(|observation| {
                        observation.binding_digest == request.binding_digest
                    }) {
                        return None;
                    }
                    Some(DatasetPnpRefreshItem {
                        item_id: item.id,
                        detection: detection.clone(),
                        request,
                    })
                })
                .collect::<Vec<_>>();
        if items.is_empty() {
            return;
        }
        for item in &items {
            let _ = self
                .session
                .install_dataset_pnp_observation(item.item_id, None);
        }
        let cancellation = CalibrationCancellation::default();
        let count = items.len();
        let batch = DatasetPnpRefreshBatch {
            board: self.session.board(),
            cancellation: cancellation.clone(),
            items,
        };
        if let Err(error) = self.worker.send(WorkerCommand::RefreshDatasetPnp(batch)) {
            self.status = error;
            return;
        }
        self.active_job = Some(CalibrationJobKind::DatasetPnpRefresh);
        self.calibration_cancellation = Some(cancellation);
        self.status = format!("Refreshing Dataset PnP for {count} Found image(s)…");
    }

    fn handle_dataset_pnp_refresh_result(&mut self, batch: DatasetPnpRefreshBatchResult) {
        if batch.board != self.session.board() {
            self.status = "Dataset PnP refresh ignored because the board changed.".to_owned();
            return;
        }
        let mut installed = 0_usize;
        let mut failed = 0_usize;
        let mut skipped = 0_usize;
        for result in batch.results {
            let Some(detection) = self
                .session
                .items()
                .iter()
                .find(|item| item.id == result.item_id)
                .and_then(|item| match &item.status {
                    CalibrationItemStatus::Found(detection) => Some(detection.clone()),
                    _ => None,
                })
            else {
                skipped = skipped.saturating_add(1);
                continue;
            };
            if detection != result.detection {
                skipped = skipped.saturating_add(1);
                continue;
            }
            let Some(binding) = self.dataset_pnp_binding(detection.image_size) else {
                let _ = self
                    .session
                    .install_dataset_pnp_observation(result.item_id, None);
                failed = failed.saturating_add(1);
                continue;
            };
            if binding.digest != result.binding_digest {
                skipped = skipped.saturating_add(1);
                continue;
            }
            match result.result {
                Ok(observation) if observation.binding_digest == binding.digest => {
                    if self
                        .session
                        .install_dataset_pnp_observation(result.item_id, Some(observation))
                        .is_ok()
                    {
                        installed = installed.saturating_add(1);
                    } else {
                        failed = failed.saturating_add(1);
                    }
                }
                Ok(_) => skipped = skipped.saturating_add(1),
                Err(_) => {
                    let _ = self
                        .session
                        .install_dataset_pnp_observation(result.item_id, None);
                    failed = failed.saturating_add(1);
                }
            }
        }
        self.status = format!(
            "Dataset PnP refresh finished: {installed} installed, {failed} failed, {skipped} skipped."
        );
    }

    fn poll_worker(&mut self, context: &egui::Context) {
        self.poll_detection_pipeline(context);
        loop {
            let event = match self.worker.receiver.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if matches!(
                        self.active_job,
                        Some(CalibrationJobKind::Calibrate | CalibrationJobKind::DatasetPnpRefresh)
                    ) {
                        self.active_job = None;
                        self.calibration_cancellation = None;
                        self.status = "Calibration worker stopped unexpectedly.".to_owned();
                    }
                    break;
                }
            };
            let finished_job = self.active_job;
            self.active_job = None;
            self.calibration_cancellation = None;
            match event {
                WorkerEvent::Calibration { snapshot, result } => match result {
                    Ok(solution) => match self.session.install_solution(snapshot, solution) {
                        Ok(()) => {
                            self.loaded_result = None;
                            self.status = "Calibration completed; result installed transactionally."
                                .to_owned()
                        }
                        Err(error) => self.status = format!("Calibration result rejected: {error}"),
                    },
                    Err(error) => self.status = error,
                },
                WorkerEvent::DatasetPnpRefresh(result) => {
                    self.handle_dataset_pnp_refresh_result(result);
                }
            }
            if matches!(finished_job, Some(CalibrationJobKind::DatasetPnpRefresh)) {
                self.drain_pending_dataset_pnp_refresh();
            }
        }
    }

    fn install_preview(
        &mut self,
        context: &egui::Context,
        id: CalibrationItemId,
        frame: Arc<Rgba8Frame>,
    ) {
        let dimensions = [frame.width as usize, frame.height as usize];
        let image = egui::ColorImage::from_rgba_unmultiplied(dimensions, frame.pixels());
        let texture = context.load_texture(
            format!("calibration-preview-{}", id.get()),
            image,
            pixel_inspection_texture_options(),
        );
        if let Some(source) = self.sources.get_mut(&id) {
            source.preview = Some(CalibrationPreview { frame, texture });
        }
    }

    fn sync_coverage(&mut self, context: &egui::Context) {
        if !self.coverage_dirty {
            return;
        }
        self.coverage_dirty = false;
        self.coverage =
            build_coverage_image(self.session.items()).map(|image| CoverageVisualization {
                density: context.load_texture(
                    "calibration-coverage-density",
                    image.density,
                    egui::TextureOptions::LINEAR,
                ),
                enabled_views: image.enabled_views,
            });
        self.auto_capture.last_assessment = self.session.assess_auto_admission(None).ok();
    }

    fn refresh_auto_intrinsics_fields(&mut self) {
        if !self.auto_intrinsics {
            return;
        }
        self.initial_distortion_coefficients = ZERO_DISTORTION_COEFFICIENTS;
        if let Some((fx, fy, cx, cy)) = self.auto_intrinsics_values() {
            self.fx = fx;
            self.fy = fy;
            self.cx = cx;
            self.cy = cy;
        }
    }

    fn auto_intrinsics_values(&self) -> Option<(f64, f64, f64, f64)> {
        let size = self
            .live_admission_context
            .as_ref()
            .map(|context| context.image_size)
            .or_else(|| {
                self.session
                    .items()
                    .iter()
                    .filter(|item| item.enabled)
                    .find_map(|item| match &item.status {
                        CalibrationItemStatus::Found(detection) => Some(detection.image_size),
                        _ => None,
                    })
            })?;
        Some((
            900.0,
            900.0,
            f64::from(size.width) * 0.5,
            f64::from(size.height) * 0.5,
        ))
    }

    fn initial_intrinsics(&self) -> Result<InitialIntrinsics, String> {
        let (fx, fy, cx, cy) = if self.auto_intrinsics {
            self.auto_intrinsics_values().ok_or_else(|| {
                "Display a live frame or detect one enabled image first".to_owned()
            })?
        } else {
            (self.fx, self.fy, self.cx, self.cy)
        };
        let initial = InitialIntrinsics {
            camera_matrix: [fx, 0.0, cx, 0.0, fy, cy, 0.0, 0.0, 1.0],
            distortion_coefficients: self.active_initial_distortion_coefficients(),
        };
        initial.validate().map_err(|error| error.to_string())?;
        Ok(initial)
    }

    fn initial_intrinsics_for_image(
        &self,
        image_size: CalibrationImageSize,
    ) -> Result<InitialIntrinsics, String> {
        let (fx, fy, cx, cy) = if self.auto_intrinsics {
            (
                900.0,
                900.0,
                f64::from(image_size.width) * 0.5,
                f64::from(image_size.height) * 0.5,
            )
        } else {
            (self.fx, self.fy, self.cx, self.cy)
        };
        let initial = InitialIntrinsics {
            camera_matrix: [fx, 0.0, cx, 0.0, fy, cy, 0.0, 0.0, 1.0],
            distortion_coefficients: self.active_initial_distortion_coefficients(),
        };
        initial.validate().map_err(|error| error.to_string())?;
        Ok(initial)
    }

    fn use_installed_result_as_initial_intrinsics(&mut self) {
        let Some((fx, fy, cx, cy, distortion)) = self.session.installed().map(|installed| {
            let camera_matrix = installed.solution.camera_matrix;
            (
                camera_matrix[0],
                camera_matrix[4],
                camera_matrix[2],
                camera_matrix[5],
                distortion_coefficients_to_d12(&installed.solution.distortion_coefficients),
            )
        }) else {
            return;
        };
        self.auto_intrinsics = false;
        self.fx = fx;
        self.fy = fy;
        self.cx = cx;
        self.cy = cy;
        self.initial_distortion_coefficients = distortion;
        self.refresh_runtime_auto_admission();
        self.status =
            "Installed result K+D12 copied into editable initial-intrinsics controls.".to_owned();
        self.request_dataset_pnp_refresh();
    }

    fn active_initial_distortion_coefficients(&self) -> Vec<f64> {
        if self.auto_intrinsics {
            ZERO_DISTORTION_COEFFICIENTS.to_vec()
        } else {
            self.initial_distortion_coefficients.to_vec()
        }
    }

    fn json_export(&self) -> Option<CalibrationExport> {
        let installed = self.session.installed()?;
        let items = self
            .session
            .items()
            .iter()
            .filter_map(|item| {
                let source = self.sources.get(&item.id)?;
                let (status, reason, corners) = match &item.status {
                    CalibrationItemStatus::Pending => ("pending", None, None),
                    CalibrationItemStatus::ReadQueued => ("read_queued", None, None),
                    CalibrationItemStatus::Reading => ("reading", None, None),
                    CalibrationItemStatus::DetectQueued => ("detect_queued", None, None),
                    CalibrationItemStatus::Detecting => ("detecting", None, None),
                    CalibrationItemStatus::Found(detection) => {
                        ("found", None, Some(detection.corners.len()))
                    }
                    CalibrationItemStatus::NotFound { .. } => {
                        ("not_found", Some("Chessboard not found"), None)
                    }
                    CalibrationItemStatus::Failed(reason) => {
                        ("failed", Some(reason.as_str()), None)
                    }
                };
                let input = match &item.input {
                    CalibrationInputKey::File(reference) => serde_json::json!({
                        "kind": "file",
                        "source_id": reference.source_id,
                        "source_path": reference.path,
                    }),
                    CalibrationInputKey::StreamCapture(capture) => serde_json::json!({
                        "kind": "stream_capture",
                        "stream_id": capture.stream_id.as_str(),
                        "channel": capture.channel,
                        "frame_sequence": capture.frame_sequence,
                    }),
                };
                let revision = match &item.revision {
                    CalibrationInputRevision::File(version) => serde_json::json!({
                        "kind": "file",
                        "value": version,
                    }),
                    CalibrationInputRevision::EphemeralPng {
                        content_sha256,
                        encoded_bytes,
                    } => serde_json::json!({

                        "kind": "ephemeral_png",
                        "content_sha256": content_sha256,
                        "encoded_bytes": encoded_bytes,
                    }),
                    CalibrationInputRevision::EphemeralRaster {
                        primary_sha256,
                        primary_bytes,
                        primary_format,
                        analysis_sha256,
                        analysis_encoded_bytes,
                    } => serde_json::json!({
                        "kind": "ephemeral_raster",
                        "primary_sha256": primary_sha256,
                        "primary_bytes": primary_bytes,
                        "primary_format": primary_format,
                        "analysis_sha256": analysis_sha256,
                        "analysis_encoded_bytes": analysis_encoded_bytes,
                    }),
                };
                let stream_provenance = match &source.kind {
                    CalibrationSourceKind::File { .. } => None,
                    CalibrationSourceKind::Stream(stream) => Some(serde_json::json!({
                        "source_pts": format!("{:?}", stream.identity.source_pts),
                        "host_monotonic_time_ns": stream.identity.host_monotonic_time_ns,
                    })),
                };
                Some(serde_json::json!({
                    "id": item.id.get(),
                    "input": input,
                    "display_path": source.display_name,
                    "remote": source.remote(),
                    "revision": revision,
                    "stream_provenance": stream_provenance,
                    "enabled": item.enabled,
                    "used": installed.item_ids.contains(&item.id),
                    "status": status,
                    "reason": reason,
                    "corners": corners,
                }))
            })
            .collect::<Vec<_>>();
        Some(CalibrationExport::Json(serde_json::json!({
            "schema_version": 1,
            "algorithm": "PangbotCompatible",
            "board": installed.request.board,
            "initial_intrinsics": installed.request.initial_intrinsics,
            "items": items,
            "solution": installed.solution,
        })))
    }
}

impl Drop for CalibrationWorkspace {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.calibration_cancellation {
            cancellation.cancel();
        }
        if let Some(batch) = &self.active_detection_batch {
            for cancellation in batch.cancellations.values() {
                cancellation.cancel();
            }
        }
        if !self.auto_capture.pending.is_empty() {
            self.cancel_auto_candidates_matching("Calibration workspace closed.", |_| true);
        }
    }
}

/// 在给定可用区域内按图像宽高比做 contain-fit；允许放大，避免高度受限时破坏比例。
pub(crate) fn contain_fit_scale(available: egui::Vec2, image_size: egui::Vec2) -> f32 {
    let finite_positive = |value: f32| {
        if value.is_finite() && value > 0.0 {
            value
        } else {
            1.0
        }
    };
    let available_width = finite_positive(available.x);
    let available_height = finite_positive(available.y);
    let image_width = finite_positive(image_size.x);
    let image_height = finite_positive(image_size.y);
    (available_width / image_width)
        .min(available_height / image_height)
        .max(0.01)
}

pub(crate) fn contain_fit_size(available: egui::Vec2, image_size: egui::Vec2) -> egui::Vec2 {
    image_size * contain_fit_scale(available, image_size)
}

/// OpenCV 以整数坐标表示像素中心；egui 的 `[0, 1]` UV 则覆盖纹理边界。
/// 因此先加半个像素，才能把检测点准确落到纹理中的连续图像坐标。
fn image_point_to_preview(
    point: CalibrationPoint,
    image_rect: egui::Rect,
    image_width: u32,
    image_height: u32,
    horizontal_flip: bool,
) -> egui::Pos2 {
    let normalized_x = (point.x + 0.5) / image_width as f32;
    let normalized_x = if horizontal_flip {
        1.0 - normalized_x
    } else {
        normalized_x
    };
    egui::pos2(
        image_rect.left() + normalized_x * image_rect.width(),
        image_rect.top() + (point.y + 0.5) / image_height as f32 * image_rect.height(),
    )
}

fn paint_reprojection_vector(painter: &egui::Painter, observed: egui::Pos2, projected: egui::Pos2) {
    let vector = projected - observed;
    let stroke = egui::Stroke::new(REPROJECTION_ARROW_WIDTH, REPROJECTED_POINT_COLOR);
    painter.line_segment([observed, projected], stroke);
    if vector.length_sq() > f32::EPSILON {
        let direction = vector / vector.length();
        let normal = egui::vec2(-direction.y, direction.x);
        let arrow_base = projected - direction * REPROJECTION_ARROW_HEAD_LENGTH;
        painter.line_segment(
            [
                projected,
                arrow_base + normal * REPROJECTION_ARROW_HEAD_HALF_WIDTH,
            ],
            stroke,
        );
        painter.line_segment(
            [
                projected,
                arrow_base - normal * REPROJECTION_ARROW_HEAD_HALF_WIDTH,
            ],
            stroke,
        );
    }
    painter.circle_filled(projected, 2.0, REPROJECTED_POINT_COLOR);
}

fn paint_current_gui_reprojection_point(painter: &egui::Painter, projected: egui::Pos2) {
    painter.circle_filled(projected, 2.75, CURRENT_GUI_REPROJECTED_POINT_COLOR);
    painter.circle_stroke(
        projected,
        4.0,
        egui::Stroke::new(1.0, CURRENT_GUI_REPROJECTED_POINT_COLOR),
    );
}

fn projected_board_corners_for_preview(
    observation: &PnPObservation,
    intrinsics: &InitialIntrinsics,
    board: BoardSpec,
    image_rect: egui::Rect,
    image_width: u32,
    image_height: u32,
    horizontal_flip: bool,
) -> Option<Vec<Option<egui::Pos2>>> {
    let rotation = rodrigues_matrix_for_preview(observation.rotation_vector)?;
    let capacity = board.corner_count().ok()?;
    let mut projected = Vec::with_capacity(capacity);
    for row in 0..board.inner_rows {
        for column in 0..board.inner_cols {
            projected.push(project_board_point(
                rotation,
                observation.translation_vector,
                [
                    f64::from(column) * board.square_size,
                    f64::from(row) * board.square_size,
                    0.0,
                ],
                intrinsics,
                image_rect,
                image_width,
                image_height,
                horizontal_flip,
            ));
        }
    }
    Some(projected)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PoseAxisProjection {
    origin: egui::Pos2,
    x_axis: egui::Pos2,
    y_axis: egui::Pos2,
    z_axis: egui::Pos2,
}

fn paint_pose_axis_overlay(painter: &egui::Painter, projection: PoseAxisProjection) {
    painter.circle_filled(
        projection.origin,
        POSE_AXIS_ORIGIN_RADIUS,
        egui::Color32::WHITE,
    );
    paint_pose_axis(
        painter,
        projection.origin,
        projection.x_axis,
        POSE_AXIS_X_COLOR,
        "X",
    );
    paint_pose_axis(
        painter,
        projection.origin,
        projection.y_axis,
        POSE_AXIS_Y_COLOR,
        "Y",
    );
    paint_pose_axis(
        painter,
        projection.origin,
        projection.z_axis,
        POSE_AXIS_Z_COLOR,
        "Z",
    );
}

fn paint_pose_axis(
    painter: &egui::Painter,
    origin: egui::Pos2,
    endpoint: egui::Pos2,
    color: egui::Color32,
    label: &'static str,
) {
    let stroke = egui::Stroke::new(POSE_AXIS_STROKE_WIDTH, color);
    painter.line_segment([origin, endpoint], stroke);
    painter.circle_filled(endpoint, POSE_AXIS_ENDPOINT_RADIUS, color);
    painter.text(
        endpoint + egui::vec2(5.0, -5.0),
        egui::Align2::LEFT_BOTTOM,
        label,
        egui::FontId::monospace(12.0),
        color,
    );
}

fn pose_axis_projection(
    observation: &PnPObservation,
    intrinsics: &InitialIntrinsics,
    board: BoardSpec,
    image_rect: egui::Rect,
    image_width: u32,
    image_height: u32,
    horizontal_flip: bool,
) -> Option<PoseAxisProjection> {
    let projection = pose_axis_image_projection(observation, intrinsics, board)?;
    let map = |point| {
        image_point_to_preview(
            point,
            image_rect,
            image_width,
            image_height,
            horizontal_flip,
        )
    };
    Some(PoseAxisProjection {
        origin: map(projection.origin),
        x_axis: map(projection.x_axis),
        y_axis: map(projection.y_axis),
        z_axis: map(projection.z_axis),
    })
}

/// 以原始像素坐标投影 pose 坐标轴，Dataset 与 Live Viewer 共用同一 D12 模型。
fn pose_axis_image_projection(
    observation: &PnPObservation,
    intrinsics: &InitialIntrinsics,
    board: BoardSpec,
) -> Option<ViewerPoseAxisOverlay> {
    let rotation = rodrigues_matrix_for_preview(observation.rotation_vector)?;
    let board_width = f64::from(board.inner_cols.saturating_sub(1)) * board.square_size;
    let board_height = f64::from(board.inner_rows.saturating_sub(1)) * board.square_size;
    let axis_length = board_width.max(board_height).max(board.square_size) * 0.25;
    let origin = [board_width * 0.5, board_height * 0.5, 0.0];
    let project = |point: [f64; 3]| {
        project_board_point_image(rotation, observation.translation_vector, point, intrinsics)
    };
    Some(ViewerPoseAxisOverlay {
        origin: project(origin)?,
        x_axis: project([origin[0] + axis_length, origin[1], origin[2]])?,
        y_axis: project([origin[0], origin[1] + axis_length, origin[2]])?,
        z_axis: project([origin[0], origin[1], origin[2] + axis_length])?,
    })
}

fn project_board_point(
    rotation: [[f64; 3]; 3],
    translation: [f64; 3],
    point: [f64; 3],
    intrinsics: &InitialIntrinsics,
    image_rect: egui::Rect,
    image_width: u32,
    image_height: u32,
    horizontal_flip: bool,
) -> Option<egui::Pos2> {
    project_board_point_image(rotation, translation, point, intrinsics).map(|point| {
        image_point_to_preview(
            point,
            image_rect,
            image_width,
            image_height,
            horizontal_flip,
        )
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
fn status_label(status: &CalibrationItemStatus) -> &'static str {
    match status {
        CalibrationItemStatus::Pending => "Pending",
        CalibrationItemStatus::ReadQueued => "Read queued",
        CalibrationItemStatus::Reading => "Reading",
        CalibrationItemStatus::DetectQueued => "Detect queued",
        CalibrationItemStatus::Detecting => "Detecting",
        CalibrationItemStatus::Found(_) => "Found",
        CalibrationItemStatus::NotFound { .. } => "Not found",
        CalibrationItemStatus::Failed(_) => "Failed",
    }
}

fn status_color(status: &CalibrationItemStatus) -> Option<egui::Color32> {
    match status {
        CalibrationItemStatus::Found(_) => Some(OBSERVED_POINT_COLOR),
        CalibrationItemStatus::NotFound { .. } | CalibrationItemStatus::Failed(_) => {
            Some(REPROJECTED_POINT_COLOR)
        }
        CalibrationItemStatus::Pending
        | CalibrationItemStatus::ReadQueued
        | CalibrationItemStatus::Reading
        | CalibrationItemStatus::DetectQueued
        | CalibrationItemStatus::Detecting => None,
    }
}

fn detection_size(status: &CalibrationItemStatus) -> Option<CalibrationImageSize> {
    match status {
        CalibrationItemStatus::Found(detection) => Some(detection.image_size),
        CalibrationItemStatus::NotFound { image_size } => Some(*image_size),
        _ => None,
    }
}

fn calibration_view(
    installed: Option<&camera_toolbox_app::InstalledCalibration>,
    item_id: CalibrationItemId,
) -> Option<&ViewCalibrationResult> {
    let installed = installed?;
    let index = installed.item_ids.iter().position(|id| *id == item_id)?;
    installed.solution.views.get(index)
}

fn calibration_metric(
    installed: Option<&camera_toolbox_app::InstalledCalibration>,
    item_id: CalibrationItemId,
) -> Option<f64> {
    calibration_view(installed, item_id).map(|view| view.reprojection_rmse)
}

fn render_rmse_cell(ui: &mut egui::Ui, metric: Option<f64>, max_metric: f64) {
    let Some(metric) = metric else {
        ui.weak("—");
        return;
    };
    let size = egui::vec2(ui.available_width().max(24.0), 16.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, egui::Color32::DARK_GRAY);
    let ratio = (metric / max_metric).clamp(0.0, 1.0) as f32;
    let fill_rect =
        egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * ratio, rect.height()));
    painter.rect_filled(fill_rect, 2.0, egui::Color32::LIGHT_BLUE);

    // 同一数值按进度边界分色，保证浅色填充区和深色轨道区都保持高对比度。
    let galley = painter.layout_no_wrap(
        format!("{metric:.3}"),
        egui::FontId::monospace(11.0),
        egui::Color32::PLACEHOLDER,
    );
    let text_position = rect.center() - galley.size() * 0.5;
    painter
        .with_clip_rect(fill_rect)
        .galley_with_override_text_color(text_position, Arc::clone(&galley), RMSE_TEXT_ON_FILL);
    let track_rect = egui::Rect::from_min_max(
        egui::pos2(fill_rect.right(), rect.top()),
        rect.right_bottom(),
    );
    painter
        .with_clip_rect(track_rect)
        .galley_with_override_text_color(text_position, galley, RMSE_TEXT_ON_TRACK);
    response.on_hover_text(format!("Reprojection RMSE: {metric:.6} px"));
}

fn render_pnp_depth_cell(
    ui: &mut egui::Ui,
    observation: Option<&PnPObservation>,
    pnp_state: Option<&AutoAdmissionPnpState>,
) {
    render_pnp_metric_cell(ui, observation, pnp_state, "Depth", |observation| {
        format!("Z {:.1}", observation.depth)
    });
}

fn render_pnp_direction_cell(
    ui: &mut egui::Ui,
    observation: Option<&PnPObservation>,
    pnp_state: Option<&AutoAdmissionPnpState>,
) {
    render_pnp_metric_cell(
        ui,
        observation,
        pnp_state,
        "Angle direction",
        |observation| format!("az {:.0}°", observation.azimuth_degrees),
    );
}

fn render_pnp_angle_cell(
    ui: &mut egui::Ui,
    observation: Option<&PnPObservation>,
    pnp_state: Option<&AutoAdmissionPnpState>,
) {
    render_pnp_metric_cell(ui, observation, pnp_state, "Angle", |observation| {
        format!("θ {:.1}°", observation.tilt_degrees)
    });
}

fn render_pnp_metric_cell(
    ui: &mut egui::Ui,
    observation: Option<&PnPObservation>,
    pnp_state: Option<&AutoAdmissionPnpState>,
    metric: &str,
    value: impl FnOnce(&PnPObservation) -> String,
) {
    let blocked = pnp_state.is_some_and(AutoAdmissionPnpState::is_blocked);
    let Some(observation) = observation else {
        if let Some(state) = pnp_state.filter(|state| state.is_blocked()) {
            render_pnp_blocked_cell(ui, state, metric);
        } else {
            ui.weak("—")
                .on_hover_text(format!("No current Dataset PnP observation for {metric}."));
        }
        return;
    };
    let label = value(observation);
    let mut response = if blocked {
        ui.colored_label(egui::Color32::LIGHT_RED, label)
    } else {
        ui.monospace(label)
    };
    let mut hover = format!(
        "Depth: {:.3} configured board units\nTilt: {:.3}°\nAzimuth: {:.3}°\nPnP RMSE: {:.4} px\nPnP max error: {:.4} px",
        observation.depth,
        observation.tilt_degrees,
        observation.azimuth_degrees,
        observation.reprojection_rmse,
        observation.max_reprojection_error,
    );
    if let Some(state) = pnp_state.filter(|state| state.is_blocked()) {
        hover.push_str("\n\nAcceptance gap: ");
        hover.push_str(&pnp_state_reason(state));
    }
    response = response.on_hover_text(hover);
    let _ = response;
}

fn render_pnp_blocked_cell(ui: &mut egui::Ui, state: &AutoAdmissionPnpState, metric: &str) {
    ui.colored_label(egui::Color32::LIGHT_RED, "PnP×")
        .on_hover_text(format!(
            "{metric} is unavailable because current Dataset PnP is not valid.\n{}",
            pnp_state_reason(state)
        ));
}

fn pnp_state_reason(state: &AutoAdmissionPnpState) -> String {
    match state {
        AutoAdmissionPnpState::Valid => "PnP is valid for current Dataset Acceptance.".to_owned(),
        AutoAdmissionPnpState::MissingBinding => {
            "Current GUI K/D12 cannot create a Dataset PnP binding.".to_owned()
        }
        AutoAdmissionPnpState::MissingObservation => {
            "This Found item has no current Dataset PnP observation yet.".to_owned()
        }
        AutoAdmissionPnpState::BindingGap(reason) => format!("PnP binding gap: {reason}"),
        AutoAdmissionPnpState::DepthGap(reason) => format!("Depth gap: {reason}"),
        AutoAdmissionPnpState::PoseGap(reason) => format!("Pose gap: {reason}"),
        AutoAdmissionPnpState::RmseReprojectionGap(reason) => {
            format!("RMSE reprojection gap: {reason}")
        }
        AutoAdmissionPnpState::MaxReprojectionGap(reason) => {
            format!("Max reprojection gap: {reason}")
        }
        AutoAdmissionPnpState::Invalid(reason) => format!("PnP evidence was rejected: {reason}"),
    }
}

fn pnp_state_gap_label(state: &AutoAdmissionPnpState) -> Option<&'static str> {
    match state {
        AutoAdmissionPnpState::Valid => None,
        AutoAdmissionPnpState::MissingBinding => Some("K/D Gap"),
        AutoAdmissionPnpState::MissingObservation => Some("PnP Gap"),
        AutoAdmissionPnpState::BindingGap(_) => Some("PnP Binding Gap"),
        AutoAdmissionPnpState::DepthGap(_) => Some("Depth Gap"),
        AutoAdmissionPnpState::PoseGap(_) => Some("Pose Gap"),
        AutoAdmissionPnpState::RmseReprojectionGap(_) => Some("RMSE ReProj Gap"),
        AutoAdmissionPnpState::MaxReprojectionGap(_) => Some("Max ReProj Gap"),
        AutoAdmissionPnpState::Invalid(_) => Some("PnP Gap"),
    }
}

fn format_gain(value: f64) -> String {
    format!("{value:.3}")
}

fn render_acceptance_status_cell(
    ui: &mut egui::Ui,
    status: &CalibrationItemStatus,
    enabled: bool,
    contribution: Option<&AutoAdmissionItemContribution>,
    assessment_active: bool,
) {
    if !enabled {
        ui.weak("Off")
            .on_hover_text("Disabled item: not part of current Dataset Acceptance.");
        return;
    }
    if !assessment_active {
        ui.weak("No criteria").on_hover_text(
            "Dataset Acceptance criteria are invalid or no compatible image size is available.",
        );
        return;
    }
    if !matches!(status, CalibrationItemStatus::Found(_)) {
        ui.weak("No Found").on_hover_text(
            "Only enabled Found Dataset items can participate in Dataset Acceptance.",
        );
        return;
    }
    let Some(contribution) = contribution else {
        ui.colored_label(egui::Color32::YELLOW, "Geometry Gap")
            .on_hover_text("Found item is outside current Dataset Acceptance because image size, image bounds, or minimum-spacing geometry gates are incompatible.");
        return;
    };
    if let Some(label) = pnp_state_gap_label(&contribution.pnp_state) {
        ui.colored_label(egui::Color32::LIGHT_RED, label)
            .on_hover_text(pnp_state_reason(&contribution.pnp_state));
    } else if !contribution.depth_covered {
        ui.colored_label(egui::Color32::YELLOW, "Depth Gap")
            .on_hover_text("PnP is valid, but no chessboard corner depth occupies the current Dataset depth bins.");
    } else if !contribution.pose_covered {
        ui.colored_label(egui::Color32::YELLOW, "Pose Gap")
            .on_hover_text(
                "PnP is valid, but this board normal occupies no current Dataset pose bin.",
            );
    } else if contribution.constraint_gain == 0.0 {
        ui.weak("No Gain Gap").on_hover_text(
            "Valid item with zero target-capped attributed Gain; required coverage is already owned by earlier compatible Dataset rows or above target.",
        );
    } else {
        ui.colored_label(egui::Color32::LIGHT_GREEN, "Accepted")
            .on_hover_text("Item participates in current Dataset Acceptance and has positive target-capped Gain.");
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum AdmissionDeltaCellState {
    Contribution(f64),
    PnpBlocked,
    Disabled,
    OutsideActiveAdmission,
}

fn admission_delta_cell_state(
    contribution: Option<&AutoAdmissionItemContribution>,
    enabled: bool,
    metric_blocked: impl FnOnce(&AutoAdmissionItemContribution) -> bool,
    value: impl FnOnce(&AutoAdmissionItemContribution) -> f64,
) -> AdmissionDeltaCellState {
    match contribution {
        Some(contribution) if metric_blocked(contribution) => AdmissionDeltaCellState::PnpBlocked,
        Some(contribution) => AdmissionDeltaCellState::Contribution(value(contribution)),
        None if enabled => AdmissionDeltaCellState::OutsideActiveAdmission,
        None => AdmissionDeltaCellState::Disabled,
    }
}

fn metric_delta_gap_reason(contribution: &AutoAdmissionItemContribution, metric: &str) -> String {
    if contribution.pnp_state.is_blocked() {
        return pnp_state_reason(&contribution.pnp_state);
    }
    format!(
        "{metric} has no occupied bin under current Dataset thresholds; this is a gap, not redundant +0 gain."
    )
}

fn render_admission_delta_cell(
    ui: &mut egui::Ui,
    contribution: Option<&AutoAdmissionItemContribution>,
    enabled: bool,
    metric: &str,
    metric_blocked: impl FnOnce(&AutoAdmissionItemContribution) -> bool,
    value: impl FnOnce(&AutoAdmissionItemContribution) -> f64,
) {
    match admission_delta_cell_state(contribution, enabled, metric_blocked, value) {
        AdmissionDeltaCellState::Contribution(delta) => {
            ui.monospace(format!("+{}", format_gain(delta))).on_hover_text(format!(
                "Target-capped {metric} Gain attributed to this item in the current Dataset. +0 means valid but redundant after required coverage is already assigned."
            ));
        }
        AdmissionDeltaCellState::PnpBlocked => {
            let reason = contribution
                .map(|contribution| metric_delta_gap_reason(contribution, metric))
                .unwrap_or_else(|| "Metric is not part of the current active Dataset.".to_owned());
            ui.colored_label(egui::Color32::LIGHT_RED, "×0")
                .on_hover_text(format!(
                    "{metric} delta is blocked by PnP, not a valid zero-gain result.\n{reason}"
                ));
        }
        AdmissionDeltaCellState::Disabled => {
            ui.weak("off")
                .on_hover_text("Disabled items are outside the active admission set.");
        }
        AdmissionDeltaCellState::OutsideActiveAdmission => {
            ui.weak("—").on_hover_text(
                "Outside current Dataset Acceptance: image size, detection state, or current geometry gates are incompatible. Local, SFTP, and RTSP provenance is not filtered."
            );
        }
    }
}

fn render_total_gain_cell(
    ui: &mut egui::Ui,
    contribution: Option<&AutoAdmissionItemContribution>,
    enabled: bool,
) {
    match contribution {
        Some(contribution) if contribution.pnp_state.is_blocked() => {
            let label = if contribution.constraint_gain == 0.0 {
                "×0".to_owned()
            } else {
                format!("+{}*", format_gain(contribution.constraint_gain))
            };
            ui.colored_label(egui::Color32::LIGHT_RED, label)
                .on_hover_text(format!(
                    "Total Gain includes valid Found/Field contributions only; Depth/Pose are blocked by PnP.\n{}",
                    pnp_state_reason(&contribution.pnp_state)
                ));
        }
        Some(contribution) => {
            ui.monospace(format!("+{}", format_gain(contribution.constraint_gain))).on_hover_text(
                "Total target-capped row Gain. +0 means the row is valid but redundant under current targets.",
            );
        }
        None if enabled => {
            ui.weak("—").on_hover_text(
                "Outside current Dataset Acceptance: image size, detection state, or current geometry gates are incompatible.",
            );
        }
        None => {
            ui.weak("off")
                .on_hover_text("Disabled items are outside the active admission set.");
        }
    }
}

struct CoverageImage {
    density: egui::ColorImage,
    enabled_views: usize,
}

fn build_coverage_image(
    items: &[camera_toolbox_app::CalibrationDatasetItem],
) -> Option<CoverageImage> {
    let image_size =
        items
            .iter()
            .filter(|item| item.enabled)
            .find_map(|item| match &item.status {
                CalibrationItemStatus::Found(detection) => Some(detection.image_size),
                _ => None,
            })?;
    let coverage_height = ((COVERAGE_WIDTH as f64 * f64::from(image_size.height)
        / f64::from(image_size.width))
    .round() as usize)
        .clamp(64, 256);
    let mut corner_hits = vec![0.0_f32; COVERAGE_WIDTH * coverage_height];
    let mut enabled_views = 0_usize;
    for detection in items.iter().filter(|item| item.enabled).filter_map(|item| {
        if let CalibrationItemStatus::Found(detection) = &item.status {
            Some(detection)
        } else {
            None
        }
    }) {
        enabled_views += 1;
        let width = detection.image_size.width as f32;
        let height = detection.image_size.height as f32;
        for point in &detection.corners {
            let x = ((point.x / width) * COVERAGE_WIDTH as f32)
                .floor()
                .clamp(0.0, (COVERAGE_WIDTH - 1) as f32) as usize;
            let y = ((point.y / height) * coverage_height as f32)
                .floor()
                .clamp(0.0, (coverage_height - 1) as f32) as usize;
            corner_hits[y * COVERAGE_WIDTH + x] += 1.0;
        }
    }
    if enabled_views == 0 {
        return None;
    }
    let density = gaussian_blur(
        &corner_hits,
        COVERAGE_WIDTH,
        coverage_height,
        COVERAGE_GAUSSIAN_SIGMA,
    );
    Some(CoverageImage {
        density: colorize_heatmap(&density, COVERAGE_WIDTH, coverage_height),
        enabled_views,
    })
}

fn gaussian_blur(values: &[f32], width: usize, height: usize, sigma: f32) -> Vec<f32> {
    let radius = (sigma * 3.0).ceil() as isize;
    let kernel = (-radius..=radius)
        .map(|offset| (-(offset as f32).powi(2) / (2.0 * sigma * sigma)).exp())
        .collect::<Vec<_>>();
    let mut horizontal = vec![0.0_f32; values.len()];
    for y in 0..height {
        for x in 0..width {
            let mut sum = 0.0;
            let mut weight_sum = 0.0;
            for (kernel_index, offset) in (-radius..=radius).enumerate() {
                let sample_x = x as isize + offset;
                if (0..width as isize).contains(&sample_x) {
                    let weight = kernel[kernel_index];
                    sum += values[y * width + sample_x as usize] * weight;
                    weight_sum += weight;
                }
            }
            horizontal[y * width + x] = sum / weight_sum.max(f32::EPSILON);
        }
    }
    let mut output = vec![0.0_f32; values.len()];
    for y in 0..height {
        for x in 0..width {
            let mut sum = 0.0;
            let mut weight_sum = 0.0;
            for (kernel_index, offset) in (-radius..=radius).enumerate() {
                let sample_y = y as isize + offset;
                if (0..height as isize).contains(&sample_y) {
                    let weight = kernel[kernel_index];
                    sum += horizontal[sample_y as usize * width + x] * weight;
                    weight_sum += weight;
                }
            }
            output[y * width + x] = sum / weight_sum.max(f32::EPSILON);
        }
    }
    output
}

fn colorize_heatmap(values: &[f32], width: usize, height: usize) -> egui::ColorImage {
    let peak = values.iter().copied().fold(0.0_f32, f32::max);
    let mut rgba = Vec::with_capacity(values.len() * 4);
    for value in values {
        let strength = if peak <= f32::EPSILON {
            0.0
        } else {
            (*value / peak).clamp(0.0, 1.0)
        };
        let color = heatmap_color(strength);
        rgba.extend_from_slice(&[color.r(), color.g(), color.b(), 255]);
    }
    egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba)
}

pub(crate) fn heatmap_color(value: f32) -> egui::Color32 {
    const STOPS: [(f32, [u8; 3]); 6] = [
        (0.0, [48, 18, 59]),
        (0.2, [50, 92, 210]),
        (0.4, [24, 199, 210]),
        (0.6, [86, 230, 104]),
        (0.8, [246, 220, 52]),
        (1.0, [180, 32, 18]),
    ];
    let value = value.clamp(0.0, 1.0);
    let upper = STOPS
        .iter()
        .position(|(position, _)| *position >= value)
        .unwrap_or(STOPS.len() - 1);
    let lower = upper.saturating_sub(1);
    let span = (STOPS[upper].0 - STOPS[lower].0).max(f32::EPSILON);
    let mix = (value - STOPS[lower].0) / span;
    let channel = |index: usize| {
        (f32::from(STOPS[lower].1[index])
            + (f32::from(STOPS[upper].1[index]) - f32::from(STOPS[lower].1[index])) * mix)
            .round() as u8
    };
    egui::Color32::from_rgb(channel(0), channel(1), channel(2))
}

pub(crate) fn paint_heatmap_guides(painter: &egui::Painter, rect: egui::Rect) {
    for index in 1..3 {
        let x = egui::lerp(rect.x_range(), index as f32 / 3.0);
        let y = egui::lerp(rect.y_range(), index as f32 / 3.0);
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(0.7, egui::Color32::from_white_alpha(90)),
        );
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(0.7, egui::Color32::from_white_alpha(90)),
        );
    }
    let legend = egui::Rect::from_min_size(
        egui::pos2(rect.right() - rect.width() * 0.34, rect.bottom() - 12.0),
        egui::vec2(rect.width() * 0.30, 7.0),
    );
    for index in 0..32 {
        let x0 = egui::lerp(legend.x_range(), index as f32 / 32.0);
        let x1 = egui::lerp(legend.x_range(), (index + 1) as f32 / 32.0);
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x0, legend.top()),
                egui::pos2(x1, legend.bottom()),
            ),
            0.0,
            heatmap_color(index as f32 / 31.0),
        );
    }
    painter.text(
        egui::pos2(legend.left() - 3.0, legend.center().y),
        egui::Align2::RIGHT_CENTER,
        "low",
        egui::FontId::monospace(9.0),
        egui::Color32::WHITE,
    );
    painter.text(
        egui::pos2(legend.right() + 3.0, legend.center().y),
        egui::Align2::LEFT_CENTER,
        "high",
        egui::FontId::monospace(9.0),
        egui::Color32::WHITE,
    );
    painter.text(
        rect.left_top() + egui::vec2(8.0, 8.0),
        egui::Align2::LEFT_TOP,
        "Inner-corner density",
        egui::FontId::proportional(12.0),
        egui::Color32::WHITE,
    );
}

fn distortion_coefficients_to_d12(values: &[f64]) -> [f64; 12] {
    let mut distortion = ZERO_DISTORTION_COEFFICIENTS;
    for (target, value) in distortion.iter_mut().zip(values.iter().copied()) {
        *target = value;
    }
    distortion
}

fn render_calibration_result(
    ui: &mut egui::Ui,
    solution: Option<&CalibrationSolution>,
    stale: bool,
    source_label: Option<&str>,
    imported_metrics_missing: bool,
) {
    let Some(solution) = solution else {
        ui.weak("Run Calibrate or Load Result YAML to display final intrinsics and distortion coefficients.");
        return;
    };
    if let Some(source_label) = source_label {
        ui.weak(source_label);
    }
    if stale {
        ui.colored_label(egui::Color32::YELLOW, STALE_CALIBRATION_RESULT_REASON);
    }
    let matrix = solution.camera_matrix;
    ui.horizontal_wrapped(|ui| {
        for (name, value) in [
            ("fx", matrix[0]),
            ("fy", matrix[4]),
            ("cx", matrix[2]),
            ("cy", matrix[5]),
        ] {
            ui.group(|ui| {
                ui.label(name);
                ui.monospace(format!("{value:.8}"));
            });
        }
    });
    ui.horizontal_wrapped(|ui| {
        if imported_metrics_missing {
            ui.monospace(format!(
                "{}×{} · RMS N/A (not provided) · flags N/A (not provided)",
                solution.image_size.width, solution.image_size.height
            ));
        } else {
            ui.monospace(format!(
                "{}×{} · RMS {:.6} px · flags {}",
                solution.image_size.width,
                solution.image_size.height,
                solution.rms_error,
                solution.calibration_flags
            ));
        }
    });
    ui.label("Camera matrix (row-major)");
    egui::Grid::new("calibration_result_matrix")
        .num_columns(3)
        .show(ui, |ui| {
            for row in matrix.chunks_exact(3) {
                for value in row {
                    ui.monospace(format!("{value:.10}"));
                }
                ui.end_row();
            }
        });
    ui.label("Distortion coefficients (OpenCV order)");
    egui::Grid::new("calibration_result_distortion")
        .num_columns(4)
        .striped(true)
        .show(ui, |ui| {
            for (index, value) in solution.distortion_coefficients.iter().enumerate() {
                let name = INITIAL_DISTORTION_NAMES.get(index).copied().unwrap_or("d");
                ui.monospace(format!("{name}[{index}] = {value:.10}"));
                if (index + 1) % 4 == 0 {
                    ui.end_row();
                }
            }
            if solution.distortion_coefficients.len() % 4 != 0 {
                ui.end_row();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_adapters::filesystem::LocalFileSystem;
    use camera_toolbox_app::{
        CaptureStoreLimits, FileSourceId, FileSystem, SourcePath, StreamSessionId,
    };
    use std::time::Instant;

    #[test]
    fn x5_authoritative_lookup_rejects_ffmpeg_pts_as_board_timestamp() {
        let identity = StreamFrameIdentity::known_at(
            StreamSessionId::new("rtsp-pts-test").unwrap(),
            0,
            42,
            camera_toolbox_app::SourcePts::Known {
                ticks: 30_000,
                time_base_numerator: 1,
                time_base_denominator: 90_000,
                provenance: camera_toolbox_app::SourcePtsProvenance::FfmpegDecodedFrame,
            },
            123,
        );

        let error = AuthoritativeYuvLookup::from_rtsp_identity(&identity).unwrap_err();

        assert!(error.contains("presentation timing only"));
        assert!(error.contains("explicit frame_id/timestamp_ns metadata"));
    }

    #[test]
    fn x5_rtsp_pts_bridge_converts_source_pts_to_90k() {
        let ffmpeg_pts = camera_toolbox_app::SourcePts::Known {
            ticks: 500,
            time_base_numerator: 1,
            time_base_denominator: 1_000,
            provenance: camera_toolbox_app::SourcePtsProvenance::FfmpegDecodedFrame,
        };

        assert_eq!(source_pts_to_90k(&ffmpeg_pts).unwrap(), 45_000);
    }

    #[test]
    fn x5_rtsp_pts_bridge_applies_cached_offset() {
        let sample = RtspPtsBridgeSample {
            source_pts_90k: 30_000,
            driver_rtsp_pts_90k: 1_030_000,
            offset_90k: 1_000_000,
            sampled_frame_sequence: 7,
            updated_at_host_ns: 0,
        };

        assert_eq!(sample.target_rtsp_pts_90k(31_527).unwrap(), 1_031_527);
    }
    #[test]
    fn x5_authoritative_ring_diagnostics_formats_status_range() {
        let status = x5_tcp_client::X5DriverStatus {
            camera_running: true,
            rtsp_started: true,
            rtsp_tx_enabled: true,
            rtsp_requested_enabled: true,
            rtsp_control_busy: false,
            rtsp_pending_action: String::new(),
            rtsp_last_error: 0,
            rtsp_action_id: 0,
            rtsp_last_message: String::new(),
            rtsp_channels: Vec::new(),
            rings: vec![x5_tcp_client::X5RingStatus {
                channel: 0,
                depth: 24,
                valid: 24,
                write_index: 5,
                min_frame_id: 10,
                max_frame_id: 33,
                last_frame_id: 33,
                min_timestamp_ns: 37_906_100_000,
                max_timestamp_ns: 37_906_777_777,
                last_timestamp_ns: 37_906_777_777,
                last_rtsp_timestamp_us: 42_118_642,
                last_rtsp_pts_90k: 3_790_677,
                min_rtsp_pts_90k: 3_790_610,
                max_rtsp_pts_90k: 3_790_677,
                retention_ns: 677_777,
                dropped: 2,
                evicted: 9,
            }],
            fps: Some(60),
            bitrate_kbps: Some(6_000),
            pipeline_config_version: Some(1),
        };

        let diagnostics = format_x5_authoritative_yuv_ring_diagnostics(&status, 0);

        assert!(diagnostics.contains("ring_valid=24/24"));
        assert!(diagnostics.contains("ring_frame_id=10..33"));
        assert!(diagnostics.contains("ring_timestamp_ns=37906100000..37906777777"));
        assert!(diagnostics.contains("ring_rtsp_pts_90k=3790610..3790677"));
        assert!(diagnostics.contains("ring_last_rtsp_pts_90k=3790677"));
        assert!(diagnostics.contains("ring_retention_ns=677777"));
        assert!(diagnostics.contains("ring_evicted=9"));
        assert!(diagnostics.contains("ring_dropped=2"));
    }
    #[test]
    fn loaded_yaml_result_becomes_active_without_dataset_calibration() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let yaml = "%YAML:1.0\nfx: 878.7023\nfy: 878.5325\ncx: 955.6284\ncy: 533.1718\nk1: 0.0345\nk2: -0.0458\np1: -0.00008590\np2: 0.00015387\nk3: 0.0119\nk4: -0.0123\nk5: 0.0234\nk6: -0.0345\ns1: 0.00001111\ns2: -0.00002222\ns3: 0.00003333\ns4: -0.00004444\nwidth: 1920\nheight: 1080\n";

        workspace.load_calibration_result_from_yaml_str(yaml, "fixture.yaml");

        let solution = workspace
            .active_calibration_solution()
            .expect("loaded YAML result must become the active result");
        assert!(workspace.session.installed().is_none());
        assert_eq!(
            solution.image_size,
            CalibrationImageSize::new(1920, 1080).unwrap()
        );
        assert_eq!(solution.camera_matrix[0], 878.7023);
        assert_eq!(solution.distortion_coefficients.len(), 12);
    }

    #[test]
    fn loaded_yaml_result_renders_missing_quality_metrics_as_not_provided() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let yaml = "%YAML:1.0\nfx: 878.7023\nfy: 878.5325\ncx: 955.6284\ncy: 533.1718\nk1: 0.0345\nk2: -0.0458\np1: -0.00008590\np2: 0.00015387\nk3: 0.0119\nk4: -0.0123\nk5: 0.0234\nk6: -0.0345\ns1: 0.00001111\ns2: -0.00002222\ns3: 0.00003333\ns4: -0.00004444\nwidth: 1920\nheight: 1080\n";
        workspace.load_calibration_result_from_yaml_str(yaml, "fixture.yaml");

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 500.0),
            )),
            ..Default::default()
        };
        let output = context.run_ui(input, |ui| {
            render_calibration_result(
                ui,
                workspace.active_calibration_solution(),
                false,
                Some("Loaded YAML: fixture.yaml"),
                true,
            );
        });
        let text = output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .into_iter()
            .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Loaded YAML: fixture.yaml"), "{text}");
        assert!(text.contains("RMS N/A (not provided)"), "{text}");
        assert!(text.contains("flags N/A (not provided)"), "{text}");
        assert!(!text.contains("RMS 0.000000"), "{text}");
    }

    #[test]
    fn loaded_yaml_result_builds_eeprom_update_request() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let yaml = "%YAML:1.0\nfx: 878.7023\nfy: 878.5325\ncx: 955.6284\ncy: 533.1718\nk1: 0.0345\nk2: -0.0458\np1: -0.00008590\np2: 0.00015387\nk3: 0.0119\nk4: -0.0123\nk5: 0.0234\nk6: -0.0345\ns1: 0.00001111\ns2: -0.00002222\ns3: 0.00003333\ns4: -0.00004444\nwidth: 1920\nheight: 1080\n";

        workspace.load_calibration_result_from_yaml_str(yaml, "fixture.yaml");

        let image = camera_toolbox_core::FullEepromImage::from_solution(
            workspace.active_calibration_solution().unwrap(),
            "2T02D2567K0042",
        )
        .expect("loaded YAML result must encode to EEPROM image");
        let request = image.update_calibration_request();
        assert_eq!(
            request.mode,
            camera_toolbox_core::EepromProvisioningMode::UpdateCalibration
        );
        assert_eq!(request.serial_number, "2T02D2567K0042");
        assert_eq!(request.segments.len(), 1);
    }

    #[test]
    fn loaded_yaml_result_overrides_installed_solution_for_eeprom_image() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        for index in 0..3 {
            install_detection_outcome(
                &mut workspace,
                &format!("installed-{index}.png"),
                found_detection(640, 480),
            );
        }
        let snapshot = workspace
            .session
            .calibration_snapshot(workspace.initial_intrinsics().unwrap())
            .unwrap();
        let views = snapshot
            .request
            .image_points
            .iter()
            .map(|points| camera_toolbox_core::ViewCalibrationResult {
                rotation_vector: [0.0; 3],
                translation_vector: [0.0, 0.0, 1.0],
                projected_points: points.clone(),
                reprojection_rmse: 0.1,
                max_reprojection_error: 0.2,
            })
            .collect();
        let installed_solution = CalibrationSolution {
            image_size: snapshot.request.image_size,
            camera_matrix: [620.0, 0.0, 318.0, 0.0, 621.0, 241.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 12],
            rms_error: 0.15,
            calibration_flags: camera_toolbox_core::PANGBOT_CALIBRATION_FLAGS,
            views,
        };
        workspace
            .session
            .install_solution(snapshot, installed_solution)
            .unwrap();
        assert_eq!(
            workspace
                .active_calibration_solution()
                .unwrap()
                .camera_matrix[0],
            620.0
        );

        let yaml = "%YAML:1.0\nfx: 878.7023\nfy: 878.5325\ncx: 955.6284\ncy: 533.1718\nk1: 0.0345\nk2: -0.0458\np1: -0.00008590\np2: 0.00015387\nk3: 0.0119\nk4: -0.0123\nk5: 0.0234\nk6: -0.0345\ns1: 0.00001111\ns2: -0.00002222\ns3: 0.00003333\ns4: -0.00004444\nwidth: 1920\nheight: 1080\n";
        workspace.load_calibration_result_from_yaml_str(yaml, "fixture.yaml");

        let image = camera_toolbox_core::FullEepromImage::from_solution(
            workspace.active_calibration_solution().unwrap(),
            "2T02D2567K0042",
        )
        .expect("loaded YAML result must encode to EEPROM image");
        let eeprom_fx = f32::from_le_bytes(image.as_bytes()[0x18..0x1c].try_into().unwrap());
        assert!((eeprom_fx - 878.7023_f32).abs() < 0.0001, "fx={eeprom_fx}");
    }

    fn guided_test_detection(center_uv: [f64; 2], scale: f64) -> ChessboardDetection {
        let image_size = CalibrationImageSize::new(1000, 800).unwrap();
        let side = (scale * f64::from(image_size.width.min(image_size.height))) as f32;
        let center_x = (center_uv[0] * f64::from(image_size.width)) as f32;
        let center_y = (center_uv[1] * f64::from(image_size.height)) as f32;
        let half = side * 0.5;
        ChessboardDetection {
            image_size,
            corners: vec![
                CalibrationPoint::new(center_x - half, center_y - half),
                CalibrationPoint::new(center_x + half, center_y - half),
                CalibrationPoint::new(center_x + half, center_y + half),
                CalibrationPoint::new(center_x - half, center_y + half),
            ],
        }
    }

    fn guided_test_rotation_vector(tilt_degrees: f64, azimuth_degrees: f64) -> [f64; 3] {
        let tilt = tilt_degrees.to_radians();
        if tilt.abs() <= f64::EPSILON {
            return [0.0; 3];
        }
        let azimuth = azimuth_degrees.to_radians();
        [-azimuth.sin() * tilt, azimuth.cos() * tilt, 0.0]
    }

    fn guided_test_pnp(
        center_uv: [f64; 2],
        scale: f64,
        tilt_degrees: f64,
        azimuth_degrees: f64,
        reprojection_rmse: f64,
        max_reprojection_error: f64,
    ) -> PnPObservation {
        let board = BoardSpec::new(11, 8, 40.0).unwrap();
        let intrinsics = guided_test_intrinsics();
        let image_size = guided_test_image_size();
        let rotation = guided_pose_rotation(tilt_degrees, azimuth_degrees);
        let translation = guided_pose_target_translation(
            board,
            center_uv,
            scale,
            rotation,
            &intrinsics,
            image_size,
        )
        .unwrap();
        let pose = guided_pose_6dof_from_rotation_translation(
            board,
            rotation,
            translation,
            &intrinsics,
            image_size,
        )
        .unwrap();
        PnPObservation {
            binding_digest: SnapshotHash::digest_bytes(b"guided-pose-test"),
            rotation_vector: guided_test_rotation_vector(tilt_degrees, azimuth_degrees),
            translation_vector: translation,
            depth: pose.xyz[2],
            minimum_board_depth: pose.xyz[2],
            maximum_board_depth: pose.xyz[2],
            tilt_degrees,
            azimuth_degrees,
            reprojection_rmse,
            max_reprojection_error,
        }
    }

    fn guided_test_image_size() -> CalibrationImageSize {
        CalibrationImageSize::new(1000, 800).unwrap()
    }

    fn guided_test_intrinsics() -> InitialIntrinsics {
        InitialIntrinsics {
            camera_matrix: [900.0, 0.0, 500.0, 0.0, 900.0, 400.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 12],
        }
    }

    fn guided_test_target(
        center_uv: [f64; 2],
        scale: f64,
        tilt_degrees: f64,
        azimuth_degrees: f64,
    ) -> GuidedPoseTarget {
        let intrinsics = guided_test_intrinsics();
        let projection = guided_pose_grid_projection(
            BoardSpec::new(11, 8, 40.0).unwrap(),
            center_uv,
            scale,
            tilt_degrees,
            azimuth_degrees,
            &intrinsics,
            guided_test_image_size(),
        )
        .unwrap();
        GuidedPoseTarget {
            label: "test",
            pose: projection.pose,
            tolerance: GuidedPoseTolerance::default(),
            outline_uv: projection.outline_uv,
            grid_lines: projection.grid_lines,
        }
    }

    fn guided_test_target_depth(
        _board: BoardSpec,
        target: &GuidedPoseTarget,
        _intrinsics: &InitialIntrinsics,
        _image_size: CalibrationImageSize,
    ) -> f64 {
        target.pose.xyz[2]
    }

    fn guided_test_target_normal(target: &GuidedPoseTarget) -> [f64; 3] {
        [
            target.pose.rotation[0][2],
            target.pose.rotation[1][2],
            target.pose.rotation[2][2],
        ]
    }
    fn guided_hold_assessment(
        xyz: [f64; 3],
        rpy_degrees: [f64; 3],
        score: f64,
    ) -> GuidedPoseAssessment {
        GuidedPoseAssessment {
            step_index: 0,
            target_label: "test",
            measurement: GuidedPoseMeasurement {
                pose: GuidedPose6Dof {
                    xyz,
                    rpy_degrees,
                    rotation: guided_test_rpy_rotation(rpy_degrees),
                    translation: xyz,
                    center_uv: [0.5, 0.5],
                },
                board: BoardSpec::new(11, 8, 40.0).unwrap(),
                initial_intrinsics: guided_test_intrinsics(),
                image_size: guided_test_image_size(),
            },
            error: GuidedPoseError {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                roll_degrees: 0.0,
                pitch_degrees: 0.0,
                yaw_degrees: 0.0,
            },
            signed_rotation_error_degrees: [0.0; 3],
            pose_error_score: score,
            matched: true,
            reason: None,
        }
    }

    fn guided_hold_runtime() -> GuidedCaptureRuntime {
        let source = test_live_source();
        let frame = live_frame(1);
        let acquisition_key = source.acquisition_key_for_frame(&frame).unwrap();
        GuidedCaptureRuntime::standard_25(GuidedCaptureBinding {
            source,
            acquisition_key,
            image_size: guided_test_image_size(),
            board: BoardSpec::new(11, 8, 40.0).unwrap(),
            initial_intrinsics: guided_test_intrinsics(),
            intrinsics_digest: SnapshotHash::digest_bytes(b"guided-hold-runtime-test"),
        })
        .unwrap()
    }

    fn guided_hold_sample(sequence: u64, stability_score: f64) -> GuidedHoldSample {
        let session = CalibrationSession::new(BoardSpec::new(11, 8, 40.0).unwrap());
        let source = test_live_source();
        let frame = live_frame(sequence);
        let store = auto_capture_store();
        let acquisition_key = source.acquisition_key_for_frame(&frame).unwrap();
        let FrozenStreamInput { source, encoded } =
            freeze_stream_input(&frame, store, acquisition_key.clone(), None).unwrap();
        let token = session
            .bind_auto_candidate(
                AutoCandidateId::new(sequence),
                frame.identity.clone(),
                encoded.source_revision.clone(),
                source.display_name.clone(),
                Some(acquisition_key),
            )
            .unwrap();
        GuidedHoldSample {
            token,
            source,
            pose_request: None,
            guided_step_index: Some(0),
            stability_score,
        }
    }

    #[test]
    fn guided_hold_rejects_jitter_between_matched_frames() {
        let mut runtime = guided_hold_runtime();
        let first = guided_hold_assessment([0.0, 0.0, 1000.0], [0.0; 3], 0.2);
        let second = guided_hold_assessment([40.0, 0.0, 1000.0], [0.0; 3], 0.2);

        let first_update = runtime.update_hold(first, None);
        assert!(first_update.capture_sample.is_none());
        assert_eq!(runtime.hold_frames, 1);

        let second_update = runtime.update_hold(second, None);
        assert!(second_update.capture_sample.is_none());
        assert_eq!(runtime.hold_frames, 0);
        let assessment = runtime.last_assessment.as_ref().unwrap();
        assert!(!assessment.matched);
        assert!(
            assessment
                .reason
                .as_ref()
                .is_some_and(|reason| reason.contains("hold jitter"))
        );
    }

    #[test]
    fn guided_hold_captures_lowest_stability_sample() {
        let mut runtime = guided_hold_runtime();
        let inputs = [(1, 0.8), (2, 0.2), (3, 0.6), (4, 0.7)];
        let mut capture = None;
        for (sequence, score) in inputs {
            capture = runtime
                .update_hold(
                    guided_hold_assessment([0.0, 0.0, 1000.0], [0.0; 3], score),
                    Some(guided_hold_sample(sequence, score)),
                )
                .capture_sample;
        }

        let capture = capture.expect("four stable hold frames should trigger capture");
        assert_eq!(capture.token.id().get(), 2);
        assert_eq!(runtime.hold_frames, GUIDED_CAPTURE_HOLD_FRAMES);
        assert!(runtime.capture_requested);
    }

    #[test]
    fn guided_hold_full_queues_capture_without_waiting_for_next_frame() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let mut runtime = guided_hold_runtime();

        for sequence in 1..=GUIDED_CAPTURE_HOLD_FRAMES {
            let update = runtime.update_hold(
                guided_hold_assessment([0.0, 0.0, 1000.0], [0.0; 3], 0.1),
                Some(guided_hold_sample(u64::from(sequence), 0.1)),
            );
            if let Some(sample) = update.capture_sample {
                workspace.enqueue_guided_capture_sample(sample);
            }
        }

        let pending = workspace
            .auto_capture
            .pending
            .front()
            .expect("hold completion should queue a capture candidate immediately");
        assert_eq!(pending.intent, CandidateIntent::GuidedCapture);
        assert_eq!(pending.guided_step_index, Some(0));
        assert!(pending.encoded.is_some());
    }

    fn guided_test_rpy_rotation(rpy_degrees: [f64; 3]) -> [[f64; 3]; 3] {
        let [roll_degrees, pitch_degrees, yaw_degrees] = rpy_degrees;
        let (sin_roll, cos_roll) = roll_degrees.to_radians().sin_cos();
        let (sin_pitch, cos_pitch) = pitch_degrees.to_radians().sin_cos();
        let (sin_yaw, cos_yaw) = yaw_degrees.to_radians().sin_cos();

        [
            [
                cos_yaw * cos_pitch,
                cos_yaw * sin_pitch * sin_roll - sin_yaw * cos_roll,
                cos_yaw * sin_pitch * cos_roll + sin_yaw * sin_roll,
            ],
            [
                sin_yaw * cos_pitch,
                sin_yaw * sin_pitch * sin_roll + cos_yaw * cos_roll,
                sin_yaw * sin_pitch * cos_roll - cos_yaw * sin_roll,
            ],
            [-sin_pitch, cos_pitch * sin_roll, cos_pitch * cos_roll],
        ]
    }

    fn guided_test_pose_from_rpy(rpy_degrees: [f64; 3]) -> GuidedPose6Dof {
        let rotation = guided_test_rpy_rotation(rpy_degrees);
        GuidedPose6Dof {
            xyz: [0.0, 0.0, 1000.0],
            rpy_degrees: guided_pose_rotation_to_rpy_degrees(rotation).unwrap(),
            rotation,
            translation: [0.0; 3],
            center_uv: [0.5, 0.5],
        }
    }

    fn assert_degrees_near(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() <= 1.0e-9,
            "expected {expected}°, got {actual}°"
        );
    }

    #[test]
    fn guided_pose_rotation_ring_sweep_emphasizes_small_errors_and_preserves_direction() {
        let tiny = guided_pose_rotation_ring_visual_sweep_degrees(1.0)
            .expect("finite angle produces a sweep");
        assert!(tiny > 3.0 && tiny < 4.0, "tiny sweep {tiny}");

        let positive = guided_pose_rotation_ring_visual_sweep_degrees(15.0)
            .expect("finite angle produces a sweep");
        assert!(
            positive > 15.0 && positive < 23.0,
            "positive sweep {positive}"
        );

        let negative = guided_pose_rotation_ring_visual_sweep_degrees(-5.0)
            .expect("finite angle produces a sweep");
        assert!(
            negative < -5.0 && negative > -14.0,
            "negative sweep {negative}"
        );

        let large = guided_pose_rotation_ring_visual_sweep_degrees(179.5)
            .expect("finite angle produces a sweep");
        assert!((large - 179.5).abs() <= 1.0e-3, "large sweep {large}");

        assert!(guided_pose_rotation_ring_visual_sweep_degrees(f64::NAN).is_none());
    }

    #[test]
    fn guided_rotation_rings_report_signed_errors_from_current_pose() {
        let target = guided_test_target([0.50, 0.50], 0.50, 0.0, 0.0);
        let assessment = assess_guided_pose(
            0,
            &target,
            &guided_test_detection([0.48, 0.52], 0.50),
            &guided_test_pnp([0.48, 0.52], 0.50, 12.0, 45.0, 0.8, 2.0),
            BoardSpec::new(11, 8, 40.0).unwrap(),
            &guided_test_intrinsics(),
            guided_test_image_size(),
        )
        .unwrap();

        let rings = guided_pose_rotation_rings_overlay(&assessment, &target);

        assert_eq!(rings.center_uv, assessment.measurement.pose.center_uv);
        assert_eq!(rings.roll.base_uv.len(), GUIDED_POSE_RING_SEGMENTS + 1);
        assert_eq!(
            rings.pitch.base_uv.len(),
            GUIDED_POSE_HALF_RING_SEGMENTS + 1
        );
        assert_eq!(rings.yaw.base_uv.len(), GUIDED_POSE_HALF_RING_SEGMENTS + 1);
        assert_eq!(rings.pitch.tick_uv, rings.yaw.tick_uv);
        assert!(
            rings
                .roll
                .base_uv
                .iter()
                .flatten()
                .all(|value| value.is_finite())
        );
        assert!(
            rings
                .pitch
                .base_uv
                .iter()
                .flatten()
                .all(|value| value.is_finite())
        );
        assert!(
            rings
                .yaw
                .base_uv
                .iter()
                .flatten()
                .all(|value| value.is_finite())
        );
        assert_eq!(rings.roll.label, "ROLL");
        assert_eq!(rings.pitch.label, "PITCH");
        assert_eq!(rings.yaw.label, "YAW");
        assert_eq!(
            rings.roll.tolerance_degrees,
            GUIDED_POSE_ROLL_TOLERANCE_DEGREES
        );
        assert!((rings.roll.error_degrees.abs() - assessment.error.roll_degrees).abs() < 1.0e-9);
        assert!((rings.pitch.error_degrees.abs() - assessment.error.pitch_degrees).abs() < 1.0e-9);
        assert!((rings.yaw.error_degrees.abs() - assessment.error.yaw_degrees).abs() < 1.0e-9);
        assert!(
            rings.roll.error_degrees.abs()
                + rings.pitch.error_degrees.abs()
                + rings.yaw.error_degrees.abs()
                > 1.0
        );
    }

    #[test]
    fn guided_rotation_ring_local_planes_match_board_axes() {
        let board = BoardSpec::new(11, 8, 40.0).unwrap();
        let center = guided_pose_inner_center_point(board);
        let radius = guided_pose_rotation_ring_radius(board);

        let roll_top = guided_pose_rotation_ring_local_point(
            center,
            radius,
            GuidedPoseRotationRingPlane::RollXy,
            -90.0_f32.to_radians(),
        );
        assert_eq!(roll_top[2], center[2]);
        assert!(roll_top[1] < center[1]);

        let pitch_dome = guided_pose_rotation_ring_local_point(
            center,
            radius,
            GuidedPoseRotationRingPlane::PitchYzNegativeZ,
            90.0_f32.to_radians(),
        );
        let yaw_dome = guided_pose_rotation_ring_local_point(
            center,
            radius,
            GuidedPoseRotationRingPlane::YawXzNegativeZ,
            90.0_f32.to_radians(),
        );

        assert!((pitch_dome[0] - center[0]).abs() < 1.0e-6);
        assert!((pitch_dome[1] - center[1]).abs() < 1.0e-4);
        assert!((yaw_dome[0] - center[0]).abs() < 1.0e-4);
        assert!((yaw_dome[1] - center[1]).abs() < 1.0e-6);
        assert!((pitch_dome[2] - (center[2] - radius)).abs() < 1.0e-4);
        assert!((yaw_dome[2] - (center[2] - radius)).abs() < 1.0e-4);
    }

    #[test]
    fn guided_rotation_error_uses_operator_roll_pitch_yaw_axes() {
        let measurement = guided_test_pose_from_rpy([20.0, -5.0, 179.0]);
        let target = guided_test_pose_from_rpy([5.0, 7.0, -179.0]);

        let error = guided_pose_rotation_error_degrees(
            &measurement,
            &target,
            GuidedPoseTolerance::default(),
        )
        .unwrap();

        assert_degrees_near(error[0], 2.0);
        assert_degrees_near(error[1], -15.0);
        assert_degrees_near(error[2], 12.0);
    }

    #[test]
    fn guided_instruction_hud_reports_dominant_board_move_and_hold() {
        let target = guided_test_target([0.50, 0.50], 0.50, 0.0, 0.0);
        let shifted = assess_guided_pose(
            0,
            &target,
            &guided_test_detection([0.25, 0.50], 0.50),
            &guided_test_pnp([0.25, 0.50], 0.50, 0.0, 0.0, 0.8, 2.0),
            BoardSpec::new(11, 8, 40.0).unwrap(),
            &guided_test_intrinsics(),
            guided_test_image_size(),
        )
        .unwrap();

        let shifted_instruction = guided_pose_instruction_overlay(&shifted, &target, 0);
        assert!(!shifted_instruction.matched);
        assert_eq!(shifted_instruction.primary, "MOVE BOARD RIGHT");
        assert!(shifted_instruction.secondary.contains("pose error"));

        let aligned = assess_guided_pose(
            0,
            &target,
            &guided_test_detection([0.50, 0.50], 0.50),
            &guided_test_pnp([0.50, 0.50], 0.50, 0.0, 0.0, 0.8, 2.0),
            BoardSpec::new(11, 8, 40.0).unwrap(),
            &guided_test_intrinsics(),
            guided_test_image_size(),
        )
        .unwrap();
        let hold_instruction = guided_pose_instruction_overlay(&aligned, &target, 2);

        assert!(hold_instruction.matched);
        assert_eq!(hold_instruction.primary, "HOLD STILL");
        assert!(hold_instruction.secondary.contains("hold 2/4"));
    }

    #[test]
    fn guided_pose_assessment_matches_preset_thresholds_and_wraps_yaw() {
        let target = guided_test_target([0.50, 0.50], 0.50, 20.0, 359.0);
        let detection = guided_test_detection([0.50, 0.50], 0.50);
        let pnp = guided_test_pnp([0.50, 0.50], 0.50, 24.0, 1.0, 0.8, 2.0);

        let assessment = assess_guided_pose(
            0,
            &target,
            &detection,
            &pnp,
            BoardSpec::new(11, 8, 40.0).unwrap(),
            &guided_test_intrinsics(),
            guided_test_image_size(),
        )
        .unwrap();

        assert!(assessment.matched, "{assessment:?}");
        assert!(assessment.pose_error_score <= 1.0, "{assessment:?}");
        assert!(assessment.error.yaw_degrees <= GUIDED_POSE_YAW_TOLERANCE_DEGREES);
    }

    #[test]
    fn guided_pose_assessment_accepts_chessboard_half_turn_yaw_ambiguity() {
        let board = BoardSpec::new(11, 8, 40.0).unwrap();
        let target = guided_test_target([0.50, 0.50], 0.50, 0.0, 0.0);
        let center = guided_pose_inner_center_point(board);
        let rotated_center = [-center[0], -center[1], center[2]];
        let translation = [
            target.pose.xyz[0] - rotated_center[0],
            target.pose.xyz[1] - rotated_center[1],
            target.pose.xyz[2] - rotated_center[2],
        ];
        let pnp = PnPObservation {
            binding_digest: SnapshotHash::digest_bytes(b"guided-pose-half-turn-test"),
            rotation_vector: [0.0, 0.0, std::f64::consts::PI],
            translation_vector: translation,
            depth: target.pose.xyz[2],
            minimum_board_depth: target.pose.xyz[2],
            maximum_board_depth: target.pose.xyz[2],
            tilt_degrees: 0.0,
            azimuth_degrees: 0.0,
            reprojection_rmse: 0.8,
            max_reprojection_error: 2.0,
        };

        let assessment = assess_guided_pose(
            0,
            &target,
            &guided_test_detection([0.50, 0.50], 0.50),
            &pnp,
            board,
            &guided_test_intrinsics(),
            guided_test_image_size(),
        )
        .unwrap();

        assert!(assessment.matched, "{assessment:?}");
        assert!(
            assessment.error.yaw_degrees <= 1.0e-9,
            "half-turn chessboard ambiguity must not report 180° yaw: {assessment:?}"
        );
    }

    #[test]
    fn guided_pose_assessment_rejects_6dof_error_without_pnp_rmse_gate() {
        let target = guided_test_target([0.50, 0.50], 0.50, 20.0, 90.0);
        let far_detection = guided_test_detection([0.80, 0.50], 0.50);
        let far_pnp = guided_test_pnp([0.80, 0.50], 0.50, 20.0, 90.0, 0.8, 2.0);

        let pose_rejected = assess_guided_pose(
            0,
            &target,
            &far_detection,
            &far_pnp,
            BoardSpec::new(11, 8, 40.0).unwrap(),
            &guided_test_intrinsics(),
            guided_test_image_size(),
        )
        .unwrap();
        assert!(!pose_rejected.matched);
        assert!(pose_rejected.pose_error_score > 1.0);

        let noisy_but_aligned = assess_guided_pose(
            0,
            &target,
            &guided_test_detection([0.50, 0.50], 0.50),
            &guided_test_pnp([0.50, 0.50], 0.50, 20.0, 90.0, 999.0, 999.0),
            BoardSpec::new(11, 8, 40.0).unwrap(),
            &guided_test_intrinsics(),
            guided_test_image_size(),
        )
        .unwrap();
        assert!(
            noisy_but_aligned.matched,
            "Guided pose should ignore PnP RMSE/max reprojection gates: {noisy_but_aligned:?}"
        );
    }

    #[test]
    fn guided_pose_overlay_projects_complete_perspective_chessboard_grid() {
        let intrinsics = guided_test_intrinsics();
        let image_size = guided_test_image_size();
        let board = BoardSpec::new(11, 8, 40.0).unwrap();
        let plan = standard_guided_pose_plan(board, &intrinsics, image_size).unwrap();

        assert_eq!(plan.len(), 45);

        let target_labels = plan.iter().map(|target| target.label).collect::<Vec<_>>();
        for required in [
            "Far left field · fronto",
            "Far top field · fronto",
            "Far right field · fronto",
            "Far bottom field · fronto",
            "Close upper left corner · fronto",
            "Outer upper left corner · fronto",
            "Close upper right corner · fronto",
            "Outer upper right corner · fronto",
            "Close lower left corner · fronto",
            "Outer lower left corner · fronto",
            "Close lower right corner · fronto",
            "Outer lower right corner · fronto",
            "Right edge · low tilt",
            "Top edge · low tilt",
            "Left edge · low tilt",
            "Bottom edge · low tilt",
            "Upper right corner · low tilt",
            "Upper left corner · low tilt",
            "Lower left corner · low tilt",
            "Lower right corner · low tilt",
        ] {
            assert!(
                target_labels.contains(&required),
                "missing guided target: {required}"
            );
        }

        let target_tilts = plan
            .iter()
            .map(|target| {
                let normal = guided_test_target_normal(target);
                normal[0]
                    .hypot(normal[1])
                    .atan2(normal[2])
                    .to_degrees()
                    .round()
            })
            .collect::<Vec<_>>();
        assert_eq!(target_tilts.iter().filter(|tilt| **tilt == 0.0).count(), 13);
        assert_eq!(
            target_tilts.iter().filter(|tilt| **tilt == 12.0).count(),
            16
        );
        assert_eq!(target_tilts.iter().filter(|tilt| **tilt == 20.0).count(), 8);
        assert_eq!(target_tilts.iter().filter(|tilt| **tilt == 28.0).count(), 8);

        for (index, target) in plan.iter().enumerate() {
            assert!(
                target
                    .pose
                    .rpy_degrees
                    .iter()
                    .all(|value| value.abs() <= 30.0),
                "guided target '{}' RPY must stay within ±30°: {:?}",
                target.label,
                target.pose.rpy_degrees
            );
            for uv in target.outline_uv.iter().copied().chain(
                target
                    .grid_lines
                    .iter()
                    .flat_map(|line| [line.start_uv, line.end_uv]),
            ) {
                assert!(
                    uv.iter()
                        .all(|value| value.is_finite() && (-0.05..=1.05).contains(value)),
                    "guided target #{index} '{}' must stay finite and near normalized image bounds: {:?}",
                    target.label,
                    uv
                );
            }
        }

        let tilted = plan
            .iter()
            .find(|target| target.label == "Right · low tilt")
            .unwrap();
        assert_eq!(
            tilted.grid_lines.len(),
            usize::from(board.inner_cols) + usize::from(board.inner_rows) + 4
        );
        assert!(
            tilted
                .outline_uv
                .iter()
                .flatten()
                .all(|value| value.is_finite())
        );
        assert!(tilted.grid_lines.iter().all(|line| {
            line.start_uv
                .iter()
                .chain(line.end_uv.iter())
                .all(|value| value.is_finite())
        }));
        assert!(
            (tilted.outline_uv[0][1] - tilted.outline_uv[1][1]).abs() > 1.0e-4,
            "tilted target should not render as an axis-aligned box: {:?}",
            tilted.outline_uv
        );

        let mut distorted = intrinsics;
        distorted.distortion_coefficients[0] = 0.15;
        let distorted_plan = standard_guided_pose_plan(board, &distorted, image_size).unwrap();
        let distorted_tilted = distorted_plan
            .iter()
            .find(|target| target.label == "Right · low tilt")
            .unwrap();
        assert_ne!(
            tilted.outline_uv, distorted_tilted.outline_uv,
            "guided target grid must be projected through the bound K/D12 model"
        );

        let sixteen_nine_size = CalibrationImageSize::new(1920, 1080).unwrap();
        let sixteen_nine_intrinsics = InitialIntrinsics {
            camera_matrix: [900.0, 0.0, 960.0, 0.0, 900.0, 540.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 12],
        };
        let sixteen_nine_plan =
            standard_guided_pose_plan(board, &sixteen_nine_intrinsics, sixteen_nine_size).unwrap();
        let mut corner_bins = [[0_usize; 4]; 4];
        let mut corner_region_counts = [0_usize; 8];
        let mut corner_min = [f64::INFINITY; 2];
        let mut corner_max = [f64::NEG_INFINITY; 2];
        let mut minimum_depth = f64::INFINITY;
        let mut maximum_depth = f64::NEG_INFINITY;
        let mut far_depth_count = 0_usize;
        let mut adjacent_translation_total = 0.0_f64;
        let mut adjacent_translation_max = 0.0_f64;
        let mut adjacent_translation_count = 0_usize;
        let mut previous_xyz = None::<[f64; 3]>;
        for target in &sixteen_nine_plan {
            let depth = guided_test_target_depth(
                board,
                target,
                &sixteen_nine_intrinsics,
                sixteen_nine_size,
            );
            minimum_depth = minimum_depth.min(depth);
            maximum_depth = maximum_depth.max(depth);
            if (700.0..=1000.0).contains(&depth) {
                far_depth_count += 1;
            }
            if target.label.contains("corner · fronto") {
                assert!(
                    depth <= 560.0,
                    "fronto corner '{}' should stay close enough to avoid large operator motion: {depth:.3}",
                    target.label
                );
            }
            if target.label.contains("field · fronto") {
                assert!(
                    (700.0..=850.0).contains(&depth),
                    "fronto middle-field '{}' should keep moderate far-depth samples: {depth:.3}",
                    target.label
                );
            }
            if let Some(previous_xyz) = previous_xyz {
                let delta = previous_xyz
                    .iter()
                    .zip(target.pose.xyz.iter())
                    .map(|(left, right)| (left - right).powi(2))
                    .sum::<f64>()
                    .sqrt();
                adjacent_translation_total += delta;
                adjacent_translation_max = adjacent_translation_max.max(delta);
                adjacent_translation_count += 1;
            }
            previous_xyz = Some(target.pose.xyz);
            for row in 0..board.inner_rows {
                for column in 0..board.inner_cols {
                    let point = project_board_point_image(
                        target.pose.rotation,
                        target.pose.translation,
                        guided_pose_board_point(board, f64::from(column), f64::from(row)),
                        &sixteen_nine_intrinsics,
                    )
                    .unwrap();
                    let x = f64::from(point.x);
                    let y = f64::from(point.y);
                    corner_min[0] = corner_min[0].min(x);
                    corner_min[1] = corner_min[1].min(y);
                    corner_max[0] = corner_max[0].max(x);
                    corner_max[1] = corner_max[1].max(y);
                    let bin_x = ((x / f64::from(sixteen_nine_size.width)) * 4.0)
                        .floor()
                        .clamp(0.0, 3.0) as usize;
                    let bin_y = ((y / f64::from(sixteen_nine_size.height)) * 4.0)
                        .floor()
                        .clamp(0.0, 3.0) as usize;
                    corner_bins[bin_y][bin_x] += 1;
                    if x < f64::from(sixteen_nine_size.width) * 0.15 {
                        corner_region_counts[0] += 1;
                    }
                    if x > f64::from(sixteen_nine_size.width) * 0.85 {
                        corner_region_counts[1] += 1;
                    }
                    if y < f64::from(sixteen_nine_size.height) * 0.15 {
                        corner_region_counts[2] += 1;
                    }
                    if y > f64::from(sixteen_nine_size.height) * 0.85 {
                        corner_region_counts[3] += 1;
                    }
                    if x < f64::from(sixteen_nine_size.width) * 0.25
                        && y < f64::from(sixteen_nine_size.height) * 0.25
                    {
                        corner_region_counts[4] += 1;
                    }
                    if x > f64::from(sixteen_nine_size.width) * 0.75
                        && y < f64::from(sixteen_nine_size.height) * 0.25
                    {
                        corner_region_counts[5] += 1;
                    }
                    if x < f64::from(sixteen_nine_size.width) * 0.25
                        && y > f64::from(sixteen_nine_size.height) * 0.75
                    {
                        corner_region_counts[6] += 1;
                    }
                    if x > f64::from(sixteen_nine_size.width) * 0.75
                        && y > f64::from(sixteen_nine_size.height) * 0.75
                    {
                        corner_region_counts[7] += 1;
                    }
                }
            }
        }
        assert!(
            minimum_depth <= 500.0 && maximum_depth >= 750.0 && maximum_depth <= 850.0,
            "16:9 guided depth distribution should include close and moderate 700..850 far samples: {minimum_depth:.3}..{maximum_depth:.3}"
        );
        assert!(
            far_depth_count >= 4,
            "far middle-field samples: {far_depth_count}"
        );
        assert_eq!(adjacent_translation_count, sixteen_nine_plan.len() - 1);
        let adjacent_translation_average =
            adjacent_translation_total / adjacent_translation_count.max(1) as f64;
        assert!(
            adjacent_translation_max <= 260.0 && adjacent_translation_average <= 100.0,
            "guided pose order should keep adjacent jumps compact: max={adjacent_translation_max:.3}, avg={adjacent_translation_average:.3}"
        );
        assert!(
            corner_min[0] <= 200.0
                && corner_max[0] >= 1720.0
                && corner_min[1] <= 65.0
                && corner_max[1] >= 1015.0,
            "16:9 inner-corner coverage should reach the frame edges: min={corner_min:?} max={corner_max:?}"
        );
        assert!(
            corner_bins[0][0] >= 36
                && corner_bins[0][3] >= 36
                && corner_bins[3][0] >= 36
                && corner_bins[3][3] >= 36,
            "16:9 corner bins lack samples: {corner_bins:?}"
        );
        assert!(
            corner_region_counts[0] >= 64
                && corner_region_counts[1] >= 64
                && corner_region_counts[2] >= 96
                && corner_region_counts[3] >= 96,
            "16:9 edge regions lack samples: {corner_region_counts:?}"
        );
        assert!(
            corner_region_counts[4..].iter().all(|count| *count >= 36),
            "16:9 corner regions lack samples: {corner_region_counts:?}"
        );
    }
    #[test]
    fn enabled_row_without_admission_contribution_is_outside_active_set() {
        assert_eq!(
            admission_delta_cell_state(
                None::<&AutoAdmissionItemContribution>,
                true,
                |_| false,
                |contribution| contribution.field_gain,
            ),
            AdmissionDeltaCellState::OutsideActiveAdmission
        );
    }

    #[test]
    fn dataset_table_renders_total_gain_cell() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        install_detection_outcome(&mut workspace, "view.png", found_detection(640, 480));
        let assessment = workspace
            .dataset_acceptance_assessment()
            .expect("Found item should produce Dataset Acceptance assessment");
        let [contribution] = assessment.item_contributions.as_slice() else {
            panic!("expected one Dataset contribution");
        };
        assert!(contribution.pnp_state.is_blocked());
        assert!(contribution.constraint_gain > 0.0);
        let expected_gain = format!("+{}*", format_gain(contribution.constraint_gain));

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1600.0, 600.0),
            )),
            ..Default::default()
        };
        let output = context.run_ui(input, |ui| {
            workspace.render_dataset(ui, true);
        });
        let text = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree is enabled")
            .nodes
            .into_iter()
            .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Gain"), "missing Gain header in {text}");
        assert!(
            text.contains(&expected_gain),
            "missing total gain cell {expected_gain:?} in {text}"
        );
    }

    #[test]
    fn opened_pngs_produce_preview_with_mode_unchanged() {
        let root = std::env::temp_dir().join(format!(
            "camera-toolbox-calibration-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let fixture = include_bytes!("../../../adapters/tests/data/chessboard_11x8_clean.png");
        let first_path = root.join("first.png");
        let second_path = root.join("second.png");
        std::fs::write(&first_path, fixture).unwrap();
        std::fs::write(&second_path, fixture).unwrap();

        let source_id = FileSourceId::new("calibration-test").unwrap();
        let file_system: Arc<dyn FileSystem> =
            Arc::new(LocalFileSystem::new(source_id.clone(), &root).unwrap());
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        workspace.auto_intrinsics = false;
        workspace.fx = 850.0;
        workspace.fy = 875.0;
        workspace.cx = 321.0;
        workspace.cy = 239.0;
        assert!(
            workspace.session.initial_intrinsics_binding().is_none(),
            "local Dataset PnP must not require a live source-bound binding"
        );
        let candidate = |name: &str, display_path: PathBuf| {
            let reference =
                camera_toolbox_app::FileRef::new(source_id.clone(), SourcePath::new(name).unwrap());
            let control = FsControl::with_timeout(Duration::from_secs(2));
            CalibrationImportCandidate {
                display_path,
                file_system: Arc::clone(&file_system),
                entry: file_system.stat(&reference, &control).unwrap(),
                remote: false,
            }
        };

        workspace.import(vec![candidate("first.png", first_path)]);
        workspace.preview_mode = CalibrationPreviewMode::Overlay;
        assert_eq!(workspace.active_job, Some(CalibrationJobKind::Detect));
        workspace.import(vec![candidate("second.png", second_path)]);

        let deadline = Instant::now() + Duration::from_secs(10);
        while workspace.active_job.is_some() && Instant::now() < deadline {
            workspace.poll_worker(&context);
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            workspace.active_job.is_none(),
            "worker did not finish: {}",
            workspace.status
        );
        assert_eq!(workspace.session.items().len(), 2);
        assert!(
            workspace
                .session
                .items()
                .iter()
                .all(|item| matches!(item.status, CalibrationItemStatus::Found(_)))
        );
        let pnp_binding = workspace
            .dataset_pnp_binding(match &workspace.session.items()[0].status {
                CalibrationItemStatus::Found(detection) => detection.image_size,
                _ => panic!("fixture must be Found"),
            })
            .expect("current GUI K must create a source-independent Dataset PnP binding");
        assert_eq!(pnp_binding.initial_intrinsics.camera_matrix[0], 850.0);
        assert_eq!(pnp_binding.initial_intrinsics.camera_matrix[4], 875.0);
        assert_eq!(pnp_binding.initial_intrinsics.camera_matrix[2], 321.0);
        assert_eq!(pnp_binding.initial_intrinsics.camera_matrix[5], 239.0);
        let pnp_binding = pnp_binding.digest;
        assert!(workspace.session.items().iter().all(|item| {
            item.pnp_observation
                .as_ref()
                .is_some_and(|observation| observation.binding_digest == pnp_binding)
        }));
        workspace.fx = 925.0;
        let refreshed_binding = workspace
            .dataset_pnp_binding(match &workspace.session.items()[0].status {
                CalibrationItemStatus::Found(detection) => detection.image_size,
                _ => panic!("fixture must be Found"),
            })
            .expect("edited GUI K must create a refreshed Dataset PnP binding")
            .digest;
        assert_ne!(refreshed_binding, pnp_binding);
        workspace.start_dataset_pnp_refresh();
        let deadline = Instant::now() + Duration::from_secs(10);
        while workspace.active_job.is_some() && Instant::now() < deadline {
            workspace.poll_worker(&context);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            workspace.active_job.is_none(),
            "PnP refresh did not finish: {}",
            workspace.status
        );
        assert!(workspace.session.items().iter().all(|item| {
            item.pnp_observation
                .as_ref()
                .is_some_and(|observation| observation.binding_digest == refreshed_binding)
        }));
        workspace.sync_coverage(&context);
        assert_eq!(workspace.preview_mode, CalibrationPreviewMode::Overlay);
        let selected_id = workspace
            .session
            .selected()
            .expect("a found result must be selected");
        assert!(workspace.sources[&selected_id].preview.is_some());
        let texture_id = workspace.sources[&selected_id]
            .preview
            .as_ref()
            .unwrap()
            .texture
            .id();
        let texture_manager = context.tex_manager();
        let options = texture_manager.read().meta(texture_id).unwrap().options;
        assert_eq!(options.magnification, egui::TextureFilter::Nearest);
        assert_eq!(options.minification, egui::TextureFilter::Linear);
        assert_eq!(options.mipmap_mode, Some(egui::TextureFilter::Linear));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn latest_found_completion_selects_preview_without_changing_layers() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let source_id = FileSourceId::new("calibration-selection-test").unwrap();
        let version = camera_toolbox_app::FileVersion {
            size: 1,
            modified_millis: Some(1),
        };
        let first_reference = camera_toolbox_app::FileRef::new(
            source_id.clone(),
            SourcePath::new("first.png").unwrap(),
        );
        let second_reference =
            camera_toolbox_app::FileRef::new(source_id, SourcePath::new("second.png").unwrap());
        let AddCalibrationItemOutcome::Added(first) =
            workspace
                .session
                .add_or_refresh(first_reference, version, "first.png".to_owned())
        else {
            panic!("first image must be added");
        };
        let AddCalibrationItemOutcome::Added(second) =
            workspace
                .session
                .add_or_refresh(second_reference, version, "second.png".to_owned())
        else {
            panic!("second image must be added");
        };
        let first_token = workspace.session.begin_detection(first).unwrap();
        let second_token = workspace.session.begin_detection(second).unwrap();
        for token in [&first_token, &second_token] {
            workspace.session.mark_reading(token).unwrap();
            workspace.session.mark_detect_queued(token).unwrap();
            workspace.session.mark_detecting(token).unwrap();
        }
        workspace.active_detection_batch = Some(DetectionBatch {
            id: 1,
            total: 2,
            completed: 0,
            reserved_encoded_bytes: 0,
            cancel_requested: false,
            terminal_status: None,
            cancellations: HashMap::new(),
            active_remote_sources: HashMap::new(),
        });
        workspace.preview_mode = CalibrationPreviewMode::Overlay;

        workspace.handle_detection_result(
            &context,
            DetectionStageResult {
                batch_id: 1,
                request: EncodedDetectionRequest::Dataset(second_token),
                reserved_bytes: 0,
                result: Ok(DetectionProduct {
                    source_revision: version.clone().into(),
                    outcome: found_detection(640, 480),
                    pnp_observation: None,
                    preview: None,
                }),
            },
        );
        assert_eq!(workspace.session.selected(), Some(second));
        assert_eq!(workspace.preview_mode, CalibrationPreviewMode::Overlay);

        workspace.handle_detection_result(
            &context,
            DetectionStageResult {
                batch_id: 1,
                request: EncodedDetectionRequest::Dataset(first_token),
                reserved_bytes: 0,
                result: Ok(DetectionProduct {
                    source_revision: version.into(),
                    outcome: found_detection(640, 480),
                    pnp_observation: None,
                    preview: None,
                }),
            },
        );
        assert_eq!(workspace.session.selected(), Some(first));
        assert_eq!(workspace.preview_mode, CalibrationPreviewMode::Overlay);
    }

    #[test]
    fn oversized_import_fails_without_leaving_batch_active() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "camera-toolbox-calibration-oversized-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("oversized.png");
        std::fs::write(&path, b"not-read").unwrap();
        let source_id = FileSourceId::new("calibration-oversized-test").unwrap();
        let file_system: Arc<dyn FileSystem> =
            Arc::new(LocalFileSystem::new(source_id.clone(), &root).unwrap());
        let reference =
            camera_toolbox_app::FileRef::new(source_id, SourcePath::new("oversized.png").unwrap());
        let control = FsControl::with_timeout(Duration::from_secs(1));
        let mut entry = file_system.stat(&reference, &control).unwrap();
        entry.version.size = MAX_ENCODED_PNG_BYTES + 1;
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();

        workspace.import(vec![CalibrationImportCandidate {
            display_path: path,
            file_system,
            entry,
            remote: false,
        }]);

        assert!(workspace.active_job.is_none());
        assert!(workspace.pending_reads.is_empty());
        assert!(workspace.pending_dataset_loaded.is_empty());
        assert!(matches!(
            &workspace.session.items()[0].status,
            CalibrationItemStatus::Failed(message) if message.contains("limit")
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_detection_resets_results_for_enabled_and_disabled_items() {
        let root = std::env::temp_dir().join(format!(
            "camera-toolbox-calibration-reset-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let fixture = include_bytes!("../../../adapters/tests/data/chessboard_11x8_clean.png");
        std::fs::write(root.join("enabled.png"), fixture).unwrap();
        std::fs::write(root.join("disabled.png"), fixture).unwrap();
        let source_id = FileSourceId::new("calibration-session-test").unwrap();
        let file_system: Arc<dyn FileSystem> =
            Arc::new(LocalFileSystem::new(source_id.clone(), &root).unwrap());
        let version_for = |name: &str| {
            let reference =
                camera_toolbox_app::FileRef::new(source_id.clone(), SourcePath::new(name).unwrap());
            file_system
                .stat(&reference, &FsControl::with_timeout(Duration::from_secs(1)))
                .unwrap()
                .version
        };
        let enabled_version = version_for("enabled.png");
        let disabled_version = version_for("disabled.png");

        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let enabled = install_detection_outcome_with_version(
            &mut workspace,
            "enabled.png",
            enabled_version,
            found_detection(640, 480),
        );
        let disabled = install_detection_outcome_with_version(
            &mut workspace,
            "disabled.png",
            disabled_version,
            found_detection(640, 480),
        );
        workspace.session.set_enabled(disabled, false).unwrap();
        workspace.sources.insert(
            enabled,
            CalibrationSource::file(root.join("enabled.png"), Arc::clone(&file_system), false),
        );
        workspace.sources.insert(
            disabled,
            CalibrationSource::file(root.join("disabled.png"), file_system, false),
        );
        workspace.preview_mode = CalibrationPreviewMode::Heatmap;

        workspace.start_detection();

        assert!(matches!(
            workspace.session.items().iter().find(|item| item.id == enabled),
            Some(item)
                if matches!(
                    item.status,
                    CalibrationItemStatus::ReadQueued
                        | CalibrationItemStatus::Reading
                        | CalibrationItemStatus::DetectQueued
                        | CalibrationItemStatus::Detecting
                )
        ));
        assert!(matches!(
            workspace.session.items().iter().find(|item| item.id == disabled),
            Some(item) if matches!(item.status, CalibrationItemStatus::Pending)
        ));
        assert!(workspace.coverage.is_none());
        assert_eq!(workspace.preview_mode, CalibrationPreviewMode::Heatmap);

        workspace.cancel_active_job();
        drop(workspace);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn install_detection_outcome(
        workspace: &mut CalibrationWorkspace,
        name: &str,
        outcome: camera_toolbox_core::ChessboardDetectionOutcome,
    ) -> CalibrationItemId {
        install_detection_outcome_with_version(
            workspace,
            name,
            camera_toolbox_app::FileVersion {
                size: 128,
                modified_millis: Some(1),
            },
            outcome,
        )
    }

    fn install_detection_outcome_with_version(
        workspace: &mut CalibrationWorkspace,
        name: &str,
        version: camera_toolbox_app::FileVersion,
        outcome: camera_toolbox_core::ChessboardDetectionOutcome,
    ) -> CalibrationItemId {
        let reference = camera_toolbox_app::FileRef::new(
            FileSourceId::new("calibration-session-test").unwrap(),
            SourcePath::new(name).unwrap(),
        );
        let AddCalibrationItemOutcome::Added(id) =
            workspace
                .session
                .add_or_refresh(reference, version, name.to_owned())
        else {
            panic!("expected a new calibration item");
        };
        let token = workspace.session.begin_detection(id).unwrap();
        workspace.session.mark_reading(&token).unwrap();
        workspace.session.mark_detect_queued(&token).unwrap();
        workspace.session.mark_detecting(&token).unwrap();
        workspace
            .session
            .install_detection(&token, version, outcome)
            .unwrap();
        id
    }

    fn found_detection(width: u32, height: u32) -> camera_toolbox_core::ChessboardDetectionOutcome {
        let corners = (0..8)
            .flat_map(|row| {
                (0..11).map(move |col| CalibrationPoint {
                    x: 60.0 + col as f32 * 40.0,
                    y: 60.0 + row as f32 * 40.0,
                })
            })
            .collect();
        camera_toolbox_core::ChessboardDetectionOutcome::Found(
            camera_toolbox_core::ChessboardDetection {
                image_size: CalibrationImageSize { width, height },
                corners,
            },
        )
    }

    fn render_controls_frame(
        context: &egui::Context,
        workspace: &mut CalibrationWorkspace,
        time: f64,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 360.0),
            )),
            time: Some(time),
            ..Default::default()
        };
        input.events = events;
        context.run_ui(input, |ui| {
            workspace.render_controls(ui);
        })
    }

    fn spin_button_center_by_value(output: &egui::FullOutput, value: &str) -> egui::Pos2 {
        let bounds = output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("accessibility tree is enabled")
            .nodes
            .iter()
            .find_map(|(_, node)| {
                (node.role() == egui::accesskit::Role::SpinButton
                    && node
                        .value()
                        .is_some_and(|node_value| node_value.contains(value)))
                .then(|| node.bounds())
                .flatten()
            })
            .unwrap_or_else(|| panic!("spin button with value {value:?} is visible"));
        #[allow(clippy::cast_possible_truncation)]
        egui::pos2(
            ((bounds.x0 + bounds.x1) * 0.5) as f32,
            ((bounds.y0 + bounds.y1) * 0.5) as f32,
        )
    }

    fn pointer_button(
        position: egui::Pos2,
        button: egui::PointerButton,
        pressed: bool,
    ) -> egui::Event {
        egui::Event::PointerButton {
            pos: position,
            button,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    #[test]
    fn intrinsics_text_entry_keeps_focus_until_commit() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        workspace.auto_intrinsics = false;
        workspace.fx = 812.0;
        workspace.fy = 823.0;
        workspace.cx = 334.0;
        workspace.cy = 245.0;
        install_detection_outcome(&mut workspace, "view.png", found_detection(640, 480));

        let output = render_controls_frame(
            &context,
            &mut workspace,
            0.0,
            vec![egui::Event::WindowFocused(true)],
        );
        let fx_position = spin_button_center_by_value(&output, "812");
        render_controls_frame(
            &context,
            &mut workspace,
            0.1,
            vec![
                egui::Event::PointerMoved(fx_position),
                pointer_button(fx_position, egui::PointerButton::Primary, true),
            ],
        );
        render_controls_frame(
            &context,
            &mut workspace,
            0.2,
            vec![pointer_button(
                fx_position,
                egui::PointerButton::Primary,
                false,
            )],
        );
        let focused_id = context
            .memory(|memory| memory.focused())
            .expect("fx DragValue should be focused for text input");

        render_controls_frame(
            &context,
            &mut workspace,
            0.3,
            vec![egui::Event::Text("1".to_owned())],
        );
        assert_eq!(context.memory(|memory| memory.focused()), Some(focused_id));
        assert_eq!(workspace.active_job, None);
        assert_eq!(workspace.fx, 812.0);

        render_controls_frame(
            &context,
            &mut workspace,
            0.4,
            vec![egui::Event::Text("2".to_owned())],
        );
        assert_eq!(context.memory(|memory| memory.focused()), Some(focused_id));
        assert_eq!(workspace.active_job, None);
        assert_eq!(workspace.fx, 812.0);
    }

    #[test]
    fn intrinsics_edits_during_pnp_refresh_queue_followup_worker() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        workspace.auto_intrinsics = false;
        workspace.active_job = Some(CalibrationJobKind::DatasetPnpRefresh);
        install_detection_outcome(&mut workspace, "queued.png", found_detection(640, 480));
        let item_id = workspace.session.items()[0].id;

        workspace.request_dataset_pnp_refresh();

        assert!(workspace.pending_dataset_pnp_refresh);
        assert_eq!(
            workspace.active_job,
            Some(CalibrationJobKind::DatasetPnpRefresh)
        );
        workspace.active_job = None;
        workspace.drain_pending_dataset_pnp_refresh();
        assert!(!workspace.pending_dataset_pnp_refresh);
        assert_eq!(
            workspace.active_job,
            Some(CalibrationJobKind::DatasetPnpRefresh)
        );

        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while workspace.active_job.is_some() && Instant::now() < deadline {
            workspace.tick(&context);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(workspace.active_job, None, "{}", workspace.status);
        assert!(
            workspace
                .session
                .items()
                .iter()
                .find(|item| item.id == item_id)
                .and_then(|item| item.pnp_observation.as_ref())
                .is_some(),
            "{}",
            workspace.status
        );
    }

    #[test]
    fn automatic_intrinsics_use_first_enabled_found_view() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        install_detection_outcome(
            &mut workspace,
            "not-found.png",
            camera_toolbox_core::ChessboardDetectionOutcome::NotFound {
                image_size: CalibrationImageSize {
                    width: 1920,
                    height: 1080,
                },
            },
        );
        install_detection_outcome(&mut workspace, "found.png", found_detection(640, 480));

        workspace.refresh_auto_intrinsics_fields();
        assert_eq!((workspace.fx, workspace.fy), (900.0, 900.0));
        assert_eq!((workspace.cx, workspace.cy), (320.0, 240.0));
        let initial = workspace.initial_intrinsics().unwrap();
        assert_eq!(
            initial.camera_matrix,
            [900.0, 0.0, 320.0, 0.0, 900.0, 240.0, 0.0, 0.0, 1.0]
        );
    }

    #[test]
    fn initial_intrinsics_and_dataset_pnp_use_editable_d12() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        workspace.auto_intrinsics = false;
        workspace.fx = 810.0;
        workspace.fy = 820.0;
        workspace.cx = 320.0;
        workspace.cy = 240.0;
        workspace.initial_distortion_coefficients = [
            0.11, -0.12, 0.001, -0.002, 0.03, 0.04, -0.05, 0.06, 0.0007, -0.0008, 0.0009, -0.001,
        ];

        let initial = workspace.initial_intrinsics().unwrap();
        assert_eq!(
            initial.distortion_coefficients,
            workspace.initial_distortion_coefficients.to_vec()
        );
        let image_size = CalibrationImageSize::new(640, 480).unwrap();
        let binding = workspace.dataset_pnp_binding(image_size).unwrap();
        assert_eq!(
            binding.initial_intrinsics.distortion_coefficients,
            workspace.initial_distortion_coefficients.to_vec()
        );
        let request = workspace
            .dataset_pose_request_for_image(image_size)
            .unwrap();
        assert_eq!(
            request.initial_intrinsics.distortion_coefficients,
            workspace.initial_distortion_coefficients.to_vec()
        );

        workspace.auto_intrinsics = true;
        let auto_initial = workspace.initial_intrinsics_for_image(image_size).unwrap();
        assert_eq!(
            auto_initial.distortion_coefficients,
            ZERO_DISTORTION_COEFFICIENTS.to_vec()
        );
        workspace.refresh_auto_intrinsics_fields();
        assert_eq!(
            workspace.initial_distortion_coefficients,
            ZERO_DISTORTION_COEFFICIENTS
        );
    }

    #[test]
    fn use_result_as_initial_intrinsics_copies_k_and_d12() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        for index in 0..3 {
            install_detection_outcome(
                &mut workspace,
                &format!("copy-{index}.png"),
                found_detection(640, 480),
            );
        }
        let snapshot = workspace
            .session
            .calibration_snapshot(workspace.initial_intrinsics().unwrap())
            .unwrap();
        let views = snapshot
            .request
            .image_points
            .iter()
            .map(|points| camera_toolbox_core::ViewCalibrationResult {
                rotation_vector: [0.0; 3],
                translation_vector: [0.0, 0.0, 1.0],
                projected_points: points.clone(),
                reprojection_rmse: 0.1,
                max_reprojection_error: 0.2,
            })
            .collect();
        let distortion = vec![
            0.1, -0.2, 0.001, -0.002, 0.03, 0.01, -0.01, 0.005, 0.0001, -0.0001, 0.0002, -0.0002,
        ];
        let solution = CalibrationSolution {
            image_size: snapshot.request.image_size,
            camera_matrix: [620.0, 0.0, 318.0, 0.0, 621.0, 241.0, 0.0, 0.0, 1.0],
            distortion_coefficients: distortion.clone(),
            rms_error: 0.15,
            calibration_flags: camera_toolbox_core::PANGBOT_CALIBRATION_FLAGS,
            views,
        };
        workspace
            .session
            .install_solution(snapshot, solution)
            .unwrap();

        workspace.use_installed_result_as_initial_intrinsics();

        assert!(!workspace.auto_intrinsics);
        assert_eq!((workspace.fx, workspace.fy), (620.0, 621.0));
        assert_eq!((workspace.cx, workspace.cy), (318.0, 241.0));
        assert_eq!(
            workspace.initial_distortion_coefficients,
            distortion_coefficients_to_d12(&distortion)
        );
        assert_eq!(
            workspace
                .initial_intrinsics()
                .unwrap()
                .distortion_coefficients,
            distortion
        );
    }

    #[test]
    fn coverage_heatmap_uses_enabled_found_items_only() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        install_detection_outcome(
            &mut workspace,
            "not-found.png",
            camera_toolbox_core::ChessboardDetectionOutcome::NotFound {
                image_size: CalibrationImageSize {
                    width: 640,
                    height: 480,
                },
            },
        );
        let found_id =
            install_detection_outcome(&mut workspace, "found.png", found_detection(640, 480));

        let coverage = build_coverage_image(workspace.session.items()).unwrap();
        assert_eq!(coverage.enabled_views, 1);
        assert_eq!(coverage.density.size, [COVERAGE_WIDTH, 144]);
        workspace.session.set_enabled(found_id, false).unwrap();
        assert!(build_coverage_image(workspace.session.items()).is_none());
    }

    #[test]
    fn preview_modes_select_expected_layers() {
        assert_eq!(
            preview_layers(CalibrationPreviewMode::Heatmap, true),
            PreviewLayers {
                input: false,
                heatmap_alpha: Some(255),
            }
        );
        assert_eq!(
            preview_layers(CalibrationPreviewMode::Overlay, true),
            PreviewLayers {
                input: true,
                heatmap_alpha: Some(150),
            }
        );
        assert_eq!(
            preview_layers(CalibrationPreviewMode::InputImage, true),
            PreviewLayers {
                input: true,
                heatmap_alpha: None,
            }
        );
        assert_eq!(
            preview_layers(CalibrationPreviewMode::Heatmap, false),
            PreviewLayers {
                input: true,
                heatmap_alpha: None,
            }
        );
    }

    #[test]
    fn pose_axis_projection_uses_board_center_origin() {
        let image_size = CalibrationImageSize::new(640, 480).unwrap();
        let initial = InitialIntrinsics {
            camera_matrix: [500.0, 0.0, 320.0, 0.0, 500.0, 240.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 12],
        };
        let binding =
            InitialIntrinsicsBinding::dataset_full_frame(initial.clone(), image_size).unwrap();
        let board = BoardSpec::new(11, 8, 40.0).unwrap();
        let observation = PnPObservation::from_view_result(
            binding.digest,
            ViewCalibrationResult {
                rotation_vector: [0.0, 0.0, 0.0],
                translation_vector: [0.0, 0.0, 1000.0],
                projected_points: Vec::new(),
                reprojection_rmse: 0.1,
                max_reprojection_error: 0.2,
            },
            board,
        )
        .unwrap();
        let projection = pose_axis_projection(
            &observation,
            &initial,
            board,
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 480.0)),
            image_size.width,
            image_size.height,
            false,
        )
        .unwrap();

        assert!((projection.origin.x - 420.5).abs() < 1.0e-4);
        assert!((projection.origin.y - 310.5).abs() < 1.0e-4);
        assert!(projection.x_axis.x > projection.origin.x);
        assert!((projection.x_axis.y - projection.origin.y).abs() < 1.0e-4);
        assert!(projection.y_axis.y > projection.origin.y);
        assert!((projection.y_axis.x - projection.origin.x).abs() < 1.0e-4);
        assert!(projection.z_axis.x < projection.origin.x);
        assert!(projection.z_axis.y < projection.origin.y);
    }

    #[test]
    fn current_gui_projection_points_apply_d12_distortion() {
        let image_size = CalibrationImageSize::new(640, 480).unwrap();
        let board = BoardSpec::new(11, 8, 40.0).unwrap();
        let mut distorted = InitialIntrinsics {
            camera_matrix: [500.0, 0.0, 320.0, 0.0, 500.0, 240.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 12],
        };
        let zero = distorted.clone();
        let binding =
            InitialIntrinsicsBinding::dataset_full_frame(zero.clone(), image_size).unwrap();
        let observation = PnPObservation::from_view_result(
            binding.digest,
            ViewCalibrationResult {
                rotation_vector: [0.0, 0.0, 0.0],
                translation_vector: [0.0, 0.0, 1000.0],
                projected_points: Vec::new(),
                reprojection_rmse: 0.1,
                max_reprojection_error: 0.2,
            },
            board,
        )
        .unwrap();
        distorted.distortion_coefficients[0] = 0.5;
        let image_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 480.0));

        let zero_points = projected_board_corners_for_preview(
            &observation,
            &zero,
            board,
            image_rect,
            image_size.width,
            image_size.height,
            false,
        )
        .unwrap();
        let distorted_points = projected_board_corners_for_preview(
            &observation,
            &distorted,
            board,
            image_rect,
            image_size.width,
            image_size.height,
            false,
        )
        .unwrap();
        let zero_last = zero_points.last().and_then(|point| *point).unwrap();
        let distorted_last = distorted_points.last().and_then(|point| *point).unwrap();

        assert!(distorted_last.x > zero_last.x);
        assert!(distorted_last.y > zero_last.y);
    }

    #[test]
    fn preview_zoom_keeps_anchor_image_position_stable() {
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 200.0));
        let image_size = egui::vec2(400.0, 200.0);
        let anchor = egui::pos2(150.0, 100.0);
        let mut state = CalibrationPreviewViewport::default();
        state.fit_to_rect(viewport, image_size);
        let before = (anchor - viewport.min - state.pan) / state.zoom;
        state.zoom_by(2.0, anchor, viewport);
        let after = (anchor - viewport.min - state.pan) / state.zoom;
        assert!((before - after).length() < 1e-4);
    }

    #[test]
    fn preview_mapping_aligns_opencv_coordinates_with_texel_centers_at_64x() {
        let scale = 64.0;
        let image_rect = egui::Rect::from_min_size(
            egui::pos2(13.0, 17.0),
            egui::vec2(640.0 * scale, 480.0 * scale),
        );

        assert_eq!(
            image_point_to_preview(CalibrationPoint::new(0.0, 0.0), image_rect, 640, 480, false),
            image_rect.min + egui::vec2(0.5 * scale, 0.5 * scale)
        );
        assert_eq!(
            image_point_to_preview(
                CalibrationPoint::new(119.5, 99.5),
                image_rect,
                640,
                480,
                false,
            ),
            image_rect.min + egui::vec2(120.0 * scale, 100.0 * scale)
        );
        assert_eq!(
            image_point_to_preview(
                CalibrationPoint::new(639.0, 479.0),
                image_rect,
                640,
                480,
                false,
            ),
            image_rect.max - egui::vec2(0.5 * scale, 0.5 * scale)
        );
        assert_eq!(
            image_point_to_preview(CalibrationPoint::new(0.0, 0.0), image_rect, 640, 480, true),
            egui::pos2(
                image_rect.right() - 0.5 * scale,
                image_rect.top() + 0.5 * scale
            )
        );
    }

    #[test]
    fn square_size_apply_preserves_detections_and_inner_corner_change_invalidates_them() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        assert_eq!(workspace.session.board().square_size, 40.0);
        install_detection_outcome(&mut workspace, "view.png", found_detection(640, 480));

        workspace.square_size = 45.0;
        assert!(workspace.apply_board());
        assert!(matches!(
            workspace.session.items()[0].status,
            CalibrationItemStatus::Found(_)
        ));

        assert!(workspace.status.contains("detections were preserved"));

        workspace.board_cols = 12;
        assert!(workspace.apply_board());
        assert!(matches!(
            workspace.session.items()[0].status,
            CalibrationItemStatus::Pending
        ));
        assert!(workspace.status.contains("detections were invalidated"));
    }

    #[test]
    fn rmse_text_colors_meet_contrast_on_both_progress_regions() {
        fn relative_luminance(color: egui::Color32) -> f32 {
            let linear = |channel: u8| {
                let value = f32::from(channel) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
        }

        fn contrast_ratio(first: egui::Color32, second: egui::Color32) -> f32 {
            let first = relative_luminance(first);
            let second = relative_luminance(second);
            (first.max(second) + 0.05) / (first.min(second) + 0.05)
        }

        assert!(contrast_ratio(RMSE_TEXT_ON_FILL, egui::Color32::LIGHT_BLUE) >= 4.5);
        assert!(contrast_ratio(RMSE_TEXT_ON_TRACK, egui::Color32::DARK_GRAY) >= 4.5);
    }

    #[test]
    fn eeprom_snid_editor_renders_converted_preview() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        workspace.snid_draft = CalibrationSnidDraft {
            module: YgStereoModuleCode::Model235,
            year: "26".to_owned(),
            month: "1".to_owned(),
            day: "9".to_owned(),
            optical_axis_class: 0,
            sequence: "1".to_owned(),
        };
        let snid = workspace.snid_draft.serial_number().unwrap();

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 480.0),
            )),
            ..Default::default()
        };
        let output = context.run_ui(input, |ui| {
            workspace.render_eeprom_snid_editor(ui, Ok(&snid));
        });
        let text = output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .into_iter()
            .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
            .collect::<Vec<_>>()
            .join("\n");

        for expected in [
            "YgStereo SNID",
            "Fixed: resolution=2/FHD, vendor=T/SmartSens, algorithm=0, reserved=0",
            "Converted SNID",
            "2T235261900000",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text}");
        }
    }

    #[test]
    fn eeprom_snid_year_requires_two_decimal_digits() {
        assert_eq!(parse_two_digit_year("26").unwrap(), 26);
        assert!(
            parse_two_digit_year("6")
                .unwrap_err()
                .contains("two decimal digits")
        );
        assert!(
            parse_two_digit_year("2026")
                .unwrap_err()
                .contains("two decimal digits")
        );
        assert!(
            parse_two_digit_year("2A")
                .unwrap_err()
                .contains("two decimal digits")
        );
    }

    #[test]
    fn installed_solution_renders_intrinsics_distortion_and_inline_rmse() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        for index in 0..3 {
            install_detection_outcome(
                &mut workspace,
                &format!("view-{index}.png"),
                found_detection(640, 480),
            );
        }
        let initial = workspace.initial_intrinsics().unwrap();
        let snapshot = workspace.session.calibration_snapshot(initial).unwrap();
        let views = snapshot
            .request
            .image_points
            .iter()
            .enumerate()
            .map(
                |(index, points)| camera_toolbox_core::ViewCalibrationResult {
                    rotation_vector: [0.0; 3],
                    translation_vector: [0.0, 0.0, 1.0],
                    projected_points: points.clone(),
                    reprojection_rmse: 0.1 + index as f64 * 0.05,
                    max_reprojection_error: 0.2 + index as f64 * 0.05,
                },
            )
            .collect();
        let solution = CalibrationSolution {
            image_size: snapshot.request.image_size,
            camera_matrix: [620.0, 0.0, 318.0, 0.0, 621.0, 241.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![
                0.1, -0.2, 0.001, -0.002, 0.03, 0.01, -0.01, 0.005, 0.0001, -0.0001, 0.0002,
                -0.0002,
            ],
            rms_error: 0.15,
            calibration_flags: camera_toolbox_core::PANGBOT_CALIBRATION_FLAGS,
            views,
        };
        workspace
            .session
            .install_solution(snapshot, solution)
            .unwrap();
        let installed = workspace.session.installed().unwrap();
        let second_view = calibration_view(Some(installed), installed.item_ids[1]).unwrap();
        assert!((second_view.reprojection_rmse - 0.15).abs() < 1e-12);

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        let output = context.run_ui(input, |ui| {
            workspace.render(
                &context,
                ui,
                true,
                None,
                Err("SFTP not connected"),
                Err("EEPROM not configured"),
                false,
                |_| None,
            );
        });
        let text = output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .into_iter()
            .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "RMSE",
            "Heatmap",
            "Overlay",
            "Input image",
            "Calibration result",
            "fx",
            "620.00000000",
            "Distortion coefficients (OpenCV order)",
            "k1[0] = 0.1000000000",
            "Square size (mm)",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text}");
        }
        assert!(!text.contains("Reprojection RMSE"));
        assert!(!text.contains("Wheel: zoom"));
        assert!(!text.contains("Remove selected"));

        let toggled_item_id = workspace.session.latest_installed().unwrap().item_ids[0];
        workspace
            .session
            .set_enabled(toggled_item_id, false)
            .unwrap();
        assert!(workspace.session.installed().is_none());
        assert!(calibration_view(workspace.session.latest_installed(), toggled_item_id).is_some());
        let stale_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        let stale_output = context.run_ui(stale_input, |ui| {
            workspace.render(
                &context,
                ui,
                true,
                None,
                Err("SFTP not connected"),
                Err("EEPROM not configured"),
                false,
                |_| None,
            );
        });
        let stale_text = stale_output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .into_iter()
            .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            STALE_CALIBRATION_RESULT_REASON,
            "620.00000000",
            "k1[0] = 0.1000000000",
        ] {
            assert!(
                stale_text.contains(expected),
                "missing retained stale result field {expected:?} in {stale_text}"
            );
        }
    }

    #[test]
    fn status_labels_distinguish_queued_and_active_detection() {
        assert_eq!(
            status_label(&CalibrationItemStatus::DetectQueued),
            "Detect queued"
        );
        assert_eq!(status_label(&CalibrationItemStatus::Detecting), "Detecting");
    }

    #[test]
    fn status_colors_distinguish_unprocessed_success_and_failure() {
        let camera_toolbox_core::ChessboardDetectionOutcome::Found(detection) =
            found_detection(640, 480)
        else {
            unreachable!();
        };
        assert_eq!(status_color(&CalibrationItemStatus::Pending), None);
        assert_eq!(status_color(&CalibrationItemStatus::Reading), None);
        assert_eq!(status_color(&CalibrationItemStatus::DetectQueued), None);
        assert_eq!(
            status_color(&CalibrationItemStatus::Found(detection)),
            Some(OBSERVED_POINT_COLOR)
        );
        assert_eq!(
            status_color(&CalibrationItemStatus::NotFound {
                image_size: CalibrationImageSize {
                    width: 640,
                    height: 480,
                },
            }),
            Some(REPROJECTED_POINT_COLOR)
        );
        assert_eq!(
            status_color(&CalibrationItemStatus::Failed("decode failed".to_owned())),
            Some(REPROJECTED_POINT_COLOR)
        );
    }

    #[test]
    fn runtime_admission_snapshots_900px_intrinsics_for_live_geometry() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let displayed = live_frame(1);
        let source = test_live_source();
        let key = source.acquisition_key_for_frame(&displayed).unwrap();

        workspace.observe_live_frame(Arc::clone(&displayed), source, store, false);

        let binding = workspace.session.initial_intrinsics_binding().unwrap();
        assert_eq!(binding.initial_intrinsics.camera_matrix[0], 900.0);
        assert_eq!(binding.initial_intrinsics.camera_matrix[4], 900.0);
        assert_eq!(binding.initial_intrinsics.camera_matrix[2], 320.0);
        assert_eq!(binding.initial_intrinsics.camera_matrix[5], 240.0);
        assert_eq!(
            binding.initial_intrinsics.distortion_coefficients,
            vec![0.0; 12]
        );
        assert_eq!(binding.acquisition_key, key);
        assert_eq!(
            workspace
                .session
                .auto_capture_baseline()
                .map(|baseline| &baseline.acquisition_key),
            Some(&key)
        );
    }

    #[test]
    fn source_or_geometry_change_rebuilds_runtime_auto_admission() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let displayed = live_frame(1);
        let source = test_live_source();
        let key = source.acquisition_key_for_frame(&displayed).unwrap();

        workspace.observe_live_frame(Arc::clone(&displayed), source, store.clone(), false);
        assert_eq!(
            workspace
                .session
                .auto_capture_baseline()
                .map(|baseline| &baseline.acquisition_key),
            Some(&key)
        );

        let mismatched_source = LiveStreamSource::Rtsp {
            label: "Other".to_owned(),
            channel: 0,
            transport: camera_toolbox_app::RtspTransport::Tcp,
            source_fingerprint: "other-rtsp-source".to_owned(),
            geometry_key: "other-rtsp-config".to_owned(),
            authoritative_capture: None,
        };
        let mismatched_key = mismatched_source
            .acquisition_key_for_frame(&live_frame(2))
            .unwrap();
        workspace.observe_live_frame(live_frame(2), mismatched_source, store, false);

        assert_eq!(
            workspace
                .session
                .auto_capture_baseline()
                .map(|baseline| &baseline.acquisition_key),
            Some(&mismatched_key)
        );
        assert_eq!(
            workspace
                .session
                .initial_intrinsics_binding()
                .map(|binding| &binding.acquisition_key),
            Some(&mismatched_key)
        );
        assert_ne!(mismatched_key, key);
    }

    fn test_live_source() -> LiveStreamSource {
        LiveStreamSource::Rtsp {
            label: "Test".to_owned(),
            channel: 0,
            transport: camera_toolbox_app::RtspTransport::Tcp,
            source_fingerprint: "test-rtsp-source".to_owned(),
            geometry_key: "test-rtsp-config".to_owned(),
            authoritative_capture: None,
        }
    }

    fn mismatched_live_source() -> LiveStreamSource {
        LiveStreamSource::Rtsp {
            label: "Other".to_owned(),
            channel: 0,
            transport: camera_toolbox_app::RtspTransport::Tcp,
            source_fingerprint: "other-rtsp-source".to_owned(),
            geometry_key: "other-rtsp-config".to_owned(),
            authoritative_capture: None,
        }
    }

    fn auto_capture_store() -> CaptureStore {
        CaptureStore::new(CaptureStoreLimits::new(4 * 1024 * 1024, 8 * 1024 * 1024).unwrap())
    }

    fn live_frame(sequence: u64) -> Arc<DecodedVideoFrame> {
        Arc::new(DecodedVideoFrame {
            width: 640,
            height: 480,
            rgba: Arc::from(vec![127_u8; 640 * 480 * 4]),
            identity: StreamFrameIdentity::unavailable(
                StreamSessionId::new("auto-capture-workspace-test").unwrap(),
                0,
                sequence,
                "test fixture",
            ),
        })
    }

    #[test]
    fn manager_observe_unknown_live_source_does_not_create_workspace() {
        let context = egui::Context::default();
        let mut manager = CalibrationWorkspaceManager::new(&context).unwrap();

        manager.observe_live_frame(
            live_frame(1),
            test_live_source(),
            auto_capture_store(),
            true,
        );

        assert_eq!(manager.workspace_count_for_test(), 1);
        assert_eq!(manager.active_label_for_test(), "Manual / Files");
    }

    #[test]
    fn manager_explicit_stream_capture_creates_live_workspace() {
        let context = egui::Context::default();
        let mut manager = CalibrationWorkspaceManager::new(&context).unwrap();

        manager.capture_displayed_stream_frame(
            live_frame(1),
            test_live_source(),
            auto_capture_store(),
        );

        assert_eq!(manager.workspace_count_for_test(), 2);
        assert_eq!(manager.active_label_for_test(), "Test CH0");
    }

    #[test]
    fn manager_can_close_live_session_without_removing_manual_workspace() {
        let context = egui::Context::default();
        let mut manager = CalibrationWorkspaceManager::new(&context).unwrap();
        let source = test_live_source();
        let live_key = CalibrationWorkspaceKey::for_live_source(&source);

        manager.ensure_live_source_for_test(&source);
        assert_eq!(manager.workspace_count_for_test(), 2);
        assert_eq!(manager.active_label_for_test(), "Test CH0");

        assert!(manager.close_session(&live_key));

        assert_eq!(manager.workspace_count_for_test(), 1);
        assert_eq!(manager.active_label_for_test(), "Manual / Files");
        assert!(!manager.active_accepts_live_source(Some(&source)));
        assert!(!manager.close_session(&CalibrationWorkspaceKey::manual()));
    }

    #[test]
    fn manager_manual_workspace_rejects_live_inspection_context() {
        let context = egui::Context::default();
        let mut manager = CalibrationWorkspaceManager::new(&context).unwrap();
        let source = test_live_source();

        assert!(!manager.active_accepts_live_source(Some(&source)));
        manager.ensure_live_source_for_test(&source);
        assert!(manager.active_accepts_live_source(Some(&source)));
        manager.import(Vec::new());

        assert_eq!(manager.active_label_for_test(), "Manual / Files");
        assert!(!manager.active_accepts_live_source(Some(&source)));
    }

    fn chessboard_live_frame(sequence: u64) -> Arc<DecodedVideoFrame> {
        let rgba = image::load_from_memory(include_bytes!(
            "../../../adapters/tests/data/chessboard_11x8_clean.png"
        ))
        .unwrap()
        .to_rgba8();
        Arc::new(DecodedVideoFrame {
            width: rgba.width(),
            height: rgba.height(),
            rgba: Arc::from(rgba.into_raw()),
            identity: StreamFrameIdentity::unavailable(
                StreamSessionId::new("board-preview-workspace-test").unwrap(),
                0,
                sequence,
                "test fixture",
            ),
        })
    }

    fn chessboard_live_frame_for_session(
        session_id: &str,
        sequence: u64,
    ) -> Arc<DecodedVideoFrame> {
        let rgba = image::load_from_memory(include_bytes!(
            "../../../adapters/tests/data/chessboard_11x8_clean.png"
        ))
        .unwrap()
        .to_rgba8();
        Arc::new(DecodedVideoFrame {
            width: rgba.width(),
            height: rgba.height(),
            rgba: Arc::from(rgba.into_raw()),
            identity: StreamFrameIdentity::unavailable(
                StreamSessionId::new(session_id).unwrap(),
                0,
                sequence,
                "test fixture",
            ),
        })
    }

    fn found_chessboard_detection(width: u32, height: u32) -> ChessboardDetection {
        let camera_toolbox_core::ChessboardDetectionOutcome::Found(detection) =
            found_detection(width, height)
        else {
            unreachable!();
        };
        detection
    }

    fn install_stream_found_item(
        context: &egui::Context,
        workspace: &mut CalibrationWorkspace,
        frame: &DecodedVideoFrame,
        source: &LiveStreamSource,
        label: &str,
    ) -> (
        CalibrationItemId,
        AutoCaptureAcquisitionKey,
        ChessboardDetection,
    ) {
        let acquisition_key = source.acquisition_key_for_frame(frame).unwrap();
        let revision = CalibrationInputRevision::EphemeralPng {
            content_sha256: format!("{:064x}", frame.identity.frame_sequence),
            encoded_bytes: 128,
        };
        let outcome = workspace.session.add_or_refresh_with_acquisition_key(
            CalibrationInputKey::StreamCapture(StreamCaptureId::from(&frame.identity)),
            revision.clone(),
            label.to_owned(),
            Some(acquisition_key.clone()),
        );
        let id = match outcome {
            AddCalibrationItemOutcome::Added(id) | AddCalibrationItemOutcome::SourceChanged(id) => {
                id
            }
            AddCalibrationItemOutcome::AlreadyPresent(id) => id,
        };
        let token = workspace.session.begin_encoded_detection(id).unwrap();
        workspace.session.mark_detecting(&token).unwrap();
        let detection = found_chessboard_detection(frame.width, frame.height);
        workspace
            .session
            .install_detection(
                &token,
                revision,
                camera_toolbox_core::ChessboardDetectionOutcome::Found(detection.clone()),
            )
            .unwrap();
        let image_size = CalibrationImageSize::new(frame.width, frame.height).unwrap();
        workspace.sources.insert(
            id,
            CalibrationSource {
                display_name: label.to_owned(),
                kind: CalibrationSourceKind::Stream(StreamCalibrationSource {
                    store: auto_capture_store(),
                    asset: None,
                    analysis_asset: None,
                    identity: frame.identity.clone(),
                    image_size,
                    acquisition_key: acquisition_key.clone(),
                    authoritative_capture: None,
                }),
                preview: None,
            },
        );
        let stride = usize::try_from(frame.width).unwrap() * 4;
        let preview = Arc::new(
            Rgba8Frame::new(frame.width, frame.height, stride, Arc::clone(&frame.rgba)).unwrap(),
        );
        workspace.install_preview(context, id, preview);
        (id, acquisition_key, detection)
    }

    #[test]
    fn board_preview_detects_corners_by_default_without_mutating_dataset() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let baseline = store.stats().unwrap();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        assert!(!workspace.auto_capture_enabled);
        let displayed = chessboard_live_frame(1);
        workspace.observe_live_frame(
            Arc::clone(&displayed),
            test_live_source(),
            store.clone(),
            false,
        );
        assert!(workspace.auto_capture.pending.is_empty());
        assert_eq!(store.stats().unwrap(), baseline);

        workspace.observe_live_frame(
            Arc::clone(&displayed),
            test_live_source(),
            store.clone(),
            true,
        );

        assert_eq!(
            workspace.auto_capture.pending.front().unwrap().intent,
            CandidateIntent::PreviewOnly
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !workspace.auto_capture.pending.is_empty() && Instant::now() < deadline {
            workspace.tick(&context);
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            workspace.auto_capture.pending.is_empty(),
            "preview worker did not finish: {}",
            workspace.status
        );
        assert!(workspace.session.items().is_empty());
        assert!(workspace.sources.is_empty());
        assert_eq!(store.stats().unwrap(), baseline);
        let overlay = workspace.viewer_overlay(&displayed, &test_live_source());
        assert!(overlay.persistent.is_none());
        assert!(workspace.auto_capture.latest_detection.is_some());
        assert!(
            !workspace
                .live_field_cells(&workspace.acceptance_last_valid_criteria)
                .is_empty()
        );
        let mismatched_overlay = workspace.viewer_overlay(&displayed, &mismatched_live_source());
        assert!(mismatched_overlay.persistent.is_none());
    }

    #[test]
    fn live_preview_does_not_hold_main_viewer_while_async_detection_completes() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let source = test_live_source();
        let candidate_frame = chessboard_live_frame(1);
        let newer_frame = chessboard_live_frame(2);

        workspace.observe_live_frame(
            Arc::clone(&candidate_frame),
            source.clone(),
            store.clone(),
            true,
        );

        let latest_slot = Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default());
        latest_slot.publish((*candidate_frame).clone());
        let mut document = crate::workspace::LiveDocument::new(
            crate::workspace::DocumentId::from_raw(10_001),
            candidate_frame.identity.stream_id.clone(),
            Arc::clone(&latest_slot),
            source.clone(),
        );
        document.install_latest_texture(&context);
        latest_slot.publish((*newer_frame).clone());
        document.install_latest_texture(&context);
        assert_eq!(
            document.displayed_frame().unwrap().identity,
            newer_frame.identity
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        while !workspace.auto_capture.pending.is_empty() && Instant::now() < deadline {
            workspace.tick(&context);
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            workspace.auto_capture.pending.is_empty(),
            "preview worker did not finish: {}",
            workspace.status
        );
        workspace.observe_live_frame(Arc::clone(&newer_frame), source.clone(), store, false);
        assert!(
            workspace
                .auto_capture
                .latest_detection
                .as_ref()
                .is_some_and(|latest| { latest.identity == candidate_frame.identity })
        );
        assert!(
            workspace
                .viewer_overlay(&newer_frame, &source)
                .persistent
                .is_none()
        );
        assert!(
            workspace
                .viewer_overlay(&candidate_frame, &source)
                .persistent
                .is_none()
        );
        assert!(workspace.live_acceptance_marker_observation().is_some());
        assert!(
            !workspace
                .live_field_cells(&workspace.acceptance_last_valid_criteria)
                .is_empty()
        );
    }

    #[test]
    fn shutter_capture_round_trip_detects_stream_frame_into_dataset() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let displayed = chessboard_live_frame(7);
        assert!(
            workspace.session.initial_intrinsics_binding().is_none(),
            "manual shutter Dataset PnP must not require a live source-bound binding"
        );

        workspace.capture_displayed_stream_frame(
            Arc::clone(&displayed),
            test_live_source(),
            store.clone(),
        );

        assert_eq!(workspace.session.items().len(), 1);
        let item_id = workspace.session.items()[0].id;
        assert_eq!(
            workspace.session.items()[0].input,
            CalibrationInputKey::StreamCapture(StreamCaptureId::from(&displayed.identity))
        );
        let CalibrationSourceKind::Stream(stream) = &workspace.sources[&item_id].kind else {
            panic!("shutter capture must retain a stream source");
        };
        assert_eq!(stream.identity, displayed.identity);
        let asset_id = stream
            .asset
            .as_ref()
            .expect("stream source must retain its captured asset")
            .id
            .clone();
        assert!(store.get(&asset_id).unwrap().is_some());

        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !matches!(
            workspace.session.items()[0].status,
            CalibrationItemStatus::Found(_)
        ) && Instant::now() < deadline
        {
            workspace.tick(&context);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let item = &workspace.session.items()[0];
        let CalibrationItemStatus::Found(detection) = &item.status else {
            panic!(
                "shutter capture detection did not finish as Found: {:?}",
                item.status
            );
        };
        assert_eq!(detection.corners.len(), 88);
        let pnp_binding = workspace
            .dataset_pnp_binding(detection.image_size)
            .expect("current GUI K must create a source-independent Dataset PnP binding")
            .digest;
        assert!(
            item.pnp_observation
                .as_ref()
                .is_some_and(|observation| observation.binding_digest == pnp_binding)
        );
        let CalibrationSourceKind::Stream(stream) = &workspace.sources[&item_id].kind else {
            panic!("detected stream item must retain its stream source");
        };
        assert_eq!(stream.identity, displayed.identity);
        assert_eq!(stream.asset.as_ref().unwrap().id, asset_id);
        assert!(store.get(&asset_id).unwrap().is_some());
    }

    #[test]
    fn viewer_overlay_keeps_same_acquisition_group_across_stream_reconnect() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let captured = chessboard_live_frame_for_session("overlay-source-session-a", 1);

        workspace.capture_displayed_stream_frame(
            Arc::clone(&captured),
            test_live_source(),
            store.clone(),
        );

        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !matches!(
            workspace.session.items()[0].status,
            CalibrationItemStatus::Found(_)
        ) && Instant::now() < deadline
        {
            workspace.tick(&context);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let reconnected = chessboard_live_frame_for_session("overlay-source-session-b", 2);
        let overlay = workspace.viewer_overlay(&reconnected, &test_live_source());

        assert_eq!(overlay.persistent.unwrap().corners.len(), 88);
        let mismatched_source = LiveStreamSource::Rtsp {
            label: "Other".to_owned(),
            channel: 0,
            transport: camera_toolbox_app::RtspTransport::Tcp,
            source_fingerprint: "other-rtsp-source".to_owned(),
            geometry_key: "other-rtsp-config".to_owned(),
            authoritative_capture: None,
        };
        let mismatched_overlay = workspace.viewer_overlay(&reconnected, &mismatched_source);
        assert!(mismatched_overlay.persistent.is_none());
    }

    #[test]
    fn live_field_cells_use_fresh_detection_without_pnp_binding() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let source = test_live_source();
        let displayed = live_frame(40);
        let acquisition_key = source.acquisition_key_for_frame(&displayed).unwrap();
        workspace.observe_live_frame(Arc::clone(&displayed), source.clone(), store, false);

        let mut criteria = workspace.acceptance_last_valid_criteria.clone();
        criteria.field_columns = 2;
        criteria.field_rows = 2;
        workspace.auto_intrinsics = false;
        workspace.fx = f64::NAN;
        workspace.refresh_runtime_auto_admission();
        assert!(workspace.acceptance_draft.error.is_some());
        workspace.auto_capture.latest_detection = Some(IdentityBoundDetection {
            identity: displayed.identity.clone(),
            acquisition_key,
            detection: found_chessboard_detection(displayed.width, displayed.height),
            pnp_observation: None,
            completed_at_ns: host_monotonic_time_ns(),
        });

        assert!(workspace.live_acceptance_marker_observation().is_none());
        assert_eq!(workspace.live_field_cells(&criteria), vec![0, 1, 2, 3]);
        let fresh_ns = host_monotonic_time_ns();
        assert_eq!(
            workspace.live_field_cells_at(&criteria, fresh_ns),
            vec![0, 1, 2, 3]
        );
        assert!(
            workspace
                .live_field_cells_at(&criteria, fresh_ns + LIVE_DETECTION_MARKER_TTL_NS + 1)
                .is_empty()
        );
    }

    #[test]
    fn latest_dataset_overlay_is_visible_before_one_second_timeout() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let source = test_live_source();
        let displayed = live_frame(41);
        let (item_id, acquisition_key, detection) = install_stream_found_item(
            &context,
            &mut workspace,
            &displayed,
            &source,
            "fresh-stream",
        );
        workspace.session.set_selected(item_id).unwrap();
        workspace.auto_capture.last_dataset_overlay = Some(DatasetDetectionOverlay {
            item_id,
            detection,
            acquisition_key,
            pnp_observation: None,
            committed_at_ns: 1_000,
        });

        let presentation = workspace
            .live_viewer_presentation_at(Some(&displayed), Some(&source), 1_000)
            .unwrap();
        assert_eq!(presentation.item_id, Some(item_id));
        assert_eq!(presentation.overlay.persistent.unwrap().corners.len(), 88);
    }

    #[test]
    fn live_preview_publishes_realtime_pose_axis_overlay() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let source = test_live_source();
        let displayed = live_frame(43);
        workspace.observe_live_frame(Arc::clone(&displayed), source.clone(), store, false);
        let acquisition_key = source.acquisition_key_for_frame(&displayed).unwrap();
        let binding = workspace
            .session
            .initial_intrinsics_binding()
            .expect("live observation creates an intrinsics binding")
            .clone();
        let board = workspace.session.board();
        let board_center = guided_pose_inner_center_point(board);
        let detection = found_chessboard_detection(displayed.width, displayed.height);
        workspace.auto_capture.latest_detection = Some(IdentityBoundDetection {
            identity: displayed.identity.clone(),
            acquisition_key,
            detection,
            pnp_observation: Some(PnPObservation {
                binding_digest: binding.digest,
                rotation_vector: [0.0, 0.0, 0.0],
                translation_vector: [-board_center[0], -board_center[1], 1000.0],
                depth: 1000.0,
                minimum_board_depth: 1000.0,
                maximum_board_depth: 1000.0,
                tilt_degrees: 0.0,
                azimuth_degrees: 0.0,
                reprojection_rmse: 999.0,
                max_reprojection_error: 999.0,
            }),
            completed_at_ns: 1_000,
        });

        let presentation = workspace
            .live_viewer_presentation_at(Some(&displayed), Some(&source), 1_000)
            .expect("fresh realtime detection should publish a viewer overlay");
        assert!(presentation.item_id.is_none());
        assert!(presentation.overlay.persistent.is_none());
        assert!(
            presentation
                .overlay
                .realtime_detection
                .as_ref()
                .and_then(|overlay| overlay.pose_axis.as_ref())
                .is_some(),
            "realtime overlay must carry the current detection pose axes"
        );
    }

    #[test]
    fn latest_dataset_overlay_hides_after_one_second_without_clearing_selection() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let source = test_live_source();
        let displayed = live_frame(42);
        let (item_id, acquisition_key, detection) = install_stream_found_item(
            &context,
            &mut workspace,
            &displayed,
            &source,
            "stale-stream",
        );
        workspace.session.set_selected(item_id).unwrap();
        workspace.auto_capture.last_dataset_overlay = Some(DatasetDetectionOverlay {
            item_id,
            detection,
            acquisition_key,
            pnp_observation: None,
            committed_at_ns: 2_000,
        });

        assert!(
            workspace
                .live_viewer_presentation_at(
                    Some(&displayed),
                    Some(&source),
                    2_000 + LATEST_DATASET_OVERLAY_TTL_NS + 1,
                )
                .is_none()
        );
        assert_eq!(workspace.session.selected(), Some(item_id));
    }

    #[test]
    fn transient_dataset_overlay_replaces_previous_item_without_waiting_ttl() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let source = test_live_source();
        let old_frame = live_frame(45);
        let new_frame = live_frame(46);
        let (old_item, old_key, old_detection) =
            install_stream_found_item(&context, &mut workspace, &old_frame, &source, "old-slot");
        let (new_item, new_key, new_detection) =
            install_stream_found_item(&context, &mut workspace, &new_frame, &source, "new-slot");

        workspace.auto_capture.last_dataset_overlay = Some(DatasetDetectionOverlay {
            item_id: old_item,
            detection: old_detection,
            acquisition_key: old_key,
            pnp_observation: None,
            committed_at_ns: 5_000,
        });
        let old_presentation = workspace
            .live_viewer_presentation_at(Some(&old_frame), Some(&source), 5_500)
            .unwrap();
        assert_eq!(old_presentation.item_id, Some(old_item));

        workspace.auto_capture.last_dataset_overlay = Some(DatasetDetectionOverlay {
            item_id: new_item,
            detection: new_detection,
            acquisition_key: new_key,
            pnp_observation: None,
            committed_at_ns: 5_600,
        });
        let new_presentation = workspace
            .live_viewer_presentation_at(Some(&new_frame), Some(&source), 5_600)
            .unwrap();
        assert_eq!(new_presentation.item_id, Some(new_item));
        assert_eq!(
            new_presentation.overlay.persistent.unwrap().corners.len(),
            88
        );
    }
    #[test]
    fn live_preview_allows_multiple_candidates_in_flight() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let source = test_live_source();

        workspace.observe_live_frame(live_frame(1), source.clone(), store.clone(), true);
        workspace.auto_capture.last_observed_at_ns = 0;
        workspace.auto_capture.last_observed = None;
        workspace.observe_live_frame(live_frame(2), source, store, true);

        assert_eq!(workspace.auto_capture.pending.len(), 2);
        assert!(
            workspace
                .auto_capture
                .pending
                .iter()
                .all(|candidate| candidate.intent == CandidateIntent::PreviewOnly)
        );
    }

    #[test]
    fn completed_auto_candidates_finalize_in_original_fifo_order() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let source = test_live_source();

        workspace.observe_live_frame(live_frame(1), source.clone(), store.clone(), true);
        workspace.auto_capture.last_observed_at_ns = 0;
        workspace.auto_capture.last_observed = None;
        workspace.observe_live_frame(live_frame(2), source, store, true);
        let first_id = workspace.auto_capture.pending[0].token.id();
        let second_id = workspace.auto_capture.pending[1].token.id();

        workspace.complete_auto_candidate(
            None,
            second_id,
            CandidateTerminal::Discard("second complete".to_owned()),
        );
        assert_eq!(workspace.auto_capture.pending.len(), 2);
        assert_ne!(workspace.status, "second complete");

        workspace.complete_auto_candidate(
            None,
            first_id,
            CandidateTerminal::Discard("first complete".to_owned()),
        );
        assert!(workspace.auto_capture.pending.is_empty());
        assert_eq!(workspace.status, "second complete");
    }

    #[test]
    fn selected_dataset_item_switches_dataset_layer_without_blocking_latest_live_overlay() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let source = test_live_source();
        let old_frame = live_frame(43);
        let new_frame = live_frame(44);
        let (old_item, old_key, old_detection) =
            install_stream_found_item(&context, &mut workspace, &old_frame, &source, "old-stream");
        let (new_item, new_key, new_detection) =
            install_stream_found_item(&context, &mut workspace, &new_frame, &source, "new-stream");

        workspace.display_layer = CalibrationDisplayLayer::LiveStream;
        workspace.select_dataset_item_for_preview(old_item);
        workspace.auto_capture.last_dataset_overlay = Some(DatasetDetectionOverlay {
            item_id: old_item,
            detection: old_detection,
            acquisition_key: old_key,
            pnp_observation: None,
            committed_at_ns: 3_000,
        });
        let old_presentation = workspace
            .live_viewer_presentation_at(Some(&old_frame), Some(&source), 3_500)
            .unwrap();
        assert_eq!(workspace.session.selected(), Some(old_item));
        assert_eq!(
            workspace.display_layer,
            CalibrationDisplayLayer::DatasetImage
        );
        assert_eq!(old_presentation.item_id, Some(old_item));

        workspace.auto_capture.last_dataset_overlay = Some(DatasetDetectionOverlay {
            item_id: new_item,
            detection: new_detection,
            acquisition_key: new_key,
            pnp_observation: None,
            committed_at_ns: 4_000,
        });
        let new_presentation = workspace
            .live_viewer_presentation_at(Some(&new_frame), Some(&source), 4_000)
            .unwrap();
        assert_eq!(workspace.session.selected(), Some(old_item));
        assert_eq!(
            workspace.display_layer,
            CalibrationDisplayLayer::DatasetImage
        );
        assert_eq!(new_presentation.item_id, Some(new_item));
        assert_eq!(
            new_presentation.overlay.persistent.unwrap().corners.len(),
            88
        );
        assert!(
            workspace
                .live_viewer_presentation_at(
                    Some(&new_frame),
                    Some(&source),
                    4_000 + LATEST_DATASET_OVERLAY_TTL_NS + 1,
                )
                .is_none()
        );
    }

    #[test]
    fn transient_auto_capture_commits_chessboard_frame_to_dataset() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let displayed = chessboard_live_frame(3);
        workspace.acceptance_draft.field_columns = "1".to_owned();
        workspace.acceptance_draft.field_rows = "1".to_owned();
        workspace.acceptance_draft.field_target_per_cell = "1".to_owned();
        workspace.acceptance_draft.pnp_depth_min = "0.001".to_owned();
        workspace.acceptance_draft.pnp_depth_max = "10000000".to_owned();
        workspace.acceptance_draft.pnp_depth_bins = "1".to_owned();
        workspace.acceptance_draft.depth_target_per_bin = "1".to_owned();
        workspace.acceptance_draft.pnp_tilt_deadband_deg = "0".to_owned();
        workspace.acceptance_draft.pnp_tilt_max_deg = "89".to_owned();
        workspace.acceptance_draft.pnp_tilt_bins = "1".to_owned();
        workspace.acceptance_draft.pnp_azimuth_sectors = "1".to_owned();
        workspace.acceptance_draft.pose_target_per_bin = "1".to_owned();
        workspace.acceptance_draft.pnp_max_rmse_px = "100".to_owned();
        workspace.acceptance_draft.pnp_max_error_px = "100".to_owned();
        workspace.auto_capture_enabled = true;

        workspace.observe_live_frame(
            Arc::clone(&displayed),
            test_live_source(),
            store.clone(),
            true,
        );

        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !workspace.auto_capture.pending.is_empty() && Instant::now() < deadline {
            workspace.tick(&context);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            workspace.auto_capture.pending.is_empty(),
            "auto worker did not finish: {}",
            workspace.status
        );
        assert_eq!(workspace.session.items().len(), 1, "{}", workspace.status);
        let item = &workspace.session.items()[0];
        let CalibrationItemStatus::Found(detection) = &item.status else {
            panic!("auto capture did not commit Found item: {:?}", item.status);
        };
        assert_eq!(detection.corners.len(), 88);
        assert!(item.pnp_observation.is_some());
        let CalibrationSourceKind::Stream(stream) = &workspace.sources[&item.id].kind else {
            panic!("auto capture must retain stream source");
        };
        assert_eq!(stream.identity, displayed.identity);
        assert!(
            stream
                .asset
                .as_ref()
                .is_some_and(|asset| store.get(&asset.id).unwrap().is_some())
        );
        assert!(
            workspace.sources[&item.id].preview.is_some(),
            "auto capture should immediately install Dataset preview"
        );
        let assessment = workspace.auto_capture.last_assessment.as_ref().unwrap();
        assert!(assessment.field_target_met);
        assert!(assessment.depth_target_met);
        assert!(assessment.pose_target_met);
        assert!(assessment.collection_target_met);
    }

    #[test]
    fn rejected_auto_candidate_keeps_displayed_assessment_on_baseline_score() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let displayed = chessboard_live_frame(5);
        workspace.acceptance_draft.field_columns = "1".to_owned();
        workspace.acceptance_draft.field_rows = "1".to_owned();
        workspace.acceptance_draft.field_target_per_cell = "1".to_owned();
        workspace.acceptance_draft.pnp_depth_min = "0.001".to_owned();
        workspace.acceptance_draft.pnp_depth_max = "10000000".to_owned();
        workspace.acceptance_draft.pnp_depth_bins = "1".to_owned();
        workspace.acceptance_draft.depth_target_per_bin = "1".to_owned();
        workspace.acceptance_draft.pnp_tilt_deadband_deg = "0".to_owned();
        workspace.acceptance_draft.pnp_tilt_max_deg = "89".to_owned();
        workspace.acceptance_draft.pnp_tilt_bins = "1".to_owned();
        workspace.acceptance_draft.pnp_azimuth_sectors = "1".to_owned();
        workspace.acceptance_draft.pose_target_per_bin = "1".to_owned();
        workspace.acceptance_draft.pnp_max_rmse_px = "100".to_owned();
        workspace.acceptance_draft.pnp_max_error_px = "100".to_owned();
        workspace.acceptance_draft.minimum_auto_gain = "0.75".to_owned();
        workspace.auto_capture_enabled = true;

        workspace.observe_live_frame(
            Arc::clone(&displayed),
            test_live_source(),
            store.clone(),
            true,
        );

        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !workspace.auto_capture.pending.is_empty() && Instant::now() < deadline {
            workspace.tick(&context);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            workspace.auto_capture.pending.is_empty(),
            "auto worker did not finish: {}",
            workspace.status
        );
        assert_eq!(workspace.session.items().len(), 0, "{}", workspace.status);
        assert!(
            workspace.status.contains("below minimum"),
            "{}",
            workspace.status
        );
        let assessment = workspace.auto_capture.last_assessment.as_ref().unwrap();
        assert!(assessment.constraint_gain.abs() < f64::EPSILON);
    }

    #[test]
    fn automatic_capture_uses_transient_admission_without_saved_profile() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        workspace.auto_capture_enabled = true;

        workspace.observe_live_frame(live_frame(1), test_live_source(), store, false);

        assert!(workspace.session.auto_capture_baseline().is_some());
        assert!(workspace.session.initial_intrinsics_binding().is_some());
        assert_eq!(
            workspace
                .auto_capture
                .pending
                .front()
                .map(|candidate| candidate.intent),
            Some(CandidateIntent::AutoCommit)
        );
    }

    #[test]
    fn guided_capture_mode_routes_live_frames_to_pose_measurement() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let source = test_live_source();
        let first_frame = live_frame(1);
        workspace.observe_live_frame(
            Arc::clone(&first_frame),
            source.clone(),
            store.clone(),
            false,
        );

        workspace.auto_capture_trigger_mode = AutoCaptureTriggerMode::GuidedPresetPose;
        workspace.start_guided_capture();

        assert!(workspace.auto_capture_enabled);
        assert_eq!(
            workspace
                .guided_capture
                .as_ref()
                .map(|runtime| (runtime.current_step, runtime.state)),
            Some((0, GuidedCaptureState::Running))
        );
        let presentation = workspace
            .live_viewer_presentation(Some(first_frame.as_ref()), Some(&source))
            .expect("guided mode should publish a target overlay");
        assert!(presentation.item_id.is_none());
        let guided_target = presentation
            .overlay
            .guided_target
            .expect("guided presentation should carry the perspective grid target");
        assert_eq!(guided_target.grid_lines.len(), 23);
        assert!(
            guided_target
                .outline_uv
                .iter()
                .flatten()
                .all(|value| value.is_finite())
        );

        workspace.observe_live_frame(live_frame(2), source, store, false);

        let pending = workspace
            .auto_capture
            .pending
            .front()
            .expect("guided mode should queue a measurement candidate");
        assert_eq!(pending.intent, CandidateIntent::GuidedMeasure);
        assert_eq!(pending.guided_step_index, Some(0));
        assert!(workspace.session.items().is_empty());
    }

    #[test]
    fn live_preview_does_not_queue_detection_when_auto_capture_is_disabled() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let source = test_live_source();
        let first_frame = live_frame(1);
        workspace.observe_live_frame(Arc::clone(&first_frame), source.clone(), store, true);

        assert!(workspace.active_live_admission());
        assert_eq!(
            workspace
                .auto_capture
                .pending
                .front()
                .map(|pending| pending.intent),
            Some(CandidateIntent::PreviewOnly)
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !workspace.auto_capture.pending.is_empty() && Instant::now() < deadline {
            workspace.tick(&context);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(workspace.auto_capture.pending.is_empty());

        workspace.auto_capture_trigger_mode = AutoCaptureTriggerMode::GuidedPresetPose;
        workspace.start_guided_capture();

        assert!(workspace.auto_capture_enabled);
        assert_eq!(
            workspace
                .guided_capture
                .as_ref()
                .map(|runtime| (runtime.current_step, runtime.state)),
            Some((0, GuidedCaptureState::Running))
        );
        let presentation = workspace
            .live_viewer_presentation(Some(first_frame.as_ref()), Some(&source))
            .expect("guided mode should publish a target overlay without live preview detection");
        assert!(presentation.overlay.guided_target.is_some());
    }

    #[test]
    fn invalid_acceptance_edit_defers_error_until_edit_finishes() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        workspace.observe_live_frame(live_frame(1), test_live_source(), store, false);
        let baseline_digest = workspace
            .session
            .auto_capture_baseline()
            .unwrap()
            .digest
            .clone();

        workspace.acceptance_draft.pnp_depth_max = "partial".to_owned();
        workspace.apply_acceptance_render_result(true, true);

        assert!(workspace.acceptance_draft.error.is_none());
        assert_eq!(
            workspace.session.auto_capture_baseline().unwrap().digest,
            baseline_digest
        );
        assert_eq!(
            workspace.acceptance_last_valid_criteria.pnp_depth_max,
            2400.0
        );

        workspace.apply_acceptance_render_result(false, false);

        assert!(
            workspace
                .acceptance_draft
                .error
                .as_deref()
                .is_some_and(|error| error.contains("PnP maximum depth"))
        );
        assert_eq!(
            workspace.session.auto_capture_baseline().unwrap().digest,
            baseline_digest
        );

        workspace.acceptance_draft.pnp_depth_max = "2600".to_owned();
        workspace.apply_acceptance_render_result(true, true);

        assert!(workspace.acceptance_draft.error.is_none());
        assert_eq!(
            workspace.acceptance_last_valid_criteria.pnp_depth_max,
            2600.0
        );
        assert_ne!(
            workspace.session.auto_capture_baseline().unwrap().digest,
            baseline_digest
        );
    }
    #[test]
    fn invalid_draft_retains_last_valid_admission_only_for_current_context() {
        let context = egui::Context::default();
        let store = auto_capture_store();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let displayed = live_frame(1);
        workspace.observe_live_frame(
            Arc::clone(&displayed),
            test_live_source(),
            store.clone(),
            false,
        );
        let baseline_digest = workspace
            .session
            .auto_capture_baseline()
            .unwrap()
            .digest
            .clone();
        let binding_digest = workspace
            .session
            .initial_intrinsics_binding()
            .unwrap()
            .digest
            .clone();

        workspace.acceptance_draft.pnp_depth_max = "partial".to_owned();
        workspace.refresh_runtime_auto_admission();

        assert!(workspace.acceptance_draft.error.is_some());
        assert_eq!(
            workspace.session.auto_capture_baseline().unwrap().digest,
            baseline_digest
        );
        assert_eq!(
            workspace
                .session
                .initial_intrinsics_binding()
                .unwrap()
                .digest,
            binding_digest
        );

        let changed_source = LiveStreamSource::Rtsp {
            label: "Changed".to_owned(),
            channel: 0,
            transport: camera_toolbox_app::RtspTransport::Tcp,
            source_fingerprint: "changed-rtsp-source".to_owned(),
            geometry_key: "changed-rtsp-config".to_owned(),
            authoritative_capture: None,
        };
        workspace.observe_live_frame(live_frame(2), changed_source, store, false);

        assert!(workspace.session.auto_capture_baseline().is_none());
        assert!(workspace.session.initial_intrinsics_binding().is_none());
    }
}
