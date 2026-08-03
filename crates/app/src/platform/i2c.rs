//! 通用 Linux I²C helper 的 typed 协议与平台服务端口。

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::RemoteOperationControl;

/// GUI、SSH adapter 与目标 helper 共同支持的通用 I²C 协议版本。
pub const I2C_HELPER_SCHEMA_VERSION: u32 = 1;
/// helper JSON stdin 的硬上限；SSH adapter 必须在连接/上传前使用同一预算预检。
pub const I2C_HELPER_MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// helper 直连允许的总 read 字节上限；避免 success JSON 超出 1 MiB stdout 边界。
pub const I2C_HELPER_MAX_TOTAL_READ_BYTES: usize = 128 * 1024;

/// 单个 helper 请求最多包含的 `I2C_RDWR` transaction 数；Baton typed row 全量读取需要超过 128 行。
pub const I2C_HELPER_MAX_TRANSACTIONS_PER_REQUEST: usize = 256;

/// 单个 `I2C_RDWR` transaction 最多包含的 message 数。
pub const I2C_HELPER_MAX_MESSAGES_PER_TRANSACTION: usize = 42;

/// 单条 read/write message 的最大 payload 长度。
pub const I2C_HELPER_MAX_MESSAGE_BYTES: usize = 8192;

/// 单次 helper 请求；每个 `Transfer` transaction 对应一次 Linux `I2C_RDWR` ioctl。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cHelperRequest {
    pub schema_version: u32,
    pub action: I2cHelperAction,
}

/// 通用 I²C helper 只暴露 bus 枚举和原始 message 序列执行；寄存器/端序由 PC 侧配置编译。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum I2cHelperAction {
    ListBuses,
    Transfer {
        transactions: Vec<I2cTransactionSpec>,
    },
}

/// 一个原子 I²C_RDWR transaction；message 顺序就是总线上的顺序，write→read 表达 repeated-start。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cTransactionSpec {
    pub bus: u32,
    pub messages: Vec<I2cMessageSpec>,
    /// 成功执行该 transaction 后的等待时间；用于 EEPROM page-write cycle settle。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle_ms: Option<u16>,
}

/// 单条 I²C message；`flags` 只允许可审计的 Linux message 修饰位，READ 位由 direction 派生。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cMessageSpec {
    pub address: u16,
    #[serde(default)]
    pub flags: Vec<I2cMessageFlag>,
    pub data: I2cMessageData,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "direction", rename_all = "snake_case", deny_unknown_fields)]
