//! Canonical text parser/compiler for expert EEPROM map presets.
//!
//! 格式固定为首行设备/传输摘要、第二行 `Remark / Offset / Size / Type`，后续每行一个字段。

use std::{fmt, num::ParseIntError};

use serde::{Deserialize, Serialize};

use crate::calibration_eeprom::{
    CalibrationStorageMap, EepromTransportSpec, PUEO_EDU_DF9_40_IMAGE_BYTES,
    PUEO_EDU_DF9_40_NATIVE_LP64_LE_V1_MAP_ID, StorageEncoding, StorageField,
    YG_STEREO_P24C64G_IMAGE_BYTES, YG_STEREO_P24C64G_V1_MAP_ID, pueo_edu_df9_40_native_lp64_le_v1,
    yg_stereo_p24c64g_v1,
};

const DEFAULT_BUS_LABEL: &str = "I2C0";
const DEFAULT_WRITE_CYCLE_MS: u16 = 5;

pub const IMX219_EEPROM_CALIBRATION_CONFIG_NAME: &str = "imx219-eeprom-calibration";
pub const PUEO_EDU_DF9_40_PINOUT_CONFIG_NAME: &str = "pueo-edu-df9-40-pinout";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltInEepromMapConfig {
    pub name: &'static str,
    pub display_name: &'static str,
    pub source_map_id: &'static str,
}

const BUILT_IN_CONFIGS: [BuiltInEepromMapConfig; 2] = [
    BuiltInEepromMapConfig {
        name: IMX219_EEPROM_CALIBRATION_CONFIG_NAME,
        display_name: "IMX219 EEPROM calibration",
        source_map_id: YG_STEREO_P24C64G_V1_MAP_ID,
    },
    BuiltInEepromMapConfig {
        name: PUEO_EDU_DF9_40_PINOUT_CONFIG_NAME,
        display_name: "PUEO-EDU DF9-40 pinout",
        source_map_id: PUEO_EDU_DF9_40_NATIVE_LP64_LE_V1_MAP_ID,
    },
];

#[must_use]
pub const fn list_builtin_eeprom_map_configs() -> &'static [BuiltInEepromMapConfig] {
    &BUILT_IN_CONFIGS
}

#[must_use]
pub fn builtin_eeprom_map_config_text(name: &str) -> Option<String> {
    let config = find_builtin_config(name)?;
    Some(canonical_text_from_storage_map(
        config.display_name,
        builtin_source_map(config),
    ))
}

pub fn dump_builtin_eeprom_map_config(name: &str) -> Result<String, EepromMapConfigError> {
    let config = find_builtin_config(name).ok_or_else(|| {
        EepromMapConfigError::new(1, 1, format!("unknown EEPROM map config '{name}'"))
    })?;
    let text = canonical_text_from_storage_map(config.display_name, builtin_source_map(config));
    compile_eeprom_map_config_text(config.name, config.display_name, &text)?;
    Ok(text)
}

pub fn compile_builtin_eeprom_map_config(
    name: &str,
) -> Result<CompiledEepromMapConfig, EepromMapConfigError> {
    let config = find_builtin_config(name).ok_or_else(|| {
        EepromMapConfigError::new(1, 1, format!("unknown EEPROM map config '{name}'"))
    })?;
    let source_map = builtin_source_map(config);
    let text = canonical_text_from_storage_map(config.display_name, source_map);
    let mut compiled = compile_eeprom_map_config_text(config.name, config.display_name, &text)?;
    compiled.transport.write_cycle_ms = source_map.transport.write_cycle_ms;
    Ok(compiled)
}

pub fn compile_eeprom_map_config_text(
    id: &str,
    display_name: &str,
    text: &str,
) -> Result<CompiledEepromMapConfig, EepromMapConfigError> {
    let raw = parse_eeprom_map_config_text(text)?;
    compile_raw_config(id, display_name, raw)
}

