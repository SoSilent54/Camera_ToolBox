mod engine_api;
mod engine_bridge;
mod files_api;
mod workflow;
mod ws_hub;
mod ws_router;

use std::{
    collections::HashMap,
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::State,
    extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
#[cfg(feature = "calibration-opencv")]
use camera_toolbox_adapters::OpenCvCalibrationBackend;
use camera_toolbox_adapters::platforms::ssh_managed::{
    CredentialResolver, ProductionCredentialResolver, RusshTransportFactory, SshCommandService,
    SshConnectionTarget, SshEepromProvisionService, SshI2cHelperService, SshTransportFactory,
    production_recipe_registry_from_env,
};
use camera_toolbox_adapters::x5_tcp_client;
use camera_toolbox_app::engine::{CaptureMode, CaptureTarget};
use camera_toolbox_app::{
    CalibrationBackend, CalibrationCancellation, CommandResult, CommandService, ControlTargetSpec,
    DecodedVideoFrame, DumpCancellation, EepromDeviceState, EepromExecutor, EepromHelperAction,
    EepromHelperResult, EepromProvisionOperation, EepromProvisionService,
    EepromProvisionServiceError, EepromSerialState, I2cExecutor, I2cHelperAction,
    I2cHelperOperation, I2cHelperResult, I2cHelperService, I2cMessageData, I2cMessageSpec,
    I2cTransactionSpec, RemoteFileStat, RemoteOperationControl, RemoteTimeouts, SftpFileReader,
    SshCommandExecutor, TypedCommandRequest, X5ControlClient, X5233CapturePayload,
    validate_i2c_transfer_transactions,
};
use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationRequest, CalibrationSolution,
    EepromProvisionRequest, EepromProvisioningMode, EepromWriteSegment, InitialIntrinsics,
    ViewCalibrationResult, YG_STEREO_P24C64G_INTRINSICS_BYTES, YG_STEREO_P24C64G_V1_MAP_ID,
};
use clap::Parser;
use image::{ColorType, codecs::jpeg::JpegEncoder};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};
use workflow::{WorkflowGraph, validate_workflow};

#[derive(Debug, Parser)]
#[command(name = "camera-toolbox-web")]
#[command(about = "Camera Toolbox browser workflow canvas server")]
struct ServerArgs {
    /// Web 服务绑定地址；默认只监听本机回环地址，避免局域网设备未经认证直连控制端点。
    /// 需要让局域网设备访问时，显式传入 `--host 0.0.0.0` 并自行加认证/防火墙。
    #[arg(long, default_value = "127.0.0.1")]
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
    control_runtime: Arc<ControlRuntime>,
    #[cfg(feature = "calibration-opencv")]
    calibration_backend: Arc<dyn CalibrationBackend>,
    eeprom_inspects: Arc<Mutex<HashMap<String, EepromInspectSnapshot>>>,
    engine_runtime: Arc<engine_api::EngineRuntime>,
    ws_hub: Arc<ws_hub::WsHub>,
    /// 后端权威工作流图；前端只渲染该状态的 snapshot，不维护独立 draft graph。
    graph_session: Arc<Mutex<WorkflowGraph>>,
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
    /// 密码注册接口仅写入此进程内 resolver；工作流只会持久化其 session 引用。
    #[cfg(feature = "platform-ssh")]
    password_credential_resolver: Arc<ProductionCredentialResolver>,
    #[cfg(feature = "platform-ssh")]
    ssh_transport: Arc<dyn SshTransportFactory>,
    #[cfg(feature = "platform-ssh")]
    helper_payload: Option<Arc<[u8]>>,
}

impl ControlRuntime {
    fn production() -> Self {
        #[cfg(feature = "platform-ssh")]
        let password_credential_resolver = Arc::new(ProductionCredentialResolver::new());
        Self {
            #[cfg(feature = "platform-ssh")]
            credential_resolver: password_credential_resolver.clone(),
            #[cfg(feature = "platform-ssh")]
            password_credential_resolver,
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
            password_credential_resolver: Arc::new(ProductionCredentialResolver::new()),
            ssh_transport,
            helper_payload: Some(helper_payload),
        }
    }
}

