//! 固定 helper + JSON stdin 的 SSH 通用 I²C adapter。

use std::{io, sync::Arc};

use camera_toolbox_app::{
    I2C_HELPER_MAX_REQUEST_BYTES, I2C_HELPER_SCHEMA_VERSION, I2cHelperAction, I2cHelperOperation,
    I2cHelperOutput, I2cHelperRequest, I2cHelperResult, I2cHelperService, I2cHelperServiceError,
    I2cMessageData, I2cMessageDirection, I2cTransactionSpec, RemoteOperationControl,
    validate_i2c_helper_action,
};

use super::{
    connection::{
        CredentialResolver, SshConnectionTarget, SshTransportFactory, TransportCommandOutput,
    },
    helper,
};
const SUCCESS_JSON_BASE_BYTES: usize = 128;
const SUCCESS_JSON_TRANSACTION_BYTES: usize = 128;
const SUCCESS_JSON_MESSAGE_BYTES: usize = 128;
const SUCCESS_JSON_BYTE_ELEMENT_BYTES: usize = 4;

pub struct SshI2cHelperService {
    service_id: String,
    target: SshConnectionTarget,
    credential_ref: String,
    output_limit: usize,
    helper_payload: Arc<[u8]>,
    resolver: Arc<dyn CredentialResolver>,
    transport: Arc<dyn SshTransportFactory>,
}

impl SshI2cHelperService {
    /// 构造通用 I²C helper service；会话复用密码 SSH，当前不保存也不校验 host key。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        service_id: String,
        mut target: SshConnectionTarget,
        credential_ref: String,
        output_limit_bytes: u64,
        helper_payload: Arc<[u8]>,
        resolver: Arc<dyn CredentialResolver>,
        transport: Arc<dyn SshTransportFactory>,
    ) -> Result<Self, I2cHelperServiceError> {
        // 与现有 EEPROM helper 路径保持一致：物理写入场景必须由可信网络/IP 上下文约束风险。
        target.expected_host_key = None;
        if !(1_024..=1_048_576).contains(&output_limit_bytes) {
            return Err(I2cHelperServiceError::Protocol(
                "I2C helper output limit must be within 1024..=1048576".to_owned(),
            ));
        }
        let output_limit = usize::try_from(output_limit_bytes).map_err(|_| {
            I2cHelperServiceError::Protocol(format!(
                "helper output limit {output_limit_bytes} cannot fit this host"
            ))
        })?;
        Ok(Self {
            service_id,
            target,
            credential_ref,
            output_limit,
            helper_payload,
            resolver,
            transport,
        })
    }
}

impl I2cHelperService for SshI2cHelperService {
    fn service_id(&self) -> &str {
        &self.service_id
    }

    fn execute(
        &self,
        request: I2cHelperOperation,
        control: RemoteOperationControl,
    ) -> Result<I2cHelperResult, I2cHelperServiceError> {
        if control.cancellation.is_cancelled() {
            return Err(I2cHelperServiceError::Transport(
                "operation cancelled".to_owned(),
            ));
        }
        if control.deadline_expired() {
            return Err(I2cHelperServiceError::Transport(
                "operation timed out".to_owned(),
            ));
        }
        let helper_request = I2cHelperRequest {
            schema_version: I2C_HELPER_SCHEMA_VERSION,
            action: request.action,
        };
        validate_action(&helper_request.action)?;
        let payload = serialize_request_bounded(&helper_request)?;
        validate_transport_budgets(&helper_request.action, payload.len(), self.output_limit)?;

        let credential = self
            .resolver
            .resolve(&self.credential_ref)
            .map_err(I2cHelperServiceError::Transport)?;
        let mut session = self
            .transport
            .connect(&self.target, credential, &control)
            .map_err(|error| {
                I2cHelperServiceError::Transport(format!("I2C SSH connection failed: {error}"))
            })?;
        helper::install_helper(&mut *session, &self.helper_payload, &control, "I2C")
            .map_err(I2cHelperServiceError::Transport)?;
        let output = session
            .execute_argv_with_stdin(
                &[helper::HELPER_PROGRAM.to_owned(), "--json-stdin".to_owned()],
                &payload,
                self.output_limit,
                &control,
            )
            .map_err(|error| {
                I2cHelperServiceError::Transport(format!("I2C helper execution failed: {error}"))
            })?;
        decode_output(&helper_request.action, output)
    }
}

