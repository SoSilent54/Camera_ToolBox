//! 通用 I²C helper 引擎；只执行显式 message 序列，不附加 EEPROM 安全语义。
//!
//! `GuardedWrite` 例外：它在一个请求内完成 compare-before、逐页写入 + readback、
//! 以及最终全镜像校验，且全程持有调用方（main）取得的设备锁。

use camera_toolbox_app::{
    I2C_HELPER_SCHEMA_VERSION, I2cBusInfo, I2cExecutionReport, I2cGuardedWriteRequest,
    I2cHelperAction, I2cHelperFailure, I2cHelperOutput, I2cHelperRequest,
    I2cHelperRequestValidationError, I2cHelperResult, I2cMessageData, I2cMessageDirection,
    I2cMessageResult, I2cMessageSpec, I2cPageExecutionReport, I2cPageWrite, I2cReadRange,
    I2cTaskTarget, I2cTransactionResult, I2cTransactionSpec, validate_i2c_helper_action,
    validate_i2c_transfer_transactions, validate_map_image,
};
use sha2::{Digest, Sha256};
use std::time::Duration;

pub(crate) trait I2cBackend {
    fn list_buses(&self) -> Result<Vec<I2cBusInfo>, String>;

    fn transfer(
        &mut self,
        transaction: &I2cTransactionSpec,
    ) -> Result<I2cTransactionResult, I2cTransferError>;
}

#[derive(Debug)]
pub(crate) struct I2cTransferError {
    pub(crate) message: String,
    pub(crate) message_index: Option<usize>,
}

impl I2cTransferError {
    pub(crate) fn transaction(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            message_index: None,
        }
    }
}

#[derive(Debug)]
struct EngineError {
    code: &'static str,
    message: String,
    transaction_index: Option<usize>,
    message_index: Option<usize>,
}

impl EngineError {
    fn request(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            transaction_index: None,
            message_index: None,
        }
    }
}

pub(crate) fn execute(request: I2cHelperRequest, backend: &mut dyn I2cBackend) -> I2cHelperOutput {
    if let Err(output) = validate_request(&request) {
        return output;
    }

    match request.action {
        I2cHelperAction::ListBuses => match backend.list_buses() {
            Ok(buses) => I2cHelperOutput::Success {
                result: I2cHelperResult::BusList { buses },
            },
            Err(message) => failure(EngineError::request("list_buses_failed", message)),
        },
        I2cHelperAction::Transfer { transactions } => transfer(transactions, backend),
        I2cHelperAction::GuardedWrite { request } => guarded_write(request, backend),
    }
}

pub(crate) fn validate_request(request: &I2cHelperRequest) -> Result<(), I2cHelperOutput> {
    if request.schema_version != I2C_HELPER_SCHEMA_VERSION {
        return Err(failure(EngineError::request(
            "unsupported_schema",
            format!(
                "helper supports I2C schema {}, got {}",
                I2C_HELPER_SCHEMA_VERSION, request.schema_version
            ),
        )));
    }

    match &request.action {
        I2cHelperAction::ListBuses => Ok(()),
        I2cHelperAction::Transfer { transactions } => {
            validate_transactions(transactions).map_err(failure)
        }
        I2cHelperAction::GuardedWrite { request } => {
            validate_i2c_helper_action(&I2cHelperAction::GuardedWrite {
                request: request.clone(),
            })
            .map_err(|error| failure(EngineError::from(error)))
        }
    }
}

fn transfer(
    transactions: Vec<I2cTransactionSpec>,
    backend: &mut dyn I2cBackend,
) -> I2cHelperOutput {
    let mut results = Vec::with_capacity(transactions.len());
    for (transaction_index, transaction) in transactions.iter().enumerate() {
        match backend.transfer(transaction) {
            Ok(result) => {
                results.push(result);
                if let Some(settle_ms) = transaction.settle_ms.filter(|value| *value > 0) {
                    // EEPROM page-write 后必须等待内部写周期结束，避免下一页事务过早开始。
                    std::thread::sleep(std::time::Duration::from_millis(u64::from(settle_ms)));
                }
            }
            Err(error) => {
                return failure(EngineError {
                    code: "transfer_failed",
                    message: error.message,
                    transaction_index: Some(transaction_index),
                    message_index: error.message_index,
                });
            }
        }
    }

    I2cHelperOutput::Success {
        result: I2cHelperResult::Transfer {
            transactions: results,
        },
    }
}

