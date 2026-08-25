//! 通用 I²C task 编码和原子执行节点。
//!
//! Encoder 只把完整 datum 编码为受校验的 EEPROM task；Executor 只消费 task 和 SSH
//! capability，不读取 map 配置、不理解业务字段，也不保存 credential material。

use std::{collections::{BTreeMap, HashSet}, sync::Arc};

use camera_toolbox_core::{builtin_i2c_map, Datum, I2cMapDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::engine::{
    DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState,
    NodeSpec,
};
use crate::platform::{
    ControlTargetSpec, DumpCancellation, I2cMapValidationContract, I2cPageWrite, I2cReadRange,
    I2cReadRequest, I2cTaskTarget, I2cWriteRequest, RemoteOperationControl, RemoteTimeouts,
    SshConnection,
};

const TASK_PORT: &str = "task";
const CONNECTION_PORT: &str = "connection";
const FIELD_PORT_KIND: &str = "data.field.v1";
const PACKET_PORT_KIND: &str = "data.packet.v1";
const TASK_SCHEMA: &str = "camera-toolbox.i2c.task.v1";

/// SSH source：在显式 Connect 后建立不含密钥材料的运行时句柄。
pub struct SshConnectionFactory;

impl NodeFactory for SshConnectionFactory {
    fn kind(&self) -> &'static str { "sshConnection" }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(SshConnectionNode { spec, connection: None }))
    }
}

pub struct SshConnectionNode {
    spec: NodeSpec,
    connection: Option<Arc<SshConnection>>,
}

impl SshConnectionNode {
    fn connect(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let target = ssh_target(&self.spec)?;
        let credential_ref = required_text(&self.spec, "credentialRef")?;
        if !credential_ref.starts_with("session:") {
            return Err(NodeError::Precondition("sshConnection credentialRef must be a process-local session reference".to_owned()));
        }
        if let Some(connection) = self.connection.as_ref() {
            rt.services().ssh_connection_service()?.revoke(connection, remote_control()?).map_err(NodeError::Execution)?;
        }
        let connection = Arc::new(rt.services().ssh_connection_service()?.connect(&target, &credential_ref, remote_control()?).map_err(NodeError::Execution)?);
        if connection.id().trim().is_empty() {
            return Err(NodeError::Execution("SSH connection service returned an empty connection id".to_owned()));
        }
        rt.emit(CONNECTION_PORT, DataPacket::SshConnection(Arc::clone(&connection)))?;
        self.connection = Some(connection);
        rt.report_state(NodeRuntimeState::Ready, "SSH connection established");
        Ok(())
    }

    fn revoke(&mut self, rt: &mut NodeRuntime, message: &str) -> Result<(), NodeError> {
        if let Some(connection) = self.connection.as_ref() {
            rt.services().ssh_connection_service()?.revoke(connection, remote_control()?).map_err(NodeError::Execution)?;
            self.connection = None;
        }
        rt.report_state(NodeRuntimeState::Idle, message);
        Ok(())
    }
}

impl NodeInstance for SshConnectionNode {
    fn kind(&self) -> &'static str { "sshConnection" }
    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "connect to establish an in-memory SSH session");
        Ok(())
    }
    fn on_input(&mut self, _: &str, _: DataPacket, _: &mut NodeRuntime) -> Result<(), NodeError> { Ok(()) }
    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Connect => self.connect(rt),
            NodeAction::Disconnect => self.revoke(rt, "SSH connection revoked"),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }
    fn on_config_update(&mut self, config: serde_json::Value, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        // 工作流编辑可保存未配置的 SSH 草稿；Connect 时才校验连接所需字段。
        self.revoke(rt, "SSH connection revoked after configuration change")?;
        self.spec.config = config;
        Ok(())
    }
    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> { self.revoke(rt, "SSH connection revoked while stopping") }
}

/// `i2cFieldEncoder` 工厂。
pub struct I2cFieldEncoderFactory;
impl NodeFactory for I2cFieldEncoderFactory {
    fn kind(&self) -> &'static str { "i2cFieldEncoder" }
    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> { Ok(Box::new(I2cFieldEncoderNode::new(spec)?)) }
}

#[derive(Clone)]
struct EncoderInput {
    port_id: String,
    datum_name: String,
    required: bool,
}

