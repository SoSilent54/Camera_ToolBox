//! I²C Transfer / EEPROM Provision 节点真实实现（依赖 app 层的 `I2cExecutor`/`EepromExecutor` trait）。
//!
//! 这两个节点不再走 skeleton，而是：
//! - `on_action(Trigger)` 读 config 的连接字段（host/port/username/credentialRef/expectedHostKey）
//!   → 构造 `ControlTargetSpec`；再读 bus/address/register/payload/mode 构造 `I2cHelperAction`
//!   （或 `EepromHelperAction`），经 `rt.services().i2c_executor()?`（/`eeprom_executor()?`）执行，
//!   result 序列化为 `DataPacket::Json` 从 `result` 端口输出。
//! - 未注入 executor 或必需连接字段缺失 → `NodeError::Precondition`，不 panic。
//!
//! 真实 SSH helper 执行体（`SshI2cHelperService`/`SshEepromProvisionService` 适配为 executor）由
//! web 层装配注入（后续任务）；本模块只依赖 trait。

use std::sync::Arc;
use std::time::Duration;

use camera_toolbox_core::Rgba8Frame;

use crate::engine::{
    DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState,
    NodeSpec,
};
use crate::platform::{
    CommandResult, ControlTargetSpec, DecodedVideoFrame, DumpCancellation, I2cHelperAction,
    I2cHelperResult, I2cMessageData, I2cMessageSpec, I2cTransactionSpec, RemoteOperationControl,
    RemoteTimeouts, StreamFrameIdentity, StreamSessionId, TypedCommandRequest,
};
use crate::ports::RasterFormat;

// ---------------------------------------------------------------------------
// I²C Transfer 节点
// ---------------------------------------------------------------------------

pub struct I2cTransferFactory;

impl NodeFactory for I2cTransferFactory {
    fn kind(&self) -> &'static str {
        "i2cTransfer"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(I2cTransferNode { spec }))
    }
}

pub struct I2cTransferNode {
    spec: NodeSpec,
}

impl NodeInstance for I2cTransferNode {
    fn kind(&self) -> &'static str {
        "i2cTransfer"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "trigger to execute I2C transfer");
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        _packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => self.execute(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl I2cTransferNode {
    fn execute(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let target = self.target()?;
        let action = self.i2c_action()?;
        let executor = rt.services().i2c_executor()?;
        let control =
            RemoteOperationControl::new(RemoteTimeouts::default(), DumpCancellation::default())
                .map_err(|error| NodeError::Execution(error.to_string()))?;

        rt.report_state(NodeRuntimeState::Running, "executing I2C transfer");
        let result: I2cHelperResult = executor
            .execute(&target, &self.credential_ref()?, action, control)
            .map_err(NodeError::Execution)?;

        emit_json_result(rt, "result", &result)?;
        rt.report_state(NodeRuntimeState::Idle, "transfer done");
        Ok(())
    }

    /// 由 config 构造连接目标；host/credentialRef 为空 → Precondition。
    fn target(&self) -> Result<ControlTargetSpec, NodeError> {
        let host = config_string(&self.spec, "host");
        if host.trim().is_empty() {
            return Err(NodeError::Precondition(
                "config `host` is required".to_owned(),
            ));
        }
        let port = config_string(&self.spec, "port");
        let port = if port.trim().is_empty() {
            22
        } else {
            port.trim()
                .parse::<u16>()
                .map_err(|_| NodeError::Config("config `port` must be a valid u16".to_owned()))?
        };
        let username = config_string(&self.spec, "username");
        let username = if username.trim().is_empty() {
            "root".to_owned()
        } else {
            username
        };
        let expected_host_key = non_empty(config_string(&self.spec, "expectedHostKey"));
        Ok(ControlTargetSpec {
            host,
            port,
            username,
            expected_host_key,
        })
    }

    fn credential_ref(&self) -> Result<String, NodeError> {
        let credential_ref = config_string(&self.spec, "credentialRef");
        if credential_ref.trim().is_empty() {
            return Err(NodeError::Precondition(
                "config `credentialRef` is required".to_owned(),
            ));
        }
        Ok(credential_ref)
    }

    /// 由 config 构造 I²C 动作：mode=read → 写 register 后读 pageSize 字节；mode=write → 整段写 payload。
    fn i2c_action(&self) -> Result<I2cHelperAction, NodeError> {
        let bus = parse_i2c_bus(&config_string(&self.spec, "bus"))?;
        let address = parse_hex_u16(&config_string(&self.spec, "address"))?;
        let register = parse_hex_u16(&config_string(&self.spec, "register"))?;
        let mode = config_string(&self.spec, "mode");

        let transactions = match mode.as_str() {
            // read：单个 transaction，先写 2 字节 register 地址（大端），再读 1 字节。
            "read" => vec![I2cTransactionSpec {
                bus,
                messages: vec![
                    I2cMessageSpec {
                        address,
                        flags: vec![],
                        data: I2cMessageData::Write {
                            bytes: register.to_be_bytes().to_vec(),
                        },
                    },
                    I2cMessageSpec {
                        address,
                        flags: vec![],
                        data: I2cMessageData::Read { byte_len: 1 },
                    },
                ],
                settle_ms: None,
            }],
            // write：按 pageSize 把 payload 分段成多个 transaction，每段 register 地址 + chunk，写后 settle。
            "write" => page_write_transactions(bus, address, register, &self.spec)?,
            other => {
                return Err(NodeError::Config(format!(
                    "unsupported mode `{other}` (read/write)"
                )));
            }
        };

        Ok(I2cHelperAction::Transfer { transactions })
    }
}

// ---------------------------------------------------------------------------
// EEPROM Provision 节点
// ---------------------------------------------------------------------------

pub struct EepromProvisionFactory;

impl NodeFactory for EepromProvisionFactory {
    fn kind(&self) -> &'static str {
        "eepromProvision"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(EepromProvisionNode { spec }))
    }
}

