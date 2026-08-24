mod engine_api;
mod engine_bridge;
mod files_api;
mod serial_field;
mod workflow;
mod ws_hub;
mod ws_router;

use std::{
    collections::HashMap,
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
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
    CredentialResolver, ProductionCredentialResolver, RusshTransportFactory, ServerHostKey,
    SshCommandService, SshConnectionTarget, SshI2cHelperService, SshTransportFactory,
    production_recipe_registry_from_env,
};
use camera_toolbox_adapters::x5_tcp_client;
use camera_toolbox_app::engine::{CaptureMode, CaptureTarget};
use camera_toolbox_app::{
    CalibrationBackend, CalibrationCancellation, CommandResult, CommandService, ControlTargetSpec,
    DecodedVideoFrame, I2cAuthorizedWritePlan, I2cHelperAction, I2cHelperOperation,
    I2cHelperResult, I2cHelperService, I2cInspectPlan, I2cMessageData, I2cMessageSpec,
    I2cPageWrite, I2cTaskExecutor, I2cTransactionSpec, RemoteFileStat, RemoteOperationControl,
    SftpFileReader, SshCommandExecutor, SshConnection, SshConnectionService, TypedCommandRequest,
    X5ControlClient, X5233CapturePayload,
};
use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationRequest, CalibrationSolution,
    InitialIntrinsics, ViewCalibrationResult,
};
use clap::Parser;
use image::{ColorType, codecs::jpeg::JpegEncoder};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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
    engine_runtime: Arc<engine_api::EngineRuntime>,
    ws_hub: Arc<ws_hub::WsHub>,
    /// 后端权威工作流图；前端只渲染该状态的 snapshot，不维护独立 draft graph。
    graph_session: Arc<Mutex<WorkflowGraph>>,
}

struct ControlRuntime {
    /// 运行时 SSH 句柄到 credential ref 的进程内绑定；永不序列化或返回前端。
    sessions: Arc<Mutex<HashMap<String, String>>>,
    #[cfg(feature = "platform-ssh")]
    credential_resolver: Arc<dyn CredentialResolver>,
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
            sessions: Arc::new(Mutex::new(HashMap::new())),
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
            sessions: Arc::new(Mutex::new(HashMap::new())),
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

/// 规范化并固定工作流 SSH 目标的服务端主机密钥，避免 transport 接收未固定的目标。
#[cfg(feature = "platform-ssh")]
fn canonical_pinned_ssh_target(
    target: &ControlTargetSpec,
) -> std::result::Result<ControlTargetSpec, String> {
    let expected_host_key = target
        .expected_host_key
        .as_deref()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| "SSH connection requires a non-empty expected host key".to_owned())?;
    let expected_host_key = ServerHostKey::from_openssh(expected_host_key)
        .map_err(|error| format!("SSH connection expected host key is invalid: {error}"))?
        .openssh()
        .to_owned();
    Ok(ControlTargetSpec {
        expected_host_key: Some(expected_host_key),
        ..target.clone()
    })
}

/// 建立计划节点使用的短生命周期 SSH 连接，并把 credential ref 留在进程内绑定表。
#[cfg(feature = "platform-ssh")]
impl SshConnectionService for ControlRuntime {
    fn connect(
        &self,
        target: &ControlTargetSpec,
        credential_ref: &str,
        control: RemoteOperationControl,
    ) -> std::result::Result<SshConnection, String> {
        let target = canonical_pinned_ssh_target(target)?;
        let credential_ref = validate_credential_ref(credential_ref).map_err(|e| e.error)?;
        let credential = self.credential_resolver.resolve(&credential_ref)?;
        let transport_target = ssh_target_from_spec(&target);
        let _session = self
            .ssh_transport
            .connect(&transport_target, credential, &control)
            .map_err(|error| error.to_string())?;
        let id = format!("workflow-ssh-{}-{}", target.host, monotonic_nonce());
        self.sessions
            .lock()
            .map_err(|_| "SSH session store is poisoned".to_owned())?
            .insert(id.clone(), credential_ref);
        Ok(SshConnection::new(id, target))
    }
    fn revoke(
        &self,
        connection: &SshConnection,
        _control: RemoteOperationControl,
    ) -> std::result::Result<(), String> {
        // connection handle 与密码 session 生命周期独立：撤销只使旧 handle 失效，
        // 保留 credentialRef 供同一已保存 SSH 配置重新连接。
        self.sessions
            .lock()
            .map_err(|_| "SSH session store is poisoned".to_owned())?
            .remove(connection.id());
        Ok(())
    }
}
#[cfg(feature = "platform-ssh")]
impl ControlRuntime {
    fn i2c_service_for(
        &self,
        connection: &SshConnection,
    ) -> std::result::Result<SshI2cHelperService, String> {
        let credential_ref = self
            .sessions
            .lock()
            .map_err(|_| "SSH session store is poisoned".to_owned())?
            .get(connection.id())
            .cloned()
            .ok_or_else(|| "SSH connection is not active".to_owned())?;
        SshI2cHelperService::new(
            format!("workflow-web-plan-{}", connection.id()),
            ssh_target_from_spec(connection.target()),
            credential_ref,
            1_048_576,
            self.helper_payload().map_err(|e| e.error)?,
            Arc::clone(&self.credential_resolver),
            Arc::clone(&self.ssh_transport),
        )
        .map_err(|error| format_i2c_service_error(&error))
    }
}