/// 原子写：单个请求内完成 compare-before → 逐页写入+精确 readback → 最终全镜像校验。
///
/// fail-stop：任何一页失败立即停止，后续页永不执行；不自动回滚。所有写路径结果都以
/// `I2cExecutionReport` 形式返回（即使 `final_verified=false`），只有读前 I/O 失败走结构化 Failure。
fn guarded_write(request: I2cGuardedWriteRequest, backend: &mut dyn I2cBackend) -> I2cHelperOutput {
    let before = match read_ranges(&request.target, &request.read_ranges, backend) {
        Ok(image) => image,
        Err(error) => return failure(error),
    };
    let before_image_sha256 = sha256_hex(&before);
    if let Some(expected) = &request.expected_before_sha256 {
        if expected != &before_image_sha256 {
            return success_report(I2cExecutionReport {
                before_image_sha256,
                pages: Vec::new(),
                final_verified: false,
                error: Some("before image digest mismatch; no page was written".to_owned()),
            });
        }
    }

    let mut page_reports = Vec::with_capacity(request.pages.len());
    for (page_index, page) in request.pages.iter().enumerate() {
        let write_transaction = page_write_transaction(&request.target, page);
        if let Err(error) = backend.transfer(&write_transaction) {
            page_reports.push(page_error_report(page, error.message));
            return success_report(failed_report(
                before_image_sha256,
                page_reports,
                format!("page {page_index} write failed"),
            ));
        }
        if page.settle_ms > 0 {
            // EEPROM page-write 后必须等待内部写周期结束，readback 才能读到稳定值。
            std::thread::sleep(Duration::from_millis(u64::from(page.settle_ms)));
        }
        let readback_transaction = page_readback_transaction(&request.target, page);
        let readback = match backend.transfer(&readback_transaction) {
            Ok(result) => read_bytes(&result),
            Err(error) => {
                page_reports.push(page_error_report(page, error.message));
                return success_report(failed_report(
                    before_image_sha256,
                    page_reports,
                    format!("page {page_index} readback failed"),
                ));
            }
        };
        if readback != page.bytes {
            page_reports.push(I2cPageExecutionReport {
                offset: page.offset,
                expected: page.bytes.clone(),
                readback: Some(readback),
                error: Some("readback does not match written bytes".to_owned()),
            });
            return success_report(failed_report(
                before_image_sha256,
                page_reports,
                format!("page {page_index} readback mismatch"),
            ));
        }
        page_reports.push(I2cPageExecutionReport {
            offset: page.offset,
            expected: page.bytes.clone(),
            readback: Some(readback),
            error: None,
        });
    }

    if request.verify_after_write {
        let after = match read_ranges(&request.target, &request.read_ranges, backend) {
            Ok(image) => image,
            Err(error) => {
                return success_report(failed_report(
                    before_image_sha256,
                    page_reports,
                    error.message,
                ));
            }
        };
        if after != request.expected_final_image {
            return success_report(failed_report(
                before_image_sha256,
                page_reports,
                first_difference(&request.expected_final_image, &after),
            ));
        }
        if let Err(error) = validate_map_image(&request.validation, &after) {
            return success_report(failed_report(before_image_sha256, page_reports, error));
        }
    }

    success_report(I2cExecutionReport {
        before_image_sha256,
        pages: page_reports,
        final_verified: true,
        error: None,
    })
}

