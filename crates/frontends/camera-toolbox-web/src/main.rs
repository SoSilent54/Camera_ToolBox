mod engine_api;
mod files_api;
mod workflow;

use std::{
    collections::HashMap,
    fs,
    net::IpAddr,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, Query, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
#[cfg(feature = "calibration-opencv")]
use camera_toolbox_adapters::OpenCvCalibrationBackend;
use camera_toolbox_adapters::media::{
    FfmpegRtspDecoder, FfmpegRtspTransport, ffmpeg_rtsp::FfmpegRtspDecoderStatsSnapshot,
};
use camera_toolbox_adapters::platforms::ssh_managed::{
    CredentialResolver, ProductionCredentialResolver, RusshTransportFactory, SshConnectionTarget,
    SshEepromProvisionService, SshI2cHelperService, SshTransportFactory,
};
use camera_toolbox_adapters::x5_tcp_client;
use camera_toolbox_app::{
    CalibrationBackend, CalibrationCancellation, DecodedVideoFrame, DumpCancellation,
    EepromDeviceState, EepromHelperAction, EepromHelperResult, EepromProvisionOperation,
    EepromProvisionService, EepromProvisionServiceError, EepromSerialState, I2cHelperAction,
    I2cHelperOperation, I2cHelperResult, I2cHelperService, I2cMessageData, I2cMessageSpec,
    I2cTransactionSpec, LatestDecodedFrameSlot, RemoteOperationControl, RemoteTimeouts,
    RtspLatencyMode, StreamCancellation, StreamSessionId, host_monotonic_time_ns,
    validate_i2c_transfer_transactions,
};
use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationRequest, CalibrationSolution,
    EepromProvisionRequest, EepromProvisioningMode, EepromWriteSegment, InitialIntrinsics,
    ViewCalibrationResult, YG_STEREO_P24C64G_INTRINSICS_BYTES, YG_STEREO_P24C64G_V1_MAP_ID,
};
use clap::Parser;
use image::{ColorType, codecs::jpeg::JpegEncoder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    time::{Instant, sleep, sleep_until},
};
use tower_http::services::{ServeDir, ServeFile};
use workflow::{
    RuntimeGraphStatus, WorkflowGraph, node_catalog, normalize_workflow, runtime_graph_status,
    seed_workflow_graph, validate_workflow, workmode_templates,
};

static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Parser)]
#[command(name = "camera-toolbox-web")]
#[command(about = "Camera Toolbox browser workflow canvas server")]
struct ServerArgs {
    /// Web 服务绑定地址；默认允许局域网设备访问，生产环境需要另加认证或防火墙。
    #[arg(long, default_value = "0.0.0.0")]
    host: IpAddr,

    /// Web 服务端口；传 0 时由系统分配可用端口。
    #[arg(long, default_value_t = 8787)]
    port: u16,

    /// 前端静态资源目录；默认使用本 crate 下的 web/dist。
    #[arg(long)]
    static_dir: Option<PathBuf>,

    /// 工作流文件目录；保存为 .ctworkflow.json，运行时字段不会写入。
    #[arg(long)]
    workflow_dir: Option<PathBuf>,
}

#[derive(Clone)]
struct AppState {
    workflow_store: Arc<WorkflowStore>,
    runtime_sessions: Arc<Mutex<HashMap<String, RuntimeGraphSession>>>,
    control_runtime: Arc<ControlRuntime>,
    #[cfg(feature = "calibration-opencv")]
    calibration_backend: Arc<dyn CalibrationBackend>,
    eeprom_inspects: Arc<Mutex<HashMap<String, EepromInspectSnapshot>>>,
    engine_runtime: Arc<engine_api::EngineRuntime>,
}

