//! I²C map → inspect → approval → execution 的 app 层节点链。
//!
//! 所有破坏性 I/O 只能通过已授权的计划逐页执行；本模块不解析 shell 命令，
//! 不保存 credential，也不提供自动回滚。

use std::{collections::BTreeMap, sync::Arc};

use camera_toolbox_core::{
    ChecksumAlgorithm, Datum, I2cMapDefinition, PrimitiveType, builtin_i2c_map,
};
#[cfg(test)]
use camera_toolbox_core::{TypedValue, YG_STEREO_P24C64G_FLAG};
use sha2::{Digest, Sha256};

use crate::engine::{
    DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState,
    NodeSpec, TypedFieldSource,
};
use crate::platform::{
    ControlTargetSpec, DumpCancellation, I2cAuthorizedWritePlan, I2cCandidateWritePlan,
    I2cExecutionReport, I2cInspectPlan, I2cInspectSnapshot, I2cMapValidationContract,
    I2cPageExecutionReport, I2cPageWrite, I2cReadRange, I2cTaskTarget, RemoteOperationControl,
    RemoteTimeouts, SshConnection,
};

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
        // 先完整校验新配置，再撤销旧 binding；失败时绝不丢失可用的旧连接。
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
        // 验证候选配置（包括所有 optional 字段的类型）必须发生在任何 session 副作用前。
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

/// 将静态 typed-field 输入编译为已绑定 map 的 inspect 和候选写计划。
pub struct I2cTaskBuilderFactory;
impl NodeFactory for I2cTaskBuilderFactory {
    fn kind(&self) -> &'static str {
        "i2cTaskBuilder"
    }
    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(I2cTaskBuilderNode::new(spec)?))
    }
}

/// 静态 typed-field fan-in 的 map 编译节点。字节编码、范围、校验和和页分段均委托 core。
/// 一个 map slot 的值和它从 structured packet 继承的来源合同。
#[derive(Clone)]
struct BuilderInput {
    datum: Arc<Datum>,
    source: Arc<TypedFieldSource>,
}

pub struct I2cTaskBuilderNode {
    map: I2cMapDefinition,
    fields: BTreeMap<String, BuilderInput>,
    generation: Option<u64>,
    emitted_generation: Option<u64>,
}

impl I2cTaskBuilderNode {
    fn new(spec: NodeSpec) -> Result<Self, NodeError> {
        let map = compile_builder_map(&spec)?;
        validate_builder_interface(&spec, &map)?;
        Ok(Self {
            map,
            fields: BTreeMap::new(),
            generation: None,
            emitted_generation: None,
        })
    }

    fn build(
        &self,
        fields: &BTreeMap<String, BuilderInput>,
    ) -> Result<(I2cInspectPlan, I2cCandidateWritePlan), NodeError> {
        // 每个 typed field 的 schema/model 分别验证；provenance 仅为审计元数据，
        // 因此绝不在此引入跨字段 sourcePacketDigest 一致性门槛。
        for slot in &self.map.inputs {
            if let Some(field) = fields.get(&slot.name) {
                validate_map_source(&self.map, &field.source, &slot.name)?;
            }
        }
        let inputs = self
            .map
            .inputs
            .iter()
            .filter_map(|slot| fields.get(&slot.name))
            .map(|field| field.datum.as_ref().clone())
            .collect::<Vec<_>>();
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
        let pages = image
            .pages
            .into_iter()
            .map(|page| I2cPageWrite {
                offset: page.offset,
                bytes: page.bytes,
                settle_ms: target.write_cycle_ms,
            })
            .collect();
        let candidate = I2cCandidateWritePlan::new(
            self.map.id.clone(),
            map_digest.clone(),
            target.clone(),
            pages,
            self.map.readback.required,
        );
        let inspect = I2cInspectPlan::new(
            self.map.id.clone(),
            map_digest,
            target,
            self.map
                .read_before
                .ranges
                .iter()
                .map(|range| I2cReadRange {
                    offset: range.offset,
                    byte_len: range.byte_len,
                })
                .collect(),
            I2cMapValidationContract::from_map(&self.map),
        );
        Ok((inspect, candidate))
    }

    fn build_if_ready(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let Some(generation) = self.generation else {
            return Ok(());
        };
        if self.emitted_generation == Some(generation)
            || self
                .map
                .inputs
                .iter()
                .filter(|slot| slot.required)
                .any(|slot| !self.fields.contains_key(&slot.name))
        {
            return Ok(());
        }
        let (inspect, candidate) = self.build(&self.fields)?;
        rt.emit("inspectPlan", DataPacket::I2cInspectPlan(Arc::new(inspect)))?;
        rt.emit(
            "candidateWritePlan",
            DataPacket::I2cCandidateWritePlan(Arc::new(candidate)),
        )?;
        self.emitted_generation = Some(generation);
        rt.report_state(
            NodeRuntimeState::Ready,
            "core map compiled one complete typed-field generation; no I/O performed",
        );
        Ok(())
    }
}