#[must_use]
pub fn canonical_text_from_storage_map(display_name: &str, map: &CalibrationStorageMap) -> String {
    let total_bytes = storage_map_total_bytes(map);
    let mut text = String::new();
    text.push_str(&format!(
        "{}, {}, 0x{:02X}, Addr{}, Page{}, Size{}\n",
        display_name,
        DEFAULT_BUS_LABEL,
        map.transport.i2c_address,
        map.transport.address_width_bits,
        map.transport.page_size_bytes,
        total_bytes
    ));
    text.push_str("Remark / Offset / Size / Type\n");

    let mut fields = map.fields.iter().copied().collect::<Vec<_>>();
    fields.sort_by_key(|field| field.offset);
    let mut cursor = 0_u16;
    let mut generated_pad_index = 0_usize;
    for field in fields {
        if field.offset > cursor {
            let gap = field.offset - cursor;
            text.push_str(&format!(
                "PAD{:02} / 0x{cursor:04X} / {gap} / RESERVED({gap})\n",
                generated_pad_index
            ));
            generated_pad_index += 1;
        }
        text.push_str(&format!(
            "{} / 0x{:04X} / {} / {}\n",
            field.remark,
            field.offset,
            field.byte_len,
            type_label_for_storage_field(field)
        ));
        cursor = field.offset.saturating_add(field.byte_len);
    }
    if cursor < total_bytes {
        let gap = total_bytes - cursor;
        text.push_str(&format!(
            "PAD{:02} / 0x{cursor:04X} / {gap} / RESERVED({gap})\n",
            generated_pad_index
        ));
    }
    text
}

fn find_builtin_config(name: &str) -> Option<BuiltInEepromMapConfig> {
    BUILT_IN_CONFIGS
        .iter()
        .copied()
        .find(|config| config.name == name || config.display_name.eq_ignore_ascii_case(name))
}

fn builtin_source_map(config: BuiltInEepromMapConfig) -> &'static CalibrationStorageMap {
    match config.source_map_id {
        YG_STEREO_P24C64G_V1_MAP_ID => yg_stereo_p24c64g_v1(),
        PUEO_EDU_DF9_40_NATIVE_LP64_LE_V1_MAP_ID => pueo_edu_df9_40_native_lp64_le_v1(),
        _ => unreachable!("built-in EEPROM config must reference a known map"),
    }
}

fn storage_map_total_bytes(map: &CalibrationStorageMap) -> u16 {
    match map.id {
        YG_STEREO_P24C64G_V1_MAP_ID => {
            u16::try_from(YG_STEREO_P24C64G_IMAGE_BYTES).expect("YG image size fits u16")
        }
        PUEO_EDU_DF9_40_NATIVE_LP64_LE_V1_MAP_ID => {
            u16::try_from(PUEO_EDU_DF9_40_IMAGE_BYTES).expect("PUEO image size fits u16")
        }
        _ => map
            .fields
            .iter()
            .map(|field| field.offset.saturating_add(field.byte_len))
            .max()
            .unwrap_or(0),
    }
}

