//! 受控 I²C 标定映射的定义、YAML 编译与确定性字节编码。
//!
//! 逻辑输入与 EEPROM 存储 ABI 被刻意分离：[`PrimitiveType`] 描述上游 datum 的
//! 精确类型，而 [`StorageEncoding`] 保留既有 EEPROM 的字节 ABI。映射在 core 中
//! 完成全部转换、范围校验、舍入、校验和及页分段，因此上层只能消费已编译的字节。

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(test)]
use crate::calibration_eeprom::{
    BATON_PARAM_RW_NATIVE_LP64_LE_V1_MAP_ID, PUEO_EDU_DF9_40_NATIVE_LP64_LE_V1_MAP_ID,
    YG_STEREO_P24C64G_V1_MAP_ID,
};
use crate::{
    Datum, PrimitiveType, TypedValue,
    calibration_eeprom::{
        BATON_PARAM_RW_IMAGE_BYTES, CalibrationStorageMap, EepromTransportSpec,
        PUEO_EDU_DF9_40_IMAGE_BYTES, StorageEncoding, StorageField, YG_STEREO_P24C64G_FLAG,
        YG_STEREO_P24C64G_IMAGE_BYTES, baton_param_rw_native_lp64_le_v1,
        pueo_edu_df9_40_native_lp64_le_v1, yg_stereo_p24c64g_v1,
    },
};

/// 自定义 map YAML 的稳定 schema 名。
pub const I2C_MAP_SCHEMA: &str = "camera-toolbox.i2c-map.v1";

/// I²C EEPROM 映射的完整、可编码定义。
#[derive(Clone, Debug, PartialEq)]
pub struct I2cMapDefinition {
    pub id: String,
    pub display_name: String,
    /// 由 storage binding 的最大终止偏移推导，不能由 YAML 任意声明。
    pub image_bytes: u16,
    pub target: I2cMapTarget,
    pub fields: Vec<I2cMapStorageField>,
    /// map 固定字节（如有效 FLAG）；它们也是编码和页写合同的一部分。
    pub fixed_bytes: Vec<I2cMapFixedBytes>,
    /// 被允许产生该 map 输入的结构化包 schema 和相机模型标识。
    pub accepts: I2cMapAccepts,
    /// 所有入口都是命名、精确类型、语义和单位均受约束的逻辑 slot。
    pub inputs: Vec<LogicalInputSlot>,
    pub checksums: Vec<ChecksumContract>,
    /// 页写必须显式声明，禁止 executor 通过猜测 map 行为补全策略。
    pub page_policy: PageWritePolicy,
    /// 写前读取范围和要求是 map 合同的一部分。
    pub read_before: ReadBeforePolicy,
    /// 页写后的读回校验范围和要求是 map 合同的一部分。
    pub readback: ReadbackPolicy,
}

impl I2cMapDefinition {
    /// 验证上游包 schema 和 camera model 都受此 map 明确允许。
    pub fn validate_source(
        &self,
        schema: &str,
        model_id: &str,
    ) -> Result<(), I2cMapDefinitionError> {
        if !self
            .accepts
            .schemas
            .iter()
            .any(|candidate| candidate == schema)
        {
            return Err(I2cMapDefinitionError::Definition(format!(
                "map `{}` does not accept schema `{schema}`",
                self.id
            )));
        }
        if !self
            .accepts
            .model_ids
            .iter()
            .any(|candidate| candidate == model_id)
        {
            return Err(I2cMapDefinitionError::Definition(format!(
                "map `{}` does not accept model id `{model_id}`",
                self.id
            )));
        }
        Ok(())
    }
}

/// map 可接受的上游来源。YAML 以 `accepts.schemas` 和 `accepts.modelIds` 表示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cMapAccepts {
    pub schemas: Vec<String>,
    pub model_ids: Vec<String>,
}

/// Map 锁定的 I²C 总线、地址和 EEPROM 页协议。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct I2cMapTarget {
    pub bus: u32,
    pub transport: EepromTransportSpec,
}
impl I2cMapDefinition {
    /// 严格解析并校验自定义 YAML；所有语义错误携带最佳可用的源位置。
    pub fn from_yaml(text: &str) -> Result<Self, I2cMapDefinitionError> {
        parse_i2c_map_yaml(text)
    }

    /// 对完整逻辑输入集合编码 EEPROM 镜像并按页边界拆分写段。
    pub fn encode(&self, inputs: &[Datum]) -> Result<I2cMapImage, I2cMapDefinitionError> {
        self.validate()?;
        let mut supplied = BTreeMap::new();
        for input in inputs {
            input
                .validate()
                .map_err(|error| I2cMapDefinitionError::Input {
                    slot: input.name.clone(),
                    message: error.to_string(),
                })?;
            if supplied.insert(input.name.as_str(), input).is_some() {
                return Err(I2cMapDefinitionError::Input {
                    slot: input.name.clone(),
                    message: "input occurs more than once".to_owned(),
                });
            }
        }
        let mut image = vec![0_u8; usize::from(self.image_bytes)];
        let mut written_ranges = Vec::with_capacity(self.inputs.len() + self.fixed_bytes.len());
        for fixed in &self.fixed_bytes {
            fixed.write(&mut image)?;
            written_ranges.push((
                fixed.offset,
                u16::try_from(fixed.bytes.len()).expect("fixed bytes validated"),
            ));
        }
        for slot in &self.inputs {
            let Some(datum) = supplied.remove(slot.name.as_str()) else {
                if slot.required {
                    return Err(I2cMapDefinitionError::Input {
                        slot: slot.name.clone(),
                        message: "required logical input is absent".to_owned(),
                    });
                }
                continue;
            };
            slot.encode(datum, &mut image)?;
            if let Some(target) = &slot.target {
                written_ranges.push((target.offset, target.byte_len));
            }
        }
        if let Some((name, _)) = supplied.into_iter().next() {
            return Err(I2cMapDefinitionError::Input {
                slot: name.to_owned(),
                message: "input is not declared by this map".to_owned(),
            });
        }
        for checksum in &self.checksums {
            checksum.write(self, &mut image)?;
            written_ranges.push((checksum.target_offset, 1));
        }
        let pages = page_segments(
            &image,
            &written_ranges,
            self.target.transport.page_size_bytes,
        )?;
        Ok(I2cMapImage {
            bytes: image,
            pages,
        })
    }

    /// 验证 map 不变量，不依赖 YAML 来源。
    pub fn validate(&self) -> Result<(), I2cMapDefinitionError> {
        if self.id.trim().is_empty() || self.display_name.trim().is_empty() {
            return Err(I2cMapDefinitionError::Definition(
                "map id and display_name must not be empty".to_owned(),
            ));
        }
        if self.image_bytes == 0 || self.target.transport.page_size_bytes == 0 {
            return Err(I2cMapDefinitionError::Definition(
                "image_bytes and target.page_size_bytes must be non-zero".to_owned(),
            ));
        }
        if !matches!(self.target.transport.address_width_bits, 8 | 16) {
            return Err(I2cMapDefinitionError::Definition(
                "target.address_width_bits must be 8 or 16".to_owned(),
            ));
        }
        let mut field_names = BTreeSet::new();
        let mut ranges = Vec::with_capacity(self.fields.len());
        for field in &self.fields {
            if field.name.trim().is_empty() || !field_names.insert(field.name.as_str()) {
                return Err(I2cMapDefinitionError::Definition(format!(
                    "storage field name `{}` is empty or duplicated",
                    field.name
                )));
            }
            validate_storage_range(field.offset, field.byte_len, self.image_bytes, &field.name)?;
            validate_encoding_width(field.encoding, field.byte_len, &field.name)?;
            ranges.push((field.offset, field.end()?, field.name.as_str()));
        }
        ranges.sort_by_key(|(start, ..)| *start);
        for pair in ranges.windows(2) {
            if pair[0].1 > pair[1].0 {
                return Err(I2cMapDefinitionError::Definition(format!(
                    "storage fields `{}` and `{}` overlap",
                    pair[0].2, pair[1].2
                )));
            }
        }
        for fixed in &self.fixed_bytes {
            fixed.validate(self.image_bytes)?;
        }
        let mut slots = BTreeSet::new();
        for slot in &self.inputs {
            if slot.name.trim().is_empty() || !slots.insert(slot.name.as_str()) {
                return Err(I2cMapDefinitionError::Definition(format!(
                    "logical input slot `{}` is empty or duplicated",
                    slot.name
                )));
            }
            slot.validate(self)?;
        }
        for checksum in &self.checksums {
            checksum.validate(self)?;
        }
        self.page_policy.validate()?;
        self.read_before.validate(self)?;
        self.readback.validate(self)?;
        Ok(())
    }
}