/// 将通用 FieldData 编码为可交换、严格验证的 task PacketData。
pub struct I2cFieldEncoderNode {
    map: I2cMapDefinition,
    operation: TaskOperation,
    inputs: Vec<EncoderInput>,
    received: BTreeMap<String, Arc<Datum>>,
}

impl I2cFieldEncoderNode {
    fn new(spec: NodeSpec) -> Result<Self, NodeError> {
        validate_encoder_ports(&spec)?;
        let map = compile_encoder_map(&spec)?;
        let operation = parse_operation(&spec.config)?;
        let inputs = parse_encoder_inputs(&spec, &map)?;
        Ok(Self { map, operation, inputs, received: BTreeMap::new() })
    }

    fn emit_task_if_ready(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        if self.inputs.iter().any(|input| input.required && !self.received.contains_key(&input.port_id)) { return Ok(()); }
        let mut fields = Vec::with_capacity(self.inputs.len());
        for input in &self.inputs {
            if let Some(field) = self.received.get(&input.port_id) { fields.push(field.as_ref().clone()); }
        }
        let image = self.map.encode(&fields).map_err(|error| NodeError::Precondition(format!("I2C encoder rejected fields: {error}")))?;
        let target = I2cTaskTarget {
            bus: self.map.target.bus,
            address: u16::from(self.map.target.transport.i2c_address),
            address_width_bytes: self.map.target.transport.address_width_bits / 8,
            page_size_bytes: self.map.target.transport.page_size_bytes,
            write_cycle_ms: self.map.target.transport.write_cycle_ms,
        };
        let read_ranges = self.map.read_before.ranges.iter().map(|range| I2cReadRange { offset: range.offset, byte_len: range.byte_len }).collect();
        let task = I2cTaskPacket {
            schema: TASK_SCHEMA.to_owned(),
            operation: self.operation,
            map_id: self.map.id.clone(),
            map_digest: sha256_hex(format!("{:?}", self.map).as_bytes()),
            target: target.clone(),
            read_ranges,
            validation: I2cMapValidationContract::from_map(&self.map),
            pages: image.pages.into_iter().map(|page| I2cPageWrite { offset: page.offset, bytes: page.bytes, settle_ms: target.write_cycle_ms }).collect(),
            final_image: image.bytes,
            verify_after_write: self.map.readback.required,
            final_image_digest: String::new(),
        }.with_digest();
        task.validate().map_err(|error| NodeError::Precondition(format!("compiled I2C task is invalid: {error}")))?;
        rt.emit(TASK_PORT, DataPacket::PacketData(Arc::new(serde_json::to_value(task).map_err(|error| NodeError::Execution(error.to_string()))?)))?;
        rt.report_state(NodeRuntimeState::Ready, "I2C task compiled from encoded fields");
        Ok(())
    }
}

impl NodeInstance for I2cFieldEncoderNode {
    fn kind(&self) -> &'static str { "i2cFieldEncoder" }
    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> { rt.report_state(NodeRuntimeState::Idle, "waiting for configured field data"); Ok(()) }
    fn on_input(&mut self, port: &str, packet: DataPacket, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let input = self.inputs.iter().find(|input| input.port_id == port).ok_or_else(|| NodeError::Precondition(format!("i2cFieldEncoder received input on unknown port `{port}`")))?;
        let DataPacket::TypedField { datum, .. } = packet else { return Err(NodeError::Precondition("i2cFieldEncoder inputs require data.field.v1".to_owned())); };
        if datum.name != input.datum_name { return Err(NodeError::Precondition(format!("encoder port `{port}` expects datum `{}`, got `{}`", input.datum_name, datum.name))); }
        self.received.insert(port.to_owned(), datum);
        self.emit_task_if_ready(rt)
    }
    fn on_action(&mut self, action: NodeAction, _: &mut NodeRuntime) -> Result<(), NodeError> { Err(NodeError::UnsupportedAction(action.name().to_owned())) }
    fn on_config_update(&mut self, config: serde_json::Value, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let spec = NodeSpec { config, ..NodeSpec { id: String::new(), kind: "i2cFieldEncoder".to_owned(), title: String::new(), inputs: Vec::new(), outputs: Vec::new(), config: serde_json::Value::Null } };
        self.map = compile_encoder_map(&spec)?;
        self.operation = parse_operation(&spec.config)?;
        self.received.clear();
        rt.report_state(NodeRuntimeState::Idle, "I2C encoder configuration updated; send fields again");
        Ok(())
    }
    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> { self.received.clear(); rt.report_state(NodeRuntimeState::Idle, "stopped"); Ok(()) }
}