fn validate_action(action: &I2cHelperAction) -> Result<(), I2cHelperServiceError> {
    validate_i2c_helper_action(action).map_err(|error| {
        I2cHelperServiceError::InvalidRequest(
            match (error.transaction_index, error.message_index) {
                (Some(transaction_index), Some(message_index)) => format!(
                    "transaction {transaction_index} message {message_index}: {}",
                    error.message
                ),
                (Some(transaction_index), None) => {
                    format!("transaction {transaction_index}: {}", error.message)
                }
                (None, _) => error.message,
            },
        )
    })
}

fn serialize_request_bounded(request: &I2cHelperRequest) -> Result<Vec<u8>, I2cHelperServiceError> {
    let mut writer = BoundedJsonWriter::new(I2C_HELPER_MAX_REQUEST_BYTES);
    match serde_json::to_writer(&mut writer, request) {
        Ok(()) => {}
        Err(_) if writer.truncated => return Err(serialized_request_too_large(writer.len())),
        Err(error) => return Err(I2cHelperServiceError::Protocol(error.to_string())),
    }
    let bytes = writer.into_bytes();
    if bytes.len() > I2C_HELPER_MAX_REQUEST_BYTES {
        return Err(serialized_request_too_large(bytes.len()));
    }
    Ok(bytes)
}

fn serialized_request_too_large(observed_len: usize) -> I2cHelperServiceError {
    I2cHelperServiceError::InvalidRequest(format!(
        "serialized I2C helper request is at least {observed_len} bytes; limit is {I2C_HELPER_MAX_REQUEST_BYTES}"
    ))
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    max_success_bytes: usize,
    attempted_bytes: usize,
    truncated: bool,
}