#[cfg(feature = "platform-ssh")]
impl ControlRuntime {
    /// 将一次 UI 密码替换为指定节点的进程内 session 引用；密码绝不进入工作流或响应体。
    fn register_password(
        &self,
        node_id: &str,
        password: String,
    ) -> std::result::Result<String, PreviewApiError> {
        if node_id.is_empty()
            || node_id.len() > 128
            || !node_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(PreviewApiError::bad_request(
                "nodeId must contain only ASCII letters, digits, '-' or '_'",
            ));
        }
        if password.is_empty() {
            return Err(PreviewApiError::bad_request("password must not be empty"));
        }
        self.password_credential_resolver
            .register_session_password(node_id, SecretString::from(password))
            .map_err(PreviewApiError::bad_request)
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

#[cfg(feature = "platform-ssh")]
fn ssh_target_from_spec(spec: &ControlTargetSpec) -> SshConnectionTarget {
    SshConnectionTarget {
        host: spec.host.clone(),
        port: spec.port,
        username: spec.username.clone(),
        expected_host_key: spec.expected_host_key.clone(),
        command_subsystem: None,
        remote_event_subsystem: None,
    }
}

/// 把 I²C executor 抽象接到真实的 SSH helper：每次操作按当前 spec 构造 service 并执行。
#[cfg(feature = "platform-ssh")]
impl I2cExecutor for ControlRuntime {
    fn execute(
        &self,
        target: &ControlTargetSpec,
        credential_ref: &str,
        action: I2cHelperAction,
        control: RemoteOperationControl,
    ) -> std::result::Result<I2cHelperResult, String> {
        let credential_ref = validate_credential_ref(credential_ref).map_err(|e| e.error)?;
        let service = SshI2cHelperService::new(
            format!("workflow-web-i2c-{}", target.host),
            ssh_target_from_spec(target),
            credential_ref,
            1_048_576,
            self.helper_payload().map_err(|e| e.error)?,
            Arc::clone(&self.credential_resolver),
            Arc::clone(&self.ssh_transport),
        )
        .map_err(|error| format_i2c_service_error(&error))?;
        service
            .execute(I2cHelperOperation { action }, control)
            .map_err(|error| format_i2c_service_error(&error))
    }
}

/// 把 EEPROM executor 抽象接到真实的 SSH helper。
#[cfg(feature = "platform-ssh")]
impl EepromExecutor for ControlRuntime {
    fn execute(
        &self,
        target: &ControlTargetSpec,
        credential_ref: &str,
        action: EepromHelperAction,
        control: RemoteOperationControl,
    ) -> std::result::Result<EepromHelperResult, String> {
        let credential_ref = validate_credential_ref(credential_ref).map_err(|e| e.error)?;
        let service = SshEepromProvisionService::new(
            format!("workflow-web-eeprom-{}", target.host),
            ssh_target_from_spec(target),
            credential_ref,
            1_048_576,
            // `i2c_bus` 不在 `EepromExecutor` trait 契约内（`ControlTargetSpec` 只承载
            // host/port/username/expected_host_key），此处固定为 0；真实 bus pinning 需
            // 后续把 bus 下钻进 spec 或 trait 后补上。map_id 已由 service 内部固定。
            0,
            self.helper_payload().map_err(|e| e.error)?,
            Arc::clone(&self.credential_resolver),
            Arc::clone(&self.ssh_transport),
        )
        .map_err(|error| format_eeprom_service_error(&error))?;
        service
            .execute(EepromProvisionOperation { action }, control)
            .map_err(|error| format_eeprom_service_error(&error))
    }
}

/// 把 X5 控制客户端抽象接到真实的 X5_233 TCP 模块（纯 TCP）。
impl X5ControlClient for ControlRuntime {
    fn probe(&self, host: &str, port: u16) -> std::result::Result<serde_json::Value, String> {
        x5_tcp_client::probe(host, port)
            .map(|summary| x5_probe_response(&summary))
            .map_err(|error| error)
    }

    fn status(&self, host: &str, port: u16) -> std::result::Result<serde_json::Value, String> {
        x5_tcp_client::status(host, port)
            .map(|status| x5_status_response(&status))
            .map_err(|error| error)
    }