/// 运行时会话只存于服务进程内；其图副本用于 Stop 后生成节点级诊断。
struct RuntimeGraphSession {
    graph: WorkflowGraph,
    status: RuntimeGraphStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EepromInspectSnapshot {
    target: EepromSnapshotTarget,
    image_sha256: String,
    device: EepromDeviceState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EepromSnapshotTarget {
    node_id: String,
    host: String,
    port: u16,
    username: String,
    credential_ref: String,
    map_id: String,
    bus: String,
    address: u8,
}

struct ControlRuntime {
    #[cfg(feature = "platform-ssh")]
    credential_resolver: Arc<dyn CredentialResolver>,
    #[cfg(feature = "platform-ssh")]
    ssh_transport: Arc<dyn SshTransportFactory>,
    #[cfg(feature = "platform-ssh")]
    helper_payload: Option<Arc<[u8]>>,
}

impl ControlRuntime {
    fn production() -> Self {
        Self {
            #[cfg(feature = "platform-ssh")]
            credential_resolver: Arc::new(ProductionCredentialResolver::new()),
            #[cfg(feature = "platform-ssh")]
            ssh_transport: Arc::new(RusshTransportFactory),
            #[cfg(feature = "platform-ssh")]
            helper_payload: None,
        }
    }

    #[cfg(all(test, feature = "platform-ssh"))]
    fn with_ssh_for_test(
        credential_resolver: Arc<dyn CredentialResolver>,
        ssh_transport: Arc<dyn SshTransportFactory>,
        helper_payload: Arc<[u8]>,
    ) -> Self {
        Self {
            credential_resolver,
            ssh_transport,
            helper_payload: Some(helper_payload),
        }
    }
}

impl ControlRuntime {
    fn execute_i2c(
        &self,
        preview: &ControlPreview,
        ssh: &SshExecutionBinding,
    ) -> std::result::Result<I2cHelperResult, PreviewApiError> {
        #[cfg(feature = "platform-ssh")]
        {
            let action = i2c_action_from_preview(preview)?;
            let control = RemoteOperationControl::new(
                RemoteTimeouts {
                    connect: Duration::from_secs(10),
                    idle: i2c_idle_timeout(&action),
                    overall: i2c_overall_timeout(&action),
                },
                DumpCancellation::default(),
            )
            .map_err(|error| PreviewApiError::bad_request(error.to_string()))?;
            let service = SshI2cHelperService::new(
                format!("workflow-web-i2c-{}", preview.target.node_id),
                ssh_target_from_binding(ssh)?,
                validate_credential_ref(&ssh.credential_ref)?,
                1_048_576,
                self.helper_payload()?,
                Arc::clone(&self.credential_resolver),
                Arc::clone(&self.ssh_transport),
            )
            .map_err(|error| PreviewApiError::bad_request(format_i2c_service_error(&error)))?;
            return service
                .execute(I2cHelperOperation { action }, control)
                .map_err(|error| PreviewApiError::bad_request(format_i2c_service_error(&error)));
        }
        #[cfg(not(feature = "platform-ssh"))]
        {
            let _ = (preview, ssh);
            Err(PreviewApiError::bad_request(
                "workflow-web was built without platform-ssh support",
            ))
        }
    }

    fn execute_eeprom(
        &self,
        preview: &ControlPreview,
        ssh: &SshExecutionBinding,
        action: EepromHelperAction,
    ) -> std::result::Result<EepromHelperResult, PreviewApiError> {
        #[cfg(feature = "platform-ssh")]
        {
            let control = RemoteOperationControl::new(
                RemoteTimeouts {
                    connect: Duration::from_secs(10),
                    idle: eeprom_idle_timeout(&action),
                    overall: eeprom_overall_timeout(&action),
                },
                DumpCancellation::default(),
            )
            .map_err(|error| PreviewApiError::bad_request(error.to_string()))?;
            let service = SshEepromProvisionService::new(
                format!("workflow-web-eeprom-{}", preview.target.node_id),
                ssh_target_from_binding(ssh)?,
                validate_credential_ref(&ssh.credential_ref)?,
                1_048_576,
                parse_eeprom_i2c_bus(&preview.target.bus)?,
                self.helper_payload()?,
                Arc::clone(&self.credential_resolver),
                Arc::clone(&self.ssh_transport),
            )
            .map_err(|error| PreviewApiError::bad_request(format_eeprom_service_error(&error)))?;
            return service
                .execute(EepromProvisionOperation { action }, control)
                .map_err(|error| {
                    PreviewApiError::bad_request(format_eeprom_service_error(&error))
                });
        }
        #[cfg(not(feature = "platform-ssh"))]
        {
            let _ = (preview, ssh);
            Err(PreviewApiError::bad_request(
                "workflow-web was built without platform-ssh support",
            ))
        }
    }
}

#[cfg(feature = "platform-ssh")]
impl ControlRuntime {
    fn helper_payload(&self) -> std::result::Result<Arc<[u8]>, PreviewApiError> {
        self.helper_payload
            .clone()
            .map(Ok)
            .unwrap_or_else(read_local_i2c_helper_payload)
    }
}

fn i2c_action_from_preview(
    preview: &ControlPreview,
) -> std::result::Result<I2cHelperAction, PreviewApiError> {
    let bus = parse_i2c_bus(&preview.target.bus)?;
    match preview.operation {
        "read" => Ok(I2cHelperAction::Transfer {
            transactions: vec![I2cTransactionSpec {
                bus,
                messages: vec![
                    I2cMessageSpec {
                        address: u16::from(preview.target.address),
                        flags: Vec::new(),
                        data: I2cMessageData::Write {
                            bytes: preview.target.register.to_be_bytes().to_vec(),
                        },
                    },
                    I2cMessageSpec {
                        address: u16::from(preview.target.address),
                        flags: Vec::new(),
                        data: I2cMessageData::Read { byte_len: 1 },
                    },
                ],
                settle_ms: None,
            }],
        }),
        "write" => Ok(I2cHelperAction::Transfer {
            transactions: page_write_transactions(&preview.target, &preview.page_split_estimate)?,
        }),
        other => Err(PreviewApiError::bad_request(format!(
            "unsupported I²C execution operation `{other}`",
        ))),
    }
}

fn page_write_transactions(
    target: &ControlTarget,
    estimate: &PageSplitEstimate,
) -> std::result::Result<Vec<I2cTransactionSpec>, PreviewApiError> {
    let bus = parse_i2c_bus(&target.bus)?;
    let mut consumed = 0_usize;
    let mut transactions = Vec::with_capacity(estimate.segments.len());
    for segment in &estimate.segments {
        let next = consumed
            .checked_add(segment.payload_length)
            .ok_or_else(|| PreviewApiError::bad_request("payload segment length overflows"))?;
        let payload = target.payload.get(consumed..next).ok_or_else(|| {
            PreviewApiError::bad_request("payload segment estimate does not match payload length")
        })?;
        let mut bytes = Vec::with_capacity(2 + payload.len());
        bytes.extend_from_slice(&segment.register.to_be_bytes());
        bytes.extend_from_slice(payload);
        transactions.push(I2cTransactionSpec {
            bus,
            messages: vec![I2cMessageSpec {
                address: u16::from(target.address),
                flags: Vec::new(),
                data: I2cMessageData::Write { bytes },
            }],
            settle_ms: Some(5),
        });
        consumed = next;
    }
    if consumed != target.payload.len() {
        return Err(PreviewApiError::bad_request(
            "payload segment estimate does not consume the full payload",
        ));
    }
    validate_i2c_transfer_transactions(&transactions).map_err(|error| {
        PreviewApiError::bad_request(format!(
            "I²C transfer request is invalid: {}",
            error.message
        ))
    })?;
    Ok(transactions)
}

fn parse_i2c_bus(bus: &str) -> std::result::Result<u32, PreviewApiError> {
    let bus = bus.trim();
    let digits = bus.strip_prefix("i2c-").unwrap_or(bus);
    digits
        .parse::<u32>()
        .map_err(|_| PreviewApiError::bad_request("bus must be `i2c-N` or decimal N"))
}

fn parse_eeprom_i2c_bus(bus: &str) -> std::result::Result<u16, PreviewApiError> {
    let bus = parse_i2c_bus(bus)?;
    u16::try_from(bus).map_err(|_| PreviewApiError::bad_request("EEPROM I²C bus must fit u16"))
}

fn eeprom_idle_timeout(action: &EepromHelperAction) -> Duration {
    match action {
        EepromHelperAction::Inspect => Duration::from_secs(30),
        EepromHelperAction::Provision { .. } => Duration::from_secs(60),
    }
}

fn eeprom_overall_timeout(action: &EepromHelperAction) -> Duration {
    match action {
        EepromHelperAction::Inspect => Duration::from_secs(90),
        EepromHelperAction::Provision { .. } => Duration::from_secs(180),
    }
}

fn format_eeprom_service_error(error: &EepromProvisionServiceError) -> String {
    match error {
        EepromProvisionServiceError::Helper(failure) => format!(
            "EEPROM helper failure: code={}, message={}, before={:?}, rollback={:?}, rollback_error={:?}",
            failure.code, failure.message, failure.before, failure.rollback, failure.rollback_error
        ),
        _ => error.to_string(),
    }
}

fn normalize_eeprom_map_id(map_id: &str) -> std::result::Result<String, PreviewApiError> {
    let map_id = map_id.trim();
    match map_id {
        YG_STEREO_P24C64G_V1_MAP_ID | "x5_233_default" => {
            Ok(YG_STEREO_P24C64G_V1_MAP_ID.to_owned())
        }
        "" => Err(PreviewApiError::bad_request(
            "EEPROM mapId must not be empty",
        )),
        other => Err(PreviewApiError::bad_request(format!(
            "unsupported EEPROM mapId `{other}`; expected `{YG_STEREO_P24C64G_V1_MAP_ID}`"
        ))),
    }
}

fn eeprom_snapshot_target(
    preview: &ControlPreview,
    ssh: &SshExecutionBinding,
) -> std::result::Result<EepromSnapshotTarget, PreviewApiError> {
    let target = ssh_target_from_binding(ssh)?;
    Ok(EepromSnapshotTarget {
        node_id: preview.target.node_id.clone(),
        host: target.host,
        port: target.port,
        username: target.username,
        credential_ref: validate_credential_ref(&ssh.credential_ref)?,
        map_id: preview.map_id.clone().ok_or_else(|| {
            PreviewApiError::bad_request("EEPROM preview is missing the normalized mapId")
        })?,
        bus: preview.target.bus.trim().to_owned(),
        address: preview.target.address,
    })
}

fn eeprom_provision_request_from_preview(
    preview: &ControlPreview,
    snapshot: &EepromInspectSnapshot,
) -> std::result::Result<EepromProvisionRequest, PreviewApiError> {
    let map_id = preview
        .map_id
        .as_deref()
        .ok_or_else(|| PreviewApiError::bad_request("EEPROM provision preview is missing mapId"))?;
    if map_id != YG_STEREO_P24C64G_V1_MAP_ID {
        return Err(PreviewApiError::bad_request(format!(
            "unsupported EEPROM provision map `{map_id}`"
        )));
    }
    if preview.target.address != 0x50 {
        return Err(PreviewApiError::bad_request(
            "Yg Stereo P24C64G EEPROM address must be 0x50",
        ));
    }
    if preview.target.register != 0x0010 {
        return Err(PreviewApiError::bad_request(
            "workflow-web EEPROM provision currently supports UpdateCalibration at register 0x0010 only",
        ));
    }
    if preview.target.payload.len() != YG_STEREO_P24C64G_INTRINSICS_BYTES {
        return Err(PreviewApiError::bad_request(format!(
            "UpdateCalibration payload must be exactly {YG_STEREO_P24C64G_INTRINSICS_BYTES} bytes"
        )));
    }
    if preview.verify_after_write != Some(true) {
        return Err(PreviewApiError::bad_request(
            "EEPROM provision requires verifyAfterWrite=true; the helper always performs bytewise readback verification",
        ));
    }
    let serial_number = match &snapshot.device.serial {
        EepromSerialState::Valid { value } => value.clone(),
        EepromSerialState::Empty => {
            return Err(PreviewApiError::bad_request(
                "UpdateCalibration requires an inspected EEPROM with an existing valid serial; empty devices require FullProvision from the calibration export path",
            ));
        }
        EepromSerialState::Invalid { .. } => {
            return Err(PreviewApiError::bad_request(
                "UpdateCalibration requires an inspected EEPROM with an existing valid serial; repair invalid serial state before writing calibration bytes",
            ));
        }
    };
    let request = EepromProvisionRequest {
        map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
        mode: EepromProvisioningMode::UpdateCalibration,
        serial_number,
        overwrite_existing_serial: false,
        segments: vec![EepromWriteSegment {
            offset: 0x0010,
            bytes: preview.target.payload.clone(),
        }],
    };
    request.validate().map_err(|error| {
        PreviewApiError::bad_request(format!("EEPROM provision request is invalid: {error}"))
    })?;
    Ok(request)
}

fn eeprom_snapshot_response(
    key: &str,
    snapshot: &EepromInspectSnapshot,
) -> EepromInspectSnapshotResponse {
    EepromInspectSnapshotResponse {
        key: key.to_owned(),
        image_sha256: snapshot.image_sha256.clone(),
        target: EepromSnapshotTargetResponse {
            node_id: snapshot.target.node_id.clone(),
            host: snapshot.target.host.clone(),
            port: snapshot.target.port,
            username: snapshot.target.username.clone(),
            map_id: snapshot.target.map_id.clone(),
            bus: snapshot.target.bus.clone(),
            address: snapshot.target.address,
        },
    }
}
fn x5_binding_from_request(
    binding: &X5BindingRequest,
) -> std::result::Result<(String, u16), PreviewApiError> {
    let host = binding.host.trim();
    if host.is_empty() || host.chars().any(char::is_control) {
        return Err(PreviewApiError::bad_request(
            "X5 host must be a non-empty printable host",
        ));
    }
    if binding.tcp_port == 0 {
        return Err(PreviewApiError::bad_request(
            "X5 TCP port must be in 1..=65535",
        ));
    }
    Ok((host.to_owned(), binding.tcp_port))
}

fn x5_rtsp_channel_response(channel: &x5_tcp_client::X5RtspChannelStatus) -> Value {
    json!({
        "channel": channel.channel,
        "runtimeEnabled": channel.runtime_enabled,
        "requestedEnabled": channel.requested_enabled,
        "started": channel.started,
        "txEnabled": channel.tx_enabled,
        "busy": channel.busy,
        "pendingAction": channel.pending_action,
        "lastError": channel.last_error,
        "actionId": channel.action_id,
        "lastMessage": channel.last_message,
        "port": channel.port,
        "rtspPtsValid": channel.rtsp_pts_valid,
        "rtspPtsOrigin90k": channel.rtsp_pts_origin_90k,
        "rtspPtsLast90k": channel.rtsp_pts_last_90k,
        "path": channel.path,
    })
}

fn x5_ring_response(ring: &x5_tcp_client::X5RingStatus) -> Value {
    json!({
        "channel": ring.channel,
        "depth": ring.depth,
        "valid": ring.valid,
        "writeIndex": ring.write_index,
        "minFrameId": ring.min_frame_id,
        "maxFrameId": ring.max_frame_id,
        "lastFrameId": ring.last_frame_id,
        "minTimestampNs": ring.min_timestamp_ns,
        "maxTimestampNs": ring.max_timestamp_ns,
        "lastTimestampNs": ring.last_timestamp_ns,
        "lastRtspTimestampUs": ring.last_rtsp_timestamp_us,
        "lastRtspPts90k": ring.last_rtsp_pts_90k,
        "minRtspPts90k": ring.min_rtsp_pts_90k,
        "maxRtspPts90k": ring.max_rtsp_pts_90k,
        "retentionNs": ring.retention_ns,
        "dropped": ring.dropped,
        "evicted": ring.evicted,
    })
}

fn x5_status_response(status: &x5_tcp_client::X5DriverStatus) -> Value {
    json!({
        "cameraRunning": status.camera_running,
        "rtspStarted": status.rtsp_started,
        "rtspTxEnabled": status.rtsp_tx_enabled,
        "rtspRequestedEnabled": status.rtsp_requested_enabled,
        "rtspControlBusy": status.rtsp_control_busy,
        "rtspPendingAction": status.rtsp_pending_action,
        "rtspLastError": status.rtsp_last_error,
        "rtspActionId": status.rtsp_action_id,
        "rtspLastMessage": status.rtsp_last_message,
        "rtspChannels": status.rtsp_channels.iter().map(x5_rtsp_channel_response).collect::<Vec<_>>(),
        "rings": status.rings.iter().map(x5_ring_response).collect::<Vec<_>>(),
        "fps": status.fps,
        "bitrateKbps": status.bitrate_kbps,
        "pipelineConfigVersion": status.pipeline_config_version,
    })
}

fn x5_probe_response(summary: &x5_tcp_client::X5ProbeSummary) -> Value {
    json!({
        "protocol": summary.protocol,
        "channels": summary.channels,
        "fps": summary.fps,
        "bitrateKbps": summary.bitrate_kbps,
        "pipelineConfigVersion": summary.pipeline_config_version,
        "rtspStarted": summary.rtsp_started,
        "rtspRequestedEnabled": summary.rtsp_requested_enabled,
        "rtspChannels": summary.rtsp_channels.iter().map(x5_rtsp_channel_response).collect::<Vec<_>>(),
        "rings": summary.rings.iter().map(x5_ring_response).collect::<Vec<_>>(),
    })
}

fn x5_rtsp_apply_response(summary: &x5_tcp_client::X5RtspApplySummary) -> Value {
    json!({
        "applyMode": summary.apply_mode,
        "fps": summary.fps,
        "bitrateKbps": summary.bitrate_kbps,
        "pipelineConfigVersion": summary.pipeline_config_version,
        "actionId": summary.action_id,
    })
}

fn x5_rtsp_stream_response(summary: &x5_tcp_client::X5RtspStreamSummary) -> Value {
    json!({
        "channel": summary.channel,
        "affectedChannels": summary.affected_channels,
        "requestedEnabled": summary.requested_enabled,
        "queuedAction": summary.queued_action,
        "workerBusy": summary.worker_busy,
        "actionId": summary.action_id,
    })
}
fn x5_snapshot_response(snapshot: &x5_tcp_client::X5YuvSnapshot) -> Value {
    json!({
        "channel": snapshot.channel,
        "width": snapshot.width,
        "height": snapshot.height,
        "pixelFormat": "nv12",
        "yLen": snapshot.y_len,
        "uvLen": snapshot.uv_len,
        "payloadBytes": snapshot.payload.len(),
        "frameId": snapshot.frame_id,
        "timestampNs": snapshot.timestamp_ns,
        "rtspTimestampUs": snapshot.rtsp_timestamp_us,
        "rtspPts90k": snapshot.rtsp_pts_90k,
        "matchRtspPtsDelta90k": snapshot.match_rtsp_pts_delta_90k,
        "matchMode": snapshot.match_mode,
    })
}

fn i2c_idle_timeout(action: &I2cHelperAction) -> Duration {
    match action {
        I2cHelperAction::ListBuses => Duration::from_secs(15),
        I2cHelperAction::Transfer { .. } => Duration::from_secs(30),
    }
}

fn i2c_overall_timeout(action: &I2cHelperAction) -> Duration {
    match action {
        I2cHelperAction::ListBuses => Duration::from_secs(45),
        I2cHelperAction::Transfer { .. } => Duration::from_secs(120),
    }
}

#[cfg(feature = "platform-ssh")]
fn ssh_target_from_binding(
    binding: &SshExecutionBinding,
) -> std::result::Result<SshConnectionTarget, PreviewApiError> {
    if binding.host.trim().is_empty() || binding.host.chars().any(char::is_control) {
        return Err(PreviewApiError::bad_request(
            "ssh.host must be a non-empty printable host",
        ));
    }
    if binding.port == 0 {
        return Err(PreviewApiError::bad_request(
            "ssh.port must be in 1..=65535",
        ));
    }
    if binding.username.trim().is_empty() || binding.username.chars().any(char::is_control) {
        return Err(PreviewApiError::bad_request(
            "ssh.username must be a non-empty printable username",
        ));
    }
    Ok(SshConnectionTarget {
        host: binding.host.trim().to_owned(),
        port: binding.port,
        username: binding.username.trim().to_owned(),
        expected_host_key: None,
        command_subsystem: None,
        remote_event_subsystem: None,
    })
}

#[cfg(feature = "platform-ssh")]
fn validate_credential_ref(reference: &str) -> std::result::Result<String, PreviewApiError> {
    let reference = reference.trim();
    if reference.is_empty() || reference.contains(['\0', '\n', '\r']) {
        return Err(PreviewApiError::bad_request(
            "ssh.credentialRef must be a non-empty process-local credential reference",
        ));
    }
    if !(reference.starts_with("session:") || reference.starts_with("key-file:/")) {
        return Err(PreviewApiError::bad_request(
            "ssh.credentialRef must use session:<id> or key-file:/absolute/path",
        ));
    }
    Ok(reference.to_owned())
}

#[cfg(feature = "platform-ssh")]
fn read_local_i2c_helper_payload() -> std::result::Result<Arc<[u8]>, PreviewApiError> {
    let current = std::env::current_exe().map_err(|error| {
        PreviewApiError::bad_request(format!("resolve current executable failed: {error}"))
    })?;
    let parent = current.parent().ok_or_else(|| {
        PreviewApiError::bad_request(format!(
            "current executable has no parent: {}",
            current.display()
        ))
    })?;
    let program = "camera-i2c-helper-linux-aarch64";
    let mut candidates = vec![parent.join(program)];
    if parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "deps")
        && let Some(profile_dir) = parent.parent()
    {
        candidates.push(profile_dir.join(program));
    }
    let mut missing = Vec::new();
    for candidate in &candidates {
        match fs::read(candidate) {
            Ok(bytes) => {
                validate_i2c_helper_payload(&bytes, candidate)?;
                return Ok(Arc::<[u8]>::from(bytes.into_boxed_slice()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(candidate.display().to_string());
            }
            Err(error) => {
                return Err(PreviewApiError::bad_request(format!(
                    "read I2C helper {} failed: {error}",
                    candidate.display()
                )));
            }
        }
    }
    Err(PreviewApiError::bad_request(format!(
        "I2C helper binary `{program}` was not found next to workflow-web executable; checked {}",
        missing.join(", ")
    )))
}

#[cfg(feature = "platform-ssh")]
fn validate_i2c_helper_payload(
    bytes: &[u8],
    candidate: &Path,
) -> std::result::Result<(), PreviewApiError> {
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
        Err(PreviewApiError::bad_request(format!(
            "I2C helper {} is not a Linux AArch64 ELF binary",
            candidate.display()
        )))
    }
}

fn format_i2c_service_error(error: &camera_toolbox_app::I2cHelperServiceError) -> String {
    match error {
        camera_toolbox_app::I2cHelperServiceError::Helper(failure) => format!(
            "I2C helper failure: code={}, message={}, transaction={:?}, message={:?}",
            failure.code, failure.message, failure.transaction_index, failure.message_index
        ),
        _ => error.to_string(),
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

struct WorkflowStore {
    dir: PathBuf,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    service: &'static str,
    status: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowSummary {
    id: String,
    title: String,
    revision: String,
    node_count: usize,
    edge_count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValidationResponse {
    ok: bool,
    error: Option<String>,
}

/// RuntimeGraph API 失败响应统一为 JSON，便于前端直接显示明确的诊断。
#[derive(Debug, Serialize)]
struct RuntimeApiError {
    error: String,
}

type RuntimeApiResult<T> = std::result::Result<Json<T>, (StatusCode, Json<RuntimeApiError>)>;

fn runtime_api_error(
    status: StatusCode,
    error: impl Into<String>,
) -> (StatusCode, Json<RuntimeApiError>) {
    (
        status,
        Json(RuntimeApiError {
            error: error.into(),
        }),
    )
}

/// I²C 预览请求仅描述一次可能的传输；它从不打开设备或建立远程会话。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct I2cPreviewRequest {
    node_id: String,
    profile_id: String,
    bus: String,
    address: u8,
    register: u16,
    payload: Vec<u8>,
    page_size: usize,
    operation: I2cPreviewOperation,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum I2cPreviewOperation {
    Read,
    Write,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EepromPreviewRequest {
    node_id: String,
    profile_id: String,
    bus: String,
    address: u8,
    register: u16,
    payload: Vec<u8>,
    page_size: usize,
    map_id: String,
    verify_after_write: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct I2cExecuteRequest {
    #[serde(flatten)]
    preview: I2cPreviewRequest,
    confirm_execution: bool,
    ssh: SshExecutionBinding,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SshExecutionBinding {
    host: String,
    #[serde(default = "default_ssh_port")]
    port: u16,
    #[serde(default = "default_ssh_username")]
    username: String,
    credential_ref: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EepromInspectRequest {
    #[serde(flatten)]
    preview: EepromPreviewRequest,
    ssh: SshExecutionBinding,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EepromExecuteRequest {
    #[serde(flatten)]
    preview: EepromPreviewRequest,
    confirm_execution: bool,
    expected_before_sha256: Option<String>,
    ssh: SshExecutionBinding,
}

fn default_ssh_port() -> u16 {
    22
}

fn default_ssh_username() -> String {
    "root".to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct X5BindingRequest {
    host: String,
    #[serde(default = "default_x5_tcp_port")]
    tcp_port: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct X5ConfigureRequest {
    #[serde(flatten)]
    binding: X5BindingRequest,
    fps: u16,
    bitrate_kbps: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct X5ChannelRequest {
    #[serde(flatten)]
    binding: X5BindingRequest,
    channel: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct X5ProbeRequest {
    #[serde(flatten)]
    binding: X5BindingRequest,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct X5StatusRequest {
    #[serde(flatten)]
    binding: X5BindingRequest,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum X5SnapshotMode {
    Latest,
    FrameId,
    TimestampNs,
    RtspPts90k,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct X5SnapshotRequest {
    #[serde(flatten)]
    binding: X5BindingRequest,
    channel: u16,
    mode: X5SnapshotMode,
    #[serde(default)]
    frame_id: Option<u64>,
    #[serde(default)]
    timestamp_ns: Option<u64>,
    #[serde(default)]
    rtsp_pts_90k: Option<u64>,
    #[serde(default)]
    rtsp_pts_tolerance_90k: Option<u64>,
}
#[cfg(feature = "calibration-opencv")]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CalibrationSolverRequest {
    image_size: CalibrationImageSize,
    board: CalibrationBoardSpec,
    image_points: Vec<Vec<CalibrationPoint>>,
    initial_intrinsics: CalibrationInitialIntrinsics,
}

#[cfg(feature = "calibration-opencv")]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CalibrationBoardSpec {
    inner_cols: u16,
    inner_rows: u16,
    square_size: f64,
}

#[cfg(feature = "calibration-opencv")]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CalibrationInitialIntrinsics {
    camera_matrix: [f64; 9],
    distortion_coefficients: Vec<f64>,
}

#[cfg(feature = "calibration-opencv")]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CalibrationSolverResponse {
    image_size: CalibrationImageSize,
    camera_matrix: [f64; 9],
    distortion_coefficients: Vec<f64>,
    rms_error: f64,
    calibration_flags: i32,
    views: Vec<CalibrationViewResult>,
}

#[cfg(feature = "calibration-opencv")]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CalibrationViewResult {
    rotation_vector: [f64; 3],
    translation_vector: [f64; 3],
    projected_points: Vec<CalibrationPoint>,
    reprojection_rmse: f64,
    max_reprojection_error: f64,
}

#[cfg(feature = "calibration-opencv")]
impl From<ViewCalibrationResult> for CalibrationViewResult {
    fn from(view: ViewCalibrationResult) -> Self {
        Self {
            rotation_vector: view.rotation_vector,
            translation_vector: view.translation_vector,
            projected_points: view.projected_points,
            reprojection_rmse: view.reprojection_rmse,
            max_reprojection_error: view.max_reprojection_error,
        }
    }
}

#[cfg(feature = "calibration-opencv")]
impl From<CalibrationSolution> for CalibrationSolverResponse {
    fn from(solution: CalibrationSolution) -> Self {
        Self {
            image_size: solution.image_size,
            camera_matrix: solution.camera_matrix,
            distortion_coefficients: solution.distortion_coefficients,
            rms_error: solution.rms_error,
            calibration_flags: solution.calibration_flags,
            views: solution.views.into_iter().map(Into::into).collect(),
        }
    }
}

fn default_x5_tcp_port() -> u16 {
    9073
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlExecutionResult {
    preview: ControlPreview,
    execution: &'static str,
    result: ControlExecutionPayload,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
enum ControlExecutionPayload {
    I2c(I2cHelperResult),
    Eeprom(EepromHelperResult),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EepromInspectResponse {
    preview: ControlPreview,
    snapshot: EepromInspectSnapshotResponse,
    result: EepromHelperResult,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EepromInspectSnapshotResponse {
    key: String,
    image_sha256: String,
    target: EepromSnapshotTargetResponse,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EepromSnapshotTargetResponse {
    node_id: String,
    host: String,
    port: u16,
    username: String,
    map_id: String,
    bus: String,
    address: u8,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlPreview {
    target: ControlTarget,
    operation: &'static str,
    page_split_estimate: PageSplitEstimate,
    requires_confirmation: bool,
    execution: &'static str,
    map_id: Option<String>,
    verify_after_write: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlTarget {
    node_id: String,
    profile_id: String,
    bus: String,
    address: u8,
    register: u16,
    payload: Vec<u8>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PageSplitEstimate {
    page_size: usize,
    write_count: usize,
    segments: Vec<PageSplitSegment>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PageSplitSegment {
    register: u16,
    payload_length: usize,
}

#[derive(Debug, Serialize)]
struct PreviewApiError {
    error: String,
}

impl PreviewApiError {
    fn bad_request(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
        }
    }
}

impl IntoResponse for PreviewApiError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, Json(self)).into_response()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MjpegStreamQuery {
    url: String,
    fps: Option<u16>,
    width: Option<u16>,
    height: Option<u16>,
}

struct MjpegStreamConfig {
    url: String,
    fps_limit: Option<u16>,
    width: u16,
    height: u16,
}

/// 本地图像预览只接受相对路径，并在解析符号链接后仍限定于声明的 workspace 根目录。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalImageQuery {
    workspace_root: String,
    relative_path: String,
}

#[derive(Debug, Serialize)]
struct LocalImageApiError {
    error: String,
}

impl LocalImageApiError {
    fn new(status: StatusCode, error: impl Into<String>) -> (StatusCode, Json<Self>) {
        (
            status,
            Json(Self {
                error: error.into(),
            }),
        )
    }
}

fn canonical_workspace_root(
    workspace_root: &str,
) -> std::result::Result<PathBuf, (StatusCode, Json<LocalImageApiError>)> {
    let trimmed = workspace_root.trim();
    if trimmed.is_empty() {
        return Err(LocalImageApiError::new(
            StatusCode::BAD_REQUEST,
            "workspaceRoot must not be empty",
        ));
    }
    let path = Path::new(trimmed);
    if !path.is_absolute() {
        return Err(LocalImageApiError::new(
            StatusCode::BAD_REQUEST,
            "workspaceRoot must be an absolute path",
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        LocalImageApiError::new(
            StatusCode::NOT_FOUND,
            format!("workspace root could not be resolved: {error}"),
        )
    })?;
    if !canonical.is_dir() {
        return Err(LocalImageApiError::new(
            StatusCode::BAD_REQUEST,
            "workspaceRoot must resolve to a directory",
        ));
    }
    Ok(canonical)
}

fn validate_relative_image_path(
    relative_path: &str,
) -> std::result::Result<PathBuf, (StatusCode, Json<LocalImageApiError>)> {
    let trimmed = relative_path.trim();
    if trimmed.is_empty() {
        return Err(LocalImageApiError::new(
            StatusCode::BAD_REQUEST,
            "relativePath must not be empty",
        ));
    }
    let mut normalized = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => continue,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(LocalImageApiError::new(
                    StatusCode::BAD_REQUEST,
                    "relativePath must stay inside the workspace root",
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(LocalImageApiError::new(
            StatusCode::BAD_REQUEST,
            "relativePath must not resolve to the workspace root",
        ));
    }
    Ok(normalized)
}

fn local_image_content_type(
    image_path: &Path,
) -> std::result::Result<&'static str, (StatusCode, Json<LocalImageApiError>)> {
    let extension = image_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());
    let content_type = match extension.as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("tif") | Some("tiff") => "image/tiff",
        Some("avif") => "image/avif",
        _ => {
            return Err(LocalImageApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("unsupported image format: {}", image_path.display()),
            ));
        }
    };
    Ok(content_type)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let _logging = camera_toolbox_logging::init();
    let args = ServerArgs::parse();
    let static_dir = args.static_dir.unwrap_or_else(default_static_dir);
    let workflow_dir = args.workflow_dir.unwrap_or_else(default_workflow_dir);
    ensure_static_dir(&static_dir)?;
    fs::create_dir_all(&workflow_dir)
        .with_context(|| format!("failed to create workflow dir {}", workflow_dir.display()))?;

    let listener = TcpListener::bind((args.host, args.port))
        .await
        .with_context(|| format!("failed to bind {}:{}", args.host, args.port))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read listener address")?;
    let router = app_router(static_dir.clone(), workflow_dir.clone());

    println!("Camera Toolbox Workflow Web listening on http://{local_addr}");
    println!("Serving frontend assets from {}", static_dir.display());
    println!("Saving workflows under {}", workflow_dir.display());
    tracing::info!(operation = "workflow_web_start", address = %local_addr, static_dir = %static_dir.display(), workflow_dir = %workflow_dir.display());

    axum::serve(listener, router)
        .await
        .context("workflow web server stopped unexpectedly")
}

fn app_router(static_dir: PathBuf, workflow_dir: PathBuf) -> Router {
    let index = static_dir.join("index.html");
    let frontend = ServeDir::new(static_dir).not_found_service(ServeFile::new(index));
    let state = AppState {
        workflow_store: Arc::new(WorkflowStore { dir: workflow_dir }),
        runtime_sessions: Arc::new(Mutex::new(HashMap::new())),
        control_runtime: Arc::new(ControlRuntime::production()),
        #[cfg(feature = "calibration-opencv")]
        calibration_backend: Arc::new(OpenCvCalibrationBackend),
        eeprom_inspects: Arc::new(Mutex::new(HashMap::new())),
        engine_runtime: Arc::new(engine_api::EngineRuntime::new()),
    };

    Router::new()
        .route("/api/health", get(health))
        .route("/api/workflow", get(workflow_graph))
        .route("/api/node-catalog", get(node_catalog_api))
        .route("/api/workmode-templates", get(workmode_templates_api))
        .route("/api/workflows", get(list_workflows).post(create_workflow))
        .route("/api/workflows/import", post(import_workflow))
        .route(
            "/api/workflows/{id}",
            get(get_workflow).put(put_workflow).delete(delete_workflow),
        )
        .route("/api/workflows/{id}/export", get(export_workflow))
        .route("/api/workflows/{id}/validate", post(validate_workflow_api))
        .route("/api/workflows/{id}/runtime", get(get_workflow_runtime))
        .route(
            "/api/workflows/{id}/runtime/run",
            post(run_workflow_runtime),
        )
        .route(
            "/api/workflows/{id}/runtime/stop",
            post(stop_workflow_runtime),
        )
        .route("/api/control/i2c/preview", post(preview_i2c_transfer))
        .route(
            "/api/control/eeprom/inspect",
            post(inspect_eeprom_provision),
        )
        .route("/api/control/i2c/run", post(run_i2c_transfer))
        .route(
            "/api/control/calibration/solver/run",
            post(run_calibration_solver),
        )
        .route("/api/control/eeprom/run", post(run_eeprom_provision))
        .route(
            "/api/control/eeprom/preview",
            post(preview_eeprom_provision),
        )
        .route("/api/control/x5/probe", post(probe_x5_control))
        .route("/api/control/x5/status", post(status_x5_control))
        .route("/api/control/x5/configure-rtsp", post(configure_x5_rtsp))
        .route("/api/control/x5/start-rtsp", post(start_x5_rtsp_channel))
        .route("/api/control/x5/stop-rtsp", post(stop_x5_rtsp_channel))
        .route("/api/control/x5/snapshot", post(capture_x5_snapshot))
        .route("/api/streams/mjpeg", get(mjpeg_stream))
        .route("/api/images/local", get(local_image_preview))
        .route("/api/files/local/list", get(files_api::list_local_files))
        .route("/api/runtime/run", post(engine_api::run_engine))
        .route("/api/runtime/stop", post(engine_api::stop_engine))
        .route(
            "/api/runtime/nodes/{id}/action",
            post(engine_api::node_action),
        )
        .route("/api/runtime/status", get(engine_api::engine_status))
        .route(
            "/api/runtime/viewer/{id}/frame",
            get(engine_api::viewer_frame),
        )
        .fallback_service(frontend)
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        service: "camera-toolbox-web",
        status: "ok",
    })
}

async fn workflow_graph() -> Json<WorkflowGraph> {
    let graph = seed_workflow_graph();
    debug_assert!(validate_workflow(&graph).is_ok());
    Json(graph)
}

async fn node_catalog_api() -> Json<Vec<workflow::NodeDefinition>> {
    Json(node_catalog())
}

async fn workmode_templates_api() -> Json<Vec<workflow::WorkmodeTemplate>> {
    Json(workmode_templates())
}

async fn list_workflows(
    State(state): State<AppState>,
) -> std::result::Result<Json<Vec<WorkflowSummary>>, (StatusCode, String)> {
    state.workflow_store.list().map(Json)
}

async fn create_workflow(
    State(state): State<AppState>,
    Json(mut graph): Json<WorkflowGraph>,
) -> std::result::Result<(StatusCode, Json<WorkflowGraph>), (StatusCode, String)> {
    if graph.id.trim().is_empty() {
        graph.id = format!("workflow-{}", next_revision());
    }
    let revision = next_revision();
    let graph = normalize_workflow(graph, revision).map_err(bad_request)?;
    state.workflow_store.save(&graph)?;
    Ok((StatusCode::CREATED, Json(graph)))
}

async fn import_workflow(
    State(state): State<AppState>,
    Json(graph): Json<WorkflowGraph>,
) -> std::result::Result<(StatusCode, Json<WorkflowGraph>), (StatusCode, String)> {
    create_or_replace_workflow(state, graph)
        .await
        .map(|graph| (StatusCode::CREATED, Json(graph)))
}

async fn export_workflow(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> std::result::Result<Response, (StatusCode, String)> {
    let graph = state.workflow_store.load(&id)?;
    let body = serde_json::to_vec_pretty(&graph).map_err(internal_error)?;
    let mut response = Body::from(body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"{}.ctworkflow.json\"",
            graph.id
        ))
        .map_err(internal_error)?,
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn get_workflow(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> std::result::Result<Json<WorkflowGraph>, (StatusCode, String)> {
    state.workflow_store.load(&id).map(Json)
}

async fn put_workflow(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(mut graph): Json<WorkflowGraph>,
) -> std::result::Result<Json<WorkflowGraph>, (StatusCode, String)> {
    graph.id = id.clone();
    if let Some(current) = state.workflow_store.load_optional(&id)? {
        if_match_revision(&headers, &current.revision)?;
    }
    let graph = normalize_workflow(graph, next_revision()).map_err(bad_request)?;
    state.workflow_store.save(&graph)?;
    Ok(Json(graph))
}

async fn delete_workflow(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    state.workflow_store.delete(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn validate_workflow_api(
    Json(graph): Json<WorkflowGraph>,
) -> (StatusCode, Json<ValidationResponse>) {
    match validate_workflow(&graph) {
        Ok(()) => (
            StatusCode::OK,
            Json(ValidationResponse {
                ok: true,
                error: None,
            }),
        ),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(ValidationResponse {
                ok: false,
                error: Some(error),
            }),
        ),
    }
}

async fn probe_x5_control(
    request: std::result::Result<Json<X5ProbeRequest>, JsonRejection>,
) -> std::result::Result<Json<Value>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid X5 probe request: {error}"))
    })?;
    let (host, port) = x5_binding_from_request(&request.binding)?;
    let summary = x5_tcp_client::probe(&host, port).map_err(PreviewApiError::bad_request)?;
    Ok(Json(x5_probe_response(&summary)))
}

async fn status_x5_control(
    request: std::result::Result<Json<X5StatusRequest>, JsonRejection>,
) -> std::result::Result<Json<Value>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid X5 status request: {error}"))
    })?;
    let (host, port) = x5_binding_from_request(&request.binding)?;
    let status = x5_tcp_client::status(&host, port).map_err(PreviewApiError::bad_request)?;
    Ok(Json(x5_status_response(&status)))
}

async fn configure_x5_rtsp(
    request: std::result::Result<Json<X5ConfigureRequest>, JsonRejection>,
) -> std::result::Result<Json<Value>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid X5 RTSP configure request: {error}"))
    })?;
    let (host, port) = x5_binding_from_request(&request.binding)?;
    let summary = x5_tcp_client::configure_rtsp(
        &host,
        port,
        x5_tcp_client::X5RtspEncoderConfig {
            fps: request.fps,
            bitrate_kbps: request.bitrate_kbps,
        },
    )
    .map_err(PreviewApiError::bad_request)?;
    Ok(Json(x5_rtsp_apply_response(&summary)))
}

async fn start_x5_rtsp_channel(
    request: std::result::Result<Json<X5ChannelRequest>, JsonRejection>,
) -> std::result::Result<Json<Value>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid X5 RTSP start request: {error}"))
    })?;
    let (host, port) = x5_binding_from_request(&request.binding)?;
    let summary = x5_tcp_client::start_rtsp_channel(&host, port, request.channel)
        .map_err(PreviewApiError::bad_request)?;
    Ok(Json(x5_rtsp_stream_response(&summary)))
}

async fn stop_x5_rtsp_channel(
    request: std::result::Result<Json<X5ChannelRequest>, JsonRejection>,
) -> std::result::Result<Json<Value>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid X5 RTSP stop request: {error}"))
    })?;
    let (host, port) = x5_binding_from_request(&request.binding)?;
    let summary = x5_tcp_client::stop_rtsp_channel(&host, port, request.channel)
        .map_err(PreviewApiError::bad_request)?;
    Ok(Json(x5_rtsp_stream_response(&summary)))
}

async fn capture_x5_snapshot(
    request: std::result::Result<Json<X5SnapshotRequest>, JsonRejection>,
) -> std::result::Result<Json<Value>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid X5 snapshot request: {error}"))
    })?;
    let (host, port) = x5_binding_from_request(&request.binding)?;
    let snapshot = match request.mode {
        X5SnapshotMode::Latest => x5_tcp_client::capture_yuv_snapshot(&host, port, request.channel),
        X5SnapshotMode::FrameId => {
            let frame_id = request.frame_id.ok_or_else(|| {
                PreviewApiError::bad_request("X5 frame_id snapshot requires frameId")
            })?;
            x5_tcp_client::capture_yuv_snapshot_by_frame_id(&host, port, request.channel, frame_id)
        }
        X5SnapshotMode::TimestampNs => {
            let timestamp_ns = request.timestamp_ns.ok_or_else(|| {
                PreviewApiError::bad_request("X5 timestamp snapshot requires timestampNs")
            })?;
            x5_tcp_client::capture_yuv_snapshot_by_timestamp_ns(
                &host,
                port,
                request.channel,
                timestamp_ns,
            )
        }
        X5SnapshotMode::RtspPts90k => {
            let rtsp_pts_90k = request.rtsp_pts_90k.ok_or_else(|| {
                PreviewApiError::bad_request("X5 rtsp_pts_90k snapshot requires rtspPts90k")
            })?;
            x5_tcp_client::capture_yuv_snapshot_by_rtsp_pts_90k(
                &host,
                port,
                request.channel,
                rtsp_pts_90k,
                request.rtsp_pts_tolerance_90k.unwrap_or(0),
            )
        }
    }
    .map_err(PreviewApiError::bad_request)?;
    Ok(Json(x5_snapshot_response(&snapshot)))
}