impl BoundedJsonWriter {
    fn new(max_success_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_success_bytes,
            attempted_bytes: 0,
            truncated: false,
        }
    }

    fn len(&self) -> usize {
        self.attempted_bytes
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl io::Write for BoundedJsonWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.truncated {
            return Err(io::Error::other(
                "serialized I2C helper request exceeded hard limit",
            ));
        }

        self.attempted_bytes = self.attempted_bytes.saturating_add(buf.len());
        if self.attempted_bytes > self.max_success_bytes {
            self.truncated = true;
            return Err(io::Error::other(
                "serialized I2C helper request exceeded hard limit",
            ));
        }

        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn validate_transport_budgets(
    action: &I2cHelperAction,
    request_len: usize,
    output_limit: usize,
) -> Result<(), I2cHelperServiceError> {
    if request_len > I2C_HELPER_MAX_REQUEST_BYTES {
        return Err(I2cHelperServiceError::InvalidRequest(format!(
            "serialized I2C helper request is {request_len} bytes; limit is {I2C_HELPER_MAX_REQUEST_BYTES}"
        )));
    }

    if let Some(success_budget) = conservative_transfer_success_json_bytes(action)? {
        if success_budget > output_limit {
            return Err(I2cHelperServiceError::InvalidRequest(format!(
                "conservative Transfer success JSON budget is {success_budget} bytes; output limit is {output_limit}"
            )));
        }
    }

    Ok(())
}

fn conservative_transfer_success_json_bytes(
    action: &I2cHelperAction,
) -> Result<Option<usize>, I2cHelperServiceError> {
    let I2cHelperAction::Transfer { transactions } = action else {
        return Ok(None);
    };
    let mut budget = SUCCESS_JSON_BASE_BYTES;
    for transaction in transactions {
        budget = checked_budget_add(budget, SUCCESS_JSON_TRANSACTION_BYTES)?;
        for message in &transaction.messages {
            budget = checked_budget_add(budget, SUCCESS_JSON_MESSAGE_BYTES)?;
            if let I2cMessageData::Read { byte_len } = &message.data {
                let byte_array_budget =
                    checked_budget_mul(usize::from(*byte_len), SUCCESS_JSON_BYTE_ELEMENT_BYTES)?;
                budget = checked_budget_add(budget, byte_array_budget)?;
            }
        }
    }
    Ok(Some(budget))
}

fn checked_budget_add(lhs: usize, rhs: usize) -> Result<usize, I2cHelperServiceError> {
    lhs.checked_add(rhs).ok_or_else(|| {
        I2cHelperServiceError::InvalidRequest("I2C helper transport budget overflow".to_owned())
    })
}

fn checked_budget_mul(lhs: usize, rhs: usize) -> Result<usize, I2cHelperServiceError> {
    lhs.checked_mul(rhs).ok_or_else(|| {
        I2cHelperServiceError::InvalidRequest("I2C helper transport budget overflow".to_owned())
    })
}

fn decode_output(
    action: &I2cHelperAction,
    output: TransportCommandOutput,
) -> Result<I2cHelperResult, I2cHelperServiceError> {
    if output.stdout_truncated || output.stderr_truncated {
        return Err(I2cHelperServiceError::Protocol(
            "helper output exceeded the configured hard limit".to_owned(),
        ));
    }
    let response: I2cHelperOutput = serde_json::from_slice(&output.stdout).map_err(|error| {
        I2cHelperServiceError::Protocol(format!(
            "invalid helper JSON response (exit {:?}): {error}",
            output.exit_status
        ))
    })?;
    match response {
        I2cHelperOutput::Failure { failure } => match output.exit_status {
            Some(status) if status != 0 => Err(I2cHelperServiceError::Helper(failure)),
            status => Err(I2cHelperServiceError::Protocol(format!(
                "helper returned failure with invalid exit status {status:?}"
            ))),
        },
        I2cHelperOutput::Success { result } => {
            if output.exit_status != Some(0) {
                return Err(I2cHelperServiceError::Protocol(format!(
                    "helper returned success with exit status {:?}",
                    output.exit_status
                )));
            }
            validate_result_kind(action, &result)?;
            validate_result_payload(action, &result)?;
            Ok(result)
        }
    }
}

fn validate_result_kind(
    action: &I2cHelperAction,
    result: &I2cHelperResult,
) -> Result<(), I2cHelperServiceError> {
    if matches!(
        (action, result),
        (I2cHelperAction::ListBuses, I2cHelperResult::BusList { .. })
            | (
                I2cHelperAction::Transfer { .. },
                I2cHelperResult::Transfer { .. }
            )
    ) {
        Ok(())
    } else {
        Err(I2cHelperServiceError::Protocol(
            "helper response kind does not match the requested action".to_owned(),
        ))
    }
}

fn validate_result_payload(
    action: &I2cHelperAction,
    result: &I2cHelperResult,
) -> Result<(), I2cHelperServiceError> {
    match (action, result) {
        (I2cHelperAction::ListBuses, I2cHelperResult::BusList { buses }) => {
            for bus in buses {
                if bus.dev_path.trim().is_empty() {
                    return Err(I2cHelperServiceError::Protocol(
                        "helper returned an empty I2C dev path".to_owned(),
                    ));
                }
            }
            Ok(())
        }
        (
            I2cHelperAction::Transfer { transactions },
            I2cHelperResult::Transfer {
                transactions: results,
            },
        ) => validate_transfer_result(transactions, results),
        _ => Ok(()),
    }
}

fn validate_transfer_result(
    requests: &[I2cTransactionSpec],
    results: &[camera_toolbox_app::I2cTransactionResult],
) -> Result<(), I2cHelperServiceError> {
    if requests.len() != results.len() {
        return Err(I2cHelperServiceError::Protocol(format!(
            "helper returned {} transaction results for {} requests",
            results.len(),
            requests.len()
        )));
    }

    for (transaction_index, (request, result)) in requests.iter().zip(results).enumerate() {
        if request.bus != result.bus {
            return Err(I2cHelperServiceError::Protocol(format!(
                "helper transaction {transaction_index} bus mismatch: requested {}, got {}",
                request.bus, result.bus
            )));
        }
        if usize::try_from(result.transferred_messages).ok() != Some(request.messages.len()) {
            return Err(I2cHelperServiceError::Protocol(format!(
                "helper transaction {transaction_index} transferred {} messages for {} requested messages",
                result.transferred_messages,
                request.messages.len()
            )));
        }
        if request.messages.len() != result.messages.len() {
            return Err(I2cHelperServiceError::Protocol(format!(
                "helper transaction {transaction_index} returned {} message results for {} requests",
                result.messages.len(),
                request.messages.len()
            )));
        }
        for (message_index, (request_message, result_message)) in
            request.messages.iter().zip(&result.messages).enumerate()
        {
            if request_message.address != result_message.address {
                return Err(I2cHelperServiceError::Protocol(format!(
                    "helper transaction {transaction_index} message {message_index} address mismatch: requested 0x{:x}, got 0x{:x}",
                    request_message.address, result_message.address
                )));
            }
            let (expected_direction, expected_len) = match &request_message.data {
                I2cMessageData::Write { bytes } => (I2cMessageDirection::Write, bytes.len()),
                I2cMessageData::Read { byte_len } => {
                    (I2cMessageDirection::Read, usize::from(*byte_len))
                }
            };
            if result_message.direction != expected_direction {
                return Err(I2cHelperServiceError::Protocol(format!(
                    "helper transaction {transaction_index} message {message_index} direction mismatch"
                )));
            }
            if usize::from(result_message.byte_len) != expected_len {
                return Err(I2cHelperServiceError::Protocol(format!(
                    "helper transaction {transaction_index} message {message_index} byte length mismatch: expected {expected_len}, got {}",
                    result_message.byte_len
                )));
            }
            match &request_message.data {
                I2cMessageData::Write { .. } => {
                    if !result_message.bytes.is_empty() {
                        return Err(I2cHelperServiceError::Protocol(format!(
                            "helper transaction {transaction_index} message {message_index} returned bytes for a write message"
                        )));
                    }
                }
                I2cMessageData::Read { .. } => {
                    if result_message.bytes.len() != expected_len {
                        return Err(I2cHelperServiceError::Protocol(format!(
                            "helper transaction {transaction_index} message {message_index} returned {} read bytes for {expected_len} requested bytes",
                            result_message.bytes.len()
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use camera_toolbox_app::{
        DumpCancellation, I2C_HELPER_MAX_MESSAGE_BYTES, I2C_HELPER_MAX_MESSAGES_PER_TRANSACTION,
        I2C_HELPER_MAX_TRANSACTIONS_PER_REQUEST, I2cBusInfo, I2cHelperFailure, I2cHelperRequest,
        I2cMessageFlag, I2cMessageResult, RemoteTimeouts,
    };

    use super::super::connection::SshCredential;
    use super::super::memory_transport::MemorySshTransport;
    use super::*;

    const HELPER_PAYLOAD: &[u8] = b"test-i2c-helper-binary";
    const STALE_HOST_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4f";

    struct FailingConnectTransport;

    impl SshTransportFactory for FailingConnectTransport {
        fn connect(
            &self,
            _target: &SshConnectionTarget,
            _credential: SshCredential,
            _control: &RemoteOperationControl,
        ) -> Result<
            Box<dyn super::super::connection::SshTransportSession>,
            super::super::connection::SshTransportError,
        > {
            Err(super::super::connection::SshTransportError::TimedOut)
        }
    }

    fn control() -> RemoteOperationControl {
        RemoteOperationControl::new(
            RemoteTimeouts {
                connect: Duration::from_secs(1),
                idle: Duration::from_secs(1),
                overall: Duration::from_secs(5),
            },
            DumpCancellation::default(),
        )
        .unwrap()
    }

    fn target(expected_host_key: Option<&str>) -> SshConnectionTarget {
        SshConnectionTarget {
            host: "camera.local".to_owned(),
            port: 22,
            username: "root".to_owned(),
            expected_host_key: expected_host_key.map(str::to_owned),
            command_subsystem: None,
            remote_event_subsystem: None,
        }
    }

    fn service(memory: &Arc<MemorySshTransport>) -> SshI2cHelperService {
        service_with_target(memory, target(None))
    }

    fn service_with_target(
        memory: &Arc<MemorySshTransport>,
        target: SshConnectionTarget,
    ) -> SshI2cHelperService {
        service_with_target_and_output_limit(memory, target, 4096)
    }

    fn service_with_target_and_output_limit(
        memory: &Arc<MemorySshTransport>,
        target: SshConnectionTarget,
        output_limit: u64,
    ) -> SshI2cHelperService {
        memory.allow_credential("slot:test");
        let resolver: Arc<dyn CredentialResolver> = memory.clone();
        let transport: Arc<dyn SshTransportFactory> = memory.clone();
        SshI2cHelperService::new(
            "test-i2c".to_owned(),
            target,
            "slot:test".to_owned(),
            output_limit,
            Arc::<[u8]>::from(HELPER_PAYLOAD),
            resolver,
            transport,
        )
        .unwrap()
    }

    fn output(result: I2cHelperResult) -> TransportCommandOutput {
        TransportCommandOutput {
            stdout: serde_json::to_vec(&I2cHelperOutput::Success { result }).unwrap(),
            stderr: Vec::new(),
            exit_status: Some(0),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    fn transfer_action() -> I2cHelperAction {
        I2cHelperAction::Transfer {
            transactions: vec![I2cTransactionSpec {
                bus: 8,
                messages: vec![
                    camera_toolbox_app::I2cMessageSpec {
                        address: 0x50,
                        flags: Vec::new(),
                        data: I2cMessageData::Write {
                            bytes: vec![0x00, 0x10],
                        },
                    },
                    camera_toolbox_app::I2cMessageSpec {
                        address: 0x50,
                        flags: vec![I2cMessageFlag::IgnoreNack],
                        data: I2cMessageData::Read { byte_len: 2 },
                    },
                ],
                settle_ms: None,
            }],
        }
    }

    fn transfer_result() -> I2cHelperResult {
        I2cHelperResult::Transfer {
            transactions: vec![camera_toolbox_app::I2cTransactionResult {
                bus: 8,
                transferred_messages: 2,
                messages: vec![
                    I2cMessageResult {
                        address: 0x50,
                        direction: I2cMessageDirection::Write,
                        byte_len: 2,
                        bytes: Vec::new(),
                    },
                    I2cMessageResult {
                        address: 0x50,
                        direction: I2cMessageDirection::Read,
                        byte_len: 2,
                        bytes: vec![0xaa, 0x55],
                    },
                ],
            }],
        }
    }

    #[test]
    fn bounded_writer_rejects_overflow_without_partial_write() {
        let mut exact = BoundedJsonWriter::new(3);
        assert_eq!(std::io::Write::write(&mut exact, b"abc").unwrap(), 3);
        assert_eq!(exact.len(), 3);
        assert_eq!(exact.into_bytes(), b"abc");

        let mut overflowing = BoundedJsonWriter::new(3);
        assert_eq!(std::io::Write::write(&mut overflowing, b"ab").unwrap(), 2);
        let error = std::io::Write::write(&mut overflowing, b"cd").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(overflowing.truncated);
        assert_eq!(overflowing.len(), 4);
        assert_eq!(overflowing.bytes, b"ab");
        assert_eq!(
            std::io::Write::write(&mut overflowing, b"e")
                .unwrap_err()
                .kind(),
            io::ErrorKind::Other
        );
        assert_eq!(overflowing.bytes, b"ab");
    }

    #[test]
    fn rejects_write_payload_over_helper_limit_before_serialization_and_ssh_session() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        let service = service_with_target_and_output_limit(&memory, target(None), 1_048_576);

        let error = service
            .execute(
                I2cHelperOperation {
                    action: I2cHelperAction::Transfer {
                        transactions: vec![I2cTransactionSpec {
                            bus: 8,
                            messages: vec![camera_toolbox_app::I2cMessageSpec {
                                address: 0x50,
                                flags: Vec::new(),
                                data: I2cMessageData::Write {
                                    bytes: vec![0x5a; I2C_HELPER_MAX_MESSAGE_BYTES + 1],
                                },
                            }],
                            settle_ms: None,
                        }],
                    },
                },
                control(),
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            I2cHelperServiceError::InvalidRequest(message)
                if message.contains("write payload")
                    && message.contains(&I2C_HELPER_MAX_MESSAGE_BYTES.to_string())
        ));
        assert!(memory.captured_argv().is_empty());
        assert!(memory.captured_stdin().is_empty());
    }

    #[test]
    fn rejects_transaction_count_over_helper_limit_before_ssh_session() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        let service = service_with_target_and_output_limit(&memory, target(None), 1_048_576);
        let transactions = (0..=I2C_HELPER_MAX_TRANSACTIONS_PER_REQUEST)
            .map(|bus| I2cTransactionSpec {
                bus: u32::try_from(bus).unwrap(),
                messages: vec![camera_toolbox_app::I2cMessageSpec {
                    address: 0x50,
                    flags: Vec::new(),
                    data: I2cMessageData::Write { bytes: vec![0x01] },
                }],
                settle_ms: None,
            })
            .collect::<Vec<_>>();

        let error = service
            .execute(
                I2cHelperOperation {
                    action: I2cHelperAction::Transfer { transactions },
                },
                control(),
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            I2cHelperServiceError::InvalidRequest(message)
                if message.contains("transactions")
                    && message.contains(&I2C_HELPER_MAX_TRANSACTIONS_PER_REQUEST.to_string())
        ));
        assert!(memory.captured_argv().is_empty());
        assert!(memory.captured_stdin().is_empty());
    }

    #[test]
    fn rejects_message_count_over_helper_limit_before_ssh_session() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        let service = service_with_target_and_output_limit(&memory, target(None), 1_048_576);
        let messages = (0..=I2C_HELPER_MAX_MESSAGES_PER_TRANSACTION)
            .map(|_| camera_toolbox_app::I2cMessageSpec {
                address: 0x50,
                flags: Vec::new(),
                data: I2cMessageData::Write { bytes: vec![0x01] },
            })
            .collect::<Vec<_>>();

        let error = service
            .execute(
                I2cHelperOperation {
                    action: I2cHelperAction::Transfer {
                        transactions: vec![I2cTransactionSpec {
                            bus: 8,
                            messages,
                            settle_ms: None,
                        }],
                    },
                },
                control(),
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            I2cHelperServiceError::InvalidRequest(message)
                if message.contains("messages per transaction")
                    && message.contains(&I2C_HELPER_MAX_MESSAGES_PER_TRANSACTION.to_string())
        ));
        assert!(memory.captured_argv().is_empty());
        assert!(memory.captured_stdin().is_empty());
    }

    #[test]
    fn rejects_request_over_stdin_budget_before_ssh_session() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        let service = service_with_target_and_output_limit(&memory, target(None), 1_048_576);
        let messages = (0..42)
            .map(|_| camera_toolbox_app::I2cMessageSpec {
                address: 0x50,
                flags: Vec::new(),
                data: I2cMessageData::Write {
                    bytes: vec![0x5a; 8192],
                },
            })
            .collect::<Vec<_>>();
        let transactions = (0..4)
            .map(|bus| I2cTransactionSpec {
                bus,
                messages: messages.clone(),
                settle_ms: None,
            })
            .collect::<Vec<_>>();

        let error = service
            .execute(
                I2cHelperOperation {
                    action: I2cHelperAction::Transfer { transactions },
                },
                control(),
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            I2cHelperServiceError::InvalidRequest(message)
                if message.contains("serialized I2C helper request")
                    && message.contains(&I2C_HELPER_MAX_REQUEST_BYTES.to_string())
        ));
        assert!(memory.captured_argv().is_empty());
        assert!(memory.captured_stdin().is_empty());
    }

    #[test]
    fn rejects_success_output_budget_before_ssh_session() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        let service = service_with_target_and_output_limit(&memory, target(None), 1024);

        let error = service
            .execute(
                I2cHelperOperation {
                    action: I2cHelperAction::Transfer {
                        transactions: vec![I2cTransactionSpec {
                            bus: 8,
                            messages: vec![camera_toolbox_app::I2cMessageSpec {
                                address: 0x50,
                                flags: Vec::new(),
                                data: I2cMessageData::Read { byte_len: 256 },
                            }],
                            settle_ms: None,
                        }],
                    },
                },
                control(),
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            I2cHelperServiceError::InvalidRequest(message)
                if message.contains("conservative Transfer success JSON budget")
        ));
        assert!(memory.captured_argv().is_empty());
        assert!(memory.captured_stdin().is_empty());
    }

    #[test]
    fn list_buses_has_no_static_success_json_budget() {
        assert!(matches!(
            conservative_transfer_success_json_bytes(&I2cHelperAction::ListBuses),
            Ok(None)
        ));
        validate_transport_budgets(&I2cHelperAction::ListBuses, 64, 1024).unwrap();
    }

    #[test]
    fn sends_fixed_helper_argv_and_i2c_json_stdin() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        memory.set_command_output(output(I2cHelperResult::BusList {
            buses: vec![I2cBusInfo {
                bus: 8,
                dev_path: "/dev/i2c-8".to_owned(),
                name: Some("muxed".to_owned()),
                dev_node_exists: true,
            }],
        }));
        let service = service(&memory);

        let result = service
            .execute(
                I2cHelperOperation {
                    action: I2cHelperAction::ListBuses,
                },
                control(),
            )
            .unwrap();

        assert!(matches!(result, I2cHelperResult::BusList { .. }));
        assert_eq!(
            memory.captured_argv(),
            vec![
                vec![
                    helper::HELPER_INSTALL_PROGRAM.to_owned(),
                    "755".to_owned(),
                    helper::HELPER_PROGRAM.to_owned(),
                ],
                vec![helper::HELPER_PROGRAM.to_owned(), "--json-stdin".to_owned()],
            ]
        );
        assert_eq!(
            memory.file_bytes(helper::HELPER_PROGRAM),
            Some(HELPER_PAYLOAD.to_vec())
        );
        let requests = memory.captured_stdin();
        assert_eq!(requests.len(), 1);
        let request: I2cHelperRequest = serde_json::from_slice(&requests[0]).unwrap();
        assert_eq!(request.schema_version, I2C_HELPER_SCHEMA_VERSION);
        assert!(matches!(request.action, I2cHelperAction::ListBuses));
    }

    #[test]
    fn stale_host_key_does_not_prevent_i2c_helper_invocation() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        memory.set_command_output(output(transfer_result()));
        let service = service_with_target(&memory, target(Some(STALE_HOST_KEY)));

        let result = service
            .execute(
                I2cHelperOperation {
                    action: transfer_action(),
                },
                control(),
            )
            .unwrap();

        assert!(matches!(result, I2cHelperResult::Transfer { .. }));
        assert_eq!(memory.captured_argv().len(), 2);
        assert_eq!(memory.captured_stdin().len(), 1);
    }

    #[test]
    fn rejects_transfer_response_that_does_not_match_request() {
        let action = transfer_action();
        let bad = I2cHelperResult::Transfer {
            transactions: vec![camera_toolbox_app::I2cTransactionResult {
                bus: 8,
                transferred_messages: 2,
                messages: vec![
                    I2cMessageResult {
                        address: 0x50,
                        direction: I2cMessageDirection::Write,
                        byte_len: 2,
                        bytes: Vec::new(),
                    },
                    I2cMessageResult {
                        address: 0x50,
                        direction: I2cMessageDirection::Read,
                        byte_len: 2,
                        bytes: vec![0xaa],
                    },
                ],
            }],
        };

        assert!(matches!(
            decode_output(&action, output(bad)),
            Err(I2cHelperServiceError::Protocol(_))
        ));
    }

    #[test]
    fn rejects_helper_failure_with_success_exit_status() {
        let failure = I2cHelperOutput::Failure {
            failure: I2cHelperFailure {
                code: "transfer_failed".to_owned(),
                message: "simulated".to_owned(),
                transaction_index: Some(0),
                message_index: None,
            },
        };
        let command = TransportCommandOutput {
            stdout: serde_json::to_vec(&failure).unwrap(),
            stderr: Vec::new(),
            exit_status: Some(0),
            stdout_truncated: false,
            stderr_truncated: false,
        };

        assert!(matches!(
            decode_output(&transfer_action(), command),
            Err(I2cHelperServiceError::Protocol(_))
        ));
    }

    #[test]
    fn ssh_connection_failure_reports_stage_before_upload() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        memory.allow_credential("slot:test");
        let resolver: Arc<dyn CredentialResolver> = memory.clone();
        let transport: Arc<dyn SshTransportFactory> = Arc::new(FailingConnectTransport);
        let service = SshI2cHelperService::new(
            "test-i2c".to_owned(),
            target(None),
            "slot:test".to_owned(),
            4096,
            Arc::<[u8]>::from(HELPER_PAYLOAD),
            resolver,
            transport,
        )
        .unwrap();

        let error = service
            .execute(
                I2cHelperOperation {
                    action: I2cHelperAction::ListBuses,
                },
                control(),
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            I2cHelperServiceError::Transport(message)
                if message == "I2C SSH connection failed: operation timed out"
        ));
        assert!(memory.captured_argv().is_empty());
        assert!(memory.captured_stdin().is_empty());
    }
}