pub struct EepromProvisionNode {
    spec: NodeSpec,
}

impl NodeInstance for EepromProvisionNode {
    fn kind(&self) -> &'static str {
        "eepromProvision"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(
            NodeRuntimeState::Ready,
            "trigger to inspect/provision EEPROM",
        );
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        _packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => self.execute(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl EepromProvisionNode {
    fn execute(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let target = self.target()?;
        let credential_ref = self.credential_ref()?;
        let action = self.eeprom_action()?;
        let executor = rt.services().eeprom_executor()?;
        let control =
            RemoteOperationControl::new(RemoteTimeouts::default(), DumpCancellation::default())
                .map_err(|error| NodeError::Execution(error.to_string()))?;

        rt.report_state(NodeRuntimeState::Running, "executing EEPROM operation");
        let result = executor
            .execute(&target, &credential_ref, action, control)
            .map_err(NodeError::Execution)?;

        emit_json_result(rt, "result", &result)?;
        rt.report_state(NodeRuntimeState::Idle, "operation done");
        Ok(())
    }

    fn target(&self) -> Result<ControlTargetSpec, NodeError> {
        let host = config_string(&self.spec, "host");
        if host.trim().is_empty() {
            return Err(NodeError::Precondition(
                "config `host` is required".to_owned(),
            ));
        }
        let port = config_string(&self.spec, "port");
        let port = if port.trim().is_empty() {
            22
        } else {
            port.trim()
                .parse::<u16>()
                .map_err(|_| NodeError::Config("config `port` must be u16".to_owned()))?
        };
        let username = config_string(&self.spec, "username");
        let username = if username.trim().is_empty() {
            "root".to_owned()
        } else {
            username
        };
        let expected_host_key = non_empty(config_string(&self.spec, "expectedHostKey"));
        Ok(ControlTargetSpec {
            host,
            port,
            username,
            expected_host_key,
        })
    }

    fn credential_ref(&self) -> Result<String, NodeError> {
        let credential_ref = config_string(&self.spec, "credentialRef");
        if credential_ref.trim().is_empty() {
            return Err(NodeError::Precondition(
                "config `credentialRef` is required".to_owned(),
            ));
        }
        Ok(credential_ref)
    }

    /// 由 config `mode`（inspect/provision）构造 EEPROM 动作。
    fn eeprom_action(&self) -> Result<crate::platform::EepromHelperAction, NodeError> {
        let mode = config_string(&self.spec, "mode");
        match mode.as_str() {
            "inspect" => Ok(crate::platform::EepromHelperAction::Inspect),
            "provision" => {
                let request = self.provision_request()?;
                // 当前 config 未携带 expected_before_sha256，占位空串；真实执行体应在调用前校验。
                Ok(crate::platform::EepromHelperAction::Provision {
                    request,
                    expected_before_sha256: config_string(&self.spec, "expectedBeforeSha256"),
                })
            }
            other => Err(NodeError::Config(format!(
                "unsupported mode `{other}` (inspect/provision)"
            ))),
        }
    }

    fn provision_request(&self) -> Result<camera_toolbox_core::EepromProvisionRequest, NodeError> {
        use camera_toolbox_core::{EepromProvisionRequest, EepromWriteSegment};
        let map_id = config_string(&self.spec, "mapId");
        let payload = parse_hex_bytes(&config_string(&self.spec, "payload"))?;
        let segments = if payload.is_empty() {
            vec![]
        } else {
            vec![EepromWriteSegment {
                offset: 0,
                bytes: payload,
            }]
        };
        Ok(EepromProvisionRequest {
            map_id,
            mode: camera_toolbox_core::EepromProvisioningMode::UpdateCalibration,
            serial_number: config_string(&self.spec, "serialNumber"),
            overwrite_existing_serial: false,
            segments,
        })
    }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 清理 config 读取用的字符串辅助。
fn config_string(spec: &NodeSpec, key: &str) -> String {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn non_empty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

/// 解析 `i2c-N` 或十进制数字为 u32。
fn parse_i2c_bus(bus: &str) -> Result<u32, NodeError> {
    let digits = bus.trim().strip_prefix("i2c-").unwrap_or(bus.trim());
    digits
        .parse::<u32>()
        .map_err(|_| NodeError::Config("config `bus` must be `i2c-N` or decimal N".to_owned()))
}

/// 解析 `0x..` 十六进制为 u16（address / register）。
fn parse_hex_u16(value: &str) -> Result<u16, NodeError> {
    let trimmed = value.trim();
    let digits = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    u16::from_str_radix(digits, 16)
        .map_err(|_| NodeError::Config(format!("config value `{value}` must be a hex u16")))
}

/// 解析十六进制字符串（可含 `0x` 前缀）为字节。
fn parse_hex_bytes(value: &str) -> Result<Vec<u8>, NodeError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }
    let digits = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    let digits = digits.replace('_', "");
    if !digits.len().is_multiple_of(2) {
        return Err(NodeError::Config(
            "hex payload must have even length".to_owned(),
        ));
    }
    (0..digits.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&digits[i..i + 2], 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|_| NodeError::Config("config `payload` must be valid hex".to_owned()))
}

fn config_usize(spec: &NodeSpec, key: &str, fallback: usize) -> usize {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(fallback)
}

/// 按 pageSize 把 payload 分段成多个写 transaction（每段 register 地址 + chunk，写后 settle 5ms）。
/// 与 main.rs 的 `page_write_transactions` 语义一致，保证 EEPROM page-write 周期正确分页。
fn page_write_transactions(
    bus: u32,
    address: u16,
    register: u16,
    spec: &NodeSpec,
) -> Result<Vec<I2cTransactionSpec>, NodeError> {
    let payload = parse_hex_bytes(&config_string(spec, "payload"))?;
    if payload.is_empty() {
        return Err(NodeError::Config(
            "config `payload` is required for write mode".to_owned(),
        ));
    }
    let page_size = config_usize(spec, "pageSize", 16).max(1);

    let mut transactions = Vec::new();
    for chunk in payload.chunks(page_size) {
        // 每段从当前 register + 已写偏移开始；register 偏移累加 page_size。
        let segment_register = register
            .checked_add(u16::try_from(transactions.len() * page_size).unwrap_or(u16::MAX))
            .ok_or_else(|| NodeError::Config("register offset overflow".to_owned()))?;
        let mut bytes = segment_register.to_be_bytes().to_vec();
        bytes.extend_from_slice(chunk);
        transactions.push(I2cTransactionSpec {
            bus,
            messages: vec![I2cMessageSpec {
                address,
                flags: vec![],
                data: I2cMessageData::Write { bytes },
            }],
            settle_ms: Some(5),
        });
    }
    Ok(transactions)
}

/// 把可序列化结果 emit 到 `result` 端口（若声明）；序列化为 Json 负载。
fn emit_json_result<T: serde::Serialize>(
    rt: &NodeRuntime,
    port: &str,
    result: &T,
) -> Result<(), NodeError> {
    let value = serde_json::to_value(result)
        .map_err(|error| NodeError::Execution(format!("serialize result failed: {error}")))?;
    rt.emit(port, DataPacket::Json(Arc::new(value)))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// X5 Device 节点
// ---------------------------------------------------------------------------

/// X5_233 设备控制节点：经 `X5ControlClient` 执行 probe / status / snapshot，
/// 输出设备状态到 `control` 端口、抓帧元数据到 `snapshot` 端口。
pub struct X5DeviceFactory;

impl NodeFactory for X5DeviceFactory {
    fn kind(&self) -> &'static str {
        "x5Device"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(X5DeviceNode { spec }))
    }
}

pub struct X5DeviceNode {
    spec: NodeSpec,
}

impl X5DeviceNode {
    fn host(&self) -> Result<String, NodeError> {
        non_empty(config_string(&self.spec, "host"))
            .ok_or_else(|| NodeError::Precondition("x5Device host must be configured".to_owned()))
    }