async fn create_or_replace_workflow(
    state: AppState,
    mut graph: WorkflowGraph,
) -> std::result::Result<WorkflowGraph, (StatusCode, String)> {
    if graph.id.trim().is_empty() {
        graph.id = format!("workflow-{}", next_revision());
    }
    let graph = normalize_workflow(graph, next_revision()).map_err(bad_request)?;
    state.workflow_store.save(&graph)?;
    Ok(graph)
}

/// 启动纯内存的 Stage 7 诊断会话；不会连接 RTSP/SSH/X5/I²C，也不会写 EEPROM。
async fn run_workflow_runtime(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    request: std::result::Result<Json<WorkflowGraph>, JsonRejection>,
) -> RuntimeApiResult<RuntimeGraphStatus> {
    let Json(graph) = request.map_err(|error| {
        runtime_api_error(
            StatusCode::BAD_REQUEST,
            format!("invalid runtime graph request: {error}"),
        )
    })?;
    if graph.id != id {
        return Err(runtime_api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "workflow ID in path `{id}` does not match request graph `{}`",
                graph.id
            ),
        ));
    }
    validate_workflow(&graph).map_err(|error| runtime_api_error(StatusCode::BAD_REQUEST, error))?;

    let status = runtime_graph_status(&graph, true);
    let mut sessions = state.runtime_sessions.lock().map_err(|_| {
        runtime_api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "runtime session state is unavailable",
        )
    })?;
    sessions.insert(
        id,
        RuntimeGraphSession {
            graph,
            status: status.clone(),
        },
    );
    Ok(Json(status))
}

