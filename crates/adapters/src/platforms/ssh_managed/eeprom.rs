//! 固定 helper + JSON stdin 的 SSH EEPROM provisioning adapter。

use std::sync::Arc;

use camera_toolbox_app::{
    EEPROM_HELPER_SCHEMA_VERSION, EepromHelperAction, EepromHelperOutput, EepromHelperRequest,
    EepromHelperResult, EepromHelperTarget, EepromProvisionOperation, EepromProvisionService,
    EepromProvisionServiceError, RemoteOperationControl,
};
use camera_toolbox_core::{YG_STEREO_P24C64G_IMAGE_BYTES, YG_STEREO_P24C64G_V1_MAP_ID};

use super::connection::{
    CredentialResolver, SshConnectionTarget, SshTransportFactory, SshTransportSession,
    TransportCommandOutput,
};

/// GUI 每次 EEPROM 操作前都会把本地 companion helper 覆盖上传到该固定路径并 chmod 755。
pub const EEPROM_HELPER_PROGRAM: &str = "/usr/local/libexec/camera-toolbox-eeprom-helper";
const EEPROM_HELPER_INSTALL_PROGRAM: &str = "/bin/sh";
const EEPROM_HELPER_INSTALL_SCRIPT: &str = concat!(
    "set -u; ",
    "umask 022; ",
    "helper=/usr/local/libexec/camera-toolbox-eeprom-helper; ",
    "if mkdir -p /usr/local/libexec; then ",
    "if cat > \"$helper\"; then ",
    "chmod 755 \"$helper\"; ",
    "else status=$?; cat >/dev/null; exit \"$status\"; fi; ",
    "else status=$?; cat >/dev/null; exit \"$status\"; fi",
);
const EEPROM_HELPER_INSTALL_OUTPUT_LIMIT: usize = 4096;

pub struct SshEepromProvisionService {
    service_id: String,
    target: SshConnectionTarget,
    credential_ref: String,
    output_limit: usize,
    helper_target: EepromHelperTarget,
    helper_payload: Arc<[u8]>,
    resolver: Arc<dyn CredentialResolver>,
    transport: Arc<dyn SshTransportFactory>,
}

impl SshEepromProvisionService {
    /// 固化单个 EEPROM helper、map 和 I²C bus；operation 无法覆盖这些目标参数。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        service_id: String,
        mut target: SshConnectionTarget,
        credential_ref: String,
        output_limit_bytes: u64,
        i2c_bus: u16,
        helper_payload: Arc<[u8]>,
        resolver: Arc<dyn CredentialResolver>,
        transport: Arc<dyn SshTransportFactory>,
    ) -> Result<Self, EepromProvisionServiceError> {
        // EEPROM 复用 Explorer SFTP 的密码会话；与 SFTP 一致，不持久化或校验 SSH host key。
        target.expected_host_key = None;
        if !(1_024..=1_048_576).contains(&output_limit_bytes) {
            return Err(EepromProvisionServiceError::Protocol(
                "EEPROM helper output limit must be within 1024..=1048576".to_owned(),
            ));
        }
        let output_limit = usize::try_from(output_limit_bytes).map_err(|_| {
            EepromProvisionServiceError::Protocol(format!(
                "helper output limit {output_limit_bytes} cannot fit this host"
            ))
        })?;
        Ok(Self {
            service_id,
            target,
            credential_ref,
            output_limit,
            helper_target: EepromHelperTarget {
                map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
                i2c_bus: u32::from(i2c_bus),
            },
            helper_payload,
            resolver,
            transport,
        })
    }
}

impl EepromProvisionService for SshEepromProvisionService {
    fn service_id(&self) -> &str {
        &self.service_id
    }