impl NodeInstance for I2cTaskBuilderNode {
    fn kind(&self) -> &'static str {
        "i2cTaskBuilder"
    }
    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(
            NodeRuntimeState::Ready,
            "waiting for one complete typed-field generation",
        );
        Ok(())
    }
    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let slot = self
            .map
            .inputs
            .iter()
            .find(|slot| slot.name == port)
            .ok_or_else(|| {
                NodeError::Precondition(format!(
                    "i2cTaskBuilder received unknown field port `{port}`"
                ))
            })?;
        let DataPacket::TypedField {
            datum,
            generation,
            source,
        } = packet
        else {
            return Err(NodeError::Precondition(format!(
                "i2cTaskBuilder.{port} requires typed field input"
            )));
        };
        validate_map_slot(&datum, slot)?;
        validate_map_source(&self.map, &source, port)?;
        match self.generation {
            Some(current) if generation < current => return Ok(()),
            Some(current) if generation > current => {
                self.fields.clear();
                self.generation = Some(generation);
                self.emitted_generation = None;
            }
            None => self.generation = Some(generation),
            Some(_) => {}
        }
        self.fields
            .insert(port.to_owned(), BuilderInput { datum, source });
        self.build_if_ready(rt)
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
        // 图接口先由持久化层原子预检；运行时只接受与现有静态 slots 完全相同的替换。
        // 解析或端口校验失败时当前 map/fields 保持不变。
        let interface_spec = NodeSpec {
            id: String::new(),
            kind: "i2cTaskBuilder".to_owned(),
            title: String::new(),
            inputs: self
                .map
                .inputs
                .iter()
                .map(|slot| crate::engine::PortSpec {
                    id: slot.name.clone(),
                    label: slot.name.clone(),
                    kind: typed_field_port_kind(slot.primitive_type).to_owned(),
                    cardinality: crate::engine::PortCardinality::One,
                    required: true,
                })
                .collect(),
            outputs: Vec::new(),
            config: serde_json::Value::Null,
        };
        validate_builder_interface(&interface_spec, &candidate)?;
        self.map = candidate;
        self.fields.clear();
        self.generation = None;
        self.emitted_generation = None;
        rt.report_state(
            NodeRuntimeState::Ready,
            "I2C map configuration replaced atomically; waiting for a complete typed-field generation",
        );
        Ok(())
    }
    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }
    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.fields.clear();
        self.generation = None;
        self.emitted_generation = None;
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 只读 inspect actor：session 与编译计划齐备后产生绑定 snapshot，绝不写 EEPROM。
pub struct I2cInspectorFactory;
impl NodeFactory for I2cInspectorFactory {
    fn kind(&self) -> &'static str {
        "i2cInspector"
    }
    fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(I2cInspectorNode {
            connection: None,
            inspect: None,
        }))
    }
}
pub struct I2cInspectorNode {
    connection: Option<Arc<SshConnection>>,
    inspect: Option<Arc<I2cInspectPlan>>,
}
impl I2cInspectorNode {
    fn inspect_if_ready(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let (Some(connection), Some(plan)) = (&self.connection, &self.inspect) else {
            return Ok(());
        };
        if !plan.is_compiled() {
            return Err(NodeError::Precondition(
                "i2cInspector refuses an uncompiled inspect plan".to_owned(),
            ));
        }
        let bytes = rt
            .services()
            .i2c_task_executor()?
            .inspect(connection, plan, remote_control()?)
            .map_err(NodeError::Execution)?;
        let expected = plan
            .read_ranges
            .iter()
            .try_fold(0_usize, |total, range| {
                total.checked_add(usize::from(range.byte_len)).ok_or(())
            })
            .map_err(|_| NodeError::Execution("inspect range length overflow".to_owned()))?;
        if bytes.len() != expected {
            return Err(NodeError::Execution(format!(
                "inspect returned {} bytes, expected {expected}",
                bytes.len()
            )));
        }
        validate_map_device_state(plan, &bytes)?;
        let snapshot = Arc::new(I2cInspectSnapshot {
            connection_id: connection.id().to_owned(),
            inspect_plan: plan.as_ref().clone(),
            map_id: plan.map_id.clone(),
            map_digest: plan.map_digest.clone(),
            target: plan.target.clone(),
            before_image_sha256: sha256_hex(&bytes),
            before_image: bytes,
        });
        rt.emit("snapshot", DataPacket::I2cInspectSnapshot(snapshot))?;
        rt.report_state(
            NodeRuntimeState::Ready,
            "inspect completed; snapshot is ready for approval",
        );
        Ok(())
    }
}
impl NodeInstance for I2cInspectorNode {
    fn kind(&self) -> &'static str {
        "i2cInspector"
    }
    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(
            NodeRuntimeState::Idle,
            "waiting for SSH session and inspect plan",
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
            ("connection", DataPacket::SshConnection(value)) => self.connection = Some(value),
            ("inspectPlan", DataPacket::I2cInspectPlan(value)) => self.inspect = Some(value),
            _ => {
                return Err(NodeError::Precondition(
                    "i2cInspector received an incompatible input".to_owned(),
                ));
            }
        };
        self.inspect_if_ready(rt)
    }
    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }
    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.connection = None;
        self.inspect = None;
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 明确审批节点：只有 Trigger 才能把候选计划与已 inspect 的 session 绑定为授权计划。
pub struct I2cWriteApprovalFactory;
impl NodeFactory for I2cWriteApprovalFactory {
    fn kind(&self) -> &'static str {
        "i2cWriteApproval"
    }
    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        validate_approval_config(&spec.config)?;
        Ok(Box::new(I2cWriteApprovalNode {
            spec,
            candidate: None,
            snapshot: None,
        }))
    }
}
pub struct I2cWriteApprovalNode {
    spec: NodeSpec,
    candidate: Option<Arc<I2cCandidateWritePlan>>,
    snapshot: Option<Arc<I2cInspectSnapshot>>,
}
impl I2cWriteApprovalNode {
    fn approve(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        if self
            .spec
            .config
            .get("confirmWrite")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Err(NodeError::Precondition(
                "i2cWriteApproval requires config confirmWrite=true before explicit trigger"
                    .to_owned(),
            ));
        }
        let candidate = self.candidate.as_ref().ok_or_else(|| {
            NodeError::Precondition("i2cWriteApproval requires candidate write plan".to_owned())
        })?;
        let snapshot = self.snapshot.as_ref().ok_or_else(|| {
            NodeError::Precondition("i2cWriteApproval requires inspect snapshot".to_owned())
        })?;
        let inspect = &snapshot.inspect_plan;
        if !candidate.is_compiled()
            || !inspect.is_compiled()
            || inspect.map_id != candidate.map_id
            || inspect.map_digest != candidate.map_digest
            || inspect.target != candidate.target
            || snapshot.map_id != inspect.map_id
            || snapshot.map_digest != inspect.map_digest
            || snapshot.target != inspect.target
        {
            return Err(NodeError::Precondition(
                "inspect snapshot does not match the compiled candidate plan".to_owned(),
            ));
        }
        let authorized = I2cAuthorizedWritePlan::new(
            snapshot.connection_id.clone(),
            snapshot.before_image_sha256.clone(),
            snapshot.before_image.clone(),
            inspect.clone(),
            candidate.as_ref().clone(),
        );
        rt.emit(
            "authorizedWritePlan",
            DataPacket::I2cAuthorizedWritePlan(Arc::new(authorized)),
        )?;
        rt.report_state(NodeRuntimeState::Ready, "write plan explicitly authorized");
        Ok(())
    }
}
impl NodeInstance for I2cWriteApprovalNode {
    fn kind(&self) -> &'static str {
        "i2cWriteApproval"
    }
    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(
            NodeRuntimeState::Idle,
            "waiting for candidate plan and inspected snapshot",
        );
        Ok(())
    }
    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        match (port, packet) {
            ("candidateWritePlan", DataPacket::I2cCandidateWritePlan(value)) => {
                self.candidate = Some(value)
            }
            ("snapshot", DataPacket::I2cInspectSnapshot(value)) => self.snapshot = Some(value),
            _ => {
                return Err(NodeError::Precondition(
                    "i2cWriteApproval only accepts candidateWritePlan and snapshot".to_owned(),
                ));
            }
        };
        Ok(())
    }
    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => self.approve(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }
    fn on_config_update(
        &mut self,
        config: serde_json::Value,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        validate_approval_config(&config)?;
        self.spec.config = config;
        self.candidate = None;
        self.snapshot = None;
        rt.report_state(
            NodeRuntimeState::Idle,
            "approval configuration updated; fresh plan and snapshot required",
        );
        Ok(())
    }
    fn on_stop(&mut self, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.candidate = None;
        self.snapshot = None;
        Ok(())
    }
}

