//! 通用 I²C helper 引擎；只执行显式 message 序列，不附加 EEPROM 安全语义。

use camera_toolbox_app::{
    I2C_HELPER_SCHEMA_VERSION, I2cBusInfo, I2cHelperAction, I2cHelperFailure, I2cHelperOutput,
    I2cHelperRequest, I2cHelperRequestValidationError, I2cHelperResult, I2cMessageData,
    I2cMessageDirection, I2cMessageResult, I2cTransactionResult, I2cTransactionSpec,
    validate_i2c_transfer_transactions,
};

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
        I2C_HELPER_MAX_MESSAGE_BYTES, I2C_HELPER_MAX_TOTAL_READ_BYTES, I2cMessageFlag,
        I2cMessageSpec,
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

    #[test]
    fn unsupported_schema_is_structured_failure() {
        let mut backend = FakeBackend::default();
        let output = execute(
            I2cHelperRequest {
                schema_version: I2C_HELPER_SCHEMA_VERSION + 1,
                action: I2cHelperAction::ListBuses,
            },
            &mut backend,
        );

        assert!(matches!(
            output,
            I2cHelperOutput::Failure {
                failure: I2cHelperFailure { ref code, .. }
            } if code == "unsupported_schema"
        ));
    }
}