/// `i2cTaskExecutor` 工厂。
pub struct I2cTaskExecutorFactory;
impl NodeFactory for I2cTaskExecutorFactory {
    fn kind(&self) -> &'static str { "i2cTaskExecutor" }
    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> { validate_executor_ports(&spec)?; Ok(Box::new(I2cTaskExecutorNode { task: None, connection: None })) }
}

/// Executor 只持有 task 与 process-local SSH capability。
pub struct I2cTaskExecutorNode { task: Option<I2cTaskPacket>, connection: Option<Arc<SshConnection>> }
impl NodeInstance for I2cTaskExecutorNode {
    fn kind(&self) -> &'static str { "i2cTaskExecutor" }
    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> { rt.report_state(NodeRuntimeState::Idle, "waiting for task and SSH connection"); Ok(()) }
    fn on_input(&mut self, port: &str, packet: DataPacket, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match (port, packet) {
            (TASK_PORT, DataPacket::PacketData(value)) => self.task = Some(parse_task(&value)?),
            (CONNECTION_PORT, DataPacket::SshConnection(connection)) => self.connection = Some(connection),
            _ => return Err(NodeError::Precondition("i2cTaskExecutor accepts only task PacketData and SSH connection".to_owned())),
        }
        if self.task.is_some() && self.connection.is_some() { rt.report_state(NodeRuntimeState::Ready, "I2C task is ready to execute"); }
        Ok(())
    }
    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        if !matches!(action, NodeAction::Custom { ref name, .. } if name == "execute") { return Err(NodeError::UnsupportedAction(action.name().to_owned())); }
        let task = self.task.as_ref().ok_or_else(|| NodeError::Precondition("i2cTaskExecutor execute requires task".to_owned()))?;
        let connection = self.connection.as_ref().ok_or_else(|| NodeError::Precondition("i2cTaskExecutor execute requires SSH connection".to_owned()))?;
        match task.operation {
            TaskOperation::Read => {
                let request = task.read_request();
                let report = rt.services().i2c_task_executor()?.read(connection, &request, remote_control()?).map_err(NodeError::Execution)?;
                let valid = report.valid;
                rt.emit("readReport", DataPacket::I2cReadReport(Arc::new(report)))?;
                rt.report_state(if valid { NodeRuntimeState::Ready } else { NodeRuntimeState::Warning }, "I2C read completed");
            }
            TaskOperation::GuardedWrite => {
                let request = task.write_request();
                let report = rt.services().i2c_task_executor()?.write(connection, &request, remote_control()?).map_err(NodeError::Execution)?;
                let verified = report.final_verified;
                rt.emit("report", DataPacket::I2cExecutionReport(Arc::new(report)))?;
                rt.report_state(if verified { NodeRuntimeState::Ready } else { NodeRuntimeState::Error }, "I2C guarded write completed");
            }
        }
        Ok(())
    }
    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> { self.task = None; self.connection = None; rt.report_state(NodeRuntimeState::Idle, "stopped"); Ok(()) }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TaskOperation { Read, GuardedWrite }

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct I2cTaskPacket {
    schema: String,
    operation: TaskOperation,
    map_id: String,
    map_digest: String,
    target: I2cTaskTarget,
    read_ranges: Vec<I2cReadRange>,
    validation: I2cMapValidationContract,
    pages: Vec<I2cPageWrite>,
    final_image: Vec<u8>,
    verify_after_write: bool,
    final_image_digest: String,
}