/// 计划执行节点：只消费已授权写计划；inspect 一律由 `I2cInspectorNode` 完成。
pub struct I2cTaskExecutorFactory;
impl NodeFactory for I2cTaskExecutorFactory {
    fn kind(&self) -> &'static str {
        "i2cExecutor"
    }
    fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(I2cTaskExecutorNode {
            connection: None,
            authorized: None,
        }))
    }
}
pub struct I2cTaskExecutorNode {
    connection: Option<Arc<SshConnection>>,
    authorized: Option<Arc<I2cAuthorizedWritePlan>>,
}
impl I2cTaskExecutorNode {
    fn execute_authorized(&self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let connection = self.connection.as_ref().ok_or_else(|| {
            NodeError::Precondition("i2cExecutor requires SSH connection".to_owned())
        })?;
        let authorized = self.authorized.as_ref().ok_or_else(|| {
            NodeError::Precondition("i2cExecutor requires authorized write plan".to_owned())
        })?;
        if !authorized.is_authorized()
            || authorized.connection_id != connection.id()
            || authorized.expected_before_sha256 != sha256_hex(&authorized.before_image)
        {
            return Err(NodeError::Precondition(
                "authorized write plan is forged or not bound to this session and inspect image"
                    .to_owned(),
            ));
        }
        let executor = rt.services().i2c_task_executor()?;
        if let Err(error) = executor.verify_authorized(connection, authorized, remote_control()?) {
            let report = I2cExecutionReport {
                before_image_sha256: authorized.expected_before_sha256.clone(),
                pages: Vec::new(),
                final_verified: false,
                error: Some(error.clone()),
            };
            rt.emit("report", DataPacket::I2cExecutionReport(Arc::new(report)))?;
            rt.report_state(NodeRuntimeState::Error, format!("write refused: {error}"));
            return Ok(());
        }
        let mut expected_final = authorized.before_image.clone();
        let mut pages = Vec::with_capacity(authorized.candidate.pages.len());
        let mut error = None;
        for (page_index, page) in authorized.candidate.pages.iter().enumerate() {
            if authorized.page_at(page_index) != Some(page) {
                return Err(NodeError::Precondition(format!(
                    "authorized page {page_index} is not the exact compiled page"
                )));
            }
            apply_expected_page(&mut expected_final, page)?;
            match executor.write_page(connection, authorized, page_index, page, remote_control()?) {
                Ok(readback) if readback == page.bytes => pages.push(I2cPageExecutionReport {
                    offset: page.offset,
                    expected: page.bytes.clone(),
                    readback: Some(readback),
                    error: None,
                }),
                Ok(readback) => {
                    let message =
                        format!("readback mismatch at EEPROM offset 0x{:04x}", page.offset);
                    pages.push(I2cPageExecutionReport {
                        offset: page.offset,
                        expected: page.bytes.clone(),
                        readback: Some(readback),
                        error: Some(message.clone()),
                    });
                    error = Some(message);
                    break;
                }
                Err(message) => {
                    pages.push(I2cPageExecutionReport {
                        offset: page.offset,
                        expected: page.bytes.clone(),
                        readback: None,
                        error: Some(message.clone()),
                    });
                    error = Some(message);
                    break;
                }
            }
        }
        if error.is_none() && authorized.candidate.verify_after_write {
            error = match executor.inspect(connection, &authorized.inspect_plan, remote_control()?)
            {
                Ok(actual) if actual == expected_final => {
                    validate_map_device_state(&authorized.inspect_plan, &actual)
                        .err()
                        .map(|error| error.to_string())
                }
                Ok(_) => Some("final full-range checksum and field verification failed".to_owned()),
                Err(error) => Some(format!(
                    "final full-range verification inspect failed: {error}"
                )),
            };
        }
        let report = I2cExecutionReport {
            before_image_sha256: authorized.expected_before_sha256.clone(),
            final_verified: error.is_none() && pages.len() == authorized.candidate.pages.len(),
            pages,
            error: error.clone(),
        };
        rt.emit("report", DataPacket::I2cExecutionReport(Arc::new(report)))?;
        if let Some(error) = error {
            rt.report_state(
                NodeRuntimeState::Error,
                format!("write halted: {error}; no rollback was attempted"),
            );
        } else {
            rt.report_state(NodeRuntimeState::Ready, "all pages wrote, read back, and passed final full-range checksum and field verification");
        }
        Ok(())
    }
}
impl NodeInstance for I2cTaskExecutorNode {
    fn kind(&self) -> &'static str {
        "i2cExecutor"
    }
    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(
            NodeRuntimeState::Idle,
            "waiting for SSH connection and authorized write plan",
        );
        Ok(())
    }
    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        match (port, packet) {
            ("connection", DataPacket::SshConnection(value)) => {
                self.connection = Some(value);
                self.authorized = None;
            }
            ("authorizedWritePlan", DataPacket::I2cAuthorizedWritePlan(value)) => {
                self.authorized = Some(value)
            }
            _ => {
                return Err(NodeError::Precondition(
                    "i2cExecutor only accepts connection and authorizedWritePlan".to_owned(),
                ));
            }
        };
        Ok(())
    }
    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => self.execute_authorized(rt),
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }
    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.connection = None;
        self.authorized = None;
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
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
    validate_builder_read_before_layout(&map)?;
    Ok(map)
}

