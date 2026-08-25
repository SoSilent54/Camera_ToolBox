//! 通用结构化数据包的精确类型、校验与确定性编解码。
//!
//! 包的数值 wire value 始终是字符串，避免 JSON/YAML 消费端把 `u64` 或浮点数
//! 静默转换为不精确的宿主数值。`PrimitiveType` 是逻辑类型；设备的存储 ABI
//! （例如 little-endian `f32`）不属于此模块。

use std::{collections::BTreeSet, fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// 全局逻辑 primitive 类型注册表。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrimitiveType {
    Bool,
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
    Str,
    Bytes,
}

impl PrimitiveType {
    /// 返回稳定的 wire type 名称。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::U8 => "u8",
            Self::I8 => "i8",
            Self::U16 => "u16",
            Self::I16 => "i16",
            Self::U32 => "u32",
            Self::I32 => "i32",
            Self::U64 => "u64",
            Self::I64 => "i64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Str => "str",
            Self::Bytes => "bytes",
        }
    }

    /// 按该 primitive 类型严格解析字符串 wire value。
    ///
    /// 浮点值必须有限；`bytes` 使用偶数长度的十六进制字符串。
    pub fn parse(self, value: &str) -> Result<TypedValue, StructuredPacketError> {
        let typed = match self {
            Self::Bool => match value {
                "true" => TypedValue::Bool(true),
                "false" => TypedValue::Bool(false),
                _ => return Err(StructuredPacketError::InvalidBoolean(value.to_owned())),
            },
            Self::U8 => TypedValue::U8(parse_number(value, self)?),
            Self::I8 => TypedValue::I8(parse_number(value, self)?),
            Self::U16 => TypedValue::U16(parse_number(value, self)?),
            Self::I16 => TypedValue::I16(parse_number(value, self)?),
            Self::U32 => TypedValue::U32(parse_number(value, self)?),
            Self::I32 => TypedValue::I32(parse_number(value, self)?),
            Self::U64 => TypedValue::U64(parse_number(value, self)?),
            Self::I64 => TypedValue::I64(parse_number(value, self)?),
            Self::F32 => TypedValue::F32(parse_f32(value)?),
            Self::F64 => TypedValue::F64(parse_f64(value)?),
            Self::Str => TypedValue::Str(value.to_owned()),
            Self::Bytes => TypedValue::Bytes(decode_hex(value)?),
        };
        Ok(typed)
    }
}

impl fmt::Display for PrimitiveType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PrimitiveType {
    type Err = StructuredPacketError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "bool" => Ok(Self::Bool),
            "u8" => Ok(Self::U8),
            "i8" => Ok(Self::I8),
            "u16" => Ok(Self::U16),
            "i16" => Ok(Self::I16),
            "u32" => Ok(Self::U32),
            "i32" => Ok(Self::I32),
            "u64" => Ok(Self::U64),
            "i64" => Ok(Self::I64),
            "f32" => Ok(Self::F32),
            "f64" => Ok(Self::F64),
            "str" => Ok(Self::Str),
            "bytes" => Ok(Self::Bytes),
            _ => Err(StructuredPacketError::UnknownPrimitiveType(
                value.to_owned(),
            )),
        }
    }
}

/// 已按精确逻辑 primitive 类型解析的值。
#[derive(Clone, Debug, PartialEq)]
pub enum TypedValue {
    Bool(bool),
    U8(u8),
    I8(i8),
    U16(u16),
    I16(i16),
    U32(u32),
    I32(i32),
    U64(u64),
    I64(i64),
    F32(f32),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
}

impl TypedValue {
    /// 返回值的精确 primitive 类型。
    #[must_use]
    pub const fn primitive_type(&self) -> PrimitiveType {
        match self {
            Self::Bool(_) => PrimitiveType::Bool,
            Self::U8(_) => PrimitiveType::U8,
            Self::I8(_) => PrimitiveType::I8,
            Self::U16(_) => PrimitiveType::U16,
            Self::I16(_) => PrimitiveType::I16,
            Self::U32(_) => PrimitiveType::U32,
            Self::I32(_) => PrimitiveType::I32,
            Self::U64(_) => PrimitiveType::U64,
            Self::I64(_) => PrimitiveType::I64,
            Self::F32(_) => PrimitiveType::F32,
            Self::F64(_) => PrimitiveType::F64,
            Self::Str(_) => PrimitiveType::Str,
            Self::Bytes(_) => PrimitiveType::Bytes,
        }
    }