fn type_label_for_storage_field(field: StorageField) -> String {
    match field.encoding {
        StorageEncoding::Ascii => format!("ASCII({})", field.byte_len),
        StorageEncoding::AsciiNulTerminated => format!("ASCII-NUL({})", field.byte_len),
        StorageEncoding::Raw => format!("RAW({})", field.byte_len),
        StorageEncoding::Reserved => format!("RESERVED({})", field.byte_len),
        StorageEncoding::U8 => "U8".to_owned(),
        StorageEncoding::U16Le => "U16LE".to_owned(),
        StorageEncoding::I16Le => "I16LE".to_owned(),
        StorageEncoding::U32Le => "U32LE".to_owned(),
        StorageEncoding::I32Le => "I32LE".to_owned(),
        StorageEncoding::F32Le => "F32LE".to_owned(),
        StorageEncoding::F64Le => "F64LE".to_owned(),
        StorageEncoding::SerialChecksum => "SERIAL-CHECKSUM".to_owned(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledEepromMapConfig {
    pub id: String,
    pub display_name: String,
    pub header_name: String,
    pub bus_label: String,
    pub total_bytes: u16,
    pub transport: EepromTransportSpec,
    pub fields: Vec<CompiledEepromMapField>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledEepromMapField {
    pub remark: String,
    pub offset: u16,
    pub byte_len: u16,
    pub encoding: StorageEncoding,
    pub type_label: String,
    pub writable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawEepromMapConfig {
    header: RawHeader,
    fields: Vec<RawField>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawHeader {
    name: String,
    bus_label: String,
    i2c_address: u8,
    address_width_bits: u8,
    page_size_bytes: u16,
    total_bytes: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawField {
    remark: String,
    offset: u16,
    size: u16,
    field_type: ParsedFieldType,
    line: usize,
    column: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedFieldType {
    kind: FieldTypeKind,
    canonical: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldTypeKind {
    Raw,
    Ascii,
    AsciiNulTerminated,
    Reserved,
    U8,
    U16Le,
    I16Le,
    U32Le,
    I32Le,
    F32Le,
    F64Le,
    SerialChecksum,
}

impl FieldTypeKind {
    const fn fixed_byte_len(self) -> Option<u16> {
        match self {
            Self::U8 | Self::SerialChecksum => Some(1),
            Self::U16Le | Self::I16Le => Some(2),
            Self::U32Le | Self::I32Le | Self::F32Le => Some(4),
            Self::F64Le => Some(8),
            Self::Raw | Self::Ascii | Self::AsciiNulTerminated | Self::Reserved => None,
        }
    }

    const fn storage_encoding(self) -> StorageEncoding {
        match self {
            Self::Raw => StorageEncoding::Raw,
            Self::Ascii => StorageEncoding::Ascii,
            Self::AsciiNulTerminated => StorageEncoding::AsciiNulTerminated,
            Self::Reserved => StorageEncoding::Reserved,
            Self::U8 => StorageEncoding::U8,
            Self::U16Le => StorageEncoding::U16Le,
            Self::I16Le => StorageEncoding::I16Le,
            Self::U32Le => StorageEncoding::U32Le,
            Self::I32Le => StorageEncoding::I32Le,
            Self::F32Le => StorageEncoding::F32Le,
            Self::F64Le => StorageEncoding::F64Le,
            Self::SerialChecksum => StorageEncoding::SerialChecksum,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EepromMapConfigError {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl EepromMapConfigError {
    fn new(line: usize, column: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            column,
            message: message.into(),
        }
    }
}

impl fmt::Display for EepromMapConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "EEPROM map config error at line {}, column {}: {}",
            self.line, self.column, self.message
        )
    }
}

impl std::error::Error for EepromMapConfigError {}

fn parse_eeprom_map_config_text(text: &str) -> Result<RawEepromMapConfig, EepromMapConfigError> {
    let mut lines = text.lines().enumerate();
    let Some((header_index, header_line)) = lines.next() else {
        return Err(EepromMapConfigError::new(1, 1, "config is empty"));
    };
    let header = parse_header(header_index + 1, header_line)?;

    let Some((columns_index, columns_line)) = lines.next() else {
        return Err(EepromMapConfigError::new(
            2,
            1,
            "missing Remark / Offset / Size / Type header",
        ));
    };
    parse_columns_header(columns_index + 1, columns_line)?;

    let mut fields = Vec::new();
    for (line_index, line) in lines {
        let line_number = line_index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        fields.push(parse_field_line(line_number, line)?);
    }
    if fields.is_empty() {
        return Err(EepromMapConfigError::new(3, 1, "config contains no fields"));
    }

    Ok(RawEepromMapConfig { header, fields })
}

fn parse_header(line_number: usize, line: &str) -> Result<RawHeader, EepromMapConfigError> {
    let parts = line.split(',').map(str::trim).collect::<Vec<_>>();
    if parts.len() != 6 {
        return Err(EepromMapConfigError::new(
            line_number,
            1,
            "first line must be '<Name>, <Bus>, <Address>, Addr8|Addr16, Page<N>, Size<N>'",
        ));
    }
    let i2c_address = parse_u16_token(parts[2])
        .and_then(|value| {
            u8::try_from(value).map_err(|_| ParseNumberError("address must fit in u8".to_owned()))
        })
        .map_err(|error| {
            EepromMapConfigError::new(line_number, column_of(line, parts[2]), error.0)
        })?;
    let address_width_bits = parse_prefixed_number(parts[3], "Addr").map_err(|error| {
        EepromMapConfigError::new(line_number, column_of(line, parts[3]), error.0)
    })?;
    if !matches!(address_width_bits, 8 | 16) {
        return Err(EepromMapConfigError::new(
            line_number,
            column_of(line, parts[3]),
            "address width must be Addr8 or Addr16",
        ));
    }
    let page_size_bytes = parse_prefixed_number(parts[4], "Page").map_err(|error| {
        EepromMapConfigError::new(line_number, column_of(line, parts[4]), error.0)
    })?;
    let total_bytes = parse_prefixed_number(parts[5], "Size").map_err(|error| {
        EepromMapConfigError::new(line_number, column_of(line, parts[5]), error.0)
    })?;
    if page_size_bytes == 0 || total_bytes == 0 {
        return Err(EepromMapConfigError::new(
            line_number,
            column_of(line, parts[5]),
            "page size and total size must be non-zero",
        ));
    }

    Ok(RawHeader {
        name: parts[0].to_owned(),
        bus_label: parts[1].to_owned(),
        i2c_address,
        address_width_bits: u8::try_from(address_width_bits).expect("8 or 16 fits u8"),
        page_size_bytes,
        total_bytes,
    })
}

fn parse_columns_header(line_number: usize, line: &str) -> Result<(), EepromMapConfigError> {
    let parts = split_field_columns(line);
    if parts == ["Remark", "Offset", "Size", "Type"] {
        Ok(())
    } else {
        Err(EepromMapConfigError::new(
            line_number,
            1,
            "second line must be exactly 'Remark / Offset / Size / Type'",
        ))
    }
}

fn parse_field_line(line_number: usize, line: &str) -> Result<RawField, EepromMapConfigError> {
    let parts = split_field_columns(line);
    if parts.len() != 4 {
        return Err(EepromMapConfigError::new(
            line_number,
            1,
            "field line must contain Remark / Offset / Size / Type",
        ));
    }
    let offset = parse_u16_token(parts[1]).map_err(|error| {
        EepromMapConfigError::new(line_number, column_of(line, parts[1]), error.0)
    })?;
    let size = parse_u16_token(parts[2]).map_err(|error| {
        EepromMapConfigError::new(line_number, column_of(line, parts[2]), error.0)
    })?;
    let field_type = parse_field_type(parts[3], size, line_number, column_of(line, parts[3]))?;
    Ok(RawField {
        remark: parts[0].to_owned(),
        offset,
        size,
        field_type,
        line: line_number,
        column: column_of(line, parts[0]),
    })
}

fn split_field_columns(line: &str) -> Vec<&str> {
    line.split(" / ").map(str::trim).collect()
}

fn parse_field_type(
    token: &str,
    size: u16,
    line: usize,
    column: usize,
) -> Result<ParsedFieldType, EepromMapConfigError> {
    if size == 0 {
        return Err(EepromMapConfigError::new(
            line,
            column,
            "field size must be non-zero",
        ));
    }
    let upper = token.to_ascii_uppercase();
    let fixed = match upper.as_str() {
        "U8" => Some((FieldTypeKind::U8, "U8")),
        "U16LE" => Some((FieldTypeKind::U16Le, "U16LE")),
        "I16LE" => Some((FieldTypeKind::I16Le, "I16LE")),
        "U32LE" => Some((FieldTypeKind::U32Le, "U32LE")),
        "I32LE" => Some((FieldTypeKind::I32Le, "I32LE")),
        "F32LE" => Some((FieldTypeKind::F32Le, "F32LE")),
        "F64LE" => Some((FieldTypeKind::F64Le, "F64LE")),
        "SERIAL-CHECKSUM" => Some((FieldTypeKind::SerialChecksum, "SERIAL-CHECKSUM")),
        _ => None,
    };
    if let Some((kind, canonical)) = fixed {
        let width = kind.fixed_byte_len().expect("fixed type has width");
        if size % width != 0 {
            return Err(EepromMapConfigError::new(
                line,
                column,
                format!(
                    "type {canonical} requires a size divisible by {width}, but Size column is {size}"
                ),
            ));
        }
        return Ok(ParsedFieldType {
            kind,
            canonical: canonical.to_owned(),
        });
    }
    for (prefix, kind, label) in [
        ("RAW", FieldTypeKind::Raw, "RAW"),
        ("ASCII", FieldTypeKind::Ascii, "ASCII"),
        ("ASCII-NUL", FieldTypeKind::AsciiNulTerminated, "ASCII-NUL"),
        ("RESERVED", FieldTypeKind::Reserved, "RESERVED"),
    ] {
        if let Some(count) = parse_count_type(token, prefix) {
            let count = count.map_err(|error| EepromMapConfigError::new(line, column, error.0))?;
            if count != size {
                return Err(EepromMapConfigError::new(
                    line,
                    column,
                    format!("type {label}({count}) does not match Size column {size}"),
                ));
            }
            return Ok(ParsedFieldType {
                kind,
                canonical: format!("{label}({count})"),
            });
        }
    }
    Err(EepromMapConfigError::new(
        line,
        column,
        format!("unsupported field type '{token}'"),
    ))
}

fn parse_count_type(token: &str, prefix: &str) -> Option<Result<u16, ParseNumberError>> {
    let upper = token.to_ascii_uppercase();
    let rest = upper.strip_prefix(prefix)?;
    let count = rest.strip_prefix('(')?.strip_suffix(')')?;
    Some(parse_u16_token(count))
}

fn compile_raw_config(
    id: &str,
    display_name: &str,
    raw: RawEepromMapConfig,
) -> Result<CompiledEepromMapConfig, EepromMapConfigError> {
    let mut fields = raw.fields;
    fields.sort_by_key(|field| field.offset);
    let mut cursor = 0_u16;
    let total = raw.header.total_bytes;
    let mut compiled_fields = Vec::with_capacity(fields.len());

    for field in fields {
        if field.offset != cursor {
            return Err(EepromMapConfigError::new(
                field.line,
                field.column,
                format!(
                    "field {} starts at 0x{:04x}, expected 0x{cursor:04x}; cover gaps with RESERVED(n)",
                    field.remark, field.offset
                ),
            ));
        }
        let end = field.offset.checked_add(field.size).ok_or_else(|| {
            EepromMapConfigError::new(field.line, field.column, "field end offset overflows u16")
        })?;
        if end > total {
            return Err(EepromMapConfigError::new(
                field.line,
                field.column,
                format!(
                    "field {} ends at 0x{end:04x}, beyond Size{total}",
                    field.remark
                ),
            ));
        }
        cursor = end;
        let writable = field.field_type.kind != FieldTypeKind::Reserved;
        compiled_fields.push(CompiledEepromMapField {
            remark: field.remark,
            offset: field.offset,
            byte_len: field.size,
            encoding: field.field_type.kind.storage_encoding(),
            type_label: field.field_type.canonical,
            writable,
        });
    }

    if cursor != total {
        return Err(EepromMapConfigError::new(
            1,
            1,
            format!("fields cover {cursor} byte(s), but header declares Size{total}"),
        ));
    }

    Ok(CompiledEepromMapConfig {
        id: id.to_owned(),
        display_name: display_name.to_owned(),
        header_name: raw.header.name,
        bus_label: raw.header.bus_label,
        total_bytes: total,
        transport: EepromTransportSpec {
            i2c_address: raw.header.i2c_address,
            address_width_bits: raw.header.address_width_bits,
            page_size_bytes: raw.header.page_size_bytes,
            write_cycle_ms: DEFAULT_WRITE_CYCLE_MS,
        },
        fields: compiled_fields,
    })
}

#[derive(Debug)]
struct ParseNumberError(String);

fn parse_prefixed_number(token: &str, prefix: &str) -> Result<u16, ParseNumberError> {
    let value = token
        .strip_prefix(prefix)
        .ok_or_else(|| ParseNumberError(format!("expected {prefix}<number>, got '{token}'")))?;
    parse_u16_token(value)
}

fn parse_u16_token(token: &str) -> Result<u16, ParseNumberError> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err(ParseNumberError("number is empty".to_owned()));
    }
    let parsed = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u16::from_str_radix(hex, 16)
    } else {
        trimmed.parse::<u16>()
    };
    parsed.map_err(|error: ParseIntError| {
        ParseNumberError(format!("invalid number '{token}': {error}"))
    })
}

fn column_of(line: &str, token: &str) -> usize {
    line.find(token).map_or(1, |index| index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_generated_from_authoritative_static_maps() {
        assert_eq!(list_builtin_eeprom_map_configs().len(), 2);
        assert_builtin_matches_source_map(
            IMX219_EEPROM_CALIBRATION_CONFIG_NAME,
            yg_stereo_p24c64g_v1(),
        );
        assert_builtin_matches_source_map(
            PUEO_EDU_DF9_40_PINOUT_CONFIG_NAME,
            pueo_edu_df9_40_native_lp64_le_v1(),
        );
    }

    #[test]
    fn pueo_builtin_uses_current_layout_fields() {
        let text = dump_builtin_eeprom_map_config(PUEO_EDU_DF9_40_PINOUT_CONFIG_NAME).unwrap();

        assert!(text.contains("PUEO-EDU DF9-40 pinout, I2C0, 0x50, Addr16, Page32, Size904"));
        assert!(text.contains("IMU.AB0 / 0x0260 / 8 / F64LE"));
        assert!(text.contains("IMU.GB2 / 0x0288 / 8 / F64LE"));
        assert!(text.contains("RGB.FPS / 0x0360 / 8 / F64LE"));
        assert!(text.contains("RGB.EXP / 0x0368 / 8 / F64LE"));
        assert!(text.contains("RGB.GAIN / 0x0370 / 8 / F64LE"));
        assert!(text.contains("RGB.AE / 0x0378 / 1 / U8"));
        assert!(!text.contains("imu_instrinsic.gyr_n"));
    }

    #[test]
    fn generated_text_does_not_fabricate_calibration_placeholders() {
        let imx219 = dump_builtin_eeprom_map_config(IMX219_EEPROM_CALIBRATION_CONFIG_NAME).unwrap();
        assert!(!imx219.contains(&format!("{}{}", "MA", "GC")));
        assert!(imx219.contains("FLAG / 0x0000 / 8 / ASCII(8)"));
        assert!(imx219.contains("fx/fy/cx/cy / 0x0018 / 16 / F32LE"));
    }

    #[test]
    fn parser_reports_line_and_column_for_type_size_mismatch() {
        let text = "Test Map, I2C0, 0x50, Addr16, Page16, Size256\n\
Remark / Offset / Size / Type\n\
ROWA / 0x0000 / 3 / U16LE\n";

        let error = compile_eeprom_map_config_text("bad", "bad", text).unwrap_err();

        assert_eq!(error.line, 3);
        assert!(error.column > 1);
        assert!(error.message.contains("divisible by 2"));
    }

    #[test]
    fn compiler_rejects_gaps_without_reserved_rows() {
        let text = "Test Map, I2C0, 0x50, Addr16, Page16, Size256\n\
Remark / Offset / Size / Type\n\
ROWA / 0x0000 / 4 / ASCII(4)\n\
ROWC / 0x0008 / 4 / ASCII(4)\n";

        let error = compile_eeprom_map_config_text("bad", "bad", text).unwrap_err();

        assert_eq!(error.line, 4);
        assert!(error.message.contains("cover gaps"));
    }

    fn assert_builtin_matches_source_map(name: &str, map: &CalibrationStorageMap) {
        let compiled = compile_builtin_eeprom_map_config(name).unwrap();
        assert_eq!(compiled.transport.i2c_address, map.transport.i2c_address);
        assert_eq!(
            compiled.transport.address_width_bits,
            map.transport.address_width_bits
        );
        assert_eq!(
            compiled.transport.page_size_bytes,
            map.transport.page_size_bytes
        );
        assert_eq!(compiled.total_bytes, storage_map_total_bytes(map));

        for field in map.fields {
            let compiled_field = compiled
                .fields
                .iter()
                .find(|candidate| {
                    candidate.remark == field.remark
                        && candidate.offset == field.offset
                        && candidate.byte_len == field.byte_len
                })
                .unwrap_or_else(|| panic!("missing parsed builtin field {}", field.name));
            assert_eq!(compiled_field.encoding, field.encoding, "{}", field.name);
            assert_eq!(
                compiled_field.writable,
                field.full_provision_writable || field.update_writable,
                "{}",
                field.name
            );
        }
    }
}
