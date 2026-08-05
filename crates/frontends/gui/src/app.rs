//! GUI 顶层编排；文档状态由 workspace 模块独立持有。

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
use crate::calibration_eeprom::{CalibrationEepromTargetRequest, CalibrationProvisionIntent};
#[cfg(feature = "calibration-opencv")]
use crate::calibration_workspace::{
    CalibrationExport, CalibrationViewerOverlay, CalibrationViewerPresentation,
    CalibrationWorkspace, ViewerDetectionOverlay, ViewerPoseAxisOverlay,
};

#[cfg(feature = "platform-ssh")]
use crate::explorer::RemoteConnectionCommit;
#[cfg(feature = "platform-ssh")]
use crate::i2c_tools::{I2cToolsAction, I2cToolsWorkspace};
use crate::{
    analysis_panel::{DesiredAnalysis, render_analysis_panel},
    analysis_worker::{
        AnalysisDomain, AnalysisKey, AnalysisRequest, AnalysisResult, AnalysisWorker,
    },
    auto_open::{AutoOpenCandidate, AutoOpenCoordinator},
    color_controls::{DisplayMode, render_color_controls},
    color_inspection::{
        ColorImagePoint, ColorInspectionAction, ColorInspectionWorkspace, ColorMetricsExport,
        paint_color_chart_overlay, paint_manual_corner_overlay,
    },
    color_worker::{ColorRenderRequest, ColorRenderResult, ColorRenderWorker},
    explorer::{ExplorerAction, ExplorerState},
    export_dialog::ExportNameDialogState,
    histogram_link::{
        DisplayHistogramImage, HistogramBinSelection, HistogramPixelSample, ImageHistogramHover,
        SpatialHighlight, SpatialHighlightRequest, SpatialHighlightResult, SpatialHighlightWorker,
        display_histogram_sample,
    },
    image_save::{
        ImageSaveWorker, SaveFormat, SaveKey, SavePayload, SaveRequest, SaveResult,
        YuvSaveDialogState,
    },
    notification::{NotificationCenter, NotificationKey, NotificationScope, UiNotification},
    platform_ui::{LiveRuntime, PlatformEffect, PlatformUiAction, StreamPanelAction},
    raw_dialog::{RawOpenDialogState, local_file_source},
    raw_inspector::render_raw_inspector,
    viewer::{
        HoverNeighborhood, HoverViewSettings, ImageViewerState, LoadedRaw, ViewerAction,
        ViewerImage, ViewerOutput, bayer_label, render_viewer, viewer_texture_uv,
    },
    workspace::{
        DocumentId, DocumentIdentity, LiveDocument, LiveDocumentLifecycle, LiveStreamSource,
        TabBarAction, WorkspaceState, render_tab_bar,
    },
    yuv_inspector::render_yuv_inspector,
};
use camera_toolbox_adapters::ImageRasterCodec;
#[cfg(feature = "platform-ssh")]
use camera_toolbox_adapters::platforms::ssh_managed::{
    RusshTransportFactory, SshConnectionTarget, SshEepromProvisionService, SshI2cHelperService,
};
use camera_toolbox_app::{
    AutoOpenActivation, EntryName, ExportDestination, ExportReceipt, FileRef, FileSystem,
    FsCancellation, FsControl, I2cBusInfo, I2cHelperAction, I2cHelperOperation, I2cHelperResult,
    I2cHelperService, ImageFileKind, ImageOpenMode, ImageOpenPipeline, ImageOpenResult,
    ImageSourceHandle, LocalRawAnalyzeReport, LocalRawAnalyzeRequest, RasterImageCodec,
    RawDecodeParams, RawInterpretation, RawOpenMode, RawOpenPipeline, RtspCodec, RtspLatencyMode,
    RtspStreamConfig, RtspTransport, SourceCache, SourceReadProgress, WorkspaceSettings,
};
#[cfg(feature = "platform-ssh")]
use camera_toolbox_app::{
    DumpCancellation, I2cHelperServiceError, RemoteOperationControl, RemoteTimeouts,
};
#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
use camera_toolbox_app::{
    EepromHelperResult, EepromInspectResult, EepromProvisionOperation, EepromProvisionService,
    EepromProvisionServiceError, EepromWriteResult, SnapshotHash,
};
use camera_toolbox_core::{
    ChromaOrder, MediaFormat, NativeImage, OwnedMediaPayload, PackedRawSpec, Rgba8Frame, Roi,
    Yuv420SpFrame, Yuv420SpSpec, YuvMatrix, YuvRange, analyze_roi, decode_le_continuous_raw,
    yuv420sp_to_rgba8_with_cancel,
};
use eframe::egui;
const LIVE_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const AUTO_OPEN_QUEUE_LIMIT: usize = 16;
const AUTO_OPEN_BACKGROUND_TAB_LIMIT: usize = 8;
const RAW_SOURCE_CACHE_BYTES: u64 = 512 * 1024 * 1024;
const RAW_SOURCE_CACHE_ENTRIES: usize = 16;
const RAW_PROGRESS_REPAINT_INTERVAL: Duration = Duration::from_millis(100);
const CAPTURED_RASTER_DECODE_BYTES: usize = 256 * 1024 * 1024;
#[cfg(feature = "calibration-opencv")]
const LIVE_VIEWER_DATASET_OVERLAY_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 190, 64);

struct OpenedRawDocument {
    report: LocalRawAnalyzeReport,
    source: ImageSourceHandle,
    interpretation: RawInterpretation,
}

enum OpenedFileDocument {
    Raw(OpenedRawDocument),
    Image(ImageOpenResult),
}

struct WorkspaceFileOpenRequest {
    display_path: PathBuf,
    file_system: Arc<dyn FileSystem>,
    reference: FileRef,
    remote: bool,
}

enum RawOpenJobEvent {
    Progress {
        attempt: u64,
        progress: SourceReadProgress,
    },
    Finished(Box<RawOpenJobResult>),
}

struct RawOpenJobResult {
    attempt: u64,
    path: PathBuf,
    result: Result<OpenedFileDocument, String>,
}

struct ActiveRawOpenJob {
    attempt: u64,
    path: PathBuf,
    remote: bool,
    progress: Option<SourceReadProgress>,
    cancellation: FsCancellation,
}

struct PendingAutoOpenRequest {
    candidate: AutoOpenCandidate,
    request: WorkspaceFileOpenRequest,
}

struct AutoOpenJobResult {
    candidate: AutoOpenCandidate,
    path: PathBuf,
    result: Result<OpenedFileDocument, String>,
}

struct ActiveAutoOpenJob {
    candidate: AutoOpenCandidate,
    cancellation: FsCancellation,
}

struct ReinterpretJobResult {
    document_id: DocumentId,
    decode_generation: u64,
    result: Result<OpenedRawDocument, String>,
}

struct YuvReinterpretJobResult {
    document_id: DocumentId,
    decode_generation: u64,
    result: Result<ImageOpenResult, String>,
}

struct PendingYuvSave {
    key: SaveKey,
    destination: ExportDestination,
    target_label: String,
    file_name: EntryName,
    frame: Arc<Rgba8Frame>,
}

struct ImageExportSnapshot {
    raw: Option<(SaveKey, Arc<camera_toolbox_core::RawFrame>)>,
    display: Option<(SaveKey, Arc<Rgba8Frame>)>,
}

enum PendingNamedExport {
    Image {
        snapshot: ImageExportSnapshot,
    },
    Color {
        export: ColorMetricsExport,
    },
    #[cfg(feature = "calibration-opencv")]
    Calibration {
        export: CalibrationExport,
    },
}

#[cfg(feature = "calibration-opencv")]
struct CalibrationExportResult {
    destination: ExportDestination,
    target_label: String,
    label: &'static str,
    result: Result<ExportReceipt, String>,
}

struct ColorExportResult {
    destination: ExportDestination,
    target_label: String,
    label: &'static str,
    result: Result<ExportReceipt, String>,
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
#[derive(Clone)]
struct EepromProvisioningTarget {
    service: Arc<dyn EepromProvisionService>,
    snapshot_hash: SnapshotHash,
    label: String,
    i2c_bus: u32,
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
#[derive(Debug)]
enum EepromOperationOutcome {
    Inspect(EepromInspectResult),
    BusDiscovery {
        buses: Vec<I2cBusInfo>,
    },
    Provision {
        result: EepromWriteResult,
        history_file: String,
    },
    ProvisionAuditFailed {
        result: EepromWriteResult,
        error: String,
    },
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EepromOperationKind {
    Inspect,
    BusDiscovery,
    Provision,
}

#[cfg(feature = "platform-ssh")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum I2cToolsOperationKind {
    BusDiscovery,
    Transfer,
}

#[cfg(feature = "platform-ssh")]
struct I2cToolsOperationResult {
    kind: I2cToolsOperationKind,
    result: Result<I2cHelperResult, String>,
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
#[derive(Debug)]
struct EepromOperationFailure {
    message: String,
    provision_state_unknown: bool,
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
impl EepromOperationFailure {
    fn known(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            provision_state_unknown: false,
        }
    }