    /// 生成确定性的字符串 wire value。
    #[must_use]
    pub fn wire_value(&self) -> String {
        match self {
            Self::Bool(value) => value.to_string(),
            Self::U8(value) => value.to_string(),
            Self::I8(value) => value.to_string(),
            Self::U16(value) => value.to_string(),
            Self::I16(value) => value.to_string(),
            Self::U32(value) => value.to_string(),
            Self::I32(value) => value.to_string(),
            Self::U64(value) => value.to_string(),
            Self::I64(value) => value.to_string(),
            Self::F32(value) => value.to_string(),
            Self::F64(value) => value.to_string(),
            Self::Str(value) => value.clone(),
            Self::Bytes(value) => encode_hex(value),
        }
    }

    /// 检查不变量，尤其拒绝不能安全编码的非有限浮点数。
    pub fn validate(&self) -> Result<(), StructuredPacketError> {
        match self {
            Self::F32(value) if !value.is_finite() => {
                Err(StructuredPacketError::NonFiniteFloat(PrimitiveType::F32))
            }
            Self::F64(value) if !value.is_finite() => {
                Err(StructuredPacketError::NonFiniteFloat(PrimitiveType::F64))
            }
            _ => Ok(()),
        }
    }
}

/// 一个稳定命名的结构化叶子字段。
#[derive(Clone, Debug, PartialEq)]
pub struct Datum {
    pub name: String,
    pub value: TypedValue,
    pub unit: Option<String>,
    pub semantic_type: Option<String>,
}

impl Datum {
    #[must_use]
    pub fn new(name: impl Into<String>, value: TypedValue) -> Self {
        Self {
            name: name.into(),
            value,
            unit: None,
            semantic_type: None,
        }
    }

    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    #[must_use]
    pub fn with_semantic_type(mut self, semantic_type: impl Into<String>) -> Self {
        self.semantic_type = Some(semantic_type.into());
        self
    }

    #[must_use]
    pub const fn primitive_type(&self) -> PrimitiveType {
        self.value.primitive_type()
    }

    pub fn validate(&self) -> Result<(), StructuredPacketError> {
        validate_field_name(&self.name)?;
        validate_optional_text("unit", self.unit.as_deref())?;
        validate_optional_text("semanticType", self.semantic_type.as_deref())?;
        self.value.validate()
    }
}

/// 审计来源；不参与普通 datum 字段匹配。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PacketProvenance {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_port: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_packet_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_sequence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_timestamp_ns: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_timestamp_ns: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_timestamp_ns: Option<String>,
}

impl PacketProvenance {
    pub fn validate(&self) -> Result<(), StructuredPacketError> {
        for (name, value) in [
            ("sourcePort", self.source_port.as_deref()),
            ("artifactDigest", self.artifact_digest.as_deref()),
            ("sourcePacketDigest", self.source_packet_digest.as_deref()),
            ("sourceSchema", self.source_schema.as_deref()),
            ("frameSequence", self.frame_sequence.as_deref()),
            (
                "presentationTimestampNs",
                self.presentation_timestamp_ns.as_deref(),
            ),
            ("hostTimestampNs", self.host_timestamp_ns.as_deref()),
            ("deviceTimestampNs", self.device_timestamp_ns.as_deref()),
        ] {
            validate_optional_text(name, value)?;
        }
        Ok(())
    }
}

/// 可交换、可审计的结构化数据包。
#[derive(Clone, Debug, PartialEq)]
pub struct StructuredPacket {
    pub schema: String,
    pub provenance: PacketProvenance,
    pub fields: Vec<Datum>,
}

impl StructuredPacket {
    /// 构造并验证包。不自动推断单位、语义或来源。
    pub fn new(
        schema: impl Into<String>,
        provenance: PacketProvenance,
        mut fields: Vec<Datum>,
    ) -> Result<Self, StructuredPacketError> {
        fields.sort_by(|left, right| left.name.cmp(&right.name));
        let packet = Self {
            schema: schema.into(),
            provenance,
            fields,
        };
        packet.validate()?;
        Ok(packet)
    }