/// 当前 app/executor 接口传递的是 SSH 读取后的连续 bytes；因此 map 必须声明完整、
/// 有序且无空洞的 `0..image_bytes` read-before 覆盖，避免把稀疏区间误当完整镜像。
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
            .ok_or_else(|| {
                NodeError::Config(format!(
                    "i2cTaskBuilder map `{}` readBefore range {index} overflows",
                    map.id
                ))
            })?;
    }
    if expected_offset != u32::from(map.image_bytes) {
        return Err(NodeError::Config(format!(
            "i2cTaskBuilder map `{}` readBefore.ranges must cover exactly 0..{}; ended at {}",
            map.id, map.image_bytes, expected_offset
        )));
    }
    Ok(())
}

fn validate_builder_interface(spec: &NodeSpec, map: &I2cMapDefinition) -> Result<(), NodeError> {
    if spec.inputs.len() != map.inputs.len() {
        return Err(NodeError::Config(format!(
            "i2cTaskBuilder requires exactly {} static typed-field inputs for map `{}`",
            map.inputs.len(),
            map.id
        )));
    }
    for slot in &map.inputs {
        let port = spec
            .inputs
            .iter()
            .find(|port| port.id == slot.name)
            .ok_or_else(|| {
                NodeError::Config(format!("i2cTaskBuilder requires input `{}`", slot.name))
            })?;
        let expected = typed_field_port_kind(slot.primitive_type);
        if port.kind != expected {
            return Err(NodeError::Config(format!(
                "i2cTaskBuilder input `{}` must use `{expected}`, got `{}`",
                slot.name, port.kind
            )));
        }
    }
    if spec
        .inputs
        .iter()
        .any(|port| !map.inputs.iter().any(|slot| slot.name == port.id))
    {
        return Err(NodeError::Config(
            "i2cTaskBuilder has an unknown input port".to_owned(),
        ));
    }
    Ok(())
}

/// 执行 map 的来源 allowlist；每一个输入独立判断，不比较其它字段的 provenance。
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

fn typed_field_port_kind(primitive_type: PrimitiveType) -> &'static str {
    match primitive_type {
        PrimitiveType::Bool => "data.field.bool.v1",
        PrimitiveType::U8 => "data.field.u8.v1",
        PrimitiveType::I8 => "data.field.i8.v1",
        PrimitiveType::U16 => "data.field.u16.v1",
        PrimitiveType::I16 => "data.field.i16.v1",
        PrimitiveType::U32 => "data.field.u32.v1",
        PrimitiveType::I32 => "data.field.i32.v1",
        PrimitiveType::U64 => "data.field.u64.v1",
        PrimitiveType::I64 => "data.field.i64.v1",
        PrimitiveType::F32 => "data.field.f32.v1",
        PrimitiveType::F64 => "data.field.f64.v1",
        PrimitiveType::Str => "data.field.str.v1",
        PrimitiveType::Bytes => "data.field.bytes.v1",
    }
}