    fn port(&self) -> u16 {
        let s = config_string(&self.spec, "tcpPort");
        s.parse::<u16>().unwrap_or(9073)
    }

    fn channel(&self) -> u16 {
        self.spec
            .config
            .get("channels")
            .and_then(serde_json::Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as u16)
            .unwrap_or(0)
    }
}

impl NodeInstance for X5DeviceNode {
    fn kind(&self) -> &'static str {
        "x5Device"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "trigger to probe X5 status");
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        _packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let client = rt.services().x5_client()?;
        let host = self.host()?;
        let port = self.port();
        match action {
            // Trigger：读取设备状态（probe + status），结果 emit 到 control 端口。
            NodeAction::Trigger => {
                let status = client.status(&host, port).map_err(NodeError::Execution)?;
                rt.report_state(NodeRuntimeState::Running, "querying X5 status");
                let _ = rt.emit("control", DataPacket::Json(Arc::new(status)));
                rt.report_state(NodeRuntimeState::Idle, "x5 status ready");
                Ok(())
            }
            // Custom "probe"：仅探针。
            NodeAction::Custom { name, .. } if name == "probe" => {
                let summary = client.probe(&host, port).map_err(NodeError::Execution)?;
                let _ = rt.emit("control", DataPacket::Json(Arc::new(summary)));
                Ok(())
            }
            // Custom "snapshot"：抓帧，元数据 emit 到 snapshot 端口。
            NodeAction::Custom { name, .. } if name == "snapshot" => {
                let snapshot = client
                    .capture_snapshot(&host, port, self.channel())
                    .map_err(NodeError::Execution)?;
                let _ = rt.emit("snapshot", DataPacket::Json(Arc::new(snapshot)));
                Ok(())
            }
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SFTP File Source 节点
// ---------------------------------------------------------------------------

const DECODED_IMAGE_BYTE_LIMIT: usize = 128 * 1024 * 1024;

/// SFTP 文件源：经 `SftpFileReader` 读取远程图片字节，经 `RasterImageCodec` 解码为 image.frame。
pub struct SftpFileSourceFactory;

impl NodeFactory for SftpFileSourceFactory {
    fn kind(&self) -> &'static str {
        "sftpFileSource"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(SftpFileSourceNode { spec }))
    }
}

