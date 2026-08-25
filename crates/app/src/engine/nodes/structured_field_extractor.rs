//! 结构化数据包字段提取节点。
//!
//! 输出定义在图规格中静态配置：每个定义用 JSON Pointer 选中一个完整 datum。
//! 运行时只接受通用 `data.packet.v1`，并将 datum 与完整来源原样作为
//! `data.field.v1` 输出，绝不执行数值转换、类型推断或业务字段解释。

use std::{collections::HashSet, sync::Arc};

use camera_toolbox_core::{Datum, StructuredPacket, TypedValue};
use serde_json::Value;

use crate::engine::{
    DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState,
    NodeSpec,
};

const INPUT_PORT: &str = "packet";
const PACKET_PORT_KIND: &str = "data.packet.v1";

/// `structuredFieldExtractor` 节点的工厂。
pub struct StructuredFieldExtractorFactory;

impl NodeFactory for StructuredFieldExtractorFactory {
    fn kind(&self) -> &'static str {
        "structuredFieldExtractor"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(StructuredFieldExtractorNode::new(spec)?))
    }
}

/// 一个预编译的静态输出定义。
#[derive(Clone, Debug, PartialEq, Eq)]
struct OutputDefinition {
    id: String,
    pointer: String,
}

/// 从结构化包按配置 JSON Pointer 提取 datum 的转换节点。
#[derive(Debug)]
pub struct StructuredFieldExtractorNode {
    outputs: Vec<OutputDefinition>,
    next_generation: u64,
}

impl StructuredFieldExtractorNode {
    fn new(spec: NodeSpec) -> Result<Self, NodeError> {
        validate_input_port(&spec)?;
        let outputs = parse_output_definitions(&spec)?;
        Ok(Self {
            outputs,
            next_generation: 1,
        })
    }

    fn extract(
        &mut self,
        packet: &StructuredPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        // JSON Pointer 的目标是 structured packet 的公开 wire 形态；仅接受完整 datum，
        // 因而字段的 name/unit/semanticType 与精确值都会被原样保留。
        let document = serde_json::to_value(packet).map_err(|error| {
            NodeError::Execution(format!(
                "could not encode structured packet for extraction: {error}"
            ))
        })?;

        let source = typed_field_source(packet);
        let generation = self.next_generation;
        self.next_generation = self.next_generation.checked_add(1).ok_or_else(|| {
            NodeError::Execution("structured field generation counter exhausted".to_owned())
        })?;
        for output in &self.outputs {
            let value = document.pointer(&output.pointer).ok_or_else(|| {
                NodeError::Precondition(format!(
                    "structured field output `{}` pointer `{}` did not resolve to a value",
                    output.id, output.pointer
                ))
            })?;
            let datum: Datum = serde_json::from_value(value.clone()).map_err(|error| {
                NodeError::Precondition(format!(
                    "structured field output `{}` pointer `{}` must select one datum: {error}",
                    output.id, output.pointer
                ))
            })?;
            datum.validate().map_err(|error| NodeError::Precondition(format!(
                "structured field output `{}` pointer `{}` selected an invalid datum: {error}",
                output.id, output.pointer
            )))?;
            rt.emit(
                &output.id,
                DataPacket::TypedField {
                    datum: Arc::new(datum),
                    generation,
                    source: Arc::clone(&source),
                },
            )?;
        }
        rt.report_state(NodeRuntimeState::Running, "structured fields extracted");
        Ok(())
    }
}

