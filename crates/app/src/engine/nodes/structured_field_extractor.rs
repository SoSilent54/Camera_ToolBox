//! 结构化数据包字段提取节点。
//!
//! 输出定义在图规格中静态配置：每个定义用 JSON Pointer 选中一个完整 datum，并声明
//! 其精确 primitive 类型。运行时只接受 `data.structured.packet.v1`，并将匹配 datum
//! 原样作为对应 `data.field.<primitive>.v1` 输出，绝不执行数值转换或推断。

use std::{collections::HashSet, str::FromStr, sync::Arc};

use camera_toolbox_core::{Datum, PrimitiveType, StructuredPacket, TypedValue};
use serde_json::Value;

use crate::engine::{
    DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState,
    NodeSpec,
};

const INPUT_PORT: &str = "packet";
const STRUCTURED_PACKET_PORT_KIND: &str = "data.structured.packet.v1";

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
    primitive_type: PrimitiveType,
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
            let actual = datum.primitive_type();
            if actual != output.primitive_type {
                return Err(NodeError::Precondition(format!(
                    "structured field output `{}` expected type `{}`, got `{actual}`",
                    output.id, output.primitive_type
                )));
            }
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
        let DataPacket::StructuredPacket(packet) = packet else {
            return Err(NodeError::Precondition(
                "structuredFieldExtractor.packet requires data.structured.packet.v1".to_owned(),
            ));
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
    if input.kind != STRUCTURED_PACKET_PORT_KIND {
        return Err(NodeError::Config(format!(
            "structuredFieldExtractor input `packet` must use `{STRUCTURED_PACKET_PORT_KIND}`, got `{}`",
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
        let primitive_type =
            PrimitiveType::from_str(required_text(definition, "type")?).map_err(|error| {
                NodeError::Config(format!("invalid output type for `{id}`: {error}"))
            })?;

        let port = spec.outputs.iter().find(|port| port.id == id).ok_or_else(|| {
            NodeError::Config(format!(
                "structuredFieldExtractor output definition `{id}` has no matching static output port"
            ))
        })?;
        let expected_kind = typed_field_port_kind(primitive_type);
        if port.kind != expected_kind {
            return Err(NodeError::Config(format!(
                "structuredFieldExtractor output `{id}` must use `{expected_kind}`, got `{}`",
                port.kind
            )));
        }
        parsed.push(OutputDefinition {
            id: id.to_owned(),
            pointer: pointer.to_owned(),
            primitive_type,
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicBool, mpsc};

    use camera_toolbox_core::{PacketProvenance, TypedValue};
    use parking_lot::Mutex;

    use super::*;
    use crate::engine::{
        EngineServices, NodeReporter, OutputRegistry, PortCardinality, PortSpec, SpawnContext,
    };

    fn port(id: &str, kind: &str) -> PortSpec {
        PortSpec {
            id: id.to_owned(),
            label: id.to_owned(),
            kind: kind.to_owned(),
            cardinality: PortCardinality::One,
            required: true,
        }
    }

    fn spec(config: Value, outputs: Vec<PortSpec>) -> NodeSpec {
        NodeSpec {
            id: "extractor-1".to_owned(),
            kind: "structuredFieldExtractor".to_owned(),
            title: "Structured fields".to_owned(),
            inputs: vec![port(INPUT_PORT, STRUCTURED_PACKET_PORT_KIND)],
            outputs,
            config,
        }
    }

    fn runtime(recorded: Arc<Mutex<Vec<DataPacket>>>) -> NodeRuntime {
        let (state_tx, _state_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let mut outputs = OutputRegistry::default();
        outputs.set_record(Arc::new(move |packet| recorded.lock().push(packet)));
        NodeRuntime::new(SpawnContext {
            outputs,
            reporter: NodeReporter::new("extractor-1".to_owned(), state_tx, event_tx),
            services: Arc::new(EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        })
    }

    fn packet() -> StructuredPacket {
        StructuredPacket::new(
            "example.packet.v1",
            PacketProvenance {
                source_port: Some("test.packet".to_owned()),
                ..PacketProvenance::default()
            },
            vec![
                Datum::new(
                    "camera.model.id",
                    TypedValue::Str("example.model.v1".to_owned()),
                )
                .with_semantic_type("camera.model-id"),
                Datum::new("camera.intrinsics.fx", TypedValue::F64(912.43)),
                Datum::new("camera.image.width", TypedValue::U32(1920)),
            ],
        )
        .expect("valid packet")
    }

    #[test]
    fn extracts_configured_datum_with_exact_typed_field_port() {
        let config = serde_json::json!({
            "outputs": [{"id": "fx", "pointer": "/fields/1", "type": "f64"}],
        });
        let mut node =
            StructuredFieldExtractorNode::new(spec(config, vec![port("fx", "data.field.f64.v1")]))
                .expect("valid static definition");
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(Arc::clone(&recorded));

        node.on_input(
            INPUT_PORT,
            DataPacket::StructuredPacket(Arc::new(packet())),
            &mut rt,
        )
        .expect("extract configured field");

        let output = recorded.lock();
        let [
            DataPacket::TypedField {
                datum: field,
                generation,
                source,
            },
        ] = output.as_slice()
        else {
            panic!("expected one typed field output");
        };
        assert_eq!(*generation, 1);
        assert_eq!(field.name, "camera.intrinsics.fx");
        assert_eq!(field.value, TypedValue::F64(912.43));
        assert_eq!(source.schema, "example.packet.v1");
        assert_eq!(source.model_id.as_deref(), Some("example.model.v1"));
        assert_eq!(
            source.provenance.source_port.as_deref(),
            Some("test.packet")
        );
        assert_eq!(
            DataPacket::TypedField {
                datum: Arc::clone(field),
                generation: *generation,
                source: Arc::clone(source),
            }
            .port_kind(),
            "data.field.f64.v1"
        );
    }

    #[test]
    fn rejects_invalid_pointer_missing_value_and_type_mismatch() {
        let invalid_pointer = StructuredFieldExtractorNode::new(spec(
            serde_json::json!({"outputs": [{"id": "fx", "pointer": "fields/0", "type": "f64"}]}),
            vec![port("fx", "data.field.f64.v1")],
        ))
        .expect_err("invalid pointer must reject config");
        assert!(matches!(invalid_pointer, NodeError::Config(_)));

        let mut missing = StructuredFieldExtractorNode::new(spec(
            serde_json::json!({"outputs": [{"id": "fx", "pointer": "/fields/9", "type": "f64"}]}),
            vec![port("fx", "data.field.f64.v1")],
        ))
        .expect("valid pointer syntax");
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime(Arc::clone(&recorded));
        let missing_error = missing
            .on_input(
                INPUT_PORT,
                DataPacket::StructuredPacket(Arc::new(packet())),
                &mut rt,
            )
            .expect_err("missing pointer value must reject input");
        assert!(matches!(missing_error, NodeError::Precondition(_)));
        assert!(recorded.lock().is_empty());

        let mut mismatch = StructuredFieldExtractorNode::new(spec(
            serde_json::json!({"outputs": [{"id": "width", "pointer": "/fields/0", "type": "f64"}]}),
            vec![port("width", "data.field.f64.v1")],
        ))
        .expect("static port matches configured type");
        let mismatch_error = mismatch
            .on_input(
                INPUT_PORT,
                DataPacket::StructuredPacket(Arc::new(packet())),
                &mut rt,
            )
            .expect_err("datum type mismatch must reject input");
        assert!(matches!(mismatch_error, NodeError::Precondition(_)));
    }

    #[test]
    fn rejects_duplicate_definition_and_port_kind_mismatch() {
        let duplicate = StructuredFieldExtractorNode::new(spec(
            serde_json::json!({
                "outputs": [
                    {"id": "fx", "pointer": "/fields/1", "type": "f64"},
                    {"id": "fx", "pointer": "/fields/1", "type": "u32"},
                ],
            }),
            vec![port("fx", "data.field.f64.v1")],
        ))
        .expect_err("duplicate output ids must reject config");
        assert!(matches!(duplicate, NodeError::Config(_)));

        let kind_mismatch = StructuredFieldExtractorNode::new(spec(
            serde_json::json!({"outputs": [{"id": "fx", "pointer": "/fields/1", "type": "f64"}]}),
            vec![port("fx", "data.field.u32.v1")],
        ))
        .expect_err("static output port kind must match configured type");
        assert!(matches!(kind_mismatch, NodeError::Config(_)));
    }
}
