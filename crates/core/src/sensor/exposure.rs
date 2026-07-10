//! 曝光语义参数和寄存器计划。

use serde::{Deserialize, Serialize};

use super::types::RegisterWriteRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposureSetpoint {
    pub exposure_lines: u32,
    pub analog_gain_milli: u32,
    pub digital_gain_milli: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposurePlan {
    pub writes: Vec<RegisterWriteRequest>,
}
