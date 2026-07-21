//! P24C64G 标定 EEPROM 的纯数据布局与编码。
//!
//! 本模块只把标定结果编码成固定字段和写段；I²C、SSH、权限与写入提交顺序由上层
//! provisioning helper 负责。这样离线 BIN 导出和直接远程烧录始终使用同一字段载荷。

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::calibration::CalibrationSolution;

/// 当前 Yg Stereo 标定 EEPROM 映射的稳定标识。
pub const YG_STEREO_P24C64G_V1_MAP_ID: &str = "yg-stereo-p24c64g-v1";
/// 兼容既有解析器的完整 EEPROM 镜像长度。
pub const YG_STEREO_P24C64G_IMAGE_BYTES: usize = 0x134;
/// 相机内参与畸变参数的连续写入区长度。
pub const YG_STEREO_P24C64G_INTRINSICS_BYTES: usize = 72;
/// 标定有效 FLAG；helper 必须在所有载荷回读通过后最后提交它。
pub const YG_STEREO_P24C64G_FLAG: [u8; 8] = *b"hessian\0";

const FLAG_OFFSET: usize = 0x0000;
const WIDTH_OFFSET: usize = 0x0010;
const HEIGHT_OFFSET: usize = 0x0014;
const FX_OFFSET: usize = 0x0018;
const FY_OFFSET: usize = 0x001c;
const CX_OFFSET: usize = 0x0020;
const CY_OFFSET: usize = 0x0024;
const DISTORTION_OFFSET: usize = 0x0028;
const SERIAL_OFFSET: usize = 0x0125;
const SERIAL_BYTES: usize = 14;
const SERIAL_CHECKSUM_OFFSET: usize = 0x0133;

/// EEPROM 字段的编码方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageEncoding {
    Ascii,
    U32Le,
    F32Le,
    SerialChecksum,
}

/// 一个可审计的 EEPROM 字段范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageField {
    pub name: &'static str,
    pub offset: u16,
    pub byte_len: u16,
    pub encoding: StorageEncoding,
    pub full_provision_writable: bool,
    pub update_writable: bool,
}

/// EEPROM 页写协议参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EepromTransportSpec {
    pub i2c_address: u8,
    pub address_width_bits: u8,
    pub page_size_bytes: u16,
    pub write_cycle_ms: u16,
}

/// 一个受支持标定存储设备的不可变布局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CalibrationStorageMap {
    pub id: &'static str,
    pub display_name: &'static str,
    pub transport: EepromTransportSpec,
    pub fields: &'static [StorageField],
}

const YG_STEREO_FIELDS: [StorageField; 6] = [
    StorageField {
        name: "flag",
        offset: FLAG_OFFSET as u16,
        byte_len: YG_STEREO_P24C64G_FLAG.len() as u16,
        encoding: StorageEncoding::Ascii,
        full_provision_writable: true,
        update_writable: false,
    },
    StorageField {
        name: "image_size",
        offset: WIDTH_OFFSET as u16,
        byte_len: 8,
        encoding: StorageEncoding::U32Le,
        full_provision_writable: true,
        update_writable: true,
    },
    StorageField {
        name: "camera_matrix",
        offset: FX_OFFSET as u16,
        byte_len: 16,
        encoding: StorageEncoding::F32Le,
        full_provision_writable: true,
        update_writable: true,
    },
    StorageField {
        name: "distortion",
        offset: DISTORTION_OFFSET as u16,
        byte_len: 48,
        encoding: StorageEncoding::F32Le,
        full_provision_writable: true,
        update_writable: true,
    },
    StorageField {
        name: "serial_number",
        offset: SERIAL_OFFSET as u16,
        byte_len: SERIAL_BYTES as u16,
        encoding: StorageEncoding::Ascii,
        full_provision_writable: true,
        update_writable: false,
    },
    StorageField {
        name: "serial_checksum",
        offset: SERIAL_CHECKSUM_OFFSET as u16,
        byte_len: 1,
        encoding: StorageEncoding::SerialChecksum,
        full_provision_writable: true,
        update_writable: false,
    },
];