    fn capture(
        &self,
        host: &str,
        port: u16,
        request: &camera_toolbox_app::engine::CaptureRequest,
    ) -> std::result::Result<X5233CapturePayload, String> {
        match (request.target, request.mode) {
            (CaptureTarget::Yuv { channel }, CaptureMode::Latest) => {
                x5_tcp_client::capture_yuv_snapshot(host, port, channel)
            }
            (CaptureTarget::Yuv { channel }, CaptureMode::FrameId(frame_id)) => {
                x5_tcp_client::capture_yuv_snapshot_by_frame_id(host, port, channel, frame_id)
            }
            (CaptureTarget::Yuv { channel }, CaptureMode::TimestampNs(timestamp_ns)) => {
                x5_tcp_client::capture_yuv_snapshot_by_timestamp_ns(
                    host,
                    port,
                    channel,
                    timestamp_ns,
                )
            }
            (CaptureTarget::Raw { camera }, CaptureMode::Latest) => {
                let snapshot = x5_tcp_client::capture_raw_snapshot(host, port, camera, 3_000)?;
                return Ok(X5233CapturePayload::BayerRaw {
                    camera: snapshot.camera,
                    width: snapshot.width,
                    height: snapshot.height,
                    stride_bytes: u32::try_from(snapshot.stride)
                        .map_err(|_| "X5 RAW stride does not fit u32".to_owned())?,
                    format_code: snapshot.format_code,
                    frame_id: snapshot.frame_id,
                    timestamp_ns: snapshot.timestamp_ns,
                    payload: Arc::from(snapshot.payload),
                });
            }
            (CaptureTarget::Raw { .. }, _) => {
                return Err(
                    "X5_233 RAW capture only supports mode=latest; no RAW frame ring is available"
                        .to_owned(),
                );
            }
        }
        .map(|snapshot| X5233CapturePayload::Nv12 {
            channel: snapshot.channel,
            width: snapshot.width,
            height: snapshot.height,
            y_len: snapshot.y_len,
            uv_len: snapshot.uv_len,
            frame_id: snapshot.frame_id,
            timestamp_ns: snapshot.timestamp_ns,
            payload: Arc::from(snapshot.payload),
        })
    }
}

/// 把 SFTP 文件读取抽象接到真实 SSH transport：每次操作按 spec 建立 session 后 stat/read。
#[cfg(feature = "platform-ssh")]
impl SftpFileReader for ControlRuntime {
    fn stat(
        &self,
        target: &ControlTargetSpec,
        credential_ref: &str,
        remote_path: &str,
        control: RemoteOperationControl,
    ) -> Result<RemoteFileStat, String> {
        let credential_ref = validate_credential_ref(credential_ref).map_err(|e| e.error)?;
        let credential = self
            .credential_resolver
            .resolve(&credential_ref)
            .map_err(|e| e)?;
        let mut session = self
            .ssh_transport
            .connect(&ssh_target_from_spec(target), credential, &control)
            .map_err(|e| e.to_string())?;
        session
            .stat(remote_path, &control)
            .map_err(|e| e.to_string())
    }

