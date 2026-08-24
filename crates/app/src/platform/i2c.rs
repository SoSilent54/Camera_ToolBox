//! 通用 Linux I²C helper 的 typed 协议与平台服务端口。

use camera_toolbox_core::{ChecksumContract, I2cMapDefinition};
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
/// inspect 所需的 EEPROM 连续读取区间。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cReadRange {
    pub offset: u16,
    pub byte_len: u16,
}

/// inspect/readback 使用的已编译 map 验证合同；不再按 plan.map_id 重新查找 builtin。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cMapValidationContract {
    pub(crate) image_bytes: u16,
    pub(crate) fixed_bytes: Vec<(u16, Vec<u8>)>,
    pub(crate) checksums: Vec<ChecksumContract>,
    pub(crate) serial_range: Option<I2cReadRange>,
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
            checksums: map.checksums.clone(),
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

/// 一个经 map 编译并固定目标的 I²C 设备身份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cTaskTarget {
    pub bus: u32,
    pub address: u16,
    pub address_width_bytes: u8,
    pub page_size_bytes: u16,
    pub write_cycle_ms: u16,
}

/// 只读检查计划；它不能表达写入操作。内部 seal 阻止外部 crate 伪造或篡改 map 编译结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cInspectPlan {
    pub map_id: String,
    pub map_digest: String,
    pub target: I2cTaskTarget,
    pub read_ranges: Vec<I2cReadRange>,
    pub(crate) validation: I2cMapValidationContract,
    seal: I2cPlanSeal,
}

/// 一页 EEPROM 写入及其强制 readback。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cPageWrite {
    pub offset: u16,
    pub bytes: Vec<u8>,
    pub settle_ms: u16,
}

/// 尚未授权的候选写计划；不得直接交给 platform executor。内部 seal 只允许 map builder 创建。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cCandidateWritePlan {
    pub map_id: String,
    pub map_digest: String,
    pub plan_digest: String,
    pub target: I2cTaskTarget,
    pub pages: Vec<I2cPageWrite>,
    pub verify_after_write: bool,
    seal: I2cPlanSeal,
}

/// inspect 返回的绑定快照。完整 before image 只保留在进程内运行时包中。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cInspectSnapshot {
    pub connection_id: String,
    /// Inspector 消费的 sealed plan；approval 只能使用该次实际读取对应的计划。
    pub inspect_plan: I2cInspectPlan,
    pub map_id: String,
    pub map_digest: String,
    pub target: I2cTaskTarget,
    pub before_image_sha256: String,
    pub before_image: Vec<u8>,
}

/// 明确审批产生的不可伪造运行时绑定；不实现 serde，禁止落盘。
///
/// 平台端在首个写页前必须根据 `inspect_plan` 重读并校验 `expected_before_sha256`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cAuthorizedWritePlan {
    pub connection_id: String,
    pub expected_before_sha256: String,
    /// 授权密封的 inspect image，用于 executor 计算最终完整镜像，避免 snapshot 旁路输入。
    pub before_image: Vec<u8>,
    pub inspect_plan: I2cInspectPlan,
    pub candidate: I2cCandidateWritePlan,
    seal: I2cPlanSeal,
}

/// 非持久化计划的 crate-private 完整性 seal。私有字段使外部调用方不能构造它，摘要可检测 Clone 后的公开字段篡改。
#[derive(Clone, Debug, PartialEq, Eq)]
struct I2cPlanSeal(String);

impl I2cInspectPlan {
    pub(crate) fn new(
        map_id: String,
        map_digest: String,
        target: I2cTaskTarget,
        read_ranges: Vec<I2cReadRange>,
        validation: I2cMapValidationContract,
    ) -> Self {
        let seal = inspect_plan_seal(&map_id, &map_digest, &target, &read_ranges, &validation);
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
            == inspect_plan_seal(
                &self.map_id,
                &self.map_digest,
                &self.target,
                &self.read_ranges,
                &self.validation,
            )
    }
}

impl I2cCandidateWritePlan {
    pub(crate) fn new(
        map_id: String,
        map_digest: String,
        target: I2cTaskTarget,
        pages: Vec<I2cPageWrite>,
        verify_after_write: bool,
    ) -> Self {
        let plan_digest = candidate_plan_digest(&map_digest, &pages);
        let seal = candidate_plan_seal(
            &map_id,
            &map_digest,
            &plan_digest,
            &target,
            &pages,
            verify_after_write,
        );
        Self {
            map_id,
            map_digest,
            plan_digest,
            target,
            pages,
            verify_after_write,
            seal,
        }
    }

    #[must_use]
    pub fn is_compiled(&self) -> bool {
        self.plan_digest == candidate_plan_digest(&self.map_digest, &self.pages)
            && self.seal
                == candidate_plan_seal(
                    &self.map_id,
                    &self.map_digest,
                    &self.plan_digest,
                    &self.target,
                    &self.pages,
                    self.verify_after_write,
                )
    }
}

impl I2cAuthorizedWritePlan {
    pub(crate) fn new(
        connection_id: String,
        expected_before_sha256: String,
        before_image: Vec<u8>,
        inspect_plan: I2cInspectPlan,
        candidate: I2cCandidateWritePlan,
    ) -> Self {
        let seal = authorized_plan_seal(
            &connection_id,
            &expected_before_sha256,
            &before_image,
            &inspect_plan,
            &candidate,
        );
        Self {
            connection_id,
            expected_before_sha256,
            before_image,
            inspect_plan,
            candidate,
            seal,
        }
    }