impl NodeInstance for StructuredFieldExtractorNode {
    fn kind(&self) -> &'static str {
        "structuredFieldExtractor"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for structured packet");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if port != INPUT_PORT {
            return Err(NodeError::Precondition(format!(
                "structuredFieldExtractor received input on unknown port `{port}`"
            )));
        }
        let packet = match packet {
            DataPacket::StructuredPacket(packet) => packet,
            DataPacket::PacketData(value) => {
                let wire = serde_json::to_string(value.as_ref()).map_err(|error| {
                    NodeError::Precondition(format!("data.packet.v1 cannot be encoded: {error}"))
                })?;
                Arc::new(StructuredPacket::from_json(&wire).map_err(|error| {
                    NodeError::Precondition(format!(
                        "structuredFieldExtractor.packet requires StructuredPacket wire JSON: {error}"
                    ))
                })?)
            }
            _ => return Err(NodeError::Precondition(
                "structuredFieldExtractor.packet requires data.packet.v1".to_owned(),
            )),
        };
        self.extract(&packet, rt)
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 从完整结构化包构造被每个 typed-field 共享的来源元数据。
///
/// 相机模型若存在则原样附着；提取器保持通用，不要求非相机包伪造模型元数据。
fn typed_field_source(packet: &StructuredPacket) -> Arc<crate::engine::TypedFieldSource> {
    let model_id = packet.fields.iter().find_map(|field| {
        match (
            &field.name[..],
            &field.value,
            field.semantic_type.as_deref(),
        ) {
            ("camera.model.id", TypedValue::Str(value), Some("camera.model-id"))
                if !value.trim().is_empty() =>
            {
                Some(value.clone())
            }
            _ => None,
        }
    });
    Arc::new(crate::engine::TypedFieldSource::new(
        packet.schema.clone(),
        packet.provenance.clone(),
        model_id,
    ))
}

fn validate_input_port(spec: &NodeSpec) -> Result<(), NodeError> {
    let input = spec
        .inputs
        .iter()
        .find(|port| port.id == INPUT_PORT)
        .ok_or_else(|| {
            NodeError::Config("structuredFieldExtractor requires input port `packet`".to_owned())
        })?;
    if input.kind != PACKET_PORT_KIND {
        return Err(NodeError::Config(format!(
            "structuredFieldExtractor input `packet` must use `{PACKET_PORT_KIND}`, got `{}`",
            input.kind
        )));
    }
    Ok(())
}

fn parse_output_definitions(spec: &NodeSpec) -> Result<Vec<OutputDefinition>, NodeError> {
    let config = spec.config.as_object().ok_or_else(|| {
        NodeError::Config("structuredFieldExtractor config must be an object".to_owned())
    })?;
    let definitions = config
        .get("outputs")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            NodeError::Config(
                "structuredFieldExtractor config requires an `outputs` array".to_owned(),
            )
        })?;
    if definitions.is_empty() {
        return Err(NodeError::Config(
            "structuredFieldExtractor config `outputs` must not be empty".to_owned(),
        ));
    }

    let mut ids = HashSet::with_capacity(definitions.len());
    let mut parsed = Vec::with_capacity(definitions.len());
    for definition in definitions {
        let definition = definition.as_object().ok_or_else(|| {
            NodeError::Config(
                "structuredFieldExtractor output definition must be an object".to_owned(),
            )
        })?;
        let id = required_text(definition, "id")?;
        if !ids.insert(id.to_owned()) {
            return Err(NodeError::Config(format!(
                "structuredFieldExtractor contains duplicate output definition `{id}`"
            )));
        }
        let pointer = required_text(definition, "pointer")?;
        validate_json_pointer(pointer)?;
        if definition.contains_key("type") {
            return Err(NodeError::Config(format!(
                "structuredFieldExtractor output `{id}` must not declare a primitive type"
            )));
        }

        let port = spec.outputs.iter().find(|port| port.id == id).ok_or_else(|| {
            NodeError::Config(format!(
                "structuredFieldExtractor output definition `{id}` has no matching static output port"
            ))
        })?;
        if port.kind != "data.field.v1" {
            return Err(NodeError::Config(format!(
                "structuredFieldExtractor output `{id}` must use `data.field.v1`, got `{}`",
                port.kind
            )));
        }
        parsed.push(OutputDefinition {
            id: id.to_owned(),
            pointer: pointer.to_owned(),
        });
    }

    if spec.outputs.len() != parsed.len() {
        return Err(NodeError::Config(
            "structuredFieldExtractor static output ports must exactly match configured outputs"
                .to_owned(),
        ));
    }
    Ok(parsed)
}

fn required_text<'a>(
    definition: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str, NodeError> {
    definition
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            NodeError::Config(format!(
                "structuredFieldExtractor output definition requires non-empty `{key}`"
            ))
        })
}

