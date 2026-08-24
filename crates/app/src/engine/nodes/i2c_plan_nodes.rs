//! 原子 I²C read/write 工作流节点。
//!
//! `I2cTaskBuilder` 现在直接消费结构化标定包、SNID typed field 和 SSH 会话，用户只需要触发
//! Read 或 Write。写入仍由目标端单个 guarded helper 请求持锁完成逐页写入、精确读回和最终校验；
//! 本模块不提供 inspect/approval/write 三段式图节点，也不保存 credential。

use std::{collections::BTreeMap, sync::Arc};

use camera_toolbox_core::{Datum, I2cMapDefinition, StructuredPacket, TypedValue, builtin_i2c_map};
use sha2::{Digest, Sha256};

use crate::engine::{
    DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState,
    NodeSpec, TypedFieldSource,
};
use crate::platform::{
    ControlTargetSpec, DumpCancellation, I2cMapValidationContract, I2cPageWrite, I2cReadRange,
    I2cReadRequest, I2cTaskTarget, I2cWriteRequest, RemoteOperationControl, RemoteTimeouts,
    SshConnection,
};

#[cfg(test)]
use camera_toolbox_core::YG_STEREO_P24C64G_FLAG;
#[cfg(test)]
const YG_MAP_ID: &str = "yg-stereo-p24c64g-v1";
#[cfg(test)]
const YG_MODEL: &str = "pinhole.rational-thin-prism.v1";
#[cfg(test)]
const YG_IMAGE_BYTES: u16 = 0x134;

/// SSH source：在显式 Connect 后建立不含密钥材料的运行时句柄。
pub struct SshConnectionFactory;

impl NodeFactory for SshConnectionFactory {
    fn kind(&self) -> &'static str {
        "sshConnection"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(SshConnectionNode {
            spec,
            connection: None,
        }))
    }
}

pub struct SshConnectionNode {
    spec: NodeSpec,
    connection: Option<Arc<SshConnection>>,
}

impl SshConnectionNode {
    fn target(&self) -> Result<ControlTargetSpec, NodeError> {
        ssh_target(&self.spec)
    }

    fn credential_ref(&self) -> Result<String, NodeError> {
        let credential_ref = required_text(&self.spec, "credentialRef")?;
        if !credential_ref.starts_with("session:") {
            return Err(NodeError::Precondition(
                "sshConnection credentialRef must be a process-local session reference".to_owned(),
            ));
        }
        Ok(credential_ref)
    }

    fn connect(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let target = self.target()?;
        let credential_ref = self.credential_ref()?;
        if self.connection.is_some() {
            self.revoke(rt, "SSH connection revoked before reconnect")?;
        }
        let connection = rt
            .services()
            .ssh_connection_service()?
            .connect(&target, &credential_ref, remote_control()?)
            .map_err(NodeError::Execution)?;
        if connection.id().trim().is_empty() {
            return Err(NodeError::Execution(
                "SSH connection service returned an empty connection id".to_owned(),
            ));
        }
        let connection = Arc::new(connection);
        rt.emit(
            "connection",
            DataPacket::SshConnection(Arc::clone(&connection)),
        )?;
        self.connection = Some(connection);
        rt.report_state(NodeRuntimeState::Ready, "SSH connection established");
        Ok(())
    }

    fn revoke(&mut self, rt: &mut NodeRuntime, state_message: &str) -> Result<(), NodeError> {
        let Some(connection) = self.connection.as_ref() else {
            rt.report_state(NodeRuntimeState::Idle, state_message);
            return Ok(());
        };
        rt.services()
            .ssh_connection_service()?
            .revoke(connection, remote_control()?)
            .map_err(NodeError::Execution)?;
        self.connection = None;
        rt.report_state(NodeRuntimeState::Idle, state_message);
        Ok(())
    }

    fn update_config(
        &mut self,
        config: serde_json::Value,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let candidate = NodeSpec {
            config,
            ..self.spec.clone()
        };
        validate_ssh_config(&candidate)?;
        if self.connection.is_some() {
            self.revoke(rt, "SSH connection revoked after configuration change")?;
        }
        self.spec = candidate;
        rt.report_state(
            NodeRuntimeState::Idle,
            "SSH configuration updated; reconnect required",
        );
        Ok(())
    }
}

