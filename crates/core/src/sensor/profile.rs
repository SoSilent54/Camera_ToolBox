//! Sensor profile 描述型号、模式和安全寄存器表。

use serde::{Deserialize, Serialize};

use crate::raw::{BayerPattern, RawSpec};

use super::types::{RegisterAddress, RegisterWriteRequest, SensorError};

pub type SensorModeId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegisterAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterMapEntry {
    pub address: RegisterAddress,
    pub access: RegisterAccess,
    pub width_bits: u8,
    pub mask: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SensorMode {
    pub id: SensorModeId,
    pub raw: RawSpec,
    pub is_wdr: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SensorProfile {
    pub model: String,
    pub modes: Vec<SensorMode>,
    pub registers: Vec<RegisterMapEntry>,
}

impl SensorProfile {
    pub fn validate_write(&self, write: &RegisterWriteRequest) -> Result<(), SensorError> {
        let Some(entry) = self
            .registers
            .iter()
            .find(|entry| entry.address == write.address)
        else {
            return Err(SensorError::RegisterWriteDenied {
                address: write.address.0,
            });
        };
        if !matches!(entry.access, RegisterAccess::ReadWrite) {
            return Err(SensorError::RegisterWriteDenied {
                address: write.address.0,
            });
        }
        if !(1..=32).contains(&entry.width_bits) || write.value.width_bits != entry.width_bits {
            return Err(SensorError::RegisterWidthMismatch {
                address: write.address.0,
                expected_bits: entry.width_bits,
                actual_bits: write.value.width_bits,
            });
        }

        let width_mask = Self::value_mask(entry.width_bits);
        if write.value.value & !width_mask != 0 {
            return Err(SensorError::RegisterValueOutOfRange {
                address: write.address.0,
                value: write.value.value,
                allowed_mask: width_mask,
            });
        }

        if let Some(entry_mask) = entry.mask {
            let request_mask = write.mask.unwrap_or(width_mask);
            if request_mask & !entry_mask != 0 || write.value.value & !entry_mask != 0 {
                return Err(SensorError::RegisterMaskViolation {
                    address: write.address.0,
                    value: write.value.value,
                    request_mask,
                    allowed_mask: entry_mask,
                });
            }
        }

        Ok(())
    }

    #[must_use]
    const fn value_mask(width_bits: u8) -> u32 {
        if width_bits >= 32 {
            u32::MAX
        } else {
            (1u32 << width_bits) - 1
        }
    }

    #[must_use]
    pub fn minimal_test_profile(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            modes: vec![SensorMode {
                id: "default".to_owned(),
                raw: RawSpec {
                    width: 2,
                    height: 2,
                    bit_depth: 10,
                    bayer: BayerPattern::Rggb,
                },
                is_wdr: false,
            }],
            registers: Vec::new(),
        }
    }
}
