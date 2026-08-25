
use camera_toolbox_core::{ChecksumAlgorithm, I2cMapDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

/// 通用 I²C helper 暴露 bus 枚举、原始 message 序列执行，以及单请求锁保护的 EEPROM 写入。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum I2cHelperAction {
    ListBuses,
    Transfer {
        transactions: Vec<I2cTransactionSpec>,
    },
    GuardedWrite {
        request: I2cGuardedWriteRequest,
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
    GuardedWrite {
        report: I2cExecutionReport,
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
        I2cHelperAction::GuardedWrite { request } => validate_guarded_write_request(request),
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
/// inspect/read/write 所需的 EEPROM 连续读取区间。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cReadRange {
    pub offset: u16,
    pub byte_len: u16,
}

/// inspect/readback 使用的已编译 map 验证合同；不再按 plan.map_id 重新查找 builtin。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct I2cMapValidationContract {
    pub image_bytes: u16,
    pub fixed_bytes: Vec<(u16, Vec<u8>)>,
    /// 已解析到物理字节跨度，helper 不需要理解 map 逻辑字段。
    pub checksums: Vec<I2cChecksumValidation>,
    pub serial_range: Option<I2cReadRange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct I2cChecksumValidation {
    pub target_offset: u16,
    pub source_ranges: Vec<I2cReadRange>,
    pub algorithm: ChecksumAlgorithm,
}

impl I2cMapValidationContract {
    pub(crate) fn from_map(map: &I2cMapDefinition) -> Self {
        Self {
            image_bytes: map.image_bytes,
            fixed_bytes: map
                .fixed_bytes
                .iter()
                .map(|fixed| (fixed.offset, fixed.bytes.clone()))
                .collect(),
            checksums: map
                .checksums
                .iter()
                .map(|checksum| I2cChecksumValidation {
                    target_offset: checksum.target_offset,
                    source_ranges: checksum
                        .source_ranges(map)
                        .expect("validated I2C map must resolve checksum source ranges")
                        .into_iter()
                        .map(|(offset, byte_len)| I2cReadRange { offset, byte_len })
                        .collect(),
                    algorithm: checksum.algorithm,
                })
                .collect(),
            serial_range: (map.id == "yg-stereo-p24c64g-v1")
                .then(|| {
                    map.inputs
                        .iter()
                        .find(|slot| slot.name == "serial.number")
                        .and_then(|slot| slot.target.as_ref())
                        .map(|target| I2cReadRange {
                            offset: target.offset,
                            byte_len: target.byte_len,
                        })
                })
                .flatten(),
        }
    }
}

/// 校验已读镜像是否满足 map 合同：长度、固定字节、校验和与 SNID 序列号区。
///
/// helper 最终校验与 GUI read 报告共用同一实现，避免两端合同漂移。
pub fn validate_map_image(
    validation: &I2cMapValidationContract,
    image: &[u8],
) -> Result<(), String> {
    if image.len() != usize::from(validation.image_bytes) {
        return Err(format!(
            "EEPROM image has {} bytes; map requires {}",
            image.len(),
            validation.image_bytes
        ));
    }
    for (offset, expected) in &validation.fixed_bytes {
        let start = usize::from(*offset);
        let end = start
            .checked_add(expected.len())
            .ok_or_else(|| "fixed-byte range overflow".to_owned())?;
        if image.get(start..end) != Some(expected.as_slice()) {
            return Err(format!(
                "EEPROM map-required fixed bytes at offset {start:#x} are absent"
            ));
        }
    }
    for checksum in &validation.checksums {
        let sum = checksum.source_ranges.iter().try_fold(0_u32, |sum, range| {
            let start = usize::from(range.offset);
            let end = start
                .checked_add(usize::from(range.byte_len))
                .ok_or_else(|| "checksum source range overflow".to_owned())?;
            let source = image
                .get(start..end)
                .ok_or_else(|| "checksum source range is outside image".to_owned())?;
            Ok::<_, String>(source.iter().fold(sum, |total, byte| total + u32::from(*byte)))
        })?;
        let expected = match checksum.algorithm {
            ChecksumAlgorithm::SerialSumMod255PlusOne => ((sum % 0xff) + 1) as u8,
        };
        if image.get(usize::from(checksum.target_offset)) != Some(&expected) {
            return Err(format!(
                "EEPROM map-required checksum at offset {:#x} is invalid",
                checksum.target_offset
            ));
        }
    }
    if let Some(serial) = &validation.serial_range {
        let start = usize::from(serial.offset);
        let end = start
            .checked_add(usize::from(serial.byte_len))
            .ok_or_else(|| "serial range overflow".to_owned())?;
        let bytes = image
            .get(start..end)
            .ok_or_else(|| "serial range is outside image".to_owned())?;
        if !valid_yg_snid(bytes) {
            return Err(
                "EEPROM map-required serial is blank, contains control bytes, or violates SNID format"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// YG 平台 SNID 布局：2T23x + 2 位数字 + 分类字符 + 2 位数字 + 字母数字 + "00"。
fn valid_yg_snid(serial: &[u8]) -> bool {
    serial.len() == 14
        && matches!(&serial[..5], b"2T233" | b"2T235")
        && serial[5..7].iter().all(u8::is_ascii_digit)
        && matches!(serial[7], b'1'..=b'9' | b'A'..=b'C')
        && matches!(serial[8], b'1'..=b'9' | b'A'..=b'V')
        && matches!(serial[9], b'0'..=b'4')
        && serial[10..12]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric())
        && serial[12..] == *b"00"
}

/// 一个经 map 编译并固定目标的 I²C 设备身份。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct I2cTaskTarget {
    pub bus: u32,
    pub address: u16,
    pub address_width_bytes: u8,
    pub page_size_bytes: u16,
    pub write_cycle_ms: u16,
}

/// map 编译出的原子读请求；只读取 map 声明的范围，不表达写入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cReadRequest {
    pub map_id: String,
    pub map_digest: String,
    pub target: I2cTaskTarget,
    pub read_ranges: Vec<I2cReadRange>,
    pub validation: I2cMapValidationContract,
    seal: I2cPlanSeal,
}

/// 一页 EEPROM 写入及其强制 readback。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct I2cPageWrite {
    pub offset: u16,
    pub bytes: Vec<u8>,
    pub settle_ms: u16,
}

/// map 编译出的原子写请求；执行端必须在单个 helper 请求内完成 fail-stop 写入与校验。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cWriteRequest {
    pub map_id: String,
    pub map_digest: String,
    pub plan_digest: String,
    pub target: I2cTaskTarget,
    pub read_ranges: Vec<I2cReadRange>,
    pub validation: I2cMapValidationContract,
    pub pages: Vec<I2cPageWrite>,
    pub expected_final_image: Vec<u8>,
    pub verify_after_write: bool,
    seal: I2cPlanSeal,
}

/// 目标 helper 可序列化的锁保护写请求；一次请求持有同一设备锁直到结束。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct I2cGuardedWriteRequest {
    pub target: I2cTaskTarget,
    pub read_ranges: Vec<I2cReadRange>,
    pub validation: I2cMapValidationContract,
    pub expected_before_sha256: Option<String>,
    pub pages: Vec<I2cPageWrite>,
    pub expected_final_image: Vec<u8>,
    pub verify_after_write: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct I2cReadReport {
    pub map_id: String,
    pub map_digest: String,
    pub target: I2cTaskTarget,
    pub image_sha256: String,
    pub byte_len: usize,
    pub valid: bool,
    pub error: Option<String>,
}

/// 非持久化计划的 crate-private 完整性 seal。私有字段使外部调用方不能构造它，摘要可检测 Clone 后的公开字段篡改。
#[derive(Clone, Debug, PartialEq, Eq)]
struct I2cPlanSeal(String);

impl I2cReadRequest {
    pub(crate) fn new(
        map_id: String,
        map_digest: String,
        target: I2cTaskTarget,
        read_ranges: Vec<I2cReadRange>,
        validation: I2cMapValidationContract,
    ) -> Self {
        let seal = read_request_seal(&map_id, &map_digest, &target, &read_ranges, &validation);
        Self {
            map_id,
            map_digest,
            target,
            read_ranges,
            validation,
            seal,
        }
    }

    #[must_use]
    pub fn is_compiled(&self) -> bool {
        self.seal
            == read_request_seal(
                &self.map_id,
                &self.map_digest,
                &self.target,
                &self.read_ranges,
                &self.validation,
            )
    }
}

impl I2cWriteRequest {
    pub(crate) fn new(
        map_id: String,
        map_digest: String,
        target: I2cTaskTarget,
        read_ranges: Vec<I2cReadRange>,
        validation: I2cMapValidationContract,
        pages: Vec<I2cPageWrite>,
        expected_final_image: Vec<u8>,
        verify_after_write: bool,
    ) -> Self {
        let plan_digest = write_request_digest(&map_digest, &pages, &expected_final_image);
        let seal = write_request_seal(
            &map_id,
            &map_digest,
            &plan_digest,
            &target,
            &read_ranges,
            &validation,
            &pages,
            &expected_final_image,
            verify_after_write,
        );
        Self {
            map_id,
            map_digest,
            plan_digest,
            target,
            read_ranges,
            validation,
            pages,
            expected_final_image,
            verify_after_write,
            seal,
        }
    }

    #[must_use]
    pub fn is_compiled(&self) -> bool {
        self.plan_digest == write_request_digest(&self.map_digest, &self.pages, &self.expected_final_image)
            && self.seal
                == write_request_seal(
                    &self.map_id,
                    &self.map_digest,
                    &self.plan_digest,
                    &self.target,
                    &self.read_ranges,
                    &self.validation,
                    &self.pages,
                    &self.expected_final_image,
                    self.verify_after_write,
                )
    }

    #[must_use]
    pub fn guarded_request(&self, expected_before_sha256: Option<String>) -> Option<I2cGuardedWriteRequest> {
        self.is_compiled().then(|| I2cGuardedWriteRequest {
            target: self.target.clone(),
            read_ranges: self.read_ranges.clone(),
            validation: self.validation.clone(),
            expected_before_sha256,
            pages: self.pages.clone(),
            expected_final_image: self.expected_final_image.clone(),
            verify_after_write: self.verify_after_write,
        })
    }
}

fn read_request_seal(
    map_id: &str,
    map_digest: &str,
    target: &I2cTaskTarget,
    ranges: &[I2cReadRange],
    validation: &I2cMapValidationContract,
) -> I2cPlanSeal {
    let mut digest = I2cPlanDigest::new("read");
    digest.text(map_id);
    digest.text(map_digest);
    digest.target(target);
    digest.ranges(ranges);
    digest.validation(validation);
    digest.finish()
}

fn write_request_seal(
    map_id: &str,
    map_digest: &str,
    plan_digest: &str,
    target: &I2cTaskTarget,
    ranges: &[I2cReadRange],
    validation: &I2cMapValidationContract,
    pages: &[I2cPageWrite],
    expected_final_image: &[u8],
    verify_after_write: bool,
) -> I2cPlanSeal {
    let mut digest = I2cPlanDigest::new("write");
    digest.text(map_id);
    digest.text(map_digest);
    digest.text(plan_digest);
    digest.target(target);
    digest.ranges(ranges);
    digest.validation(validation);
    digest.bool(verify_after_write);
    digest.pages(pages);
    digest.bytes(expected_final_image);
    digest.finish()
}

fn write_request_digest(map_digest: &str, pages: &[I2cPageWrite], expected_final_image: &[u8]) -> String {
    let mut digest = I2cPlanDigest::new("write-pages");
    digest.text(map_digest);
    digest.pages(pages);
    digest.bytes(expected_final_image);
    digest.finish().0
}

struct I2cPlanDigest(Sha256);

impl I2cPlanDigest {
    fn new(domain: &str) -> Self {
        let mut digest = Self(Sha256::new());
        digest.text("camera-toolbox/i2c-task-plan/v2");
        digest.text(domain);
        digest
    }
    fn text(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }
    fn bytes(&mut self, value: &[u8]) {
        self.0.update((value.len() as u64).to_le_bytes());
        self.0.update(value);
    }
    fn u16(&mut self, value: u16) {
        self.0.update(value.to_le_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.0.update(value.to_le_bytes());
    }
    fn bool(&mut self, value: bool) {
        self.0.update([u8::from(value)]);
    }
    fn target(&mut self, target: &I2cTaskTarget) {
        self.u32(target.bus);
        self.u16(target.address);
        self.0.update([target.address_width_bytes]);
        self.u16(target.page_size_bytes);
        self.u16(target.write_cycle_ms);
    }
    fn ranges(&mut self, ranges: &[I2cReadRange]) {
        self.0.update((ranges.len() as u64).to_le_bytes());
        for range in ranges {
            self.u16(range.offset);
            self.u16(range.byte_len);
        }
    }
    fn validation(&mut self, validation: &I2cMapValidationContract) {
        self.u16(validation.image_bytes);
        self.u32(validation.fixed_bytes.len() as u32);
        for (offset, bytes) in &validation.fixed_bytes {
            self.u16(*offset);
            self.bytes(bytes);
        }
        self.u32(validation.checksums.len() as u32);
        for checksum in &validation.checksums {
            self.u16(checksum.target_offset);
            self.u32(checksum.source_ranges.len() as u32);
            for range in &checksum.source_ranges {
                self.u16(range.offset);
                self.u16(range.byte_len);
            }
            self.text(&format!("{:?}", checksum.algorithm));
        }
        match &validation.serial_range {
            Some(range) => {
                self.bool(true);
                self.u16(range.offset);
                self.u16(range.byte_len);
            }
            None => self.bool(false),
        }
    }
    fn pages(&mut self, pages: &[I2cPageWrite]) {
        self.0.update((pages.len() as u64).to_le_bytes());
        for page in pages {
            self.u16(page.offset);
            self.u16(page.settle_ms);
            self.bytes(&page.bytes);
        }
    }
    fn finish(self) -> I2cPlanSeal {
        let mut text = String::with_capacity(71);
        text.push_str("sha256:");
        for byte in self.0.finalize() {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
        }
        I2cPlanSeal(text)
    }
}

/// 单页执行结果；失败页也要进入报告，后续页永不执行。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct I2cPageExecutionReport {
    pub offset: u16,
    pub expected: Vec<u8>,
    pub readback: Option<Vec<u8>>,
    pub error: Option<String>,
}

/// I²C 写入报告；没有 rollback 字段，因为本路径绝不自动回滚。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct I2cExecutionReport {
    pub before_image_sha256: String,
    pub pages: Vec<I2cPageExecutionReport>,
    pub final_verified: bool,
    pub error: Option<String>,
}