/// 按 read_ranges 顺序读取并拼接完整镜像；范围必须是 map 合同的连续覆盖。
fn read_ranges(
    target: &I2cTaskTarget,
    ranges: &[I2cReadRange],
    backend: &mut dyn I2cBackend,
) -> Result<Vec<u8>, EngineError> {
    let mut image = Vec::new();
    for range in ranges {
        let transaction = I2cTransactionSpec {
            bus: target.bus,
            messages: vec![
                I2cMessageSpec {
                    address: target.address,
                    flags: Vec::new(),
                    data: I2cMessageData::Write {
                        bytes: register_bytes(target.address_width_bytes, range.offset),
                    },
                },
                I2cMessageSpec {
                    address: target.address,
                    flags: Vec::new(),
                    data: I2cMessageData::Read {
                        byte_len: range.byte_len,
                    },
                },
            ],
            settle_ms: None,
        };
        let result = backend
            .transfer(&transaction)
            .map_err(|error| EngineError {
                code: "guarded_write_read_failed",
                message: error.message,
                transaction_index: None,
                message_index: error.message_index,
            })?;
        image.extend_from_slice(&read_bytes(&result));
    }
    Ok(image)
}

fn read_bytes(result: &I2cTransactionResult) -> Vec<u8> {
    result
        .messages
        .iter()
        .filter(|message| matches!(message.direction, I2cMessageDirection::Read))
        .flat_map(|message| message.bytes.iter().copied())
        .collect()
}

fn register_bytes(width: u8, offset: u16) -> Vec<u8> {
    match width {
        1 => vec![(offset & 0xff) as u8],
        _ => offset.to_be_bytes().to_vec(),
    }
}

fn page_write_transaction(target: &I2cTaskTarget, page: &I2cPageWrite) -> I2cTransactionSpec {
    I2cTransactionSpec {
        bus: target.bus,
        messages: vec![I2cMessageSpec {
            address: target.address,
            flags: Vec::new(),
            data: I2cMessageData::Write {
                bytes: [
                    register_bytes(target.address_width_bytes, page.offset),
                    page.bytes.clone(),
                ]
                .concat(),
            },
        }],
        settle_ms: Some(page.settle_ms),
    }
}

fn page_readback_transaction(target: &I2cTaskTarget, page: &I2cPageWrite) -> I2cTransactionSpec {
    I2cTransactionSpec {
        bus: target.bus,
        messages: vec![
            I2cMessageSpec {
                address: target.address,
                flags: Vec::new(),
                data: I2cMessageData::Write {
                    bytes: register_bytes(target.address_width_bytes, page.offset),
                },
            },
            I2cMessageSpec {
                address: target.address,
                flags: Vec::new(),
                data: I2cMessageData::Read {
                    byte_len: u16::try_from(page.bytes.len())
                        .expect("validated page length fits u16"),
                },
            },
        ],
        settle_ms: None,
    }
}

fn page_error_report(page: &I2cPageWrite, error: String) -> I2cPageExecutionReport {
    I2cPageExecutionReport {
        offset: page.offset,
        expected: page.bytes.clone(),
        readback: None,
        error: Some(error),
    }
}

fn failed_report(
    before_image_sha256: String,
    pages: Vec<I2cPageExecutionReport>,
    error: impl Into<String>,
) -> I2cExecutionReport {
    I2cExecutionReport {
        before_image_sha256,
        pages,
        final_verified: false,
        error: Some(error.into()),
    }
}

fn success_report(report: I2cExecutionReport) -> I2cHelperOutput {
    I2cHelperOutput::Success {
        result: I2cHelperResult::GuardedWrite { report },
    }
}