    fn execute(
        &self,
        request: EepromProvisionOperation,
        control: RemoteOperationControl,
    ) -> Result<EepromHelperResult, EepromProvisionServiceError> {
        if control.cancellation.is_cancelled() {
            return Err(EepromProvisionServiceError::Transport(
                "operation cancelled".to_owned(),
            ));
        }
        if control.deadline_expired() {
            return Err(EepromProvisionServiceError::Transport(
                "operation timed out".to_owned(),
            ));
        }
        let target = self.helper_target.clone();
        validate_action(&request.action, &target)?;
        let payload = serde_json::to_vec(&EepromHelperRequest {
            schema_version: EEPROM_HELPER_SCHEMA_VERSION,
            target,
            action: request.action.clone(),
        })
        .map_err(|error| EepromProvisionServiceError::Protocol(error.to_string()))?;

        let credential = self
            .resolver
            .resolve(&self.credential_ref)
            .map_err(EepromProvisionServiceError::Transport)?;
        let mut session = self
            .transport
            .connect(&self.target, credential, &control)
            .map_err(|error| {
                EepromProvisionServiceError::Transport(format!(
                    "EEPROM SSH connection failed: {error}"
                ))
            })?;
        install_helper(&mut *session, &self.helper_payload, &control)?;
        let output = session
            .execute_argv_with_stdin(
                &[EEPROM_HELPER_PROGRAM.to_owned(), "--json-stdin".to_owned()],
                &payload,
                self.output_limit,
                &control,
            )
            .map_err(|error| {
                EepromProvisionServiceError::Transport(format!(
                    "EEPROM helper execution failed: {error}"
                ))
            })?;
        decode_output(&request.action, output)
    }
}