    fn unknown(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            provision_state_unknown: true,
        }
    }
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
impl From<String> for EepromOperationFailure {
    fn from(message: String) -> Self {
        Self::known(message)
    }
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
struct EepromOperationResult {
    kind: EepromOperationKind,
    target_label: String,
    result: Result<EepromOperationOutcome, EepromOperationFailure>,
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn run_eeprom_operation(
    target: EepromProvisioningTarget,
    intent: CalibrationProvisionIntent,
    operation_id: u64,
    cancellation: DumpCancellation,
) -> Result<EepromOperationOutcome, EepromOperationFailure> {
    let action = intent
        .helper_action()
        .ok_or_else(|| "cancel is not an executable EEPROM helper action".to_owned())?;

    let control = RemoteOperationControl::new(
        RemoteTimeouts {
            connect: Duration::from_secs(10),
            idle: Duration::from_secs(30),
            overall: Duration::from_secs(120),
        },
        cancellation,
    )
    .map_err(|error| error.to_string())?;
    let operation = EepromProvisionOperation {
        action: action.clone(),
    };
    let helper_result = match target.service.execute(operation, control) {
        Ok(result) => result,
        Err(error) => {
            let provision_state_unknown = matches!(
                (&intent, &error),
                (
                    CalibrationProvisionIntent::Provision { .. },
                    EepromProvisionServiceError::Transport(_)
                        | EepromProvisionServiceError::Protocol(_)
                )
            ) || matches!(
                (&intent, &error),
                (
                    CalibrationProvisionIntent::Provision { .. },
                    EepromProvisionServiceError::Helper(failure)
                ) if failure.rollback == camera_toolbox_app::EepromRollbackState::Failed
            );
            let mut message = format_eeprom_service_error(&error);
            if let CalibrationProvisionIntent::Provision { request, .. } = &intent {
                let document = serde_json::json!({
                    "schema_version": 2,
                    "operation": "eeprom_provision_failure",
                    "operation_id": operation_id,
                    "target": eeprom_target_json(&target),
                    "request": eeprom_action_parameters_json(&action),
                    "failure": eeprom_failure_json(&error),
                    "device_state_unknown": provision_state_unknown,
                });
                match persist_eeprom_write_history_yaml(
                    &request.serial_number,
                    operation_id,
                    &document,
                ) {
                    Ok(label) => message.push_str(&format!("; failure audit: {label}")),
                    Err(audit_error) => message
                        .push_str(&format!("; failure audit save also failed: {audit_error}")),
                }
            }
            return Err(if provision_state_unknown {
                EepromOperationFailure::unknown(message)
            } else {
                EepromOperationFailure::known(message)
            });
        }
    };

    match (intent, helper_result) {
        (CalibrationProvisionIntent::Inspect, EepromHelperResult::Inspect(result)) => {
            Ok(EepromOperationOutcome::Inspect(result))
        }
        (
            CalibrationProvisionIntent::Provision { request, .. },
            EepromHelperResult::Provision(result),
        ) => {
            let document = serde_json::json!({
                "schema_version": 2,
                "operation": "eeprom_provision_success",
                "operation_id": operation_id,
                "target": eeprom_target_json(&target),
                "request": eeprom_action_parameters_json(&action),
                "result": eeprom_write_result_json(&result),
            });
            match persist_eeprom_write_history_yaml(&request.serial_number, operation_id, &document)
            {
                Ok(history_file) => Ok(EepromOperationOutcome::Provision {
                    result,
                    history_file,
                }),
                Err(error) => Ok(EepromOperationOutcome::ProvisionAuditFailed { result, error }),
            }
        }

        (_, unexpected) => Err(EepromOperationFailure::known(format!(
            "EEPROM helper returned an unexpected result kind: {unexpected:?}"
        ))),
    }
}
#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn run_i2c_bus_discovery(
    connection: SshConnectionTarget,
    credential_ref: String,
    helper_payload: Arc<[u8]>,
    resolver: Arc<dyn camera_toolbox_adapters::platforms::ssh_managed::CredentialResolver>,
    transport: Arc<dyn camera_toolbox_adapters::platforms::ssh_managed::SshTransportFactory>,
    operation_id: u64,
    cancellation: DumpCancellation,
) -> Result<EepromOperationOutcome, EepromOperationFailure> {
    let control = RemoteOperationControl::new(
        RemoteTimeouts {
            connect: Duration::from_secs(10),
            idle: Duration::from_secs(15),
            overall: Duration::from_secs(45),
        },
        cancellation,
    )
    .map_err(|error| error.to_string())?;
    let service = SshI2cHelperService::new(
        format!("calibration-i2c-{}", operation_id),
        connection,
        credential_ref,
        65_536,
        helper_payload,
        resolver,
        transport,
    )
    .map_err(|error: camera_toolbox_app::I2cHelperServiceError| {
        EepromOperationFailure::known(error.to_string())
    })?;
    let result = service
        .execute(
            I2cHelperOperation {
                action: I2cHelperAction::ListBuses,
            },
            control,
        )
        .map_err(|error: camera_toolbox_app::I2cHelperServiceError| {
            EepromOperationFailure::known(error.to_string())
        })?;
    match result {
        I2cHelperResult::BusList { buses } => Ok(EepromOperationOutcome::BusDiscovery { buses }),
        unexpected => Err(EepromOperationFailure::known(format!(
            "I2C helper returned an unexpected result kind: {unexpected:?}"
        ))),
    }
}

#[cfg(feature = "platform-ssh")]
fn run_i2c_tools_request(
    connection: SshConnectionTarget,
    credential_ref: String,
    helper_payload: Arc<[u8]>,
    resolver: Arc<dyn camera_toolbox_adapters::platforms::ssh_managed::CredentialResolver>,
    transport: Arc<dyn camera_toolbox_adapters::platforms::ssh_managed::SshTransportFactory>,
    action: I2cHelperAction,
    cancellation: DumpCancellation,
) -> Result<I2cHelperResult, String> {
    let (idle, overall) = match action {
        I2cHelperAction::ListBuses => (Duration::from_secs(15), Duration::from_secs(45)),
        I2cHelperAction::Transfer { .. } => (Duration::from_secs(30), Duration::from_secs(120)),
    };
    let control = RemoteOperationControl::new(
        RemoteTimeouts {
            connect: Duration::from_secs(10),
            idle,
            overall,
        },
        cancellation,
    )
    .map_err(|error| error.to_string())?;
    let service = SshI2cHelperService::new(
        "i2c-tools".to_owned(),
        connection,
        credential_ref,
        1_048_576,
        helper_payload,
        resolver,
        transport,
    )
    .map_err(|error| error.to_string())?;
    service
        .execute(I2cHelperOperation { action }, control)
        .map_err(|error| format_i2c_service_error(&error))
}

#[cfg(feature = "platform-ssh")]
fn format_i2c_service_error(error: &I2cHelperServiceError) -> String {
    match error {
        I2cHelperServiceError::Helper(failure) => format!(
            "I2C helper failure: code={}, message={}, transaction={:?}, message={:?}",
            failure.code, failure.message, failure.transaction_index, failure.message_index
        ),
        _ => error.to_string(),
    }
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn format_eeprom_service_error(error: &EepromProvisionServiceError) -> String {
    match error {
        EepromProvisionServiceError::Helper(failure) => format!(
            "EEPROM helper failure: code={}, message={}, rollback={:?}, rollback_error={}",
            failure.code,
            failure.message,
            failure.rollback,
            failure.rollback_error.as_deref().unwrap_or("none")
        ),
        _ => error.to_string(),
    }
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
const EEPROM_FLAG_OFFSET: u16 = 0x0000;
#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
const EEPROM_CALIBRATION_OFFSET: u16 = 0x0010;
#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
const EEPROM_SERIAL_OFFSET: u16 = 0x0125;
#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
const EEPROM_SERIAL_BYTES: usize = 14;

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_failure_json(error: &EepromProvisionServiceError) -> serde_json::Value {
    match error {
        EepromProvisionServiceError::Helper(failure) => serde_json::json!({
            "kind": "helper",
            "code": failure.code,
            "message": failure.message,
            "before": failure.before,
            "backup_sha256": SnapshotHash::digest_bytes(&failure.backup).to_hex(),
            "backup_bytes": failure.backup.len(),
            "rollback": failure.rollback,
            "rollback_error": failure.rollback_error,
        }),
        _ => serde_json::json!({
            "kind": "service",
            "message": error.to_string(),
        }),
    }
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_write_result_json(result: &EepromWriteResult) -> serde_json::Value {
    serde_json::json!({
        "before": result.before,
        "after": result.after,
        "backup_sha256": SnapshotHash::digest_bytes(&result.backup).to_hex(),
        "backup_bytes": result.backup.len(),
        "page_plan": result.page_plan,
        "bytewise_verified": result.bytewise_verified,
        "rollback": result.rollback,
    })
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_target_json(target: &EepromProvisioningTarget) -> serde_json::Value {
    serde_json::json!({
        "label": target.label,
        "i2c_bus": target.i2c_bus,
        "target_snapshot_sha256": target.snapshot_hash.to_hex(),
    })
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_action_parameters_json(
    action: &camera_toolbox_app::EepromHelperAction,
) -> serde_json::Value {
    match action {
        camera_toolbox_app::EepromHelperAction::Inspect => serde_json::json!({
            "action": "inspect",
        }),
        camera_toolbox_app::EepromHelperAction::Provision {
            request,
            expected_before_sha256,
        } => serde_json::json!({
            "action": "provision",
            "expected_before_sha256": expected_before_sha256,
            "request": eeprom_request_parameters_json(request),
        }),
    }
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_request_parameters_json(
    request: &camera_toolbox_core::EepromProvisionRequest,
) -> serde_json::Value {
    let calibration_parameters = eeprom_calibration_parameters_json(request);
    serde_json::json!({
        "map_id": request.map_id,
        "mode": request.mode,
        "serial_number": request.serial_number,
        "overwrite_existing_serial": request.overwrite_existing_serial,
        "snid": eeprom_snid_json(&request.serial_number),
        "calibration_parameters": calibration_parameters,
        "write_segments": request.segments.iter().map(eeprom_write_segment_json).collect::<Vec<_>>(),
    })
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_write_segment_json(
    segment: &camera_toolbox_core::EepromWriteSegment,
) -> serde_json::Value {
    serde_json::json!({
        "offset": format!("0x{:04x}", segment.offset),
        "offset_u16": segment.offset,
        "byte_len": segment.bytes.len(),
        "purpose": eeprom_segment_purpose(segment.offset),
        "payload_sha256": SnapshotHash::digest_bytes(&segment.bytes).to_hex(),
        "semantic_value": eeprom_segment_semantic_json(segment),
    })
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_segment_purpose(offset: u16) -> &'static str {
    match offset {
        EEPROM_FLAG_OFFSET => "valid_flag",
        EEPROM_CALIBRATION_OFFSET => "calibration_parameters",
        EEPROM_SERIAL_OFFSET => "serial_number_and_checksum",
        _ => "custom_segment",
    }
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_segment_semantic_json(
    segment: &camera_toolbox_core::EepromWriteSegment,
) -> serde_json::Value {
    match segment.offset {
        EEPROM_FLAG_OFFSET if segment.bytes == b"hessian\0" => serde_json::json!({
            "flag_ascii": "hessian\\0",
        }),
        EEPROM_CALIBRATION_OFFSET => {
            decode_calibration_segment_json(&segment.bytes).unwrap_or(serde_json::Value::Null)
        }
        EEPROM_SERIAL_OFFSET if segment.bytes.len() >= EEPROM_SERIAL_BYTES => {
            let serial = std::str::from_utf8(&segment.bytes[..EEPROM_SERIAL_BYTES]).ok();
            serde_json::json!({
                "serial_number": serial,
                "snid": serial.map(eeprom_snid_json),
                "checksum_u8": segment.bytes.get(EEPROM_SERIAL_BYTES).copied(),
                "checksum_hex": segment.bytes.get(EEPROM_SERIAL_BYTES).map(|value| format!("0x{value:02x}")),
            })
        }
        _ => serde_json::Value::Null,
    }
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_calibration_parameters_json(
    request: &camera_toolbox_core::EepromProvisionRequest,
) -> serde_json::Value {
    request
        .segments
        .iter()
        .find(|segment| segment.offset == EEPROM_CALIBRATION_OFFSET)
        .and_then(|segment| decode_calibration_segment_json(&segment.bytes))
        .unwrap_or(serde_json::Value::Null)
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn decode_calibration_segment_json(bytes: &[u8]) -> Option<serde_json::Value> {
    let width = read_u32_le(bytes, 0)?;
    let height = read_u32_le(bytes, 4)?;
    let fx = read_f32_le(bytes, 8)?;
    let fy = read_f32_le(bytes, 12)?;
    let cx = read_f32_le(bytes, 16)?;
    let cy = read_f32_le(bytes, 20)?;
    let distortion = (0..12)
        .map(|index| read_f32_le(bytes, 24 + index * 4))
        .collect::<Option<Vec<_>>>()?;
    Some(serde_json::json!({
        "image_size": {
            "width": width,
            "height": height,
        },
        "camera_matrix": {
            "fx": fx,
            "fy": fy,
            "cx": cx,
            "cy": cy,
            "matrix_3x3": [
                [fx, 0.0, cx],
                [0.0, fy, cy],
                [0.0, 0.0, 1.0],
            ],
        },
        "distortion": {
            "model": "opencv_pinhole_radtan_thin_prism_d12",
            "coefficients": {
                "k1": distortion[0],
                "k2": distortion[1],
                "p1": distortion[2],
                "p2": distortion[3],
                "k3": distortion[4],
                "k4": distortion[5],
                "k5": distortion[6],
                "k6": distortion[7],
                "s1": distortion[8],
                "s2": distortion[9],
                "s3": distortion[10],
                "s4": distortion[11],
            },
        },
    }))
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn read_f32_le(bytes: &[u8], offset: usize) -> Option<f32> {
    Some(f32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_snid_json(serial_number: &str) -> serde_json::Value {
    let bytes = serial_number.as_bytes();
    if bytes.len() != EEPROM_SERIAL_BYTES {
        return serde_json::json!({
            "raw": serial_number,
            "decoded": null,
            "error": "SNID must be 14 ASCII bytes",
        });
    }
    let module = std::str::from_utf8(&bytes[2..5]).unwrap_or("");
    let year = std::str::from_utf8(&bytes[5..7]).unwrap_or("");
    let sequence = decode_snid_sequence(bytes[10], bytes[11]);
    serde_json::json!({
        "resolution": {
            "code": char::from(bytes[0]).to_string(),
            "meaning": if bytes[0] == b'2' { "FHD" } else { "unknown" },
        },
        "vendor": {
            "code": char::from(bytes[1]).to_string(),
            "meaning": if bytes[1] == b'T' { "SmartSens" } else { "unknown" },
        },
        "module": module,
        "year": year,
        "month": {
            "input_decimal": decode_snid_month(bytes[7]),
            "encoded": char::from(bytes[7]).to_string(),
        },
        "day": {
            "input_decimal": decode_snid_day(bytes[8]),
            "encoded": char::from(bytes[8]).to_string(),
        },
        "optical_axis_class": {
            "input": decode_ascii_digit(bytes[9]),
            "encoded": char::from(bytes[9]).to_string(),
        },
        "sequence": {
            "input_decimal": sequence,
            "encoded_high": char::from(bytes[10]).to_string(),
            "encoded_low": char::from(bytes[11]).to_string(),
        },
        "algorithm_version": char::from(bytes[12]).to_string(),
        "reserved": char::from(bytes[13]).to_string(),
        "checksum_expected": {
            "offset": "0x0133",
            "algorithm": "sum(serial_bytes) % 0xff + 1",
            "value_u8": serial_checksum_value(bytes),
            "value_hex": format!("0x{:02x}", serial_checksum_value(bytes)),
        },
    })
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn decode_ascii_digit(byte: u8) -> Option<u8> {
    byte.is_ascii_digit().then_some(byte - b'0')
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn decode_snid_month(byte: u8) -> Option<u8> {
    match byte {
        b'1'..=b'9' => Some(byte - b'0'),
        b'A'..=b'C' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn decode_snid_day(byte: u8) -> Option<u8> {
    match byte {
        b'1'..=b'9' => Some(byte - b'0'),
        b'A'..=b'V' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn decode_base62_digit(byte: u8) -> Option<u16> {
    match byte {
        b'0'..=b'9' => Some(u16::from(byte - b'0')),
        b'a'..=b'z' => Some(u16::from(byte - b'a') + 10),
        b'A'..=b'Z' => Some(u16::from(byte - b'A') + 36),
        _ => None,
    }
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn decode_snid_sequence(high: u8, low: u8) -> Option<u16> {
    Some(decode_base62_digit(high)? * 62 + decode_base62_digit(low)? + 1)
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn serial_checksum_value(bytes: &[u8]) -> u8 {
    ((bytes.iter().map(|byte| u16::from(*byte)).sum::<u16>() % 0xff) + 1) as u8
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_history_path(serial_number: &str) -> Result<PathBuf, String> {
    let file_name = safe_eeprom_history_file_name(serial_number)?;
    Ok(PathBuf::from("write_history").join(file_name))
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn legacy_eeprom_history_path(serial_number: &str) -> Result<PathBuf, String> {
    Ok(PathBuf::from("write_history")
        .join(format!("{}.json", safe_eeprom_history_stem(serial_number)?)))
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn safe_eeprom_history_stem(serial_number: &str) -> Result<String, String> {
    let serial = serial_number.trim();
    if serial.is_empty() {
        return Err("EEPROM serial number is empty; cannot create write history file".to_owned());
    }
    if !serial
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!(
            "EEPROM serial number {serial:?} cannot be used as a write history filename"
        ));
    }
    Ok(serial.to_owned())
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn safe_eeprom_history_file_name(serial_number: &str) -> Result<String, String> {
    Ok(format!("{}.yaml", safe_eeprom_history_stem(serial_number)?))
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn ensure_eeprom_history_slot_available(serial_number: &str) -> Result<(), String> {
    new_eeprom_history_path(serial_number).map(|_| ())
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn new_eeprom_history_path(serial_number: &str) -> Result<PathBuf, String> {
    let serial = safe_eeprom_history_stem(serial_number)?;
    let default_path = eeprom_history_path(&serial)?;
    let legacy_path = legacy_eeprom_history_path(&serial)?;
    let default_name = eeprom_file_name_to_string(&default_path)?;
    let legacy_name = eeprom_file_name_to_string(&legacy_path)?;
    let history_dir = Path::new("write_history");
    let entries = match fs::read_dir(history_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(default_path),
        Err(error) => {
            return Err(format!(
                "Failed to inspect EEPROM write history directory {} before writing SN {serial}: {error}",
                history_dir.display()
            ));
        }
    };
    let mut existing_names = Vec::new();
    let mut default_name_collides = false;

    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to inspect EEPROM write history directory {} before writing SN {serial}: {error}",
                history_dir.display()
            )
        })?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str().map(str::to_owned) else {
            continue;
        };
        let path = entry.path();
        if eeprom_history_may_record_snid(&file_name)
            && eeprom_history_recorded_serial_number(&path).as_deref() == Some(serial.as_str())
        {
            return Err(format!(
                "Write history already records SN {serial}: {}. Refusing to start EEPROM write; rename or archive the existing file before retrying.",
                path.display()
            ));
        }
        if file_name.eq_ignore_ascii_case(&default_name)
            || file_name.eq_ignore_ascii_case(&legacy_name)
        {
            default_name_collides = true;
        }
        existing_names.push(file_name);
    }

    if !default_name_collides {
        return Ok(default_path);
    }

    // Windows 可能用大小写不敏感方式占用默认文件名；非重复 SNID 改用稳定后缀保证审计可落盘。
    let suffix = eeprom_history_stem_hex_suffix(&serial);
    for index in 0..1000_u16 {
        let file_name = if index == 0 {
            format!("{serial}--{suffix}.yaml")
        } else {
            format!("{serial}--{suffix}-{index}.yaml")
        };
        if !existing_names
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&file_name))
        {
            return Ok(history_dir.join(file_name));
        }
    }

    Err(format!(
        "No available EEPROM write history filename remains for SN {serial}; archive colliding history files before retrying."
    ))
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_file_name_to_string(path: &Path) -> Result<String, String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "EEPROM write history path {} has no UTF-8 file name",
                path.display()
            )
        })
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_history_may_record_snid(file_name: &str) -> bool {
    file_name.ends_with(".yaml") || file_name.ends_with(".json")
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_history_stem_hex_suffix(serial: &str) -> String {
    serial
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn eeprom_history_recorded_serial_number(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let document: serde_json::Value = serde_yaml::from_slice(&bytes).ok()?;
    // Windows 目录可按大小写不敏感方式命中文件名；重复判断只信审计内容里的原始 SNID。
    document
        .pointer("/request/request/serial_number")
        .or_else(|| document.pointer("/request/request/snid/raw"))
        .or_else(|| document.pointer("/request/serial_number"))
        .or_else(|| document.pointer("/request/snid/raw"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn path_parent_or_current(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh", not(windows)))]
fn sync_directory(path: &Path, label: &str) -> Result<(), String> {
    let directory = std::fs::File::open(path).map_err(|error| {
        format!(
            "failed to open {label} directory {} for sync: {error}",
            path.display()
        )
    })?;
    directory.sync_all().map_err(|error| {
        format!(
            "failed to sync {label} directory {}: {error}",
            path.display()
        )
    })
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh", windows))]
fn sync_directory(_path: &Path, _label: &str) -> Result<(), String> {
    // Windows 标准库不能用 File::open 打开目录做 sync；文件本身已 sync_all。
    Ok(())
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn ensure_directory_durable(path: &Path, label: &str) -> Result<(), String> {
    let existed = path.exists();
    fs::create_dir_all(path).map_err(|error| {
        format!(
            "failed to create {label} directory {}: {error}",
            path.display()
        )
    })?;
    if !existed {
        // 新目录入口在父目录中，必须同步父目录后才能称为持久化。
        sync_directory(path_parent_or_current(path), label)?;
    }
    Ok(())
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn create_new_file(
    path: &Path,
    bytes: &[u8],
    operation_id: u64,
    label: &str,
) -> Result<(), String> {
    let parent = path.parent().map(Path::to_path_buf);
    if let Some(parent) = parent.as_deref() {
        ensure_directory_durable(parent, label)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            format!(
                "failed to create {label} for operation {operation_id} at {}: {error}",
                path.display()
            )
        })?;
    std::io::Write::write_all(&mut file, bytes).map_err(|error| {
        format!(
            "failed to write {label} for operation {operation_id} at {}: {error}",
            path.display()
        )
    })?;
    file.sync_all().map_err(|error| {
        format!(
            "failed to sync {label} for operation {operation_id} at {}: {error}",
            path.display()
        )
    })?;
    if let Some(parent) = parent.as_deref() {
        sync_directory(parent, label)?;
    }
    Ok(())
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn serialize_eeprom_yaml(document: &serde_json::Value) -> Result<Vec<u8>, String> {
    let mut text = serde_yaml::to_string(document).map_err(|error| error.to_string())?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text.into_bytes())
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
fn persist_eeprom_write_history_yaml(
    serial_number: &str,
    operation_id: u64,
    document: &serde_json::Value,
) -> Result<String, String> {
    let path = new_eeprom_history_path(serial_number)?;
    let bytes = serialize_eeprom_yaml(document)?;
    create_new_file(&path, &bytes, operation_id, "EEPROM write history")?;
    Ok(path.display().to_string())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ProductWorkspace {
    #[default]
    Viewer,
    Color,
    #[cfg(feature = "platform-ssh")]
    I2cTools,
    #[cfg(feature = "calibration-opencv")]
    Calibration,
}

#[derive(Debug, Clone)]
struct DirectRtspWorkspace {
    url: String,
    channel: u16,
    width: u32,
    height: u32,
    codec: RtspCodec,
    transport: RtspTransport,
    latency_mode: RtspLatencyMode,
    prefer_hardware_acceleration: bool,
    last_error: Option<String>,
}

impl Default for DirectRtspWorkspace {
    fn default() -> Self {
        Self {
            url: "rtsp://".to_owned(),
            channel: 0,
            width: 1920,
            height: 1080,
            codec: RtspCodec::H264,
            transport: RtspTransport::Tcp,
            latency_mode: RtspLatencyMode::Stable,
            prefer_hardware_acceleration: false,
            last_error: None,
        }
    }
}

pub(crate) struct CameraToolboxApp {
    product_workspace: ProductWorkspace,
    #[cfg(feature = "calibration-opencv")]
    calibration: CalibrationWorkspace,
    workspace: WorkspaceState,
    color_inspection: ColorInspectionWorkspace,
    auto_open: AutoOpenCoordinator,
    explorer: ExplorerState,
    explorer_panel_expanded: bool,
    direct_rtsp: DirectRtspWorkspace,
    empty_viewer: ImageViewerState,
    raw_dialog: RawOpenDialogState,
    yuv_save_dialog: YuvSaveDialogState,
    export_name_dialog: ExportNameDialogState,
    raw_pipeline: RawOpenPipeline,
    image_pipeline: ImageOpenPipeline,
    color_worker: ColorRenderWorker,
    analysis_worker: AnalysisWorker,
    spatial_worker: SpatialHighlightWorker,
    save_worker: ImageSaveWorker,
    color_export_sender: Sender<ColorExportResult>,
    color_export_receiver: Receiver<ColorExportResult>,
    #[cfg(feature = "calibration-opencv")]
    calibration_export_sender: Sender<CalibrationExportResult>,
    #[cfg(feature = "calibration-opencv")]
    calibration_export_receiver: Receiver<CalibrationExportResult>,
    #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
    eeprom_target: Option<EepromProvisioningTarget>,
    #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
    eeprom_operation_sender: Sender<EepromOperationResult>,
    #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
    eeprom_operation_receiver: Receiver<EepromOperationResult>,
    #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
    active_eeprom_cancellation: Option<DumpCancellation>,
    #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
    active_eeprom_cancellable: bool,
    #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
    next_eeprom_operation: u64,
    #[cfg(feature = "platform-ssh")]
    i2c_tools: I2cToolsWorkspace,
    #[cfg(feature = "platform-ssh")]
    i2c_tools_sender: Sender<I2cToolsOperationResult>,
    #[cfg(feature = "platform-ssh")]
    i2c_tools_receiver: Receiver<I2cToolsOperationResult>,
    #[cfg(feature = "platform-ssh")]
    active_i2c_tools_cancellation: Option<DumpCancellation>,
    #[cfg(feature = "platform-ssh")]
    active_i2c_tools_cancellable: bool,
    notifications: NotificationCenter,
    next_generation: u64,
    live_runtime: LiveRuntime,
    next_load_attempt: u64,
    pending_ephemeral_close: Option<DocumentId>,
    raw_open_sender: Sender<RawOpenJobEvent>,
    raw_open_receiver: Receiver<RawOpenJobEvent>,
    active_raw_open: Option<ActiveRawOpenJob>,
    pending_auto_open: VecDeque<PendingAutoOpenRequest>,
    pending_yuv_save: Option<PendingYuvSave>,
    pending_named_export: Option<PendingNamedExport>,
    auto_open_sender: Sender<AutoOpenJobResult>,
    auto_open_receiver: Receiver<AutoOpenJobResult>,
    active_auto_open: Option<ActiveAutoOpenJob>,
    auto_open_documents: BTreeMap<String, DocumentId>,
    auto_open_background_tabs: VecDeque<DocumentId>,
    reinterpret_sender: Sender<ReinterpretJobResult>,
    reinterpret_receiver: Receiver<ReinterpretJobResult>,
    yuv_reinterpret_sender: Sender<YuvReinterpretJobResult>,
    yuv_reinterpret_receiver: Receiver<YuvReinterpretJobResult>,
}

impl CameraToolboxApp {
    pub(crate) fn new(context: &egui::Context) -> std::io::Result<Self> {
        let cache = SourceCache::new(RAW_SOURCE_CACHE_BYTES, RAW_SOURCE_CACHE_ENTRIES)
            .map_err(std::io::Error::other)?;
        let codec: Arc<dyn RasterImageCodec> = Arc::new(ImageRasterCodec);
        let raw_pipeline = RawOpenPipeline::new(cache, Vec::new(), 256 * 1024 * 1024);
        let image_pipeline = ImageOpenPipeline::new(
            raw_pipeline.clone(),
            Arc::clone(&codec),
            256 * 1024 * 1024,
            256 * 1024 * 1024,
        );
        let workspace_settings = WorkspaceSettings::default();
        let (raw_open_sender, raw_open_receiver) = mpsc::channel();
        let (reinterpret_sender, reinterpret_receiver) = mpsc::channel();
        let (yuv_reinterpret_sender, yuv_reinterpret_receiver) = mpsc::channel();
        let (auto_open_sender, auto_open_receiver) = mpsc::channel();
        let (color_export_sender, color_export_receiver) = mpsc::channel();
        #[cfg(feature = "calibration-opencv")]
        let (calibration_export_sender, calibration_export_receiver) = mpsc::channel();
        #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
        let (eeprom_operation_sender, eeprom_operation_receiver) = mpsc::channel();
        #[cfg(feature = "platform-ssh")]
        let (i2c_tools_sender, i2c_tools_receiver) = mpsc::channel();
        let live_runtime = LiveRuntime::new().map_err(std::io::Error::other)?;
        #[cfg(feature = "platform-ssh")]
        let explorer =
            ExplorerState::new(live_runtime.ssh_resolver(), Arc::new(RusshTransportFactory));
        #[cfg(not(feature = "platform-ssh"))]
        let explorer = ExplorerState::new();
        let auto_open = AutoOpenCoordinator::from_settings(&workspace_settings, &explorer);
        Ok(Self {
            product_workspace: ProductWorkspace::Viewer,
            #[cfg(feature = "calibration-opencv")]
            calibration: CalibrationWorkspace::new(context)?,
            workspace: WorkspaceState::default(),
            color_inspection: ColorInspectionWorkspace::default(),
            explorer,
            auto_open,
            explorer_panel_expanded: false,
            direct_rtsp: DirectRtspWorkspace::default(),
            empty_viewer: ImageViewerState::default(),
            raw_dialog: RawOpenDialogState::default(),
            yuv_save_dialog: YuvSaveDialogState::default(),
            export_name_dialog: ExportNameDialogState::default(),
            raw_pipeline,
            image_pipeline,
            color_worker: ColorRenderWorker::new(context)?,
            analysis_worker: AnalysisWorker::new(context)?,
            spatial_worker: SpatialHighlightWorker::new(context)?,
            save_worker: ImageSaveWorker::new(context, codec)?,
            color_export_sender,
            color_export_receiver,
            #[cfg(feature = "calibration-opencv")]
            calibration_export_sender,
            #[cfg(feature = "calibration-opencv")]
            calibration_export_receiver,
            #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
            eeprom_target: None,
            #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
            eeprom_operation_sender,
            #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
            eeprom_operation_receiver,
            #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
            active_eeprom_cancellation: None,
            #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
            active_eeprom_cancellable: false,
            #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
            next_eeprom_operation: 1,
            #[cfg(feature = "platform-ssh")]
            i2c_tools: I2cToolsWorkspace::default(),
            #[cfg(feature = "platform-ssh")]
            i2c_tools_sender,
            #[cfg(feature = "platform-ssh")]
            i2c_tools_receiver,
            #[cfg(feature = "platform-ssh")]
            active_i2c_tools_cancellation: None,
            #[cfg(feature = "platform-ssh")]
            active_i2c_tools_cancellable: false,
            notifications: NotificationCenter::default(),
            next_generation: 1,
            live_runtime,
            next_load_attempt: 1,
            pending_ephemeral_close: None,
            raw_open_sender,
            raw_open_receiver,
            active_raw_open: None,
            pending_auto_open: VecDeque::new(),
            pending_yuv_save: None,
            pending_named_export: None,
            auto_open_sender,
            auto_open_receiver,
            active_auto_open: None,
            auto_open_documents: BTreeMap::new(),
            auto_open_background_tabs: VecDeque::new(),
            reinterpret_sender,
            reinterpret_receiver,
            yuv_reinterpret_sender,
            yuv_reinterpret_receiver,
        })
    }
}

impl eframe::App for CameraToolboxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.poll_color_result(&context);
        self.poll_analysis_result();
        self.poll_spatial_highlight_result();
        self.poll_save_result(&context);
        self.poll_color_export_result(&context);
        #[cfg(feature = "calibration-opencv")]
        self.poll_calibration_export_result(&context);
        #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
        self.poll_eeprom_operation_result(&context);
        #[cfg(feature = "platform-ssh")]
        self.poll_i2c_tools_result();
        self.poll_raw_open_result(&context);
        self.poll_auto_open_result(&context);
        self.enqueue_auto_open_candidates();
        self.dispatch_auto_open(&context);
        self.poll_reinterpret_result(&context);
        self.poll_yuv_reinterpret_result(&context);
        self.poll_stream_events();
        for effect in self.live_runtime.poll_platform_events() {
            self.handle_platform_effect(&context, effect);
        }
        self.advance_live_close_deadlines();
        #[cfg(feature = "calibration-opencv")]
        let displayed_live_frame = if let Some(document) = self.workspace.active_live_mut() {
            document.install_latest_texture(&context);
            document.displayed_frame().cloned().map(|frame| {
                (
                    frame,
                    document.source.clone(),
                    document.show_calibration_detection,
                )
            })
        } else {
            None
        };
        #[cfg(not(feature = "calibration-opencv"))]
        if let Some(document) = self.workspace.active_live_mut() {
            document.install_latest_texture(&context);
        }
        #[cfg(feature = "calibration-opencv")]
        {
            self.calibration.tick(&context);
            if let Some((frame, source, preview_requested)) = displayed_live_frame {
                self.calibration.observe_live_frame(
                    frame,
                    source,
                    self.live_runtime.capture_store().clone(),
                    preview_requested,
                );
            }
        }
        self.ensure_active_resources(&context);
        if let Some(document) = self.workspace.active_image_mut() {
            if let Err(error) = document.ensure_texture(&context) {
                tracing::error!(
                    operation = "install_static_image_texture",
                    document_id = %document.id,
                    error = %error,
                    "failed to install static image texture"
                );
            }
        }
        self.ensure_analysis();
        if let Some(document) = self.workspace.active_mut() {
            let native = NativeImage::Raw(Arc::clone(&document.loaded.frame));
            document.viewer.refresh_cursor(&context, &native);
        }
        if let Some(document) = self.workspace.active_image_mut() {
            document.viewer.refresh_cursor(&context, &document.native);
        }

        egui::Panel::top("menu_bar").show(ui, |ui| self.render_menu_bar(ui));
        if !self.is_calibration_workspace() {
            let tab_action = egui::Panel::top("document_tabs")
                .resizable(false)
                .show(ui, |ui| render_tab_bar(ui, &self.workspace))
                .inner;
            if let Some(action) = tab_action {
                self.handle_tab_action(&context, action);
            }
        }
        let calibration_workspace = self.is_calibration_workspace();
        let (direct_rtsp_config, explorer_action, workspace_stream_action) = if self
            .explorer_panel_expanded
        {
            let mut collapse = false;
            let actions = egui::Panel::left("workspace_explorer_panel")
                .resizable(true)
                .default_size(280.0)
                .min_size(220.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("Workspace");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("‹").on_hover_text("Collapse Workspace").clicked() {
                                collapse = true;
                            }
                        });
                    });
                    ui.separator();
                    let explorer_action = self.explorer.render(&context, ui, calibration_workspace);
                    let (direct_rtsp_config, stream_action) = if self.explorer.is_rtsp_mode() {
                        egui::ScrollArea::vertical()
                            .id_salt("workspace_rtsp_sidebar_scroll")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.separator();
                                let direct_rtsp_config = self.render_direct_rtsp_workspace(ui);
                                ui.separator();
                                let stream_action = self.render_workspace_stream_section(ui);
                                self.render_stream_metrics(ui);
                                (direct_rtsp_config, stream_action)
                            })
                            .inner
                    } else {
                        (None, None)
                    };
                    (direct_rtsp_config, explorer_action, stream_action)
                })
                .inner;
            if collapse {
                self.explorer_panel_expanded = false;
            }
            actions
        } else {
            let expand = egui::Panel::left("workspace_explorer_panel_rail")
                .resizable(false)
                .min_size(32.0)
                .max_size(32.0)
                .show(ui, |ui| {
                    ui.button("›").on_hover_text("Expand Workspace").clicked()
                })
                .inner;
            if expand {
                self.explorer_panel_expanded = true;
            }
            (None, None, None)
        };
        if let Some(action) = explorer_action {
            self.handle_explorer_action(&context, action);
        }
        if let Some(config) = direct_rtsp_config {
            self.start_direct_rtsp(config);
        }
        if let Some(action) = workspace_stream_action {
            self.handle_workspace_stream_action(&context, action);
        }
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            #[cfg(feature = "calibration-opencv")]
            if self.is_calibration_workspace() {
                self.calibration.render_status(ui);
                return;
            }
            self.render_status_bar(ui);
        });
        if self.product_workspace == ProductWorkspace::Viewer {
            self.render_analysis_panel_ui(ui);
            self.render_color_panel(ui);
            self.render_yuv_inspector_panel(ui);
        }
        let color_action = if self.is_color_workspace() {
            let active_label = self
                .workspace
                .active_image()
                .filter(|document| document.is_png_workspace_file() || document.is_color_capture())
                .map(|document| document.title.clone());
            let can_analyze = active_label.is_some();
            let can_capture_rtsp = self
                .workspace
                .active_live()
                .is_some_and(|document| document.displayed_frame().is_some());
            egui::Panel::right("color_inspection_panel")
                .resizable(true)
                .default_size(360.0)
                .min_size(280.0)
                .show(ui, |ui| {
                    self.color_inspection.render_right_panel(
                        ui,
                        active_label.as_deref(),
                        can_analyze,
                        can_capture_rtsp,
                    )
                })
                .inner
        } else {
            None
        };
        if let Some(action) = color_action {
            self.handle_color_inspection_action(&context, action);
        }

        #[cfg(feature = "platform-ssh")]
        let i2c_tools_sftp_label: Result<String, String> = self
            .explorer
            .connected_sftp_label()
            .map(str::to_owned)
            .ok_or_else(|| "Connect Explorer SFTP before using I²C Tools.".to_owned());
        #[cfg(feature = "platform-ssh")]
        let mut i2c_tools_action = None;
        #[cfg(feature = "calibration-opencv")]
        let calibration_sftp_label: Result<String, String> = {
            #[cfg(feature = "platform-ssh")]
            {
                self.explorer
                    .connected_sftp_label()
                    .map(str::to_owned)
                    .ok_or_else(|| "Connect Explorer SFTP before configuring EEPROM.".to_owned())
            }
            #[cfg(not(feature = "platform-ssh"))]
            {
                Err("This build does not include Explorer SFTP.".to_owned())
            }
        };
        #[cfg(feature = "calibration-opencv")]
        let calibration_provision_label: Result<String, String> = {
            #[cfg(feature = "platform-ssh")]
            {
                self.eeprom_target
                    .as_ref()
                    .map(|target| target.label.clone())
                    .ok_or_else(|| {
                        "Use the connected Explorer SFTP source for EEPROM, then Inspect."
                            .to_owned()
                    })
            }
            #[cfg(not(feature = "platform-ssh"))]
            {
                Err("This build does not include SSH EEPROM provisioning.".to_owned())
            }
        };
        let calibration_export_error = self.explorer.export_dialog_prefill(&context).err();

        #[cfg(feature = "calibration-opencv")]
        let calibration_viewer_presentation = self.workspace.active_live().and_then(|document| {
            self.calibration.live_viewer_presentation(
                document.displayed_frame().map(Arc::as_ref),
                Some(&document.source),
            )
        });
        let color_workspace = self.is_color_workspace();
        let viewer_output = egui::CentralPanel::default()
            .show(ui, |ui| {
                #[cfg(feature = "platform-ssh")]
                if self.product_workspace == ProductWorkspace::I2cTools {
                    let rect = ui.available_rect_before_wrap();
                    i2c_tools_action = self.i2c_tools.render(
                        ui,
                        i2c_tools_sftp_label
                            .as_ref()
                            .map(|label| label.as_str())
                            .map_err(|error| error.as_str()),
                    );
                    return ViewerOutput { rect, action: None };
                }
                #[cfg(feature = "calibration-opencv")]
                if self.is_calibration_workspace() {
                    let has_live_inspection = self.workspace.active_live().is_some();
                    let workspace = &mut self.workspace;
                    let (rect, _) = self.calibration.render(
                        &context,
                        ui,
                        calibration_export_error.is_none(),
                        calibration_export_error.as_deref(),
                        calibration_sftp_label
                            .as_ref()
                            .map(|label| label.as_str())
                            .map_err(|error| error.as_str()),
                        calibration_provision_label
                            .as_ref()
                            .map(|label| label.as_str())
                            .map_err(|error| error.as_str()),
                        has_live_inspection,
                        |ui| {
                            let document = workspace.active_live_mut()?;
                            let _ = Self::render_live_viewer(
                                ui,
                                document,
                                true,
                                calibration_viewer_presentation.as_ref(),
                            );
                            None
                        },
                    );
                    return ViewerOutput { rect, action: None };
                }
                if let Some(document) = self.workspace.active_live_mut() {
                    let (rect, _) = Self::render_live_viewer(
                        ui,
                        document,
                        cfg!(feature = "calibration-opencv"),
                        #[cfg(feature = "calibration-opencv")]
                        calibration_viewer_presentation.as_ref(),
                    );
                    ViewerOutput { rect, action: None }
                } else if let Some(document) = self.workspace.active_image_mut() {
                    let document_id = document.id;
                    let generation = document.generation;
                    let dimensions = document.native.dimensions();
                    let image = document.display.texture_id().map(|texture_id| {
                        if color_workspace {
                            ViewerImage::native_without_roi(
                                document.generation,
                                document.native.clone(),
                                texture_id,
                            )
                        } else {
                            ViewerImage::native(
                                document.generation,
                                document.native.clone(),
                                texture_id,
                                document.roi,
                            )
                        }
                    });
                    let output = render_viewer(
                        ui,
                        image,
                        &mut document.viewer,
                        document.hover_view,
                        document.spatial_highlight.as_ref(),
                    );
                    if color_workspace
                        && let Some(analysis) = self
                            .color_inspection
                            .analysis_for_overlay(document_id, generation)
                        && let Some(image_rect) = document.viewer.displayed_image_rect()
                    {
                        paint_color_chart_overlay(
                            ui.painter(),
                            image_rect,
                            dimensions,
                            analysis,
                            document.viewer.horizontal_flip,
                            self.color_inspection.selected_patch_index(),
                        );
                    }
                    if color_workspace
                        && let Some(points) = self
                            .color_inspection
                            .manual_corners_for_overlay(document_id, generation)
                        && let Some(image_rect) = document.viewer.displayed_image_rect()
                    {
                        paint_manual_corner_overlay(
                            ui.painter(),
                            image_rect,
                            dimensions,
                            points,
                            document.viewer.horizontal_flip,
                        );
                    }
                    output
                } else if let Some(document) = self.workspace.active_mut() {
                    let image = ViewerImage::raw(&document.loaded, document.display_mode);
                    render_viewer(
                        ui,
                        Some(image),
                        &mut document.viewer,
                        document.hover_view,
                        document.spatial_highlight.as_ref(),
                    )
                } else {
                    render_viewer(
                        ui,
                        None,
                        &mut self.empty_viewer,
                        HoverViewSettings::default(),
                        None,
                    )
                }
            })
            .inner;
        if let Some(action) = viewer_output.action {
            self.handle_viewer_action(action);
        }
        #[cfg(feature = "platform-ssh")]
        if let Some(action) = i2c_tools_action {
            self.begin_i2c_tools_operation(&context, action);
        }
        #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
        if let Some(intent) = self.calibration.take_provision_intent() {
            self.begin_eeprom_operation(&context, intent);
        }
        #[cfg(feature = "calibration-opencv")]
        if let Some(export) = self.calibration.take_export() {
            self.begin_calibration_export(&context, export);
        }
        if let Some(export) = self.color_inspection.take_export() {
            self.begin_color_export(&context, export);
        }
        self.render_named_export_dialog(&context);
        self.render_yuv_save_dialog(&context);
        self.notifications.render(
            &context,
            viewer_output.rect,
            context.input(|input| input.time),
        );
        self.render_raw_open_dialog(&context);
        self.render_pending_ephemeral_close(&context);
        self.ensure_analysis();
        self.workspace.enforce_derived_budget();
        self.workspace.release_inactive_live_textures();
        if !self.workspace.live_documents().is_empty() {
            context.request_repaint_after(Duration::from_millis(33));
        } else {
            context.request_repaint_after(Duration::from_millis(100));
        }
    }
}

impl CameraToolboxApp {
    fn is_calibration_workspace(&self) -> bool {
        #[cfg(feature = "calibration-opencv")]
        {
            self.product_workspace == ProductWorkspace::Calibration
        }
        #[cfg(not(feature = "calibration-opencv"))]
        {
            false
        }
    }

    fn is_color_workspace(&self) -> bool {
        self.product_workspace == ProductWorkspace::Color
    }

    fn render_product_workspace_switch(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.product_workspace,
                ProductWorkspace::Viewer,
                "Viewer",
            );
            ui.selectable_value(
                &mut self.product_workspace,
                ProductWorkspace::Color,
                "Color",
            );
            #[cfg(feature = "platform-ssh")]
            ui.selectable_value(
                &mut self.product_workspace,
                ProductWorkspace::I2cTools,
                "I²C Tools",
            );
            #[cfg(feature = "calibration-opencv")]
            ui.selectable_value(
                &mut self.product_workspace,
                ProductWorkspace::Calibration,
                "Calibration",
            );
        });
    }

    fn render_menu_bar(&mut self, ui: &mut egui::Ui) {
        let mut request_color = false;
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| self.render_file_menu(ui));
            ui.menu_button("View", |ui| {
                request_color |= self.render_view_menu(ui);
            });
            ui.menu_button("Tools", |ui| self.render_tools_menu(ui));
            ui.menu_button("Help", |ui| {
                ui.label("Camera Toolbox");
                ui.label("Local Bayer RAW color viewer");
                ui.separator();
                ui.label("Log directory");
                if let Some(directory) = camera_toolbox_logging::logging_directory() {
                    let path = directory.display().to_string();
                    ui.monospace(&path);
                    if ui.button("Copy log path").clicked() {
                        ui.ctx().copy_text(path);
                    }
                } else {
                    ui.label("Unavailable on this platform");
                }
            });
            ui.separator();
            self.render_product_workspace_switch(ui);
        });
        if request_color {
            self.request_current_color();
        }
    }

    fn render_file_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Open...").clicked() {
            self.raw_dialog.open(ui.ctx());
            ui.close();
        }
        if ui
            .add_enabled(
                self.workspace.active().is_some() || self.workspace.active_image().is_some(),
                egui::Button::new("Save..."),
            )
            .clicked()
        {
            self.start_save_active_image(ui.ctx());
            ui.close();
        }
        if ui
            .add_enabled(
                self.workspace.active_id().is_some(),
                egui::Button::new("Close Image"),
            )
            .clicked()
        {
            if let Some(id) = self.workspace.active_id() {
                self.close_document(ui.ctx(), id);
            }
            ui.close();
        }
        ui.separator();
        if ui.button("Quit").clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn render_view_menu(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(document) = self.workspace.active_mut() else {
            ui.add_enabled(false, egui::Button::new(DisplayMode::RawMono.label()));
            ui.add_enabled(false, egui::Button::new(DisplayMode::Color.label()));
            return false;
        };
        let mut request_color = false;
        if ui
            .add(egui::Button::selectable(
                document.display_mode == DisplayMode::RawMono,
                DisplayMode::RawMono.label(),
            ))
            .clicked()
        {
            document.display_mode = DisplayMode::RawMono;
            ui.close();
        }
        if ui
            .add(egui::Button::selectable(
                document.display_mode == DisplayMode::Color,
                DisplayMode::Color.label(),
            ))
            .clicked()
        {
            document.display_mode = DisplayMode::Color;
            request_color = true;
            ui.close();
        }
        ui.separator();
        Self::render_view_navigation(ui, document);
        request_color
    }

    fn render_tools_menu(&mut self, ui: &mut egui::Ui) {
        let Some(document) = self.workspace.active_mut() else {
            ui.add_enabled(false, egui::Checkbox::new(&mut false, "Hover View"));
            return;
        };
        ui.checkbox(&mut document.hover_view.enabled, "Hover View");
        if document.hover_view.enabled {
            ui.menu_button("Hover View Settings", |ui| {
                ui.label("Neighborhood");
                ui.separator();
                for neighborhood in HoverNeighborhood::ALL {
                    if ui
                        .selectable_value(
                            &mut document.hover_view.neighborhood,
                            neighborhood,
                            neighborhood.label(),
                        )
                        .clicked()
                    {
                        ui.close();
                    }
                }
            });
        }
        ui.checkbox(&mut document.analysis_panel.expanded, "Analysis Panel");
        if ui.button("Reset ROI to Full Frame").clicked() {
            let width = document.loaded.frame.spec.width;
            let height = document.loaded.frame.spec.height;
            Self::commit_roi_for_document(
                document,
                Roi {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
            );
            ui.close();
        }
    }

    fn render_view_navigation(ui: &mut egui::Ui, document: &mut crate::workspace::RawDocument) {
        if ui.button("Fit to Window").clicked() {
            document.viewer.fit_on_next_frame = true;
            ui.close();
        }
        if ui.button("Actual Size / 100%").clicked() {
            document.viewer.zoom = 1.0;
            document.viewer.fit_on_next_frame = false;
            ui.close();
        }
        if ui.button("Zoom In").clicked() {
            document.viewer.zoom_by(1.25, None, egui::Rect::NOTHING);
            ui.close();
        }
        if ui.button("Zoom Out").clicked() {
            document.viewer.zoom_by(0.8, None, egui::Rect::NOTHING);
            ui.close();
        }
        if ui.button("Reset View").clicked() {
            document.viewer = ImageViewerState::default();
            ui.close();
        }
    }

    fn render_color_panel(&mut self, ui: &mut egui::Ui) {
        let Some(expanded) = self
            .workspace
            .active()
            .map(|document| document.color_panel_expanded)
        else {
            return;
        };
        if !expanded {
            let mut expand = false;
            egui::Panel::right("color_processing_rail")
                .resizable(false)
                .min_size(32.0)
                .max_size(32.0)
                .show(ui, |ui| {
                    let response =
                        ui.add_sized(egui::vec2(28.0, 28.0), egui::Button::new("‹").frame(false));
                    expand = response.clicked();
                    response.on_hover_text("Expand Color Processing");
                });
            if expand && let Some(document) = self.workspace.active_mut() {
                document.color_panel_expanded = true;
            }
            return;
        }

        let mut should_submit = false;
        let mut collapse = false;
        let mut reinterpret_request = None;
        let workspace = &mut self.workspace;
        egui::Panel::right("color_processing")
            .resizable(true)
            .default_size(280.0)
            .min_size(240.0)
            .max_size(420.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Color Processing");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let response = ui.button("›");
                        collapse = response.clicked();
                        response.on_hover_text("Collapse Color Processing");
                    });
                });
                egui::ScrollArea::vertical()
                    .id_salt("color_processing_controls")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let document = workspace.active_mut().expect("active document exists");
                        let source_spec = document.loaded.frame.spec.clone();
                        let installed_revision = document.loaded.installed_revision();
                        let response = render_color_controls(
                            ui,
                            &mut document.loaded.color_edit,
                            &source_spec,
                            &mut document.display_mode,
                            installed_revision,
                        );
                        let inspector = render_raw_inspector(
                            ui,
                            &mut document.raw_inspector,
                            document.raw_source.is_some(),
                        );
                        if inspector.params_changed {
                            // 先使在途任务失效；真正提交时再分配一个严格更新的任务 generation。
                            document.decode_generation =
                                document.decode_generation.saturating_add(1);
                        }
                        if document.raw_inspector.submission_due(Instant::now()) {
                            let bayer = document.loaded.frame.spec.bayer;
                            match document.raw_inspector.decode_params(bayer) {
                                Ok(params) => match document.raw_source.clone() {
                                    Some(source) => {
                                        let decode_generation =
                                            document.decode_generation.saturating_add(1);
                                        document.decode_generation = decode_generation;
                                        document.raw_inspector.mark_submitted(decode_generation);
                                        reinterpret_request = Some((
                                            document.id,
                                            decode_generation,
                                            source,
                                            params,
                                            document.loaded.roi,
                                            document.loaded.path.clone(),
                                        ));
                                    }
                                    None => document.raw_inspector.mark_validation_error(
                                        "当前文档没有可复用的 RAW source".to_owned(),
                                    ),
                                },
                                Err(error) => {
                                    document.raw_inspector.mark_validation_error(error);
                                }
                            }
                        }
                        should_submit = response.params_changed
                            || (response.mode_changed
                                && document.display_mode == DisplayMode::Color);
                    });
            });
        if let Some((document_id, decode_generation, source, params, roi, path)) =
            reinterpret_request
        {
            self.start_reinterpret(
                ui.ctx(),
                document_id,
                decode_generation,
                source,
                params,
                roi,
                path,
            );
        }
        if collapse && let Some(document) = self.workspace.active_mut() {
            document.color_panel_expanded = false;
        }
        if should_submit {
            self.request_current_color();
        }
    }

    fn render_yuv_inspector_panel(&mut self, ui: &mut egui::Ui) {
        let Some(document_id) = self.workspace.active_id() else {
            return;
        };
        let mut request = None;
        let Some(document) = self.workspace.image_mut(document_id) else {
            return;
        };
        let source = document.yuv_workspace_source();
        let mut params_changed = false;
        {
            let Some(inspector) = document.yuv_inspector.as_mut() else {
                return;
            };
            egui::Panel::right("yuv_decode")
                .resizable(true)
                .default_size(280.0)
                .min_size(240.0)
                .max_size(420.0)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("yuv_decode_controls")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            params_changed = render_yuv_inspector(ui, inspector, source.is_some())
                                .params_changed;
                        });
                });
        }
        if params_changed {
            // 草稿变更立即使在途结果过期，不能等到防抖提交后才更新 generation。
            document.invalidate_yuv_reinterpretation();
        }
        let Some(inspector) = document.yuv_inspector.as_mut() else {
            return;
        };
        if inspector.submission_due(Instant::now()) {
            match inspector.decode_spec() {
                Ok(spec) => match source {
                    Some((source, kind)) => {
                        let decode_generation = document.decode_generation.saturating_add(1);
                        document.decode_generation = decode_generation;
                        inspector.mark_submitted(decode_generation);
                        request = Some((document.id, decode_generation, source, kind, spec));
                    }
                    None => inspector
                        .mark_validation_error("当前文档没有可复用的不可变 YUV source".to_owned()),
                },
                Err(error) => inspector.mark_validation_error(error),
            }
        }
        if let Some((document_id, decode_generation, source, kind, spec)) = request {
            self.start_yuv_reinterpret(
                ui.ctx(),
                document_id,
                decode_generation,
                source,
                kind,
                spec,
            );
        }
    }