impl NodeInstance for SshConnectionNode {
    fn kind(&self) -> &'static str {
        "sshConnection"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(
            NodeRuntimeState::Idle,
            "connect to establish an in-memory SSH session",
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
            NodeAction::Connect => self.connect(rt),
            NodeAction::Disconnect => self.revoke(rt, "SSH connection revoked"),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_config_update(
        &mut self,
        config: serde_json::Value,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        self.update_config(config, rt)
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.revoke(rt, "SSH connection revoked while stopping")
    }
}

pub struct I2cTaskBuilderFactory;

impl NodeFactory for I2cTaskBuilderFactory {
    fn kind(&self) -> &'static str {
        "i2cTaskBuilder"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(I2cTaskBuilderNode::new(spec)?))
    }
}

#[derive(Clone)]
struct BuilderInput {
    datum: Arc<Datum>,
    source: Arc<TypedFieldSource>,
}

pub struct I2cTaskBuilderNode {
    map: I2cMapDefinition,
    packet: Option<Arc<StructuredPacket>>,
    serial: Option<BuilderInput>,
    connection: Option<Arc<SshConnection>>,
    read_request: Option<Arc<I2cReadRequest>>,
    write_request: Option<Arc<I2cWriteRequest>>,
}

impl I2cTaskBuilderNode {
    fn new(spec: NodeSpec) -> Result<Self, NodeError> {
        let map = compile_builder_map(&spec)?;
        validate_builder_read_before_layout(&map)?;
        Ok(Self {
            map,
            packet: None,
            serial: None,
            connection: None,
            read_request: None,
            write_request: None,
        })
    }

    fn build(&self) -> Result<(I2cReadRequest, I2cWriteRequest), NodeError> {
        let packet = self.packet.as_ref().ok_or_else(|| {
            NodeError::Precondition("i2cTaskBuilder requires a structured calibration packet".to_owned())
        })?;
        let model_id = packet_model_id(packet)?;
        self.map
            .validate_source(&packet.schema, &model_id)
            .map_err(|error| {
                NodeError::Precondition(format!(
                    "I2C map `{}` rejects structured packet source: {error}",
                    self.map.id
                ))
            })?;

        let by_name = packet
            .fields
            .iter()
            .map(|datum| (datum.name.as_str(), datum))
            .collect::<BTreeMap<_, _>>();
        let mut inputs = Vec::with_capacity(self.map.inputs.len());
        for slot in &self.map.inputs {
            let datum = if slot.name == "serial.number" {
                let serial = self.serial.as_ref().ok_or_else(|| {
                    NodeError::Precondition(
                        "i2cTaskBuilder requires serial.number before read/write".to_owned(),
                    )
                })?;
                validate_map_source(&self.map, &serial.source, &slot.name)?;
                serial.datum.as_ref()
            } else {
                by_name.get(slot.name.as_str()).copied().ok_or_else(|| {
                    NodeError::Precondition(format!(
                        "structured packet is missing I2C map input `{}`",
                        slot.name
                    ))
                })?
            };
            validate_map_slot(datum, slot)?;
            inputs.push(datum.clone());
        }

        let image = self.map.encode(&inputs).map_err(|error| {
            NodeError::Precondition(format!(
                "I2C map `{}` rejected inputs: {error}",
                self.map.id
            ))
        })?;
        let target = I2cTaskTarget {
            bus: self.map.target.bus,
            address: u16::from(self.map.target.transport.i2c_address),
            address_width_bytes: self.map.target.transport.address_width_bits / 8,
            page_size_bytes: self.map.target.transport.page_size_bytes,
            write_cycle_ms: self.map.target.transport.write_cycle_ms,
        };
        let map_digest = sha256_hex(format!("{:?}", self.map).as_bytes());
        let read_ranges = self
            .map
            .read_before
            .ranges
            .iter()
            .map(|range| I2cReadRange {
                offset: range.offset,
                byte_len: range.byte_len,
            })
            .collect::<Vec<_>>();
        let validation = I2cMapValidationContract::from_map(&self.map);
        let pages = image
            .pages
            .into_iter()
            .map(|page| I2cPageWrite {
                offset: page.offset,
                bytes: page.bytes,
                settle_ms: target.write_cycle_ms,
            })
            .collect::<Vec<_>>();
        let read = I2cReadRequest::new(
            self.map.id.clone(),
            map_digest.clone(),
            target.clone(),
            read_ranges.clone(),
            validation.clone(),
        );
        let write = I2cWriteRequest::new(
            self.map.id.clone(),
            map_digest,
            target,
            read_ranges,
            validation,
            pages,
            image.bytes,
            self.map.readback.required,
        );
        Ok((read, write))
    }