pub struct SftpFileSourceNode {
    spec: NodeSpec,
}

impl SftpFileSourceNode {
    fn target(&self) -> Result<ControlTargetSpec, NodeError> {
        let host = non_empty(config_string(&self.spec, "host")).ok_or_else(|| {
            NodeError::Precondition("sftpFileSource host must be configured".to_owned())
        })?;
        let port = config_string(&self.spec, "port")
            .parse::<u16>()
            .map_err(|_| {
                NodeError::Config("sftpFileSource port must be in 1..=65535".to_owned())
            })?;
        if port == 0 {
            return Err(NodeError::Config(
                "sftpFileSource port must be in 1..=65535".to_owned(),
            ));
        }
        Ok(ControlTargetSpec {
            host,
            port,
            username: config_string(&self.spec, "username"),
            expected_host_key: non_empty(config_string(&self.spec, "expectedHostKey")),
        })
    }

    fn remote_path(&self) -> Result<String, NodeError> {
        let root = config_string(&self.spec, "remoteRoot");
        let selection = config_string(&self.spec, "selection");
        if selection.trim().is_empty() {
            return Err(NodeError::Precondition(
                "sftpFileSource selection must be configured".to_owned(),
            ));
        }
        let mut path = root.trim_end_matches('/').to_owned();
        path.push('/');
        path.push_str(selection.trim_start_matches('/'));
        Ok(path)
    }
}