    fn render_analysis_panel_ui(&mut self, ui: &mut egui::Ui) {
        let Some(active_id) = self.workspace.active_id() else {
            return;
        };
        let image_hover = self.image_histogram_hover();
        let (expanded, default_size, min_size) =
            if let Some(document) = self.workspace.document(active_id) {
                (
                    document.analysis_panel.expanded,
                    document.analysis_panel.panel_height(),
                    document.analysis_panel.min_height(),
                )
            } else if let Some(document) = self.workspace.image(active_id) {
                (
                    document.analysis_panel.expanded,
                    document.analysis_panel.panel_height(),
                    document.analysis_panel.min_height(),
                )
            } else {
                return;
            };
        let max_size = if expanded {
            (ui.available_height() * 0.45).max(min_size)
        } else {
            min_size
        };
        let response = egui::Panel::bottom("analysis_panel")
            .resizable(expanded)
            .default_size(default_size)
            .min_size(min_size)
            .max_size(max_size)
            .show(ui, |ui| {
                if let Some(document) = self.workspace.document_mut(active_id) {
                    render_analysis_panel(ui, &mut document.analysis_panel, image_hover)
                } else {
                    let document = self
                        .workspace
                        .image_mut(active_id)
                        .expect("active analysis document exists");
                    render_analysis_panel(ui, &mut document.analysis_panel, image_hover)
                }
            })
            .inner;
        if response.selection_changed {
            if let Some(document) = self.workspace.document_mut(active_id) {
                document.analysis_pending_active = None;
            } else if let Some(document) = self.workspace.image_mut(active_id) {
                document.analysis_pending_active = None;
            }
        }
        self.update_spatial_highlight(
            response.hovered_bin,
            response.selection_changed || response.view_interacting,
        );
    }

    fn update_spatial_highlight(
        &mut self,
        selection: Option<HistogramBinSelection>,
        suppress: bool,
    ) {
        let Some(selection) = selection.filter(|_| !suppress) else {
            self.clear_active_spatial_highlight();
            return;
        };
        let request = if let Some(document) = self.workspace.active_mut() {
            if document.spatial_requested == Some(selection) {
                return;
            }
            if document.analysis_panel.current_key() != Some(selection.key)
                || document.id != selection.key.document_id
                || document.loaded.generation != selection.key.generation
            {
                document.spatial_requested = None;
                document.spatial_highlight = None;
                document.viewer.evict_derived_resources();
                return;
            }
            let display_image = match selection.key.domain {
                AnalysisDomain::RawBayer => None,
                AnalysisDomain::DisplayRgb => {
                    let Some(preview) = document.loaded.installed_color.as_ref() else {
                        document.spatial_requested = None;
                        document.spatial_highlight = None;
                        return;
                    };
                    if Some(preview.rendered_revision) != selection.key.source_revision {
                        document.spatial_requested = None;
                        document.spatial_highlight = None;
                        return;
                    }
                    Some(DisplayHistogramImage::Color(Arc::clone(&preview.image)))
                }
                AnalysisDomain::SourceRgb | AnalysisDomain::SourceYuv => return,
            };
            document.spatial_highlight = None;
            document.viewer.evict_derived_resources();
            document.spatial_requested = Some(selection);
            SpatialHighlightRequest {
                selection,
                native: NativeImage::Raw(Arc::clone(&document.loaded.frame)),
                display_image,
            }
        } else if let Some(document) = self.workspace.active_image_mut() {
            if document.spatial_requested == Some(selection) {
                return;
            }
            if document.analysis_panel.current_key() != Some(selection.key)
                || document.id != selection.key.document_id
                || document.generation != selection.key.generation
                || !matches!(
                    selection.key.domain,
                    AnalysisDomain::SourceRgb
                        | AnalysisDomain::SourceYuv
                        | AnalysisDomain::DisplayRgb
                )
            {
                document.spatial_requested = None;
                document.spatial_highlight = None;
                document.viewer.evict_derived_resources();
                return;
            }
            let display_image = (selection.key.domain == AnalysisDomain::DisplayRgb)
                .then(|| DisplayHistogramImage::Rgba8(Arc::clone(&document.display.frame)));
            document.spatial_highlight = None;
            document.viewer.evict_derived_resources();
            document.spatial_requested = Some(selection);
            SpatialHighlightRequest {
                selection,
                native: document.native.clone(),
                display_image,
            }
        } else {
            return;
        };
        self.workspace
            .supersede_spatial_submissions_except(request.selection.key.document_id);
        self.spatial_worker.submit(request);
    }

    fn clear_active_spatial_highlight(&mut self) {
        if let Some(document) = self.workspace.active_mut() {
            if document.spatial_requested.is_none() && document.spatial_highlight.is_none() {
                return;
            }
            document.spatial_requested = None;
            document.spatial_highlight = None;
            document.viewer.evict_derived_resources();
        } else if let Some(document) = self.workspace.active_image_mut() {
            if document.spatial_requested.is_none() && document.spatial_highlight.is_none() {
                return;
            }
            document.spatial_requested = None;
            document.spatial_highlight = None;
            document.viewer.evict_derived_resources();
        }
    }

    fn poll_spatial_highlight_result(&mut self) {
        let Some(result) = self.spatial_worker.take_ready() else {
            return;
        };
        self.install_spatial_highlight_result(result);
    }

    fn install_spatial_highlight_result(&mut self, result: SpatialHighlightResult) {
        let identity = DocumentIdentity {
            document_id: result.selection.key.document_id,
            generation: result.selection.key.generation,
        };
        let highlight = match result.result {
            Ok(payload) => Some(SpatialHighlight {
                selection: result.selection,
                mask: payload.mask,
                overlay_image: payload.overlay_image,
            }),
            Err(error) => {
                tracing::warn!(
                    operation = "build_histogram_spatial_highlight",
                    document_id = %identity.document_id,
                    generation = identity.generation,
                    error = %error,
                    "spatial highlight failed"
                );
                None
            }
        };
        if let Some(document) = self.workspace.matching_document_mut(identity) {
            if document.spatial_requested != Some(result.selection)
                || document.analysis_panel.current_key() != Some(result.selection.key)
            {
                return;
            }
            document.spatial_requested = highlight.as_ref().map(|highlight| highlight.selection);
            document.spatial_highlight = highlight;
            if document.spatial_highlight.is_some() {
                document.mark_derived_loaded();
            }
            return;
        }
        let Some(document) = self.workspace.matching_image_mut(identity) else {
            return;
        };
        if document.spatial_requested != Some(result.selection)
            || document.analysis_panel.current_key() != Some(result.selection.key)
        {
            return;
        }
        document.spatial_requested = highlight.as_ref().map(|highlight| highlight.selection);
        document.spatial_highlight = highlight;
    }