const YG_STEREO_MAP: CalibrationStorageMap = CalibrationStorageMap {
    id: YG_STEREO_P24C64G_V1_MAP_ID,
    display_name: "Yg Stereo P24C64G v1",
    transport: EepromTransportSpec {
        i2c_address: 0x50,
        address_width_bits: 16,
        page_size_bytes: 32,
        write_cycle_ms: 10,
    },
    fields: &YG_STEREO_FIELDS,
};

/// 返回当前已知 P24C64G 模组的标定 EEPROM 映射。
#[must_use]
pub const fn yg_stereo_p24c64g_v1() -> &'static CalibrationStorageMap {
    &YG_STEREO_MAP
}

/// 直接烧录或仅更新内参时 helper 的行为模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EepromProvisioningMode {
    FullProvision,
    UpdateCalibration,
}

/// helper 可执行的一段 EEPROM 写入载荷。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EepromWriteSegment {
    pub offset: u16,
    pub bytes: Vec<u8>,
}

/// 发送给远程 helper 的受控写入请求。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EepromProvisionRequest {
    pub map_id: String,
    pub mode: EepromProvisioningMode,
    /// FullProvision 时为写入的 SN；UpdateCalibration 时为必须匹配的既有 SN。
    pub serial_number: String,
    /// 只有预读到的非空 SN 与输入不同且用户明确确认后才可为 true。
    pub overwrite_existing_serial: bool,
    pub segments: Vec<EepromWriteSegment>,
}

impl EepromProvisionRequest {
    /// 校验 helper 请求只覆盖当前映射允许的精确字段集合。
    ///
    /// # Errors
    ///
    /// map、SN、模式、段顺序/范围或 checksum 不符合固定协议时返回错误。
    pub fn validate(&self) -> Result<(), EepromProvisionRequestError> {
        if self.map_id != YG_STEREO_P24C64G_V1_MAP_ID {
            return Err(EepromProvisionRequestError::UnsupportedMap(
                self.map_id.clone(),
            ));
        }
        let serial = validate_serial_number(&self.serial_number)
            .map_err(EepromProvisionRequestError::Serial)?;
        match self.mode {
            EepromProvisioningMode::FullProvision => {
                validate_segments(
                    &self.segments,
                    &[
                        (FLAG_OFFSET, YG_STEREO_P24C64G_FLAG.len()),
                        (WIDTH_OFFSET, YG_STEREO_P24C64G_INTRINSICS_BYTES),
                        (SERIAL_OFFSET, SERIAL_BYTES + 1),
                    ],
                )?;
                if self.segments[0].bytes != YG_STEREO_P24C64G_FLAG {
                    return Err(EepromProvisionRequestError::InvalidFlag);
                }
                if self.segments[2].bytes[..SERIAL_BYTES] != serial {
                    return Err(EepromProvisionRequestError::SerialPayloadMismatch);
                }
                if self.segments[2].bytes[SERIAL_BYTES] != serial_checksum(&serial) {
                    return Err(EepromProvisionRequestError::InvalidSerialChecksum);
                }
            }
            EepromProvisioningMode::UpdateCalibration => {
                if self.overwrite_existing_serial {
                    return Err(EepromProvisionRequestError::OverwriteNotAllowedForUpdate);
                }
                validate_segments(
                    &self.segments,
                    &[(WIDTH_OFFSET, YG_STEREO_P24C64G_INTRINSICS_BYTES)],
                )?;
            }
        }
        Ok(())
    }
}