/// EEPROM 中一个物理字段。`StorageEncoding` 是现有 ABI，绝不可由 YAML 重定义。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cMapStorageField {
    pub name: String,
    pub offset: u16,
    pub byte_len: u16,
    /// 输入 slot 名；`None` 表示该字段只读、保留或由 checksum 合同生成。
    #[serde(default)]
    pub source: Option<String>,
    pub encoding: StorageEncoding,
    #[serde(default)]
    pub writable: bool,
}

/// 映射定义的固定 EEPROM 字节，例如在所有 payload 校验完成后写入的 FLAG。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cMapFixedBytes {
    pub offset: u16,
    pub bytes: Vec<u8>,
}

impl I2cMapFixedBytes {
    fn validate(&self, image_bytes: u16) -> Result<(), I2cMapDefinitionError> {
        let byte_len = u16::try_from(self.bytes.len()).map_err(|_| {
            I2cMapDefinitionError::Definition(
                "fixed byte payload exceeds u16 address space".to_owned(),
            )
        })?;
        validate_storage_range(self.offset, byte_len, image_bytes, "fixed byte payload")
    }

    fn write(&self, image: &mut [u8]) -> Result<(), I2cMapDefinitionError> {
        let end = usize::from(self.offset)
            .checked_add(self.bytes.len())
            .ok_or_else(|| {
                I2cMapDefinitionError::Definition("fixed byte range overflows".to_owned())
            })?;
        let target = image
            .get_mut(usize::from(self.offset)..end)
            .ok_or_else(|| {
                I2cMapDefinitionError::Definition("fixed byte range lies outside image".to_owned())
            })?;
        target.copy_from_slice(&self.bytes);
        Ok(())
    }
}

impl I2cMapStorageField {
    fn end(&self) -> Result<u16, I2cMapDefinitionError> {
        self.offset.checked_add(self.byte_len).ok_or_else(|| {
            I2cMapDefinitionError::Definition(format!(
                "storage field `{}` range overflows",
                self.name
            ))
        })
    }
}

/// 上游 datum 与 EEPROM 字节范围之间的严格绑定。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalInputSlot {
    pub name: String,
    pub primitive_type: PrimitiveType,
    pub required: bool,
    #[serde(default)]
    pub semantic_type: Option<String>,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub target: Option<StorageTarget>,
    #[serde(default)]
    pub conversion: Option<NumericConversion>,
    #[serde(default)]
    pub exact_string: Option<String>,
}

impl LogicalInputSlot {
    fn validate(&self, map: &I2cMapDefinition) -> Result<(), I2cMapDefinitionError> {
        if self.exact_string.is_some() && self.primitive_type != PrimitiveType::Str {
            return Err(I2cMapDefinitionError::Definition(format!(
                "logical input `{}` exact_string requires primitive_type str",
                self.name
            )));
        }
        let Some(target) = &self.target else {
            if self.conversion.is_some() {
                return Err(I2cMapDefinitionError::Definition(format!(
                    "constraint-only logical input `{}` cannot have conversion",
                    self.name
                )));
            }
            return Ok(());
        };
        validate_storage_range(target.offset, target.byte_len, map.image_bytes, &self.name)?;
        validate_encoding_width(target.encoding, target.byte_len, &self.name)?;
        let target_end = target.offset.checked_add(target.byte_len).ok_or_else(|| {
            I2cMapDefinitionError::Definition(format!(
                "logical input `{}` range overflows",
                self.name
            ))
        })?;
        let backing = map.fields.iter().find(|field| {
            field.offset <= target.offset
                && field.end().is_ok_and(|end| end >= target_end)
                && field.encoding == target.encoding
        });
        if backing.is_none() {
            return Err(I2cMapDefinitionError::Definition(format!(
                "logical input `{}` must target a matching declared storage field",
                self.name
            )));
        }
        let numeric = is_numeric_storage(target.encoding);
        if numeric != self.conversion.is_some() {
            return Err(I2cMapDefinitionError::Definition(format!(
                "logical input `{}` must {} a numeric conversion",
                self.name,
                if numeric { "declare" } else { "not declare" }
            )));
        }
        if numeric
            && !matches!(
                self.primitive_type,
                PrimitiveType::F32
                    | PrimitiveType::F64
                    | PrimitiveType::U8
                    | PrimitiveType::U16
                    | PrimitiveType::U32
                    | PrimitiveType::U64
                    | PrimitiveType::I8
                    | PrimitiveType::I16
                    | PrimitiveType::I32
                    | PrimitiveType::I64
            )
        {
            return Err(I2cMapDefinitionError::Definition(format!(
                "numeric logical input `{}` has non-numeric primitive type",
                self.name
            )));
        }
        if let Some(conversion) = &self.conversion {
            conversion.validate(&self.name)?;
        }
        Ok(())
    }

    fn encode(&self, datum: &Datum, image: &mut [u8]) -> Result<(), I2cMapDefinitionError> {
        if datum.name != self.name || datum.primitive_type() != self.primitive_type {
            return Err(I2cMapDefinitionError::Input {
                slot: self.name.clone(),
                message: format!(
                    "requires datum `{}` with primitive type {}",
                    self.name, self.primitive_type
                ),
            });
        }
        if datum.semantic_type.as_deref() != self.semantic_type.as_deref()
            || datum.unit.as_deref() != self.unit.as_deref()
        {
            return Err(I2cMapDefinitionError::Input {
                slot: self.name.clone(),
                message: "datum semantic_type or unit does not match the slot contract".to_owned(),
            });
        }
        if let Some(expected) = &self.exact_string {
            let TypedValue::Str(actual) = &datum.value else {
                unreachable!("validated primitive type")
            };
            if actual != expected {
                return Err(I2cMapDefinitionError::Input {
                    slot: self.name.clone(),
                    message: format!("requires exact string `{expected}`, got `{actual}`"),
                });
            }
        }
        let Some(target) = &self.target else {
            return Ok(());
        };
        let output = image
            .get_mut(usize::from(target.offset)..usize::from(target.offset + target.byte_len))
            .expect("validated target range");
        match target.encoding {
            StorageEncoding::Ascii | StorageEncoding::AsciiNulTerminated => {
                let TypedValue::Str(value) = &datum.value else {
                    return Err(I2cMapDefinitionError::Input {
                        slot: self.name.clone(),
                        message: "ASCII storage requires str input".to_owned(),
                    });
                };
                if !value.is_ascii() || value.len() > output.len() {
                    return Err(I2cMapDefinitionError::Input {
                        slot: self.name.clone(),
                        message: format!(
                            "ASCII input must contain at most {} ASCII bytes",
                            output.len()
                        ),
                    });
                }
                output[..value.len()].copy_from_slice(value.as_bytes());
            }
            StorageEncoding::Raw => {
                let TypedValue::Bytes(value) = &datum.value else {
                    return Err(I2cMapDefinitionError::Input {
                        slot: self.name.clone(),
                        message: "RAW storage requires bytes input".to_owned(),
                    });
                };
                if value.len() != output.len() {
                    return Err(I2cMapDefinitionError::Input {
                        slot: self.name.clone(),
                        message: format!("RAW input must contain exactly {} bytes", output.len()),
                    });
                }
                output.copy_from_slice(value);
            }
            StorageEncoding::Reserved | StorageEncoding::SerialChecksum => {
                unreachable!("reserved/checksum storage cannot be logical target")
            }
            encoding => {
                let conversion = self.conversion.as_ref().expect("validated numeric target");
                let value = conversion.apply(
                    numeric_value(&datum.value, &self.name)?,
                    encoding,
                    &self.name,
                )?;
                write_numeric(output, encoding, value, &self.name)?;
            }
        }
        Ok(())
    }
}