    fn image_histogram_hover(&self) -> Option<ImageHistogramHover> {
        if let Some(document) = self.workspace.active() {
            let key = document.analysis_panel.current_key()?;
            let cursor = document.viewer.cursor?;
            if document.id != key.document_id
                || document.loaded.generation != key.generation
                || !key.roi.contains(cursor.x, cursor.y)
            {
                return None;
            }
            let row_width = usize::try_from(document.loaded.frame.spec.width).ok()?;
            let index = usize::try_from(cursor.y)
                .ok()?
                .checked_mul(row_width)?
                .checked_add(usize::try_from(cursor.x).ok()?)?;
            let sample = match key.domain {
                AnalysisDomain::RawBayer => HistogramPixelSample::Raw {
                    site: document.loaded.frame.spec.bayer.site_at(cursor.x, cursor.y),
                    value: *document.loaded.frame.pixels().get(index)?,
                },
                AnalysisDomain::DisplayRgb => {
                    let preview = document.loaded.installed_color.as_ref()?;
                    if Some(preview.rendered_revision) != key.source_revision {
                        return None;
                    }
                    HistogramPixelSample::Display(display_histogram_sample(
                        *preview.image.pixels.get(index)?,
                    ))
                }
                AnalysisDomain::SourceRgb | AnalysisDomain::SourceYuv => return None,
            };
            return Some(ImageHistogramHover {
                key,
                x: cursor.x,
                y: cursor.y,
                sample,
            });
        }

        let document = self.workspace.active_image()?;
        let key = document.analysis_panel.current_key()?;
        let cursor = document.viewer.cursor?;
        if document.id != key.document_id
            || document.generation != key.generation
            || !key.roi.contains(cursor.x, cursor.y)
        {
            return None;
        }
        let sample = match key.domain {
            AnalysisDomain::DisplayRgb => {
                let [r, g, b, _] = document.display.frame.pixel(cursor.x, cursor.y)?;
                HistogramPixelSample::Display(display_histogram_sample(egui::Color32::from_rgb(
                    r, g, b,
                )))
            }
            AnalysisDomain::SourceRgb => match document.native.sample_at(cursor.x, cursor.y)? {
                camera_toolbox_core::NativePixelSample::Rgba { r, g, b, a } => {
                    HistogramPixelSample::SourceRgb { r, g, b, a }
                }
                _ => return None,
            },
            AnalysisDomain::SourceYuv => match document.native.sample_at(cursor.x, cursor.y)? {
                camera_toolbox_core::NativePixelSample::Yuv { y, u, v, .. } => {
                    HistogramPixelSample::SourceYuv { y, u, v }
                }
                _ => return None,
            },
            AnalysisDomain::RawBayer => return None,
        };
        Some(ImageHistogramHover {
            key,
            x: cursor.x,
            y: cursor.y,
            sample,
        })
    }

    fn handle_viewer_action(&mut self, action: ViewerAction) {
        let color_workspace = self.product_workspace == ProductWorkspace::Color;
        if let Some(document) = self.workspace.active_image_mut() {
            match action {
                ViewerAction::ClickPixel { x, y } if color_workspace => {
                    self.color_inspection.handle_manual_corner_click(
                        document,
                        ColorImagePoint {
                            x: f64::from(x),
                            y: f64::from(y),
                        },
                    );
                }
                ViewerAction::ClickPixel { .. } => {}
                ViewerAction::CommitRoi(roi) => Self::commit_roi_for_image(document, roi),
                ViewerAction::ResetRoi => {
                    let [width, height] = document.native.dimensions();
                    Self::commit_roi_for_image(
                        document,
                        Roi {
                            x: 0,
                            y: 0,
                            width,
                            height,
                        },
                    );
                }
            }
            return;
        }
        let Some(document) = self.workspace.active_mut() else {
            return;
        };
        match action {
            ViewerAction::CommitRoi(roi) => Self::commit_roi_for_document(document, roi),
            ViewerAction::ResetRoi => {
                let spec = &document.loaded.frame.spec;
                Self::commit_roi_for_document(
                    document,
                    Roi {
                        x: 0,
                        y: 0,
                        width: spec.width,
                        height: spec.height,
                    },
                );
            }
            ViewerAction::ClickPixel { .. } => {}
        }
    }

    fn commit_roi_for_document(document: &mut crate::workspace::RawDocument, roi: Roi) {
        let Some(roi) = roi.clamped_to(
            document.loaded.frame.spec.width,
            document.loaded.frame.spec.height,
        ) else {
            return;
        };
        if document.loaded.roi == roi {
            return;
        }
        tracing::debug!(
            operation = "commit_roi",
            document_id = %document.id,
            generation = document.loaded.generation,
            roi_x = roi.x,
            roi_y = roi.y,
            roi_width = roi.width,
            roi_height = roi.height,
            "committed viewer ROI"
        );
        document.loaded.roi = roi;
        document.loaded.stats = None;
        document.analysis_pending_active = None;
    }

    fn commit_roi_for_image(document: &mut crate::workspace::ImageDocument, roi: Roi) {
        let [width, height] = document.native.dimensions();
        let Some(roi) = roi.clamped_to(width, height) else {
            return;
        };
        if document.roi == roi {
            return;
        }
        tracing::debug!(
            operation = "commit_roi",
            document_id = %document.id,
            generation = document.generation,
            roi_x = roi.x,
            roi_y = roi.y,
            roi_width = roi.width,
            roi_height = roi.height,
            "committed static image ROI"
        );
        document.roi = roi;
        document.analysis_pending_active = None;
    }

    fn ensure_analysis(&mut self) {
        let Some(active_id) = self.workspace.active_id() else {
            return;
        };
        let request = if let Some(document) = self.workspace.document_mut(active_id) {
            Self::analysis_request_for_raw(document)
        } else if let Some(document) = self.workspace.image_mut(active_id) {
            Self::analysis_request_for_image(document)
        } else {
            None
        };
        let Some(request) = request else {
            return;
        };
        self.workspace
            .supersede_analysis_submissions_except(request.key.document_id);
        self.analysis_worker.submit(request);
    }

    fn analysis_request_for_raw(
        document: &mut crate::workspace::RawDocument,
    ) -> Option<AnalysisRequest> {
        let loaded = &document.loaded;
        let chart_roi = document.analysis_panel.scope.resolve(
            loaded.roi,
            loaded.frame.spec.width,
            loaded.frame.spec.height,
        );
        let (source_revision, display_frame) = match document.analysis_panel.domain {
            AnalysisDomain::RawBayer => (None, None),
            AnalysisDomain::SourceRgb | AnalysisDomain::SourceYuv => {
                document.analysis_panel.wait_for_source();
                return None;
            }
            AnalysisDomain::DisplayRgb => {
                let Some(preview) = &loaded.installed_color else {
                    document.analysis_panel.wait_for_source();
                    return None;
                };
                (
                    Some(preview.rendered_revision),
                    Some(Arc::clone(&preview.frame)),
                )
            }
        };
        let key = AnalysisKey {
            document_id: document.id,
            generation: loaded.generation,
            source_revision,
            roi: chart_roi,
            domain: document.analysis_panel.domain,
        };
        let desired = document.analysis_panel.set_desired(key);
        let chart_ready = document.analysis_panel.has_current(key);
        let stats_ready = loaded.stats.is_some();
        let stats_pending =
            document.analysis_pending_active == Some((loaded.generation, loaded.roi));
        if desired != DesiredAnalysis::Submit && (stats_ready || stats_pending) {
            return None;
        }
        let compute_chart = !chart_ready;
        document.analysis_pending_active = Some((loaded.generation, loaded.roi));
        Some(AnalysisRequest {
            key,
            active_roi: loaded.roi,
            compute_chart,
            native: NativeImage::Raw(Arc::clone(&loaded.frame)),
            display_frame,
        })
    }

    fn analysis_request_for_image(
        document: &mut crate::workspace::ImageDocument,
    ) -> Option<AnalysisRequest> {
        let [width, height] = document.native.dimensions();
        let chart_roi = document
            .analysis_panel
            .scope
            .resolve(document.roi, width, height);
        let (source_revision, display_frame) = match document.analysis_panel.domain {
            AnalysisDomain::SourceRgb if matches!(&document.native, NativeImage::Rgba8(_)) => {
                (None, None)
            }
            AnalysisDomain::SourceYuv if matches!(&document.native, NativeImage::Yuv420Sp(_)) => {
                (None, None)
            }
            AnalysisDomain::DisplayRgb => (
                Some(document.display.revision),
                Some(Arc::clone(&document.display.frame)),
            ),
            AnalysisDomain::RawBayer | AnalysisDomain::SourceRgb | AnalysisDomain::SourceYuv => {
                document.analysis_panel.wait_for_source();
                return None;
            }
        };
        let key = AnalysisKey {
            document_id: document.id,
            generation: document.generation,
            source_revision,
            roi: chart_roi,
            domain: document.analysis_panel.domain,
        };
        if document.analysis_panel.set_desired(key) != DesiredAnalysis::Submit {
            return None;
        }
        document.analysis_pending_active = Some((document.generation, document.roi));
        Some(AnalysisRequest {
            key,
            active_roi: document.roi,
            compute_chart: true,
            native: document.native.clone(),
            display_frame,
        })
    }

    fn poll_analysis_result(&mut self) {
        let Some(result) = self.analysis_worker.take_ready() else {
            return;
        };
        self.install_analysis_result(result);
    }

    fn install_analysis_result(&mut self, result: AnalysisResult) {
        let key = result.key;
        let identity = DocumentIdentity {
            document_id: key.document_id,
            generation: key.generation,
        };
        if let Some(document) = self.workspace.matching_document_mut(identity) {
            let accepted = document.analysis_panel.accept_result(result);
            let Some((active_roi, stats)) = accepted else {
                tracing::debug!(
                    operation = "poll_histogram_analysis",
                    document_id = %key.document_id,
                    generation = key.generation,
                    revision = ?key.source_revision,
                    domain = key.domain.label(),
                    "dropped stale or failed histogram analysis"
                );
                return;
            };
            if document.loaded.roi == active_roi {
                document.loaded.stats = Some(stats);
                document.analysis_pending_active = None;
                document.mark_derived_loaded();
                tracing::debug!(
                    operation = "install_histogram_analysis",
                    document_id = %key.document_id,
                    generation = key.generation,
                    revision = ?key.source_revision,
                    domain = key.domain.label(),
                    "installed histogram analysis"
                );
            }
            return;
        }
        if let Some(document) = self.workspace.matching_image_mut(identity) {
            let accepted = document.analysis_panel.accept_result(result);
            let Some((active_roi, _stats)) = accepted else {
                tracing::debug!(
                    operation = "poll_histogram_analysis",
                    document_id = %key.document_id,
                    generation = key.generation,
                    revision = ?key.source_revision,
                    domain = key.domain.label(),
                    "dropped stale or failed static image analysis"
                );
                return;
            };
            if document.roi == active_roi {
                document.analysis_pending_active = None;
                tracing::debug!(
                    operation = "install_histogram_analysis",
                    document_id = %key.document_id,
                    generation = key.generation,
                    revision = ?key.source_revision,
                    domain = key.domain.label(),
                    "installed static image histogram analysis"
                );
            }
            return;
        }
        tracing::debug!(
            operation = "poll_histogram_analysis",
            document_id = %key.document_id,
            generation = key.generation,
            "dropped analysis result for closed or replaced document"
        );
    }