    fn rebuild_if_ready(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        if self.packet.is_none() || self.map.inputs.iter().any(|slot| slot.name == "serial.number") && self.serial.is_none() {
            return Ok(());
        }
        let (read, write) = self.build()?;
        self.read_request = Some(Arc::new(read));
        self.write_request = Some(Arc::new(write));
        rt.report_state(
            NodeRuntimeState::Ready,
            "I2C read/write requests are ready; use Read or Write",
        );
        Ok(())
    }

    fn read(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let connection = self.connection.as_ref().ok_or_else(|| {
            NodeError::Precondition("i2cTaskBuilder read requires SSH connection".to_owned())
        })?;
        let request = self.read_request.as_ref().ok_or_else(|| {
            NodeError::Precondition("i2cTaskBuilder read requires compiled map inputs".to_owned())
        })?;
        if !request.is_compiled() {
            return Err(NodeError::Precondition(
                "i2cTaskBuilder read request was mutated after compilation".to_owned(),
            ));
        }
        let report = rt
            .services()
            .i2c_task_executor()?
            .read(connection, request, remote_control()?)
            .map_err(NodeError::Execution)?;
        let valid = report.valid;
        let error = report.error.clone();
        rt.emit("readReport", DataPacket::I2cReadReport(Arc::new(report)))?;
        if valid {
            rt.report_state(NodeRuntimeState::Ready, "I2C read completed and map state is valid");
        } else {
            rt.report_state(
                NodeRuntimeState::Warning,
                format!("I2C read completed with validation warning: {}", error.unwrap_or_else(|| "unknown".to_owned())),
            );
        }
        Ok(())
    }

    fn write(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let connection = self.connection.as_ref().ok_or_else(|| {
            NodeError::Precondition("i2cTaskBuilder write requires SSH connection".to_owned())
        })?;
        let request = self.write_request.as_ref().ok_or_else(|| {
            NodeError::Precondition("i2cTaskBuilder write requires compiled map inputs".to_owned())
        })?;
        if !request.is_compiled() {
            return Err(NodeError::Precondition(
                "i2cTaskBuilder write request was mutated after compilation".to_owned(),
            ));
        }
        let report = rt
            .services()
            .i2c_task_executor()?
            .write(connection, request, remote_control()?)
            .map_err(NodeError::Execution)?;
        let final_verified = report.final_verified;
        let error = report.error.clone();
        rt.emit("report", DataPacket::I2cExecutionReport(Arc::new(report)))?;
        if final_verified {
            rt.report_state(
                NodeRuntimeState::Ready,
                "I2C write completed; pages read back and final image verified",
            );
        } else {
            rt.report_state(
                NodeRuntimeState::Error,
                format!("I2C write halted: {}; no rollback was attempted", error.unwrap_or_else(|| "unknown".to_owned())),
            );
        }
        Ok(())
    }
}