/// 一个逻辑槽所写入的 EEPROM 子范围。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageTarget {
    pub offset: u16,
    pub byte_len: u16,
    pub encoding: StorageEncoding,
}

/// 数值输入的显式换算、范围和量化契约。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumericConversion {
    pub scale: f64,
    pub offset: f64,
    pub minimum: f64,
    pub maximum: f64,
    pub rounding: RoundingMode,
}

impl NumericConversion {
    fn validate(&self, slot: &str) -> Result<(), I2cMapDefinitionError> {
        if !self.scale.is_finite()
            || !self.offset.is_finite()
            || !self.minimum.is_finite()
            || !self.maximum.is_finite()
            || self.minimum > self.maximum
        {
            return Err(I2cMapDefinitionError::Definition(format!(
                "logical input `{slot}` has non-finite or inverted numeric conversion range"
            )));
        }
        Ok(())
    }

    fn apply(
        &self,
        input: f64,
        encoding: StorageEncoding,
        slot: &str,
    ) -> Result<f64, I2cMapDefinitionError> {
        let converted = input.mul_add(self.scale, self.offset);
        if !converted.is_finite() || converted < self.minimum || converted > self.maximum {
            return Err(I2cMapDefinitionError::Input {
                slot: slot.to_owned(),
                message: format!(
                    "converted value {converted} is outside [{}, {}]",
                    self.minimum, self.maximum
                ),
            });
        }
        let encoded = if is_integer_storage(encoding) {
            match self.rounding {
                RoundingMode::Exact => {
                    if converted.fract() != 0.0 {
                        return Err(I2cMapDefinitionError::Input {
                            slot: slot.to_owned(),
                            message: "integer storage requires an exact integer".to_owned(),
                        });
                    }
                    converted
                }
                RoundingMode::Floor => converted.floor(),
                RoundingMode::Ceiling => converted.ceil(),
                RoundingMode::TowardZero => converted.trunc(),
                RoundingMode::NearestTiesToEven => round_ties_to_even(converted),
            }
        } else {
            // IEEE-754 storage conversion uses hardware round-to-nearest-even; the declared
            // rounding mode documents the logical integer quantization only.
            converted
        };
        if encoded < numeric_minimum(encoding) || encoded > numeric_maximum(encoding) {
            return Err(I2cMapDefinitionError::Input {
                slot: slot.to_owned(),
                message: format!(
                    "post-rounding value {encoded} exceeds {encoding:?} storage range"
                ),
            });
        }
        Ok(encoded)
    }
}

/// 整数目标的量化规则。浮点目标固定采用 IEEE-754 nearest-ties-to-even 转换。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundingMode {
    Exact,
    Floor,
    Ceiling,
    TowardZero,
    NearestTiesToEven,
}

/// 已编码的校验和字段及其覆盖范围。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChecksumContract {
    pub target_offset: u16,
    /// 设备定义的连续物理字节范围；必须和 `source_fields` 二选一。
    pub source_offset: Option<u16>,
    pub source_byte_len: Option<u16>,
    /// 已映射逻辑字段的物理跨度，按声明顺序拼接后参与校验和。
    #[serde(default)]
    pub source_fields: Vec<String>,
    pub algorithm: ChecksumAlgorithm,
}

impl ChecksumContract {
    fn source_ranges(&self, map: &I2cMapDefinition) -> Result<Vec<(u16, u16)>, I2cMapDefinitionError> {
        match (self.source_offset, self.source_byte_len, self.source_fields.is_empty()) {
            (Some(offset), Some(byte_len), true) => Ok(vec![(offset, byte_len)]),
            (None, None, false) => {
                let mut names = BTreeSet::new();
                self.source_fields.iter().map(|name| {
                    if !names.insert(name.as_str()) {
                        return Err(I2cMapDefinitionError::Definition(format!("checksum source field `{name}` is duplicated")));
                    }
                    let slot = map.inputs.iter().find(|slot| slot.name == *name).ok_or_else(|| {
                        I2cMapDefinitionError::Definition(format!("checksum source field `{name}` is not declared"))
                    })?;
                    let target = slot.target.as_ref().ok_or_else(|| {
                        I2cMapDefinitionError::Definition(format!("checksum source field `{name}` has no encoded storage span"))
                    })?;
                    Ok((target.offset, target.byte_len))
                }).collect()
            }
            _ => Err(I2cMapDefinitionError::Definition(
                "checksum requires either sourceOffset/sourceByteLen or non-empty sourceFields".to_owned(),
            )),
        }
    }

    fn validate(&self, map: &I2cMapDefinition) -> Result<(), I2cMapDefinitionError> {
        let ranges = self.source_ranges(map)?;
        for (offset, byte_len) in &ranges {
            validate_storage_range(*offset, *byte_len, map.image_bytes, "checksum source")?;
            if self.target_offset >= *offset && self.target_offset < *offset + *byte_len {
                return Err(I2cMapDefinitionError::Definition(
                    "checksum target must not lie in its source range".to_owned(),
                ));
            }
        }
        validate_storage_range(self.target_offset, 1, map.image_bytes, "checksum target")?;
        Ok(())
    }

    fn write(&self, map: &I2cMapDefinition, image: &mut [u8]) -> Result<(), I2cMapDefinitionError> {
        let ranges = self.source_ranges(map)?;
        let sum = ranges.into_iter().try_fold(0_u32, |sum, (offset, byte_len)| {
            let source = image.get(usize::from(offset)..usize::from(offset + byte_len)).ok_or_else(|| {
                I2cMapDefinitionError::Definition("checksum source range is invalid".to_owned())
            })?;
            Ok::<_, I2cMapDefinitionError>(source.iter().fold(sum, |total, byte| total + u32::from(*byte)))
        })?;
        let value = match self.algorithm {
            ChecksumAlgorithm::SerialSumMod255PlusOne => ((sum % 0xff) + 1) as u8,
        };
        *image.get_mut(usize::from(self.target_offset)).ok_or_else(|| {
            I2cMapDefinitionError::Definition("checksum target range is invalid".to_owned())
        })? = value;
        Ok(())
    }
}

/// 已知 EEPROM checksum 算法；新增算法必须显式加入该 enum，禁止 YAML 表达式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumAlgorithm {
    SerialSumMod255PlusOne,
}
/// 页写策略。该策略故意没有隐式默认值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageWritePolicy {
    SplitAtBoundary,
}

impl PageWritePolicy {
    fn validate(self) -> Result<(), I2cMapDefinitionError> {
        Ok(())
    }
}