    fn render_status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(active) = self.active_raw_open.as_ref().filter(|active| active.remote) {
                let name = active
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("remote RAW");
                ui.spinner();
                match active.progress {
                    Some(progress) => {
                        let percent = if progress.total_bytes == 0 {
                            100
                        } else {
                            ((u128::from(progress.bytes_read) * 100
                                + u128::from(progress.total_bytes) / 2)
                                / u128::from(progress.total_bytes))
                            .min(100)
                        };
                        let mib = u128::from(1024_u64 * 1024);
                        let read_tenths = (u128::from(progress.bytes_read) * 10 + mib / 2) / mib;
                        let total_tenths = (u128::from(progress.total_bytes) * 10 + mib / 2) / mib;
                        ui.label(format!(
                            "Transferring {name} · {}.{:01}/{}.{:01} MiB · {percent}%",
                            read_tenths / 10,
                            read_tenths % 10,
                            total_tenths / 10,
                            total_tenths % 10
                        ));
                    }
                    None => {
                        ui.label(format!("Preparing remote transfer · {name}"));
                    }
                }
                return;
            }
            if let Some(document) = self.workspace.active_live() {
                let media = document.media.as_ref().map_or_else(
                    || format!("Stage: {:?}", document.stage),
                    |media| {
                        format!(
                            "{:?} {}×{} PT {} SSRC {:08x} {} fps",
                            media.codec,
                            media.width,
                            media.height,
                            media.payload_type,
                            media.ssrc,
                            media.frame_rate
                        )
                    },
                );
                ui.label(&document.title);
                ui.separator();
                ui.label(media);
                ui.separator();
                let counters = match &document.source {
                    LiveStreamSource::Rtsp { .. } => {
                        let io = if document.metrics.network_bytes_available {
                            format!("FFmpeg I/O {} B", document.metrics.network_bytes)
                        } else {
                            "FFmpeg I/O N/A".to_owned()
                        };
                        format!(
                            "{io} · media {} B · preview dropped {} · resync {}",
                            document.metrics.ffmpeg_media_bytes,
                            document.metrics.preview_dropped,
                            document.metrics.decoder_resyncs
                        )
                    }
                    LiveStreamSource::Cv610 { .. } => format!(
                        "Network {} B · RTP {} · gaps {} · preview dropped {} · resync {}",
                        document.metrics.network_bytes,
                        document.metrics.rtp_packets,
                        document.metrics.rtp_gaps,
                        document.metrics.preview_dropped,
                        document.metrics.decoder_resyncs
                    ),
                };
                ui.label(counters);
                return;
            }
            if let Some(document) = self.workspace.active_image() {
                let [width, height] = document.native.dimensions();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!("{:.0}%", document.viewer.zoom * 100.0));
                    ui.separator();
                    ui.label(format!(
                        "ROI: {}×{}",
                        document.roi.width, document.roi.height
                    ));
                    ui.separator();
                    ui.allocate_ui_with_layout(
                        ui.available_size(),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.add(egui::Label::new(&document.title).truncate());
                            ui.separator();
                            ui.label(format!(
                                "{}×{} · {}",
                                width,
                                height,
                                document.format_label()
                            ));
                        },
                    );
                });
                return;
            }
            let Some(document) = self.workspace.active() else {
                ui.label("Ready");
                return;
            };
            let loaded = &document.loaded;
            let displayed_bayer = loaded.displayed_bayer(document.display_mode);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!("{:.0}%", document.viewer.zoom * 100.0));
                ui.separator();
                ui.label(document.display_mode.label());
                Self::render_diagnostic_badges(ui, document);
                ui.separator();
                ui.label(format!("ROI: {}×{}", loaded.roi.width, loaded.roi.height));
                ui.separator();
                ui.allocate_ui_with_layout(
                    ui.available_size(),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add(egui::Label::new(&document.title).truncate());
                        ui.separator();
                        ui.label(format!(
                            "{}×{} · {}-bit · {}",
                            loaded.frame.spec.width,
                            loaded.frame.spec.height,
                            loaded.frame.spec.bit_depth,
                            bayer_label(displayed_bayer).to_uppercase()
                        ));
                    },
                );
            });
        });
    }

    fn render_diagnostic_badges(ui: &mut egui::Ui, document: &crate::workspace::RawDocument) {
        let loaded = &document.loaded;
        if loaded.diagnostics.has_out_of_range() {
            ui.separator();
            ui.colored_label(
                egui::Color32::YELLOW,
                format!("RAW range: {}", loaded.diagnostics.out_of_range_pixels),
            );
        }
        if document.display_mode != DisplayMode::Color {
            return;
        }
        let Some(preview) = &loaded.installed_color else {
            return;
        };
        if preview.diagnostics.display_clipped_channels > 0 {
            ui.separator();
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "RGB clipped: {}",
                    preview.diagnostics.display_clipped_channels
                ),
            );
        }
        if preview.diagnostics.missing_neighbor_channels > 0 {
            ui.separator();
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "Demosaic edge: {}",
                    preview.diagnostics.missing_neighbor_channels
                ),
            );
        }
    }

    fn handle_explorer_action(&mut self, context: &egui::Context, action: ExplorerAction) {
        match action {
            #[cfg(feature = "platform-ssh")]
            ExplorerAction::ActivateSftp(RemoteConnectionCommit {
                config,
                session_password,
            }) => {
                #[cfg(feature = "calibration-opencv")]
                if self.active_eeprom_cancellation.is_some() {
                    self.explorer.set_remote_connection_error(
                        "Wait for the active EEPROM operation to finish before replacing Explorer SFTP.",
                    );
                    return;
                }
                #[cfg(feature = "calibration-opencv")]
                self.invalidate_eeprom_target(
                    "Explorer SFTP connection changed. Select it again for EEPROM and Inspect.",
                );
                let camera_toolbox_app::RemoteAuthentication::Password { slot_id } =
                    &config.authentication
                else {
                    self.explorer.set_remote_connection_error(
                        "Explorer SFTP requires process-only password authentication",
                    );
                    return;
                };
                if let Err(error) = self
                    .live_runtime
                    .ssh_credential_resolver()
                    .register_session_password(slot_id, session_password)
                {
                    tracing::error!(
                        operation = "explorer_register_remote_password",
                        connection_id = config.id.as_str(),
                        slot_id,
                        error = %error,
                        "failed to register remote session password"
                    );
                    self.explorer.set_remote_connection_error(format!(
                        "Register remote password failed: {error}"
                    ));
                    return;
                }
                if let Err(error) = self.explorer.finish_sftp_connection(config, context) {
                    tracing::error!(
                        operation = "explorer_activate_sftp",
                        error = %error,
                        "failed to activate ephemeral SFTP workspace"
                    );
                    self.explorer.set_remote_connection_error(format!(
                        "Open SFTP workspace failed: {error}"
                    ));
                }
            }
            ExplorerAction::OpenAuto {
                display_path,
                file_system,
                reference,
                remote,
            } => {
                let kind = ImageOpenPipeline::classify(&reference);
                let request = WorkspaceFileOpenRequest {
                    display_path: display_path.clone(),
                    file_system,
                    reference,
                    remote,
                };
                match kind {
                    Ok(kind) if kind == ImageFileKind::Png || !self.is_color_workspace() => {
                        let attempt = self.begin_raw_open_attempt();
                        self.start_load_workspace_file(
                            context,
                            attempt,
                            request,
                            ImageOpenMode::Auto,
                        );
                    }
                    Ok(kind) => {
                        let attempt = self.begin_raw_open_attempt();
                        tracing::warn!(
                            operation = "open_color_input",
                            path = %display_path.display(),
                            kind = ?kind,
                            "rejected non-PNG Color page file input"
                        );
                        self.notifications.push_once(UiNotification::error(
                            NotificationKey::RawLoadFailed { attempt },
                            "Color input must be PNG",
                            "Color page file input accepts PNG only; use RTSP Capture for live input.",
                        ));
                    }
                    Err(error) => {
                        let attempt = self.begin_raw_open_attempt();
                        tracing::warn!(
                            operation = "classify_image_file",
                            path = %display_path.display(),
                            error = %error,
                            "rejected unsupported image file"
                        );
                        self.notifications.push_once(UiNotification::error(
                            NotificationKey::RawLoadFailed { attempt },
                            "Unsupported image file",
                            &error.to_string(),
                        ));
                    }
                }
            }
            ExplorerAction::AddCalibration(candidates) => {
                #[cfg(feature = "calibration-opencv")]
                {
                    self.product_workspace = ProductWorkspace::Calibration;
                    self.calibration.import(candidates);
                }
                #[cfg(not(feature = "calibration-opencv"))]
                {
                    let _ = candidates;
                    tracing::warn!(
                        operation = "add_calibration_dataset",
                        "calibration-opencv feature is disabled"
                    );
                }
            }
            ExplorerAction::CalibrationImportRejected { display_path } => {
                #[cfg(feature = "calibration-opencv")]
                self.calibration.reject_import(&display_path);
                #[cfg(not(feature = "calibration-opencv"))]
                let _ = display_path;
            }
        }
    }

    fn cancel_active_auto_open(&mut self) {
        if let Some(active) = self.active_auto_open.take() {
            active.cancellation.cancel();
        }
    }

    fn enqueue_auto_open_candidates(&mut self) {
        for candidate in self.auto_open.poll() {
            let Some(ExplorerAction::OpenAuto {
                display_path,
                file_system,
                reference,
                remote,
            }) = self.explorer.open_action_for(&candidate.reference)
            else {
                tracing::warn!(
                    operation = "auto_open_enqueue",
                    rule_id = candidate.rule_id.as_str(),
                    source_id = %candidate.reference.source_id,
                    path = %candidate.reference.path.as_str(),
                    "auto-open source is no longer mounted; dropping candidate"
                );
                continue;
            };
            if self.pending_auto_open.len() >= AUTO_OPEN_QUEUE_LIMIT {
                self.pending_auto_open.pop_front();
            }
            self.pending_auto_open.push_back(PendingAutoOpenRequest {
                candidate,
                request: WorkspaceFileOpenRequest {
                    display_path,
                    file_system,
                    reference,
                    remote,
                },
            });
        }
    }

    fn dispatch_auto_open(&mut self, context: &egui::Context) {
        if self.active_raw_open.is_some() || self.active_auto_open.is_some() {
            return;
        }
        let Some(pending) = self.pending_auto_open.pop_front() else {
            return;
        };
        let cancellation = FsCancellation::default();
        self.active_auto_open = Some(ActiveAutoOpenJob {
            candidate: pending.candidate.clone(),
            cancellation: cancellation.clone(),
        });
        let pipeline = self.image_pipeline.clone();
        let sender = self.auto_open_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let path = pending.request.display_path.clone();
            let mut ignore_progress = |_| {};
            let result = decode_workspace_image_request(
                &pipeline,
                pending.request,
                ImageOpenMode::Auto,
                cancellation,
                &mut ignore_progress,
            )
            .map_err(|error| error.to_string());
            let _ = sender.send(AutoOpenJobResult {
                candidate: pending.candidate,
                path,
                result,
            });
            context.request_repaint();
        });
    }

    fn begin_raw_open_attempt(&mut self) -> u64 {
        let attempt = self.next_load_attempt;
        self.next_load_attempt = self.next_load_attempt.saturating_add(1);
        if attempt > 1 {
            self.notifications
                .clear_scope(NotificationScope::LoadAttempt(attempt - 1));
        }
        attempt
    }

    fn render_raw_open_dialog(&mut self, context: &egui::Context) {
        let Some(request) = self.raw_dialog.show(context, &self.raw_pipeline) else {
            return;
        };
        let attempt = self.begin_raw_open_attempt();
        self.start_load_raw(context, attempt, request);
        self.raw_dialog.close(context);
    }

    fn render_yuv_save_dialog(&mut self, context: &egui::Context) {
        if let Some((chroma_order, matrix, range)) = self.yuv_save_dialog.show(context) {
            let Some(pending) = self.pending_yuv_save.take() else {
                return;
            };
            self.save_worker.submit(SaveRequest {
                key: pending.key,
                destination: pending.destination,
                target_label: pending.target_label,
                file_name: pending.file_name,
                format: SaveFormat::Yuv420Sp {
                    chroma_order,
                    matrix,
                    range,
                },
                payload: SavePayload::Display(pending.frame),
            });
        } else if !self.yuv_save_dialog.is_open() {
            self.pending_yuv_save = None;
        }
    }

    fn render_named_export_dialog(&mut self, context: &egui::Context) {
        let Some(selection) = self.export_name_dialog.show(context) else {
            if !self.export_name_dialog.is_open() {
                self.pending_named_export = None;
            }
            return;
        };
        let resolved = match self
            .explorer
            .export_destination_for(selection.source, &selection.directory_path)
        {
            Ok(resolved) => resolved,
            Err(error) => {
                self.export_name_dialog.reject(error);
                return;
            }
        };
        let pending = self
            .pending_named_export
            .take()
            .expect("an open export dialog has a pending export");
        match pending {
            PendingNamedExport::Image { snapshot } => self.route_image_export(
                resolved.destination,
                resolved.directory_label,
                selection.file_name,
                snapshot,
            ),
            PendingNamedExport::Color { export } => self.submit_color_export(
                context,
                resolved.destination,
                resolved.directory_label,
                selection.file_name,
                export,
            ),
            #[cfg(feature = "calibration-opencv")]
            PendingNamedExport::Calibration { export } => self.submit_calibration_export(
                context,
                resolved.destination,
                resolved.directory_label,
                selection.file_name,
                export,
            ),
        }
    }

    fn start_save_active_image(&mut self, context: &egui::Context) -> bool {
        let Some(default_name) = self.active_save_default_name() else {
            return false;
        };
        let Some(snapshot) = self.active_image_export_snapshot() else {
            return false;
        };
        let prefill = match self.explorer.export_dialog_prefill(context) {
            Ok(prefill) => prefill,
            Err(error) => {
                if let Some(key) = Self::snapshot_key(&snapshot) {
                    self.notify_save_error(key, error);
                }
                return false;
            }
        };
        self.pending_named_export = Some(PendingNamedExport::Image { snapshot });
        self.export_name_dialog
            .open("Save image", default_name, prefill);
        true
    }

    fn active_save_default_name(&self) -> Option<String> {
        if let Some(document) = self.workspace.active() {
            let stem = document
                .title
                .strip_suffix(".raw")
                .unwrap_or(&document.title);
            return Some(format!("{stem}.raw"));
        }
        let document = self.workspace.active_image()?;
        let stem = document
            .title
            .rsplit_once('.')
            .map_or(document.title.as_str(), |(stem, _)| stem);
        Some(format!("{stem}.png"))
    }

    fn active_image_export_snapshot(&self) -> Option<ImageExportSnapshot> {
        if let Some(document) = self.workspace.active() {
            let raw_key = SaveKey {
                document_id: document.id,
                generation: document.loaded.generation,
                revision: document.decode_generation,
            };
            let display = document.loaded.installed_color.as_ref().map(|preview| {
                (
                    SaveKey {
                        document_id: document.id,
                        generation: document.loaded.generation,
                        revision: preview.rendered_revision,
                    },
                    Arc::clone(&preview.frame),
                )
            });
            return Some(ImageExportSnapshot {
                raw: Some((raw_key, Arc::clone(&document.loaded.frame))),
                display,
            });
        }
        let document = self.workspace.active_image()?;
        Some(ImageExportSnapshot {
            raw: None,
            display: Some((
                SaveKey {
                    document_id: document.id,
                    generation: document.generation,
                    revision: document.display.revision,
                },
                Arc::clone(&document.display.frame),
            )),
        })
    }

    fn snapshot_key(snapshot: &ImageExportSnapshot) -> Option<SaveKey> {
        snapshot
            .raw
            .as_ref()
            .map(|(key, _)| *key)
            .or_else(|| snapshot.display.as_ref().map(|(key, _)| *key))
    }

    fn route_image_export(
        &mut self,
        destination: ExportDestination,
        directory_label: String,
        file_name: EntryName,
        snapshot: ImageExportSnapshot,
    ) {
        let extension = Path::new(file_name.as_str())
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        let target_label = Self::export_target_label(&directory_label, &file_name);
        match extension.as_deref() {
            Some("raw") => match snapshot.raw {
                Some((key, frame)) => self.save_worker.submit(SaveRequest {
                    key,
                    destination,
                    target_label,
                    file_name,
                    format: SaveFormat::RawU16Le,
                    payload: SavePayload::Raw(frame),
                }),
                None => {
                    if let Some(key) = snapshot.display.as_ref().map(|(key, _)| *key) {
                        self.notify_save_error(
                            key,
                            "RAW save requires authoritative native RAW data".to_owned(),
                        );
                    }
                }
            },
            Some("png") => match snapshot.display {
                Some((key, frame)) => self.save_worker.submit(SaveRequest {
                    key,
                    destination,
                    target_label,
                    file_name,
                    format: SaveFormat::Png,
                    payload: SavePayload::Display(frame),
                }),
                None => {
                    if let Some(key) = snapshot.raw.as_ref().map(|(key, _)| *key) {
                        self.notify_save_error(
                            key,
                            "PNG save requires an available immutable display revision".to_owned(),
                        );
                    }
                }
            },
            Some("nv12") | Some("nv21") | Some("yuv") => match snapshot.display {
                Some((key, frame)) => {
                    let chroma_order_hint = match extension.as_deref() {
                        Some("nv12") => Some(ChromaOrder::Uv),
                        Some("nv21") => Some(ChromaOrder::Vu),
                        _ => None,
                    };
                    let (matrix, range) = self.active_yuv_save_defaults();
                    self.pending_yuv_save = Some(PendingYuvSave {
                        key,
                        destination,
                        target_label: target_label.clone(),
                        file_name,
                        frame: Arc::clone(&frame),
                    });
                    self.yuv_save_dialog.open(
                        target_label,
                        [frame.width, frame.height],
                        chroma_order_hint,
                        matrix,
                        range,
                    );
                }
                None => {
                    if let Some(key) = snapshot.raw.as_ref().map(|(key, _)| *key) {
                        self.notify_save_error(
                            key,
                            "YUV save requires an available immutable display revision".to_owned(),
                        );
                    }
                }
            },
            _ => {
                if let Some(key) = Self::snapshot_key(&snapshot) {
                    self.notify_save_error(
                        key,
                        "choose a .raw, .png, .nv12, .nv21, or .yuv file name".to_owned(),
                    );
                }
            }
        }
    }

    fn export_target_label(directory_label: &str, file_name: &EntryName) -> String {
        if directory_label.ends_with('/') {
            format!("{directory_label}{}", file_name.as_str())
        } else {
            format!("{directory_label}/{}", file_name.as_str())
        }
    }

    #[cfg(feature = "calibration-opencv")]
    fn begin_calibration_export(&mut self, context: &egui::Context, export: CalibrationExport) {
        let prefill = match self.explorer.export_dialog_prefill(context) {
            Ok(prefill) => prefill,
            Err(error) => {
                self.calibration
                    .report_export_finished(export.label(), "Workspace", Err(&error));
                return;
            }
        };
        let suggested_name = export.suggested_name();
        self.pending_named_export = Some(PendingNamedExport::Calibration { export });
        self.export_name_dialog
            .open("Export calibration", suggested_name, prefill);
    }

    #[cfg(feature = "calibration-opencv")]
    fn submit_calibration_export(
        &mut self,
        context: &egui::Context,
        destination: ExportDestination,
        directory_label: String,
        file_name: EntryName,
        export: CalibrationExport,
    ) {
        let target_label = Self::export_target_label(&directory_label, &file_name);
        let label = export.label();
        self.calibration.report_export_started(label, &target_label);
        let sender = self.calibration_export_sender.clone();
        let worker_context = context.clone();
        let worker_destination = destination.clone();
        let worker_target_label = target_label.clone();
        let spawn_result = thread::Builder::new()
            .name("camera-toolbox-calibration-export".to_owned())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    export.save_new(
                        &worker_destination,
                        &file_name,
                        &FsControl::with_timeout(Duration::from_secs(60)),
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(camera_toolbox_app::FileSystemError::Io(
                        "calibration export worker panicked".to_owned(),
                    ))
                })
                .map_err(|error| error.to_string());
                let _ = sender.send(CalibrationExportResult {
                    destination: worker_destination,
                    target_label: worker_target_label,
                    label,
                    result,
                });
                worker_context.request_repaint();
            });
        if let Err(error) = spawn_result {
            self.calibration.report_export_finished(
                label,
                &target_label,
                Err(&format!("start export worker failed: {error}")),
            );
        }
    }

    fn begin_color_export(&mut self, context: &egui::Context, export: ColorMetricsExport) {
        let prefill = match self.explorer.export_dialog_prefill(context) {
            Ok(prefill) => prefill,
            Err(error) => {
                self.color_inspection.report_export_finished(
                    export.label(),
                    "Workspace",
                    Err(&error),
                );
                return;
            }
        };
        let suggested_name = export.suggested_name();
        self.pending_named_export = Some(PendingNamedExport::Color { export });
        self.export_name_dialog
            .open("Export color metrics", suggested_name, prefill);
    }

    fn submit_color_export(
        &mut self,
        context: &egui::Context,
        destination: ExportDestination,
        directory_label: String,
        file_name: EntryName,
        export: ColorMetricsExport,
    ) {
        let target_label = Self::export_target_label(&directory_label, &file_name);
        let label = export.label();
        self.color_inspection
            .report_export_started(label, &target_label);
        let sender = self.color_export_sender.clone();
        let worker_context = context.clone();
        let worker_destination = destination.clone();
        let worker_target_label = target_label.clone();
        let spawn_result = thread::Builder::new()
            .name("camera-toolbox-color-export".to_owned())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    export.save_new(
                        &worker_destination,
                        &file_name,
                        &FsControl::with_timeout(Duration::from_secs(60)),
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(camera_toolbox_app::FileSystemError::Io(
                        "color metrics export worker panicked".to_owned(),
                    ))
                })
                .map_err(|error| error.to_string());
                let _ = sender.send(ColorExportResult {
                    destination: worker_destination,
                    target_label: worker_target_label,
                    label,
                    result,
                });
                worker_context.request_repaint();
            });
        if let Err(error) = spawn_result {
            self.color_inspection.report_export_finished(
                label,
                &target_label,
                Err(&format!("start export worker failed: {error}")),
            );
        }
    }

    fn poll_color_export_result(&mut self, context: &egui::Context) {
        while let Ok(result) = self.color_export_receiver.try_recv() {
            match result.result {
                Ok(receipt) => {
                    self.explorer
                        .refresh_save_destination(&result.destination, context);
                    self.color_inspection.report_export_finished(
                        result.label,
                        &result.target_label,
                        Ok(receipt.bytes_written()),
                    );
                }
                Err(error) => self.color_inspection.report_export_finished(
                    result.label,
                    &result.target_label,
                    Err(&error),
                ),
            }
        }
    }

    #[cfg(feature = "calibration-opencv")]
    fn poll_calibration_export_result(&mut self, context: &egui::Context) {
        while let Ok(result) = self.calibration_export_receiver.try_recv() {
            match result.result {
                Ok(receipt) => {
                    self.explorer
                        .refresh_save_destination(&result.destination, context);
                    self.calibration.report_export_finished(
                        result.label,
                        &result.target_label,
                        Ok(receipt.bytes_written()),
                    );
                }
                Err(error) => self.calibration.report_export_finished(
                    result.label,
                    &result.target_label,
                    Err(&error),
                ),
            }
        }
    }

    #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
    fn invalidate_eeprom_target(&mut self, message: impl Into<String>) {
        self.eeprom_target = None;
        self.calibration.report_target_invalidated(message);
    }

    #[cfg(feature = "platform-ssh")]
    fn local_eeprom_helper_program_name() -> &'static str {
        "camera-toolbox-eeprom-helper-linux-aarch64"
    }

    #[cfg(feature = "platform-ssh")]
    fn local_eeprom_helper_candidates() -> Result<Vec<PathBuf>, String> {
        let current = std::env::current_exe()
            .map_err(|error| format!("Resolve current executable failed: {error}"))?;
        let parent = current
            .parent()
            .ok_or_else(|| format!("Current executable has no parent: {}", current.display()))?;
        let program = Self::local_eeprom_helper_program_name();
        let mut candidates = vec![parent.join(&program)];
        if parent
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "deps")
            && let Some(profile_dir) = parent.parent()
        {
            candidates.push(profile_dir.join(program));
        }
        Ok(candidates)
    }

    /// 远端 EEPROM helper 固定为 Linux AArch64 ELF，避免将 GUI 宿主二进制上传后执行失败。
    fn validate_eeprom_helper_payload(bytes: &[u8], candidate: &Path) -> Result<(), String> {
        const ELF_HEADER_BYTES: usize = 20;
        const ELF64_CLASS: u8 = 2;
        const LITTLE_ENDIAN: u8 = 1;
        const AARCH64_MACHINE: [u8; 2] = [0xb7, 0x00];

        let valid = bytes.len() >= ELF_HEADER_BYTES
            && bytes.starts_with(b"\x7fELF")
            && bytes[4] == ELF64_CLASS
            && bytes[5] == LITTLE_ENDIAN
            && bytes[18..20] == AARCH64_MACHINE;
        if valid {
            Ok(())
        } else {
            Err(format!(
                "EEPROM helper {} is not a Linux AArch64 ELF binary",
                candidate.display()
            ))
        }
    }

    #[cfg(feature = "platform-ssh")]
    fn read_local_eeprom_helper_payload() -> Result<Arc<[u8]>, String> {
        let candidates = Self::local_eeprom_helper_candidates()?;
        let mut missing = Vec::new();
        for candidate in &candidates {
            match fs::read(candidate) {
                Ok(bytes) => {
                    Self::validate_eeprom_helper_payload(&bytes, candidate)?;
                    return Ok(Arc::<[u8]>::from(bytes.into_boxed_slice()));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(candidate.display().to_string());
                }
                Err(error) => {
                    return Err(format!(
                        "Read EEPROM helper {} failed: {error}",
                        candidate.display()
                    ));
                }
            }
        }
        Err(format!(
            "EEPROM helper binary not found; expected {}",
            missing.join(" or ")
        ))
    }

    #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
    fn configure_eeprom_target(
        &mut self,
        request: CalibrationEepromTargetRequest,
    ) -> Result<String, String> {
        let source = self
            .explorer
            .connected_sftp_connection()
            .cloned()
            .ok_or_else(|| "Connect Explorer SFTP before configuring EEPROM.".to_owned())?;
        let camera_toolbox_app::RemoteAuthentication::Password { slot_id } = &source.authentication
        else {
            return Err("EEPROM requires the Explorer SFTP process-only password".to_owned());
        };
        let credential_ref = format!("session:{slot_id}");
        let connection = SshConnectionTarget {
            host: source.host.clone(),
            port: source.port,
            username: source.username.clone(),
            expected_host_key: None,
            command_subsystem: None,
            remote_event_subsystem: None,
        };
        let target_document = serde_json::json!({
            "connection_id": source.id.as_str(),
            "host": source.host,
            "port": source.port,
            "username": source.username,
            "i2c_bus": request.i2c_bus,
            "map_id": camera_toolbox_core::YG_STEREO_P24C64G_V1_MAP_ID,
        });
        let target_bytes =
            serde_json::to_vec(&target_document).map_err(|error| error.to_string())?;
        let snapshot_hash = SnapshotHash::digest_bytes(&target_bytes);
        let helper_payload = Self::read_local_eeprom_helper_payload()?;
        let service = SshEepromProvisionService::new(
            format!("calibration-eeprom-{}", &snapshot_hash.to_hex()[..12]),
            connection,
            credential_ref,
            65_536,
            request.i2c_bus,
            helper_payload,
            self.live_runtime.ssh_resolver(),
            Arc::new(RusshTransportFactory),
        )
        .map_err(|error| error.to_string())?;
        let label = format!(
            "{}@{}:{} / i2c-{} @{}",
            target_document["username"].as_str().unwrap_or(""),
            target_document["host"].as_str().unwrap_or(""),
            target_document["port"].as_u64().unwrap_or(0),
            request.i2c_bus,
            &snapshot_hash.to_hex()[..12],
        );
        self.eeprom_target = Some(EepromProvisioningTarget {
            service: Arc::new(service),
            snapshot_hash,
            label: label.clone(),
            i2c_bus: u32::from(request.i2c_bus),
        });
        Ok(label)
    }

    #[cfg(feature = "platform-ssh")]
    fn begin_i2c_tools_operation(&mut self, context: &egui::Context, action: I2cToolsAction) {
        if matches!(action, I2cToolsAction::Cancel) {
            if !self.active_i2c_tools_cancellable {
                self.i2c_tools
                    .report_error("No cancellable I²C Tools operation is active.");
            } else if let Some(cancellation) = &self.active_i2c_tools_cancellation {
                cancellation.cancel();
                self.i2c_tools.report_cancelled();
            }
            return;
        }
        if self.active_i2c_tools_cancellation.is_some() {
            self.i2c_tools
                .report_error("An I²C Tools operation is already active.");
            return;
        }
        let source = match self.explorer.connected_sftp_connection().cloned() {
            Some(source) => source,
            None => {
                self.i2c_tools
                    .report_error("Connect Explorer SFTP before using I²C Tools.");
                return;
            }
        };
        let camera_toolbox_app::RemoteAuthentication::Password { slot_id } = &source.authentication
        else {
            self.i2c_tools
                .report_error("I²C Tools requires the Explorer SFTP process-only password.");
            return;
        };
        let helper_payload = match Self::read_local_eeprom_helper_payload() {
            Ok(payload) => payload,
            Err(error) => {
                self.i2c_tools.report_error(error);
                return;
            }
        };
        let (kind, helper_action) = match action {
            I2cToolsAction::DiscoverBuses => (
                I2cToolsOperationKind::BusDiscovery,
                I2cHelperAction::ListBuses,
            ),
            I2cToolsAction::ExecuteTransfer(transactions) => (
                I2cToolsOperationKind::Transfer,
                I2cHelperAction::Transfer { transactions },
            ),
            I2cToolsAction::Cancel => unreachable!("cancel returns before worker dispatch"),
        };
        let credential_ref = format!("session:{slot_id}");
        let connection = SshConnectionTarget {
            host: source.host,
            port: source.port,
            username: source.username,
            expected_host_key: None,
            command_subsystem: None,
            remote_event_subsystem: None,
        };
        let cancellation = DumpCancellation::default();
        self.active_i2c_tools_cancellation = Some(cancellation.clone());
        self.active_i2c_tools_cancellable = true;
        let sender = self.i2c_tools_sender.clone();
        let worker_context = context.clone();
        let resolver = self.live_runtime.ssh_resolver();
        let transport = Arc::new(RusshTransportFactory);
        let spawn_result = thread::Builder::new()
            .name("camera-toolbox-i2c-tools".to_owned())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_i2c_tools_request(
                        connection,
                        credential_ref,
                        helper_payload,
                        resolver,
                        transport,
                        helper_action,
                        cancellation,
                    )
                }))
                .unwrap_or_else(|_| Err("I²C Tools worker panicked".to_owned()));
                let _ = sender.send(I2cToolsOperationResult { kind, result });
                worker_context.request_repaint();
            });
        if let Err(error) = spawn_result {
            self.active_i2c_tools_cancellation = None;
            self.active_i2c_tools_cancellable = false;
            self.i2c_tools
                .report_error(format!("Failed to start I²C Tools worker: {error}"));
        }
    }

    #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
    fn begin_eeprom_operation(
        &mut self,
        context: &egui::Context,
        intent: CalibrationProvisionIntent,
    ) {
        let intent = match intent {
            CalibrationProvisionIntent::ConfigureTarget(request) => {
                if self.active_eeprom_cancellation.is_some() {
                    self.calibration.report_provision_error(
                        "Wait for the active EEPROM operation to finish before reconfiguring its target.",
                    );
                    return;
                }
                self.invalidate_eeprom_target(
                    "Reconfiguring EEPROM target; previous Inspect and Write authorization are invalid.",
                );
                match self.configure_eeprom_target(request) {
                    Ok(label) => self.calibration.report_target_configured(&label),
                    Err(error) => self.calibration.report_target_configuration_failed(error),
                }
                return;
            }
            other => other,
        };
        if matches!(&intent, CalibrationProvisionIntent::Cancel) {
            if !self.active_eeprom_cancellable {
                self.calibration
                    .report_provision_error("No cancellable EEPROM read operation is active.");
            } else if let Some(cancellation) = &self.active_eeprom_cancellation {
                cancellation.cancel();
            }
            return;
        }
        if self.active_eeprom_cancellation.is_some() {
            self.calibration
                .report_provision_error("An EEPROM operation is already active.");
            return;
        }
        if matches!(&intent, CalibrationProvisionIntent::DiscoverBuses) {
            let source = match self.explorer.connected_sftp_connection().cloned() {
                Some(source) => source,
                None => {
                    self.calibration.report_bus_discovery_failed(
                        "Connect Explorer SFTP before discovering I²C buses.",
                    );
                    return;
                }
            };
            let camera_toolbox_app::RemoteAuthentication::Password { slot_id } =
                &source.authentication
            else {
                self.calibration.report_bus_discovery_failed(
                    "I²C bus discovery requires the Explorer SFTP process-only password",
                );
                return;
            };
            let credential_ref = format!("session:{slot_id}");
            let connection = SshConnectionTarget {
                host: source.host.clone(),
                port: source.port,
                username: source.username.clone(),
                expected_host_key: None,
                command_subsystem: None,
                remote_event_subsystem: None,
            };
            let helper_payload = match Self::read_local_eeprom_helper_payload() {
                Ok(payload) => payload,
                Err(error) => {
                    self.calibration.report_bus_discovery_failed(error);
                    return;
                }
            };
            let operation_id = self.next_eeprom_operation;
            self.next_eeprom_operation = self.next_eeprom_operation.wrapping_add(1).max(1);
            let cancellation = DumpCancellation::default();
            self.active_eeprom_cancellation = Some(cancellation.clone());
            self.active_eeprom_cancellable = true;
            let sender = self.eeprom_operation_sender.clone();
            let worker_context = context.clone();
            let target_label = format!(
                "{}@{}:{} / i2c-buses",
                source.username, source.host, source.port,
            );
            let resolver = self.live_runtime.ssh_resolver();
            let transport = Arc::new(RusshTransportFactory);
            let spawn_result = thread::Builder::new()
                .name("camera-toolbox-i2c-discovery".to_owned())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_i2c_bus_discovery(
                            connection,
                            credential_ref,
                            helper_payload,
                            resolver,
                            transport,
                            operation_id,
                            cancellation,
                        )
                    }))
                    .unwrap_or_else(|_| {
                        Err(EepromOperationFailure::known(
                            "I²C bus discovery worker panicked",
                        ))
                    });
                    let _ = sender.send(EepromOperationResult {
                        kind: EepromOperationKind::BusDiscovery,
                        target_label,
                        result,
                    });
                    worker_context.request_repaint();
                });
            if let Err(error) = spawn_result {
                self.active_eeprom_cancellation = None;
                self.active_eeprom_cancellable = false;
                self.calibration.report_bus_discovery_failed(format!(
                    "Failed to start I²C bus discovery worker: {error}"
                ));
            }
            return;
        }

        let target = match self.eeprom_target.clone() {
            Some(target) => target,
            None => {
                self.calibration.report_provision_error(
                    "Configure the EEPROM SSH target in the Calibration panel.",
                );
                return;
            }
        };
        if let CalibrationProvisionIntent::Provision { request, .. } = &intent
            && let Err(error) = ensure_eeprom_history_slot_available(&request.serial_number)
        {
            self.calibration.report_provision_error(error);
            return;
        }
        let operation_id = self.next_eeprom_operation;
        self.next_eeprom_operation = self.next_eeprom_operation.wrapping_add(1).max(1);
        let cancellation = DumpCancellation::default();
        self.active_eeprom_cancellation = Some(cancellation.clone());
        self.active_eeprom_cancellable =
            !matches!(&intent, CalibrationProvisionIntent::Provision { .. });
        let sender = self.eeprom_operation_sender.clone();
        let worker_context = context.clone();
        let target_label = target.label.clone();
        let provision_attempt = matches!(&intent, CalibrationProvisionIntent::Provision { .. });
        let kind = match &intent {
            CalibrationProvisionIntent::Inspect => EepromOperationKind::Inspect,
            CalibrationProvisionIntent::Provision { .. } => EepromOperationKind::Provision,
            CalibrationProvisionIntent::Cancel
            | CalibrationProvisionIntent::ConfigureTarget(_)
            | CalibrationProvisionIntent::DiscoverBuses => {
                unreachable!("non-worker EEPROM intents return before worker dispatch")
            }
        };
        let spawn_result = thread::Builder::new()
            .name("camera-toolbox-eeprom-operation".to_owned())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_eeprom_operation(target, intent, operation_id, cancellation)
                }))
                .unwrap_or_else(|_| {
                    Err(if provision_attempt {
                        EepromOperationFailure::unknown("EEPROM operation worker panicked")
                    } else {
                        EepromOperationFailure::known("EEPROM operation worker panicked")
                    })
                });
                let _ = sender.send(EepromOperationResult {
                    kind,
                    target_label,
                    result,
                });
                worker_context.request_repaint();
            });
        if let Err(error) = spawn_result {
            self.active_eeprom_cancellation = None;
            self.active_eeprom_cancellable = false;
            self.calibration.report_provision_error(format!(
                "Failed to start EEPROM operation worker: {error}"
            ));
        }
    }

    #[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
    fn poll_eeprom_operation_result(&mut self, _context: &egui::Context) {
        while let Ok(operation) = self.eeprom_operation_receiver.try_recv() {
            self.active_eeprom_cancellation = None;
            self.active_eeprom_cancellable = false;
            match operation.result {
                Ok(EepromOperationOutcome::Inspect(result)) => self
                    .calibration
                    .report_eeprom_inspect(operation.target_label, result),
                Ok(EepromOperationOutcome::BusDiscovery { buses }) => {
                    self.calibration.report_bus_discovery(buses);
                }
                Ok(EepromOperationOutcome::Provision {
                    result,
                    history_file,
                }) => self.calibration.report_eeprom_provision(
                    operation.target_label,
                    &result,
                    history_file,
                ),
                Ok(EepromOperationOutcome::ProvisionAuditFailed { result, error }) => self
                    .calibration
                    .report_eeprom_provision_audit_error(operation.target_label, &result, &error),
                Err(error) if error.provision_state_unknown => self
                    .calibration
                    .report_eeprom_provision_unknown(error.message),
                Err(error) if operation.kind == EepromOperationKind::BusDiscovery => {
                    self.calibration.report_bus_discovery_failed(error.message);
                }
                Err(error) => self.calibration.report_provision_error(error.message),
            }
        }
    }

    #[cfg(feature = "platform-ssh")]
    fn poll_i2c_tools_result(&mut self) {
        while let Ok(operation) = self.i2c_tools_receiver.try_recv() {
            self.active_i2c_tools_cancellation = None;
            self.active_i2c_tools_cancellable = false;
            match (operation.kind, operation.result) {
                (I2cToolsOperationKind::BusDiscovery, Ok(I2cHelperResult::BusList { buses })) => {
                    self.i2c_tools.report_buses(buses)
                }
                (
                    I2cToolsOperationKind::Transfer,
                    Ok(I2cHelperResult::Transfer { transactions }),
                ) => self.i2c_tools.report_transfer(transactions),
                (kind, Ok(unexpected)) => self.i2c_tools.report_error(format!(
                    "I²C helper returned an unexpected result for {kind:?}: {unexpected:?}"
                )),
                (_, Err(error)) => self.i2c_tools.report_error(error),
            }
        }
    }

    fn active_yuv_save_defaults(&self) -> (YuvMatrix, YuvRange) {
        if let Some(document) = self.workspace.active_image()
            && let NativeImage::Yuv420Sp(frame) = &document.native
        {
            return (frame.spec.matrix, frame.spec.range);
        }
        (YuvMatrix::Bt601, YuvRange::Limited)
    }

    fn poll_save_result(&mut self, context: &egui::Context) {
        while let Some(result) = self.save_worker.take_ready() {
            if result.result.is_ok() {
                self.explorer
                    .refresh_save_destination(&result.destination, context);
            }
            self.install_save_result(result);
        }
    }

    fn install_save_result(&mut self, result: SaveResult) {
        match result.result {
            Ok(bytes_written) => {
                if let Some(document) = self.workspace.image_mut(result.key.document_id)
                    && document.generation == result.key.generation
                {
                    document.unsaved = false;
                }
                tracing::debug!(
                    operation = "save_image",
                    document_id = %result.key.document_id,
                    generation = result.key.generation,
                    revision = result.key.revision,
                    path = %result.target_label,
                    bytes_written,
                    format = ?result.format,
                    "saved image"
                );
            }
            Err(error) => self.notify_save_error(result.key, error),
        }
    }

    fn notify_save_error(&mut self, key: SaveKey, error: String) {
        tracing::error!(
            operation = "save_image",
            document_id = %key.document_id,
            generation = key.generation,
            revision = key.revision,
            error = %error,
            "image save failed"
        );
        self.notifications.push_once(UiNotification::error(
            NotificationKey::SaveFailed {
                generation: key.generation,
                revision: key.revision,
            },
            "Image save failed",
            error,
        ));
    }

    fn start_load_raw(
        &mut self,
        context: &egui::Context,
        attempt: u64,
        request: LocalRawAnalyzeRequest,
    ) {
        if let Some(active) = self.active_raw_open.take() {
            active.cancellation.cancel();
        }
        self.cancel_active_auto_open();
        let cancellation = FsCancellation::default();
        let path = request.path.clone();
        self.active_raw_open = Some(ActiveRawOpenJob {
            attempt,
            path: path.clone(),
            remote: false,
            progress: None,
            cancellation: cancellation.clone(),
        });
        let pipeline = self.raw_pipeline.clone();
        let sender = self.raw_open_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let result = decode_raw_request(&pipeline, request, cancellation)
                .map(OpenedFileDocument::Raw)
                .map_err(|error| error.to_string());
            let _ = sender.send(RawOpenJobEvent::Finished(Box::new(RawOpenJobResult {
                attempt,
                path,
                result,
            })));
            context.request_repaint();
        });
    }

    fn start_load_workspace_file(
        &mut self,
        context: &egui::Context,
        attempt: u64,
        request: WorkspaceFileOpenRequest,
        mode: ImageOpenMode,
    ) {
        if let Some(active) = self.active_raw_open.take() {
            active.cancellation.cancel();
        }
        let cancellation = FsCancellation::default();
        self.cancel_active_auto_open();
        let path = request.display_path.clone();
        let remote = request.remote;
        self.active_raw_open = Some(ActiveRawOpenJob {
            attempt,
            path: path.clone(),
            remote,
            progress: None,
            cancellation: cancellation.clone(),
        });
        let pipeline = self.image_pipeline.clone();
        let sender = self.raw_open_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let mut last_repaint = Instant::now()
                .checked_sub(RAW_PROGRESS_REPAINT_INTERVAL)
                .unwrap_or_else(Instant::now);
            let mut report_progress = |progress: SourceReadProgress| {
                let final_update = progress.bytes_read == progress.total_bytes;
                if progress.bytes_read == 0
                    || final_update
                    || last_repaint.elapsed() >= RAW_PROGRESS_REPAINT_INTERVAL
                {
                    let _ = sender.send(RawOpenJobEvent::Progress { attempt, progress });
                    last_repaint = Instant::now();
                    context.request_repaint();
                }
            };
            let result = decode_workspace_image_request(
                &pipeline,
                request,
                mode,
                cancellation,
                &mut report_progress,
            )
            .map_err(|error| error.to_string());
            let _ = sender.send(RawOpenJobEvent::Finished(Box::new(RawOpenJobResult {
                attempt,
                path,
                result,
            })));
            context.request_repaint();
        });
    }

    fn poll_raw_open_result(&mut self, context: &egui::Context) {
        loop {
            let event = match self.raw_open_receiver.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            match event {
                RawOpenJobEvent::Progress { attempt, progress } => {
                    if let Some(active) = self
                        .active_raw_open
                        .as_mut()
                        .filter(|active| active.attempt == attempt && active.remote)
                    {
                        active.progress = Some(progress);
                    }
                }
                RawOpenJobEvent::Finished(result) => {
                    let result = *result;
                    if !self
                        .active_raw_open
                        .as_ref()
                        .is_some_and(|active| active.attempt == result.attempt)
                    {
                        continue;
                    }
                    self.active_raw_open = None;
                    match result.result {
                        Ok(OpenedFileDocument::Raw(completed)) => {
                            self.install_opened_raw(context, result.attempt, completed);
                        }
                        Ok(OpenedFileDocument::Image(completed)) => {
                            self.install_opened_image(
                                context,
                                result.attempt,
                                result.path.clone(),
                                completed,
                            );
                        }
                        Err(message) => {
                            tracing::error!(
                                operation = "load_image",
                                attempt = result.attempt,
                                path = %result.path.display(),
                                error = %message,
                                "image loading failed"
                            );
                            self.notifications.push_once(UiNotification::error(
                                NotificationKey::RawLoadFailed {
                                    attempt: result.attempt,
                                },
                                "Image load failed",
                                &message,
                            ));
                            self.raw_dialog.set_error(message);
                        }
                    }
                }
            }
        }
    }

    fn poll_auto_open_result(&mut self, context: &egui::Context) {
        loop {
            let result = match self.auto_open_receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            let Some(active) = self.active_auto_open.as_ref() else {
                continue;
            };
            if active.candidate.rule_id != result.candidate.rule_id
                || active.candidate.reference != result.candidate.reference
            {
                continue;
            }
            self.active_auto_open = None;
            match result.result {
                Ok(OpenedFileDocument::Raw(completed)) => {
                    self.install_auto_opened_raw(context, result.candidate, completed)
                }
                Ok(OpenedFileDocument::Image(completed)) => self.install_auto_opened_image(
                    context,
                    result.candidate,
                    result.path,
                    completed,
                ),
                Err(message) => {
                    tracing::error!(
                        operation = "auto_open_raw",
                        rule_id = result.candidate.rule_id.as_str(),
                        path = %result.path.display(),
                        error = %message,
                        "auto-open RAW loading failed"
                    );
                }
            }
        }
    }

    fn install_opened_raw(
        &mut self,
        context: &egui::Context,
        attempt: u64,
        completed: OpenedRawDocument,
    ) {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let loaded = LoadedRaw::from_report(context, completed.report, generation);
        let notification = loaded.diagnostics.first_out_of_range().map(|first| {
            UiNotification::raw_range(
                generation,
                loaded.frame.spec.bit_depth,
                loaded.frame.spec.max_code_value(),
                loaded.diagnostics.out_of_range_pixels,
                loaded.diagnostics.observed_max,
                (
                    first.x,
                    first.y,
                    first.raw_value.expect("RAW diagnostic carries a RAW code"),
                ),
                context.input(|input| input.time),
            )
        });
        tracing::debug!(
            operation = "load_raw",
            attempt,
            generation,
            path = %loaded.path.display(),
            width = loaded.frame.spec.width,
            height = loaded.frame.spec.height,
            bit_depth = loaded.frame.spec.bit_depth,
            "RAW loaded"
        );
        if loaded.diagnostics.has_out_of_range() {
            tracing::warn!(
                attempt,
                operation = "load_raw",
                generation,
                path = %loaded.path.display(),
                bit_depth = loaded.frame.spec.bit_depth,
                out_of_range_pixels = loaded.diagnostics.out_of_range_pixels,
                observed_max = loaded.diagnostics.observed_max,
                "RAW samples exceed declared bit-depth range"
            );
        }
        let document_id = self.workspace.open_file_raw(
            loaded,
            completed.source,
            completed.interpretation,
            generation,
            true,
        );
        if let Some(notification) = notification {
            self.notifications.push_once(notification);
        }
        tracing::debug!(
            operation = "open_document",
            document_id = %document_id,
            generation,
            "opened local RAW in new tab"
        );
        self.request_current_color();
        self.workspace.enforce_derived_budget();
    }

    fn install_opened_image(
        &mut self,
        context: &egui::Context,
        attempt: u64,
        path: PathBuf,
        completed: ImageOpenResult,
    ) {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let dimensions = completed.native.dimensions();
        let kind = format!("{:?}", completed.kind);
        let document_id = match self
            .workspace
            .open_image(generation, path.clone(), completed, true)
        {
            Ok(document_id) => document_id,
            Err(error) => {
                tracing::error!(
                    operation = "open_image_document",
                    attempt,
                    generation,
                    path = %path.display(),
                    error = %error,
                    "failed to create static image document"
                );
                self.notifications.push_once(UiNotification::error(
                    NotificationKey::RawLoadFailed { attempt },
                    "Image open failed",
                    &error,
                ));
                return;
            }
        };
        let texture_error = self
            .workspace
            .active_image_mut()
            .and_then(|document| document.ensure_texture(context).err());
        if let Some(error) = texture_error {
            self.notifications.push_once(UiNotification::error(
                NotificationKey::RawLoadFailed { attempt },
                "Image texture failed",
                &error,
            ));
        }
        tracing::debug!(
            operation = "open_image_document",
            document_id = %document_id,
            attempt,
            generation,
            path = %path.display(),
            width = dimensions[0],
            height = dimensions[1],
            kind,
            "opened static image in new tab"
        );
        self.workspace.enforce_derived_budget();
    }

    fn install_auto_opened_image(
        &mut self,
        context: &egui::Context,
        candidate: AutoOpenCandidate,
        path: PathBuf,
        completed: ImageOpenResult,
    ) {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let dimensions = completed.native.dimensions();
        let kind = format!("{:?}", completed.kind);
        let foreground = if candidate.activation == AutoOpenActivation::FollowLatest {
            let existing_id = self.auto_open_documents.remove(candidate.rule_id.as_str());
            let was_active = existing_id.is_some_and(|id| self.workspace.active_id() == Some(id));
            if let Some(id) = existing_id {
                self.close_document(context, id);
            }
            was_active
        } else {
            false
        };
        let document_id =
            match self
                .workspace
                .open_image(generation, path.clone(), completed, foreground)
            {
                Ok(document_id) => document_id,
                Err(error) => {
                    tracing::error!(
                        operation = "auto_open_image",
                        rule_id = candidate.rule_id.as_str(),
                        generation,
                        path = %path.display(),
                        error = %error,
                        "failed to create auto-open static image document"
                    );
                    return;
                }
            };
        match candidate.activation {
            AutoOpenActivation::FollowLatest => {
                self.auto_open_documents
                    .insert(candidate.rule_id.as_str().to_owned(), document_id);
            }
            AutoOpenActivation::NewBackgroundTab => {
                self.track_auto_open_background_tab(context, document_id);
            }
        }
        tracing::debug!(
            operation = "auto_open_image",
            rule_id = candidate.rule_id.as_str(),
            document_id = %document_id,
            generation,
            path = %path.display(),
            width = dimensions[0],
            height = dimensions[1],
            kind,
            "auto-opened static image"
        );
        self.workspace.enforce_derived_budget();
    }

    fn install_auto_opened_raw(
        &mut self,
        context: &egui::Context,
        candidate: AutoOpenCandidate,
        completed: OpenedRawDocument,
    ) {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let loaded = LoadedRaw::from_report(context, completed.report, generation);
        let notification = loaded.diagnostics.first_out_of_range().map(|first| {
            UiNotification::raw_range(
                generation,
                loaded.frame.spec.bit_depth,
                loaded.frame.spec.max_code_value(),
                loaded.diagnostics.out_of_range_pixels,
                loaded.diagnostics.observed_max,
                (
                    first.x,
                    first.y,
                    first.raw_value.expect("RAW diagnostic carries a RAW code"),
                ),
                context.input(|input| input.time),
            )
        });
        tracing::debug!(
            operation = "auto_open_raw",
            rule_id = candidate.rule_id.as_str(),
            generation,
            path = %loaded.path.display(),
            width = loaded.frame.spec.width,
            height = loaded.frame.spec.height,
            bit_depth = loaded.frame.spec.bit_depth,
            "auto-open RAW loaded"
        );
        if loaded.diagnostics.has_out_of_range() {
            tracing::warn!(
                operation = "auto_open_raw",
                rule_id = candidate.rule_id.as_str(),
                generation,
                path = %loaded.path.display(),
                bit_depth = loaded.frame.spec.bit_depth,
                out_of_range_pixels = loaded.diagnostics.out_of_range_pixels,
                observed_max = loaded.diagnostics.observed_max,
                "auto-open RAW samples exceed declared bit-depth range"
            );
        }
        let mut foreground = false;
        if candidate.activation == AutoOpenActivation::FollowLatest
            && let Some(document_id) = self.auto_open_documents.remove(candidate.rule_id.as_str())
        {
            let was_active = self.workspace.active_id() == Some(document_id);
            let previous_generation = self
                .workspace
                .document(document_id)
                .map(|document| document.loaded.generation);
            if let Some(previous_generation) = previous_generation {
                let replaced = self.workspace.replace_file_raw(
                    document_id,
                    loaded,
                    completed.source,
                    completed.interpretation,
                    generation,
                );
                debug_assert!(
                    replaced,
                    "existing follow-latest document must still be replaceable"
                );
                self.auto_open_documents
                    .insert(candidate.rule_id.as_str().to_owned(), document_id);
                self.notifications
                    .clear_scope(NotificationScope::ImageGeneration(previous_generation));
                if let Some(notification) = notification {
                    self.notifications.push_once(notification);
                }
                if was_active {
                    self.request_current_color();
                }
                self.workspace.enforce_derived_budget();
                return;
            }
            if was_active {
                foreground = true;
            }
            self.close_document(context, document_id);
        }
        let document_id = self.workspace.open_file_raw(
            loaded,
            completed.source,
            completed.interpretation,
            generation,
            foreground,
        );
        match candidate.activation {
            AutoOpenActivation::FollowLatest => {
                self.auto_open_documents
                    .insert(candidate.rule_id.as_str().to_owned(), document_id);
            }
            AutoOpenActivation::NewBackgroundTab => {
                self.track_auto_open_background_tab(context, document_id);
            }
        }
        if let Some(notification) = notification {
            self.notifications.push_once(notification);
        }
        if self.workspace.active_id() == Some(document_id) {
            self.request_current_color();
        }
        self.workspace.enforce_derived_budget();
    }

    fn start_reinterpret(
        &mut self,
        context: &egui::Context,
        document_id: DocumentId,
        decode_generation: u64,
        source: ImageSourceHandle,
        params: RawDecodeParams,
        roi: Roi,
        path: PathBuf,
    ) {
        let pipeline = self.raw_pipeline.clone();
        let sender = self.reinterpret_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let result =
                decode_raw_reinterpret(&pipeline, source, params, decode_generation, roi, path)
                    .map_err(|error| error.to_string());
            let _ = sender.send(ReinterpretJobResult {
                document_id,
                decode_generation,
                result,
            });
            context.request_repaint();
        });
    }

    fn poll_reinterpret_result(&mut self, context: &egui::Context) {
        loop {
            let result = match self.reinterpret_receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            match result.result {
                Ok(completed) => self.install_reinterpreted_raw(
                    context,
                    result.document_id,
                    result.decode_generation,
                    completed,
                ),
                Err(message) => {
                    if let Some(document) = self.workspace.document_mut(result.document_id) {
                        document
                            .raw_inspector
                            .mark_error(Some(result.decode_generation), message);
                    }
                }
            }
        }
    }

    fn start_yuv_reinterpret(
        &mut self,
        context: &egui::Context,
        document_id: DocumentId,
        decode_generation: u64,
        source: ImageSourceHandle,
        kind: ImageFileKind,
        spec: Yuv420SpSpec,
    ) {
        let pipeline = self.image_pipeline.clone();
        let sender = self.yuv_reinterpret_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let result = decode_yuv_reinterpret(&pipeline, source, kind, spec)
                .map_err(|error| error.to_string());
            let _ = sender.send(YuvReinterpretJobResult {
                document_id,
                decode_generation,
                result,
            });
            context.request_repaint();
        });
    }

    fn poll_yuv_reinterpret_result(&mut self, context: &egui::Context) {
        loop {
            let result = match self.yuv_reinterpret_receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            match result.result {
                Ok(completed) => self.install_reinterpreted_yuv(
                    context,
                    result.document_id,
                    result.decode_generation,
                    completed,
                ),
                Err(message) => {
                    if let Some(document) = self.workspace.image_mut(result.document_id)
                        && document.decode_generation == result.decode_generation
                        && let Some(inspector) = &mut document.yuv_inspector
                    {
                        inspector.mark_error(result.decode_generation, message);
                    }
                }
            }
        }
    }

    fn install_reinterpreted_yuv(
        &mut self,
        _context: &egui::Context,
        document_id: DocumentId,
        decode_generation: u64,
        completed: ImageOpenResult,
    ) {
        let Some(document) = self.workspace.image(document_id) else {
            return;
        };
        if document.decode_generation != decode_generation {
            return;
        }
        let Some(display) = completed.display else {
            return;
        };
        if !matches!(completed.native, NativeImage::Yuv420Sp(_)) {
            return;
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        if let Some(document) = self.workspace.image_mut(document_id) {
            let _ = document.install_reinterpreted_yuv(
                generation,
                decode_generation,
                completed.native,
                display,
            );
        }
    }

    fn install_reinterpreted_raw(
        &mut self,
        context: &egui::Context,
        document_id: DocumentId,
        decode_generation: u64,
        completed: OpenedRawDocument,
    ) {
        let Some(previous_generation) = self.workspace.document(document_id).and_then(|document| {
            (document.decode_generation == decode_generation).then_some(document.loaded.generation)
        }) else {
            return;
        };
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let loaded = LoadedRaw::from_report(context, completed.report, generation);
        let notification = loaded.diagnostics.first_out_of_range().map(|first| {
            UiNotification::raw_range(
                generation,
                loaded.frame.spec.bit_depth,
                loaded.frame.spec.max_code_value(),
                loaded.diagnostics.out_of_range_pixels,
                loaded.diagnostics.observed_max,
                (
                    first.x,
                    first.y,
                    first.raw_value.expect("RAW diagnostic carries a RAW code"),
                ),
                context.input(|input| input.time),
            )
        });
        tracing::debug!(
            operation = "reinterpret_raw",
            document_id = %document_id,
            generation,
            decode_generation,
            path = %loaded.path.display(),
            width = loaded.frame.spec.width,
            height = loaded.frame.spec.height,
            bit_depth = loaded.frame.spec.bit_depth,
            "installed reinterpreted RAW"
        );
        if loaded.diagnostics.has_out_of_range() {
            tracing::warn!(
                operation = "reinterpret_raw",
                document_id = %document_id,
                generation,
                decode_generation,
                path = %loaded.path.display(),
                bit_depth = loaded.frame.spec.bit_depth,
                out_of_range_pixels = loaded.diagnostics.out_of_range_pixels,
                observed_max = loaded.diagnostics.observed_max,
                "RAW samples exceed declared bit-depth range"
            );
        }
        let Some(document) = self.workspace.document_mut(document_id) else {
            return;
        };
        if document.decode_generation != decode_generation {
            return;
        }
        document.install_reinterpreted(
            loaded,
            completed.source,
            completed.interpretation,
            decode_generation,
        );
        self.notifications
            .clear_scope(NotificationScope::ImageGeneration(previous_generation));
        if let Some(notification) = notification {
            self.notifications.push_once(notification);
        }
        if self.workspace.active_id() == Some(document_id) {
            self.request_current_color();
        }
        self.workspace.enforce_derived_budget();
    }

    fn request_current_color(&mut self) {
        let request = {
            let Some(document) = self.workspace.active_mut() else {
                return;
            };
            let loaded = &mut document.loaded;
            let revision = loaded.color_edit.revision;
            if loaded.color_edit.submitted_revision == Some(revision) {
                return;
            }
            if let Err(error) = loaded
                .color_edit
                .params
                .validate(loaded.frame.spec.max_code_value())
            {
                tracing::warn!(
                    operation = "validate_color_params",
                    document_id = %document.id,
                    generation = loaded.generation,
                    revision,
                    error = %error,
                    "color parameter validation rejected"
                );
                loaded.color_edit.mark_error(error.to_string());
                return;
            }
            let request = ColorRenderRequest {
                document_id: document.id,
                frame_generation: loaded.generation,
                revision,
                frame: Arc::clone(&loaded.frame),
                params: loaded.color_edit.params,
            };
            loaded.color_edit.mark_submitted();
            request
        };
        tracing::debug!(
            operation = "submit_color_render",
            document_id = %request.document_id,
            generation = request.frame_generation,
            revision = request.revision,
            "submitted color render"
        );
        self.workspace
            .supersede_color_submissions_except(request.document_id);
        self.color_worker.submit(request);
    }

    fn poll_color_result(&mut self, context: &egui::Context) {
        let Some(result) = self.color_worker.take_ready() else {
            return;
        };
        self.install_color_result(context, result);
    }

    fn install_color_result(&mut self, context: &egui::Context, result: ColorRenderResult) {
        let identity = DocumentIdentity {
            document_id: result.document_id,
            generation: result.frame_generation,
        };
        let Some(document) = self.workspace.matching_document_mut(identity) else {
            tracing::debug!(
                operation = "poll_color_result",
                document_id = %result.document_id,
                generation = result.frame_generation,
                revision = result.revision,
                "dropped color result for closed or replaced document"
            );
            return;
        };
        if document.loaded.color_edit.revision != result.revision {
            tracing::debug!(
                operation = "poll_color_result",
                document_id = %result.document_id,
                generation = result.frame_generation,
                revision = result.revision,
                "dropped stale color result"
            );
            return;
        }
        match result.rendered {
            Ok(rendered) => {
                document.loaded.install_color(
                    context,
                    result.revision,
                    result.params,
                    rendered.image,
                    rendered.frame,
                    rendered.diagnostics,
                );
                document.loaded.color_edit.render_error = None;
                document.mark_derived_loaded();
                tracing::debug!(
                    operation = "install_color_render",
                    document_id = %document.id,
                    generation = document.loaded.generation,
                    revision = result.revision,
                    "installed color render"
                );
            }
            Err(error) => {
                document.loaded.color_edit.mark_error(error.clone());
                tracing::error!(
                    operation = "render_color",
                    document_id = %document.id,
                    generation = document.loaded.generation,
                    revision = result.revision,
                    error = %error,
                    "accepted color render failed"
                );

                self.notifications.push_once(UiNotification::error(
                    NotificationKey::ColorRenderFailed {
                        generation: document.loaded.generation,
                        revision: result.revision,
                    },
                    "Color preview failed",
                    error,
                ));
            }
        }
    }

    fn ensure_active_resources(&mut self, context: &egui::Context) {
        let should_request_color = if let Some(document) = self.workspace.active_mut() {
            document.loaded.ensure_raw_texture(context);
            document.display_mode == DisplayMode::Color
                && document.loaded.installed_revision() != Some(document.loaded.color_edit.revision)
                && document.loaded.color_edit.submitted_revision
                    != Some(document.loaded.color_edit.revision)
        } else {
            false
        };
        if should_request_color {
            self.request_current_color();
        }
    }

    fn handle_platform_ui_action(&mut self, context: &egui::Context, action: PlatformUiAction) {
        match action {
            PlatformUiAction::OpenRaw => self.raw_dialog.open(context),
            PlatformUiAction::Stream(action) => self.handle_stream_panel_action(action),
        }
    }

    fn handle_platform_effect(&mut self, context: &egui::Context, effect: PlatformEffect) {
        let PlatformEffect::OpenAsset {
            asset,
            snapshot,
            foreground,
            spec,
        } = effect;
        let result = match asset.metadata.format {
            MediaFormat::RawPacked { bit_depth } => self
                .open_packed_raw_asset(context, asset, snapshot, spec.bayer, bit_depth, foreground),
            MediaFormat::Jpeg | MediaFormat::Png | MediaFormat::Yuv420Sp { .. } => {
                self.open_captured_raster_asset(asset, snapshot, foreground)
            }
            ref format => Err(format!(
                "captured asset format {format:?} cannot be opened as an image"
            )),
        };
        if let Err(error) = result {
            self.live_runtime.panel.last_error = Some(error);
        }
    }

    fn open_packed_raw_asset(
        &mut self,
        context: &egui::Context,
        asset: Arc<camera_toolbox_core::EphemeralAsset>,
        snapshot: Arc<camera_toolbox_app::TargetResolutionSnapshot>,
        bayer: camera_toolbox_core::BayerPattern,
        bit_depth: u8,
        foreground: bool,
    ) -> Result<(), String> {
        let attribute = |name: &str| -> Result<usize, String> {
            asset
                .metadata
                .attributes
                .get(name)
                .ok_or_else(|| format!("captured RAW metadata is missing {name}"))?
                .parse::<usize>()
                .map_err(|error| format!("captured RAW metadata {name} is invalid: {error}"))
        };
        let width = u32::try_from(attribute("width")?)
            .map_err(|_| "captured RAW width does not fit u32".to_owned())?;
        let height = u32::try_from(attribute("height")?)
            .map_err(|_| "captured RAW height does not fit u32".to_owned())?;
        let stride = attribute("stride")?;
        let bytes = match &asset.source {
            OwnedMediaPayload::Bytes(bytes) => bytes.as_ref(),
            OwnedMediaPayload::Planes(_) => {
                return Err("packed RAW source must be one contiguous payload".to_owned());
            }
        };
        let frame = decode_le_continuous_raw(
            PackedRawSpec {
                width,
                height,
                stride,
                bit_depth,
            },
            bayer,
            bytes,
        )
        .map_err(|error| error.to_string())?;
        let roi = Roi {
            x: 0,
            y: 0,
            width,
            height,
        };
        let stats = analyze_roi(&frame, roi).map_err(|error| error.to_string())?;
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let loaded = LoadedRaw::from_report(
            context,
            LocalRawAnalyzeReport {
                path: std::path::PathBuf::from(&asset.metadata.source_name),
                frame,
                roi,
                stats,
            },
            generation,
        );
        self.workspace
            .open_captured_raw(loaded, asset, snapshot, foreground);
        Ok(())
    }

    fn open_captured_raster_asset(
        &mut self,
        asset: Arc<camera_toolbox_core::EphemeralAsset>,
        snapshot: Arc<camera_toolbox_app::TargetResolutionSnapshot>,
        foreground: bool,
    ) -> Result<(), String> {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let (native, display) = match asset.metadata.format {
            MediaFormat::Jpeg | MediaFormat::Png => {
                let bytes = asset_payload_bytes(&asset.source)?;
                let format = if matches!(asset.metadata.format, MediaFormat::Png) {
                    camera_toolbox_app::RasterFormat::Png
                } else {
                    camera_toolbox_app::RasterFormat::Jpeg
                };
                let frame = Arc::new(
                    ImageRasterCodec
                        .decode_rgba8(format, bytes, CAPTURED_RASTER_DECODE_BYTES)
                        .map_err(|error| error.to_string())?,
                );
                (NativeImage::Rgba8(Arc::clone(&frame)), frame)
            }
            MediaFormat::Yuv420Sp { chroma_order } => {
                let width = u32::try_from(asset_attribute_usize(&asset, "width")?)
                    .map_err(|_| "captured YUV width does not fit u32".to_owned())?;
                let height = u32::try_from(asset_attribute_usize(&asset, "height")?)
                    .map_err(|_| "captured YUV height does not fit u32".to_owned())?;
                let spec = Yuv420SpSpec {
                    width,
                    height,
                    y_stride: asset_attribute_usize(&asset, "y_stride")?,
                    chroma_stride: asset_attribute_usize(&asset, "chroma_stride")?,
                    chroma_order,
                    matrix: YuvMatrix::Bt601,
                    range: YuvRange::Limited,
                };
                let bytes = Arc::new(asset_payload_bytes(&asset.source)?.to_vec());
                let yuv = Arc::new(
                    Yuv420SpFrame::from_contiguous(spec, bytes)
                        .map_err(|error| error.to_string())?,
                );
                let display = Arc::new(
                    yuv420sp_to_rgba8_with_cancel(&yuv, &|| false)
                        .map_err(|error| error.to_string())?,
                );
                (NativeImage::Yuv420Sp(yuv), display)
            }
            ref format => {
                return Err(format!(
                    "captured media format {format:?} cannot be opened as an image"
                ));
            }
        };
        self.workspace
            .open_captured_image(generation, asset, snapshot, native, display, foreground)?;
        Ok(())
    }

    fn capture_live_frame_for_color(
        &mut self,
        context: &egui::Context,
        id: DocumentId,
    ) -> Result<(), String> {
        let frame = self
            .workspace
            .live(id)
            .and_then(|document| document.displayed_frame().cloned())
            .ok_or_else(|| "active RTSP stream has no displayed frame to capture".to_owned())?;
        let display = Arc::new(
            Rgba8Frame::tight(frame.width, frame.height, Arc::clone(&frame.rgba))
                .map_err(|error| error.to_string())?,
        );
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let source_name = format!(
            "color-rtsp-ch{}-frame{}.png",
            frame.identity.channel, frame.identity.frame_sequence
        );
        let document_id = self.workspace.open_generated_capture(
            generation,
            source_name.clone(),
            Arc::clone(&display),
            true,
        );
        if let Some(document) = self.workspace.image_mut(document_id) {
            document.ensure_texture(context)?;
        }
        self.color_inspection.analyze_frame(
            document_id,
            generation,
            source_name,
            Arc::clone(&display),
        );
        Ok(())
    }

    fn handle_color_inspection_action(
        &mut self,
        context: &egui::Context,
        action: ColorInspectionAction,
    ) {
        match action {
            ColorInspectionAction::AnalyzeCurrent => {
                let Some(document) = self.workspace.active_image() else {
                    self.color_inspection
                        .report_error("open a PNG or capture an RTSP frame before analysis");
                    return;
                };
                if !(document.is_png_workspace_file() || document.is_color_capture()) {
                    self.color_inspection
                        .report_error("Color page accepts PNG files and RTSP captures only");
                    return;
                }
                self.color_inspection.analyze_document(document);
            }
            ColorInspectionAction::StartManualCorners => {
                let Some(document) = self.workspace.active_image() else {
                    self.color_inspection.report_error(
                        "open a PNG or capture an RTSP frame before manual corner picking",
                    );
                    return;
                };
                if !(document.is_png_workspace_file() || document.is_color_capture()) {
                    self.color_inspection
                        .report_error("Color page accepts PNG files and RTSP captures only");
                    return;
                }
                self.color_inspection.start_manual_corners(document);
            }
            ColorInspectionAction::ClearManualCorners => {
                self.color_inspection.clear_manual_corners()
            }
            ColorInspectionAction::CaptureActiveRtsp => {
                let Some(id) = self.workspace.active_live().map(|document| document.id) else {
                    self.color_inspection
                        .report_error("select an active RTSP stream before capture");
                    return;
                };
                if let Err(error) = self.capture_live_frame_for_color(context, id) {
                    self.color_inspection.report_error(error);
                }
            }
            ColorInspectionAction::ExportMetrics => self.color_inspection.prepare_export(),
            ColorInspectionAction::ExportYamlReport => {
                self.color_inspection.prepare_yaml_report_export();
            }
        }
    }

    fn save_active_ephemeral_source(&mut self) -> bool {
        let asset = self
            .workspace
            .active()
            .and_then(|document| document.source_asset.as_ref().map(Arc::clone))
            .or_else(|| {
                self.workspace
                    .active_image()
                    .and_then(|document| document.source.asset().map(Arc::clone))
            });
        let Some(asset) = asset else { return false };
        let extension = asset_extension(&asset.metadata.format);
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Captured source", &[extension])
            .set_file_name(format!("{}.{}", asset.metadata.source_name, extension))
            .save_file()
        else {
            return false;
        };
        match save_asset_source(&path, &asset) {
            Ok(()) => {
                if let Some(document) = self.workspace.active_mut() {
                    document.unsaved = false;
                }
                if let Some(document) = self.workspace.active_image_mut() {
                    document.unsaved = false;
                }
                true
            }
            Err(error) => {
                self.live_runtime.panel.last_error = Some(error);
                false
            }
        }
    }

    fn render_pending_ephemeral_close(&mut self, context: &egui::Context) {
        let Some(id) = self.pending_ephemeral_close else {
            return;
        };
        let mut cancel = false;
        let mut save = false;
        let mut discard = false;
        egui::Window::new("Unsaved captured source")
            .collapsible(false)
            .resizable(false)
            .show(context, |ui| {
                ui.label("Save writes the source from memory to the chosen destination.");
                ui.horizontal(|ui| {
                    cancel = ui.button("Cancel").clicked();
                    save = ui.button("Save...").clicked();
                    discard = ui.button("Discard tab").clicked();
                });
            });
        if cancel {
            self.pending_ephemeral_close = None;
        } else if save {
            self.workspace.activate(id);
            if self.save_active_ephemeral_source() {
                self.pending_ephemeral_close = None;
                self.close_document(context, id);
            }
        } else if discard {
            if let Some(document) = self.workspace.document_mut(id) {
                document.unsaved = false;
            }
            if let Some(document) = self.workspace.image_mut(id) {
                document.unsaved = false;
            }
            self.pending_ephemeral_close = None;
            self.close_document(context, id);
        }
    }

    fn render_direct_rtsp_workspace(&mut self, ui: &mut egui::Ui) -> Option<RtspStreamConfig> {
        ui.heading("RTSP Stream");
        ui.weak("Independent live-image input; it does not use Local or SFTP mounts.");
        ui.label("URL");
        ui.text_edit_singleline(&mut self.direct_rtsp.url);
        ui.horizontal(|ui| {
            ui.label("Channel");
            ui.add(egui::DragValue::new(&mut self.direct_rtsp.channel).range(0..=255));
            ui.label("Width");
            ui.add(egui::DragValue::new(&mut self.direct_rtsp.width).range(1..=16_384));
            ui.label("Height");
            ui.add(egui::DragValue::new(&mut self.direct_rtsp.height).range(1..=16_384));
        });
        ui.horizontal(|ui| {
            ui.label("Codec");
            ui.radio_value(&mut self.direct_rtsp.codec, RtspCodec::H264, "H.264");
            ui.radio_value(&mut self.direct_rtsp.codec, RtspCodec::H265, "H.265");
        });
        ui.horizontal(|ui| {
            ui.label("Transport");
            ui.radio_value(&mut self.direct_rtsp.transport, RtspTransport::Tcp, "TCP");
            ui.radio_value(&mut self.direct_rtsp.transport, RtspTransport::Udp, "UDP");
        });
        ui.horizontal(|ui| {
            ui.label("Latency");
            ui.radio_value(
                &mut self.direct_rtsp.latency_mode,
                RtspLatencyMode::Low,
                "Low",
            );
            ui.radio_value(
                &mut self.direct_rtsp.latency_mode,
                RtspLatencyMode::Stable,
                "Stable",
            );
        });
        ui.checkbox(
            &mut self.direct_rtsp.prefer_hardware_acceleration,
            "Prefer hardware acceleration",
        );
        ui.weak("A preference only; the Viewer reports the actual decoder backend after connect.");
        if let Some(error) = self.direct_rtsp.last_error.as_deref() {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }
        if ui.button("Connect RTSP").clicked() {
            return Some(RtspStreamConfig {
                url: self.direct_rtsp.url.clone(),
                channel: self.direct_rtsp.channel,
                width: self.direct_rtsp.width,
                height: self.direct_rtsp.height,
                codec: self.direct_rtsp.codec,
                transport: self.direct_rtsp.transport,
                latency_mode: self.direct_rtsp.latency_mode,
            });
        }
        None
    }

    fn render_workspace_stream_section(
        &mut self,
        ui: &mut egui::Ui,
    ) -> Option<WorkspaceStreamAction> {
        use egui_extras::{Column, TableBuilder};

        ui.heading("Active Streams");
        let active = self.workspace.active_id();
        let items: Vec<_> = self
            .workspace
            .live_documents()
            .iter()
            .map(|document| {
                (
                    document.id,
                    document.title.clone(),
                    document.source.detail(),
                    document.status_label().to_owned(),
                    format!("{:?}", document.stage),
                    matches!(document.lifecycle, LiveDocumentLifecycle::Open),
                    matches!(document.lifecycle, LiveDocumentLifecycle::Open)
                        && document.displayed_frame().is_some(),
                )
            })
            .collect();
        if items.is_empty() {
            ui.weak("No active stream documents.");
            return None;
        }
        let mut action = None;
        egui::ScrollArea::horizontal()
            .id_salt("stream_table_hscroll")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                TableBuilder::new(ui)
                    .id_salt("workspace_stream_table")
                    .striped(true)
                    .resizable(true)
                    .max_scroll_height(200.0)
                    .auto_shrink([false, true])
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::remainder().at_least(80.0).clip(true))
                    .column(Column::initial(80.0).at_least(60.0).clip(true))
                    .column(Column::initial(112.0).at_least(80.0).clip(true))
                    .header(24.0, |mut header| {
                        header.col(|ui| {
                            ui.strong("Stream");
                        });
                        header.col(|ui| {
                            ui.strong("Status");
                        });
                        header.col(|ui| {
                            ui.strong("Actions");
                        });
                    })
                    .body(|body| {
                        body.rows(26.0, items.len(), |mut row| {
                            let (
                                id,
                                ref title,
                                ref detail,
                                ref status,
                                ref stage,
                                can_stop,
                                can_capture,
                            ) = items[row.index()];
                            let is_selected = active == Some(id);
                            let capture_hover = if self.is_color_workspace() {
                                Some("Capture the displayed frame into the Color page")
                            } else if cfg!(feature = "calibration-opencv") {
                                Some("Capture the displayed frame into the Calibration dataset")
                            } else {
                                None
                            };
                            row.col(|ui| {
                                if ui.selectable_label(is_selected, title).clicked() {
                                    action = Some(WorkspaceStreamAction::Activate(id));
                                }
                                ui.add_space(4.0);
                                ui.weak(detail);
                            });
                            row.col(|ui| {
                                ui.label(format!("{stage} · {status}"));
                            });
                            row.col(|ui| {
                                if let Some(hover) = capture_hover
                                    && ui
                                        .add_enabled(
                                            can_capture,
                                            egui::Button::new("Capture").small(),
                                        )
                                        .on_hover_text(hover)
                                        .clicked()
                                {
                                    action = Some(WorkspaceStreamAction::Capture(id));
                                }
                                let label = if can_stop { "■" } else { "—" };
                                if ui
                                    .add_enabled(can_stop, egui::Button::new(label).small())
                                    .on_hover_text(detail)
                                    .clicked()
                                {
                                    action = Some(WorkspaceStreamAction::Stop(id));
                                }
                            });
                        });
                    });
            });
        action
    }

    fn render_stream_metrics(&self, ui: &mut egui::Ui) {
        let Some(document) = self.workspace.active_live() else {
            return;
        };
        let displayed_frame = document.displayed_frame().cloned();
        ui.separator();
        ui.scope(|ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            match &document.source {
                LiveStreamSource::Rtsp { .. } => {
                    if document.metrics.network_bytes_available {
                        ui.monospace(format!("FFmpeg I/O {} B", document.metrics.network_bytes));
                        ui.monospace(format!(
                            "FFmpeg I/O rate {:.2} MiB/s",
                            document.metrics.network_bytes_per_second as f64 / (1024.0 * 1024.0)
                        ));
                    } else {
                        ui.monospace("FFmpeg I/O N/A");
                    }
                    ui.monospace(format!(
                        "FFmpeg media {} B",
                        document.metrics.ffmpeg_media_bytes
                    ));
                    ui.monospace(format!(
                        "FFmpeg media rate {:.2} MiB/s",
                        document.metrics.ffmpeg_media_bytes_per_second as f64 / (1024.0 * 1024.0)
                    ));
                }
                LiveStreamSource::Cv610 { .. } => {
                    ui.monospace(format!("Network {} B", document.metrics.network_bytes));
                    ui.monospace(format!("RTP {}", document.metrics.rtp_packets));
                    ui.monospace(format!("Gaps {}", document.metrics.rtp_gaps));
                }
            }
            ui.monospace(format!("Dropped {}", document.metrics.preview_dropped));
            ui.monospace(format!("Decoded {}", document.metrics.decoded_frames));
            ui.monospace(format!(
                "Dec {:.1} fps",
                document.metrics.decoded_fps_millihz as f64 / 1_000.0
            ));
            ui.monospace(format!(
                "Pres {:.1} fps",
                document.presented_fps_millihz_at(camera_toolbox_app::host_monotonic_time_ns(),)
                    as f64
                    / 1_000.0
            ));
            ui.monospace(format!(
                "Host pres {:.2} ms",
                document.metrics.host_presentation_delay_ns as f64 / 1_000_000.0
            ));
            ui.monospace(format!(
                "Stage codec {:.2} ms",
                document.metrics.decoder_codec_stage_ns as f64 / 1_000_000.0
            ));
            ui.monospace(format!(
                "Stage scale {:.2} ms",
                document.metrics.decoder_scale_stage_ns as f64 / 1_000_000.0
            ));
            ui.monospace(format!(
                "Stage copy {:.2} ms",
                document.metrics.decoder_copy_stage_ns as f64 / 1_000_000.0
            ));
            ui.monospace(format!(
                "Decoder {}",
                document
                    .metrics
                    .decoder_backend
                    .as_deref()
                    .unwrap_or("Not reported")
            ));
            ui.monospace(format!("Presented {}", document.presented_frames));
            ui.monospace(format!("Resync {}", document.metrics.decoder_resyncs));
            ui.monospace(format!("Record {} B", document.metrics.record_bytes));
            ui.monospace(format!(
                "Preview Q {}",
                document.metrics.preview_queue_depth
            ));
            ui.monospace(format!(
                "Decoder Q {}",
                document.metrics.decoder_queue_depth
            ));
            ui.monospace(format!(
                "Record Q {} B",
                document.metrics.recorder_queue_bytes
            ));
            let provenance = displayed_frame.as_ref().map_or_else(
                || "No displayed frame".to_owned(),
                |frame| match &frame.identity.source_pts {
                    camera_toolbox_app::SourcePts::Known {
                        ticks,
                        time_base_numerator,
                        time_base_denominator,
                        provenance,
                    } => format!(
                        "{} ch{} seq{} PTS {} @ {}/{} ({provenance:?})",
                        frame.identity.stream_id.as_str(),
                        frame.identity.channel,
                        frame.identity.frame_sequence,
                        ticks,
                        time_base_numerator,
                        time_base_denominator,
                    ),
                    camera_toolbox_app::SourcePts::Unavailable { reason } => format!(
                        "{} ch{} seq{} PTS unavailable: {reason}",
                        frame.identity.stream_id.as_str(),
                        frame.identity.channel,
                        frame.identity.frame_sequence,
                    ),
                },
            );
            ui.monospace(provenance);
        });
    }

    fn start_direct_rtsp(&mut self, config: RtspStreamConfig) {
        let prefer_hardware_acceleration = self.direct_rtsp.prefer_hardware_acceleration;
        match self
            .live_runtime
            .start_direct_rtsp(config, prefer_hardware_acceleration)
        {
            Ok((session_id, latest_frame, source)) => {
                self.workspace.open_live(session_id, latest_frame, source);
                self.direct_rtsp.last_error = None;
            }
            Err(error) => self.direct_rtsp.last_error = Some(error),
        }
    }

    fn handle_stream_panel_action(&mut self, action: StreamPanelAction) {
        match action {
            StreamPanelAction::Start => match self.live_runtime.start() {
                Ok((session_id, latest_frame, source)) => {
                    self.workspace.open_live(session_id, latest_frame, source);
                    self.live_runtime.panel.last_error = None;
                }
                Err(error) => self.live_runtime.panel.last_error = Some(error),
            },
            StreamPanelAction::RequestStop => {
                let id = self
                    .workspace
                    .active_live()
                    .map(|document| document.id)
                    .or_else(|| {
                        self.workspace
                            .live_documents()
                            .first()
                            .map(|document| document.id)
                    });
                if let Some(id) = id
                    && let Some(document) = self.workspace.live_mut(id)
                    && matches!(document.lifecycle, LiveDocumentLifecycle::Open)
                {
                    if self.live_runtime.request_close(&document.session_id) {
                        document.lifecycle = LiveDocumentLifecycle::Closing {
                            stop_deadline: Instant::now() + LIVE_STOP_TIMEOUT,
                        };
                    } else {
                        self.workspace.remove_live(id);
                    }
                }
            }
        }
    }

    fn handle_workspace_stream_action(
        &mut self,
        context: &egui::Context,
        action: WorkspaceStreamAction,
    ) {
        match action {
            WorkspaceStreamAction::Activate(id) => {
                self.workspace.activate(id);
            }
            WorkspaceStreamAction::Stop(id) => {
                let Some(document) = self.workspace.live_mut(id) else {
                    return;
                };
                if matches!(document.lifecycle, LiveDocumentLifecycle::Open) {
                    if self.live_runtime.request_close(&document.session_id) {
                        document.lifecycle = LiveDocumentLifecycle::Closing {
                            stop_deadline: Instant::now() + LIVE_STOP_TIMEOUT,
                        };
                    } else {
                        self.workspace.remove_live(id);
                    }
                }
            }
            WorkspaceStreamAction::Capture(id) => {
                if self.is_color_workspace() {
                    if let Err(error) = self.capture_live_frame_for_color(context, id) {
                        self.color_inspection.report_error(error);
                    }
                    return;
                }
                #[cfg(feature = "calibration-opencv")]
                {
                    self.workspace.activate(id);
                    let Some((frame, source)) = self.workspace.live(id).and_then(|document| {
                        document
                            .displayed_frame()
                            .cloned()
                            .map(|frame| (frame, document.source.clone()))
                    }) else {
                        return;
                    };
                    self.calibration.capture_displayed_stream_frame(
                        frame,
                        source,
                        self.live_runtime.capture_store().clone(),
                    );
                    self.product_workspace = ProductWorkspace::Calibration;
                }
            }
        }
    }

    fn poll_stream_events(&mut self) {
        loop {
            let event = match self.live_runtime.try_recv() {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(error) => {
                    self.live_runtime.panel.last_error = Some(error);
                    break;
                }
            };
            #[cfg(feature = "calibration-opencv")]
            if matches!(
                &event.event,
                camera_toolbox_app::StreamServiceEvent::Terminal(_)
            ) {
                self.calibration.stream_disconnected(&event.session_id);
            }
            if let Some(document) = self.workspace.live_by_session_mut(&event.session_id) {
                let is_terminal = matches!(
                    event.event,
                    camera_toolbox_app::StreamServiceEvent::Terminal(_)
                );
                document.apply_event(event.event);
                if is_terminal {
                    let id = document.id;
                    self.workspace.remove_live(id);
                }
            }
        }
    }

    fn advance_live_close_deadlines(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .workspace
            .live_documents()
            .iter()
            .filter_map(|document| match document.lifecycle {
                LiveDocumentLifecycle::Closing { stop_deadline } if now >= stop_deadline => {
                    Some((document.id, document.session_id.clone()))
                }
                _ => None,
            })
            .collect();
        for (id, session_id) in expired {
            if self.live_runtime.force_cleanup(&session_id) {
                #[cfg(feature = "calibration-opencv")]
                self.calibration.stream_disconnected(&session_id);
                self.workspace.remove_live(id);
            }
        }
    }

    #[cfg(feature = "calibration-opencv")]
    fn live_viewer_render_texture<'a>(
        document: &'a LiveDocument,
        _calibration_presentation: Option<&'a CalibrationViewerPresentation>,
    ) -> Option<&'a egui::TextureHandle> {
        document.texture()
    }

    fn render_live_viewer(
        ui: &mut egui::Ui,
        document: &mut LiveDocument,
        calibration_capture_enabled: bool,
        #[cfg(feature = "calibration-opencv")] calibration_presentation: Option<
            &CalibrationViewerPresentation,
        >,
    ) -> (
        egui::Rect,
        Option<Arc<camera_toolbox_app::DecodedVideoFrame>>,
    ) {
        let rect = ui.max_rect();
        let displayed_frame = document.displayed_frame().cloned();
        ui.horizontal(|ui| {
            ui.heading(&document.title);
            if calibration_capture_enabled {
                ui.checkbox(&mut document.show_calibration_detection, "Board detection");
            }
            ui.add_enabled(false, egui::Button::new("Fit"))
                .on_hover_text("Live Stream is always fit to the Viewer window.");
            ui.checkbox(&mut document.horizontal_flip, "Flip X")
                .on_hover_text("Mirror the displayed image and all live overlays horizontally.");
            if ui.button("Snapshot...").clicked()
                && let Some(frame) = displayed_frame.as_ref()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("PNG image", &["png"])
                    .save_file()
            {
                document.last_snapshot = Some(match write_live_snapshot(&path, frame) {
                    Ok(()) => format!("Saved {}", path.display()),
                    Err(error) => format!("Snapshot failed: {error}"),
                });
            }
        });
        ui.label(document.source.detail());
        ui.label(format!("Stream stage: {:?}", document.stage));
        if let Some(message) = document.last_snapshot.as_deref() {
            ui.label(message);
        }
        match &document.lifecycle {
            LiveDocumentLifecycle::Closing { .. } => {
                ui.colored_label(egui::Color32::YELLOW, "Closing asynchronously...");
            }
            LiveDocumentLifecycle::ForcedCleanup { terminal }
            | LiveDocumentLifecycle::Terminal { terminal } => {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("Stopped: {terminal:?}"));
            }
            LiveDocumentLifecycle::Open => {}
        }
        if let Some(reason) = document.decoder_unavailable.as_deref() {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!("Decoder unavailable; record-only mode: {reason}"),
            );
        }
        ui.separator();
        #[cfg(feature = "calibration-opencv")]
        let render_texture = Self::live_viewer_render_texture(document, calibration_presentation);
        #[cfg(not(feature = "calibration-opencv"))]
        let render_texture = document.texture();
        if let Some(texture) = render_texture {
            let available = ui.available_size();
            let source = texture.size_vec2();
            let finite_positive = |value: f32| {
                if value.is_finite() && value > 0.0 {
                    value
                } else {
                    1.0
                }
            };
            let scale = (finite_positive(available.x) / finite_positive(source.x))
                .min(finite_positive(available.y) / finite_positive(source.y))
                .max(0.01);
            let fitted = source * scale;
            let canvas_size = egui::vec2(available.x.max(fitted.x), fitted.y);
            let (canvas_rect, _) = ui.allocate_exact_size(canvas_size, egui::Sense::hover());
            let image_rect = egui::Rect::from_center_size(canvas_rect.center(), fitted);
            let response_rect = ui
                .put(
                    image_rect,
                    egui::Image::new(texture)
                        .fit_to_exact_size(fitted)
                        .uv(viewer_texture_uv(document.horizontal_flip)),
                )
                .rect;
            #[cfg(not(feature = "calibration-opencv"))]
            let _ = response_rect;
            #[cfg(feature = "calibration-opencv")]
            if let Some(overlay) =
                calibration_presentation.map(|presentation| &presentation.overlay)
            {
                Self::paint_live_calibration_overlay(
                    &ui.painter_at(response_rect),
                    response_rect,
                    overlay,
                    document.show_calibration_detection,
                    document.horizontal_flip,
                );
            }
        } else {
            let available = ui.available_size();
            let placeholder = egui::vec2(16.0, 9.0);
            let scale = (available.x.max(1.0) / placeholder.x)
                .min(available.y.max(1.0) / placeholder.y)
                .max(0.01);
            let canvas_size = egui::vec2(available.x, placeholder.y * scale);
            let (canvas_rect, _) = ui.allocate_exact_size(canvas_size, egui::Sense::hover());
            ui.painter_at(canvas_rect).text(
                canvas_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Waiting for decoded frame",
                egui::FontId::proportional(14.0),
                egui::Color32::GRAY,
            );
        }
        (rect, None)
    }

    #[cfg(feature = "calibration-opencv")]
    fn paint_live_calibration_overlay(
        painter: &egui::Painter,
        image_rect: egui::Rect,
        overlay: &CalibrationViewerOverlay,
        show_detection: bool,
        horizontal_flip: bool,
    ) {
        if !show_detection {
            return;
        }
        if let Some(persistent) = &overlay.persistent {
            Self::paint_live_detection_overlay(
                painter,
                image_rect,
                persistent,
                horizontal_flip,
                LIVE_VIEWER_DATASET_OVERLAY_COLOR,
                egui::Stroke::new(1.4, egui::Color32::WHITE),
                3.0,
            );
        }
    }

    #[cfg(feature = "calibration-opencv")]
    fn paint_live_detection_overlay(
        painter: &egui::Painter,
        image_rect: egui::Rect,
        detection: &ViewerDetectionOverlay,
        horizontal_flip: bool,
        point_color: egui::Color32,
        point_stroke: egui::Stroke,
        point_radius: f32,
    ) {
        for point in &detection.corners {
            if let Some(position) =
                Self::live_overlay_point(*point, detection.image_size, image_rect, horizontal_flip)
            {
                painter.circle_filled(position, point_radius, point_color);
                painter.circle_stroke(position, point_radius + 1.0, point_stroke);
            }
        }
        if let Some(axis) = &detection.pose_axis {
            Self::paint_live_pose_axis_overlay(
                painter,
                image_rect,
                detection.image_size,
                axis,
                horizontal_flip,
            );
        }
    }

    #[cfg(feature = "calibration-opencv")]
    fn paint_live_pose_axis_overlay(
        painter: &egui::Painter,
        image_rect: egui::Rect,
        image_size: camera_toolbox_core::CalibrationImageSize,
        axis: &ViewerPoseAxisOverlay,
        horizontal_flip: bool,
    ) {
        let Some(origin) =
            Self::live_overlay_point(axis.origin, image_size, image_rect, horizontal_flip)
        else {
            return;
        };
        painter.circle_filled(origin, 4.0, egui::Color32::WHITE);
        Self::paint_live_pose_axis(
            painter,
            origin,
            axis.x_axis,
            image_size,
            image_rect,
            horizontal_flip,
            egui::Color32::from_rgb(255, 80, 80),
            "X",
        );
        Self::paint_live_pose_axis(
            painter,
            origin,
            axis.y_axis,
            image_size,
            image_rect,
            horizontal_flip,
            egui::Color32::from_rgb(80, 220, 80),
            "Y",
        );
        Self::paint_live_pose_axis(
            painter,
            origin,
            axis.z_axis,
            image_size,
            image_rect,
            horizontal_flip,
            egui::Color32::from_rgb(80, 140, 255),
            "Z",
        );
    }

    #[cfg(feature = "calibration-opencv")]
    fn paint_live_pose_axis(
        painter: &egui::Painter,
        origin: egui::Pos2,
        endpoint: camera_toolbox_core::CalibrationPoint,
        image_size: camera_toolbox_core::CalibrationImageSize,
        image_rect: egui::Rect,
        horizontal_flip: bool,
        color: egui::Color32,
        label: &'static str,
    ) {
        let Some(endpoint) =
            Self::live_overlay_point(endpoint, image_size, image_rect, horizontal_flip)
        else {
            return;
        };
        painter.line_segment([origin, endpoint], egui::Stroke::new(2.0, color));
        painter.circle_filled(endpoint, 4.0, color);
        painter.text(
            endpoint + egui::vec2(5.0, -5.0),
            egui::Align2::LEFT_BOTTOM,
            label,
            egui::FontId::monospace(12.0),
            color,
        );
    }

    #[cfg(feature = "calibration-opencv")]
    pub(crate) fn live_overlay_point(
        point: camera_toolbox_core::CalibrationPoint,
        image_size: camera_toolbox_core::CalibrationImageSize,
        image_rect: egui::Rect,
        horizontal_flip: bool,
    ) -> Option<egui::Pos2> {
        let normalized_x = (point.x + 0.5) / image_size.width as f32;
        let normalized_x = if horizontal_flip {
            1.0 - normalized_x
        } else {
            normalized_x
        };
        let normalized_y = (point.y + 0.5) / image_size.height as f32;
        if !normalized_x.is_finite()
            || !normalized_y.is_finite()
            || !(0.0..=1.0).contains(&normalized_x)
            || !(0.0..=1.0).contains(&normalized_y)
        {
            return None;
        }
        Some(egui::pos2(
            image_rect.left() + normalized_x * image_rect.width(),
            image_rect.top() + normalized_y * image_rect.height(),
        ))
    }

    fn forget_auto_open_document(&mut self, id: DocumentId) {
        self.auto_open_background_tabs
            .retain(|existing| *existing != id);
        self.auto_open_documents
            .retain(|_, existing| *existing != id);
    }

    fn track_auto_open_background_tab(&mut self, context: &egui::Context, id: DocumentId) {
        self.auto_open_background_tabs
            .retain(|existing| *existing != id);
        self.auto_open_background_tabs.push_back(id);
        while self.auto_open_background_tabs.len() > AUTO_OPEN_BACKGROUND_TAB_LIMIT {
            let Some(oldest) = self.auto_open_background_tabs.pop_front() else {
                break;
            };
            if self.workspace.active_id() == Some(oldest) {
                self.auto_open_background_tabs.push_back(oldest);
                break;
            }
            self.close_document(context, oldest);
        }
    }

    fn handle_tab_action(&mut self, context: &egui::Context, action: TabBarAction) {
        match action {
            TabBarAction::Activate(id) => {
                if self.workspace.activate(id) {
                    self.ensure_active_resources(context);
                }
            }
            TabBarAction::Close(id) => self.close_document(context, id),
        }
    }

    fn close_document(&mut self, context: &egui::Context, id: DocumentId) {
        if self
            .workspace
            .document(id)
            .is_some_and(|document| document.unsaved && document.source_asset.is_some())
            || self
                .workspace
                .image(id)
                .is_some_and(|document| document.unsaved && document.source.asset().is_some())
        {
            self.pending_ephemeral_close = Some(id);
            self.workspace.activate(id);
            return;
        }
        self.forget_auto_open_document(id);
        if self.workspace.image(id).is_some() {
            self.workspace.remove_image(id);
            self.ensure_active_resources(context);
            self.workspace.enforce_derived_budget();
            return;
        }
        if let Some(document) = self.workspace.live_mut(id) {
            match document.lifecycle {
                LiveDocumentLifecycle::Open => {
                    if self.live_runtime.request_close(&document.session_id) {
                        document.lifecycle = LiveDocumentLifecycle::Closing {
                            stop_deadline: Instant::now() + LIVE_STOP_TIMEOUT,
                        };
                    } else {
                        self.workspace.remove_live(id);
                    }
                }
                LiveDocumentLifecycle::Closing { .. } => {}
                LiveDocumentLifecycle::Terminal { .. }
                | LiveDocumentLifecycle::ForcedCleanup { .. } => {
                    self.workspace.remove_live(id);
                }
            }
            return;
        }
        let Some(document) = self.workspace.close(id) else {
            return;
        };
        self.notifications
            .clear_scope(NotificationScope::ImageGeneration(
                document.loaded.generation,
            ));
        tracing::debug!(
            operation = "close_document",
            document_id = %document.id,
            generation = document.loaded.generation,
            "closed RAW document"
        );
        self.ensure_active_resources(context);
        self.workspace.enforce_derived_budget();
    }
}

