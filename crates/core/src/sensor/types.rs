//! Sensor 基础类型。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RegisterAddress(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterValue {
    pub value: u32,
    pub width_bits: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterWriteRequest {
    pub address: RegisterAddress,
    pub value: RegisterValue,
    pub mask: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureRequest {
    pub mode_id: String,
    pub frame_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureArtifact {
    pub raw_path: PathBuf,
    pub manifest_path: Option<PathBuf>,
    pub frame_id: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SensorError {
    #[error("unsupported sensor model: {0}")]
    UnsupportedModel(String),
    #[error("register write is not allowed: 0x{address:04x}")]
    RegisterWriteDenied { address: u16 },
    #[error(
        "register width mismatch at 0x{address:04x}: expected {expected_bits} bits, got {actual_bits} bits"
    )]
    RegisterWidthMismatch {
        address: u16,
        expected_bits: u8,
        actual_bits: u8,
    },
    #[error(
        "register value out of range at 0x{address:04x}: value=0x{value:x}, allowed_mask=0x{allowed_mask:x}"
    )]
    RegisterValueOutOfRange {
        address: u16,
        value: u32,
        allowed_mask: u32,
    },
    #[error(
        "register mask violation at 0x{address:04x}: value=0x{value:x}, request_mask=0x{request_mask:x}, allowed_mask=0x{allowed_mask:x}"
    )]
    RegisterMaskViolation {
        address: u16,
        value: u32,
        request_mask: u32,
        allowed_mask: u32,
    },
    #[error("invalid exposure setpoint: {0}")]
    InvalidExposure(String),
    #[error("capture failed: {0}")]
    CaptureFailed(String),
    #[error("adapter error: {0}")]
    Adapter(String),
}