impl I2cTaskPacket {
    fn with_digest(mut self) -> Self { self.final_image_digest = sha256_hex(&self.final_image); self }
    fn validate(&self) -> Result<(), String> {
        if self.schema != TASK_SCHEMA { return Err(format!("schema must be `{TASK_SCHEMA}`")); }
        if self.map_id.trim().is_empty() || self.map_digest.trim().is_empty() { return Err("map identity must not be blank".to_owned()); }
        if !matches!(self.target.address_width_bytes, 1 | 2) || !(0x03..=0x7f).contains(&self.target.address) || self.target.page_size_bytes == 0 { return Err("target is invalid".to_owned()); }
        if self.final_image.is_empty() || self.final_image.len() != usize::from(self.validation.image_bytes) { return Err("final image does not match validation image size".to_owned()); }
        if self.final_image_digest != sha256_hex(&self.final_image) { return Err("final image digest does not match image bytes".to_owned()); }
        if self.read_ranges.is_empty() || self.read_ranges.iter().any(|range| range.byte_len == 0 || usize::from(range.offset).saturating_add(usize::from(range.byte_len)) > self.final_image.len()) { return Err("read ranges are invalid".to_owned()); }
        if self.operation == TaskOperation::GuardedWrite && self.pages.is_empty() { return Err("guarded write task must contain pages".to_owned()); }
        for page in &self.pages {
            if page.bytes.is_empty() || usize::from(page.offset).saturating_add(page.bytes.len()) > self.final_image.len() || page.settle_ms != self.target.write_cycle_ms { return Err("page layout is invalid".to_owned()); }
        }
        Ok(())
    }
    fn read_request(&self) -> I2cReadRequest { I2cReadRequest::new(self.map_id.clone(), self.map_digest.clone(), self.target.clone(), self.read_ranges.clone(), self.validation.clone()) }
    fn write_request(&self) -> I2cWriteRequest { I2cWriteRequest::new(self.map_id.clone(), self.map_digest.clone(), self.target.clone(), self.read_ranges.clone(), self.validation.clone(), self.pages.clone(), self.final_image.clone(), self.verify_after_write) }
}

fn parse_task(value: &serde_json::Value) -> Result<I2cTaskPacket, NodeError> {
    let task: I2cTaskPacket = serde_json::from_value(value.clone()).map_err(|error| NodeError::Precondition(format!("invalid I2C task JSON: {error}")))?;
    task.validate().map_err(|error| NodeError::Precondition(format!("invalid I2C task JSON: {error}")))?;
    Ok(task)
}

fn compile_encoder_map(spec: &NodeSpec) -> Result<I2cMapDefinition, NodeError> {
    let mode = config_text(spec, "mapMode").unwrap_or_else(|| "builtin".to_owned());
    let mut map = match mode.as_str() {
        "builtin" => builtin_i2c_map(&config_text(spec, "mapId").unwrap_or_else(|| "yg-stereo-p24c64g-v1".to_owned())).ok_or_else(|| NodeError::Config("unsupported I2C map".to_owned()))?,
        "custom" => I2cMapDefinition::from_yaml(&config_yaml(spec, "mapYaml").ok_or_else(|| NodeError::Config("i2cFieldEncoder custom map requires mapYaml".to_owned()))?).map_err(|error| NodeError::Config(format!("I2C map compilation failed: {error}")))?,
        _ => return Err(NodeError::Config("i2cFieldEncoder mapMode must be builtin or custom".to_owned())),
    };
    map.target.bus = config_u32(spec, "bus")?.unwrap_or(0);
    let address = config_u16(spec, "address")?.ok_or_else(|| NodeError::Config("i2cFieldEncoder config `address` is required".to_owned()))?;
    map.target.transport.i2c_address = u8::try_from(address).map_err(|_| NodeError::Config("address must fit u8".to_owned()))?;
    if let Some(width) = config_u16(spec, "addressWidthBytes")? { map.target.transport.address_width_bits = u8::try_from(width.checked_mul(8).ok_or_else(|| NodeError::Config("addressWidthBytes overflow".to_owned()))?).map_err(|_| NodeError::Config("addressWidthBytes must be 1 or 2".to_owned()))?; }
    if let Some(size) = config_u16(spec, "pageSizeBytes")? { map.target.transport.page_size_bytes = size; }
    if let Some(cycle) = config_u16(spec, "writeCycleMs")? { map.target.transport.write_cycle_ms = cycle; }
    // Encoder only owns physical field encoding. Source-schema constraints belonged to the removed
    // builder and must not make generic FieldData depend on calibration business fields.
    map.inputs.retain(|slot| slot.target.is_some());
    map.validate().map_err(|error| NodeError::Config(format!("I2C encoder map is invalid: {error}")))?;
    Ok(map)
}