    #[must_use]
    pub fn is_authorized(&self) -> bool {
        self.inspect_plan.is_compiled()
            && self.candidate.is_compiled()
            && self.inspect_plan.map_id == self.candidate.map_id
            && self.inspect_plan.map_digest == self.candidate.map_digest
            && self.inspect_plan.target == self.candidate.target
            && self.seal
                == authorized_plan_seal(
                    &self.connection_id,
                    &self.expected_before_sha256,
                    &self.before_image,
                    &self.inspect_plan,
                    &self.candidate,
                )
    }

    #[must_use]
    pub fn page_at(&self, index: usize) -> Option<&I2cPageWrite> {
        self.is_authorized()
            .then(|| self.candidate.pages.get(index))
            .flatten()
    }
}

fn inspect_plan_seal(
    map_id: &str,
    map_digest: &str,
    target: &I2cTaskTarget,
    ranges: &[I2cReadRange],
    validation: &I2cMapValidationContract,
) -> I2cPlanSeal {
    let mut digest = I2cPlanDigest::new("inspect");
    digest.text(map_id);
    digest.text(map_digest);
    digest.target(target);
    for range in ranges {
        digest.u16(range.offset);
        digest.u16(range.byte_len);
    }
    digest.u16(validation.image_bytes);
    digest.u32(validation.fixed_bytes.len() as u32);
    for (offset, bytes) in &validation.fixed_bytes {
        digest.u16(*offset);
        digest.bytes(bytes);
    }
    digest.u32(validation.checksums.len() as u32);
    for checksum in &validation.checksums {
        digest.u16(checksum.target_offset);
        digest.u16(checksum.source_offset);
        digest.u16(checksum.source_byte_len);
        digest.text(&format!("{:?}", checksum.algorithm));
    }
    match &validation.serial_range {
        Some(range) => {
            digest.bool(true);
            digest.u16(range.offset);
            digest.u16(range.byte_len);
        }
        None => digest.bool(false),
    }
    digest.finish()
}

fn candidate_plan_seal(
    map_id: &str,
    map_digest: &str,
    plan_digest: &str,
    target: &I2cTaskTarget,
    pages: &[I2cPageWrite],
    verify_after_write: bool,
) -> I2cPlanSeal {
    let mut digest = I2cPlanDigest::new("candidate");
    digest.text(map_id);
    digest.text(map_digest);
    digest.text(plan_digest);
    digest.target(target);
    digest.bool(verify_after_write);
    digest.pages(pages);
    digest.finish()
}

fn authorized_plan_seal(
    connection_id: &str,
    before_sha256: &str,
    before_image: &[u8],
    inspect: &I2cInspectPlan,
    candidate: &I2cCandidateWritePlan,
) -> I2cPlanSeal {
    let mut digest = I2cPlanDigest::new("authorization");
    digest.text(connection_id);
    digest.text(before_sha256);
    digest.bytes(before_image);
    digest.text(&inspect.seal.0);
    digest.text(&candidate.seal.0);
    digest.finish()
}

fn candidate_plan_digest(map_digest: &str, pages: &[I2cPageWrite]) -> String {
    let mut digest = I2cPlanDigest::new("candidate-pages");
    digest.text(map_digest);
    digest.pages(pages);
    digest.finish().0
}

struct I2cPlanDigest(Sha256);

impl I2cPlanDigest {
    fn new(domain: &str) -> Self {
        let mut digest = Self(Sha256::new());
        digest.text("camera-toolbox/i2c-task-plan/v1");
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cPageExecutionReport {
    pub offset: u16,
    pub expected: Vec<u8>,
    pub readback: Option<Vec<u8>>,
    pub error: Option<String>,
}

/// I²C 计划执行报告；没有 rollback 字段，因为本路径绝不自动回滚。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cExecutionReport {
    pub before_image_sha256: String,
    pub pages: Vec<I2cPageExecutionReport>,
    pub final_verified: bool,
    pub error: Option<String>,
}

/// 计划专用 I²C 平台端口。它没有原始 transaction 或 shell 输入，调用方只能 inspect 或逐页写入。
pub trait I2cTaskExecutor: Send + Sync {
    fn inspect(
        &self,
        connection: &super::SshConnection,
        plan: &I2cInspectPlan,
        control: RemoteOperationControl,
    ) -> Result<Vec<u8>, String>;

    /// 在首个写页前远端重读 `inspect_plan` 并拒绝 before-image/session/target 不匹配。
    fn verify_authorized(
        &self,
        connection: &super::SshConnection,
        authorized: &I2cAuthorizedWritePlan,
        control: RemoteOperationControl,
    ) -> Result<(), String>;

    /// `page_index` 与 sealed authorized plan 中的页必须精确相同；实现不得把调用方传入的页当作自由 payload。
    fn write_page(
        &self,
        connection: &super::SshConnection,
        authorized: &I2cAuthorizedWritePlan,
        page_index: usize,
        page: &I2cPageWrite,
        control: RemoteOperationControl,
    ) -> Result<Vec<u8>, String>;
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
