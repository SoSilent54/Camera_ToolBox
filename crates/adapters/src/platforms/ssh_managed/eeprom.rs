//! 固定 helper + JSON stdin 的 SSH EEPROM provisioning adapter。

use std::sync::Arc;

use camera_toolbox_app::{
    EEPROM_EXPERIMENTAL_PROVISION_WARNING, EEPROM_HELPER_SCHEMA_VERSION, EepromHelperAction,
    EepromHelperOutput, EepromHelperRequest, EepromHelperResult, EepromHelperTarget,
    EepromProvisionOperation, EepromProvisionService, EepromProvisionServiceError,
    RemoteOperationControl,
};
use camera_toolbox_core::{YG_STEREO_P24C64G_IMAGE_BYTES, YG_STEREO_P24C64G_V1_MAP_ID};

use super::{
    ServerHostKey,
    connection::{
        CredentialResolver, SshConnectionTarget, SshTransportFactory, TransportCommandOutput,
    },
};

pub struct SshEepromProvisionService {
    service_id: String,
    target: SshConnectionTarget,
    credential_ref: String,
    helper_program: String,
    output_limit: usize,
    helper_target: EepromHelperTarget,
    resolver: Arc<dyn CredentialResolver>,
    transport: Arc<dyn SshTransportFactory>,
}

impl SshEepromProvisionService {
    /// 固化单个 EEPROM helper、map 和 I²C bus；operation 无法覆盖这些目标参数。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        service_id: String,
        target: SshConnectionTarget,
        credential_ref: String,
        helper_program: String,
        output_limit_bytes: u64,
        i2c_bus: u16,
        resolver: Arc<dyn CredentialResolver>,
        transport: Arc<dyn SshTransportFactory>,
    ) -> Result<Self, EepromProvisionServiceError> {
        let expected_host_key = target
            .expected_host_key
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                EepromProvisionServiceError::Protocol(
                    "EEPROM SSH target requires a pinned server host key".to_owned(),
                )
            })?;
        ServerHostKey::from_openssh(expected_host_key).map_err(|error| {
            EepromProvisionServiceError::Protocol(format!(
                "EEPROM SSH target host key is invalid: {error}"
            ))
        })?;
        if !is_normalized_absolute_path(&helper_program) {
            return Err(EepromProvisionServiceError::Protocol(
                "EEPROM helper program must be a normalized absolute path".to_owned(),
            ));
        }
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
            helper_program,
            output_limit,
            helper_target: EepromHelperTarget {
                map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
                i2c_bus: u32::from(i2c_bus),
            },
            resolver,
            transport,
        })
    }
}