/// 获取指定工作流的进程内诊断快照。工作流文件及其 revision 不会被读取或修改。
async fn get_workflow_runtime(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> RuntimeApiResult<RuntimeGraphStatus> {
    let sessions = state.runtime_sessions.lock().map_err(|_| {
        runtime_api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "runtime session state is unavailable",
        )
    })?;
    let status = sessions
        .get(&id)
        .map(|session| session.status.clone())
        .ok_or_else(|| {
            runtime_api_error(
                StatusCode::NOT_FOUND,
                format!("no runtime session exists for workflow `{id}`"),
            )
        })?;
    Ok(Json(status))
}

/// 停止运行时标记并保留节点级 idle 诊断；不会执行外部停止命令。
async fn stop_workflow_runtime(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> RuntimeApiResult<RuntimeGraphStatus> {
    let mut sessions = state.runtime_sessions.lock().map_err(|_| {
        runtime_api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "runtime session state is unavailable",
        )
    })?;
    let session = sessions.get_mut(&id).ok_or_else(|| {
        runtime_api_error(
            StatusCode::NOT_FOUND,
            format!("no runtime session exists for workflow `{id}`"),
        )
    })?;
    let status = runtime_graph_status(&session.graph, false);
    session.status = status.clone();
    Ok(Json(status))
}

