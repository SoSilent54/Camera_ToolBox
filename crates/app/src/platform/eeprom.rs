//! 标定 EEPROM helper 的 typed 协议与平台服务端口。

use camera_toolbox_core::EepromProvisionRequest;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::RemoteOperationControl;

/// GUI、SSH adapter 与目标 helper 共同支持的协议版本。
pub const EEPROM_HELPER_SCHEMA_VERSION: u32 = 1;
/// 实验性远程写入不会把 SSH 断线伪装成可恢复事务；调用端必须显式确认该风险。
pub const EEPROM_EXPERIMENTAL_PROVISION_WARNING: &str = "Experimental write: if SSH is interrupted after writing begins, device state is unknown. Do not retry; inspect the EEPROM and use the saved backup.";

/// helper 进程实际访问的目标；bus 只能来自已持久化平台配置。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EepromHelperTarget {
    pub map_id: String,
    pub i2c_bus: u32,
}

/// helper 支持的三阶段操作；Provision 必须携带同一设备状态上的 dry-run token。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum EepromHelperAction {
    Inspect,
    DryRun {
        request: EepromProvisionRequest,
    },
    Provision {
        request: EepromProvisionRequest,
        expected_before_sha256: String,
        dry_run_token: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EepromHelperRequest {
    pub schema_version: u32,
    pub target: EepromHelperTarget,
    pub action: EepromHelperAction,
}

/// 设备 SN 区的解析状态；非空损坏值不会被伪装成“空”。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum EepromSerialState {
    Empty,
    Valid { value: String },
    Invalid { raw_hex: String, checksum: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EepromDeviceState {
    pub image_sha256: String,
    pub flag_valid: bool,
    pub serial: EepromSerialState,
}

/// 实际页写计划；`stage` 明确区分失效 FLAG、数据写入和最终提交。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EepromPageWritePlan {
    pub stage: String,
    pub offset: u16,
    pub byte_len: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EepromRollbackState {
    NotRequired,
    Restored,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EepromInspectResult {
    pub state: EepromDeviceState,
    /// 完整 0x134-byte 备份；调用端在允许写入前必须先持久化它。
    pub backup: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EepromDryRunResult {
    pub state: EepromDeviceState,
    pub backup: Vec<u8>,
    pub page_plan: Vec<EepromPageWritePlan>,
    pub dry_run_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EepromWriteResult {
    pub before: EepromDeviceState,
    pub after: EepromDeviceState,
    pub backup: Vec<u8>,
    pub page_plan: Vec<EepromPageWritePlan>,
    pub bytewise_verified: bool,
    pub rollback: EepromRollbackState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EepromHelperResult {
    Inspect(EepromInspectResult),
    DryRun(EepromDryRunResult),
    Provision(EepromWriteResult),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("{code}: {message}")]
#[serde(deny_unknown_fields)]
pub struct EepromHelperFailure {
    pub code: String,
    pub message: String,
    pub before: Option<EepromDeviceState>,
    pub backup: Vec<u8>,
    pub rollback: EepromRollbackState,
    pub rollback_error: Option<String>,
}

/// helper 即使失败也必须输出结构化结果，禁止让 GUI 从 stderr 猜测状态。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum EepromHelperOutput {
    Success { result: EepromHelperResult },
    Failure { failure: EepromHelperFailure },
}

/// GUI 提交给固定 EEPROM SSH target 的请求；helper 路径、map 和 bus 不来自该请求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EepromProvisionOperation {
    pub action: EepromHelperAction,
    /// 仅 Provision 使用；adapter 必须拒绝未显式确认断线风险的写入。
    pub experimental_disconnect_risk_acknowledged: bool,
}

pub trait EepromProvisionService: Send + Sync {
    fn service_id(&self) -> &str;

    /// # Errors
    ///
    /// target 未绑定、SSH 失败、helper 协议损坏或 helper 拒绝写入时返回错误。
    fn execute(
        &self,
        request: EepromProvisionOperation,
        control: RemoteOperationControl,
    ) -> Result<EepromHelperResult, EepromProvisionServiceError>;
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum EepromProvisionServiceError {
    #[error("EEPROM request map {request_map} does not match configured map {configured_map}")]
    MapMismatch {
        request_map: String,
        configured_map: String,
    },
    #[error("EEPROM request is invalid: {0}")]
    InvalidRequest(String),
    #[error("EEPROM transport failed: {0}")]
    Transport(String),
    #[error("EEPROM helper protocol failed: {0}")]
    Protocol(String),
    #[error(transparent)]
    Helper(#[from] EepromHelperFailure),
}