fn is_normalized_absolute_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() > 1
        && !path.ends_with('/')
        && !path.contains("//")
        && !path.contains(char::is_control)
        && !path.split('/').any(|part| matches!(part, "." | ".."))
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
        if matches!(&request.action, EepromHelperAction::Provision { .. })
            && !request.experimental_disconnect_risk_acknowledged
        {
            return Err(EepromProvisionServiceError::Protocol(format!(
                "explicit experimental disconnect-risk acknowledgement is required: {EEPROM_EXPERIMENTAL_PROVISION_WARNING}"
            )));
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
            .map_err(|error| EepromProvisionServiceError::Transport(error.to_string()))?;
        let output = session
            .execute_argv_with_stdin(
                &[self.helper_program.clone(), "--json-stdin".to_owned()],
                &payload,
                self.output_limit,
                &control,
            )
            .map_err(|error| EepromProvisionServiceError::Transport(error.to_string()))?;
        decode_output(&request.action, output)
    }
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

    use super::super::memory_transport::MemorySshTransport;

    const PINNED_HOST_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4f";

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

    fn service(
        memory: &Arc<MemorySshTransport>,
        expected_host_key: &str,
    ) -> SshEepromProvisionService {
        memory.allow_credential("slot:test");
        let resolver: Arc<dyn CredentialResolver> = memory.clone();
        let transport: Arc<dyn SshTransportFactory> = memory.clone();
        SshEepromProvisionService::new(
            "test-eeprom".to_owned(),
            target(Some(expected_host_key)),
            "slot:test".to_owned(),
            "/usr/local/libexec/camera-toolbox-eeprom-helper".to_owned(),
            4096,
            7,
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
    fn sends_only_fixed_helper_argv_and_typed_json_stdin() {
        let memory = Arc::new(MemorySshTransport::new(PINNED_HOST_KEY));
        memory.set_command_output(output(EepromHelperResult::Inspect(EepromInspectResult {
            state: device_state('a'),
            backup: vec![0; YG_STEREO_P24C64G_IMAGE_BYTES],
        })));
        let service = service(&memory, PINNED_HOST_KEY);

        let result = service
            .execute(
                EepromProvisionOperation {
                    action: EepromHelperAction::Inspect,
                    experimental_disconnect_risk_acknowledged: false,
                },
                control(),
            )
            .unwrap();

        assert!(matches!(result, EepromHelperResult::Inspect(_)));
        assert_eq!(
            memory.captured_argv(),
            vec![vec![
                "/usr/local/libexec/camera-toolbox-eeprom-helper".to_owned(),
                "--json-stdin".to_owned(),
            ]]
        );
        let requests = memory.captured_stdin();
        assert_eq!(requests.len(), 1);
        let request: EepromHelperRequest = serde_json::from_slice(&requests[0]).unwrap();
        assert_eq!(request.schema_version, EEPROM_HELPER_SCHEMA_VERSION);
        assert_eq!(request.target.map_id, YG_STEREO_P24C64G_V1_MAP_ID);
        assert_eq!(request.target.i2c_bus, 7);
        assert!(matches!(request.action, EepromHelperAction::Inspect));
    }

    #[test]
    fn host_key_mismatch_prevents_helper_invocation() {
        let memory = Arc::new(MemorySshTransport::new("different-key"));
        let service = service(&memory, PINNED_HOST_KEY);

        let error = service
            .execute(
                EepromProvisionOperation {
                    action: EepromHelperAction::Inspect,
                    experimental_disconnect_risk_acknowledged: false,
                },
                control(),
            )
            .unwrap_err();

        assert!(matches!(error, EepromProvisionServiceError::Transport(_)));
        assert!(memory.captured_argv().is_empty());
        assert!(memory.captured_stdin().is_empty());
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
    fn remote_provision_requires_explicit_disconnect_risk_acknowledgement() {
        let memory = Arc::new(MemorySshTransport::new(PINNED_HOST_KEY));
        let service = service(&memory, PINNED_HOST_KEY);
        let request = EepromProvisionRequest {
            map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
            mode: EepromProvisioningMode::UpdateCalibration,
            serial_number: "2T02D2567K0042".to_owned(),
            overwrite_existing_serial: false,
            segments: Vec::new(),
        };

        let error = service
            .execute(
                EepromProvisionOperation {
                    action: EepromHelperAction::Provision {
                        request,
                        expected_before_sha256: "a".repeat(64),
                        dry_run_token: "b".repeat(64),
                    },
                    experimental_disconnect_risk_acknowledged: false,
                },
                control(),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            EepromProvisionServiceError::Protocol(message)
                if message.contains("explicit experimental disconnect-risk acknowledgement")
        ));
        assert!(memory.captured_argv().is_empty());
        assert!(memory.captured_stdin().is_empty());
    }

    #[test]
    fn rejects_missing_or_invalid_host_key_before_any_transport_use() {
        let memory = Arc::new(MemorySshTransport::new(PINNED_HOST_KEY));
        let resolver: Arc<dyn CredentialResolver> = memory.clone();
        let transport: Arc<dyn SshTransportFactory> = memory.clone();

        for expected_host_key in [None, Some("   "), Some("not-an-openssh-key")] {
            let result = SshEepromProvisionService::new(
                "test-eeprom".to_owned(),
                target(expected_host_key),
                "slot:test".to_owned(),
                "/usr/local/libexec/camera-toolbox-eeprom-helper".to_owned(),
                4096,
                7,
                resolver.clone(),
                transport.clone(),
            );
            assert!(matches!(
                result,
                Err(EepromProvisionServiceError::Protocol(_))
            ));
        }

        assert!(memory.captured_argv().is_empty());
        assert!(memory.captured_stdin().is_empty());
    }
}