impl NodeInstance for I2cTaskBuilderNode {
    fn kind(&self) -> &'static str {
        "i2cTaskBuilder"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(
            NodeRuntimeState::Idle,
            "waiting for structured packet, serial number, and SSH connection",
        );
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        match (port, packet) {
            ("packet", DataPacket::StructuredPacket(value)) => {
                self.packet = Some(value);
                self.read_request = None;
                self.write_request = None;
            }
            ("serial.number", DataPacket::TypedField { datum, source, .. }) => {
                let slot = self
                    .map
                    .inputs
                    .iter()
                    .find(|slot| slot.name == "serial.number")
                    .ok_or_else(|| {
                        NodeError::Precondition(
                            "i2cTaskBuilder received serial.number but selected map has no serial slot"
                                .to_owned(),
                        )
                    })?;
                validate_map_slot(&datum, slot)?;
                validate_map_source(&self.map, &source, "serial.number")?;
                self.serial = Some(BuilderInput { datum, source });
                self.read_request = None;
                self.write_request = None;
            }
            ("connection", DataPacket::SshConnection(value)) => self.connection = Some(value),
            _ => {
                return Err(NodeError::Precondition(
                    "i2cTaskBuilder accepts packet, serial.number, and connection".to_owned(),
                ));
            }
        };
        self.rebuild_if_ready(rt)
    }

    fn on_config_update(
        &mut self,
        config: serde_json::Value,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let candidate_spec = NodeSpec {
            config,
            ..NodeSpec {
                id: String::new(),
                kind: "i2cTaskBuilder".to_owned(),
                title: String::new(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                config: serde_json::Value::Null,
            }
        };
        let candidate = compile_builder_map(&candidate_spec)?;
        validate_builder_read_before_layout(&candidate)?;
        self.map = candidate;
        self.packet = None;
        self.serial = None;
        self.read_request = None;
        self.write_request = None;
        rt.report_state(
            NodeRuntimeState::Idle,
            "I2C map configuration updated; send packet and serial again",
        );
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Custom { name, .. } if name == "read" => self.read(rt),
            NodeAction::Custom { name, .. } if name == "write" => self.write(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.packet = None;
        self.serial = None;
        self.connection = None;
        self.read_request = None;
        self.write_request = None;
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

fn packet_model_id(packet: &StructuredPacket) -> Result<String, NodeError> {
    packet
        .fields
        .iter()
        .find(|datum| datum.name == "camera.model.id")
        .and_then(|datum| match &datum.value {
            TypedValue::Str(value) => Some(value.clone()),
            _ => None,
        })
        .ok_or_else(|| {
            NodeError::Precondition(
                "structured packet must contain string camera.model.id for I2C map source validation"
                    .to_owned(),
            )
        })
}

/// 编译 builder 配置中的唯一 map 来源。自定义 YAML 的 core 诊断会保留精确行列位置。
fn compile_builder_map(spec: &NodeSpec) -> Result<I2cMapDefinition, NodeError> {
    let mode = config_text(spec, "mapMode").ok_or_else(|| {
        NodeError::Config(
            "i2cTaskBuilder config `mapMode` must be `builtin` or `custom`".to_owned(),
        )
    })?;
    let mut map = match mode.as_str() {
        "builtin" => {
            let map_id = config_text(spec, "mapId").ok_or_else(|| {
                NodeError::Config(
                    "i2cTaskBuilder builtin map requires non-empty `mapId`".to_owned(),
                )
            })?;
            builtin_i2c_map(&map_id)
                .ok_or_else(|| NodeError::Config(format!("unsupported I2C map `{map_id}`")))?
        }
        "custom" => {
            let yaml = config_yaml(spec, "mapYaml").ok_or_else(|| {
                NodeError::Config(
                    "i2cTaskBuilder custom map requires non-empty `mapYaml`".to_owned(),
                )
            })?;
            I2cMapDefinition::from_yaml(&yaml).map_err(|error| {
                NodeError::Config(format!(
                    "i2cTaskBuilder custom map compilation failed: {error}"
                ))
            })?
        }
        _ => {
            return Err(NodeError::Config(format!(
                "i2cTaskBuilder config `mapMode` must be `builtin` or `custom`, got `{mode}`"
            )));
        }
    };
    map.target.bus = config_u32(spec, "bus")?;
    Ok(map)
}

fn validate_builder_read_before_layout(map: &I2cMapDefinition) -> Result<(), NodeError> {
    let mut expected_offset = 0_u32;
    for (index, range) in map.read_before.ranges.iter().enumerate() {
        if range.byte_len == 0 || u32::from(range.offset) != expected_offset {
            return Err(NodeError::Config(format!(
                "i2cTaskBuilder map `{}` readBefore.ranges must be an ordered contiguous cover of 0..{}; range {index} starts at {}, expected {}",
                map.id, map.image_bytes, range.offset, expected_offset
            )));
        }
        expected_offset = expected_offset
            .checked_add(u32::from(range.byte_len))
            .ok_or_else(|| NodeError::Config("readBefore range length overflow".to_owned()))?;
    }
    if expected_offset != u32::from(map.image_bytes) {
        return Err(NodeError::Config(format!(
            "i2cTaskBuilder map `{}` readBefore.ranges cover 0..{}, expected 0..{}",
            map.id, expected_offset, map.image_bytes
        )));
    }
    Ok(())
}

fn validate_map_source(
    map: &I2cMapDefinition,
    source: &TypedFieldSource,
    slot_name: &str,
) -> Result<(), NodeError> {
    let Some(model_id) = source.model_id.as_deref() else {
        return Err(NodeError::Precondition(format!(
            "I2C map `{}` rejects source for input `{slot_name}`: camera model id is absent",
            map.id
        )));
    };
    map.validate_source(&source.schema, model_id)
        .map_err(|error| {
            NodeError::Precondition(format!(
                "I2C map `{}` rejects source for input `{slot_name}`: {error}",
                map.id
            ))
        })
}

fn validate_map_slot(
    field: &Datum,
    slot: &camera_toolbox_core::LogicalInputSlot,
) -> Result<(), NodeError> {
    if field.name != slot.name || field.primitive_type() != slot.primitive_type {
        return Err(NodeError::Precondition(format!(
            "field `{}` does not match map input `{}` and its primitive type",
            field.name, slot.name
        )));
    }
    if field.semantic_type.as_deref() != slot.semantic_type.as_deref()
        || field.unit.as_deref() != slot.unit.as_deref()
    {
        return Err(NodeError::Precondition(format!(
            "field `{}` does not match the map semantic type or unit contract",
            slot.name
        )));
    }
    Ok(())
}

fn ssh_target(spec: &NodeSpec) -> Result<ControlTargetSpec, NodeError> {
    let config = spec
        .config
        .as_object()
        .ok_or_else(|| NodeError::Config("sshConnection config must be an object".to_owned()))?;
    let host = strict_required_text(config, "host")?;
    let port = config_u16(spec, "port", 22)?;
    let username = strict_optional_text(config, "username")?.unwrap_or_else(|| "root".to_owned());
    Ok(ControlTargetSpec {
        host,
        port,
        username,
        expected_host_key: None,
    })
}

fn validate_ssh_config(spec: &NodeSpec) -> Result<(), NodeError> {
    let _ = ssh_target(spec)?;
    let credential_ref = required_text(spec, "credentialRef")?;
    if !credential_ref.starts_with("session:") {
        return Err(NodeError::Config(
            "sshConnection credentialRef must be a process-local session reference".to_owned(),
        ));
    }
    Ok(())
}

fn strict_required_text(
    config: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<String, NodeError> {
    strict_optional_text(config, key)?
        .ok_or_else(|| NodeError::Config(format!("sshConnection config `{key}` is required")))
}

fn strict_optional_text(
    config: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<String>, NodeError> {
    let Some(value) = config.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| NodeError::Config(format!("sshConnection config `{key}` must be text")))?
        .trim();
    if value.is_empty() {
        return Err(NodeError::Config(format!(
            "sshConnection config `{key}` must not be blank"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(NodeError::Config(format!(
            "sshConnection config `{key}` must not contain control characters"
        )));
    }
    Ok(Some(value.to_owned()))
}

fn config_text(spec: &NodeSpec, key: &str) -> Option<String> {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn config_yaml(spec: &NodeSpec, key: &str) -> Option<String> {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn required_text(spec: &NodeSpec, key: &str) -> Result<String, NodeError> {
    config_text(spec, key)
        .ok_or_else(|| NodeError::Precondition(format!("{} config `{key}` is required", spec.kind)))
}

fn config_u16(spec: &NodeSpec, key: &str, fallback: u16) -> Result<u16, NodeError> {
    let Some(value) = spec.config.get(key) else {
        return Ok(fallback);
    };
    let value = value
        .as_u64()
        .ok_or_else(|| NodeError::Config(format!("config `{key}` must be u16")))?;
    u16::try_from(value).map_err(|_| NodeError::Config(format!("config `{key}` must be u16")))
}

fn config_u32(spec: &NodeSpec, key: &str) -> Result<u32, NodeError> {
    let value = spec
        .config
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            NodeError::Config(format!(
                "i2cTaskBuilder config `{key}` must be an explicit u32"
            ))
        })?;
    u32::try_from(value).map_err(|_| {
        NodeError::Config(format!(
            "i2cTaskBuilder config `{key}` must be an explicit u32"
        ))
    })
}

fn remote_control() -> Result<RemoteOperationControl, NodeError> {
    RemoteOperationControl::new(RemoteTimeouts::default(), DumpCancellation::default())
        .map_err(|error| NodeError::Execution(error.to_string()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(71);
    output.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_core::PacketProvenance;
    use crate::platform::{I2cExecutionReport, I2cReadReport};
    use crate::{
        engine::{
            EngineServices, NodeReporter, OutputRegistry, PortCardinality, PortSpec, SpawnContext,
        },
        platform::I2cTaskExecutor,
    };
    use parking_lot::Mutex;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    #[derive(Default)]
    struct FakeSsh {
        active: Arc<AtomicBool>,
        revoked: Mutex<Vec<String>>,
    }

    impl crate::platform::SshConnectionService for FakeSsh {
        fn connect(
            &self,
            target: &ControlTargetSpec,
            _: &str,
            _: RemoteOperationControl,
        ) -> Result<SshConnection, String> {
            self.active.store(true, Ordering::Release);
            Ok(SshConnection::new("fake-session", target.clone()))
        }

        fn revoke(
            &self,
            connection: &SshConnection,
            _: RemoteOperationControl,
        ) -> Result<(), String> {
            self.active.store(false, Ordering::Release);
            self.revoked.lock().push(connection.id().to_owned());
            Ok(())
        }
    }

    struct FakeI2c {
        read_report: Mutex<Option<I2cReadReport>>,
        write_report: Mutex<Option<I2cExecutionReport>>,
        active: Arc<AtomicBool>,
    }

    impl I2cTaskExecutor for FakeI2c {
        fn read(
            &self,
            _: &SshConnection,
            request: &I2cReadRequest,
            _: RemoteOperationControl,
        ) -> Result<I2cReadReport, String> {
            if !self.active.load(Ordering::Acquire) {
                return Err("SSH connection is not active".to_owned());
            }
            let report = self.read_report.lock().clone().unwrap_or(I2cReadReport {
                map_id: request.map_id.clone(),
                map_digest: request.map_digest.clone(),
                target: request.target.clone(),
                image_sha256: sha256_hex(&valid_image()),
                byte_len: usize::from(YG_IMAGE_BYTES),
                valid: true,
                error: None,
            });
            Ok(report)
        }

        fn write(
            &self,
            _: &SshConnection,
            request: &I2cWriteRequest,
            _: RemoteOperationControl,
        ) -> Result<I2cExecutionReport, String> {
            if !self.active.load(Ordering::Acquire) {
                return Err("SSH connection is not active".to_owned());
            }
            Ok(self.write_report
                .lock()
                .clone()
                .unwrap_or(crate::platform::I2cExecutionReport {
                before_image_sha256: sha256_hex(&valid_image()),
                pages: request
                    .pages
                    .iter()
                    .map(|page| crate::platform::I2cPageExecutionReport {
                        offset: page.offset,
                        expected: page.bytes.clone(),
                        readback: Some(page.bytes.clone()),
                        error: None,
                    })
                    .collect(),
                final_verified: true,
                error: None,
            }))
        }
    }

    fn port(id: &str, kind: &str) -> PortSpec {
        PortSpec {
            id: id.to_owned(),
            label: id.to_owned(),
            kind: kind.to_owned(),
            cardinality: PortCardinality::One,
            required: true,
        }
    }

    fn builder_spec(bus: Option<u32>) -> NodeSpec {
        let mut config = serde_json::json!({"mapMode": "builtin", "mapId": YG_MAP_ID});
        if let Some(bus) = bus {
            config["bus"] = serde_json::json!(bus);
        }
        NodeSpec {
            id: "builder".to_owned(),
            kind: "i2cTaskBuilder".to_owned(),
            title: "builder".to_owned(),
            inputs: vec![
                port("packet", "data.structured.packet.v1"),
                port("serial.number", "data.field.str.v1"),
                port("connection", "ssh.connection.v1"),
            ],
            outputs: vec![
                port("readReport", "i2c.read-report.v1"),
                port("report", "i2c.execution-report.v1"),
            ],
            config,
        }
    }

    fn action_spec() -> NodeSpec {
        NodeSpec {
            id: "ssh".to_owned(),
            kind: "sshConnection".to_owned(),
            title: "ssh".to_owned(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            config: serde_json::json!({"host":"fake", "credentialRef":"session:fake"}),
        }
    }

    fn source(schema: &str, model_id: &str) -> Arc<TypedFieldSource> {
        Arc::new(TypedFieldSource::new(
            schema,
            PacketProvenance {
                source_port: Some("test.serial".to_owned()),
                ..PacketProvenance::default()
            },
            Some(model_id.to_owned()),
        ))
    }

    fn serial_packet() -> DataPacket {
        DataPacket::TypedField {
            datum: Arc::new(
                Datum::new(
                    "serial.number",
                    TypedValue::Str("2T23326AV4ZZ00".to_owned()),
                )
                .with_semantic_type("device.serial-number"),
            ),
            generation: 1,
            source: source("camera-toolbox.calib.solution.v1", YG_MODEL),
        }
    }

    fn solution_packet() -> Arc<StructuredPacket> {
        let mut fields = vec![
            Datum::new("camera.model.id", TypedValue::Str(YG_MODEL.to_owned()))
                .with_semantic_type("camera.model-id"),
            Datum::new("camera.image.width", TypedValue::U32(1920))
                .with_unit("px")
                .with_semantic_type("image.width"),
            Datum::new("camera.image.height", TypedValue::U32(1080))
                .with_unit("px")
                .with_semantic_type("image.height"),
        ];
        for (name, semantic) in [
            ("camera.intrinsics.fx", "camera.focal-length"),
            ("camera.intrinsics.fy", "camera.focal-length"),
            ("camera.intrinsics.cx", "camera.principal-point"),
            ("camera.intrinsics.cy", "camera.principal-point"),
        ] {
            fields.push(
                Datum::new(name, TypedValue::F64(1.0))
                    .with_unit("px")
                    .with_semantic_type(semantic),
            );
        }
        for name in [
            "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
        ] {
            fields.push(
                Datum::new(format!("distortion.{name}"), TypedValue::F64(0.0))
                    .with_unit("dimensionless")
                    .with_semantic_type("camera.distortion-coefficient"),
            );
        }
        Arc::new(
            StructuredPacket::new(
                "camera-toolbox.calib.solution.v1",
                PacketProvenance::default(),
                fields,
            )
            .unwrap(),
        )
    }

    fn valid_image() -> Vec<u8> {
        let mut image = vec![0; usize::from(YG_IMAGE_BYTES)];
        image[..YG_STEREO_P24C64G_FLAG.len()].copy_from_slice(&YG_STEREO_P24C64G_FLAG);
        let serial = b"2T23326AV4ZZ00";
        image[0x125..0x133].copy_from_slice(serial);
        image[0x133] = ((serial
            .iter()
            .fold(0_u16, |sum, byte| sum + u16::from(*byte))
            % 0xff)
            + 1) as u8;
        image
    }

    fn connection() -> Arc<SshConnection> {
        Arc::new(SshConnection::new(
            "fake-session",
            ControlTargetSpec {
                host: "fake".to_owned(),
                port: 22,
                username: "root".to_owned(),
                expected_host_key: None,
            },
        ))
    }

    fn runtime(services: EngineServices, outputs: Arc<Mutex<Vec<DataPacket>>>) -> NodeRuntime {
        let (state, _) = mpsc::channel();
        let (events, _) = mpsc::channel();
        let mut registry = OutputRegistry::default();
        registry.set_record(Arc::new(move |packet| outputs.lock().push(packet)));
        NodeRuntime::new(SpawnContext {
            outputs: registry,
            reporter: NodeReporter::new("test".to_owned(), state, events),
            services: Arc::new(services),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        })
    }

    #[test]
    fn ssh_target_ignores_legacy_empty_host_key() {
        let mut spec = action_spec();
        spec.config
            .as_object_mut()
            .expect("SSH connection config is an object")
            .insert("expectedHostKey".to_owned(), serde_json::json!(""));

        assert_eq!(ssh_target(&spec).unwrap().expected_host_key, None);
    }

    #[test]
    fn builder_requires_explicit_bus_and_direct_packet_inputs() {
        assert!(matches!(
            I2cTaskBuilderNode::new(builder_spec(None)),
            Err(NodeError::Config(_))
        ));
        let mut node = I2cTaskBuilderNode::new(builder_spec(Some(7))).unwrap();
        let mut rt = runtime(EngineServices::default(), Arc::new(Mutex::new(Vec::new())));
        node.on_input(
            "packet",
            DataPacket::StructuredPacket(solution_packet()),
            &mut rt,
        )
        .unwrap();
        assert!(node.read_request.is_none());
        node.on_input("serial.number", serial_packet(), &mut rt).unwrap();
        assert_eq!(node.read_request.as_ref().unwrap().target.bus, 7);
        assert!(node.write_request.as_ref().unwrap().is_compiled());
    }

    #[test]
    fn builder_read_and_write_are_actions_on_the_same_node() {
        let active = Arc::new(AtomicBool::new(true));
        let service = Arc::new(FakeI2c {
            read_report: Mutex::new(None),
            write_report: Mutex::new(None),
            active,
        });
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(
            EngineServices {
                i2c_task_executor: Some(service),
                ..EngineServices::default()
            },
            Arc::clone(&outputs),
        );
        let mut node = I2cTaskBuilderNode::new(builder_spec(Some(7))).unwrap();
        node.on_input("connection", DataPacket::SshConnection(connection()), &mut rt)
            .unwrap();
        node.on_input(
            "packet",
            DataPacket::StructuredPacket(solution_packet()),
            &mut rt,
        )
        .unwrap();
        node.on_input("serial.number", serial_packet(), &mut rt).unwrap();
        node.on_action(
            NodeAction::Custom {
                name: "read".to_owned(),
                payload: serde_json::Value::Null,
            },
            &mut rt,
        )
        .unwrap();
        node.on_action(
            NodeAction::Custom {
                name: "write".to_owned(),
                payload: serde_json::Value::Null,
            },
            &mut rt,
        )
        .unwrap();
        assert!(matches!(outputs.lock()[0], DataPacket::I2cReadReport(_)));
        assert!(matches!(outputs.lock()[1], DataPacket::I2cExecutionReport(_)));
    }

    #[test]
    fn disconnect_revokes_handle_and_stale_operations_are_rejected() {
        let ssh = Arc::new(FakeSsh {
            active: Arc::new(AtomicBool::new(true)),
            revoked: Mutex::new(Vec::new()),
        });
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(
            EngineServices {
                ssh_connection_service: Some(ssh.clone()),
                ..EngineServices::default()
            },
            outputs,
        );
        let mut node = SshConnectionNode {
            spec: action_spec(),
            connection: Some(connection()),
        };
        node.on_action(NodeAction::Disconnect, &mut rt).unwrap();
        assert_eq!(ssh.revoked.lock().as_slice(), ["fake-session"]);
    }
}