    /// 验证 schema、来源、字段命名、精确类型与重复字段。
    pub fn validate(&self) -> Result<(), StructuredPacketError> {
        validate_required_text("schema", &self.schema)?;
        self.provenance.validate()?;

        let mut names = BTreeSet::new();
        for field in &self.fields {
            field.validate()?;
            if !names.insert(&field.name) {
                return Err(StructuredPacketError::DuplicateFieldName(
                    field.name.clone(),
                ));
            }
        }
        Ok(())
    }

    /// 返回按字段名称排序、无空白的 canonical JSON。
    pub fn canonical_json(&self) -> Result<String, StructuredPacketError> {
        self.validate()?;
        serde_json::to_string(&WirePacket::from_packet(self)).map_err(StructuredPacketError::Json)
    }

    /// 返回按字段名称排序的 canonical YAML。
    pub fn canonical_yaml(&self) -> Result<String, StructuredPacketError> {
        self.validate()?;
        serde_yaml::to_string(&WirePacket::from_packet(self)).map_err(StructuredPacketError::Yaml)
    }

    /// 解析 JSON wire representation 并验证其精确类型与范围。
    pub fn from_json(input: &str) -> Result<Self, StructuredPacketError> {
        let wire =
            serde_json::from_str::<WirePacket>(input).map_err(StructuredPacketError::Json)?;
        Self::try_from(wire)
    }

    /// 解析 YAML wire representation 并验证其精确类型与范围。
    pub fn from_yaml(input: &str) -> Result<Self, StructuredPacketError> {
        let wire =
            serde_yaml::from_str::<WirePacket>(input).map_err(StructuredPacketError::Yaml)?;
        Self::try_from(wire)
    }

    /// canonical JSON 的 SHA-256 digest，固定带 `sha256:` 前缀。
    pub fn digest(&self) -> Result<String, StructuredPacketError> {
        let canonical = self.canonical_json()?;
        let digest = Sha256::digest(canonical.as_bytes());
        Ok(format!("sha256:{}", encode_hex(&digest)))
    }
}