fn ssh_target(spec: &NodeSpec) -> Result<ControlTargetSpec, NodeError> {
    let config = spec
        .config
        .as_object()
        .ok_or_else(|| NodeError::Config("sshConnection config must be an object".to_owned()))?;
    let host = strict_required_text(config, "host")?;
    let port = config_u16(spec, "port", 22)?;
    let username = strict_optional_text(config, "username")?.unwrap_or_else(|| "root".to_owned());
    let expected_host_key = strict_optional_text(config, "expectedHostKey")?;
    Ok(ControlTargetSpec {
        host,
        port,
        username,
        expected_host_key,
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

fn validate_approval_config(config: &serde_json::Value) -> Result<(), NodeError> {
    let config = config
        .as_object()
        .ok_or_else(|| NodeError::Config("i2cWriteApproval config must be an object".to_owned()))?;
    if !matches!(config.get("confirmWrite"), Some(serde_json::Value::Bool(_))) {
        return Err(NodeError::Config(
            "i2cWriteApproval config requires boolean confirmWrite".to_owned(),
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
/// YAML 是编辑器源文本；只用 trim 判断空白，绝不改写正文以保持 core 诊断坐标可追溯。
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

/// 按 inspect plan 携带的已编译合同验证设备状态；绝不按 map id 回查 builtin。
fn validate_map_device_state(plan: &I2cInspectPlan, image: &[u8]) -> Result<(), NodeError> {
    let validation = &plan.validation;
    if image.len() != usize::from(validation.image_bytes) {
        return Err(NodeError::Precondition(format!(
            "EEPROM image has {} bytes; map `{}` requires {}",
            image.len(),
            plan.map_id,
            validation.image_bytes
        )));
    }
    for (offset, expected) in &validation.fixed_bytes {
        let start = usize::from(*offset);
        let end = start
            .checked_add(expected.len())
            .ok_or_else(|| NodeError::Precondition("fixed-byte range overflow".to_owned()))?;
        if image.get(start..end) != Some(expected.as_slice()) {
            return Err(NodeError::Precondition(format!(
                "EEPROM map-required fixed bytes at offset {start:#x} are absent"
            )));
        }
    }
    for checksum in &validation.checksums {
        let start = usize::from(checksum.source_offset);
        let end = start
            .checked_add(usize::from(checksum.source_byte_len))
            .ok_or_else(|| NodeError::Precondition("checksum source range overflow".to_owned()))?;
        let source = image.get(start..end).ok_or_else(|| {
            NodeError::Precondition("checksum source range is outside image".to_owned())
        })?;
        let expected = match checksum.algorithm {
            ChecksumAlgorithm::SerialSumMod255PlusOne => {
                ((source
                    .iter()
                    .fold(0_u16, |sum, byte| sum + u16::from(*byte))
                    % 0xff)
                    + 1) as u8
            }
        };
        if image.get(usize::from(checksum.target_offset)) != Some(&expected) {
            return Err(NodeError::Precondition(format!(
                "EEPROM map-required checksum at offset {:#x} is invalid",
                checksum.target_offset
            )));
        }
    }
    if let Some(serial) = &validation.serial_range {
        let start = usize::from(serial.offset);
        let end = start
            .checked_add(usize::from(serial.byte_len))
            .ok_or_else(|| NodeError::Precondition("serial range overflow".to_owned()))?;
        let bytes = image
            .get(start..end)
            .ok_or_else(|| NodeError::Precondition("serial range is outside image".to_owned()))?;
        if !valid_yg_serial(bytes) {
            return Err(NodeError::Precondition(
                "EEPROM map-required serial is blank, contains control bytes, or violates SNID format"
                    .to_owned(),
            ));
        }
    }
    Ok(())
}

fn valid_yg_serial(serial: &[u8]) -> bool {
    serial.len() == 14
        && matches!(&serial[..5], b"2T233" | b"2T235")
        && serial[5..7].iter().all(u8::is_ascii_digit)
        && matches!(serial[7], b'1'..=b'9' | b'A'..=b'C')
        && matches!(serial[8], b'1'..=b'9' | b'A'..=b'V')
        && matches!(serial[9], b'0'..=b'4')
        && serial[10..12]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric())
        && serial[12..] == *b"00"
}

/// 预期最终镜像只由已 sealed 的页面覆盖；超出 inspect 全范围的页在任何 I/O 前拒绝。
fn apply_expected_page(image: &mut [u8], page: &I2cPageWrite) -> Result<(), NodeError> {
    let start = usize::from(page.offset);
    let end = start
        .checked_add(page.bytes.len())
        .ok_or_else(|| NodeError::Precondition("authorized page range overflow".to_owned()))?;
    let destination = image.get_mut(start..end).ok_or_else(|| {
        NodeError::Precondition("authorized page lies outside final verification range".to_owned())
    })?;
    destination.copy_from_slice(&page.bytes);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        engine::{
            EngineServices, NodeReporter, OutputRegistry, PortCardinality, PortSpec, SpawnContext,
        },
        platform::I2cTaskExecutor,
    };
    use camera_toolbox_core::PacketProvenance;
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
        image: Mutex<Vec<u8>>,
        fail_at: Option<u16>,
        corrupt: bool,
        active: Arc<AtomicBool>,
        calls: Mutex<Vec<(usize, u16)>>,
        inspect_calls: Mutex<usize>,
    }
    impl crate::platform::I2cTaskExecutor for FakeI2c {
        fn inspect(
            &self,
            _: &SshConnection,
            _: &I2cInspectPlan,
            _: RemoteOperationControl,
        ) -> Result<Vec<u8>, String> {
            if !self.active.load(Ordering::Acquire) {
                return Err("SSH connection is not active".to_owned());
            }
            *self.inspect_calls.lock() += 1;
            Ok(self.image.lock().clone())
        }
        fn verify_authorized(
            &self,
            connection: &SshConnection,
            authorized: &I2cAuthorizedWritePlan,
            _: RemoteOperationControl,
        ) -> Result<(), String> {
            if !self.active.load(Ordering::Acquire) {
                return Err("SSH connection is not active".to_owned());
            }
            if !authorized.is_authorized() || authorized.connection_id != connection.id() {
                return Err("session or authorization mismatch".to_owned());
            }
            if sha256_hex(&self.image.lock()) != authorized.expected_before_sha256 {
                return Err("before image changed".to_owned());
            }
            Ok(())
        }
        fn write_page(
            &self,
            _: &SshConnection,
            authorized: &I2cAuthorizedWritePlan,
            index: usize,
            page: &I2cPageWrite,
            _: RemoteOperationControl,
        ) -> Result<Vec<u8>, String> {
            if !self.active.load(Ordering::Acquire) {
                return Err("SSH connection is not active".to_owned());
            }
            if authorized.page_at(index) != Some(page) {
                return Err("page does not match sealed authorization".to_owned());
            }
            self.calls.lock().push((index, page.offset));
            if self.fail_at == Some(page.offset) {
                return Err("fake page failure".to_owned());
            }
            let mut image = self.image.lock();
            let start = usize::from(page.offset);
            image[start..start + page.bytes.len()].copy_from_slice(&page.bytes);
            if self.corrupt {
                image[0x100] ^= 1;
            }
            Ok(page.bytes.clone())
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
            inputs: builtin_i2c_map(YG_MAP_ID)
                .expect("built-in YG map")
                .inputs
                .iter()
                .map(|slot| port(&slot.name, typed_field_port_kind(slot.primitive_type)))
                .collect(),
            outputs: Vec::new(),
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
                source_port: Some("test.extractor".to_owned()),
                ..PacketProvenance::default()
            },
            Some(model_id.to_owned()),
        ))
    }

    fn typed_field(input: &BuilderInput, generation: u64) -> DataPacket {
        DataPacket::TypedField {
            datum: Arc::clone(&input.datum),
            generation,
            source: Arc::clone(&input.source),
        }
    }

    fn fields() -> BTreeMap<String, BuilderInput> {
        let mut values = BTreeMap::new();
        values.insert(
            "camera.model.id".to_owned(),
            Arc::new(
                Datum::new("camera.model.id", TypedValue::Str(YG_MODEL.to_owned()))
                    .with_semantic_type("camera.model-id"),
            ),
        );
        values.insert(
            "camera.image.width".to_owned(),
            Arc::new(
                Datum::new("camera.image.width", TypedValue::U32(1920))
                    .with_unit("px")
                    .with_semantic_type("image.width"),
            ),
        );
        values.insert(
            "camera.image.height".to_owned(),
            Arc::new(
                Datum::new("camera.image.height", TypedValue::U32(1080))
                    .with_unit("px")
                    .with_semantic_type("image.height"),
            ),
        );
        for (name, semantic) in [
            ("camera.intrinsics.fx", "camera.focal-length"),
            ("camera.intrinsics.fy", "camera.focal-length"),
            ("camera.intrinsics.cx", "camera.principal-point"),
            ("camera.intrinsics.cy", "camera.principal-point"),
        ] {
            values.insert(
                name.to_owned(),
                Arc::new(
                    Datum::new(name, TypedValue::F64(1.0))
                        .with_unit("px")
                        .with_semantic_type(semantic),
                ),
            );
        }
        for name in [
            "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
        ] {
            let source = format!("distortion.{name}");
            values.insert(
                source.clone(),
                Arc::new(
                    Datum::new(source, TypedValue::F64(0.0))
                        .with_unit("dimensionless")
                        .with_semantic_type("camera.distortion-coefficient"),
                ),
            );
        }
        values.insert(
            "serial.number".to_owned(),
            Arc::new(
                Datum::new(
                    "serial.number",
                    TypedValue::Str("2T23326AV4ZZ00".to_owned()),
                )
                .with_semantic_type("device.serial-number"),
            ),
        );
        values
            .into_iter()
            .map(|(name, datum)| {
                (
                    name,
                    BuilderInput {
                        datum,
                        source: source("camera-toolbox.calib.solution.v1", YG_MODEL),
                    },
                )
            })
            .collect()
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
    fn authorized_plan() -> (
        Arc<SshConnection>,
        Arc<I2cInspectSnapshot>,
        Arc<I2cAuthorizedWritePlan>,
    ) {
        let connection = connection();
        let builder = I2cTaskBuilderNode::new(builder_spec(Some(7))).unwrap();
        let (inspect, candidate) = builder.build(&fields()).unwrap();
        let before = valid_image();
        let snapshot = Arc::new(I2cInspectSnapshot {
            connection_id: connection.id().to_owned(),
            inspect_plan: inspect.clone(),
            map_id: inspect.map_id.clone(),
            map_digest: inspect.map_digest.clone(),
            target: inspect.target.clone(),
            before_image_sha256: sha256_hex(&before),
            before_image: before.clone(),
        });
        let authorized = Arc::new(I2cAuthorizedWritePlan::new(
            connection.id().to_owned(),
            snapshot.before_image_sha256.clone(),
            before,
            inspect,
            candidate,
        ));
        (connection, snapshot, authorized)
    }
    fn fake(
        image: Vec<u8>,
        active: Arc<AtomicBool>,
        fail_at: Option<u16>,
        corrupt: bool,
    ) -> Arc<FakeI2c> {
        Arc::new(FakeI2c {
            image: Mutex::new(image),
            fail_at,
            corrupt,
            active,
            calls: Mutex::new(Vec::new()),
            inspect_calls: Mutex::new(0),
        })
    }

    #[test]
    fn builder_requires_explicit_bus_and_exact_typed_inputs() {
        assert!(matches!(
            I2cTaskBuilderNode::new(builder_spec(None)),
            Err(NodeError::Config(_))
        ));
        let builder = I2cTaskBuilderNode::new(builder_spec(Some(7))).unwrap();
        let (inspect, candidate) = builder.build(&fields()).unwrap();
        assert_eq!(inspect.target.bus, 7);
        assert_eq!(candidate.target.bus, 7);
        let mut wrong = fields();
        wrong.insert(
            "camera.image.width".to_owned(),
            BuilderInput {
                datum: Arc::new(
                    Datum::new("camera.image.width", TypedValue::F64(1920.0))
                        .with_unit("px")
                        .with_semantic_type("image.width"),
                ),
                source: source("camera-toolbox.calib.solution.v1", YG_MODEL),
            },
        );
        assert!(matches!(
            builder.build(&wrong),
            Err(NodeError::Precondition(_))
        ));
        let mut node = I2cTaskBuilderNode::new(builder_spec(Some(7))).unwrap();
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(EngineServices::default(), outputs);
        assert!(matches!(
            node.on_input(
                "camera.image.width",
                typed_field(wrong.get("camera.image.width").unwrap(), 1),
                &mut rt,
            ),
            Err(NodeError::Precondition(_))
        ));
    }
    fn custom_map_yaml() -> String {
        format!(
            "schema: camera-toolbox.i2c-map.v1\nid: custom-demo\ndisplayName: Custom demo\naccepts:\n  schemas: [camera-toolbox.calib.solution.v1]\n  modelIds: [pinhole.rational-thin-prism.v1]\ntarget:\n  bus: 0\n  address: 80\n  addressWidthBits: 16\n  pageSizeBytes: 4\n  writeCycleMs: 5\n  pagePolicy: {{ mode: split-at-boundary }}\n  readBefore:\n    required: true\n    ranges: [{{ offset: 0, byteLen: 4 }}]\n  readback:\n    required: true\n    verification: exact-written-ranges\nstorage:\n  - source: value\n    offset: 0\n    encoding: u32-le\ninputs:\n  - name: value\n    type: f64\n    required: true\n    conversion: {{ scale: 1.0, offset: 0.0, minimum: 0.0, maximum: 4294967295.0, rounding: exact }}\n"
        )
    }

    fn custom_builder_spec(yaml: String) -> NodeSpec {
        NodeSpec {
            id: "custom-builder".to_owned(),
            kind: "i2cTaskBuilder".to_owned(),
            title: "custom builder".to_owned(),
            inputs: vec![port("value", "data.field.f64.v1")],
            outputs: Vec::new(),
            config: serde_json::json!({"mapMode": "custom", "mapYaml": yaml, "bus": 7}),
        }
    }

    #[test]
    fn custom_map_rejects_sparse_or_nonzero_read_before_ranges() {
        let sparse = custom_map_yaml().replace(
            "ranges: [{ offset: 0, byteLen: 4 }]",
            "ranges: [{ offset: 1, byteLen: 3 }]",
        );
        let error = match I2cTaskBuilderNode::new(custom_builder_spec(sparse)) {
            Ok(_) => panic!("sparse read-before ranges are not representable by app executor"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            NodeError::Config(message) if message.contains("ordered contiguous cover")
        ));
    }

    fn custom_fields(schema: &str, model_id: &str) -> BTreeMap<String, BuilderInput> {
        BTreeMap::from([(
            "value".to_owned(),
            BuilderInput {
                datum: Arc::new(Datum::new("value", TypedValue::F64(12.0))),
                source: source(schema, model_id),
            },
        )])
    }

    #[test]
    fn custom_map_compiles_into_its_static_slot_interface_and_emits_plans() {
        let builder = I2cTaskBuilderNode::new(custom_builder_spec(custom_map_yaml()))
            .expect("custom YAML compiles against its static typed slot");
        let (inspect, candidate) = builder
            .build(&custom_fields("camera-toolbox.calib.solution.v1", YG_MODEL))
            .expect("accepted custom source emits plans");
        assert_eq!(inspect.map_id, "custom-demo");
        assert_eq!(inspect.target.bus, 7);
        assert_eq!(candidate.target, inspect.target);
        assert_eq!(candidate.pages.len(), 1);
        assert_eq!(candidate.pages[0].bytes, 12_u32.to_le_bytes());
    }

    #[test]
    fn custom_map_compile_errors_keep_source_locations_and_previous_map() {
        let mut node = I2cTaskBuilderNode::new(custom_builder_spec(custom_map_yaml())).unwrap();
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(EngineServices::default(), outputs);
        let invalid = format!(
            "\n\n{}",
            custom_map_yaml().replace("encoding: u32-le", "encoding: invalid")
        );
        let expected_line = invalid
            .lines()
            .position(|line| line.contains("encoding: invalid"))
            .expect("invalid encoding is present")
            + 1;
        let expected_column = invalid
            .lines()
            .nth(expected_line - 1)
            .and_then(|line| line.find("encoding:"))
            .expect("encoding key is present")
            + 1;
        let error = node
            .on_config_update(custom_builder_spec(invalid).config, &mut rt)
            .expect_err("invalid YAML must not replace the compiled map");
        let NodeError::Config(message) = error else {
            panic!("expected configuration diagnostic");
        };
        assert!(message.contains(&format!("line {expected_line}, column {expected_column}")));
        assert!(
            node.build(&custom_fields("camera-toolbox.calib.solution.v1", YG_MODEL))
                .is_ok()
        );
    }

    #[test]
    fn map_rejects_each_typed_field_schema_and_model_before_plan_emission() {
        let builder = I2cTaskBuilderNode::new(custom_builder_spec(custom_map_yaml())).unwrap();
        let schema_error = builder
            .build(&custom_fields("unexpected.schema.v1", YG_MODEL))
            .expect_err("unaccepted schema must reject plan");
        assert!(schema_error.to_string().contains("does not accept schema"));
        let model_error = builder
            .build(&custom_fields(
                "camera-toolbox.calib.solution.v1",
                "other.model.v1",
            ))
            .expect_err("unaccepted model must reject plan");
        assert!(model_error.to_string().contains("does not accept model id"));
    }

    #[test]
    fn custom_map_inspect_uses_sealed_validation_contract() {
        let builder = I2cTaskBuilderNode::new(custom_builder_spec(custom_map_yaml())).unwrap();
        let (inspect, _) = builder
            .build(&custom_fields("camera-toolbox.calib.solution.v1", YG_MODEL))
            .expect("custom plan compiles");
        let active = Arc::new(AtomicBool::new(true));
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(
            EngineServices {
                i2c_task_executor: Some(fake(vec![0; 4], Arc::clone(&active), None, false)),
                ..EngineServices::default()
            },
            Arc::clone(&outputs),
        );
        I2cInspectorNode {
            connection: Some(connection()),
            inspect: Some(Arc::new(inspect)),
        }
        .inspect_if_ready(&mut rt)
        .expect("custom compiled validation contract accepts matching image");
        assert!(matches!(
            outputs.lock().as_slice(),
            [DataPacket::I2cInspectSnapshot(_)]
        ));
    }

    #[test]
    fn inspect_rejects_invalid_map_required_device_state() {
        let builder = I2cTaskBuilderNode::new(builder_spec(Some(7))).unwrap();
        let (inspect, _) = builder.build(&fields()).unwrap();
        let mut invalid = valid_image();
        invalid[0] = b'x';
        let active = Arc::new(AtomicBool::new(true));
        let node = I2cInspectorNode {
            connection: Some(connection()),
            inspect: Some(Arc::new(inspect)),
        };
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(
            EngineServices {
                i2c_task_executor: Some(fake(invalid, active, None, false)),
                ..EngineServices::default()
            },
            outputs,
        );
        assert!(matches!(
            node.inspect_if_ready(&mut rt),
            Err(NodeError::Precondition(_))
        ));
    }
    #[test]
    fn executor_rejects_mutated_authorization_before_any_write() {
        let (connection, snapshot, authorized) = authorized_plan();
        let mut forged = authorized.as_ref().clone();
        forged.candidate.pages[0].bytes[0] ^= 0x55;
        let active = Arc::new(AtomicBool::new(true));
        let service = fake(valid_image(), active, None, false);
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(
            EngineServices {
                i2c_task_executor: Some(service.clone()),
                ..EngineServices::default()
            },
            outputs,
        );
        let node = I2cTaskExecutorNode {
            connection: Some(connection),
            authorized: Some(Arc::new(forged)),
        };
        assert!(matches!(
            node.execute_authorized(&mut rt),
            Err(NodeError::Precondition(_))
        ));
        assert!(service.calls.lock().is_empty());
    }
    #[test]
    fn executor_halts_after_first_page_failure_without_rollback() {
        let (connection, _snapshot, authorized) = authorized_plan();
        let active = Arc::new(AtomicBool::new(true));
        let service = fake(
            valid_image(),
            active,
            Some(authorized.candidate.pages[1].offset),
            false,
        );
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(
            EngineServices {
                i2c_task_executor: Some(service.clone()),
                ..EngineServices::default()
            },
            Arc::clone(&outputs),
        );
        I2cTaskExecutorNode {
            connection: Some(connection),
            authorized: Some(authorized),
        }
        .execute_authorized(&mut rt)
        .unwrap();
        assert_eq!(service.calls.lock().len(), 2);
        let DataPacket::I2cExecutionReport(report) = &outputs.lock()[0] else {
            panic!("expected report");
        };
        assert!(!report.final_verified);
        assert_eq!(report.pages.len(), 2);
    }
    #[test]
    fn final_full_range_verification_detects_silent_corruption() {
        let (connection, _snapshot, authorized) = authorized_plan();
        let active = Arc::new(AtomicBool::new(true));
        let service = fake(valid_image(), active, None, true);
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(
            EngineServices {
                i2c_task_executor: Some(service.clone()),
                ..EngineServices::default()
            },
            Arc::clone(&outputs),
        );
        I2cTaskExecutorNode {
            connection: Some(connection),
            authorized: Some(authorized),
        }
        .execute_authorized(&mut rt)
        .unwrap();
        let DataPacket::I2cExecutionReport(report) = &outputs.lock()[0] else {
            panic!("expected report");
        };
        assert!(!report.final_verified);
        assert!(
            report
                .error
                .as_deref()
                .unwrap()
                .contains("final full-range")
        );
        assert_eq!(*service.inspect_calls.lock(), 1);
    }
    #[test]
    fn disconnect_revokes_handle_and_stale_operations_are_rejected() {
        let ssh = Arc::new(FakeSsh {
            active: Arc::new(AtomicBool::new(true)),
            revoked: Mutex::new(Vec::new()),
        });
        let stale = connection();
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
            connection: Some(Arc::clone(&stale)),
        };
        node.on_action(NodeAction::Disconnect, &mut rt).unwrap();
        assert_eq!(ssh.revoked.lock().as_slice(), ["fake-session"]);
        ssh.active.store(true, Ordering::Release);
        let mut stopped = SshConnectionNode {
            spec: action_spec(),
            connection: Some(connection()),
        };
        stopped.on_stop(&mut rt).unwrap();
        assert_eq!(
            ssh.revoked.lock().as_slice(),
            ["fake-session", "fake-session"]
        );
        let builder = I2cTaskBuilderNode::new(builder_spec(Some(7))).unwrap();
        let (inspect, _) = builder.build(&fields()).unwrap();
        assert_eq!(
            fake(valid_image(), Arc::clone(&ssh.active), None, false)
                .inspect(&stale, &inspect, remote_control().unwrap())
                .unwrap_err(),
            "SSH connection is not active"
        );
    }
    #[test]
    fn builder_never_mixes_typed_field_generations() {
        let mut node = I2cTaskBuilderNode::new(builder_spec(Some(7))).unwrap();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(EngineServices::default(), Arc::clone(&recorded));
        let fields = fields();
        let entries = fields.iter().collect::<Vec<_>>();
        let (first_name, first_value) = entries[0];
        let (new_name, new_value) = entries[1];

        node.on_input(first_name, typed_field(first_value, 1), &mut rt)
            .unwrap();
        node.on_input(new_name, typed_field(new_value, 2), &mut rt)
            .unwrap();
        for (name, value) in entries.iter().skip(1) {
            node.on_input(name, typed_field(value, 1), &mut rt).unwrap();
        }
        assert!(
            recorded.lock().is_empty(),
            "old fields must not complete generation two"
        );
        for (name, value) in entries.iter().filter(|(name, _)| **name != *new_name) {
            node.on_input(name, typed_field(value, 2), &mut rt).unwrap();
        }
        assert_eq!(
            recorded.lock().len(),
            2,
            "one complete generation emits two plans"
        );
    }

    #[test]
    fn inspector_rejects_blank_control_and_malformed_serials() {
        let builder = I2cTaskBuilderNode::new(builder_spec(Some(7))).unwrap();
        let (inspect, _) = builder.build(&fields()).unwrap();
        for serial in [
            b"              ".as_slice(),
            b"2T23326A\x014ZZ00".as_slice(),
            b"2T29926AV4ZZ00".as_slice(),
        ] {
            let mut image = valid_image();
            image[0x125..0x133].copy_from_slice(serial);
            image[0x133] = ((serial
                .iter()
                .fold(0_u16, |sum, byte| sum + u16::from(*byte))
                % 0xff)
                + 1) as u8;
            assert!(matches!(
                validate_map_device_state(&inspect, &image),
                Err(NodeError::Precondition(_))
            ));
        }
    }
    #[test]
    fn inspector_accepts_state_contract_of_other_builtin_map() {
        let builder = I2cTaskBuilderNode::new(builder_spec(Some(7))).unwrap();
        let (_, _) = builder.build(&fields()).unwrap();
        let mut map = camera_toolbox_core::builtin_i2c_maps()
            .into_iter()
            .find(|map| map.id != YG_MAP_ID)
            .expect("core exposes a non-YG builtin map");
        map.target.bus = 7;
        let target = I2cTaskTarget {
            bus: map.target.bus,
            address: u16::from(map.target.transport.i2c_address),
            address_width_bytes: map.target.transport.address_width_bits / 8,
            page_size_bytes: map.target.transport.page_size_bytes,
            write_cycle_ms: map.target.transport.write_cycle_ms,
        };
        let inspect = I2cInspectPlan::new(
            map.id.clone(),
            sha256_hex(format!("{map:?}").as_bytes()),
            target,
            map.read_before
                .ranges
                .iter()
                .map(|range| I2cReadRange {
                    offset: range.offset,
                    byte_len: range.byte_len,
                })
                .collect(),
            I2cMapValidationContract::from_map(&map),
        );
        assert!(
            validate_map_device_state(&inspect, &vec![0; usize::from(map.image_bytes)]).is_ok()
        );
    }

    #[test]
    fn config_update_revokes_connected_ssh_before_reconnect() {
        let fake = Arc::new(FakeSsh::default());
        let ssh: Arc<dyn crate::platform::SshConnectionService> = fake.clone();
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(
            EngineServices {
                ssh_connection_service: Some(Arc::clone(&ssh)),
                ..EngineServices::default()
            },
            outputs,
        );
        let mut node = SshConnectionNode {
            spec: action_spec(),
            connection: None,
        };
        node.on_action(NodeAction::Connect, &mut rt).unwrap();
        node.on_config_update(
            serde_json::json!({"host": "new-fake", "credentialRef": "session:new"}),
            &mut rt,
        )
        .unwrap();
        assert_eq!(fake.revoked.lock().as_slice(), ["fake-session"]);
        assert!(node.connection.is_none());
        assert!(matches!(
            node.on_config_update(
                serde_json::json!({"host": 1, "credentialRef": "session:new"}),
                &mut rt
            ),
            Err(NodeError::Config(_))
        ));
    }

    #[test]
    fn approval_config_update_is_validated_and_invalidates_pending_inputs() {
        let mut node = I2cWriteApprovalNode {
            spec: NodeSpec {
                id: "approval".to_owned(),
                kind: "i2cWriteApproval".to_owned(),
                title: "approval".to_owned(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                config: serde_json::json!({"confirmWrite": true}),
            },
            candidate: None,
            snapshot: None,
        };
        let mut rt = runtime(EngineServices::default(), Arc::new(Mutex::new(Vec::new())));
        node.on_config_update(serde_json::json!({"confirmWrite": false}), &mut rt)
            .unwrap();
        assert!(node.snapshot.is_none());
        assert!(matches!(
            node.on_config_update(serde_json::json!({"confirmWrite": "yes"}), &mut rt),
            Err(NodeError::Config(_))
        ));
        assert_eq!(node.spec.config, serde_json::json!({"confirmWrite": false}));
    }

    #[test]
    fn executor_rejects_non_authorized_plan_inputs() {
        let builder = I2cTaskBuilderNode::new(builder_spec(Some(7))).unwrap();
        let (inspect, _) = builder.build(&fields()).unwrap();
        let (_, snapshot, _) = authorized_plan();
        let mut node = I2cTaskExecutorNode {
            connection: None,
            authorized: None,
        };
        let mut rt = runtime(EngineServices::default(), Arc::new(Mutex::new(Vec::new())));
        assert!(matches!(
            node.on_input(
                "inspectPlan",
                DataPacket::I2cInspectPlan(Arc::new(inspect)),
                &mut rt
            ),
            Err(NodeError::Precondition(_))
        ));
        assert!(matches!(
            node.on_input(
                "snapshot",
                DataPacket::I2cInspectSnapshot(snapshot),
                &mut rt
            ),
            Err(NodeError::Precondition(_))
        ));
    }
}