/// 校验 RFC 6901 语法；空指针会选中完整 packet，不能构成单个 datum。
fn validate_json_pointer(pointer: &str) -> Result<(), NodeError> {
    if pointer.is_empty() || !pointer.starts_with('/') {
        return Err(NodeError::Config(format!(
            "structuredFieldExtractor pointer `{pointer}` must start with `/`"
        )));
    }
    let bytes = pointer.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'~' {
            let Some(escape) = bytes.get(index + 1) else {
                return Err(NodeError::Config(format!(
                    "structuredFieldExtractor pointer `{pointer}` contains an incomplete `~` escape"
                )));
            };
            if *escape != b'0' && *escape != b'1' {
                return Err(NodeError::Config(format!(
                    "structuredFieldExtractor pointer `{pointer}` contains invalid `~{}` escape",
                    char::from(*escape)
                )));
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use std::sync::{atomic::AtomicBool, mpsc, Arc};

    use camera_toolbox_core::{PacketProvenance, TypedValue};
    use parking_lot::Mutex;

    use super::*;
    use crate::engine::{
        EngineServices, NodeReporter, OutputRegistry, PortCardinality, PortSpec, SpawnContext,
    };

    fn port(id: &str, kind: &str) -> PortSpec {
        PortSpec { id: id.to_owned(), label: id.to_owned(), kind: kind.to_owned(), cardinality: PortCardinality::One, required: true }
    }

    fn spec(config: Value, outputs: Vec<PortSpec>) -> NodeSpec {
        NodeSpec { id: "extractor".to_owned(), kind: "structuredFieldExtractor".to_owned(), title: String::new(), inputs: vec![port(INPUT_PORT, PACKET_PORT_KIND)], outputs, config }
    }

    fn runtime(recorded: Arc<Mutex<Vec<DataPacket>>>) -> NodeRuntime {
        let (state_tx, _) = mpsc::channel();
        let (event_tx, _) = mpsc::channel();
        let mut outputs = OutputRegistry::default();
        outputs.set_record(Arc::new(move |packet| recorded.lock().push(packet)));
        NodeRuntime::new(SpawnContext { outputs, reporter: NodeReporter::new("extractor".to_owned(), state_tx, event_tx), services: Arc::new(EngineServices::default()), cancel: Arc::new(AtomicBool::new(false)), viewer_slot: None })
    }

    fn packet() -> StructuredPacket {
        StructuredPacket::new("example.packet.v1", PacketProvenance::default(), vec![Datum::new("camera.intrinsics.fx", TypedValue::F64(912.43))]).expect("valid packet")
    }

    #[test]
    fn emits_complete_datum_on_generic_field_port() {
        let mut node = StructuredFieldExtractorNode::new(spec(serde_json::json!({"outputs":[{"id":"fx","pointer":"/fields/0"}]}), vec![port("fx", "data.field.v1")])).expect("valid config");
        let recorded = Arc::new(Mutex::new(Vec::new()));
        node.on_input(INPUT_PORT, DataPacket::StructuredPacket(Arc::new(packet())), &mut runtime(Arc::clone(&recorded))).expect("extract");
        let output = recorded.lock();
        let [DataPacket::TypedField { datum, .. }] = output.as_slice() else { panic!("expected field") };
        assert_eq!(datum.name, "camera.intrinsics.fx");
        assert_eq!(DataPacket::TypedField { datum: Arc::clone(datum), generation: 1, source: typed_field_source(&packet()) }.port_kind(), "data.field.v1");
    }

    #[test]
    fn rejects_primitive_typed_port_or_config() {
        assert!(StructuredFieldExtractorNode::new(spec(serde_json::json!({"outputs":[{"id":"fx","pointer":"/fields/0","type":"f64"}]}), vec![port("fx", "data.field.v1")])).is_err());
        assert!(StructuredFieldExtractorNode::new(spec(serde_json::json!({"outputs":[{"id":"fx","pointer":"/fields/0"}]}), vec![port("fx", "data.field.f64.v1")])).is_err());
    }
}