async fn local_image_preview(
    Query(query): Query<LocalImageQuery>,
) -> std::result::Result<Response, (StatusCode, Json<LocalImageApiError>)> {
    let workspace_root = canonical_workspace_root(&query.workspace_root)?;
    let relative_path = validate_relative_image_path(&query.relative_path)?;
    let image_path = workspace_root.join(relative_path);
    let image_path = fs::canonicalize(&image_path).map_err(|error| {
        LocalImageApiError::new(
            StatusCode::NOT_FOUND,
            format!("image file could not be resolved: {error}"),
        )
    })?;
    if !image_path.starts_with(&workspace_root) {
        return Err(LocalImageApiError::new(
            StatusCode::FORBIDDEN,
            "image path resolves outside the configured workspace root",
        ));
    }
    if !image_path.is_file() {
        return Err(LocalImageApiError::new(
            StatusCode::BAD_REQUEST,
            "image path must resolve to a regular file",
        ));
    }
    let content_type = local_image_content_type(&image_path)?;
    let bytes = fs::read(&image_path).map_err(|error| {
        LocalImageApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read image file: {error}"),
        )
    })?;
    let mut response = Body::from(bytes).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
async fn preview_i2c_transfer(
    request: std::result::Result<Json<I2cPreviewRequest>, JsonRejection>,
) -> std::result::Result<Json<ControlPreview>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid I²C preview request: {error}"))
    })?;
    build_i2c_preview(request).map(Json)
}

async fn run_i2c_transfer(
    State(state): State<AppState>,
    request: std::result::Result<Json<I2cExecuteRequest>, JsonRejection>,
) -> std::result::Result<Json<ControlExecutionResult>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid I²C execution request: {error}"))
    })?;
    let preview = build_i2c_preview(request.preview)?;
    if preview.requires_confirmation && !request.confirm_execution {
        return Err(PreviewApiError::bad_request(
            "I²C write execution requires confirmExecution=true",
        ));
    }
    let result = state.control_runtime.execute_i2c(&preview, &request.ssh)?;
    Ok(Json(ControlExecutionResult {
        preview,
        execution: "completed",
        result: ControlExecutionPayload::I2c(result),
    }))
}

async fn preview_eeprom_provision(
    request: std::result::Result<Json<EepromPreviewRequest>, JsonRejection>,
) -> std::result::Result<Json<ControlPreview>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid EEPROM preview request: {error}"))
    })?;
    build_eeprom_preview(request).map(Json)
}

async fn inspect_eeprom_provision(
    State(state): State<AppState>,
    request: std::result::Result<Json<EepromInspectRequest>, JsonRejection>,
) -> std::result::Result<Json<EepromInspectResponse>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid EEPROM inspect request: {error}"))
    })?;
    let preview = build_eeprom_preview(request.preview)?;
    let target = eeprom_snapshot_target(&preview, &request.ssh)?;
    let result = state.control_runtime.execute_eeprom(
        &preview,
        &request.ssh,
        EepromHelperAction::Inspect,
    )?;
    let EepromHelperResult::Inspect(inspect) = &result else {
        return Err(PreviewApiError::bad_request(
            "EEPROM inspect returned a non-inspect helper result",
        ));
    };
    let key = target.node_id.clone();
    let snapshot = EepromInspectSnapshot {
        target: target.clone(),
        image_sha256: inspect.state.image_sha256.clone(),
        device: inspect.state.clone(),
    };
    let response = EepromInspectResponse {
        preview,
        snapshot: eeprom_snapshot_response(&key, &snapshot),
        result,
    };
    state
        .eeprom_inspects
        .lock()
        .map_err(|_| PreviewApiError::bad_request("EEPROM inspect snapshot state is unavailable"))?
        .insert(key, snapshot);
    Ok(Json(response))
}

