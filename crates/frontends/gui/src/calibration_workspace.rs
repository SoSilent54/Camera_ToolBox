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
    CalibrationSession, CalibrationSnapshot, CaptureStore, DecodedVideoFrame, EepromDryRunResult,
    EepromInspectResult, EepromWriteResult, EntryName, ExportDestination, ExportReceipt,
    ExportService, FileSourceId, FileSystem, FileSystemError, FsCancellation, FsControl,
    InitialIntrinsicsBinding, OperationId, PnPObservation, RasterImageCodec, SnapshotHash,
    StreamCaptureId, StreamFrameIdentity, StreamSessionId, host_monotonic_time_ns,
};
use camera_toolbox_core::{
    AssetId, BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationSolution,
    CaptureMetadata, ChessboardDetection, EphemeralAsset, FullEepromImage, InitialIntrinsics,
    IntegrityState, MediaFormat, OwnedMediaPayload, Rgba8Frame, ViewCalibrationResult,
    write_opencv_pinhole_radtan_yaml,
};
use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::calibration_acceptance::{
    DatasetAcceptanceDraft, DatasetAcceptancePanelState, DatasetAcceptanceProgress,
    render_dataset_acceptance,
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
    workspace::LiveStreamSource,
};

const MAX_DATASET_ITEMS: usize = 256;
const REMOTE_READS_PER_SOURCE: usize = 8;
const AUTO_CAPTURE_ANALYSIS_INTERVAL_NS: u64 = 200_000_000;
const AUTO_CAPTURE_ACCEPT_COOLDOWN_NS: u64 = 750_000_000;
const COVERAGE_WIDTH: usize = 192;
const COVERAGE_GAUSSIAN_SIGMA: f32 = 42.0 / 1920.0 * COVERAGE_WIDTH as f32;
pub(crate) const VIEWER_COVERAGE_COLUMNS: usize = 48;
pub(crate) const VIEWER_COVERAGE_ROWS: usize = 36;
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
    EepromBin(FullEepromImage),
}

impl CalibrationExport {
    #[must_use]
    pub(crate) const fn suggested_name(&self) -> &'static str {
        match self {
            Self::Json(_) => "camera_intrinsics.json",
            Self::Yaml(_) => "camera_intrinsics.yaml",
            Self::EepromBin(_) => "camera_eeprom.bin",
        }
    }

    #[must_use]
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Json(_) => "calibration JSON",
            Self::Yaml(_) => "calibration YAML",
            Self::EepromBin(_) => "EEPROM BIN",
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
            Self::EepromBin(image) => writer
                .write_all(image.as_bytes())
                .map_err(FileSystemError::io),
        }
    }
}

struct CalibrationSource {
    display_name: String,
    kind: CalibrationSourceKind,
    preview: Option<CalibrationPreview>,
}

enum CalibrationSourceKind {
    File {
        file_system: Arc<dyn FileSystem>,
        remote: bool,
    },
    Stream(StreamCalibrationSource),
}

struct StreamCalibrationSource {
    store: CaptureStore,
    asset: Option<Arc<EphemeralAsset>>,
    identity: StreamFrameIdentity,
    image_size: CalibrationImageSize,
    acquisition_key: camera_toolbox_app::AutoCaptureAcquisitionKey,
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
        Self {
            display_name: format!(
                "RTSP ch{} frame {}",
                identity.channel, identity.frame_sequence
            ),
            kind: CalibrationSourceKind::Stream(StreamCalibrationSource {
                store,
                asset: Some(asset),
                identity,
                image_size,
                acquisition_key,
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
        let Some(asset) = stream.asset.as_ref() else {
            return Err("stream calibration asset was released".to_owned());
        };
        if asset.metadata.format != MediaFormat::Png {
            return Err("stream calibration source is not a PNG asset".to_owned());
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
        let Some(asset) = self.asset.take() else {
            return;
        };
        let id = asset.id.clone();
        drop(asset);
        if let Err(error) = self.store.release(&id) {
            tracing::warn!(asset_id = %id, %error, "stream calibration asset release deferred by external ownership");
        }
    }
}

struct FrozenStreamInput {
    source: CalibrationSource,
    encoded: CalibrationEncodedPng,
}

fn freeze_stream_input(
    frame: &Arc<DecodedVideoFrame>,
    store: CaptureStore,
    acquisition_key: camera_toolbox_app::AutoCaptureAcquisitionKey,
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
        source: CalibrationSource::stream(
            store,
            asset,
            frame.identity.clone(),
            image_size,
            acquisition_key,
        ),
        encoded: CalibrationEncodedPng {
            bytes,
            image_size,
            source_revision,
        },
    })
}

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
    detection: ChessboardDetection,
}