pub enum I2cMessageData {
    Write { bytes: Vec<u8> },
    Read { byte_len: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum I2cMessageFlag {
    TenBitAddress,
    Stop,
    NoStart,
    IgnoreNack,
    IgnoreAck,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cBusInfo {
    pub bus: u32,
    pub dev_path: String,
    pub name: Option<String>,
    pub dev_node_exists: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cTransactionResult {
    pub bus: u32,
    pub transferred_messages: u32,
    pub messages: Vec<I2cMessageResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cMessageResult {
    pub address: u16,
    pub direction: I2cMessageDirection,
    pub byte_len: u16,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum I2cMessageDirection {
    Write,
    Read,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum I2cHelperResult {
    BusList {
        buses: Vec<I2cBusInfo>,
    },
    Transfer {
        transactions: Vec<I2cTransactionResult>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("{code}: {message}")]
#[serde(deny_unknown_fields)]
pub struct I2cHelperFailure {
    pub code: String,
    pub message: String,
    pub transaction_index: Option<usize>,
    pub message_index: Option<usize>,
}

/// helper 即使失败也必须输出结构化结果，禁止 GUI 从 stderr 猜测状态。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum I2cHelperOutput {
    Success { result: I2cHelperResult },
    Failure { failure: I2cHelperFailure },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cHelperOperation {
    pub action: I2cHelperAction,
}

pub trait I2cHelperService: Send + Sync {
    fn service_id(&self) -> &str;

    /// # Errors
    ///
    /// target 未绑定、SSH 失败、helper 协议损坏或 helper 拒绝执行时返回错误。
    fn execute(
        &self,
        request: I2cHelperOperation,
        control: RemoteOperationControl,
    ) -> Result<I2cHelperResult, I2cHelperServiceError>;
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum I2cHelperServiceError {
    #[error("I2C request is invalid: {0}")]
    InvalidRequest(String),
    #[error("I2C transport failed: {0}")]
    Transport(String),
    #[error("I2C helper protocol failed: {0}")]
    Protocol(String),
    #[error(transparent)]
    Helper(#[from] I2cHelperFailure),
}

/// I²C helper 请求的结构/字段校验错误；adapter 与 helper 共用同一组上限。
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct I2cHelperRequestValidationError {
    pub code: &'static str,
    pub message: String,
    pub transaction_index: Option<usize>,
    pub message_index: Option<usize>,
}

impl I2cHelperRequestValidationError {
    fn request(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            transaction_index: None,
            message_index: None,
        }
    }

    fn transaction(
        code: &'static str,
        transaction_index: usize,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            transaction_index: Some(transaction_index),
            message_index: None,
        }
    }

    fn message(
        code: &'static str,
        transaction_index: usize,
        message_index: usize,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            transaction_index: Some(transaction_index),
            message_index: Some(message_index),
        }
    }
}

/// 校验 helper action 的结构限制和物理安全边界，不做任何 I/O。
///
/// # Errors
///
/// action 超过 helper 可执行的 transaction/message 数量、地址范围、读写长度或总 read 预算时返回错误。
pub fn validate_i2c_helper_action(
    action: &I2cHelperAction,
) -> Result<(), I2cHelperRequestValidationError> {
    match action {
        I2cHelperAction::ListBuses => Ok(()),
        I2cHelperAction::Transfer { transactions } => {
            validate_i2c_transfer_transactions(transactions)
        }
    }
}

/// 校验 `Transfer` transaction 列表；供 SSH adapter 预检和 helper 直连入口共用。
///
/// # Errors
///
/// transaction/message 数量、地址范围、读写长度或总 read 预算超过 helper 边界时返回错误。
pub fn validate_i2c_transfer_transactions(
    transactions: &[I2cTransactionSpec],
) -> Result<(), I2cHelperRequestValidationError> {
    if transactions.is_empty() {
        return Err(I2cHelperRequestValidationError::request(
            "invalid_request",
            "transfer requires at least one transaction",
        ));
    }
    if transactions.len() > I2C_HELPER_MAX_TRANSACTIONS_PER_REQUEST {
        return Err(I2cHelperRequestValidationError::request(
            "invalid_request",
            format!(
                "transfer supports at most {I2C_HELPER_MAX_TRANSACTIONS_PER_REQUEST} transactions, got {}",
                transactions.len()
            ),
        ));
    }

    let mut total_read_bytes = 0_usize;
    for (transaction_index, transaction) in transactions.iter().enumerate() {
        if transaction.messages.is_empty() {
            return Err(I2cHelperRequestValidationError::transaction(
                "invalid_transaction",
                transaction_index,
                "transaction requires at least one message",
            ));
        }
        if transaction.messages.len() > I2C_HELPER_MAX_MESSAGES_PER_TRANSACTION {
            return Err(I2cHelperRequestValidationError::transaction(
                "invalid_transaction",
                transaction_index,
                format!(
                    "Linux I2C_RDWR supports at most {I2C_HELPER_MAX_MESSAGES_PER_TRANSACTION} messages per transaction, got {}",
                    transaction.messages.len()
                ),
            ));
        }

        for (message_index, message) in transaction.messages.iter().enumerate() {
            let ten_bit = message
                .flags
                .iter()
                .any(|flag| matches!(flag, I2cMessageFlag::TenBitAddress));
            if message.address < 0x0003 {
                return Err(I2cHelperRequestValidationError::message(
                    "invalid_message",
                    transaction_index,
                    message_index,
                    format!(
                        "message address 0x{:x} is below 0x03; 0x00 is I2C General Call and low reserved addresses are not accepted",
                        message.address
                    ),
                ));
            }
            let max_address = if ten_bit { 0x03ff } else { 0x007f };
            if message.address > max_address {
                return Err(I2cHelperRequestValidationError::message(
                    "invalid_message",
                    transaction_index,
                    message_index,
                    format!(
                        "message address 0x{:x} exceeds {}-bit I2C range",
                        message.address,
                        if ten_bit { 10 } else { 7 }
                    ),
                ));
            }

            match &message.data {
                I2cMessageData::Write { bytes } => validate_i2c_message_len(
                    transaction_index,
                    message_index,
                    bytes.len(),
                    "write payload",
                )?,
                I2cMessageData::Read { byte_len } => {
                    let byte_len = usize::from(*byte_len);
                    validate_i2c_message_len(
                        transaction_index,
                        message_index,
                        byte_len,
                        "read length",
                    )?;
                    total_read_bytes = total_read_bytes.checked_add(byte_len).ok_or_else(|| {
                        I2cHelperRequestValidationError::request(
                            "invalid_request",
                            "transfer total read bytes overflowed host usize",
                        )
                    })?;
                    if total_read_bytes > I2C_HELPER_MAX_TOTAL_READ_BYTES {
                        return Err(I2cHelperRequestValidationError::request(
                            "invalid_request",
                            format!(
                                "transfer total read bytes must be <= {I2C_HELPER_MAX_TOTAL_READ_BYTES}, got {total_read_bytes}"
                            ),
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

fn validate_i2c_message_len(
    transaction_index: usize,
    message_index: usize,
    byte_len: usize,
    label: &'static str,
) -> Result<(), I2cHelperRequestValidationError> {
    if byte_len == 0 || byte_len > I2C_HELPER_MAX_MESSAGE_BYTES {
        return Err(I2cHelperRequestValidationError::message(
            "invalid_message",
            transaction_index,
            message_index,
            format!(
                "{label} must contain 1..={I2C_HELPER_MAX_MESSAGE_BYTES} bytes, got {byte_len}"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_request_roundtrips_as_tagged_json() {
        let request = I2cHelperRequest {
            schema_version: I2C_HELPER_SCHEMA_VERSION,
            action: I2cHelperAction::Transfer {
                transactions: vec![I2cTransactionSpec {
                    bus: 7,
                    messages: vec![
                        I2cMessageSpec {
                            address: 0x50,
                            flags: Vec::new(),
                            data: I2cMessageData::Write {
                                bytes: vec![0x00, 0x10],
                            },
                        },
                        I2cMessageSpec {
                            address: 0x50,
                            flags: vec![I2cMessageFlag::IgnoreNack],
                            data: I2cMessageData::Read { byte_len: 4 },
                        },
                    ],
                    settle_ms: None,
                }],
            },
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["action"]["action"], "transfer");
        assert_eq!(
            json["action"]["transactions"][0]["messages"][1]["data"]["direction"],
            "read"
        );
        assert_eq!(
            json["action"]["transactions"][0]["messages"][1]["data"]["byte_len"],
            4
        );

        let decoded: I2cHelperRequest = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn unknown_request_fields_are_rejected() {
        let json = serde_json::json!({
            "schema_version": I2C_HELPER_SCHEMA_VERSION,
            "action": { "action": "list_buses" },
            "unexpected": true
        });

        let error = serde_json::from_value::<I2cHelperRequest>(json).unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn shared_validation_rejects_oversized_write_payload() {
        let action = I2cHelperAction::Transfer {
            transactions: vec![I2cTransactionSpec {
                bus: 7,
                messages: vec![I2cMessageSpec {
                    address: 0x50,
                    flags: Vec::new(),
                    data: I2cMessageData::Write {
                        bytes: vec![0x5a; I2C_HELPER_MAX_MESSAGE_BYTES + 1],
                    },
                }],
                settle_ms: None,
            }],
        };

        let error = validate_i2c_helper_action(&action).unwrap_err();

        assert_eq!(error.code, "invalid_message");
        assert_eq!(error.transaction_index, Some(0));
        assert_eq!(error.message_index, Some(0));
        assert!(error.message.contains("write payload"));
    }

    #[test]
    fn failure_output_roundtrips_with_location() {
        let output = I2cHelperOutput::Failure {
            failure: I2cHelperFailure {
                code: "transfer_failed".to_owned(),
                message: "Remote I/O error".to_owned(),
                transaction_index: Some(1),
                message_index: Some(0),
            },
        };

        let json = serde_json::to_value(&output).unwrap();
        assert_eq!(json["status"], "failure");
        assert_eq!(json["failure"]["transaction_index"], 1);
        assert_eq!(json["failure"]["message_index"], 0);
        let decoded: I2cHelperOutput = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, output);
    }
}