async fn run_eeprom_provision(
    State(state): State<AppState>,
    request: std::result::Result<Json<EepromExecuteRequest>, JsonRejection>,
) -> std::result::Result<Json<ControlExecutionResult>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid EEPROM execution request: {error}"))
    })?;
    let preview = build_eeprom_preview(request.preview)?;
    if !request.confirm_execution {
        return Err(PreviewApiError::bad_request(
            "EEPROM provision requires confirmExecution=true",
        ));
    }
    let Some(expected_before_sha256) = request.expected_before_sha256.as_deref() else {
        return Err(PreviewApiError::bad_request(
            "EEPROM provision requires expectedBeforeSha256 from the latest inspect",
        ));
    };
    if !is_sha256(expected_before_sha256) {
        return Err(PreviewApiError::bad_request(
            "expectedBeforeSha256 must contain 64 lowercase hex characters",
        ));
    }
    let target = eeprom_snapshot_target(&preview, &request.ssh)?;
    let snapshot = state
        .eeprom_inspects
        .lock()
        .map_err(|_| PreviewApiError::bad_request("EEPROM inspect snapshot state is unavailable"))?
        .get(&preview.target.node_id)
        .cloned()
        .ok_or_else(|| {
            PreviewApiError::bad_request(
                "EEPROM provision requires a process-local inspect snapshot for this node",
            )
        })?;
    if snapshot.target != target {
        return Err(PreviewApiError::bad_request(
            "EEPROM target changed after inspect; inspect the selected SSH/bus/map/address again",
        ));
    }
    if snapshot.image_sha256 != expected_before_sha256 {
        return Err(PreviewApiError::bad_request(
            "expectedBeforeSha256 does not match the latest process-local inspect snapshot",
        ));
    }
    let provision_request = eeprom_provision_request_from_preview(&preview, &snapshot)?;
    let result = state.control_runtime.execute_eeprom(
        &preview,
        &request.ssh,
        EepromHelperAction::Provision {
            request: provision_request,
            expected_before_sha256: expected_before_sha256.to_owned(),
        },
    )?;
    state
        .eeprom_inspects
        .lock()
        .map_err(|_| PreviewApiError::bad_request("EEPROM inspect snapshot state is unavailable"))?
        .remove(&preview.target.node_id);
    Ok(Json(ControlExecutionResult {
        preview,
        execution: "completed",
        result: ControlExecutionPayload::Eeprom(result),
    }))
}

#[cfg(feature = "calibration-opencv")]
async fn run_calibration_solver(
    State(state): State<AppState>,
    Json(request): Json<CalibrationSolverRequest>,
) -> std::result::Result<Json<CalibrationSolverResponse>, PreviewApiError> {
    let request = into_calibration_request(request)?;
    let cancellation = CalibrationCancellation::default();
    let solution = state
        .calibration_backend
        .calibrate(&request, &cancellation)
        .map_err(|error| PreviewApiError::bad_request(error.to_string()))?;
    Ok(Json(CalibrationSolverResponse::from(solution)))
}

#[cfg(feature = "calibration-opencv")]
fn into_calibration_request(
    request: CalibrationSolverRequest,
) -> std::result::Result<CalibrationRequest, PreviewApiError> {
    let image_size = CalibrationImageSize::new(request.image_size.width, request.image_size.height)
        .map_err(|error| {
            PreviewApiError::bad_request(format!("invalid calibration imageSize: {error}"))
        })?;
    let board = BoardSpec::new(
        request.board.inner_cols,
        request.board.inner_rows,
        request.board.square_size,
    )
    .map_err(|error| PreviewApiError::bad_request(format!("invalid calibration board: {error}")))?;
    let calibration_request = CalibrationRequest {
        image_size,
        board,
        image_points: request.image_points,
        initial_intrinsics: InitialIntrinsics {
            camera_matrix: request.initial_intrinsics.camera_matrix,
            distortion_coefficients: request.initial_intrinsics.distortion_coefficients,
        },
    };
    calibration_request.validate().map_err(|error| {
        PreviewApiError::bad_request(format!("invalid calibration solver request: {error}"))
    })?;
    Ok(calibration_request)
}

fn build_i2c_preview(
    request: I2cPreviewRequest,
) -> std::result::Result<ControlPreview, PreviewApiError> {
    let target = ControlTarget {
        node_id: request.node_id,
        profile_id: request.profile_id,
        bus: request.bus,
        address: request.address,
        register: request.register,
        payload: request.payload,
    };
    validate_preview_target(&target)?;
    let is_write = matches!(request.operation, I2cPreviewOperation::Write);
    if is_write && target.payload.is_empty() {
        return Err(PreviewApiError::bad_request(
            "I²C write preview requires at least one payload byte",
        ));
    }
    if !is_write && !target.payload.is_empty() {
        return Err(PreviewApiError::bad_request(
            "I²C read preview must not include a write payload",
        ));
    }
    let page_split_estimate =
        page_split_estimate(target.register, target.payload.len(), request.page_size)?;
    Ok(ControlPreview {
        target,
        operation: if is_write { "write" } else { "read" },
        page_split_estimate,
        requires_confirmation: is_write,
        execution: "preview-only",
        map_id: None,
        verify_after_write: None,
    })
}

fn build_eeprom_preview(
    request: EepromPreviewRequest,
) -> std::result::Result<ControlPreview, PreviewApiError> {
    let map_id = normalize_eeprom_map_id(&request.map_id)?;
    let target = ControlTarget {
        node_id: request.node_id,
        profile_id: request.profile_id,
        bus: request.bus,
        address: request.address,
        register: request.register,
        payload: request.payload,
    };
    validate_preview_target(&target)?;
    if target.payload.is_empty() {
        return Err(PreviewApiError::bad_request(
            "EEPROM provision preview requires at least one payload byte",
        ));
    }
    let page_split_estimate =
        page_split_estimate(target.register, target.payload.len(), request.page_size)?;
    Ok(ControlPreview {
        target,
        operation: "provision",
        page_split_estimate,
        requires_confirmation: true,
        execution: "preview-only",
        map_id: Some(map_id),
        verify_after_write: Some(request.verify_after_write),
    })
}

fn validate_preview_target(target: &ControlTarget) -> std::result::Result<(), PreviewApiError> {
    if target.node_id.trim().is_empty() {
        return Err(PreviewApiError::bad_request("nodeId must not be empty"));
    }
    if target.profile_id.trim().is_empty() {
        return Err(PreviewApiError::bad_request("profileId must not be empty"));
    }
    if target.bus.trim().is_empty() || target.bus.chars().any(char::is_control) {
        return Err(PreviewApiError::bad_request(
            "bus must be a non-empty printable identifier",
        ));
    }
    if !(0x03..=0x77).contains(&target.address) {
        return Err(PreviewApiError::bad_request(
            "address must be a 7-bit I²C address in 0x03..=0x77",
        ));
    }
    if target.payload.len() > 4096 {
        return Err(PreviewApiError::bad_request(
            "payload exceeds the 4096-byte preview limit",
        ));
    }
    Ok(())
}

fn page_split_estimate(
    register: u16,
    payload_length: usize,
    page_size: usize,
) -> std::result::Result<PageSplitEstimate, PreviewApiError> {
    if !(1..=256).contains(&page_size) {
        return Err(PreviewApiError::bad_request(
            "pageSize must be in 1..=256 bytes",
        ));
    }
    let mut segments = Vec::new();
    let mut next_register = usize::from(register);
    let end_register = next_register
        .checked_add(payload_length)
        .ok_or_else(|| PreviewApiError::bad_request("payload register range overflows"))?;
    if end_register > usize::from(u16::MAX) + 1 {
        return Err(PreviewApiError::bad_request(
            "payload exceeds the 16-bit register range",
        ));
    }
    while next_register < end_register {
        let page_remaining = page_size - (next_register % page_size);
        let payload_length = page_remaining.min(end_register - next_register);
        segments.push(PageSplitSegment {
            register: next_register as u16,
            payload_length,
        });
        next_register += payload_length;
    }
    Ok(PageSplitEstimate {
        page_size,
        write_count: segments.len(),
        segments,
    })
}

fn if_match_revision(
    headers: &HeaderMap,
    current_revision: &str,
) -> std::result::Result<(), (StatusCode, String)> {
    let Some(raw) = headers.get(header::IF_MATCH) else {
        return Ok(());
    };
    let expected = raw
        .to_str()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "invalid If-Match header".to_owned(),
            )
        })?
        .trim_matches('"');
    if expected != current_revision {
        return Err((
            StatusCode::CONFLICT,
            format!("workflow revision conflict: current `{current_revision}`, got `{expected}`"),
        ));
    }
    Ok(())
}