fn install_helper(
    session: &mut dyn SshTransportSession,
    helper_payload: &[u8],
    control: &RemoteOperationControl,
) -> Result<(), EepromProvisionServiceError> {
    let output = session
        .execute_argv_with_stdin(
            &[
                EEPROM_HELPER_INSTALL_PROGRAM.to_owned(),
                "-c".to_owned(),
                EEPROM_HELPER_INSTALL_SCRIPT.to_owned(),
            ],
            helper_payload,
            EEPROM_HELPER_INSTALL_OUTPUT_LIMIT,
            control,
        )
        .map_err(|error| {
            EepromProvisionServiceError::Transport(format!(
                "EEPROM helper upload command failed: {error}"
            ))
        })?;
    if output.exit_status == Some(0) {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(EepromProvisionServiceError::Transport(format!(
        "EEPROM helper upload failed: exit={:?}, stderr={}, stdout={}",
        output.exit_status,
        stderr.trim(),
        stdout.trim()
    )))
}

fn validate_action(
    action: &EepromHelperAction,
    target: &EepromHelperTarget,
) -> Result<(), EepromProvisionServiceError> {
    let request = match action {
        EepromHelperAction::Inspect => return Ok(()),
        EepromHelperAction::DryRun { request } | EepromHelperAction::Provision { request, .. } => {
            request
        }
    };
    request
        .validate()
        .map_err(|error| EepromProvisionServiceError::InvalidRequest(error.to_string()))?;
    if request.map_id != target.map_id {
        return Err(EepromProvisionServiceError::MapMismatch {
            request_map: request.map_id.clone(),
            configured_map: target.map_id.clone(),
        });
    }
    if let EepromHelperAction::Provision {
        expected_before_sha256,
        dry_run_token,
        ..
    } = action
    {
        if !is_sha256(expected_before_sha256) {
            return Err(EepromProvisionServiceError::InvalidRequest(
                "expected-before SHA-256 must contain 64 lowercase hex characters".to_owned(),
            ));
        }
        if !is_sha256(dry_run_token) {
            return Err(EepromProvisionServiceError::InvalidRequest(
                "dry-run token must contain 64 lowercase hex characters".to_owned(),
            ));
        }
    }
    Ok(())
}

fn decode_output(
    action: &EepromHelperAction,
    output: TransportCommandOutput,
) -> Result<EepromHelperResult, EepromProvisionServiceError> {
    if output.stdout_truncated || output.stderr_truncated {
        return Err(EepromProvisionServiceError::Protocol(
            "helper output exceeded the configured hard limit".to_owned(),
        ));
    }
    let response: EepromHelperOutput = serde_json::from_slice(&output.stdout).map_err(|error| {
        EepromProvisionServiceError::Protocol(format!(
            "invalid helper JSON response (exit {:?}): {error}",
            output.exit_status
        ))
    })?;
    match response {
        EepromHelperOutput::Failure { failure } => match output.exit_status {
            Some(status) if status != 0 => Err(EepromProvisionServiceError::Helper(failure)),
            status => Err(EepromProvisionServiceError::Protocol(format!(
                "helper returned failure with invalid exit status {status:?}"
            ))),
        },
        EepromHelperOutput::Success { result } => {
            if output.exit_status != Some(0) {
                return Err(EepromProvisionServiceError::Protocol(format!(
                    "helper returned success with exit status {:?}",
                    output.exit_status
                )));
            }
            validate_result_kind(action, &result)?;
            validate_result_payload(&result)?;
            Ok(result)
        }
    }
}

fn validate_result_kind(
    action: &EepromHelperAction,
    result: &EepromHelperResult,
) -> Result<(), EepromProvisionServiceError> {
    if matches!(
        (action, result),
        (EepromHelperAction::Inspect, EepromHelperResult::Inspect(_))
            | (
                EepromHelperAction::DryRun { .. },
                EepromHelperResult::DryRun(_)
            )
            | (
                EepromHelperAction::Provision { .. },
                EepromHelperResult::Provision(_)
            )
    ) {
        Ok(())
    } else {
        Err(EepromProvisionServiceError::Protocol(
            "helper response kind does not match the requested action".to_owned(),
        ))
    }
}

fn validate_result_payload(result: &EepromHelperResult) -> Result<(), EepromProvisionServiceError> {
    let (backup, hashes, token, verified) = match result {
        EepromHelperResult::Inspect(result) => {
            (&result.backup, vec![&result.state.image_sha256], None, true)
        }
        EepromHelperResult::DryRun(result) => (
            &result.backup,
            vec![&result.state.image_sha256],
            Some(result.dry_run_token.as_str()),
            true,
        ),
        EepromHelperResult::Provision(result) => (
            &result.backup,
            vec![&result.before.image_sha256, &result.after.image_sha256],
            None,
            result.bytewise_verified,
        ),
    };
    if backup.len() != YG_STEREO_P24C64G_IMAGE_BYTES {
        return Err(EepromProvisionServiceError::Protocol(format!(
            "helper backup has {} bytes, expected {YG_STEREO_P24C64G_IMAGE_BYTES}",
            backup.len()
        )));
    }
    if hashes.into_iter().any(|hash| !is_sha256(hash)) {
        return Err(EepromProvisionServiceError::Protocol(
            "helper returned an invalid image SHA-256".to_owned(),
        ));
    }
    if token.is_some_and(|token| !is_sha256(token)) {
        return Err(EepromProvisionServiceError::Protocol(
            "helper returned an invalid dry-run token".to_owned(),
        ));
    }
    if !verified {
        return Err(EepromProvisionServiceError::Protocol(
            "helper reported an unverified EEPROM write as success".to_owned(),
        ));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use camera_toolbox_app::{
        DumpCancellation, EepromDeviceState, EepromHelperFailure, EepromInspectResult,
        EepromRollbackState, EepromSerialState, EepromWriteResult, RemoteTimeouts,
    };
    use camera_toolbox_core::{
        EepromProvisionRequest, EepromProvisioningMode, YG_STEREO_P24C64G_V1_MAP_ID,
    };

    use super::super::connection::{SshCredential, SshTransportError};
    use super::super::memory_transport::MemorySshTransport;

    struct FailingConnectTransport;

    impl SshTransportFactory for FailingConnectTransport {
        fn connect(
            &self,
            _target: &SshConnectionTarget,
            _credential: SshCredential,
            _control: &RemoteOperationControl,
        ) -> Result<Box<dyn SshTransportSession>, SshTransportError> {
            Err(SshTransportError::TimedOut)
        }
    }

    const STALE_HOST_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4f";
    const HELPER_PAYLOAD: &[u8] = b"test-eeprom-helper-binary";

    fn device_state(hash: char) -> EepromDeviceState {
        EepromDeviceState {
            image_sha256: hash.to_string().repeat(64),
            flag_valid: false,
            serial: EepromSerialState::Empty,
        }
    }

    fn output(result: EepromHelperResult) -> TransportCommandOutput {
        TransportCommandOutput {
            stdout: serde_json::to_vec(&EepromHelperOutput::Success { result }).unwrap(),
            stderr: Vec::new(),
            exit_status: Some(0),
            stdout_truncated: false,
            stderr_truncated: false,
        }
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

    fn service(memory: &Arc<MemorySshTransport>) -> SshEepromProvisionService {
        service_with_target(memory, target(None))
    }

    fn service_with_target(
        memory: &Arc<MemorySshTransport>,
        target: SshConnectionTarget,
    ) -> SshEepromProvisionService {
        memory.allow_credential("slot:test");
        let resolver: Arc<dyn CredentialResolver> = memory.clone();
        let transport: Arc<dyn SshTransportFactory> = memory.clone();
        SshEepromProvisionService::new(
            "test-eeprom".to_owned(),
            target,
            "slot:test".to_owned(),
            4096,
            7,
            Arc::<[u8]>::from(HELPER_PAYLOAD),
            resolver,
            transport,
        )
        .unwrap()
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

    #[test]
    fn ssh_connection_failure_reports_stage_before_upload() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        memory.allow_credential("slot:test");
        let resolver: Arc<dyn CredentialResolver> = memory.clone();
        let transport: Arc<dyn SshTransportFactory> = Arc::new(FailingConnectTransport);
        let service = SshEepromProvisionService::new(
            "test-eeprom".to_owned(),
            target(None),
            "slot:test".to_owned(),
            4096,
            7,
            Arc::<[u8]>::from(HELPER_PAYLOAD),
            resolver,
            transport,
        )
        .unwrap();

        let error = service
            .execute(
                EepromProvisionOperation {
                    action: EepromHelperAction::Inspect,
                },
                control(),
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            EepromProvisionServiceError::Transport(message)
                if message == "EEPROM SSH connection failed: operation timed out"
        ));
        assert!(memory.captured_argv().is_empty());
        assert!(memory.captured_stdin().is_empty());
    }

    #[test]
    fn sends_only_fixed_helper_argv_and_typed_json_stdin() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        memory.set_command_output(output(EepromHelperResult::Inspect(EepromInspectResult {
            state: device_state('a'),
            backup: vec![0; YG_STEREO_P24C64G_IMAGE_BYTES],
        })));
        let service = service(&memory);

        let result = service
            .execute(
                EepromProvisionOperation {
                    action: EepromHelperAction::Inspect,
                },
                control(),
            )
            .unwrap();

        assert!(matches!(result, EepromHelperResult::Inspect(_)));
        assert_eq!(
            memory.captured_argv(),
            vec![
                vec![
                    EEPROM_HELPER_INSTALL_PROGRAM.to_owned(),
                    "-c".to_owned(),
                    EEPROM_HELPER_INSTALL_SCRIPT.to_owned(),
                ],
                vec![EEPROM_HELPER_PROGRAM.to_owned(), "--json-stdin".to_owned(),],
            ]
        );
        let requests = memory.captured_stdin();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], HELPER_PAYLOAD);
        let request: EepromHelperRequest = serde_json::from_slice(&requests[1]).unwrap();
        assert_eq!(request.schema_version, EEPROM_HELPER_SCHEMA_VERSION);
        assert_eq!(request.target.map_id, YG_STEREO_P24C64G_V1_MAP_ID);
        assert_eq!(request.target.i2c_bus, 7);
        assert!(matches!(request.action, EepromHelperAction::Inspect));
    }

    #[test]
    fn helper_install_script_uploads_to_fixed_path_and_drains_on_failures() {
        assert!(EEPROM_HELPER_INSTALL_SCRIPT.contains("mkdir -p /usr/local/libexec"));
        assert!(EEPROM_HELPER_INSTALL_SCRIPT.contains("cat > \"$helper\""));
        assert!(EEPROM_HELPER_INSTALL_SCRIPT.contains("chmod 755 \"$helper\""));
        assert!(!EEPROM_HELPER_INSTALL_SCRIPT.contains("/tmp"));
        assert!(!EEPROM_HELPER_INSTALL_SCRIPT.contains("mv "));
        assert_eq!(
            EEPROM_HELPER_INSTALL_SCRIPT
                .matches("cat >/dev/null")
                .count(),
            2
        );
    }

    #[test]
    fn stale_host_key_does_not_prevent_helper_invocation() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        memory.set_command_output(output(EepromHelperResult::Inspect(EepromInspectResult {
            state: device_state('a'),
            backup: vec![0; YG_STEREO_P24C64G_IMAGE_BYTES],
        })));
        let service = service_with_target(&memory, target(Some(STALE_HOST_KEY)));

        let result = service
            .execute(
                EepromProvisionOperation {
                    action: EepromHelperAction::Inspect,
                },
                control(),
            )
            .unwrap();

        assert!(matches!(result, EepromHelperResult::Inspect(_)));
        assert_eq!(memory.captured_argv().len(), 2);
        assert_eq!(memory.captured_stdin().len(), 2);
    }

    #[test]
    fn helper_upload_failure_reports_stage_and_skips_helper_invocation() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        memory.set_command_output(TransportCommandOutput {
            stdout: Vec::new(),
            stderr: b"mkdir: can't create directory '/usr/local/libexec': Permission denied"
                .to_vec(),
            exit_status: Some(1),
            stdout_truncated: false,
            stderr_truncated: false,
        });
        let service = service(&memory);

        let error = service
            .execute(
                EepromProvisionOperation {
                    action: EepromHelperAction::Inspect,
                },
                control(),
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            EepromProvisionServiceError::Transport(message)
                if message.contains("EEPROM helper upload failed")
                    && message.contains("Permission denied")
        ));
        assert_eq!(memory.captured_argv().len(), 1);
        assert_eq!(memory.captured_stdin(), vec![HELPER_PAYLOAD.to_vec()]);
    }

    #[test]
    fn rejects_truncated_or_unverified_success() {
        let mut truncated = output(EepromHelperResult::Inspect(EepromInspectResult {
            state: device_state('a'),
            backup: vec![0; YG_STEREO_P24C64G_IMAGE_BYTES],
        }));
        truncated.stdout_truncated = true;
        assert!(matches!(
            decode_output(&EepromHelperAction::Inspect, truncated),
            Err(EepromProvisionServiceError::Protocol(_))
        ));

        let request = EepromProvisionRequest {
            map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
            mode: EepromProvisioningMode::UpdateCalibration,
            serial_number: "2T02D2567K0042".to_owned(),
            overwrite_existing_serial: false,
            segments: Vec::new(),
        };
        let action = EepromHelperAction::Provision {
            request,
            expected_before_sha256: "a".repeat(64),
            dry_run_token: "b".repeat(64),
        };
        let unverified = EepromHelperResult::Provision(EepromWriteResult {
            before: device_state('a'),
            after: device_state('b'),
            backup: vec![0; YG_STEREO_P24C64G_IMAGE_BYTES],
            page_plan: Vec::new(),
            bytewise_verified: false,
            rollback: EepromRollbackState::NotRequired,
        });
        assert!(matches!(
            decode_output(&action, output(unverified)),
            Err(EepromProvisionServiceError::Protocol(_))
        ));
    }

    #[test]
    fn rejects_failure_with_success_exit_status() {
        let failure = EepromHelperOutput::Failure {
            failure: EepromHelperFailure {
                code: "write_failed".to_owned(),
                message: "simulated".to_owned(),
                before: None,
                backup: Vec::new(),
                rollback: EepromRollbackState::NotRequired,
                rollback_error: None,
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
            decode_output(&EepromHelperAction::Inspect, command),
            Err(EepromProvisionServiceError::Protocol(_))
        ));
    }

    #[test]
    fn accepts_missing_blank_or_invalid_host_key_without_transport_use() {
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        let resolver: Arc<dyn CredentialResolver> = memory.clone();
        let transport: Arc<dyn SshTransportFactory> = memory.clone();

        for expected_host_key in [None, Some("   "), Some("not-an-openssh-key")] {
            let result = SshEepromProvisionService::new(
                "test-eeprom".to_owned(),
                target(expected_host_key),
                "slot:test".to_owned(),
                4096,
                7,
                Arc::<[u8]>::from(HELPER_PAYLOAD),
                resolver.clone(),
                transport.clone(),
            );
            assert!(result.is_ok());
        }

        assert!(memory.captured_argv().is_empty());
        assert!(memory.captured_stdin().is_empty());
    }
}