fn decode_raw_request(
    pipeline: &RawOpenPipeline,
    request: LocalRawAnalyzeRequest,
    cancellation: FsCancellation,
) -> anyhow::Result<OpenedRawDocument> {
    tracing::debug!(
        operation = "load_raw",
        path = %request.path.display(),
        "starting RAW load"
    );
    let (file_system, reference) = local_file_source(&request.path)?;
    let mut control = FsControl::with_timeout(Duration::from_secs(30));
    control.cancellation = cancellation;
    let result = pipeline.begin(
        &file_system,
        &reference,
        RawOpenMode::WithOptions(RawDecodeParams {
            spec: request.spec.clone(),
            encoding: request.encoding,
        }),
        &control,
    )?;
    let roi = request
        .roi
        .clamped_to(result.frame.spec.width, result.frame.spec.height)
        .unwrap_or(Roi {
            x: 0,
            y: 0,
            width: result.frame.spec.width,
            height: result.frame.spec.height,
        });
    let stats = analyze_roi(&result.frame, roi)?;
    Ok(OpenedRawDocument {
        report: LocalRawAnalyzeReport {
            path: request.path,
            frame: result.frame,
            roi,
            stats,
        },

        source: result.source,
        interpretation: result.interpretation,
    })
}

fn decode_workspace_image_request(
    pipeline: &ImageOpenPipeline,
    request: WorkspaceFileOpenRequest,
    mode: ImageOpenMode,
    cancellation: FsCancellation,
    progress: &mut dyn FnMut(SourceReadProgress),
) -> anyhow::Result<OpenedFileDocument> {
    tracing::debug!(
        operation = "load_image",
        path = %request.display_path.display(),
        source_id = %request.reference.source_id,
        "starting workspace image open"
    );
    let mut control = FsControl::with_timeout(Duration::from_secs(30));
    control.cancellation = cancellation;
    let opened = pipeline.open_with_progress(
        &*request.file_system,
        &request.reference,
        mode,
        &control,
        progress,
    )?;
    if !matches!(&opened.kind, ImageFileKind::Raw) {
        return Ok(OpenedFileDocument::Image(opened));
    }
    let ImageOpenResult {
        source,
        native,
        raw,
        ..
    } = opened;
    let metadata = raw.ok_or_else(|| anyhow::anyhow!("RAW open result is missing metadata"))?;
    let NativeImage::Raw(frame) = native else {
        return Err(anyhow::anyhow!(
            "RAW open result is missing native RAW data"
        ));
    };
    let frame = Arc::try_unwrap(frame).unwrap_or_else(|frame| (*frame).clone());
    let roi = Roi {
        x: 0,
        y: 0,
        width: frame.spec.width,
        height: frame.spec.height,
    };
    let stats = analyze_roi(&frame, roi)?;
    Ok(OpenedFileDocument::Raw(OpenedRawDocument {
        report: LocalRawAnalyzeReport {
            path: request.display_path,
            frame,
            roi,
            stats,
        },
        source,
        interpretation: metadata.interpretation,
    }))
}