    fn read(
        &self,
        target: &ControlTargetSpec,
        credential_ref: &str,
        remote_path: &str,
        limit: usize,
        control: RemoteOperationControl,
    ) -> Result<Vec<u8>, String> {
        let credential_ref = validate_credential_ref(credential_ref).map_err(|e| e.error)?;
        let credential = self
            .credential_resolver
            .resolve(&credential_ref)
            .map_err(|e| e)?;
        let mut session = self
            .ssh_transport
            .connect(&ssh_target_from_spec(target), credential, &control)
            .map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        let mut total = 0_usize;
        session
            .read_file(remote_path, &control, &mut |chunk| {
                total = total.saturating_add(chunk.len());
                if total > limit {
                    return Err(camera_toolbox_adapters::platforms::ssh_managed::SshTransportError::ReadLimitExceeded {
                        requested: total as u64,
                        limit: limit as u64,
                    });
                }
                bytes.extend_from_slice(chunk);
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    }
}

/// 把 SSH 命令执行抽象接到真实 SSH command service（allowlisted recipe）。
#[cfg(feature = "platform-ssh")]
impl SshCommandExecutor for ControlRuntime {
    fn execute(
        &self,
        target: &ControlTargetSpec,
        credential_ref: &str,
        request: TypedCommandRequest,
        control: RemoteOperationControl,
    ) -> Result<CommandResult, String> {
        let credential_ref = validate_credential_ref(credential_ref).map_err(|e| e.error)?;
        // recipe 注册表从环境变量加载；无部署 recipe 时退化为空注册表（execute 会报 RecipeNotAllowed）。
        let recipes =
            std::sync::Arc::new(production_recipe_registry_from_env().unwrap_or_else(|_| {
                camera_toolbox_adapters::platforms::ssh_managed::CommandRecipeRegistry::new()
            }));
        let allowed_recipe_id = request.recipe_id.clone();
        let service = SshCommandService::new(
            format!("workflow-web-ssh-{}", target.host),
            ssh_target_from_spec(target),
            credential_ref,
            allowed_recipe_id,
            Arc::clone(&self.credential_resolver),
            Arc::clone(&self.ssh_transport),
            recipes,
            1_048_576,
        );
        service.execute(request, control).map_err(|e| e.to_string())
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
            "ssh.credentialRef must be a non-empty credential reference",
        ));
    }
    if !reference.starts_with("session:") || reference.len() == "session:".len() {
        return Err(PreviewApiError::bad_request(
            "ssh.credentialRef must use session:<node-id>",
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
struct SshPasswordRegistrationRequest {
    node_id: String,
    password: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SshPasswordRegistrationResponse {
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
        control_runtime: Arc::new(ControlRuntime::production()),
        #[cfg(feature = "calibration-opencv")]
        calibration_backend: Arc::new(OpenCvCalibrationBackend),
        eeprom_inspects: Arc::new(Mutex::new(HashMap::new())),
        engine_runtime: Arc::new(engine_api::EngineRuntime::new()),
        ws_hub: Arc::new(ws_hub::WsHub::new()),
        graph_session: Arc::new(Mutex::new(workflow::seed_workflow_graph())),
    };

    // 启动引擎桥接任务：状态/事件/帧持续经 ws_hub 广播（三个 tokio::spawn 循环）。
    engine_bridge::spawn(state.clone(), Arc::clone(&state.ws_hub));

    Router::new()
        .route("/api/health", get(health))
        .route("/api/ws", get(ws_upgrade))
        .fallback_service(frontend)
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        service: "camera-toolbox-web",
        status: "ok",
    })
}

/// WS 握手端点：升级连接后注册进 [`ws_hub::WsHub`]，并进入接收循环保持连接存活。
///
/// 本阶段（P0）只做回显/忽略——收到的文本消息原样回显，收到的其它帧类型直接忽略；
/// 真正按信封 `path` 分发到命令处理器由 t5 的 `ws_router` 接管。连接存活期间，
/// 出站通道注册进 hub 以便引擎桥接后续广播（状态/事件/帧）。
async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_ws_socket(socket, state))
}

/// `control.*` 命令处理器（WS 路径分发复用）：把 payload 反序列化为既有请求结构体，
/// 调用与本 crate HTTP 端点完全相同的辅助函数/执行逻辑，返回 `Result<Value, String>`。
///
/// 结构体/辅助函数均私有于 main.rs，故本分发器也置于此模块（ws_router 委托进来），
/// 避免把十余个控制请求类型搬出做 `pub(crate)`。
pub(crate) fn control_dispatch(
    path: &str,
    payload: serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value, String> {
    match path {
        "control.ssh.password" => {
            let request: SshPasswordRegistrationRequest =
                serde_json::from_value(payload).map_err(ws_deser_err)?;
            #[cfg(feature = "platform-ssh")]
            {
                let credential_ref = state
                    .control_runtime
                    .register_password(&request.node_id, request.password)
                    .map_err(|error| error.error)?;
                return serde_json::to_value(SshPasswordRegistrationResponse { credential_ref })
                    .map_err(ws_ser_err);
            }
            #[cfg(not(feature = "platform-ssh"))]
            {
                let _ = request;
                return Err("workflow-web was built without platform-ssh support".to_owned());
            }
        }
        "control.i2c.preview" => {
            let req: I2cPreviewRequest = serde_json::from_value(payload).map_err(ws_deser_err)?;
            let preview = build_i2c_preview(req).map_err(|e| e.error)?;
            serde_json::to_value(preview).map_err(ws_ser_err)
        }
        "control.i2c.run" => {
            let req: I2cExecuteRequest = serde_json::from_value(payload).map_err(ws_deser_err)?;
            let preview = build_i2c_preview(req.preview).map_err(|e| e.error)?;
            if preview.requires_confirmation && !req.confirm_execution {
                return Err("I²C write execution requires confirmExecution=true".to_owned());
            }
            let result = state
                .control_runtime
                .execute_i2c(&preview, &req.ssh)
                .map_err(|e| e.error)?;
            serde_json::to_value(ControlExecutionResult {
                preview,
                execution: "completed",
                result: ControlExecutionPayload::I2c(result),
            })
            .map_err(ws_ser_err)
        }
        "control.eeprom.preview" => {
            let req: EepromPreviewRequest =
                serde_json::from_value(payload).map_err(ws_deser_err)?;
            let preview = build_eeprom_preview(req).map_err(|e| e.error)?;
            serde_json::to_value(preview).map_err(ws_ser_err)
        }
        "control.eeprom.inspect" => {
            let req: EepromInspectRequest =
                serde_json::from_value(payload).map_err(ws_deser_err)?;
            let preview = build_eeprom_preview(req.preview).map_err(|e| e.error)?;
            let target = eeprom_snapshot_target(&preview, &req.ssh).map_err(|e| e.error)?;
            let result = state
                .control_runtime
                .execute_eeprom(&preview, &req.ssh, EepromHelperAction::Inspect)
                .map_err(|e| e.error)?;
            let EepromHelperResult::Inspect(inspect) = &result else {
                return Err("EEPROM inspect returned a non-inspect helper result".to_owned());
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
                .map_err(|_| "EEPROM inspect snapshot state is unavailable".to_owned())?
                .insert(key, snapshot);
            serde_json::to_value(response).map_err(ws_ser_err)
        }
        "control.eeprom.run" => {
            let req: EepromExecuteRequest =
                serde_json::from_value(payload).map_err(ws_deser_err)?;
            let preview = build_eeprom_preview(req.preview).map_err(|e| e.error)?;
            if !req.confirm_execution {
                return Err("EEPROM provision requires confirmExecution=true".to_owned());
            }
            let Some(expected_before_sha256) = req.expected_before_sha256.as_deref() else {
                return Err(
                    "EEPROM provision requires expectedBeforeSha256 from the latest inspect"
                        .to_owned(),
                );
            };
            if !is_sha256(expected_before_sha256) {
                return Err(
                    "expectedBeforeSha256 must contain 64 lowercase hex characters".to_owned(),
                );
            }
            let target = eeprom_snapshot_target(&preview, &req.ssh).map_err(|e| e.error)?;
            let snapshot = state
                .eeprom_inspects
                .lock()
                .map_err(|_| "EEPROM inspect snapshot state is unavailable".to_owned())?
                .get(&preview.target.node_id)
                .cloned()
                .ok_or_else(|| {
                    "EEPROM provision requires a process-local inspect snapshot for this node"
                        .to_owned()
                })?;
            if snapshot.target != target {
                return Err(
                    "EEPROM target changed after inspect; inspect the selected SSH/bus/map/address again"
                        .to_owned(),
                );
            }
            if snapshot.image_sha256 != expected_before_sha256 {
                return Err(
                    "expectedBeforeSha256 does not match the latest process-local inspect snapshot"
                        .to_owned(),
                );
            }
            let provision_request =
                eeprom_provision_request_from_preview(&preview, &snapshot).map_err(|e| e.error)?;
            let result = state
                .control_runtime
                .execute_eeprom(
                    &preview,
                    &req.ssh,
                    EepromHelperAction::Provision {
                        request: provision_request,
                        expected_before_sha256: expected_before_sha256.to_owned(),
                    },
                )
                .map_err(|e| e.error)?;
            state
                .eeprom_inspects
                .lock()
                .map_err(|_| "EEPROM inspect snapshot state is unavailable".to_owned())?
                .remove(&preview.target.node_id);
            serde_json::to_value(ControlExecutionResult {
                preview,
                execution: "completed",
                result: ControlExecutionPayload::Eeprom(result),
            })
            .map_err(ws_ser_err)
        }
        #[cfg(feature = "calibration-opencv")]
        "control.calibration.solver.run" => {
            let req: CalibrationSolverRequest =
                serde_json::from_value(payload).map_err(ws_deser_err)?;
            let request = into_calibration_request(req).map_err(|e| e.error)?;
            let cancellation = CalibrationCancellation::default();
            let solution = state
                .calibration_backend
                .calibrate(&request, &cancellation)
                .map_err(|e| e.to_string())?;
            serde_json::to_value(CalibrationSolverResponse::from(solution)).map_err(ws_ser_err)
        }
        #[cfg(not(feature = "calibration-opencv"))]
        "control.calibration.solver.run" => {
            Err("calibration solver not available in this build".to_owned())
        }
        "control.x5.probe" => {
            let req: X5ProbeRequest = serde_json::from_value(payload).map_err(ws_deser_err)?;
            let (host, port) = x5_binding_from_request(&req.binding).map_err(|e| e.error)?;
            let summary = x5_tcp_client::probe(&host, port).map_err(|e| e.to_string())?;
            Ok(x5_probe_response(&summary))
        }
        "control.x5.status" => {
            let req: X5StatusRequest = serde_json::from_value(payload).map_err(ws_deser_err)?;
            let (host, port) = x5_binding_from_request(&req.binding).map_err(|e| e.error)?;
            let status = x5_tcp_client::status(&host, port).map_err(|e| e.to_string())?;
            Ok(x5_status_response(&status))
        }
        "control.x5.configure-rtsp" => {
            let req: X5ConfigureRequest = serde_json::from_value(payload).map_err(ws_deser_err)?;
            let (host, port) = x5_binding_from_request(&req.binding).map_err(|e| e.error)?;
            let summary = x5_tcp_client::configure_rtsp(
                &host,
                port,
                x5_tcp_client::X5RtspEncoderConfig {
                    fps: req.fps,
                    bitrate_kbps: req.bitrate_kbps,
                },
            )
            .map_err(|e| e.to_string())?;
            Ok(x5_rtsp_apply_response(&summary))
        }
        "control.x5.start-rtsp" => {
            let req: X5ChannelRequest = serde_json::from_value(payload).map_err(ws_deser_err)?;
            let (host, port) = x5_binding_from_request(&req.binding).map_err(|e| e.error)?;
            let summary = x5_tcp_client::start_rtsp_channel(&host, port, req.channel)
                .map_err(|e| e.to_string())?;
            Ok(x5_rtsp_stream_response(&summary))
        }
        "control.x5.stop-rtsp" => {
            let req: X5ChannelRequest = serde_json::from_value(payload).map_err(ws_deser_err)?;
            let (host, port) = x5_binding_from_request(&req.binding).map_err(|e| e.error)?;
            let summary = x5_tcp_client::stop_rtsp_channel(&host, port, req.channel)
                .map_err(|e| e.to_string())?;
            Ok(x5_rtsp_stream_response(&summary))
        }
        "control.x5.snapshot" => {
            let req: X5SnapshotRequest = serde_json::from_value(payload).map_err(ws_deser_err)?;
            let (host, port) = x5_binding_from_request(&req.binding).map_err(|e| e.error)?;
            let snapshot = match req.mode {
                X5SnapshotMode::Latest => {
                    x5_tcp_client::capture_yuv_snapshot(&host, port, req.channel)
                }
                X5SnapshotMode::FrameId => {
                    let frame_id = req
                        .frame_id
                        .ok_or_else(|| "X5 frame_id snapshot requires frameId".to_owned())?;
                    x5_tcp_client::capture_yuv_snapshot_by_frame_id(
                        &host,
                        port,
                        req.channel,
                        frame_id,
                    )
                }
                X5SnapshotMode::TimestampNs => {
                    let timestamp_ns = req
                        .timestamp_ns
                        .ok_or_else(|| "X5 timestamp snapshot requires timestampNs".to_owned())?;
                    x5_tcp_client::capture_yuv_snapshot_by_timestamp_ns(
                        &host,
                        port,
                        req.channel,
                        timestamp_ns,
                    )
                }
            }
            .map_err(|e| e.to_string())?;
            Ok(x5_snapshot_response(&snapshot))
        }
        other => Err(format!("unknown ws path `{other}`")),
    }
}

fn ws_deser_err(error: impl std::fmt::Display) -> String {
    format!("invalid payload: {error}")
}

fn ws_ser_err(error: impl std::fmt::Display) -> String {
    format!("serialize response failed: {error}")
}

/// 单连接的生命周期：分出收发两半，出站半由独立转发任务绑定到 hub 注册的 mpsc 通道，
/// 入站半循环读取 request 信封 → ws_router 分发 → response 信封写回本连接；循环退出时注销。
async fn handle_ws_socket(socket: WebSocket, state: AppState) {
    let hub = Arc::clone(&state.ws_hub);
    let (mut sender, mut receiver) = socket.split();

    // 出站：hub 广播 + 单连接 response 都写入 mpsc 无界通道，转发任务把消息送到 socket。
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WsMessage>();
    let key = hub.register(tx.clone());
    if let Ok(snapshot) = ws_router::snapshot_envelope_text(&state) {
        let _ = tx.send(WsMessage::Text(snapshot.into()));
    }
    let forward = tokio::spawn(async move {
        use futures_util::SinkExt;
        while let Some(msg) = rx.recv().await {
            if sender.send(msg).await.is_err() {
                // socket 已关闭，停止转发。
                break;
            }
        }
    });

    // 入站：request 信封 `{id, kind:"request", path, payload}` → ws_router.dispatch → response 信封。
    use futures_util::StreamExt;
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            WsMessage::Text(text) => {
                let id = match parse_request_envelope(&text) {
                    Some(envelope) => envelope,
                    None => continue,
                };
                // 同步执行命令处理器；控制类命令（SSH/I2C/X5）本身就阻塞，与 HTTP 端点一致。
                let response = match ws_router::dispatch(&id.path, id.payload, &state) {
                    Ok(payload) => serde_json::json!({
                        "id": id.id,
                        "kind": "response",
                        "ok": true,
                        "payload": payload,
                    }),
                    Err(error) => serde_json::json!({
                        "id": id.id,
                        "kind": "response",
                        "ok": false,
                        "error": error,
                    }),
                };
                if tx
                    .send(WsMessage::Text(response.to_string().into()))
                    .is_err()
                {
                    break; // 连接已关闭。
                }
            }
            WsMessage::Close(_) => break,
            _ => { /* 二进制/乒乓等帧：忽略（帧推送为出站方向） */ }
        }
    }

    // 连接结束：摘除出站通道，并收尾转发任务。
    hub.unregister(key);
    forward.abort();
}

/// 解析一条入站文本为 request 信封；非 request（如回显/探测）或缺字段则返回 `None`。
struct RequestEnvelope {
    id: u64,
    path: String,
    payload: serde_json::Value,
}

fn parse_request_envelope(text: &str) -> Option<RequestEnvelope> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("kind")?.as_str()? != "request" {
        return None;
    }
    let id = value.get("id")?.as_u64()?;
    let path = value.get("path")?.as_str()?.to_owned();
    let payload = value
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Some(RequestEnvelope { id, path, payload })
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

/// 编码一次 RGB 图像为 JPEG（engine_bridge viewer 帧推送复用）。
fn encode_rgb_jpeg(rgb: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>, String> {
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, quality)
        .encode(rgb, width, height, ColorType::Rgb8.into())
        .map_err(|error| format!("JPEG encode failed: {error}"))?;
    Ok(jpeg)
}

/// 按目标最大宽度等比降采样后编码为 JPEG（宽高坍缩到 `max_width` 内）；返回 JPEG 与实际尺寸。
///
/// P2 的 viewer 帧推送用：1080p 全帧编码 200-500ms 不可接受，`viewer_encode_width=960` 降分辨率
/// 后编码耗时显著下降且带宽可控。`max_width >= frame.width` 时不缩放，直接编码原尺寸。
pub(crate) fn encode_rgba_scaled_jpeg(
    frame: &DecodedVideoFrame,
    max_width: u32,
) -> Result<EncodedFrame, String> {
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

    // 目标尺寸：宽度不超过 max_width，等比缩放，不小于 1。
    let (out_w, out_h) = if frame.width > max_width && max_width > 0 {
        let ratio = f64::from(max_width) / f64::from(frame.width);
        let h = (f64::from(frame.height) * ratio).round().max(1.0) as u32;
        (max_width, h)
    } else {
        (frame.width, frame.height)
    };

    let rgb: Vec<u8> = if (out_w, out_h) == (frame.width, frame.height) {
        let rgb_len = usize::try_from(pixel_count.saturating_mul(3))
            .map_err(|_| "RGB byte length overflows usize".to_owned())?;
        let mut rgb = Vec::with_capacity(rgb_len);
        for pixel in frame.rgba.chunks_exact(4) {
            rgb.extend_from_slice(&pixel[..3]);
        }
        rgb
    } else {
        let img = image::ImageBuffer::<image::Rgba<u8>, &[u8]>::from_raw(
            frame.width,
            frame.height,
            frame.rgba.as_ref(),
        )
        .ok_or_else(|| "RGBA frame buffer invalid".to_owned())?;
        let resized =
            image::imageops::resize(&img, out_w, out_h, image::imageops::FilterType::Triangle);
        let flat = resized.into_raw();
        let rgb_len = usize::try_from(u64::from(out_w) * u64::from(out_h) * 3)
            .map_err(|_| "RGB byte length overflows usize".to_owned())?;
        let mut rgb = Vec::with_capacity(rgb_len);
        for pixel in flat.chunks_exact(4) {
            rgb.extend_from_slice(&pixel[..3]);
        }
        rgb
    };

    let jpeg = encode_rgb_jpeg(&rgb, out_w, out_h, 82)?;
    Ok(EncodedFrame {
        jpeg,
        width: out_w,
        height: out_h,
    })
}

/// 一帧的编码产物：JPEG 字节 + 实际（可能已降采样）宽高，供 frame_meta 头回填尺寸。
pub(crate) struct EncodedFrame {
    pub(crate) jpeg: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
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

pub(crate) fn next_revision() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("rev-{nanos}")
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
            credential_ref: "session:test-key".to_owned(),
        }
    }

    #[cfg(feature = "platform-ssh")]
    fn eeprom_ssh_payload() -> serde_json::Value {
        serde_json::json!({
            "host": "camera.local",
            "port": 22,
            "username": "root",
            "credentialRef": "session:test-key",
        })
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
        memory.allow_credential("session:test-key");
        let resolver: Arc<dyn CredentialResolver> = memory.clone();
        let transport: Arc<dyn SshTransportFactory> = memory.clone();
        AppState {
            workflow_store: Arc::new(WorkflowStore {
                dir: std::env::temp_dir(),
            }),
            control_runtime: Arc::new(ControlRuntime::with_ssh_for_test(
                resolver,
                transport,
                Arc::<[u8]>::from(b"test-helper".as_slice()),
            )),
            #[cfg(feature = "calibration-opencv")]
            calibration_backend: Arc::new(OpenCvCalibrationBackend),
            eeprom_inspects: Arc::new(Mutex::new(HashMap::new())),
            engine_runtime: Arc::new(engine_api::EngineRuntime::new()),
            ws_hub: Arc::new(ws_hub::WsHub::new()),
            graph_session: Arc::new(Mutex::new(workflow::seed_workflow_graph())),
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
    fn x5_snapshot_response_exposes_exact_frame_identity_only() {
        let snapshot = x5_tcp_client::X5YuvSnapshot {
            channel: 3,
            width: 1920,
            height: 1080,
            y_len: 2,
            uv_len: 0,
            frame_id: 77,
            timestamp_ns: 7_654_321,
            payload: vec![0x11, 0x22],
        };

        let value = x5_snapshot_response(&snapshot);

        assert_eq!(value["channel"], 3);
        assert_eq!(value["frameId"], 77);
        assert_eq!(value["timestampNs"], 7_654_321);
        assert_eq!(value["payloadBytes"], 2);
        assert!(value.get("payload").is_none());
    }

    #[test]
    fn workflow_store_roundtrips_normalized_graph() {
        let dir = std::env::temp_dir().join(format!("workflow-store-test-{}", next_revision()));
        let store = WorkflowStore { dir: dir.clone() };
        let mut graph = crate::workflow::seed_workflow_graph();
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
    #[test]
    fn eeprom_inspect_then_provision_uses_same_snapshot_target() {
        let memory = Arc::new(MemorySshTransport::new("host-key"));
        let state = eeprom_state(&memory);
        memory.set_command_output(eeprom_output(EepromHelperResult::Inspect(
            EepromInspectResult {
                state: eeprom_device_state('a'),
                backup: vec![0; camera_toolbox_core::YG_STEREO_P24C64G_IMAGE_BYTES],
            },
        )));

        let inspect = control_dispatch(
            "control.eeprom.inspect",
            serde_json::json!({
                "nodeId": "eeprom-node",
                "profileId": "x5-lab",
                "bus": "i2c-7",
                "address": 0x50,
                "register": 0x0010,
                "payload": vec![0x42; YG_STEREO_P24C64G_INTRINSICS_BYTES],
                "pageSize": 32,
                "mapId": YG_STEREO_P24C64G_V1_MAP_ID,
                "verifyAfterWrite": true,
                "ssh": eeprom_ssh_payload(),
            }),
            &state,
        )
        .expect("inspect succeeds");
        assert_eq!(inspect["snapshot"]["imageSha256"], "a".repeat(64));

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
        let result = control_dispatch(
            "control.eeprom.run",
            serde_json::json!({
                "nodeId": "eeprom-node",
                "profileId": "x5-lab",
                "bus": "i2c-7",
                "address": 0x50,
                "register": 0x0010,
                "payload": vec![0x42; YG_STEREO_P24C64G_INTRINSICS_BYTES],
                "pageSize": 32,
                "mapId": YG_STEREO_P24C64G_V1_MAP_ID,
                "verifyAfterWrite": true,
                "confirmExecution": true,
                "expectedBeforeSha256": "a".repeat(64),
                "ssh": eeprom_ssh_payload(),
            }),
            &state,
        )
        .expect("provision succeeds");

        assert_eq!(result["execution"], "completed");
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
        let mut graph = crate::workflow::seed_workflow_graph();
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

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn ssh_target_uses_password_session_without_host_key_pin() {
        let binding = SshExecutionBinding {
            host: "camera.local".to_owned(),
            port: 22,
            username: "root".to_owned(),
            credential_ref: "session:test".to_owned(),
        };
        let target = ssh_target_from_binding(&binding).expect("password SSH target accepted");
        assert_eq!(target.host, "camera.local");
        assert_eq!(target.port, 22);
        assert_eq!(target.username, "root");
        assert!(target.expected_host_key.is_none());
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn credential_ref_accepts_only_password_session_reference() {
        assert!(validate_credential_ref("session:test").is_ok());
        assert!(validate_credential_ref("password:secret").is_err());
        assert!(validate_credential_ref("key-file:/tmp/id_ed25519").is_err());
        assert!(validate_credential_ref("session:").is_err());
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn password_registration_returns_process_local_session_reference() {
        let runtime = ControlRuntime::production();
        let reference = runtime
            .register_password("ssh-node", "password".to_owned())
            .expect("password registers");
        assert_eq!(reference, "session:ssh-node");
        assert!(
            runtime
                .password_credential_resolver
                .has_session(&reference)
                .unwrap()
        );
    }
}