impl Serialize for StructuredPacket {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        WirePacket::from_packet(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StructuredPacket {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        WirePacket::deserialize(deserializer)
            .and_then(|wire| Self::try_from(wire).map_err(serde::de::Error::custom))
    }
}

impl Serialize for Datum {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        WireDatum::from_datum(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Datum {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        WireDatum::deserialize(deserializer)
            .and_then(|wire| Datum::try_from(wire).map_err(serde::de::Error::custom))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WirePacket {
    schema: String,
    provenance: PacketProvenance,
    fields: Vec<WireDatum>,
}

impl WirePacket {
    fn from_packet(packet: &StructuredPacket) -> Self {
        let mut fields = packet
            .fields
            .iter()
            .map(WireDatum::from_datum)
            .collect::<Vec<_>>();
        fields.sort_by(|left, right| left.name.cmp(&right.name));
        Self {
            schema: packet.schema.clone(),
            provenance: packet.provenance.clone(),
            fields,
        }
    }
}

impl TryFrom<WirePacket> for StructuredPacket {
    type Error = StructuredPacketError;

    fn try_from(wire: WirePacket) -> Result<Self, Self::Error> {
        let fields = wire
            .fields
            .into_iter()
            .map(Datum::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(wire.schema, wire.provenance, fields)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireDatum {
    name: String,
    #[serde(rename = "type")]
    primitive_type: PrimitiveType,
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    semantic_type: Option<String>,
}

impl WireDatum {
    fn from_datum(datum: &Datum) -> Self {
        Self {
            name: datum.name.clone(),
            primitive_type: datum.primitive_type(),
            value: datum.value.wire_value(),
            unit: datum.unit.clone(),
            semantic_type: datum.semantic_type.clone(),
        }
    }
}

impl TryFrom<WireDatum> for Datum {
    type Error = StructuredPacketError;

    fn try_from(wire: WireDatum) -> Result<Self, Self::Error> {
        let datum = Self {
            name: wire.name,
            value: wire.primitive_type.parse(&wire.value)?,
            unit: wire.unit,
            semantic_type: wire.semantic_type,
        };
        datum.validate()?;
        Ok(datum)
    }
}

/// 结构化包编解码和不变量错误。
#[derive(Debug, Error)]
pub enum StructuredPacketError {
    #[error("unknown primitive type '{0}'")]
    UnknownPrimitiveType(String),
    #[error("value '{value}' is not a valid {primitive_type}")]
    InvalidNumber {
        primitive_type: PrimitiveType,
        value: String,
    },
    #[error("boolean wire value must be 'true' or 'false', got '{0}'")]
    InvalidBoolean(String),
    #[error("{0} must be finite")]
    NonFiniteFloat(PrimitiveType),
    #[error("bytes wire value must be an even-length hexadecimal string, got '{0}'")]
    InvalidBytes(String),
    #[error("{0} must not be empty")]
    EmptyText(&'static str),
    #[error("invalid stable field name '{0}'")]
    InvalidFieldName(String),
    #[error("duplicate datum name '{0}'")]
    DuplicateFieldName(String),
    #[error("JSON codec error: {0}")]
    Json(serde_json::Error),
    #[error("YAML codec error: {0}")]
    Yaml(serde_yaml::Error),
}

fn parse_number<T>(value: &str, primitive_type: PrimitiveType) -> Result<T, StructuredPacketError>
where
    T: FromStr,
{
    value
        .parse::<T>()
        .map_err(|_| StructuredPacketError::InvalidNumber {
            primitive_type,
            value: value.to_owned(),
        })
}

fn parse_f32(value: &str) -> Result<f32, StructuredPacketError> {
    let parsed: f32 = parse_number(value, PrimitiveType::F32)?;
    if parsed.is_finite() {
        Ok(parsed)
    } else {
        Err(StructuredPacketError::NonFiniteFloat(PrimitiveType::F32))
    }
}

fn parse_f64(value: &str) -> Result<f64, StructuredPacketError> {
    let parsed: f64 = parse_number(value, PrimitiveType::F64)?;
    if parsed.is_finite() {
        Ok(parsed)
    } else {
        Err(StructuredPacketError::NonFiniteFloat(PrimitiveType::F64))
    }
}

fn validate_required_text(name: &'static str, value: &str) -> Result<(), StructuredPacketError> {
    if value.trim().is_empty() {
        Err(StructuredPacketError::EmptyText(name))
    } else {
        Ok(())
    }
}

fn validate_optional_text(
    name: &'static str,
    value: Option<&str>,
) -> Result<(), StructuredPacketError> {
    if let Some(value) = value {
        validate_required_text(name, value)?;
    }
    Ok(())
}

fn validate_field_name(name: &str) -> Result<(), StructuredPacketError> {
    let invalid = || StructuredPacketError::InvalidFieldName(name.to_owned());
    let mut component_has_name = false;
    let mut in_index = false;
    let mut index_has_digit = false;
    let mut after_index = false;

    for byte in name.bytes() {
        if in_index {
            match byte {
                b'0'..=b'9' => index_has_digit = true,
                b']' if index_has_digit => {
                    in_index = false;
                    after_index = true;
                }
                _ => return Err(invalid()),
            }
            continue;
        }

        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' if !after_index => {
                component_has_name = true;
            }
            b'[' if component_has_name => {
                in_index = true;
                index_has_digit = false;
                after_index = false;
            }
            b'.' if component_has_name => {
                component_has_name = false;
                after_index = false;
            }
            _ => return Err(invalid()),
        }
    }

    if component_has_name && !in_index {
        Ok(())
    } else {
        Err(invalid())
    }
}

fn decode_hex(value: &str) -> Result<Vec<u8>, StructuredPacketError> {
    if value.len() % 2 != 0 {
        return Err(StructuredPacketError::InvalidBytes(value.to_owned()));
    }

    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high = hex_nibble(pair[0])
            .ok_or_else(|| StructuredPacketError::InvalidBytes(value.to_owned()))?;
        let low = hex_nibble(pair[1])
            .ok_or_else(|| StructuredPacketError::InvalidBytes(value.to_owned()))?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

const fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(fields: Vec<Datum>) -> StructuredPacket {
        StructuredPacket::new(
            "calib.solution.v1",
            PacketProvenance {
                source_port: Some("calib.solution".to_owned()),
                artifact_digest: Some("sha256:artifact".to_owned()),
                ..PacketProvenance::default()
            },
            fields,
        )
        .expect("valid packet")
    }

    #[test]
    fn parses_exact_width_integers_and_rejects_out_of_range_values() {
        assert!(matches!(
            PrimitiveType::U8.parse("255"),
            Ok(TypedValue::U8(255))
        ));
        assert!(matches!(
            PrimitiveType::U8.parse("256"),
            Err(StructuredPacketError::InvalidNumber { .. })
        ));
        assert!(matches!(
            PrimitiveType::I64.parse("-9223372036854775808"),
            Ok(TypedValue::I64(i64::MIN))
        ));
        assert!(matches!(
            PrimitiveType::I64.parse("9223372036854775808"),
            Err(StructuredPacketError::InvalidNumber { .. })
        ));
    }

    #[test]
    fn rejects_non_finite_floats() {
        for value in ["NaN", "inf", "-inf", "Infinity"] {
            assert!(matches!(
                PrimitiveType::F64.parse(value),
                Err(StructuredPacketError::NonFiniteFloat(PrimitiveType::F64))
                    | Err(StructuredPacketError::InvalidNumber { .. })
            ));
        }
        assert!(matches!(
            TypedValue::F32(f32::NAN).validate(),
            Err(StructuredPacketError::NonFiniteFloat(PrimitiveType::F32))
        ));
    }

    #[test]
    fn rejects_duplicate_or_invalid_field_names() {
        let field = Datum::new("camera.matrix.r00", TypedValue::F64(1.0));
        assert!(matches!(
            StructuredPacket::new(
                "calib.solution.v1",
                PacketProvenance::default(),
                vec![field.clone(), field]
            ),
            Err(StructuredPacketError::DuplicateFieldName(_))
        ));
        assert!(matches!(
            StructuredPacket::new(
                "calib.solution.v1",
                PacketProvenance::default(),
                vec![Datum::new("camera..fx", TypedValue::F64(1.0))]
            ),
            Err(StructuredPacketError::InvalidFieldName(_))
        ));
        assert!(
            Datum::new("detection.corners[0].x", TypedValue::F64(12.5))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn canonical_digest_is_independent_of_input_field_order() {
        let left = packet(vec![
            Datum::new("distortion.k1", TypedValue::F64(-0.12)),
            Datum::new("camera.intrinsics.fx", TypedValue::F64(912.43)).with_unit("px"),
        ]);
        let right = packet(vec![
            Datum::new("camera.intrinsics.fx", TypedValue::F64(912.43)).with_unit("px"),
            Datum::new("distortion.k1", TypedValue::F64(-0.12)),
        ]);

        assert_eq!(
            left.canonical_json().unwrap(),
            right.canonical_json().unwrap()
        );
        assert_eq!(left.digest().unwrap(), right.digest().unwrap());
        assert!(left.digest().unwrap().starts_with("sha256:"));
    }

    #[test]
    fn canonical_json_and_yaml_round_trip_with_string_numeric_values() {
        let original = packet(vec![
            Datum::new("camera.intrinsics.fx", TypedValue::F64(912.43))
                .with_unit("px")
                .with_semantic_type("camera.focal-length"),
            Datum::new("camera.image.width", TypedValue::U32(3840)),
            Datum::new("calibration.flags", TypedValue::U64(u64::MAX)),
            Datum::new("calibration.enabled", TypedValue::Bool(true)),
            Datum::new(
                "artifact.digest",
                TypedValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            ),
        ]);

        let json = original.canonical_json().unwrap();
        assert!(json.contains("\"value\":\"18446744073709551615\""));
        assert_eq!(StructuredPacket::from_json(&json).unwrap(), original);

        let yaml = original.canonical_yaml().unwrap();
        assert!(
            yaml.contains("value: '18446744073709551615'")
                || yaml.contains("value: \"18446744073709551615\"")
        );
        assert_eq!(StructuredPacket::from_yaml(&yaml).unwrap(), original);
    }

    #[test]
    fn wire_type_and_value_must_match() {
        let json = r#"{
            "schema":"calib.solution.v1",
            "provenance":{},
            "fields":[{"name":"camera.image.width","type":"u8","value":"256"}]
        }"#;
        assert!(matches!(
            StructuredPacket::from_json(json),
            Err(StructuredPacketError::InvalidNumber {
                primitive_type: PrimitiveType::U8,
                ..
            })
        ));
    }
}