fn decode_raw_reinterpret(
    pipeline: &RawOpenPipeline,
    source: ImageSourceHandle,
    params: RawDecodeParams,
    decode_generation: u64,
    roi: Roi,
    path: PathBuf,
) -> anyhow::Result<OpenedRawDocument> {
    tracing::debug!(
        operation = "reinterpret_raw",
        decode_generation,
        path = %path.display(),
        "starting RAW reinterpret"
    );
    let control = FsControl::with_timeout(Duration::from_secs(30));
    let result = pipeline.reinterpret(source, params, decode_generation, &control)?;
    let roi = roi
        .clamped_to(result.frame.spec.width, result.frame.spec.height)
        .unwrap_or(Roi {
            x: 0,
            y: 0,
            width: result.frame.spec.width,
            height: result.frame.spec.height,
        });
    let stats = analyze_roi(&result.frame, roi)?;
    Ok(OpenedRawDocument {
        report: LocalRawAnalyzeReport {
            path,
            frame: result.frame,
            roi,
            stats,
        },
        source: result.source,
        interpretation: result.interpretation,
    })
}

fn decode_yuv_reinterpret(
    pipeline: &ImageOpenPipeline,
    source: ImageSourceHandle,
    kind: ImageFileKind,
    spec: Yuv420SpSpec,
) -> anyhow::Result<ImageOpenResult> {
    let control = FsControl::with_timeout(Duration::from_secs(30));
    Ok(pipeline.reinterpret_yuv(source, kind, spec, &control)?)
}
fn asset_payload_bytes(payload: &OwnedMediaPayload) -> Result<&[u8], String> {
    match payload {
        OwnedMediaPayload::Bytes(bytes) => Ok(bytes),
        OwnedMediaPayload::Planes(_) => {
            Err("multi-plane captured source requires a container format".to_owned())
        }
    }
}