/// 计划专用 I²C 平台端口。读和写都是一次 helper 请求；写请求由目标端持锁完成 fail-stop 校验。
pub trait I2cTaskExecutor: Send + Sync {
    fn read(
        &self,
        connection: &super::SshConnection,
        request: &I2cReadRequest,
        control: RemoteOperationControl,
    ) -> Result<I2cReadReport, String>;

    fn write(
        &self,
        connection: &super::SshConnection,
        request: &I2cWriteRequest,
        control: RemoteOperationControl,
    ) -> Result<I2cExecutionReport, String>;
}

fn validate_guarded_write_request(request: &I2cGuardedWriteRequest) -> Result<(), I2cHelperRequestValidationError> {
    validate_i2c_target(&request.target)?;
    validate_read_ranges(&request.read_ranges, Some(request.expected_final_image.len()))?;
    if request.pages.is_empty() {
        return Err(I2cHelperRequestValidationError::request(
            "invalid_request",
            "guarded_write requires at least one page",
        ));
    }
    let transaction_budget = request
        .read_ranges
        .len()
        .checked_mul(2)
        .and_then(|value| value.checked_add(request.pages.len().saturating_mul(2)))
        .ok_or_else(|| I2cHelperRequestValidationError::request("invalid_request", "guarded_write transaction budget overflowed"))?;
    if transaction_budget > I2C_HELPER_MAX_TRANSACTIONS_PER_REQUEST {
        return Err(I2cHelperRequestValidationError::request(
            "invalid_request",
            format!("guarded_write uses {transaction_budget} logical transactions; max is {I2C_HELPER_MAX_TRANSACTIONS_PER_REQUEST}"),
        ));
    }
    for (index, page) in request.pages.iter().enumerate() {
        if page.bytes.is_empty() || page.bytes.len() > usize::from(request.target.page_size_bytes) {
            return Err(I2cHelperRequestValidationError::transaction(
                "invalid_transaction",
                index,
                format!("page byte length must be 1..={} bytes", request.target.page_size_bytes),
            ));
        }
        let payload_len = usize::from(request.target.address_width_bytes)
            .checked_add(page.bytes.len())
            .ok_or_else(|| I2cHelperRequestValidationError::transaction("invalid_transaction", index, "page payload length overflowed"))?;
        validate_i2c_message_len(index, 0, payload_len, "guarded_write page payload")?;
    }
    Ok(())
}