fn parse_encoder_inputs(spec: &NodeSpec, map: &I2cMapDefinition) -> Result<Vec<EncoderInput>, NodeError> {
    let configured = spec.config.get("inputs").and_then(serde_json::Value::as_array);
    let rows = if let Some(rows) = configured {
        rows.iter().map(|value| {
            let row = value.as_object().ok_or_else(|| NodeError::Config("encoder input row must be an object".to_owned()))?;
            let port_id = row.get("id").and_then(serde_json::Value::as_str).filter(|value| !value.trim().is_empty()).ok_or_else(|| NodeError::Config("encoder input requires id".to_owned()))?;
            let datum_name = row.get("name").and_then(serde_json::Value::as_str).filter(|value| !value.trim().is_empty()).ok_or_else(|| NodeError::Config("encoder input requires name".to_owned()))?;
            let required = row.get("required").and_then(serde_json::Value::as_bool).unwrap_or(true);
            let slot = map.inputs.iter().find(|slot| slot.name == datum_name).ok_or_else(|| NodeError::Config(format!("encoder input `{port_id}` names an undeclared field `{datum_name}`")))?;
            let target = slot.target.as_ref().ok_or_else(|| NodeError::Config(format!("encoder input `{port_id}` names a non-storage map field `{datum_name}`")))?;
            let offset = row.get("offset").and_then(serde_json::Value::as_u64).and_then(|value| u16::try_from(value).ok()).ok_or_else(|| NodeError::Config(format!("encoder input `{port_id}` requires u16 offset")))?;
            let byte_len = row.get("byteLength").and_then(serde_json::Value::as_u64).and_then(|value| u16::try_from(value).ok()).ok_or_else(|| NodeError::Config(format!("encoder input `{port_id}` requires u16 byteLength")))?;
            let encoding = row.get("encoding").and_then(serde_json::Value::as_str).ok_or_else(|| NodeError::Config(format!("encoder input `{port_id}` requires encoding")))?;
            if offset != target.offset || byte_len != target.byte_len || normalize_encoding(encoding) != normalize_encoding(&format!("{:?}", target.encoding)) {
                return Err(NodeError::Config(format!("encoder input `{port_id}` storage layout does not match map field `{datum_name}`")));
            }
            Ok(EncoderInput { port_id: port_id.to_owned(), datum_name: datum_name.to_owned(), required })
        }).collect::<Result<Vec<_>, NodeError>>()?
    } else {
        map.inputs.iter().map(|slot| EncoderInput { port_id: slot.name.clone(), datum_name: slot.name.clone(), required: slot.required }).collect()
    };
    if rows.is_empty() { return Err(NodeError::Config("i2cFieldEncoder requires at least one input".to_owned())); }
    let mut ids = HashSet::new();
    for row in &rows {
        if !ids.insert(&row.port_id) || !map.inputs.iter().any(|slot| slot.name == row.datum_name) { return Err(NodeError::Config(format!("encoder input `{}` is duplicate or not declared by map", row.port_id))); }
        let port = spec.inputs.iter().find(|port| port.id == row.port_id).ok_or_else(|| NodeError::Config(format!("encoder input `{}` has no graph port", row.port_id)))?;
        if port.kind != FIELD_PORT_KIND { return Err(NodeError::Config(format!("encoder input `{}` must use `{FIELD_PORT_KIND}`", row.port_id))); }
    }
    if spec.inputs.len() != rows.len() { return Err(NodeError::Config("encoder graph inputs must exactly match configured inputs".to_owned())); }
    Ok(rows)
}

fn normalize_encoding(value: &str) -> String {
    value.trim().chars().flat_map(char::to_lowercase).filter(|character| *character != '-' && *character != '_').collect()
}

fn parse_operation(config: &serde_json::Value) -> Result<TaskOperation, NodeError> {
    match config.get("operation").and_then(serde_json::Value::as_str).unwrap_or("guarded_write") {
        "read" => Ok(TaskOperation::Read), "guarded_write" => Ok(TaskOperation::GuardedWrite),
        value => Err(NodeError::Config(format!("i2cFieldEncoder operation must be read or guarded_write, got `{value}`"))),
    }
}

fn validate_encoder_ports(spec: &NodeSpec) -> Result<(), NodeError> {
    let task = spec.outputs.iter().find(|port| port.id == TASK_PORT).ok_or_else(|| {
        NodeError::Config("i2cFieldEncoder requires task output".to_owned())
    })?;
    if task.kind != PACKET_PORT_KIND || spec.outputs.len() != 1 {
        return Err(NodeError::Config("i2cFieldEncoder has exactly one data.packet.v1 task output".to_owned()));
    }
    Ok(())
}