impl NodeInstance for SftpFileSourceNode {
    fn kind(&self) -> &'static str {
        "sftpFileSource"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "trigger to fetch remote image");
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        _packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => self.fetch(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl SftpFileSourceNode {
    fn fetch(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let target = self.target()?;
        let credential_ref =
            non_empty(config_string(&self.spec, "credentialRef")).ok_or_else(|| {
                NodeError::Precondition(
                    "sftpFileSource credentialRef must be configured".to_owned(),
                )
            })?;
        let path = self.remote_path()?;
        rt.report_state(NodeRuntimeState::Running, "fetching remote image");
        let format = match path
            .rsplit('.')
            .next()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => RasterFormat::Png,
            Some("jpg" | "jpeg") => RasterFormat::Jpeg,
            _ => {
                return Err(NodeError::Precondition(
                    "unsupported remote image extension".to_owned(),
                ));
            }
        };

        let reader = rt.services().sftp_reader()?;
        let control = control_timeout(30, 120)?;
        let bytes = reader
            .read(
                &target,
                &credential_ref,
                &path,
                DECODED_IMAGE_BYTE_LIMIT,
                control,
            )
            .map_err(NodeError::Execution)?;

        let codec = rt.services().image_codec()?;
        let rgba: Rgba8Frame = codec
            .decode_rgba8(format, &bytes, DECODED_IMAGE_BYTE_LIMIT)
            .map_err(|e| NodeError::Execution(e.to_string()))?;

        let (width, height) = (rgba.width, rgba.height);
        let compact = compact_rgba8(&rgba, width, height)?;
        let frame = DecodedVideoFrame {
            width,
            height,
            rgba: compact,
            identity: StreamFrameIdentity::unavailable(
                StreamSessionId::new(format!("sftp-{}", self.spec.id))
                    .map_err(|_| NodeError::Execution("invalid session id".to_owned()))?,
                0,
                0,
                "sftp file source",
            ),
        };
        rt.emit("image", DataPacket::ImageFrame(Arc::new(frame)))?;
        rt.report_state(NodeRuntimeState::Idle, "remote image ready");
        Ok(())
    }
}

/// 把 `Rgba8Frame`（可能带 stride）复制为紧密排列的 RGBA 字节。
fn compact_rgba8(frame: &Rgba8Frame, width: u32, height: u32) -> Result<Arc<[u8]>, NodeError> {
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|w| w.checked_mul(4))
        .ok_or_else(|| NodeError::Execution("image width overflow".to_owned()))?;
    let total = row_bytes
        .checked_mul(height as usize)
        .ok_or_else(|| NodeError::Execution("image size overflow".to_owned()))?;
    let mut compact = Vec::with_capacity(total);
    let pixels = frame.pixels();
    for row in 0..height as usize {
        let start = row * frame.stride;
        let end = start + row_bytes;
        let Some(row_slice) = pixels.get(start..end) else {
            return Err(NodeError::Execution(
                "image stride/layout inconsistent".to_owned(),
            ));
        };
        compact.extend_from_slice(row_slice);
    }
    Ok(compact.into())
}