fn first_difference(expected: &[u8], actual: &[u8]) -> String {
    if expected.len() != actual.len() {
        return format!(
            "final image length {} != expected {}",
            actual.len(),
            expected.len()
        );
    }
    match expected
        .iter()
        .zip(actual.iter())
        .position(|(expected, actual)| expected != actual)
    {
        Some(index) => format!(
            "final image differs at byte {index:#x}: expected {:#04x}, read {:#04x}",
            expected[index], actual[index]
        ),
        None => "final image matches expected".to_owned(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(71);
    text.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn validate_transactions(transactions: &[I2cTransactionSpec]) -> Result<(), EngineError> {
    validate_i2c_transfer_transactions(transactions).map_err(EngineError::from)
}

impl From<I2cHelperRequestValidationError> for EngineError {
    fn from(error: I2cHelperRequestValidationError) -> Self {
        Self {
            code: error.code,
            message: error.message,
            transaction_index: error.transaction_index,
            message_index: error.message_index,
        }
    }
}

pub(crate) fn result_from_read_buffers(
    transaction: &I2cTransactionSpec,
    transferred_messages: u32,
    read_buffers: &[Vec<u8>],
) -> I2cTransactionResult {
    let mut read_index = 0_usize;
    let messages = transaction
        .messages
        .iter()
        .map(|message| match &message.data {
            I2cMessageData::Write { bytes } => I2cMessageResult {
                address: message.address,
                direction: I2cMessageDirection::Write,
                byte_len: u16::try_from(bytes.len()).expect("validated write length fits u16"),
                bytes: Vec::new(),
            },
            I2cMessageData::Read { byte_len } => {
                let bytes = read_buffers
                    .get(read_index)
                    .expect("read buffer count matches read message count")
                    .clone();
                read_index += 1;
                I2cMessageResult {
                    address: message.address,
                    direction: I2cMessageDirection::Read,
                    byte_len: *byte_len,
                    bytes,
                }
            }
        })
        .collect();

    I2cTransactionResult {
        bus: transaction.bus,
        transferred_messages,
        messages,
    }
}

fn failure(error: EngineError) -> I2cHelperOutput {
    I2cHelperOutput::Failure {
        failure: I2cHelperFailure {
            code: error.code.to_owned(),
            message: error.message,
            transaction_index: error.transaction_index,
            message_index: error.message_index,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_app::{
        I2C_HELPER_MAX_MESSAGE_BYTES, I2C_HELPER_MAX_TOTAL_READ_BYTES, I2cMapValidationContract,
        I2cMessageFlag, I2cMessageSpec,
    };

    #[derive(Default)]
    struct FakeBackend {
        buses: Vec<I2cBusInfo>,
        transfers: Vec<I2cTransactionSpec>,
    }

    impl I2cBackend for FakeBackend {
        fn list_buses(&self) -> Result<Vec<I2cBusInfo>, String> {
            Ok(self.buses.clone())
        }

        fn transfer(
            &mut self,
            transaction: &I2cTransactionSpec,
        ) -> Result<I2cTransactionResult, I2cTransferError> {
            self.transfers.push(transaction.clone());
            let read_buffers = transaction
                .messages
                .iter()
                .filter_map(|message| match message.data {
                    I2cMessageData::Read { byte_len } => Some(vec![0xa5; usize::from(byte_len)]),
                    I2cMessageData::Write { .. } => None,
                })
                .collect::<Vec<_>>();
            Ok(result_from_read_buffers(
                transaction,
                u32::try_from(transaction.messages.len()).unwrap(),
                &read_buffers,
            ))
        }
    }

    fn write_message(address: u16, bytes: &[u8]) -> I2cMessageSpec {
        I2cMessageSpec {
            address,
            flags: Vec::new(),
            data: I2cMessageData::Write {
                bytes: bytes.to_vec(),
            },
        }
    }

    fn read_message(address: u16, byte_len: u16) -> I2cMessageSpec {
        I2cMessageSpec {
            address,
            flags: Vec::new(),
            data: I2cMessageData::Read { byte_len },
        }
    }

    #[test]
    fn list_buses_returns_backend_inventory() {
        let mut backend = FakeBackend {
            buses: vec![I2cBusInfo {
                bus: 7,
                dev_path: "/dev/i2c-7".to_owned(),
                name: Some("mux".to_owned()),
                dev_node_exists: true,
            }],
            transfers: Vec::new(),
        };

        let output = execute(
            I2cHelperRequest {
                schema_version: I2C_HELPER_SCHEMA_VERSION,
                action: I2cHelperAction::ListBuses,
            },
            &mut backend,
        );

        assert!(matches!(
            output,
            I2cHelperOutput::Success {
                result: I2cHelperResult::BusList { ref buses }
            } if buses[0].bus == 7 && buses[0].dev_node_exists
        ));
    }

    #[test]
    fn transfer_preserves_order_and_returns_only_read_payloads() {
        let mut backend = FakeBackend::default();
        let transaction = I2cTransactionSpec {
            bus: 8,
            messages: vec![write_message(0x50, &[0x00, 0x10]), read_message(0x50, 3)],
            settle_ms: None,
        };

        let output = execute(
            I2cHelperRequest {
                schema_version: I2C_HELPER_SCHEMA_VERSION,
                action: I2cHelperAction::Transfer {
                    transactions: vec![transaction.clone()],
                },
            },
            &mut backend,
        );

        let I2cHelperOutput::Success {
            result: I2cHelperResult::Transfer { transactions },
        } = output
        else {
            panic!("transfer failed")
        };
        assert_eq!(backend.transfers, vec![transaction]);
        assert_eq!(transactions[0].transferred_messages, 2);
        assert_eq!(
            transactions[0].messages[0].direction,
            I2cMessageDirection::Write
        );
        assert!(transactions[0].messages[0].bytes.is_empty());
        assert_eq!(
            transactions[0].messages[1].direction,
            I2cMessageDirection::Read
        );
        assert_eq!(transactions[0].messages[1].bytes, vec![0xa5, 0xa5, 0xa5]);
    }

    #[test]
    fn rejects_high_address_before_backend_io() {
        let mut backend = FakeBackend::default();
        let output = execute(
            I2cHelperRequest {
                schema_version: I2C_HELPER_SCHEMA_VERSION,
                action: I2cHelperAction::Transfer {
                    transactions: vec![I2cTransactionSpec {
                        bus: 1,
                        messages: vec![read_message(0x80, 1)],
                        settle_ms: None,
                    }],
                },
            },
            &mut backend,
        );

        assert!(backend.transfers.is_empty());
        assert!(matches!(
            output,
            I2cHelperOutput::Failure {
                failure: I2cHelperFailure {
                    ref code,
                    transaction_index: Some(0),
                    message_index: Some(0),
                    ..
                }
            } if code == "invalid_message"
        ));
    }

    #[test]
    fn rejects_general_call_address_before_backend_io() {
        let mut backend = FakeBackend::default();
        let output = execute(
            I2cHelperRequest {
                schema_version: I2C_HELPER_SCHEMA_VERSION,
                action: I2cHelperAction::Transfer {
                    transactions: vec![I2cTransactionSpec {
                        bus: 1,
                        messages: vec![write_message(0x00, &[0x01])],
                        settle_ms: None,
                    }],
                },
            },
            &mut backend,
        );

        assert!(backend.transfers.is_empty());
        assert!(matches!(
            output,
            I2cHelperOutput::Failure {
                failure: I2cHelperFailure {
                    ref code,
                    ref message,
                    transaction_index: Some(0),
                    message_index: Some(0),
                    ..
                }
            } if code == "invalid_message" && message.contains("General Call")
        ));
    }

    #[test]
    fn rejects_total_read_budget_before_backend_io() {
        let mut backend = FakeBackend::default();
        let messages = (0..=(I2C_HELPER_MAX_TOTAL_READ_BYTES / I2C_HELPER_MAX_MESSAGE_BYTES))
            .map(|_| read_message(0x50, u16::try_from(I2C_HELPER_MAX_MESSAGE_BYTES).unwrap()))
            .collect::<Vec<_>>();

        let output = execute(
            I2cHelperRequest {
                schema_version: I2C_HELPER_SCHEMA_VERSION,
                action: I2cHelperAction::Transfer {
                    transactions: vec![I2cTransactionSpec {
                        bus: 1,
                        messages,
                        settle_ms: None,
                    }],
                },
            },
            &mut backend,
        );

        assert!(backend.transfers.is_empty());
        assert!(matches!(
            output,
            I2cHelperOutput::Failure {
                failure: I2cHelperFailure {
                    ref code,
                    ref message,
                    transaction_index: None,
                    message_index: None,
                }
            } if code == "invalid_request" && message.contains("total read bytes")
        ));
    }

    #[test]
    fn ten_bit_flag_extends_address_range() {
        let mut backend = FakeBackend::default();
        let mut message = read_message(0x2ff, 1);
        message.flags.push(I2cMessageFlag::TenBitAddress);

        let output = execute(
            I2cHelperRequest {
                schema_version: I2C_HELPER_SCHEMA_VERSION,
                action: I2cHelperAction::Transfer {
                    transactions: vec![I2cTransactionSpec {
                        bus: 1,
                        messages: vec![message],
                        settle_ms: None,
                    }],
                },
            },
            &mut backend,
        );

        assert!(matches!(output, I2cHelperOutput::Success { .. }));
        assert_eq!(backend.transfers.len(), 1);
    }

    /// 有状态 EEPROM fake：写事务落镜像，读事务按当前镜像返回。
    #[derive(Default)]
    struct FakeEepromBackend {
        image: Vec<u8>,
        writes: usize,
        corrupt_writes: bool,
        fail_reads: bool,
    }

    impl FakeEepromBackend {
        fn with_image(image: Vec<u8>) -> Self {
            Self {
                image,
                writes: 0,
                corrupt_writes: false,
                fail_reads: false,
            }
        }
    }

    impl I2cBackend for FakeEepromBackend {
        fn list_buses(&self) -> Result<Vec<I2cBusInfo>, String> {
            Ok(Vec::new())
        }

        fn transfer(
            &mut self,
            transaction: &I2cTransactionSpec,
        ) -> Result<I2cTransactionResult, I2cTransferError> {
            let mut pending_offset = None;
            let mut read_buffers = Vec::new();
            for message in &transaction.messages {
                match &message.data {
                    I2cMessageData::Write { bytes } if bytes.len() <= 2 => {
                        pending_offset = Some(decode_register(bytes));
                    }
                    I2cMessageData::Write { bytes } => {
                        let offset = decode_register(&bytes[..2]);
                        let mut stored = bytes[2..].to_vec();
                        if self.corrupt_writes {
                            stored.iter_mut().for_each(|byte| *byte ^= 0xff);
                        }
                        let start = usize::from(offset);
                        self.image[start..start + stored.len()].copy_from_slice(&stored);
                        self.writes += 1;
                    }
                    I2cMessageData::Read { byte_len } => {
                        if self.fail_reads {
                            return Err(I2cTransferError::transaction("read failed"));
                        }
                        let offset = pending_offset
                            .take()
                            .expect("read message follows register write");
                        let start = usize::from(offset);
                        let end = start + usize::from(*byte_len);
                        read_buffers.push(self.image[start..end].to_vec());
                    }
                }
            }
            Ok(result_from_read_buffers(
                transaction,
                u32::try_from(transaction.messages.len()).unwrap(),
                &read_buffers,
            ))
        }
    }

    fn decode_register(bytes: &[u8]) -> u16 {
        match bytes {
            [high, low] => u16::from_be_bytes([*high, *low]),
            [byte] => u16::from(*byte),
            _ => 0,
        }
    }

    fn sample_target() -> I2cTaskTarget {
        I2cTaskTarget {
            bus: 7,
            address: 0x50,
            address_width_bytes: 2,
            page_size_bytes: 8,
            write_cycle_ms: 5,
        }
    }

    fn guarded_request(
        image_bytes: u16,
        pages: Vec<I2cPageWrite>,
        verify_after_write: bool,
        expected_before_sha256: Option<String>,
        expected_final_image: Vec<u8>,
    ) -> I2cGuardedWriteRequest {
        I2cGuardedWriteRequest {
            target: sample_target(),
            read_ranges: vec![I2cReadRange {
                offset: 0,
                byte_len: image_bytes,
            }],
            validation: I2cMapValidationContract {
                image_bytes,
                fixed_bytes: Vec::new(),
                checksums: Vec::new(),
                serial_range: None,
            },
            expected_before_sha256,
            pages,
            expected_final_image,
            verify_after_write,
        }
    }

    fn pages() -> Vec<I2cPageWrite> {
        vec![
            I2cPageWrite {
                offset: 0,
                bytes: vec![1, 2, 3],
                settle_ms: 5,
            },
            I2cPageWrite {
                offset: 3,
                bytes: vec![4, 5],
                settle_ms: 5,
            },
        ]
    }

    #[test]
    fn guarded_write_writes_pages_reads_back_and_verifies_final_image() {
        let mut backend = FakeEepromBackend::with_image(vec![0; 8]);
        let before_sha = sha256_hex(&backend.image);
        let output = execute(
            I2cHelperRequest {
                schema_version: I2C_HELPER_SCHEMA_VERSION,
                action: I2cHelperAction::GuardedWrite {
                    request: guarded_request(
                        8,
                        pages(),
                        true,
                        Some(before_sha.clone()),
                        vec![1, 2, 3, 4, 5, 0, 0, 0],
                    ),
                },
            },
            &mut backend,
        );

        let I2cHelperOutput::Success {
            result: I2cHelperResult::GuardedWrite { report },
        } = output
        else {
            panic!("guarded write failed")
        };
        assert_eq!(report.before_image_sha256, before_sha);
        assert!(report.error.is_none());
        assert!(report.final_verified);
        assert_eq!(report.pages.len(), 2);
        assert!(report.pages.iter().all(|page| page.error.is_none()));
        assert_eq!(backend.writes, 2);
        assert_eq!(backend.image, vec![1, 2, 3, 4, 5, 0, 0, 0]);
    }

    #[test]
    fn guarded_write_stops_on_readback_mismatch_without_rollback() {
        let mut backend = FakeEepromBackend::with_image(vec![0; 8]);
        backend.corrupt_writes = true;
        let output = execute(
            I2cHelperRequest {
                schema_version: I2C_HELPER_SCHEMA_VERSION,
                action: I2cHelperAction::GuardedWrite {
                    request: guarded_request(8, pages(), false, None, vec![0; 8]),
                },
            },
            &mut backend,
        );

        let I2cHelperOutput::Success {
            result: I2cHelperResult::GuardedWrite { report },
        } = output
        else {
            panic!("guarded write failed")
        };
        assert!(!report.final_verified);
        assert!(report.error.is_some());
        assert_eq!(report.pages.len(), 1, "second page must never run");
        assert!(report.pages[0].error.is_some());
        assert_eq!(backend.writes, 1, "fail-stop must stop after first page");
    }

    #[test]
    fn guarded_write_refuses_stale_before_image_without_writing() {
        let mut backend = FakeEepromBackend::with_image(vec![0; 8]);
        let output = execute(
            I2cHelperRequest {
                schema_version: I2C_HELPER_SCHEMA_VERSION,
                action: I2cHelperAction::GuardedWrite {
                    request: guarded_request(
                        8,
                        pages(),
                        true,
                        Some("sha256:deadbeef".to_owned()),
                        vec![0; 8],
                    ),
                },
            },
            &mut backend,
        );

        let I2cHelperOutput::Success {
            result: I2cHelperResult::GuardedWrite { report },
        } = output
        else {
            panic!("guarded write failed")
        };
        assert!(!report.final_verified);
        assert!(report.pages.is_empty());
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|message| message.contains("digest mismatch"))
        );
        assert_eq!(
            backend.writes, 0,
            "no page may run after stale before image"
        );
    }

    #[test]
    fn guarded_write_before_read_failure_is_structured_failure() {
        let mut backend = FakeEepromBackend::with_image(vec![0; 8]);
        backend.fail_reads = true;
        let output = execute(
            I2cHelperRequest {
                schema_version: I2C_HELPER_SCHEMA_VERSION,
                action: I2cHelperAction::GuardedWrite {
                    request: guarded_request(8, pages(), true, None, vec![0; 8]),
                },
            },
            &mut backend,
        );

        assert!(matches!(
            output,
            I2cHelperOutput::Failure {
                failure: I2cHelperFailure { ref code, .. }
            } if code == "guarded_write_read_failed"
        ));
        assert_eq!(backend.writes, 0);
    }

    #[test]
    fn guarded_write_rejects_empty_pages_before_any_io() {
        let mut backend = FakeEepromBackend::with_image(vec![0; 8]);
        let output = execute(
            I2cHelperRequest {
                schema_version: I2C_HELPER_SCHEMA_VERSION,
                action: I2cHelperAction::GuardedWrite {
                    request: guarded_request(8, Vec::new(), false, None, vec![0; 8]),
                },
            },
            &mut backend,
        );

        assert!(matches!(
            output,
            I2cHelperOutput::Failure {
                failure: I2cHelperFailure {
                    ref code,
                    transaction_index: None,
                    message_index: None,
                    ..
                }
            } if code == "invalid_request"
        ));
        assert_eq!(backend.writes, 0);
    }
}
