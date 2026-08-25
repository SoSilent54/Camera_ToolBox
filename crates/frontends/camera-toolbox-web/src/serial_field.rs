//! Web 工作流的显式序列号 typed-field source。
//!
//! 该节点不接触硬件；用户显式触发后才把已验证的序列号以及声明的 schema、provenance
//! 和 camera model 组成 `data.field.v1`。编码器按通用 FieldData 合同独立校验每个字段。

use std::sync::Arc;

use camera_toolbox_app::engine::{
    DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState,
    NodeSpec, TypedFieldSource,
};
use camera_toolbox_core::{Datum, PacketProvenance, TypedValue};

const OUTPUT_PORT: &str = "field";
const OUTPUT_KIND: &str = "data.field.v1";

pub(crate) struct SerialFieldFactory;

impl NodeFactory for SerialFieldFactory {
    fn kind(&self) -> &'static str {
        "serialField"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(SerialFieldNode::new(spec)?))
    }
}

struct SerialFieldNode {
    node_id: String,
    value: String,
    source: Arc<TypedFieldSource>,
    generation: u64,
}

impl SerialFieldNode {
    fn new(spec: NodeSpec) -> Result<Self, NodeError> {
        let config = spec
            .config
            .as_object()
            .ok_or_else(|| NodeError::Config("serialField config must be an object".to_owned()))?;
        let value = required_text(config, "value")?;
        validate_yg_snid(&value)?;
        let schema = required_text(config, "sourceSchema")?;
        let model_id = required_text(config, "sourceModelId")?;
        let output = spec
            .outputs
            .iter()
            .find(|port| port.id == OUTPUT_PORT)
            .ok_or_else(|| {
                NodeError::Config("serialField requires output `serial.number`".to_owned())
            })?;
        if output.kind != OUTPUT_KIND {
            return Err(NodeError::Config(format!(
                "serialField output `serial.number` must use `{OUTPUT_KIND}`, got `{}`",
                output.kind
            )));
        }
        Ok(Self {
            node_id: spec.id.clone(),
            value,
            source: Arc::new(TypedFieldSource::new(
                schema,
                PacketProvenance {
                    source_port: Some(format!("{}.{}", spec.id, OUTPUT_PORT)),
                    source_schema: Some("camera-toolbox.serial-field.v1".to_owned()),
                    ..PacketProvenance::default()
                },
                Some(model_id),
            )),
            generation: 1,
        })
    }

    fn emit(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let generation = self.generation;
        self.generation = self.generation.checked_add(1).ok_or_else(|| {
            NodeError::Execution("serial field generation counter exhausted".to_owned())
        })?;
        let datum = Datum::new("serial.number", TypedValue::Str(self.value.clone()))
            .with_semantic_type("device.serial-number");
        rt.emit(
            OUTPUT_PORT,
            DataPacket::TypedField {
                datum: Arc::new(datum),
                generation,
                source: Arc::clone(&self.source),
            },
        )?;
        rt.report_state(NodeRuntimeState::Ready, "serial field emitted");
        Ok(())
    }
}

impl NodeInstance for SerialFieldNode {
    fn kind(&self) -> &'static str {
        "serialField"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(
            NodeRuntimeState::Ready,
            "set serial field and trigger emission",
        );
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        _packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        Err(NodeError::Precondition(format!(
            "serialField has no input port `{port}`"
        )))
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        if matches!(action, NodeAction::Trigger) {
            return self.emit(rt);
        }
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }

    fn on_config_update(
        &mut self,
        config: serde_json::Value,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let spec = NodeSpec {
            id: self.node_id.clone(),
            kind: self.kind().to_owned(),
            title: "Serial Field".to_owned(),
            inputs: Vec::new(),
            outputs: vec![camera_toolbox_app::engine::PortSpec {
                id: OUTPUT_PORT.to_owned(),
                label: "Serial number".to_owned(),
                kind: OUTPUT_KIND.to_owned(),
                cardinality: camera_toolbox_app::engine::PortCardinality::One,
                required: false,
            }],
            config,
        };
        let replacement = Self::new(spec)?;
        self.value = replacement.value;
        self.source = replacement.source;
        self.generation = 1;
        rt.report_state(
            NodeRuntimeState::Ready,
            "serial field configuration updated",
        );
        Ok(())
    }
}

fn required_text(
    config: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<String, NodeError> {
    let value = config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| NodeError::Config(format!("serialField requires non-empty `{key}`")))?;
    if !value.chars().all(|character| !character.is_control()) {
        return Err(NodeError::Config(format!(
            "serialField `{key}` must not contain control characters"
        )));
    }
    Ok(value.to_owned())
}

fn validate_yg_snid(value: &str) -> Result<(), NodeError> {
    let serial = value.as_bytes();
    let valid = serial.len() == 14
        && matches!(&serial[..5], b"2T233" | b"2T235")
        && serial[5..7].iter().all(u8::is_ascii_digit)
        && matches!(serial[7], b'1'..=b'9' | b'A'..=b'C')
        && matches!(serial[8], b'1'..=b'9' | b'A'..=b'V')
        && matches!(serial[9], b'0'..=b'4')
        && serial[10..12].iter().all(u8::is_ascii_alphanumeric)
        && serial[12..] == *b"00";
    valid.then_some(()).ok_or_else(|| {
        NodeError::Config("serialField value must be a valid 14-byte YG SNID".to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::validate_yg_snid;

    #[test]
    fn yg_snid_validation_rejects_each_constrained_segment() {
        for serial in ["2T23326AV4ZZ00", "2T23599CV4ZZ00"] {
            assert!(validate_yg_snid(serial).is_ok(), "{serial}");
        }
        for serial in [
            "2T99926AV4ZZ00", // 前缀
            "2T233A6AV4ZZ00", // 年份
            "2T233260V4ZZ00", // 月份
            "2T23326A04ZZ00", // 日期
            "2T23326AV9ZZ00", // 班次
            "2T23326AV4!!00", // 机身字符
            "2T23326AV4ZZ01", // 后缀
            "2T23326AV4ZZ0",  // 长度
        ] {
            assert!(validate_yg_snid(serial).is_err(), "{serial}");
        }
    }
}