#[derive(Clone)]
pub(crate) struct ViewerDetectionOverlay {
    pub(crate) image_size: CalibrationImageSize,
    pub(crate) corners: Vec<CalibrationPoint>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ViewerCoverageCell {
    pub(crate) column: usize,
    pub(crate) row: usize,
    pub(crate) density: f32,
}

#[derive(Clone, Default)]
pub(crate) struct CalibrationViewerOverlay {
    pub(crate) detection: Option<ViewerDetectionOverlay>,
    pub(crate) coverage: Vec<ViewerCoverageCell>,
    pub(crate) coverage_views: usize,
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
}

struct PendingCandidate {
    token: AutoCandidateToken,
    intent: CandidateIntent,
    source: CalibrationSource,
    encoded: Option<CalibrationEncodedPng>,
    cancellation: CalibrationCancellation,
    pose_request: Option<PoseEstimationRequest>,
    state: AutoCandidateState,
}

#[derive(Default)]
struct AutoCaptureSession {
    pending: Option<PendingCandidate>,
    last_observed: Option<StreamCaptureId>,
    last_observed_at_ns: u64,
    latest_detection: Option<IdentityBoundDetection>,
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
    serial_number: String,
    pending_export: Option<CalibrationExport>,
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
    coverage: Option<CoverageVisualization>,
    coverage_dirty: bool,
    auto_capture_enabled: bool,
    dataset_sidebar_expanded: bool,
    acceptance_draft: DatasetAcceptanceDraft,
    acceptance_last_valid_criteria: AutoCaptureAcceptanceCriteria,
    live_admission_context: Option<LiveAdmissionContext>,
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
            serial_number: String::new(),
            pending_export: None,
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
            coverage: None,
            coverage_dirty: true,
            auto_capture_enabled: false,
            dataset_sidebar_expanded: true,
            acceptance_draft,
            acceptance_last_valid_criteria,
            live_admission_context: None,
            auto_capture: AutoCaptureSession {
                next_candidate_id: 1,
                ..AutoCaptureSession::default()
            },
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
            .as_ref()
            .is_some_and(|candidate| candidate.token.frame_identity() == &frame.identity)
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
            match freeze_stream_input(&frame, store, source_acquisition_key.clone()) {
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
        let intent = if self.auto_capture_enabled && self.active_live_admission() {
            CandidateIntent::AutoCommit
        } else if preview_requested {
            CandidateIntent::PreviewOnly
        } else {
            return;
        };
        if self.active_job.is_some()
            || self.auto_capture.pending.is_some()
            || (intent == CandidateIntent::AutoCommit
                && self.session.items().len() >= MAX_DATASET_ITEMS)
        {
            return;
        }
        let capture_id = StreamCaptureId::from(&frame.identity);
        if self.auto_capture.last_observed.as_ref() == Some(&capture_id)
            || (intent == CandidateIntent::AutoCommit
                && self.session.items().iter().any(|item| {
                    item.input == CalibrationInputKey::StreamCapture(capture_id.clone())
                }))
        {
            return;
        }
        let now_ns = host_monotonic_time_ns();
        if (self.auto_capture.last_observed.is_some()
            && now_ns.saturating_sub(self.auto_capture.last_observed_at_ns)
                < AUTO_CAPTURE_ANALYSIS_INTERVAL_NS)
            || (intent == CandidateIntent::AutoCommit
                && self.auto_capture.last_accepted_at_ns != 0
                && now_ns.saturating_sub(self.auto_capture.last_accepted_at_ns)
                    < AUTO_CAPTURE_ACCEPT_COOLDOWN_NS)
        {
            return;
        }
        let pose_request = match intent {
            CandidateIntent::PreviewOnly => None,
            CandidateIntent::AutoCommit => {
                let Some(binding) = self.session.initial_intrinsics_binding() else {
                    self.status =
                        "Automatic capture waits for a valid current K/D12 binding.".to_owned();
                    return;
                };
                Some(PoseEstimationRequest {
                    initial_intrinsics: binding.initial_intrinsics.clone(),
                    reference_image_size: binding.reference_image_size,
                    binding_digest: binding.digest.clone(),
                })
            }
        };
        self.auto_capture.last_observed = Some(capture_id);
        self.auto_capture.last_observed_at_ns = now_ns;

        let FrozenStreamInput { source, encoded } =
            match freeze_stream_input(&frame, store, source_acquisition_key.clone()) {
                Ok(frozen) => frozen,
                Err(error) => {
                    let operation = match intent {
                        CandidateIntent::PreviewOnly => "Board preview",
                        CandidateIntent::AutoCommit => "Automatic capture",
                    };
                    self.status = format!("{operation} rejected before detection: {error}");
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
                let operation = match intent {
                    CandidateIntent::PreviewOnly => "Board preview candidate",
                    CandidateIntent::AutoCommit => "Automatic candidate",
                };
                self.status = format!("{operation} rejected: {error}");
                return;
            }
        };
        self.auto_capture.pending = Some(PendingCandidate {
            token,
            intent,
            source,
            encoded: Some(encoded),
            cancellation: CalibrationCancellation::default(),
            pose_request,
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
            if self.auto_capture.pending.is_some() {
                self.cancel_auto_candidate(
                    "Live detection candidate cancelled because source or image geometry changed.",
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
        let reconfigure = self.session.auto_capture_baseline() != Some(&baseline)
            || self.session.initial_intrinsics_binding() != Some(&binding);
        if reconfigure {
            if self.auto_capture.pending.is_some() {
                self.cancel_auto_candidate(
                    "Live detection candidate cancelled because acceptance settings or K/D12 changed.",
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
        let pending_matches = self
            .auto_capture
            .pending
            .as_ref()
            .is_some_and(|candidate| candidate.token.frame_identity().stream_id == *session_id);
        if pending_matches {
            self.cancel_auto_candidate(
                "Live detection candidate cancelled because its stream disconnected.",
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
        let Some(candidate) = self.auto_capture.pending.as_mut() else {
            return;
        };
        if candidate.state != AutoCandidateState::Queued {
            return;
        }
        let Some(encoded) = candidate.encoded.take() else {
            return;
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
            Err(TrySendError::Full(job)) => candidate.encoded = Some(job.encoded),
            Err(TrySendError::Disconnected(_)) => self.finalize_candidate(
                Some(context),
                candidate_id,
                CandidateTerminal::Discard(
                    "Calibration detection workers stopped unexpectedly.".to_owned(),
                ),
            ),
        }
    }

    fn finalize_candidate(
        &mut self,
        _context: Option<&egui::Context>,
        candidate_id: AutoCandidateId,
        terminal: CandidateTerminal,
    ) {
        let Some(candidate) = self.auto_capture.pending.take() else {
            return;
        };
        if candidate.token.id() != candidate_id {
            self.auto_capture.pending = Some(candidate);
            return;
        }
        let PendingCandidate {
            token,
            intent,
            source,
            cancellation,
            ..
        } = candidate;
        cancellation.cancel();
        match terminal {
            CandidateTerminal::Detection(Ok(DetectionProduct {
                source_revision,
                outcome: camera_toolbox_core::ChessboardDetectionOutcome::Found(detection),
                pnp_observation,
                preview: _,
            })) => {
                if source_revision != *token.source_revision() {
                    self.auto_capture.latest_detection = None;
                    self.status = "Live detection source revision changed.".to_owned();
                    return;
                }
                self.auto_capture.latest_detection = Some(IdentityBoundDetection {
                    identity: token.frame_identity().clone(),
                    detection: detection.clone(),
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
                        let commit = AutoCandidateCommit::new(
                            token,
                            source_revision,
                            detection,
                            pnp_observation,
                        );
                        match self.session.commit_auto_candidate(commit) {
                            Ok(item_id) => {
                                self.sources.insert(item_id, source);
                                self.auto_capture.last_assessment =
                                    self.session.assess_auto_admission(None).ok();
                                self.auto_capture.last_accepted_at_ns = host_monotonic_time_ns();
                                self.coverage_dirty = true;
                                self.status = format!(
                                    "Automatic capture committed as dataset item {} (candidate gain {}).",
                                    item_id.get(),
                                    gain
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
                };
            }
            CandidateTerminal::Discard(message) => {
                self.auto_capture.latest_detection = None;
                self.status = message;
            }
        }
    }

    fn cancel_auto_candidate(&mut self, message: impl Into<String>) {
        let message = message.into();
        if let Some(candidate_id) = self
            .auto_capture
            .pending
            .as_ref()
            .map(|candidate| candidate.token.id())
        {
            self.finalize_candidate(None, candidate_id, CandidateTerminal::Discard(message));
        } else {
            self.status = message;
        }
    }

    fn remember_stream_detection(
        &mut self,
        item_id: CalibrationItemId,
        outcome: &camera_toolbox_core::ChessboardDetectionOutcome,
    ) {
        let Some(source) = self.sources.get(&item_id) else {
            return;
        };
        let CalibrationSourceKind::Stream(stream) = &source.kind else {
            return;
        };
        self.auto_capture.latest_detection = match outcome {
            camera_toolbox_core::ChessboardDetectionOutcome::Found(detection) => {
                Some(IdentityBoundDetection {
                    identity: stream.identity.clone(),
                    detection: detection.clone(),
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

    pub(crate) fn report_eeprom_dry_run(
        &mut self,
        target_label: String,
        request: camera_toolbox_core::EepromProvisionRequest,
        result: EepromDryRunResult,
        backup_file: String,
        manifest_file: String,
    ) {
        self.eeprom
            .report_dry_run(target_label, request, result, backup_file, manifest_file);
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

    /// 为当前 live frame 生成 identity-safe 检测点和同采集组的归一化 coverage。
    pub(crate) fn viewer_overlay(
        &self,
        frame: &DecodedVideoFrame,
        live_source: &LiveStreamSource,
    ) -> CalibrationViewerOverlay {
        let Ok(image_size) = CalibrationImageSize::new(frame.width, frame.height) else {
            return CalibrationViewerOverlay::default();
        };
        let detection = self
            .auto_capture
            .latest_detection
            .as_ref()
            .filter(|latest| {
                latest.identity == frame.identity && latest.detection.image_size == image_size
            })
            .map(|latest| ViewerDetectionOverlay {
                image_size,
                corners: latest.detection.corners.clone(),
            });
        let current_acquisition_key = live_source.acquisition_key_for_frame(frame).ok();

        let mut counts = vec![0_u32; VIEWER_COVERAGE_COLUMNS * VIEWER_COVERAGE_ROWS];
        let mut coverage_views = 0_usize;
        for item in self.session.items().iter().filter(|item| item.enabled) {
            let CalibrationItemStatus::Found(item_detection) = &item.status else {
                continue;
            };
            if item_detection.image_size != image_size {
                continue;
            }
            let Some(source) = self.sources.get(&item.id) else {
                continue;
            };
            let CalibrationSourceKind::Stream(stream) = &source.kind else {
                continue;
            };
            if stream.image_size != image_size
                || item.acquisition_key.as_ref() != current_acquisition_key.as_ref()
            {
                continue;
            }
            coverage_views = coverage_views.saturating_add(1);
            for corner in &item_detection.corners {
                let normalized_x = (corner.x + 0.5) / image_size.width as f32;
                let normalized_y = (corner.y + 0.5) / image_size.height as f32;
                if !(0.0..1.0).contains(&normalized_x) || !(0.0..1.0).contains(&normalized_y) {
                    continue;
                }
                let column = ((normalized_x * VIEWER_COVERAGE_COLUMNS as f32) as usize)
                    .min(VIEWER_COVERAGE_COLUMNS - 1);
                let row = ((normalized_y * VIEWER_COVERAGE_ROWS as f32) as usize)
                    .min(VIEWER_COVERAGE_ROWS - 1);
                let index = row * VIEWER_COVERAGE_COLUMNS + column;
                counts[index] = counts[index].saturating_add(1);
            }
        }
        let maximum = counts.iter().copied().max().unwrap_or_default();
        let coverage = if maximum == 0 {
            Vec::new()
        } else {
            counts
                .into_iter()
                .enumerate()
                .filter_map(|(index, count)| {
                    (count != 0).then_some(ViewerCoverageCell {
                        column: index % VIEWER_COVERAGE_COLUMNS,
                        row: index / VIEWER_COVERAGE_COLUMNS,
                        density: count as f32 / maximum as f32,
                    })
                })
                .collect()
        };
        CalibrationViewerOverlay {
            detection,
            coverage,
            coverage_views,
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
                    self.render_dataset_acceptance_panel(ui);
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("calibration_dataset_sidebar")
                        .show(ui, |ui| self.render_dataset(ui, false));
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
            if self.active_job.is_some() || self.auto_capture.pending.is_some() {
                ui.spinner();
                ui.separator();
            }
            ui.label(&self.status);
            if let Some(installed) = self.session.installed() {
                ui.separator();
                ui.monospace(format!("RMS {:.4} px", installed.solution.rms_error));
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
                assessment.constraint_gain,
                assessment.field_gain,
                assessment.depth_gain,
                assessment.pose_gain
            ));
        });
        ui.weak(
            "Runtime source-bound collection state; it is not persisted or production-qualified.",
        );
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
        let installed = self.session.installed().is_some();
        let regular_export_enabled = idle && installed && export_enabled;

        egui::CollapsingHeader::new("Calibration result")
            .id_salt("calibration_result_foldout")
            .default_open(true)
            .show(ui, |ui| {
                render_calibration_result(ui, self.session.installed());
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(regular_export_enabled, egui::Button::new("Export JSON"))
                        .on_disabled_hover_text(export_reason.unwrap_or(
                            "A calibration result and writable Explorer directory are required.",
                        ))
                        .clicked()
                    {
                        self.pending_export = self.json_export();
                    }
                    let export_yaml = ui
                        .add_enabled(
                            regular_export_enabled,
                            egui::Button::new("Export YAML Result"),
                        )
                        .on_disabled_hover_text(export_reason.unwrap_or(
                            "A calibration result and writable Explorer directory are required.",
                        ))
                        .clicked()
                        .then(|| {
                            self.session
                                .installed()
                                .map(|installed| installed.solution.clone())
                        })
                        .flatten();
                    if let Some(solution) = export_yaml {
                        self.pending_export = Some(CalibrationExport::Yaml(solution));
                    }
                });
                if installed && !export_enabled {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        export_reason
                            .unwrap_or("Select a writable Explorer directory before exporting."),
                    );
                }
            });

        let eeprom_error = self.eeprom_image().err();
        ui.collapsing("EEPROM Provisioning", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("EEPROM SN");
                ui.add(
                    egui::TextEdit::singleline(&mut self.serial_number)
                        .desired_width(160.0)
                        .hint_text("14 ASCII bytes"),
                );
                ui.weak("Required only for EEPROM BIN / direct provisioning.");
                let eeprom_enabled = regular_export_enabled && eeprom_error.is_none();
                if ui
                    .add_enabled(eeprom_enabled, egui::Button::new("Save EEPROM BIN"))
                    .on_disabled_hover_text(
                        eeprom_error.as_deref().unwrap_or(
                            "A calibration result and valid 14-byte ASCII SN are required.",
                        ),
                    )
                    .clicked()
                    && let Ok(image) = self.eeprom_image()
                {
                    self.pending_export = Some(CalibrationExport::EepromBin(image));
                }
            });
            if !export_enabled {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    export_reason
                        .unwrap_or("Select a writable Explorer directory before exporting."),
                );
            }
            let solution = self
                .session
                .installed()
                .map(|installed| installed.solution.clone());
            self.eeprom.render_body(
                context,
                ui,
                solution.as_ref(),
                &self.serial_number,
                sftp_source,
                provision_target,
                (!export_enabled).then_some(
                    export_reason
                        .unwrap_or("Select a writable Explorer directory before exporting."),
                ),
            );
        });
        self.eeprom.render_confirmation(context, provision_target);
    }

    fn render_controls(&mut self, ui: &mut egui::Ui) {
        if !self.intrinsics_value_editing {
            self.refresh_auto_intrinsics_fields();
        }
        let idle = self.active_job.is_none();
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
            let auto_capture_changed = ui
                .checkbox(&mut self.auto_capture_enabled, "Request RTSP auto capture")
                .on_hover_text(
                    "Uses the current displayed source, valid Dataset Acceptance thresholds, and the current K/D12 seed.",
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
                "Admission is in-memory and source-bound. Valid threshold and K edits take effect immediately.",
            );
            if let Some(assessment) = self.auto_capture.last_assessment.as_ref() {
                self.render_dataset_assessment(ui, assessment);
            } else {
                ui.weak("Display a live frame to initialize runtime Dataset Acceptance.");
            }
            if let Some(candidate) = self.auto_capture.pending.as_ref() {
                ui.monospace(format!(
                    "Candidate {}: {:?}",
                    candidate.token.id().get(),
                    candidate.state
                ));
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
                    idle && self.auto_capture.pending.is_none()
                        && self.session.installed().is_some(),
                    egui::Button::new("Use result as initial K+D12"),
                )
                .on_disabled_hover_text(
                    "Requires an installed result and no active calibration or live candidate.",
                )
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

    // 为下方 Dataset 表保留可操作高度；小窗口仍保留最小的验收滚动视口。
    const DATASET_ACCEPTANCE_MIN_VIEWPORT_HEIGHT: f32 = 96.0;
    const DATASET_ACCEPTANCE_MAX_VIEWPORT_HEIGHT: f32 = 420.0;
    const DATASET_TABLE_RESERVED_HEIGHT: f32 = 180.0;

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

    fn render_dataset_acceptance_panel(&mut self, ui: &mut egui::Ui) {
        let fallback_criteria = self.acceptance_last_valid_criteria.clone();
        let progress = self.dataset_acceptance_assessment().map_or_else(
            || DatasetAcceptanceProgress::empty(&fallback_criteria),
            |assessment| DatasetAcceptanceProgress::from_assessment(&assessment),
        );
        let state = DatasetAcceptancePanelState {
            has_live_context: self.live_admission_context.is_some(),
            admission_active: self.active_live_admission(),
            auto_capture_enabled: self.auto_capture_enabled,
        };
        let acceptance_viewport_height =
            (ui.available_height() - Self::DATASET_TABLE_RESERVED_HEIGHT).clamp(
                Self::DATASET_ACCEPTANCE_MIN_VIEWPORT_HEIGHT,
                Self::DATASET_ACCEPTANCE_MAX_VIEWPORT_HEIGHT,
            );
        let render_result = render_dataset_acceptance(
            ui,
            &mut self.acceptance_draft,
            &progress,
            state,
            acceptance_viewport_height,
        );
        self.apply_acceptance_render_result(render_result.changed, render_result.editing);
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

    fn render_dataset(&mut self, ui: &mut egui::Ui, show_heading: bool) {
        if show_heading {
            ui.heading(format!("Dataset ({})", self.session.items().len()));
        }
        let mut toggle = None;
        let mut select = None;
        let installed = self.session.installed();
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
            let _ = self.session.set_selected(id);
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
        let projected = calibration_view(self.session.installed(), id)
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
            if self.auto_capture.pending.is_some() {
                self.cancel_auto_candidate(
                    "Live detection candidate cancelled because the board specification changed.",
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
                    if let Some(candidate_id) = self
                        .auto_capture
                        .pending
                        .as_ref()
                        .map(|candidate| candidate.token.id())
                    {
                        self.finalize_candidate(
                            Some(context),
                            candidate_id,
                            CandidateTerminal::Discard(
                                "Calibration detection workers stopped unexpectedly.".to_owned(),
                            ),
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
                if let Some(candidate) = self.auto_capture.pending.as_mut()
                    && candidate.token == token
                {
                    candidate.state = AutoCandidateState::Detecting;
                    self.status = match candidate.intent {
                        CandidateIntent::PreviewOnly => {
                            format!("Detecting board preview candidate {}.", token.id().get())
                        }
                        CandidateIntent::AutoCommit => {
                            format!("Detecting automatic candidate {}.", token.id().get())
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
                self.finalize_candidate(
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
                    self.remember_stream_detection(token.item_id, &outcome);
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

    fn eeprom_image(&self) -> Result<FullEepromImage, String> {
        let installed = self
            .session
            .installed()
            .ok_or_else(|| "Calibrate successfully before exporting EEPROM data.".to_owned())?;
        FullEepromImage::from_solution(&installed.solution, &self.serial_number)
            .map_err(|error| error.to_string())
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
        if let Some(candidate_id) = self
            .auto_capture
            .pending
            .as_ref()
            .map(|candidate| candidate.token.id())
        {
            self.finalize_candidate(
                None,
                candidate_id,
                CandidateTerminal::Discard("Calibration workspace closed.".to_owned()),
            );
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
    let rotation = rodrigues_matrix_for_preview(observation.rotation_vector)?;
    let board_width = f64::from(board.inner_cols.saturating_sub(1)) * board.square_size;
    let board_height = f64::from(board.inner_rows.saturating_sub(1)) * board.square_size;
    let axis_length = board_width.max(board_height).max(board.square_size) * 0.25;
    let origin = [board_width * 0.5, board_height * 0.5, 0.0];
    let project = |point: [f64; 3]| {
        project_board_point(
            rotation,
            observation.translation_vector,
            point,
            intrinsics,
            image_rect,
            image_width,
            image_height,
            horizontal_flip,
        )
    };
    Some(PoseAxisProjection {
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
    let distortion = &intrinsics.distortion_coefficients;
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
    let x_distorted = x * radial
        + 2.0 * coefficient(2) * x * y
        + coefficient(3) * (r2 + 2.0 * x * x)
        + coefficient(8) * r2
        + coefficient(9) * r4;
    let y_distorted = y * radial
        + coefficient(2) * (r2 + 2.0 * y * y)
        + 2.0 * coefficient(3) * x * y
        + coefficient(10) * r2
        + coefficient(11) * r4;
    let matrix = intrinsics.camera_matrix;
    let image_x = matrix[0] * x_distorted + matrix[2];
    let image_y = matrix[4] * y_distorted + matrix[5];
    if !image_x.is_finite() || !image_y.is_finite() {
        return None;
    }
    Some(image_point_to_preview(
        CalibrationPoint {
            x: image_x as f32,
            y: image_y as f32,
        },
        image_rect,
        image_width,
        image_height,
        horizontal_flip,
    ))
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
    } else if contribution.constraint_gain == 0 {
        ui.weak("No Gain Gap").on_hover_text(
            "Valid item with zero target-capped attributed Gain; required coverage is already owned by earlier compatible Dataset rows or above target.",
        );
    } else {
        ui.colored_label(egui::Color32::LIGHT_GREEN, "Accepted")
            .on_hover_text("Item participates in current Dataset Acceptance and has positive target-capped Gain.");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdmissionDeltaCellState {
    Contribution(usize),
    PnpBlocked,
    Disabled,
    OutsideActiveAdmission,
}

fn admission_delta_cell_state(
    contribution: Option<&AutoAdmissionItemContribution>,
    enabled: bool,
    metric_blocked: impl FnOnce(&AutoAdmissionItemContribution) -> bool,
    value: impl FnOnce(&AutoAdmissionItemContribution) -> usize,
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
    value: impl FnOnce(&AutoAdmissionItemContribution) -> usize,
) {
    match admission_delta_cell_state(contribution, enabled, metric_blocked, value) {
        AdmissionDeltaCellState::Contribution(delta) => {
            ui.monospace(format!("+{delta}")).on_hover_text(format!(
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
            let label = if contribution.constraint_gain == 0 {
                "×0".to_owned()
            } else {
                format!("+{}*", contribution.constraint_gain)
            };
            ui.colored_label(egui::Color32::LIGHT_RED, label)
                .on_hover_text(format!(
                    "Total Gain includes valid Found/Field contributions only; Depth/Pose are blocked by PnP.\n{}",
                    pnp_state_reason(&contribution.pnp_state)
                ));
        }
        Some(contribution) => {
            ui.monospace(format!("+{}", contribution.constraint_gain)).on_hover_text(
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
    installed: Option<&camera_toolbox_app::InstalledCalibration>,
) {
    let Some(installed) = installed else {
        ui.weak("Run Calibrate to display final intrinsics and distortion coefficients.");
        return;
    };
    let solution = &installed.solution;
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
        ui.monospace(format!(
            "{}×{} · RMS {:.6} px · flags {}",
            solution.image_size.width,
            solution.image_size.height,
            solution.rms_error,
            solution.calibration_flags
        ));
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
        assert!(workspace.auto_capture.pending.is_none());
        assert_eq!(store.stats().unwrap(), baseline);

        workspace.observe_live_frame(
            Arc::clone(&displayed),
            test_live_source(),
            store.clone(),
            true,
        );

        assert_eq!(
            workspace.auto_capture.pending.as_ref().unwrap().intent,
            CandidateIntent::PreviewOnly
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while workspace.auto_capture.pending.is_some() && Instant::now() < deadline {
            workspace.tick(&context);
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            workspace.auto_capture.pending.is_none(),
            "preview worker did not finish: {}",
            workspace.status
        );
        assert!(workspace.session.items().is_empty());
        assert!(workspace.sources.is_empty());
        assert_eq!(store.stats().unwrap(), baseline);
        let overlay = workspace.viewer_overlay(&displayed, &test_live_source());
        assert_eq!(overlay.detection.unwrap().corners.len(), 88);
        assert_eq!(overlay.coverage_views, 0);
        assert!(overlay.coverage.is_empty());
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

        assert_eq!(overlay.coverage_views, 1);
        assert!(!overlay.coverage.is_empty());
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
        while workspace.auto_capture.pending.is_some() && Instant::now() < deadline {
            workspace.tick(&context);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            workspace.auto_capture.pending.is_none(),
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
        workspace.acceptance_draft.minimum_auto_gain = "999".to_owned();
        workspace.auto_capture_enabled = true;

        workspace.observe_live_frame(
            Arc::clone(&displayed),
            test_live_source(),
            store.clone(),
            true,
        );

        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while workspace.auto_capture.pending.is_some() && Instant::now() < deadline {
            workspace.tick(&context);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            workspace.auto_capture.pending.is_none(),
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
        assert_eq!(assessment.constraint_gain, 0);
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
                .as_ref()
                .map(|candidate| candidate.intent),
            Some(CandidateIntent::AutoCommit)
        );
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
        };
        workspace.observe_live_frame(live_frame(2), changed_source, store, false);

        assert!(workspace.session.auto_capture_baseline().is_none());
        assert!(workspace.session.initial_intrinsics_binding().is_none());
    }
}