fn validate_i2c_target(target: &I2cTaskTarget) -> Result<(), I2cHelperRequestValidationError> {
    if !matches!(target.address_width_bytes, 1 | 2) {
        return Err(I2cHelperRequestValidationError::request(
            "invalid_request",
            "target address width must be 1 or 2 bytes",
        ));
    }
    if target.address < 0x03 || target.address > 0x7f {
        return Err(I2cHelperRequestValidationError::request(
            "invalid_request",
            format!("target I2C address 0x{:x} is outside accepted 7-bit range", target.address),
        ));
    }
    if target.page_size_bytes == 0 {
        return Err(I2cHelperRequestValidationError::request(
            "invalid_request",
            "target page size must be non-zero",
        ));
    }
    Ok(())
}

fn validate_read_ranges(ranges: &[I2cReadRange], expected_len: Option<usize>) -> Result<(), I2cHelperRequestValidationError> {
    if ranges.is_empty() {
        return Err(I2cHelperRequestValidationError::request(
            "invalid_request",
            "read ranges must not be empty",
        ));
    }
    let mut total = 0_usize;
    for (index, range) in ranges.iter().enumerate() {
        if range.byte_len == 0 {
            return Err(I2cHelperRequestValidationError::transaction(
                "invalid_transaction",
                index,
                "read range byte length must be non-zero",
            ));
        }
        total = total
            .checked_add(usize::from(range.byte_len))
            .ok_or_else(|| I2cHelperRequestValidationError::request("invalid_request", "read range total length overflowed"))?;
    }
    if total > I2C_HELPER_MAX_TOTAL_READ_BYTES {
        return Err(I2cHelperRequestValidationError::request(
            "invalid_request",
            format!("read ranges total {total} exceeds {I2C_HELPER_MAX_TOTAL_READ_BYTES}"),
        ));
    }
    if let Some(expected) = expected_len && total != expected {
        return Err(I2cHelperRequestValidationError::request(
            "invalid_request",
            format!("expected final image is {expected} bytes, but read ranges cover {total}"),
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