fn validate_segments(
    segments: &[EepromWriteSegment],
    expected: &[(usize, usize)],
) -> Result<(), EepromProvisionRequestError> {
    if segments.len() != expected.len() {
        return Err(EepromProvisionRequestError::UnexpectedSegmentCount {
            expected: expected.len(),
            actual: segments.len(),
        });
    }
    for (index, (segment, (offset, byte_len))) in segments.iter().zip(expected).enumerate() {
        if usize::from(segment.offset) != *offset || segment.bytes.len() != *byte_len {
            return Err(EepromProvisionRequestError::UnexpectedSegment {
                index,
                expected_offset: *offset as u16,
                expected_len: *byte_len,
                actual_offset: segment.offset,
                actual_len: segment.bytes.len(),
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum EepromProvisionRequestError {
    #[error("unsupported calibration EEPROM map: {0}")]
    UnsupportedMap(String),
    #[error(transparent)]
    Serial(#[from] EepromImageError),
    #[error("expected {expected} EEPROM write segments, got {actual}")]
    UnexpectedSegmentCount { expected: usize, actual: usize },
    #[error(
        "EEPROM segment {index} must be offset 0x{expected_offset:04x} length {expected_len}, got offset 0x{actual_offset:04x} length {actual_len}"
    )]
    UnexpectedSegment {
        index: usize,
        expected_offset: u16,
        expected_len: usize,
        actual_offset: u16,
        actual_len: usize,
    },
    #[error("FullProvision FLAG payload must be hessian\\0")]
    InvalidFlag,
    #[error("FullProvision SN payload differs from the request SN")]
    SerialPayloadMismatch,
    #[error("FullProvision SN checksum does not match the request SN")]
    InvalidSerialChecksum,
    #[error("UpdateCalibration cannot overwrite an existing SN")]
    OverwriteNotAllowedForUpdate,
}

/// 与既有 `make_eeprom_bin.py` 相同布局的完整 EEPROM 镜像。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FullEepromImage {
    bytes: [u8; YG_STEREO_P24C64G_IMAGE_BYTES],
}

impl FullEepromImage {
    /// 从标定解与 14-byte ASCII SN 构造完整 EEPROM 镜像。
    ///
    /// 所有写入字段均使用小端编码；未定义的保留区保持零值。
    pub fn from_solution(
        solution: &CalibrationSolution,
        serial_number: &str,
    ) -> Result<Self, EepromImageError> {
        let serial = validate_serial_number(serial_number)?;
        let distortion = validated_distortion(&solution.distortion_coefficients)?;
        let mut bytes = [0_u8; YG_STEREO_P24C64G_IMAGE_BYTES];

        bytes[FLAG_OFFSET..FLAG_OFFSET + YG_STEREO_P24C64G_FLAG.len()]
            .copy_from_slice(&YG_STEREO_P24C64G_FLAG);
        write_u32(&mut bytes, WIDTH_OFFSET, solution.image_size.width);
        write_u32(&mut bytes, HEIGHT_OFFSET, solution.image_size.height);
        write_f32(&mut bytes, FX_OFFSET, solution.camera_matrix[0], "fx")?;
        write_f32(&mut bytes, FY_OFFSET, solution.camera_matrix[4], "fy")?;
        write_f32(&mut bytes, CX_OFFSET, solution.camera_matrix[2], "cx")?;
        write_f32(&mut bytes, CY_OFFSET, solution.camera_matrix[5], "cy")?;
        for (index, value) in distortion.iter().copied().enumerate() {
            write_f32(
                &mut bytes,
                DISTORTION_OFFSET + index * std::mem::size_of::<f32>(),
                value,
                DISTORTION_FIELD_NAMES[index],
            )?;
        }
        bytes[SERIAL_OFFSET..SERIAL_OFFSET + SERIAL_BYTES].copy_from_slice(&serial);
        bytes[SERIAL_CHECKSUM_OFFSET] = serial_checksum(&serial);

        Ok(Self { bytes })
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; YG_STEREO_P24C64G_IMAGE_BYTES] {
        &self.bytes
    }

    #[must_use]
    pub fn into_bytes(self) -> [u8; YG_STEREO_P24C64G_IMAGE_BYTES] {
        self.bytes
    }

    /// 返回镜像中已校验并编码的 SN；所有烧录请求必须以此作为唯一身份来源。
    #[must_use]
    pub fn serial_number(&self) -> &str {
        std::str::from_utf8(&self.bytes[SERIAL_OFFSET..SERIAL_OFFSET + SERIAL_BYTES])
            .expect("FullEepromImage serial bytes are validated ASCII")
    }

    /// 构造完整烧录请求。FLAG 段仅表示目标值，实际 helper 必须在最后提交。
    #[must_use]
    pub fn full_provision_request(
        &self,
        overwrite_existing_serial: bool,
    ) -> EepromProvisionRequest {
        EepromProvisionRequest {
            map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
            mode: EepromProvisioningMode::FullProvision,
            serial_number: self.serial_number().to_owned(),
            overwrite_existing_serial,
            segments: vec![
                segment(&self.bytes, FLAG_OFFSET, YG_STEREO_P24C64G_FLAG.len()),
                segment(
                    &self.bytes,
                    WIDTH_OFFSET,
                    YG_STEREO_P24C64G_INTRINSICS_BYTES,
                ),
                segment(&self.bytes, SERIAL_OFFSET, SERIAL_BYTES + 1),
            ],
        }
    }

    /// 构造只更新内参的请求。SN 只作设备身份校验，永远不在此模式写入。
    #[must_use]
    pub fn update_calibration_request(&self) -> EepromProvisionRequest {
        EepromProvisionRequest {
            map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
            mode: EepromProvisioningMode::UpdateCalibration,
            serial_number: self.serial_number().to_owned(),
            overwrite_existing_serial: false,
            segments: vec![segment(
                &self.bytes,
                WIDTH_OFFSET,
                YG_STEREO_P24C64G_INTRINSICS_BYTES,
            )],
        }
    }
}

const DISTORTION_FIELD_NAMES: [&str; 12] = [
    "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
];

fn segment(bytes: &[u8], offset: usize, byte_len: usize) -> EepromWriteSegment {
    EepromWriteSegment {
        offset: offset as u16,
        bytes: bytes[offset..offset + byte_len].to_vec(),
    }
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + std::mem::size_of::<u32>()].copy_from_slice(&value.to_le_bytes());
}

fn write_f32(
    bytes: &mut [u8],
    offset: usize,
    value: f64,
    field: &'static str,
) -> Result<(), EepromImageError> {
    if !value.is_finite() {
        return Err(EepromImageError::NonFiniteField { field });
    }
    if value.abs() > f64::from(f32::MAX) {
        return Err(EepromImageError::FloatOutOfRange { field, value });
    }
    bytes[offset..offset + std::mem::size_of::<f32>()]
        .copy_from_slice(&(value as f32).to_le_bytes());
    Ok(())
}

fn validated_distortion(values: &[f64]) -> Result<[f64; 12], EepromImageError> {
    if !matches!(values.len(), 8 | 12) {
        return Err(EepromImageError::UnexpectedDistortionCount {
            expected: DISTORTION_FIELD_NAMES.len(),
            actual: values.len(),
        });
    }
    // 与 make_eeprom_bin.py --yml 保持一致：只写 OpenCV 前 8 项，s1..s4 默认清零。
    let mut distortion = [0.0_f64; 12];
    distortion[..8].copy_from_slice(&values[..8]);
    Ok(distortion)
}

fn validate_serial_number(serial_number: &str) -> Result<[u8; SERIAL_BYTES], EepromImageError> {
    if !serial_number.is_ascii() {
        return Err(EepromImageError::NonAsciiSerialNumber);
    }
    let bytes = serial_number.as_bytes();
    if bytes.len() != SERIAL_BYTES {
        return Err(EepromImageError::InvalidSerialNumberLength {
            actual: bytes.len(),
        });
    }
    let mut serial = [0_u8; SERIAL_BYTES];
    serial.copy_from_slice(bytes);
    Ok(serial)
}

/// 与既有 EEPROM 解析器一致的 SN 校验和。
#[must_use]
pub fn serial_checksum(serial: &[u8; SERIAL_BYTES]) -> u8 {
    let sum = serial
        .iter()
        .fold(0_u16, |sum, byte| sum + u16::from(*byte));
    ((sum % 0xff) + 1) as u8
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum EepromImageError {
    #[error("EEPROM serial number must contain exactly 14 ASCII bytes, got {actual}")]
    InvalidSerialNumberLength { actual: usize },
    #[error("EEPROM serial number must contain ASCII bytes only")]
    NonAsciiSerialNumber,
    #[error("expected {expected} distortion coefficients, got {actual}")]
    UnexpectedDistortionCount { expected: usize, actual: usize },
    #[error("EEPROM field {field} is NaN or infinity")]
    NonFiniteField { field: &'static str },
    #[error("EEPROM field {field}={value} cannot be represented as f32")]
    FloatOutOfRange { field: &'static str, value: f64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::{
        CalibrationImageSize, CalibrationSolution, PANGBOT_CALIBRATION_FLAGS,
    };

    fn solution() -> CalibrationSolution {
        CalibrationSolution {
            image_size: CalibrationImageSize::new(1920, 1080).unwrap(),
            camera_matrix: [1234.56, 0.0, 960.12, 0.0, 1234.78, 540.34, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![
                0.1, -0.05, 0.001, -0.002, 0.003, -0.004, 0.005, -0.006, 0.007, -0.008, 0.009,
                -0.01,
            ],
            rms_error: 0.1,
            calibration_flags: PANGBOT_CALIBRATION_FLAGS,
            views: Vec::new(),
        }
    }

    #[test]
    fn full_image_matches_make_eeprom_bin_default_byte_for_byte() {
        let image = FullEepromImage::from_solution(&solution(), "2T02D2567K0042").unwrap();
        let golden: &[u8; YG_STEREO_P24C64G_IMAGE_BYTES] =
            include_bytes!("fixtures/yg_stereo_p24c64g_script_default.bin");

        assert_eq!(image.as_bytes(), golden);
        let thin_prism_offset = DISTORTION_OFFSET + 8 * std::mem::size_of::<f32>();
        assert!(
            image.as_bytes()[thin_prism_offset..thin_prism_offset + 4 * 4]
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    #[test]
    fn full_and_update_requests_write_the_intended_segments() {
        let image = FullEepromImage::from_solution(&solution(), "2T02D2567K0042").unwrap();
        assert_eq!(image.serial_number(), "2T02D2567K0042");

        let full = image.full_provision_request(false);
        assert_eq!(full.mode, EepromProvisioningMode::FullProvision);
        assert_eq!(full.serial_number, image.serial_number());
        assert_eq!(
            full.segments
                .iter()
                .map(|segment| segment.offset)
                .collect::<Vec<_>>(),
            [0, 0x10, 0x125]
        );
        assert_eq!(
            full.segments
                .iter()
                .map(|segment| segment.bytes.len())
                .collect::<Vec<_>>(),
            [8, 72, 15]
        );

        let update = image.update_calibration_request();
        assert_eq!(update.mode, EepromProvisioningMode::UpdateCalibration);
        assert_eq!(update.serial_number, image.serial_number());
        assert_eq!(update.segments.len(), 1);
        assert_eq!(update.segments[0].offset, WIDTH_OFFSET as u16);
        assert_eq!(
            update.segments[0].bytes.len(),
            YG_STEREO_P24C64G_INTRINSICS_BYTES
        );
    }

    #[test]
    fn rejects_nonportable_serial_and_incomplete_distortion() {
        assert!(matches!(
            FullEepromImage::from_solution(&solution(), "not-ascii-序号"),
            Err(EepromImageError::NonAsciiSerialNumber)
        ));

        let mut invalid = solution();
        invalid.distortion_coefficients.pop();
        assert!(matches!(
            FullEepromImage::from_solution(&invalid, "2T02D2567K0042"),
            Err(EepromImageError::UnexpectedDistortionCount { .. })
        ));
    }
}