/// 写前读取的精确范围；range 使用 EEPROM 相对字节偏移。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadBeforePolicy {
    pub required: bool,
    pub ranges: Vec<I2cReadRange>,
}

impl ReadBeforePolicy {
    fn validate(&self, map: &I2cMapDefinition) -> Result<(), I2cMapDefinitionError> {
        if self.required && self.ranges.is_empty() {
            return Err(I2cMapDefinitionError::Definition(
                "read_before.required requires at least one range".to_owned(),
            ));
        }
        for range in &self.ranges {
            validate_storage_range(
                range.offset,
                range.byte_len,
                map.image_bytes,
                "read_before range",
            )?;
        }
        Ok(())
    }
}

/// 页写后必须执行的读回验证合同。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadbackPolicy {
    pub required: bool,
    pub verification: ReadbackVerification,
}

impl ReadbackPolicy {
    fn validate(&self, _map: &I2cMapDefinition) -> Result<(), I2cMapDefinitionError> {
        Ok(())
    }
}

/// 读回验证的覆盖范围；禁止由 executor 自行推断。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadbackVerification {
    ExactWrittenRanges,
    FullReadBeforeImage,
}

/// I²C inspect 的连续读取范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct I2cReadRange {
    pub offset: u16,
    pub byte_len: u16,
}

/// 可直接交给 I²C executor 的确定性镜像和页安全写段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cMapImage {
    pub bytes: Vec<u8>,
    pub pages: Vec<I2cMapPage>,
}

/// 一页内的连续写入段。该段绝不跨 EEPROM 页边界。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cMapPage {
    pub offset: u16,
    pub bytes: Vec<u8>,
}

/// YAML 或运行时 map 合同的错误。
#[derive(Debug, Error, PartialEq)]
pub enum I2cMapDefinitionError {
    #[error("I²C map source error at line {line}, column {column}: {message}")]
    Source {
        line: usize,
        column: usize,
        message: String,
    },
    #[error("invalid I²C map definition: {0}")]
    Definition(String),
    #[error("logical input `{slot}`: {message}")]
    Input { slot: String, message: String },
}