#[cfg(feature = "platform-ssh")]
impl I2cTaskExecutor for ControlRuntime {
    fn inspect(
        &self,
        connection: &SshConnection,
        plan: &I2cInspectPlan,
        control: RemoteOperationControl,
    ) -> std::result::Result<Vec<u8>, String> {
        let transactions = plan
            .read_ranges
            .iter()
            .map(|range| I2cTransactionSpec {
                bus: plan.target.bus,
                messages: vec![
                    I2cMessageSpec {
                        address: plan.target.address,
                        flags: Vec::new(),
                        data: I2cMessageData::Write {
                            bytes: register_bytes(plan.target.address_width_bytes, range.offset),
                        },
                    },
                    I2cMessageSpec {
                        address: plan.target.address,
                        flags: Vec::new(),
                        data: I2cMessageData::Read {
                            byte_len: range.byte_len,
                        },
                    },
                ],
                settle_ms: None,
            })
            .collect();
        let result = self
            .i2c_service_for(connection)?
            .execute(
                I2cHelperOperation {
                    action: I2cHelperAction::Transfer { transactions },
                },
                control,
            )
            .map_err(|error| format_i2c_service_error(&error))?;
        Ok(read_result_bytes(result))
    }

    fn verify_authorized(
        &self,
        connection: &SshConnection,
        authorized: &I2cAuthorizedWritePlan,
        control: RemoteOperationControl,
    ) -> std::result::Result<(), String> {
        if authorized.connection_id != connection.id() {
            return Err("authorized write plan is bound to another SSH connection".to_owned());
        }
        let before = self.inspect(connection, &authorized.inspect_plan, control)?;
        if before_image_digest(&before) != authorized.expected_before_sha256 {
            return Err("EEPROM before-image changed since approval".to_owned());
        }
        Ok(())
    }

    fn write_page(
        &self,
        connection: &SshConnection,
        authorized: &I2cAuthorizedWritePlan,
        page_index: usize,
        page: &I2cPageWrite,
        control: RemoteOperationControl,
    ) -> std::result::Result<Vec<u8>, String> {
        if authorized.connection_id != connection.id() {
            return Err("authorized write plan is bound to another SSH connection".to_owned());
        }
        if authorized.page_at(page_index) != Some(page) {
            return Err(format!(
                "authorized page {page_index} is not the exact compiled page"
            ));
        }
        let transaction = I2cTransactionSpec {
            bus: authorized.candidate.target.bus,
            messages: vec![I2cMessageSpec {
                address: authorized.candidate.target.address,
                flags: Vec::new(),
                data: I2cMessageData::Write {
                    bytes: [
                        register_bytes(
                            authorized.candidate.target.address_width_bytes,
                            page.offset,
                        ),
                        page.bytes.clone(),
                    ]
                    .concat(),
                },
            }],
            settle_ms: Some(page.settle_ms),
        };
        let readback = I2cTransactionSpec {
            bus: authorized.candidate.target.bus,
            messages: vec![
                I2cMessageSpec {
                    address: authorized.candidate.target.address,
                    flags: Vec::new(),
                    data: I2cMessageData::Write {
                        bytes: register_bytes(
                            authorized.candidate.target.address_width_bytes,
                            page.offset,
                        ),
                    },
                },
                I2cMessageSpec {
                    address: authorized.candidate.target.address,
                    flags: Vec::new(),
                    data: I2cMessageData::Read {
                        byte_len: u16::try_from(page.bytes.len())
                            .map_err(|_| "page exceeds u16 length".to_owned())?,
                    },
                },
            ],
            settle_ms: None,
        };
        let result = self
            .i2c_service_for(connection)?
            .execute(
                I2cHelperOperation {
                    action: I2cHelperAction::Transfer {
                        transactions: vec![transaction, readback],
                    },
                },
                control,
            )
            .map_err(|error| format_i2c_service_error(&error))?;
        Ok(read_result_bytes(result))
    }
}