fn asset_attribute_usize(
    asset: &camera_toolbox_core::EphemeralAsset,
    name: &str,
) -> Result<usize, String> {
    asset
        .metadata
        .attributes
        .get(name)
        .ok_or_else(|| format!("captured metadata is missing {name}"))?
        .parse::<usize>()
        .map_err(|error| format!("captured metadata {name} is invalid: {error}"))
}

fn asset_extension(format: &MediaFormat) -> &'static str {
    match format {
        MediaFormat::RawPacked { .. } | MediaFormat::RawU16Le { .. } => "raw",
        MediaFormat::Jpeg => "jpg",
        MediaFormat::Png => "png",
        MediaFormat::Yuv420Sp { .. } => "nv21",
        MediaFormat::H264AnnexB => "h264",
        MediaFormat::H265AnnexB => "h265",
        MediaFormat::Binary => "bin",
    }
}
fn save_asset_source(
    path: &Path,
    asset: &camera_toolbox_core::EphemeralAsset,
) -> Result<(), String> {
    save_asset_source_with(path, asset, |file, asset| {
        use std::io::Write;

        match &asset.source {
            OwnedMediaPayload::Bytes(bytes) => file.write_all(bytes)?,
            OwnedMediaPayload::Planes(planes) => {
                for plane in planes {
                    file.write_all(&plane.bytes)?;
                }
            }
        }
        Ok(())
    })
}

fn save_asset_source_with<F>(
    path: &Path,
    asset: &camera_toolbox_core::EphemeralAsset,
    write_payload: F,
) -> Result<(), String>
where
    F: FnOnce(&mut std::fs::File, &camera_toolbox_core::EphemeralAsset) -> std::io::Result<()>,
{
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "destination already exists and was preserved; choose a new path: {}",
                    path.display()
                )
            } else {
                error.to_string()
            }
        })?;
    let result = write_payload(&mut file, asset)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = result {
        let _ = std::fs::remove_file(path);
        return Err(format!("export incomplete: {error}"));
    }
    Ok(())
}

fn write_live_snapshot(
    path: &Path,
    frame: &camera_toolbox_app::DecodedVideoFrame,
) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
    writer
        .write_image_data(&frame.rgba)
        .map_err(|error| error.to_string())
}

/// Actions from the left-sidebar RTSP stream table (distinct from platform StreamPanelAction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceStreamAction {
    Activate(DocumentId),
    Capture(DocumentId),
    Stop(DocumentId),
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