/// 解析自定义 map YAML。该自由函数适合不需要构造对象的调用方。
pub fn parse_i2c_map_yaml(text: &str) -> Result<I2cMapDefinition, I2cMapDefinitionError> {
    let raw: RawI2cMapDefinition = serde_yaml::from_str(text).map_err(|error| {
        let (line, column) = error
            .location()
            .map_or((1, 1), |location| (location.line(), location.column()));
        I2cMapDefinitionError::Source {
            line,
            column,
            message: error.to_string(),
        }
    })?;
    if raw.schema != I2C_MAP_SCHEMA {
        return Err(source_error(
            text,
            "schema",
            format!("schema must be `{I2C_MAP_SCHEMA}`"),
        ));
    }
    let fields = raw
        .storage
        .into_iter()
        .enumerate()
        .map(|(index, field)| {
            field.into_field().map_err(|error| {
                source_error_occurrence(text, "encoding", index, error.to_string())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let source_targets = fields
        .iter()
        .filter_map(|field| {
            field.source.clone().map(|source| {
                (
                    source,
                    StorageTarget {
                        offset: field.offset,
                        byte_len: field.byte_len,
                        encoding: field.encoding,
                    },
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    let checksums = raw
        .checksums
        .into_iter()
        .map(RawChecksumContract::into_checksum)
        .collect::<Result<Vec<_>, _>>()?;
    let image_bytes = fields
        .iter()
        .map(I2cMapStorageField::end)
        .chain(checksums.iter().map(|checksum| {
            checksum.target_offset.checked_add(1).ok_or_else(|| {
                I2cMapDefinitionError::Definition("checksum target range overflows".to_owned())
            })
        }))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    let definition = I2cMapDefinition {
        id: raw.id,
        display_name: raw.display_name,
        image_bytes,
        target: I2cMapTarget {
            bus: raw.target.bus,
            transport: EepromTransportSpec {
                i2c_address: raw.target.address,
                address_width_bits: raw.target.address_width_bits,
                page_size_bytes: raw.target.page_size_bytes,
                write_cycle_ms: raw.target.write_cycle_ms,
            },
        },
        fields,
        fixed_bytes: Vec::new(),
        accepts: I2cMapAccepts {
            schemas: raw.accepts.schemas,
            model_ids: raw.accepts.model_ids,
        },
        inputs: raw
            .inputs
            .into_iter()
            .map(|input| {
                let target = source_targets.get(input.name.as_str()).cloned();
                input.into_slot(target)
            })
            .collect::<Result<_, _>>()?,
        checksums,
        page_policy: raw.target.page_policy.into_policy()?,
        read_before: ReadBeforePolicy {
            required: raw.target.read_before.required,
            ranges: raw
                .target
                .read_before
                .ranges
                .into_iter()
                .map(|range| I2cReadRange {
                    offset: range.offset,
                    byte_len: range.byte_len,
                })
                .collect(),
        },
        readback: ReadbackPolicy {
            required: raw.target.readback.required,
            verification: raw.target.readback.into_verification()?,
        },
    };
    definition.validate().map_err(|error| match error {
        I2cMapDefinitionError::Definition(message) => {
            let key = semantic_error_key(&message).to_owned();
            source_error(text, &key, message)
        }
        other => other,
    })?;
    Ok(definition)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawI2cMapDefinition {
    schema: String,
    id: String,
    display_name: String,
    accepts: RawI2cMapAccepts,
    target: RawTarget,
    storage: Vec<RawI2cMapStorageField>,
    inputs: Vec<RawLogicalInputSlot>,
    #[serde(default)]
    checksums: Vec<RawChecksumContract>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawTarget {
    bus: u32,
    address: u8,
    address_width_bits: u8,
    page_size_bytes: u16,
    write_cycle_ms: u16,
    page_policy: RawPageWritePolicy,
    read_before: RawReadBeforePolicy,
    readback: RawReadbackPolicy,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawI2cMapAccepts {
    schemas: Vec<String>,
    model_ids: Vec<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawI2cMapStorageField {
    source: String,
    offset: u16,
    #[serde(default)]
    byte_len: Option<u16>,
    encoding: String,
    #[serde(default = "default_writable")]
    writable: bool,
}
const fn default_writable() -> bool {
    true
}
impl RawI2cMapStorageField {
    fn into_field(self) -> Result<I2cMapStorageField, I2cMapDefinitionError> {
        let encoding = parse_storage_encoding(&self.encoding)?;
        let byte_len = self
            .byte_len
            .or_else(|| fixed_storage_width(encoding))
            .ok_or_else(|| {
                I2cMapDefinitionError::Definition(format!(
                    "storage binding `{}` with {} requires byteLen",
                    self.source, self.encoding
                ))
            })?;
        Ok(I2cMapStorageField {
            name: self.source.clone(),
            offset: self.offset,
            byte_len,
            source: Some(self.source),
            encoding,
            writable: self.writable,
        })
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawLogicalInputSlot {
    name: String,
    #[serde(rename = "type")]
    primitive_type: PrimitiveType,
    required: bool,
    #[serde(default)]
    semantic_type: Option<String>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    conversion: Option<NumericConversion>,
    #[serde(default)]
    exact_string: Option<String>,
}
impl RawLogicalInputSlot {
    fn into_slot(
        self,
        target: Option<StorageTarget>,
    ) -> Result<LogicalInputSlot, I2cMapDefinitionError> {
        Ok(LogicalInputSlot {
            name: self.name,
            primitive_type: self.primitive_type,
            required: self.required,
            semantic_type: self.semantic_type,
            unit: self.unit,
            target,
            conversion: self.conversion,
            exact_string: self.exact_string,
        })
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawChecksumContract {
    target_offset: u16,
    source_offset: Option<u16>,
    source_byte_len: Option<u16>,
    #[serde(default)]
    source_fields: Vec<String>,
    algorithm: String,
}
impl RawChecksumContract {
    fn into_checksum(self) -> Result<ChecksumContract, I2cMapDefinitionError> {
        Ok(ChecksumContract {
            target_offset: self.target_offset,
            source_offset: self.source_offset,
            source_byte_len: self.source_byte_len,
            source_fields: self.source_fields,
            algorithm: match self.algorithm.as_str() {
                "serial-sum-mod-255-plus-one" => ChecksumAlgorithm::SerialSumMod255PlusOne,
                _ => {
                    return Err(I2cMapDefinitionError::Definition(format!(
                        "unsupported checksum algorithm `{}`",
                        self.algorithm
                    )));
                }
            },
        })
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawPageWritePolicy {
    mode: String,
}
impl RawPageWritePolicy {
    fn into_policy(self) -> Result<PageWritePolicy, I2cMapDefinitionError> {
        match self.mode.as_str() {
            "split-at-boundary" => Ok(PageWritePolicy::SplitAtBoundary),
            _ => Err(I2cMapDefinitionError::Definition(format!(
                "unsupported pagePolicy.mode `{}`",
                self.mode
            ))),
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawReadRange {
    offset: u16,
    byte_len: u16,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawReadBeforePolicy {
    required: bool,
    ranges: Vec<RawReadRange>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawReadbackPolicy {
    required: bool,
    verification: String,
}
impl RawReadbackPolicy {
    fn into_verification(self) -> Result<ReadbackVerification, I2cMapDefinitionError> {
        match self.verification.as_str() {
            "exact-written-ranges" => Ok(ReadbackVerification::ExactWrittenRanges),
            "full-read-before-image" => Ok(ReadbackVerification::FullReadBeforeImage),
            _ => Err(I2cMapDefinitionError::Definition(format!(
                "unsupported readback.verification `{}`",
                self.verification
            ))),
        }
    }
}

fn parse_storage_encoding(value: &str) -> Result<StorageEncoding, I2cMapDefinitionError> {
    match value {
        "ascii" => Ok(StorageEncoding::Ascii),
        "ascii-nul-terminated" => Ok(StorageEncoding::AsciiNulTerminated),
        "raw" => Ok(StorageEncoding::Raw),
        "reserved" => Ok(StorageEncoding::Reserved),
        "u8" => Ok(StorageEncoding::U8),
        "u16-le" => Ok(StorageEncoding::U16Le),
        "i16-le" => Ok(StorageEncoding::I16Le),
        "u32-le" => Ok(StorageEncoding::U32Le),
        "i32-le" => Ok(StorageEncoding::I32Le),
        "f32-le" => Ok(StorageEncoding::F32Le),
        "f64-le" => Ok(StorageEncoding::F64Le),
        "serial-checksum" => Ok(StorageEncoding::SerialChecksum),
        _ => Err(I2cMapDefinitionError::Definition(format!(
            "unsupported storage encoding `{value}`"
        ))),
    }
}
fn source_error_occurrence(
    text: &str,
    key: &str,
    occurrence: usize,
    message: String,
) -> I2cMapDefinitionError {
    let needle = format!("{key}:");
    let (line, column) = text
        .lines()
        .enumerate()
        .filter_map(|(index, line)| line.find(&needle).map(|column| (index + 1, column + 1)))
        .nth(occurrence)
        .unwrap_or_else(|| source_location_of(text, key));
    I2cMapDefinitionError::Source {
        line,
        column,
        message,
    }
}

fn source_location_of(text: &str, scalar: &str) -> (usize, usize) {
    text.lines()
        .enumerate()
        .find_map(|(index, line)| line.find(scalar).map(|column| (index + 1, column + 1)))
        .unwrap_or((1, 1))
}

fn source_error(text: &str, key: &str, message: String) -> I2cMapDefinitionError {
    let needle = format!("{key}:");
    let (line, column) = text
        .lines()
        .enumerate()
        .find_map(|(index, line)| line.find(&needle).map(|column| (index + 1, column + 1)))
        .unwrap_or_else(|| source_location_of(text, key));
    I2cMapDefinitionError::Source {
        line,
        column,
        message,
    }
}

fn semantic_error_key(message: &str) -> &str {
    if message.contains("logical input") {
        "inputs"
    } else if message.contains("checksum") {
        "checksums"
    } else if message.contains("target") || message.contains("page") {
        "target"
    } else if message.contains("storage") || message.contains("field") {
        "storage"
    } else {
        "id"
    }
}

/// 返回当前所有受支持 EEPROM 的 core map 适配器。
#[must_use]
pub fn builtin_i2c_maps() -> Vec<I2cMapDefinition> {
    vec![
        yg_stereo_i2c_map(),
        storage_map_adapter(
            baton_param_rw_native_lp64_le_v1(),
            BATON_PARAM_RW_IMAGE_BYTES,
        ),
        storage_map_adapter(
            pueo_edu_df9_40_native_lp64_le_v1(),
            PUEO_EDU_DF9_40_IMAGE_BYTES,
        ),
    ]
}

/// 按稳定 map id 查找内置 I²C map。
#[must_use]
pub fn builtin_i2c_map(id: &str) -> Option<I2cMapDefinition> {
    builtin_i2c_maps().into_iter().find(|map| map.id == id)
}

/// 当前 YG P24C64G 标定 EEPROM 的 typed-input 适配器。
#[must_use]
pub fn yg_stereo_i2c_map() -> I2cMapDefinition {
    let mut map = storage_map_adapter(yg_stereo_p24c64g_v1(), YG_STEREO_P24C64G_IMAGE_BYTES);
    map.inputs = yg_logical_inputs();
    map.checksums = vec![ChecksumContract {
        target_offset: 0x0133,
        source_offset: Some(0x0125),
        source_byte_len: Some(14),
        source_fields: Vec::new(),
        algorithm: ChecksumAlgorithm::SerialSumMod255PlusOne,
    }];
    map.fixed_bytes = vec![I2cMapFixedBytes {
        offset: 0,
        bytes: YG_STEREO_P24C64G_FLAG.to_vec(),
    }];
    map
}

fn storage_map_adapter(source: &CalibrationStorageMap, image_bytes: usize) -> I2cMapDefinition {
    let fields = source
        .fields
        .iter()
        .map(storage_field_adapter)
        .collect::<Vec<_>>();
    let inputs = source
        .fields
        .iter()
        .filter(|field| {
            field.full_provision_writable
                && !matches!(
                    field.encoding,
                    StorageEncoding::Reserved | StorageEncoding::SerialChecksum
                )
        })
        .filter_map(generic_slot)
        .collect();
    let image_bytes = u16::try_from(image_bytes).expect("supported EEPROM image fits u16");
    I2cMapDefinition {
        id: source.id.to_owned(),
        display_name: source.display_name.to_owned(),
        image_bytes,
        target: I2cMapTarget {
            bus: 0,
            transport: source.transport,
        },
        fields,
        accepts: I2cMapAccepts {
            schemas: vec!["camera-toolbox.calib.solution.v1".to_owned()],
            model_ids: vec!["pinhole.rational-thin-prism.v1".to_owned()],
        },
        fixed_bytes: Vec::new(),
        inputs,
        checksums: Vec::new(),
        page_policy: PageWritePolicy::SplitAtBoundary,
        read_before: ReadBeforePolicy {
            required: true,
            ranges: vec![I2cReadRange {
                offset: 0,
                byte_len: image_bytes,
            }],
        },
        readback: ReadbackPolicy {
            required: true,
            verification: ReadbackVerification::FullReadBeforeImage,
        },
    }
}

fn storage_field_adapter(field: &StorageField) -> I2cMapStorageField {
    I2cMapStorageField {
        name: field.name.to_owned(),
        offset: field.offset,
        byte_len: field.byte_len,
        source: (field.full_provision_writable
            && !matches!(
                field.encoding,
                StorageEncoding::Reserved | StorageEncoding::SerialChecksum
            ))
        .then(|| field.name.to_owned()),
        encoding: field.encoding,
        writable: field.full_provision_writable || field.update_writable,
    }
}

fn generic_slot(field: &StorageField) -> Option<LogicalInputSlot> {
    let primitive_type = match field.encoding {
        StorageEncoding::Ascii | StorageEncoding::AsciiNulTerminated => PrimitiveType::Str,
        StorageEncoding::Raw => PrimitiveType::Bytes,
        StorageEncoding::U8 => PrimitiveType::U8,
        StorageEncoding::U16Le => PrimitiveType::U16,
        StorageEncoding::I16Le => PrimitiveType::I16,
        StorageEncoding::U32Le => PrimitiveType::U32,
        StorageEncoding::I32Le => PrimitiveType::I32,
        StorageEncoding::F32Le => PrimitiveType::F32,
        StorageEncoding::F64Le => PrimitiveType::F64,
        StorageEncoding::Reserved | StorageEncoding::SerialChecksum => return None,
    };
    let conversion = is_numeric_storage(field.encoding).then(|| NumericConversion {
        scale: 1.0,
        offset: 0.0,
        minimum: numeric_minimum(field.encoding),
        maximum: numeric_maximum(field.encoding),
        rounding: RoundingMode::Exact,
    });
    Some(LogicalInputSlot {
        name: field.name.to_owned(),
        primitive_type,
        required: true,
        semantic_type: None,
        unit: None,
        target: Some(StorageTarget {
            offset: field.offset,
            byte_len: field.byte_len,
            encoding: field.encoding,
        }),
        conversion,
        exact_string: None,
    })
}

fn yg_logical_inputs() -> Vec<LogicalInputSlot> {
    let constraint = |name: &str, expected: &str, semantic: &str| LogicalInputSlot {
        name: name.to_owned(),
        primitive_type: PrimitiveType::Str,
        required: true,
        semantic_type: Some(semantic.to_owned()),
        unit: None,
        target: None,
        conversion: None,
        exact_string: Some(expected.to_owned()),
    };
    let u32_slot = |name: &str, offset: u16| {
        numeric_slot(
            name,
            PrimitiveType::U32,
            Some("image.width"),
            Some("px"),
            offset,
            StorageEncoding::U32Le,
            4,
            0.0,
            f64::from(u32::MAX),
        )
    };
    let f32_slot = |name: &str, semantic: &str, unit: &str, offset: u16| {
        numeric_slot(
            name,
            PrimitiveType::F64,
            Some(semantic),
            Some(unit),
            offset,
            StorageEncoding::F32Le,
            4,
            -f64::from(f32::MAX),
            f64::from(f32::MAX),
        )
    };
    let mut slots = vec![
        constraint(
            "camera.model.id",
            "pinhole.rational-thin-prism.v1",
            "camera.model-id",
        ),
        u32_slot("camera.image.width", 0x0010),
        numeric_slot(
            "camera.image.height",
            PrimitiveType::U32,
            Some("image.height"),
            Some("px"),
            0x0014,
            StorageEncoding::U32Le,
            4,
            0.0,
            f64::from(u32::MAX),
        ),
        f32_slot("camera.intrinsics.fx", "camera.focal-length", "px", 0x0018),
        f32_slot("camera.intrinsics.fy", "camera.focal-length", "px", 0x001c),
        f32_slot(
            "camera.intrinsics.cx",
            "camera.principal-point",
            "px",
            0x0020,
        ),
        f32_slot(
            "camera.intrinsics.cy",
            "camera.principal-point",
            "px",
            0x0024,
        ),
    ];
    for (index, name) in [
        "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
    ]
    .iter()
    .enumerate()
    {
        slots.push(f32_slot(
            &format!("distortion.{name}"),
            "camera.distortion-coefficient",
            "dimensionless",
            0x0028 + u16::try_from(index * 4).expect("fixed index"),
        ));
    }
    slots.push(LogicalInputSlot {
        name: "serial.number".to_owned(),
        primitive_type: PrimitiveType::Str,
        required: true,
        semantic_type: Some("device.serial-number".to_owned()),
        unit: None,
        target: Some(StorageTarget {
            offset: 0x0125,
            byte_len: 14,
            encoding: StorageEncoding::Ascii,
        }),
        conversion: None,
        exact_string: None,
    });
    slots
}

fn numeric_slot(
    name: &str,
    primitive_type: PrimitiveType,
    semantic_type: Option<&str>,
    unit: Option<&str>,
    offset: u16,
    encoding: StorageEncoding,
    byte_len: u16,
    minimum: f64,
    maximum: f64,
) -> LogicalInputSlot {
    LogicalInputSlot {
        name: name.to_owned(),
        primitive_type,
        required: true,
        semantic_type: semantic_type.map(ToOwned::to_owned),
        unit: unit.map(ToOwned::to_owned),
        target: Some(StorageTarget {
            offset,
            byte_len,
            encoding,
        }),
        conversion: Some(NumericConversion {
            scale: 1.0,
            offset: 0.0,
            minimum,
            maximum,
            rounding: RoundingMode::Exact,
        }),
        exact_string: None,
    }
}

const fn fixed_storage_width(encoding: StorageEncoding) -> Option<u16> {
    match encoding {
        StorageEncoding::U8 | StorageEncoding::SerialChecksum => Some(1),
        StorageEncoding::U16Le | StorageEncoding::I16Le => Some(2),
        StorageEncoding::U32Le | StorageEncoding::I32Le | StorageEncoding::F32Le => Some(4),
        StorageEncoding::F64Le => Some(8),
        StorageEncoding::Ascii
        | StorageEncoding::AsciiNulTerminated
        | StorageEncoding::Raw
        | StorageEncoding::Reserved => None,
    }
}

fn validate_storage_range(
    offset: u16,
    byte_len: u16,
    image_bytes: u16,
    name: &str,
) -> Result<(), I2cMapDefinitionError> {
    if byte_len == 0
        || offset
            .checked_add(byte_len)
            .is_none_or(|end| end > image_bytes)
    {
        return Err(I2cMapDefinitionError::Definition(format!(
            "`{name}` storage range is empty, overflows, or lies outside image_bytes"
        )));
    }
    Ok(())
}

fn validate_encoding_width(
    encoding: StorageEncoding,
    byte_len: u16,
    name: &str,
) -> Result<(), I2cMapDefinitionError> {
    let expected = match encoding {
        StorageEncoding::U8 | StorageEncoding::SerialChecksum => Some(1),
        StorageEncoding::U16Le | StorageEncoding::I16Le => Some(2),
        StorageEncoding::U32Le | StorageEncoding::I32Le | StorageEncoding::F32Le => Some(4),
        StorageEncoding::F64Le => Some(8),
        StorageEncoding::Ascii
        | StorageEncoding::AsciiNulTerminated
        | StorageEncoding::Raw
        | StorageEncoding::Reserved => None,
    };
    if expected.is_some_and(|expected| byte_len % expected != 0) {
        return Err(I2cMapDefinitionError::Definition(format!(
            "`{name}` encoding {encoding:?} requires a byte length divisible by {}",
            expected.unwrap()
        )));
    }
    Ok(())
}

const fn is_numeric_storage(encoding: StorageEncoding) -> bool {
    matches!(
        encoding,
        StorageEncoding::U8
            | StorageEncoding::U16Le
            | StorageEncoding::I16Le
            | StorageEncoding::U32Le
            | StorageEncoding::I32Le
            | StorageEncoding::F32Le
            | StorageEncoding::F64Le
    )
}
const fn is_integer_storage(encoding: StorageEncoding) -> bool {
    matches!(
        encoding,
        StorageEncoding::U8
            | StorageEncoding::U16Le
            | StorageEncoding::I16Le
            | StorageEncoding::U32Le
            | StorageEncoding::I32Le
    )
}
fn numeric_minimum(encoding: StorageEncoding) -> f64 {
    match encoding {
        StorageEncoding::U8 => 0.0,
        StorageEncoding::U16Le => 0.0,
        StorageEncoding::I16Le => f64::from(i16::MIN),
        StorageEncoding::U32Le => 0.0,
        StorageEncoding::I32Le => f64::from(i32::MIN),
        StorageEncoding::F32Le => -f64::from(f32::MAX),
        StorageEncoding::F64Le => -f64::MAX,
        _ => unreachable!("numeric storage"),
    }
}
fn numeric_maximum(encoding: StorageEncoding) -> f64 {
    match encoding {
        StorageEncoding::U8 => f64::from(u8::MAX),
        StorageEncoding::U16Le => f64::from(u16::MAX),
        StorageEncoding::I16Le => f64::from(i16::MAX),
        StorageEncoding::U32Le => f64::from(u32::MAX),
        StorageEncoding::I32Le => f64::from(i32::MAX),
        StorageEncoding::F32Le => f64::from(f32::MAX),
        StorageEncoding::F64Le => f64::MAX,
        _ => unreachable!("numeric storage"),
    }
}

fn numeric_value(value: &TypedValue, slot: &str) -> Result<f64, I2cMapDefinitionError> {
    let value = match value {
        TypedValue::U8(value) => f64::from(*value),
        TypedValue::I8(value) => f64::from(*value),
        TypedValue::U16(value) => f64::from(*value),
        TypedValue::I16(value) => f64::from(*value),
        TypedValue::U32(value) => f64::from(*value),
        TypedValue::I32(value) => f64::from(*value),
        TypedValue::U64(value) => *value as f64,
        TypedValue::I64(value) => *value as f64,
        TypedValue::F32(value) => f64::from(*value),
        TypedValue::F64(value) => *value,
        _ => {
            return Err(I2cMapDefinitionError::Input {
                slot: slot.to_owned(),
                message: "numeric storage requires a numeric datum".to_owned(),
            });
        }
    };
    if value.is_finite() {
        Ok(value)
    } else {
        Err(I2cMapDefinitionError::Input {
            slot: slot.to_owned(),
            message: "numeric datum must be finite".to_owned(),
        })
    }
}

fn write_numeric(
    output: &mut [u8],
    encoding: StorageEncoding,
    value: f64,
    slot: &str,
) -> Result<(), I2cMapDefinitionError> {
    let bytes: Vec<u8> = match encoding {
        StorageEncoding::U8 => vec![value as u8],
        StorageEncoding::U16Le => (value as u16).to_le_bytes().to_vec(),
        StorageEncoding::I16Le => (value as i16).to_le_bytes().to_vec(),
        StorageEncoding::U32Le => (value as u32).to_le_bytes().to_vec(),
        StorageEncoding::I32Le => (value as i32).to_le_bytes().to_vec(),
        StorageEncoding::F32Le => {
            let value = value as f32;
            if !value.is_finite() {
                return Err(I2cMapDefinitionError::Input {
                    slot: slot.to_owned(),
                    message: "cannot represent value as finite f32".to_owned(),
                });
            }
            value.to_le_bytes().to_vec()
        }
        StorageEncoding::F64Le => value.to_le_bytes().to_vec(),
        _ => unreachable!("validated numeric storage"),
    };
    output.copy_from_slice(&bytes);
    Ok(())
}

fn page_segments(
    image: &[u8],
    ranges: &[(u16, u16)],
    page_size: u16,
) -> Result<Vec<I2cMapPage>, I2cMapDefinitionError> {
    let mut covered = BTreeSet::new();
    for (offset, len) in ranges {
        let end = offset
            .checked_add(*len)
            .ok_or_else(|| I2cMapDefinitionError::Definition("write range overflows".to_owned()))?;
        covered.extend(*offset..end);
    }
    let mut pages = Vec::new();
    let mut start = None;
    let mut previous = 0_u16;
    for offset in covered {
        let same_page = start.is_some_and(|start: u16| start / page_size == offset / page_size);
        if start.is_none() || offset != previous + 1 || !same_page {
            if let Some(start) = start {
                pages.push(page_from_range(image, start, previous + 1)?);
            }
            start = Some(offset);
        }
        previous = offset;
    }
    if let Some(start) = start {
        pages.push(page_from_range(image, start, previous + 1)?);
    }
    Ok(pages)
}

fn page_from_range(
    image: &[u8],
    start: u16,
    end: u16,
) -> Result<I2cMapPage, I2cMapDefinitionError> {
    let bytes = image
        .get(usize::from(start)..usize::from(end))
        .ok_or_else(|| {
            I2cMapDefinitionError::Definition("write range lies outside image".to_owned())
        })?
        .to_vec();
    Ok(I2cMapPage {
        offset: start,
        bytes,
    })
}

fn round_ties_to_even(value: f64) -> f64 {
    let floor = value.floor();
    let fraction = value - floor;
    if fraction < 0.5 {
        floor
    } else if fraction > 0.5 {
        floor + 1.0
    } else if (floor as i128) % 2 == 0 {
        floor
    } else {
        floor + 1.0
    }
}

impl fmt::Display for RoundingMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Exact => "exact",
            Self::Floor => "floor",
            Self::Ceiling => "ceiling",
            Self::TowardZero => "toward_zero",
            Self::NearestTiesToEven => "nearest_ties_to_even",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yg_inputs() -> Vec<Datum> {
        let mut inputs = vec![
            Datum::new(
                "camera.model.id",
                TypedValue::Str("pinhole.rational-thin-prism.v1".to_owned()),
            )
            .with_semantic_type("camera.model-id"),
            Datum::new("camera.image.width", TypedValue::U32(1920))
                .with_semantic_type("image.width")
                .with_unit("px"),
            Datum::new("camera.image.height", TypedValue::U32(1080))
                .with_semantic_type("image.height")
                .with_unit("px"),
        ];
        for (name, semantic, value) in [
            ("camera.intrinsics.fx", "camera.focal-length", 1234.56),
            ("camera.intrinsics.fy", "camera.focal-length", 1234.78),
            ("camera.intrinsics.cx", "camera.principal-point", 960.12),
            ("camera.intrinsics.cy", "camera.principal-point", 540.34),
        ] {
            inputs.push(
                Datum::new(name, TypedValue::F64(value))
                    .with_semantic_type(semantic)
                    .with_unit("px"),
            );
        }
        for (name, value) in [
            "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
        ]
        .into_iter()
        .zip([
            0.1, -0.05, 0.001, -0.002, 0.003, -0.004, 0.005, -0.006, 0.0, 0.0, 0.0, 0.0,
        ]) {
            inputs.push(
                Datum::new(format!("distortion.{name}"), TypedValue::F64(value))
                    .with_semantic_type("camera.distortion-coefficient")
                    .with_unit("dimensionless"),
            );
        }
        inputs.push(
            Datum::new(
                "serial.number",
                TypedValue::Str("2T02D2567K0042".to_owned()),
            )
            .with_semantic_type("device.serial-number"),
        );
        inputs
    }

    #[test]
    fn builtins_adapt_every_supported_calibration_eeprom_map() {
        let maps = builtin_i2c_maps();
        assert_eq!(maps.len(), 3);
        for id in [
            YG_STEREO_P24C64G_V1_MAP_ID,
            BATON_PARAM_RW_NATIVE_LP64_LE_V1_MAP_ID,
            PUEO_EDU_DF9_40_NATIVE_LP64_LE_V1_MAP_ID,
        ] {
            assert!(builtin_i2c_map(id).is_some(), "missing {id}");
        }
        assert!(maps.iter().all(|map| map.validate().is_ok()));
        assert!(
            maps.iter()
                .all(|map| { map.accepts.schemas == ["camera-toolbox.calib.solution.v1"] })
        );
    }

    #[test]
    fn yg_adapter_golden_bytes_checksum_and_page_boundaries() {
        let encoded = yg_stereo_i2c_map().encode(&yg_inputs()).unwrap();
        let golden = include_bytes!("fixtures/yg_stereo_p24c64g_script_default.bin");
        assert_eq!(encoded.bytes, golden);
        assert!(encoded.pages.iter().all(|page| page.offset / 32
            == (page.offset + u16::try_from(page.bytes.len()).unwrap() - 1) / 32));
    }

    #[test]
    fn named_checksum_sources_use_declared_encoded_field_spans() {
        let mut map = yg_stereo_i2c_map();
        let checksum = map.checksums.first_mut().expect("YG checksum");
        checksum.source_offset = None;
        checksum.source_byte_len = None;
        checksum.source_fields = vec!["serial.number".to_owned()];

        let encoded = map.encode(&yg_inputs()).expect("named checksum source");
        let golden = include_bytes!("fixtures/yg_stereo_p24c64g_script_default.bin");
        assert_eq!(encoded.bytes, golden);
    }

    #[test]
    fn named_checksum_source_rejects_duplicate_or_missing_fields() {
        let mut map = yg_stereo_i2c_map();
        let checksum = map.checksums.first_mut().expect("YG checksum");
        checksum.source_offset = None;
        checksum.source_byte_len = None;
        checksum.source_fields = vec!["serial.number".to_owned(), "serial.number".to_owned()];
        assert!(map.validate().is_err());

        map.checksums[0].source_fields = vec!["missing.field".to_owned()];
        assert!(map.validate().is_err());
    }

    #[test]
    fn pueo_and_baton_adapters_emit_full_zero_golden_images() {
        for (map, golden) in [
            (
                baton_param_rw_native_lp64_le_v1(),
                include_bytes!("fixtures/baton_param_rw_zero.bin").as_slice(),
            ),
            (
                pueo_edu_df9_40_native_lp64_le_v1(),
                include_bytes!("fixtures/pueo_edu_df9_40_zero.bin").as_slice(),
            ),
        ] {
            let map = builtin_i2c_map(map.id).unwrap();
            let encoded = map.encode(&zero_inputs(&map)).unwrap();
            assert_eq!(encoded.bytes, golden, "{}", map.id);
        }
    }

    fn zero_inputs(map: &I2cMapDefinition) -> Vec<Datum> {
        map.inputs
            .iter()
            .map(|slot| {
                let value = match slot.primitive_type {
                    PrimitiveType::Bool => TypedValue::Bool(false),
                    PrimitiveType::U8 => TypedValue::U8(0),
                    PrimitiveType::I8 => TypedValue::I8(0),
                    PrimitiveType::U16 => TypedValue::U16(0),
                    PrimitiveType::I16 => TypedValue::I16(0),
                    PrimitiveType::U32 => TypedValue::U32(0),
                    PrimitiveType::I32 => TypedValue::I32(0),
                    PrimitiveType::U64 => TypedValue::U64(0),
                    PrimitiveType::I64 => TypedValue::I64(0),
                    PrimitiveType::F32 => TypedValue::F32(0.0),
                    PrimitiveType::F64 => TypedValue::F64(0.0),
                    PrimitiveType::Str => TypedValue::Str(String::new()),
                    PrimitiveType::Bytes => {
                        TypedValue::Bytes(vec![
                            0;
                            slot.target
                                .as_ref()
                                .map_or(0, |target| usize::from(target.byte_len))
                        ])
                    }
                };
                let mut datum = Datum::new(&slot.name, value);
                if let Some(semantic_type) = &slot.semantic_type {
                    datum = datum.with_semantic_type(semantic_type);
                }
                if let Some(unit) = &slot.unit {
                    datum = datum.with_unit(unit);
                }
                datum
            })
            .collect()
    }
    fn approved_yaml() -> String {
        format!(
            "schema: {I2C_MAP_SCHEMA}\nid: demo\ndisplayName: Demo\naccepts:\n  schemas: [calib.solution]\n  modelIds: [pinhole.rational-thin-prism.v1]\ntarget:\n  bus: 0\n  address: 80\n  addressWidthBits: 16\n  pageSizeBytes: 4\n  writeCycleMs: 5\n  pagePolicy: {{ mode: split-at-boundary }}\n  readBefore:\n    required: true\n    ranges: [{{ offset: 0, byteLen: 4 }}]\n  readback:\n    required: true\n    verification: exact-written-ranges\nstorage:\n  - source: value\n    offset: 0\n    encoding: u32-le\ninputs:\n  - name: value\n    type: f64\n    required: true\n    conversion: {{ scale: 1.0, offset: 0.0, minimum: 0.0, maximum: 4294967295.0, rounding: exact }}\n"
        )
    }

    #[test]
    fn yaml_reports_exact_storage_encoding_location() {
        let yaml = approved_yaml().replace("encoding: u32-le", "encoding: u32-lee");
        let expected_line = yaml
            .lines()
            .position(|line| line.contains("encoding:"))
            .unwrap()
            + 1;
        let expected_column = yaml
            .lines()
            .nth(expected_line - 1)
            .unwrap()
            .find("encoding:")
            .unwrap()
            + 1;
        assert!(
            matches!(parse_i2c_map_yaml(&yaml), Err(I2cMapDefinitionError::Source { line, column, .. }) if line == expected_line && column == expected_column)
        );
    }

    #[test]
    fn approved_yaml_binds_storage_source_and_requires_conversion() {
        let map = parse_i2c_map_yaml(&approved_yaml()).unwrap();
        map.validate_source("calib.solution", "pinhole.rational-thin-prism.v1")
            .unwrap();
        let image = map
            .encode(&[Datum::new("value", TypedValue::F64(12.0))])
            .unwrap();
        assert_eq!(image.bytes[..4], 12_u32.to_le_bytes());
        assert!(matches!(parse_i2c_map_yaml(&approved_yaml().replace("    conversion: { scale: 1.0, offset: 0.0, minimum: 0.0, maximum: 4294967295.0, rounding: exact }\n", "")), Err(I2cMapDefinitionError::Source { .. })));
    }

    #[test]
    fn post_rounding_value_must_fit_storage_type() {
        let conversion = NumericConversion {
            scale: 1.0,
            offset: 0.0,
            minimum: 0.0,
            maximum: 1000.0,
            rounding: RoundingMode::Ceiling,
        };
        assert!(matches!(
            conversion.apply(255.1, StorageEncoding::U8, "value"),
            Err(I2cMapDefinitionError::Input { .. })
        ));
    }

    #[test]
    fn rounding_is_explicit_and_ties_to_even() {
        assert_eq!(round_ties_to_even(2.5), 2.0);
        assert_eq!(round_ties_to_even(3.5), 4.0);
        assert_eq!(round_ties_to_even(-1.5), -2.0);
    }
}