fn register_bytes(width: u8, offset: u16) -> Vec<u8> {
    match width {
        1 => vec![(offset & 0xff) as u8],
        _ => offset.to_be_bytes().to_vec(),
    }
}

/// 与 app 层 inspect/approval/execution 报告一致的唯一 before-image 表示。
fn before_image_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn read_result_bytes(result: I2cHelperResult) -> Vec<u8> {
    let I2cHelperResult::Transfer { transactions } = result else {
        return Vec::new();
    };
    transactions
        .into_iter()
        .flat_map(|transaction| transaction.messages)
        .filter(|message| {
            matches!(
                message.direction,
                camera_toolbox_app::I2cMessageDirection::Read
            )
        })
        .flat_map(|message| message.bytes)
        .collect()
}

fn monotonic_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

/// 把 X5 控制客户端抽象接到真实的 X5_233 TCP 模块（纯 TCP）。
impl X5ControlClient for ControlRuntime {
    fn probe(&self, host: &str, port: u16) -> std::result::Result<serde_json::Value, String> {
        x5_tcp_client::probe(host, port).map(|summary| x5_probe_response(&summary))
    }
    fn status(&self, host: &str, port: u16) -> std::result::Result<serde_json::Value, String> {
        x5_tcp_client::status(host, port).map(|status| x5_status_response(&status))
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
        let credential = self.credential_resolver.resolve(&credential_ref)?;
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
        let credential = self.credential_resolver.resolve(&credential_ref)?;
        let mut session = self
            .ssh_transport
            .connect(&ssh_target_from_spec(target), credential, &control)
            .map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        session.read_file(remote_path, &control, &mut |chunk| {
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(camera_toolbox_adapters::platforms::ssh_managed::SshTransportError::ReadLimitExceeded { requested: bytes.len().saturating_add(chunk.len()) as u64, limit: limit as u64 });
            }
            bytes.extend_from_slice(chunk); Ok(())
        }).map_err(|e| e.to_string())?;
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
        let target = canonical_pinned_ssh_target(target)?;
        let credential_ref = validate_credential_ref(credential_ref).map_err(|e| e.error)?;
        // recipe 注册表从环境变量加载；无部署 recipe 时退化为空注册表（execute 会报 RecipeNotAllowed）。
        let recipes =
            std::sync::Arc::new(production_recipe_registry_from_env().unwrap_or_else(|_| {
                camera_toolbox_adapters::platforms::ssh_managed::CommandRecipeRegistry::new()
            }));
        let allowed_recipe_id = request.recipe_id.clone();
        let service = SshCommandService::new(
            format!("workflow-web-ssh-{}", target.host),
            ssh_target_from_spec(&target),
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
        validate_workflow(graph).map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid workflow for save: {error}"),
            )
        })?;
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
    let envelope: Value = serde_json::from_slice(&raw).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("failed to parse workflow `{}`: {error}", path.display()),
        )
    })?;
    if envelope.get("schemaVersion").and_then(Value::as_str) == Some("workflow.v1") {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "invalid workflow `{}`: unsupported workflow schema `workflow.v1`",
                path.display()
            ),
        ));
    }
    let graph: WorkflowGraph = serde_json::from_value(envelope).map_err(|error| {
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
    #[test]
    fn canonical_pinned_ssh_target_rejects_missing_or_empty_key_and_strips_comments() {
        let target = ControlTargetSpec {
            host: "camera.local".to_owned(),
            port: 22,
            username: "root".to_owned(),
            expected_host_key: None,
        };
        assert!(canonical_pinned_ssh_target(&target).is_err());

        let target = ControlTargetSpec {
            expected_host_key: Some(" ".to_owned()),
            ..target
        };
        assert!(canonical_pinned_ssh_target(&target).is_err());

        let target = ControlTargetSpec {
            expected_host_key: Some("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ trusted-camera".to_owned()),
            ..target
        };
        let canonical = canonical_pinned_ssh_target(&target).expect("valid OpenSSH host key");
        assert_eq!(
            canonical.expected_host_key.as_deref(),
            Some(
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ"
            )
        );
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
        let definition = crate::workflow::node_definition(crate::workflow::NodeKind::SshConnection);
        graph.nodes.push(crate::workflow::WorkflowNode {
            id: "ssh-pinned".to_owned(),
            kind: crate::workflow::NodeKind::SshConnection,
            title: "Pinned SSH".to_owned(),
            position: crate::workflow::NodePosition { x: 0.0, y: 0.0 },
            state: crate::workflow::NodeRuntimeState::Ready,
            category: definition.category,
            inputs: definition.inputs,
            outputs: definition.outputs,
            config: json!({
                "host": "camera.local",
                "port": "22",
                "username": "root",
                "expectedHostKey": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ",
                "credentialRef": "session:ssh-pinned"
            }),
        });
        store.save(&graph).expect("workflow saved");

        let loaded = store.load("roundtrip").expect("workflow loaded");
        assert_eq!(loaded.id, "roundtrip");
        assert_eq!(loaded.revision, "rev-test");
        assert_eq!(loaded.nodes.len(), graph.nodes.len());
        assert_eq!(
            loaded
                .nodes
                .iter()
                .find(|node| node.id == "ssh-pinned")
                .and_then(|node| node.config.get("expectedHostKey")),
            Some(&json!(
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ"
            ))
        );
        fs::remove_dir_all(dir).ok();
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
    #[test]
    fn workflow_store_rejects_persisted_workflow_v1() {
        let dir = std::env::temp_dir().join(format!("workflow-v1-test-{}", next_revision()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("legacy.ctworkflow.json"),
            br#"{"schemaVersion":"workflow.v1"}"#,
        )
        .unwrap();
        let error = WorkflowStore { dir: dir.clone() }
            .load("legacy")
            .expect_err("v1 must not load");
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.contains("workflow.v1"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn before_image_digest_uses_the_app_canonical_representation() {
        assert_eq!(
            before_image_digest(b"camera-toolbox"),
            "sha256:958421a9fe0d533e8e63a5f73d2e397107202258da1e9e1432f744e9d0c5e59c"
        );
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn ssh_revoke_removes_connection_but_retains_credential_for_reconnect() {
        let runtime = ControlRuntime::production();
        let reference = runtime
            .register_password("ssh-node", "password".to_owned())
            .unwrap();
        let connection = SshConnection::new(
            "connection-1",
            ControlTargetSpec {
                host: "camera.local".to_owned(),
                port: 22,
                username: "root".to_owned(),
                expected_host_key: None,
            },
        );
        runtime
            .sessions
            .lock()
            .expect("session map")
            .insert(connection.id().to_owned(), reference.clone());
        let control = RemoteOperationControl::new(
            camera_toolbox_app::RemoteTimeouts {
                connect: std::time::Duration::from_secs(1),
                idle: std::time::Duration::from_secs(1),
                overall: std::time::Duration::from_secs(1),
            },
            camera_toolbox_app::DumpCancellation::default(),
        )
        .unwrap();
        SshConnectionService::revoke(&runtime, &connection, control).unwrap();
        assert!(
            !runtime
                .sessions
                .lock()
                .expect("session map")
                .contains_key(connection.id())
        );
        assert!(
            runtime
                .password_credential_resolver
                .has_session(&reference)
                .unwrap()
        );
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn control_connect_rejects_missing_or_malformed_pinned_host_key_before_network_io() {
        let runtime = ControlRuntime::production();
        for expected_host_key in [None, Some("not-an-openssh-public-key".to_owned())] {
            let target = ControlTargetSpec {
                host: "192.0.2.1".to_owned(),
                port: 22,
                username: "root".to_owned(),
                expected_host_key,
            };
            let control = RemoteOperationControl::new(
                camera_toolbox_app::RemoteTimeouts {
                    connect: std::time::Duration::from_secs(1),
                    idle: std::time::Duration::from_secs(1),
                    overall: std::time::Duration::from_secs(1),
                },
                camera_toolbox_app::DumpCancellation::default(),
            )
            .unwrap();
            let error =
                SshConnectionService::connect(&runtime, &target, "session:missing", control)
                    .expect_err(
                        "invalid host keys must be rejected before credential or network use",
                    );
            assert!(error.contains("expected host key"));
        }
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