// ---------------------------------------------------------------------------
// SSH Session 节点
// ---------------------------------------------------------------------------

/// SSH 会话：经 `SshCommandExecutor` 执行一次 allowlisted typed 命令，输出 CommandResult。
pub struct SshSessionFactory;

impl NodeFactory for SshSessionFactory {
    fn kind(&self) -> &'static str {
        "sshSession"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(SshSessionNode { spec }))
    }
}

pub struct SshSessionNode {
    spec: NodeSpec,
}

impl NodeInstance for SshSessionNode {
    fn kind(&self) -> &'static str {
        "sshSession"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "trigger to run remote command");
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        _packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => self.run(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl SshSessionNode {
    fn run(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let host = non_empty(config_string(&self.spec, "host")).ok_or_else(|| {
            NodeError::Precondition("sshSession host must be configured".to_owned())
        })?;
        let credential_ref =
            non_empty(config_string(&self.spec, "credentialRef")).ok_or_else(|| {
                NodeError::Precondition("sshSession credentialRef must be configured".to_owned())
            })?;
        let recipe_id = non_empty(config_string(&self.spec, "recipeId")).ok_or_else(|| {
            NodeError::Precondition("sshSession recipeId must be configured".to_owned())
        })?;
        let target = ControlTargetSpec {
            host,
            port: config_string(&self.spec, "port")
                .parse::<u16>()
                .unwrap_or(22),
            username: config_string(&self.spec, "username"),
            expected_host_key: non_empty(config_string(&self.spec, "expectedHostKey")),
        };
        let request = TypedCommandRequest::new(recipe_id)
            .map_err(|e| NodeError::Precondition(e.to_string()))?;

        let executor = rt.services().ssh_command_executor()?;
        let control = control_timeout(10, 60)?;
        let result: CommandResult = executor
            .execute(&target, &credential_ref, request, control)
            .map_err(NodeError::Execution)?;

        // CommandResult 未实现 Serialize，手动折叠为 JSON（stdout/stderr 只给长度摘要）。
        let value = serde_json::json!({
            "terminal": format!("{:?}", result.terminal),
            "stdoutLen": result.stdout.len(),
            "stderrLen": result.stderr.len(),
            "stdoutTruncated": result.stdout_truncated,
            "stderrTruncated": result.stderr_truncated,
            "artifactPath": result.artifact_path,
        });
        let _ = rt.emit("result", DataPacket::Json(Arc::new(value)));
        rt.report_state(NodeRuntimeState::Idle, "command executed");
        Ok(())
    }
}

fn control_timeout(
    connect_secs: u64,
    overall_secs: u64,
) -> Result<RemoteOperationControl, NodeError> {
    RemoteOperationControl::new(
        RemoteTimeouts {
            connect: Duration::from_secs(connect_secs),
            idle: Duration::from_secs(overall_secs),
            overall: Duration::from_secs(overall_secs),
        },
        DumpCancellation::default(),
    )
    .map_err(|e| NodeError::Precondition(e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};

    use super::*;
    use crate::engine::{EngineServices, NodeReporter, OutputRegistry, SpawnContext};
    use crate::platform::{EepromExecutor, I2cExecutor};

    fn i2c_spec() -> NodeSpec {
        NodeSpec {
            id: "i2c-1".to_owned(),
            kind: "i2cTransfer".to_owned(),
            title: "I2C Transfer".to_owned(),
            inputs: vec![],
            outputs: vec![crate::engine::PortSpec {
                id: "result".to_owned(),
                label: "Result".to_owned(),
                kind: "i2c.result.v1".to_owned(),
                cardinality: crate::engine::PortCardinality::One,
                required: false,
            }],
            config: serde_json::json!({
                "host": "camera.local",
                "port": "22",
                "username": "root",
                "credentialRef": "key-file:/x",
                "expectedHostKey": "",
                "bus": "i2c-8",
                "address": "0x50",
                "register": "0x0000",
                "payload": "",
                "pageSize": 16,
                "mode": "read",
                "confirmWrites": true,
            }),
        }
    }

    fn runtime(services: EngineServices) -> (NodeRuntime, OutputRegistry) {
        let (status_tx, _status_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("i2c-1".to_owned(), status_tx, event_tx);
        let outputs = OutputRegistry::default();
        let ctx = SpawnContext {
            outputs: outputs.clone(),
            reporter,
            services: Arc::new(services),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        };
        (NodeRuntime::new(ctx), outputs)
    }

    /// 记录调用并返回固定 BusList 结果的 mock I2cExecutor。
    struct RecordingI2cExecutor {
        called: Arc<Mutex<usize>>,
    }

    impl I2cExecutor for RecordingI2cExecutor {
        fn execute(
            &self,
            _target: &ControlTargetSpec,
            _credential_ref: &str,
            _action: I2cHelperAction,
            _control: RemoteOperationControl,
        ) -> Result<I2cHelperResult, String> {
            *self.called.lock().unwrap() += 1;
            Ok(I2cHelperResult::Transfer {
                transactions: vec![],
            })
        }
    }

    #[test]
    fn missing_executor_is_precondition() {
        let mut node = I2cTransferNode { spec: i2c_spec() };
        let (mut rt, _outputs) = runtime(EngineServices::default());
        let err = node
            .on_action(NodeAction::Trigger, &mut rt)
            .expect_err("missing i2c_executor must be a precondition");
        assert!(matches!(err, NodeError::Precondition(_)), "got {err:?}");
    }

    #[test]
    fn missing_host_is_precondition_before_executor() {
        let mut spec = i2c_spec();
        spec.config["host"] = serde_json::json!("");
        spec.config["credentialRef"] = serde_json::json!("key-file:/x");
        let mut node = I2cTransferNode { spec };
        let executor_called = Arc::new(Mutex::new(0));
        let services = EngineServices {
            i2c_executor: Some(Arc::new(RecordingI2cExecutor {
                called: Arc::clone(&executor_called),
            })),
            ..EngineServices::default()
        };
        let (mut rt, _outputs) = runtime(services);
        let err = node
            .on_action(NodeAction::Trigger, &mut rt)
            .expect_err("missing host must be precondition");
        assert!(matches!(err, NodeError::Precondition(_)), "got {err:?}");
        assert_eq!(*executor_called.lock().unwrap(), 0);
    }

    #[test]
    fn executor_is_invoked_and_result_emitted() {
        let mut node = I2cTransferNode { spec: i2c_spec() };
        let executor_called = Arc::new(Mutex::new(0));
        let services = EngineServices {
            i2c_executor: Some(Arc::new(RecordingI2cExecutor {
                called: Arc::clone(&executor_called),
            })),
            ..EngineServices::default()
        };
        let (mut rt, _outputs) = runtime(services);
        // 输出端口 result 未接线（无下游 emit 为 no-op 成功），触发应成功返回。
        assert!(node.on_action(NodeAction::Trigger, &mut rt).is_ok());
        assert_eq!(*executor_called.lock().unwrap(), 1);
    }

    #[test]
    fn hex_and_bus_parsing() {
        assert_eq!(parse_hex_u16("0x50").unwrap(), 0x50);
        assert_eq!(parse_hex_u16("0X50").unwrap(), 0x50);
        assert_eq!(parse_i2c_bus("i2c-8").unwrap(), 8);
        assert_eq!(parse_i2c_bus("8").unwrap(), 8);
        assert_eq!(parse_hex_bytes("0x00ab").unwrap(), vec![0x00, 0xab]);
        assert_eq!(parse_hex_bytes("").unwrap(), Vec::<u8>::new());
        assert!(parse_hex_bytes("0x0").is_err()); // 奇数长度
    }

    // 避免 EepromExecutor 未使用告警：即便本任务只深入测 i2c，也确保 trait 可被引用。
    #[allow(dead_code)]
    fn _eeprom_executor_is_importable() -> Option<Arc<dyn EepromExecutor>> {
        None
    }
}