async fn mjpeg_stream(
    Query(query): Query<MjpegStreamQuery>,
) -> std::result::Result<Response, (StatusCode, String)> {
    let config = MjpegStreamConfig::from_query(query)?;
    let latest_frame = Arc::new(LatestDecodedFrameSlot::default());
    let cancellation = StreamCancellation::default();
    let session_id = StreamSessionId::new(format!(
        "workflow-mjpeg-{}",
        NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed)
    ))
    .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let decoder = FfmpegRtspDecoder::start(
        &config.url,
        FfmpegRtspTransport::Tcp,
        RtspLatencyMode::Low,
        u32::from(config.width),
        u32::from(config.height),
        session_id,
        0,
        Arc::clone(&latest_frame),
        Duration::from_secs(8),
        false,
        &cancellation,
    )
    .map_err(|error| {
        (
            StatusCode::BAD_GATEWAY,
            format!("failed to start internal RTSP decoder: {error}"),
        )
    })?;

    let frame_interval = config
        .fps_limit
        .map(|fps| Duration::from_secs_f64(1.0 / f64::from(fps)));
    let body_stream = async_stream::stream! {
        let _decoder = decoder;
        let _cancellation = cancellation;
        let mut last_sequence = None;
        let mut next_frame_at = Instant::now();
        loop {
            if let Some(completion) = _decoder.completion() {
                if let Err(error) = completion {
                    tracing::debug!(operation = "mjpeg_internal_decoder", error = %error);
                }
                break;
            }
            if let Some(frame) = latest_frame.latest()
                && last_sequence != Some(frame.identity.frame_sequence)
            {
                if let Some(interval) = frame_interval {
                    let now = Instant::now();
                    if now < next_frame_at {
                        sleep_until(next_frame_at).await;
                        continue;
                    }
                    next_frame_at += interval;
                    let now = Instant::now();
                    while next_frame_at <= now {
                        next_frame_at += interval;
                    }
                }
                last_sequence = Some(frame.identity.frame_sequence);
                let stats = _decoder.stats().snapshot();
                match mjpeg_chunk(&frame, &stats) {
                    Ok(chunk) => yield Ok::<Bytes, std::io::Error>(Bytes::from(chunk)),
                    Err(error) => yield Err(std::io::Error::other(error)),
                }
                continue;
            }
            sleep(Duration::from_millis(10)).await;
        }
    };

    let mut response = Body::from_stream(body_stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("multipart/x-mixed-replace; boundary=frame"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    Ok(response)
}

fn mjpeg_chunk(
    frame: &DecodedVideoFrame,
    stats: &FfmpegRtspDecoderStatsSnapshot,
) -> Result<Vec<u8>, String> {
    let encode_start = Instant::now();
    let jpeg = encode_rgba_as_jpeg(frame)?;
    let encode_ns = duration_nanos(encode_start.elapsed());
    let sent_at_ns = host_monotonic_time_ns();
    let headers = format!(
        "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nX-Frame-Sequence: {}\r\nX-Frame-Published-At-Ns: {}\r\nX-Mjpeg-Sent-At-Ns: {}\r\nX-Decoder-Frames: {}\r\nX-Decoder-Codec-Ns: {}\r\nX-Decoder-Scale-Ns: {}\r\nX-Decoder-Copy-Ns: {}\r\nX-Mjpeg-Encode-Ns: {}\r\nX-Mjpeg-Jpeg-Bytes: {}\r\n\r\n",
        jpeg.len(),
        frame.identity.frame_sequence,
        frame.identity.host_monotonic_time_ns,
        sent_at_ns,
        stats.decoded_frames,
        stats.codec_stage_ns,
        stats.scale_stage_ns,
        stats.copy_stage_ns,
        encode_ns,
        jpeg.len(),
    );
    let mut chunk = Vec::with_capacity(headers.len() + jpeg.len() + 2);
    chunk.extend_from_slice(headers.as_bytes());
    chunk.extend_from_slice(&jpeg);
    chunk.extend_from_slice(b"\r\n");
    Ok(chunk)
}

fn encode_rgba_as_jpeg(frame: &DecodedVideoFrame) -> Result<Vec<u8>, String> {
    let pixel_count = u64::from(frame.width)
        .checked_mul(u64::from(frame.height))
        .ok_or_else(|| "frame dimensions overflow".to_owned())?;
    let expected_rgba_len = usize::try_from(pixel_count.saturating_mul(4))
        .map_err(|_| "frame byte length overflows usize".to_owned())?;
    if frame.rgba.len() != expected_rgba_len {
        return Err(format!(
            "RGBA frame length mismatch: expected {expected_rgba_len}, got {}",
            frame.rgba.len()
        ));
    }
    let rgb_len = usize::try_from(pixel_count.saturating_mul(3))
        .map_err(|_| "RGB frame byte length overflows usize".to_owned())?;
    let mut rgb = Vec::with_capacity(rgb_len);
    for pixel in frame.rgba.chunks_exact(4) {
        rgb.extend_from_slice(&pixel[..3]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 82)
        .encode(&rgb, frame.width, frame.height, ColorType::Rgb8.into())
        .map_err(|error| format!("JPEG encode failed: {error}"))?;
    Ok(jpeg)
}

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

impl MjpegStreamConfig {
    fn from_query(query: MjpegStreamQuery) -> std::result::Result<Self, (StatusCode, String)> {
        let url = query.url.trim();
        if !(url.starts_with("rtsp://") || url.starts_with("rtsps://")) {
            return Err((
                StatusCode::BAD_REQUEST,
                "viewer stream URL must use rtsp:// or rtsps://".to_owned(),
            ));
        }
        let width = query.width.unwrap_or(960).clamp(160, 1920);
        let default_height = u16::try_from(u32::from(width).saturating_mul(9) / 16)
            .unwrap_or(u16::MAX)
            .clamp(90, 1080);
        Ok(Self {
            url: url.to_owned(),
            // 不传 fps 时跟随 RTSP/decoder 发布的新帧；显式传入时才做预览降采样。
            fps_limit: query.fps.map(|fps| fps.clamp(1, 120)),
            width,
            height: query.height.unwrap_or(default_height).clamp(90, 1080),
        })
    }
}

impl WorkflowStore {
    fn list(&self) -> std::result::Result<Vec<WorkflowSummary>, (StatusCode, String)> {
        let mut summaries = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(internal_error)? {
            let entry = entry.map_err(internal_error)?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let graph = read_workflow_file(&path)?;
            summaries.push(WorkflowSummary {
                id: graph.id,
                title: graph.title,
                revision: graph.revision,
                node_count: graph.nodes.len(),
                edge_count: graph.edges.len(),
            });
        }
        summaries.sort_by(|left, right| left.title.cmp(&right.title));
        Ok(summaries)
    }

    fn load(&self, id: &str) -> std::result::Result<WorkflowGraph, (StatusCode, String)> {
        self.load_optional(id)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("workflow `{id}` not found")))
    }

    fn load_optional(
        &self,
        id: &str,
    ) -> std::result::Result<Option<WorkflowGraph>, (StatusCode, String)> {
        let path = self.path_for_id(id)?;
        if !path.exists() {
            return Ok(None);
        }
        read_workflow_file(&path).map(Some)
    }

    fn delete(&self, id: &str) -> std::result::Result<(), (StatusCode, String)> {
        let path = self.path_for_id(id)?;
        if !path.exists() {
            return Err((StatusCode::NOT_FOUND, format!("workflow `{id}` not found")));
        }
        fs::remove_file(&path).map_err(internal_error)?;
        Ok(())
    }

    fn save(&self, graph: &WorkflowGraph) -> std::result::Result<(), (StatusCode, String)> {
        let path = self.path_for_id(&graph.id)?;
        fs::create_dir_all(&self.dir).map_err(internal_error)?;
        let tmp = path.with_extension("json.tmp");
        let content = serde_json::to_vec_pretty(graph).map_err(internal_error)?;
        fs::write(&tmp, content).map_err(internal_error)?;
        fs::rename(&tmp, path).map_err(internal_error)?;
        Ok(())
    }

    fn path_for_id(&self, id: &str) -> std::result::Result<PathBuf, (StatusCode, String)> {
        if !is_safe_workflow_id(id) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("invalid workflow id `{id}`"),
            ));
        }
        Ok(self.dir.join(format!("{id}.ctworkflow.json")))
    }
}

fn read_workflow_file(path: &Path) -> std::result::Result<WorkflowGraph, (StatusCode, String)> {
    let raw = fs::read(path).map_err(internal_error)?;
    let graph: WorkflowGraph = serde_json::from_slice(&raw).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("failed to parse workflow `{}`: {error}", path.display()),
        )
    })?;
    validate_workflow(&graph).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid workflow `{}`: {error}", path.display()),
        )
    })?;
    Ok(graph)
}

fn is_safe_workflow_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn next_revision() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("rev-{nanos}")
}

fn bad_request(error: String) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, error)
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn default_static_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web/dist")
}

fn default_workflow_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".workflow-web/workflows")
}