fn validate_executor_ports(spec: &NodeSpec) -> Result<(), NodeError> {
    let expect = |id: &str, kind: &str| spec.inputs.iter().find(|port| port.id == id).filter(|port| port.kind == kind).ok_or_else(|| NodeError::Config(format!("i2cTaskExecutor requires `{id}` input of kind `{kind}`")));
    let _ = expect(TASK_PORT, PACKET_PORT_KIND)?;
    let _ = expect(CONNECTION_PORT, "ssh.connection.v1")?;
    if spec.inputs.len() != 2 { return Err(NodeError::Config("i2cTaskExecutor accepts exactly task and connection".to_owned())); }
    Ok(())
}

fn ssh_target(spec: &NodeSpec) -> Result<ControlTargetSpec, NodeError> {
    let host = required_text(spec, "host")?;
    let port = config_u16(spec, "port")?.unwrap_or(22);
    let username = config_text(spec, "username").unwrap_or_else(|| "root".to_owned());
    Ok(ControlTargetSpec { host, port, username, expected_host_key: None })
}
fn config_text(spec: &NodeSpec, key: &str) -> Option<String> { spec.config.get(key).and_then(serde_json::Value::as_str).map(str::trim).filter(|value| !value.is_empty()).map(ToOwned::to_owned) }
fn config_yaml(spec: &NodeSpec, key: &str) -> Option<String> { spec.config.get(key).and_then(serde_json::Value::as_str).filter(|value| !value.trim().is_empty()).map(ToOwned::to_owned) }
fn required_text(spec: &NodeSpec, key: &str) -> Result<String, NodeError> { config_text(spec, key).ok_or_else(|| NodeError::Precondition(format!("{} config `{key}` is required", spec.kind))) }
fn config_u16(spec: &NodeSpec, key: &str) -> Result<Option<u16>, NodeError> { match spec.config.get(key) { None => Ok(None), Some(value) => u16::try_from(value.as_u64().ok_or_else(|| NodeError::Config(format!("config `{key}` must be u16")))?).map(Some).map_err(|_| NodeError::Config(format!("config `{key}` must be u16"))) } }
fn config_u32(spec: &NodeSpec, key: &str) -> Result<Option<u32>, NodeError> { match spec.config.get(key) { None => Ok(None), Some(value) => u32::try_from(value.as_u64().ok_or_else(|| NodeError::Config(format!("config `{key}` must be u32")))?).map(Some).map_err(|_| NodeError::Config(format!("config `{key}` must be u32"))) } }
fn remote_control() -> Result<RemoteOperationControl, NodeError> { RemoteOperationControl::new(RemoteTimeouts::default(), DumpCancellation::default()).map_err(|error| NodeError::Execution(error.to_string())) }
fn sha256_hex(bytes: &[u8]) -> String { let mut output = String::from("sha256:"); for byte in Sha256::digest(bytes) { use std::fmt::Write as _; let _ = write!(output, "{byte:02x}"); } output }

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> I2cTaskPacket {
        let map = builtin_i2c_map("yg-stereo-p24c64g-v1").expect("builtin map");
        let target = I2cTaskTarget { bus: 0, address: 0x50, address_width_bytes: 2, page_size_bytes: 32, write_cycle_ms: 5 };
        let image = vec![0_u8; usize::from(map.image_bytes)];
        I2cTaskPacket {
            schema: TASK_SCHEMA.to_owned(), operation: TaskOperation::GuardedWrite,
            map_id: map.id.clone(), map_digest: "sha256:map".to_owned(), target: target.clone(),
            read_ranges: vec![I2cReadRange { offset: 0, byte_len: map.image_bytes }],
            validation: I2cMapValidationContract::from_map(&map),
            pages: vec![I2cPageWrite { offset: 0, bytes: image.clone(), settle_ms: target.write_cycle_ms }],
            final_image: image, verify_after_write: true, final_image_digest: String::new(),
        }.with_digest()
    }

    #[test]
    fn task_json_rejects_digest_mismatch_and_reconstructs_sealed_requests() {
        let task = task();
        let value = serde_json::to_value(&task).expect("task JSON");
        let parsed = parse_task(&value).expect("valid task");
        assert!(parsed.read_request().is_compiled());
        assert!(parsed.write_request().is_compiled());
        let mut malformed = value;
        malformed["finalImageDigest"] = serde_json::json!("sha256:bad");
        assert!(parse_task(&malformed).is_err());
    }

    #[test]
    fn task_json_rejects_unknown_fields() {
        let mut value = serde_json::to_value(task()).expect("task JSON");
        value["mapYaml"] = serde_json::json!("not executor input");
        assert!(parse_task(&value).is_err());
    }
}