fn ensure_static_dir(static_dir: &PathBuf) -> Result<()> {
    let index = static_dir.join("index.html");
    if !index.is_file() {
        bail!(
            "frontend build not found at `{}`; run `npm install && npm run build` in crates/frontends/camera-toolbox-web/web first, or pass --static-dir",
            static_dir.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "platform-ssh")]
    use camera_toolbox_adapters::platforms::ssh_managed::{
        MemorySshTransport, TransportCommandOutput,
    };
    #[cfg(feature = "platform-ssh")]
    use camera_toolbox_app::{
        EepromHelperOutput, EepromHelperRequest, EepromInspectResult, EepromRollbackState,
        EepromWriteResult,
    };

    #[cfg(feature = "platform-ssh")]
    fn eeprom_preview_payload() -> EepromPreviewRequest {
        EepromPreviewRequest {
            node_id: "eeprom-node".to_owned(),
            profile_id: "x5-lab".to_owned(),
            bus: "i2c-7".to_owned(),
            address: 0x50,
            register: 0x0010,
            payload: vec![0x42; YG_STEREO_P24C64G_INTRINSICS_BYTES],
            page_size: 32,
            map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
            verify_after_write: true,
        }
    }

    #[cfg(feature = "platform-ssh")]
    fn eeprom_ssh_binding() -> SshExecutionBinding {
        SshExecutionBinding {
            host: "camera.local".to_owned(),
            port: 22,
            username: "root".to_owned(),
            credential_ref: "session:test".to_owned(),
        }
    }

    #[cfg(feature = "platform-ssh")]
    fn eeprom_device_state(hash: char) -> EepromDeviceState {
        EepromDeviceState {
            image_sha256: hash.to_string().repeat(64),
            flag_valid: true,
            serial: EepromSerialState::Valid {
                value: "2T02D2567K0042".to_owned(),
            },
        }
    }

    #[cfg(feature = "platform-ssh")]
    fn eeprom_output(result: EepromHelperResult) -> TransportCommandOutput {
        TransportCommandOutput {
            stdout: serde_json::to_vec(&EepromHelperOutput::Success { result }).unwrap(),
            stderr: Vec::new(),
            exit_status: Some(0),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[cfg(feature = "platform-ssh")]
    fn eeprom_state(memory: &Arc<MemorySshTransport>) -> AppState {
        memory.allow_credential("session:test");
        let resolver: Arc<dyn CredentialResolver> = memory.clone();
        let transport: Arc<dyn SshTransportFactory> = memory.clone();
        AppState {
            workflow_store: Arc::new(WorkflowStore {
                dir: std::env::temp_dir(),
            }),
            runtime_sessions: Arc::new(Mutex::new(HashMap::new())),
            control_runtime: Arc::new(ControlRuntime::with_ssh_for_test(
                resolver,
                transport,
                Arc::<[u8]>::from(b"test-helper".as_slice()),
            )),
            #[cfg(feature = "calibration-opencv")]
            calibration_backend: Arc::new(OpenCvCalibrationBackend),
            eeprom_inspects: Arc::new(Mutex::new(HashMap::new())),
            engine_runtime: Arc::new(engine_api::EngineRuntime::new()),
        }
    }

    #[test]
    fn default_static_dir_points_to_web_dist() {
        let path = default_static_dir();
        assert!(path.ends_with("web/dist"));
    }

    #[test]
    fn default_workflow_dir_points_to_crate_local_store() {
        let path = default_workflow_dir();
        assert!(path.ends_with(".workflow-web/workflows"));
    }

    #[test]
    fn mjpeg_config_rejects_non_rtsp_url() {
        let result = MjpegStreamConfig::from_query(MjpegStreamQuery {
            url: "http://camera.local/stream".to_owned(),
            fps: None,
            width: None,
            height: None,
        });
        assert!(matches!(result, Err((StatusCode::BAD_REQUEST, _))));
    }

    #[test]
    fn mjpeg_chunk_includes_runtime_metrics_headers() {
        let frame = DecodedVideoFrame {
            width: 1,
            height: 1,
            rgba: vec![16, 32, 48, 255].into(),
            identity: camera_toolbox_app::StreamFrameIdentity::unavailable(
                StreamSessionId::new("workflow-mjpeg-test").unwrap(),
                0,
                42,
                "unit test",
            ),
        };
        let stats = FfmpegRtspDecoderStatsSnapshot {
            decoded_frames: 7,
            io_bytes_available: false,
            io_bytes: 0,
            media_packet_bytes: 0,
            codec_stage_ns: 10,
            scale_stage_ns: 20,
            copy_stage_ns: 30,
        };

        let chunk = mjpeg_chunk(&frame, &stats).unwrap();
        let header_end = chunk
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("headers are terminated");
        let headers = std::str::from_utf8(&chunk[..header_end]).unwrap();

        assert!(headers.contains("Content-Type: image/jpeg"));
        assert!(headers.contains("X-Frame-Sequence: 42"));
        assert!(headers.contains("X-Mjpeg-Sent-At-Ns:"));
        assert!(headers.contains("X-Decoder-Frames: 7"));
        assert!(headers.contains("X-Decoder-Codec-Ns: 10"));
        assert!(headers.contains("X-Decoder-Scale-Ns: 20"));
        assert!(headers.contains("X-Decoder-Copy-Ns: 30"));
        assert!(headers.contains("X-Mjpeg-Encode-Ns:"));
        assert!(headers.contains("X-Mjpeg-Jpeg-Bytes:"));
    }

    #[test]
    fn mjpeg_config_preserves_source_rate_by_default() {
        let explicit_limit = MjpegStreamConfig::from_query(MjpegStreamQuery {
            url: "rtsp://camera.local/stream".to_owned(),
            fps: Some(300),
            width: Some(4096),
            height: Some(4096),
        })
        .expect("valid RTSP URL");
        assert_eq!(explicit_limit.fps_limit, Some(120));
        assert_eq!(explicit_limit.width, 1920);
        assert_eq!(explicit_limit.height, 1080);

        let source_rate = MjpegStreamConfig::from_query(MjpegStreamQuery {
            url: "rtsp://camera.local/stream".to_owned(),
            fps: None,
            width: Some(960),
            height: None,
        })
        .expect("valid RTSP URL");
        assert_eq!(source_rate.fps_limit, None);
        assert_eq!(source_rate.height, 540);
    }

    #[test]
    fn x5_binding_request_trims_host_and_preserves_port() {
        let binding = X5BindingRequest {
            host: " 10.21.12.108 ".to_owned(),
            tcp_port: 9073,
        };

        let (host, port) = x5_binding_from_request(&binding).expect("binding is valid");

        assert_eq!(host, "10.21.12.108");
        assert_eq!(port, 9073);
    }

    #[test]
    fn x5_snapshot_response_exposes_metadata_without_payload() {
        let snapshot = x5_tcp_client::X5YuvSnapshot {
            channel: 3,
            width: 1920,
            height: 1080,
            y_len: 1,
            uv_len: 1,
            frame_id: 42,
            timestamp_ns: 7_654_321,
            rtsp_timestamp_us: 123_456,
            rtsp_pts_90k: 456_789,
            match_rtsp_pts_delta_90k: Some(0),
            match_mode: Some("rtsp_pts_90k".to_owned()),
            payload: vec![0x11, 0x22],
        };

        let value = x5_snapshot_response(&snapshot);

        assert_eq!(value["channel"], 3);
        assert_eq!(value["pixelFormat"], "nv12");
        assert_eq!(value["payloadBytes"], 2);
        assert_eq!(value["matchMode"], "rtsp_pts_90k");
        assert!(value.get("payload").is_none());
    }

    #[test]
    fn workflow_store_roundtrips_normalized_graph() {
        let dir = std::env::temp_dir().join(format!("workflow-store-test-{}", next_revision()));
        let store = WorkflowStore { dir: dir.clone() };
        let mut graph = seed_workflow_graph();
        graph.id = "roundtrip".to_owned();
        graph.revision = "rev-test".to_owned();
        store.save(&graph).expect("workflow saved");

        let loaded = store.load("roundtrip").expect("workflow loaded");
        assert_eq!(loaded.id, "roundtrip");
        assert_eq!(loaded.revision, "rev-test");
        assert_eq!(loaded.nodes.len(), graph.nodes.len());
        fs::remove_dir_all(dir).ok();
    }
    #[test]
    fn i2c_write_execution_splits_payload_on_page_boundaries() {
        let preview = build_i2c_preview(I2cPreviewRequest {
            node_id: "i2c-node".to_owned(),
            profile_id: "x5-lab".to_owned(),
            bus: "i2c-6".to_owned(),
            address: 0x50,
            register: 0x000e,
            payload: vec![0xaa, 0xbb, 0xcc, 0xdd],
            page_size: 4,
            operation: I2cPreviewOperation::Write,
        })
        .expect("valid preview");

        let I2cHelperAction::Transfer { transactions } =
            i2c_action_from_preview(&preview).expect("valid action")
        else {
            panic!("expected transfer action");
        };

        assert_eq!(transactions.len(), 2);
        assert_eq!(transactions[0].bus, 6);
        assert_eq!(transactions[0].settle_ms, Some(5));
        let I2cMessageData::Write { bytes } = &transactions[0].messages[0].data else {
            panic!("expected first page write");
        };
        assert_eq!(bytes, &[0x00, 0x0e, 0xaa, 0xbb]);
        let I2cMessageData::Write { bytes } = &transactions[1].messages[0].data else {
            panic!("expected second page write");
        };
        assert_eq!(bytes, &[0x00, 0x10, 0xcc, 0xdd]);
    }

    #[cfg(feature = "calibration-opencv")]
    #[test]
    fn calibration_solver_request_is_validated_and_preserves_intrinsics() {
        let request = CalibrationSolverRequest {
            image_size: CalibrationImageSize {
                width: 1920,
                height: 1080,
            },
            board: CalibrationBoardSpec {
                inner_cols: 8,
                inner_rows: 11,
                square_size: 30.0,
            },
            image_points: vec![vec![CalibrationPoint { x: 12.5, y: 34.5 }; 88]],
            initial_intrinsics: CalibrationInitialIntrinsics {
                camera_matrix: [1234.0, 0.0, 960.0, 0.0, 1234.0, 540.0, 0.0, 0.0, 1.0],
                distortion_coefficients: vec![0.0; 12],
            },
        };

        let request = into_calibration_request(request).expect("valid calibration request");

        assert_eq!(request.image_size.width, 1920);
        assert_eq!(request.board.inner_cols, 8);
        assert_eq!(request.board.inner_rows, 11);
        assert_eq!(request.image_points.len(), 1);
        assert_eq!(request.initial_intrinsics.camera_matrix[0], 1234.0);
        assert_eq!(request.initial_intrinsics.distortion_coefficients.len(), 12);
    }

    #[test]
    fn eeprom_run_path_requires_prior_inspect_hash() {
        assert!(!is_sha256("A".repeat(64).as_str()));
        assert!(is_sha256("a".repeat(64).as_str()));
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn eeprom_update_request_reuses_inspected_serial_and_payload_gate() {
        let preview = build_eeprom_preview(eeprom_preview_payload()).expect("valid preview");
        let snapshot = EepromInspectSnapshot {
            target: eeprom_snapshot_target(&preview, &eeprom_ssh_binding()).unwrap(),
            image_sha256: "a".repeat(64),
            device: eeprom_device_state('a'),
        };

        let request = eeprom_provision_request_from_preview(&preview, &snapshot).unwrap();

        assert_eq!(request.map_id, YG_STEREO_P24C64G_V1_MAP_ID);
        assert_eq!(request.mode, EepromProvisioningMode::UpdateCalibration);
        assert_eq!(request.serial_number, "2T02D2567K0042");
        assert_eq!(request.segments.len(), 1);
        assert_eq!(request.segments[0].offset, 0x0010);
        assert_eq!(
            request.segments[0].bytes,
            vec![0x42; YG_STEREO_P24C64G_INTRINSICS_BYTES]
        );
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn eeprom_update_request_rejects_empty_inspected_serial() {
        let preview = build_eeprom_preview(eeprom_preview_payload()).expect("valid preview");
        let mut snapshot = EepromInspectSnapshot {
            target: eeprom_snapshot_target(&preview, &eeprom_ssh_binding()).unwrap(),
            image_sha256: "a".repeat(64),
            device: eeprom_device_state('a'),
        };
        snapshot.device.serial = EepromSerialState::Empty;

        let error = eeprom_provision_request_from_preview(&preview, &snapshot).unwrap_err();

        assert!(error.error.contains("existing valid serial"));
    }

    #[cfg(feature = "platform-ssh")]
    #[tokio::test]
    async fn eeprom_inspect_then_provision_uses_same_snapshot_target() {
        let memory = Arc::new(MemorySshTransport::new("host-key"));
        let state = eeprom_state(&memory);
        memory.set_command_output(eeprom_output(EepromHelperResult::Inspect(
            EepromInspectResult {
                state: eeprom_device_state('a'),
                backup: vec![0; camera_toolbox_core::YG_STEREO_P24C64G_IMAGE_BYTES],
            },
        )));

        let inspect = inspect_eeprom_provision(
            State(state.clone()),
            Ok(Json(EepromInspectRequest {
                preview: eeprom_preview_payload(),
                ssh: eeprom_ssh_binding(),
            })),
        )
        .await
        .expect("inspect succeeds");
        assert_eq!(inspect.snapshot.image_sha256, "a".repeat(64));

        memory.set_command_output(eeprom_output(EepromHelperResult::Provision(
            EepromWriteResult {
                before: eeprom_device_state('a'),
                after: eeprom_device_state('b'),
                backup: vec![0; camera_toolbox_core::YG_STEREO_P24C64G_IMAGE_BYTES],
                page_plan: Vec::new(),
                bytewise_verified: true,
                rollback: EepromRollbackState::NotRequired,
            },
        )));
        let result = run_eeprom_provision(
            State(state),
            Ok(Json(EepromExecuteRequest {
                preview: eeprom_preview_payload(),
                confirm_execution: true,
                expected_before_sha256: Some("a".repeat(64)),
                ssh: eeprom_ssh_binding(),
            })),
        )
        .await
        .expect("provision succeeds");

        assert_eq!(result.execution, "completed");
        assert!(matches!(
            result.result,
            ControlExecutionPayload::Eeprom(EepromHelperResult::Provision(_))
        ));
        let requests = memory.captured_stdin();
        assert_eq!(requests.len(), 2);
        let provision_request: EepromHelperRequest = serde_json::from_slice(&requests[1]).unwrap();
        let EepromHelperAction::Provision {
            request,
            expected_before_sha256,
        } = provision_request.action
        else {
            panic!("expected provision action");
        };
        assert_eq!(expected_before_sha256, "a".repeat(64));
        assert_eq!(request.serial_number, "2T02D2567K0042");
        assert_eq!(request.segments[0].offset, 0x0010);
    }

    #[test]
    fn workflow_store_delete_removes_saved_graph() {
        let dir =
            std::env::temp_dir().join(format!("workflow-store-delete-test-{}", next_revision()));
        let store = WorkflowStore { dir: dir.clone() };
        let mut graph = seed_workflow_graph();
        graph.id = "delete-me".to_owned();
        graph.revision = "rev-test".to_owned();
        store.save(&graph).expect("workflow saved");

        store.delete("delete-me").expect("workflow deleted");
        let error = store.load("delete-me").expect_err("workflow removed");
        assert_eq!(error.0, StatusCode::NOT_FOUND);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn unsafe_workflow_ids_are_rejected() {
        assert!(!is_safe_workflow_id("../escape"));
        assert!(!is_safe_workflow_id(""));
        assert!(is_safe_workflow_id("camera_toolbox-1"));
    }
}
