//! 固定 helper + JSON stdin 的 SSH EEPROM provisioning adapter。

use std::{collections::BTreeMap, sync::Arc};

use camera_toolbox_app::{
    EEPROM_HELPER_SCHEMA_VERSION, EEPROM_REMOTE_PROVISION_DISABLED_REASON, EepromHelperAction,
    EepromHelperOutput, EepromHelperRequest, EepromHelperResult, EepromHelperTarget,
    EepromProvisionOperation, EepromProvisionService, EepromProvisionServiceError,
    RemoteOperationControl, SensorModeKey, SshEepromConfig,
};
use camera_toolbox_core::YG_STEREO_P24C64G_IMAGE_BYTES;

use super::connection::{
    CredentialResolver, SshConnectionTarget, SshTransportFactory, TransportCommandOutput,
};

pub struct SshEepromProvisionService {
    service_id: String,
    target: SshConnectionTarget,
    credential_ref: String,
    helper_program: String,
    output_limit: usize,
    targets: BTreeMap<SensorModeKey, EepromHelperTarget>,
    resolver: Arc<dyn CredentialResolver>,
    transport: Arc<dyn SshTransportFactory>,
}

impl SshEepromProvisionService {
    /// profile 已在 app 层校验；这里固化不再由 GUI 控制的 helper 与 bus 映射。
    ///
    /// # Errors
    ///
    /// output limit 不能表示为当前平台 `usize`，或 target 重复时返回错误。
    pub fn new(
        service_id: String,
        target: SshConnectionTarget,
        credential_ref: String,
        config: &SshEepromConfig,
        resolver: Arc<dyn CredentialResolver>,
        transport: Arc<dyn SshTransportFactory>,
    ) -> Result<Self, EepromProvisionServiceError> {
        let output_limit = usize::try_from(config.output_limit_bytes).map_err(|_| {
            EepromProvisionServiceError::Protocol(format!(
                "helper output limit {} cannot fit this host",
                config.output_limit_bytes
            ))
        })?;
        let mut targets = BTreeMap::new();
        for configured in &config.targets {
            let target = EepromHelperTarget {
                map_id: configured.map_id.clone(),
                i2c_bus: u32::from(configured.i2c_bus),
            };
            if targets
                .insert(configured.sensor_mode.clone(), target)
                .is_some()
            {
                return Err(EepromProvisionServiceError::Protocol(format!(
                    "duplicate EEPROM target for {:?}",
                    configured.sensor_mode
                )));
            }
        }
        Ok(Self {
            service_id,
            target,
            credential_ref,
            helper_program: config.helper_program.clone(),
            output_limit,
            targets,
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
        if matches!(&request.action, EepromHelperAction::Provision { .. }) {
            return Err(EepromProvisionServiceError::Protocol(
                EEPROM_REMOTE_PROVISION_DISABLED_REASON.to_owned(),
            ));
        }
        let target = self
            .targets
            .get(&request.sensor_mode)
            .cloned()
            .ok_or_else(|| {
                EepromProvisionServiceError::TargetNotConfigured(request.sensor_mode.clone())
            })?;
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
        EepromRollbackState, EepromSerialState, EepromWriteResult, RemoteTimeouts, SensorId,
        SshEepromTargetConfig,
    };
    use camera_toolbox_core::{
        EepromProvisionRequest, EepromProvisioningMode, YG_STEREO_P24C64G_V1_MAP_ID,
    };

    use super::super::memory_transport::MemorySshTransport;

    fn sensor_mode() -> SensorModeKey {
        SensorModeKey {
            sensor_id: SensorId::new("imx219").unwrap(),
            mode_id: "1920x1080".to_owned(),
        }
    }

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

    fn service(
        memory: &Arc<MemorySshTransport>,
        expected_host_key: &str,
    ) -> SshEepromProvisionService {
        memory.allow_credential("slot:test");
        let config = SshEepromConfig {
            helper_program: "/usr/local/libexec/camera-toolbox-eeprom-helper".to_owned(),
            output_limit_bytes: 4096,
            targets: vec![SshEepromTargetConfig {
                sensor_mode: sensor_mode(),
                map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
                i2c_bus: 7,
            }],
        };
        let resolver: Arc<dyn CredentialResolver> = memory.clone();
        let transport: Arc<dyn SshTransportFactory> = memory.clone();
        SshEepromProvisionService::new(
            "test-eeprom".to_owned(),
            SshConnectionTarget {
                host: "camera.local".to_owned(),
                port: 22,
                username: "root".to_owned(),
                expected_host_key: Some(expected_host_key.to_owned()),
                command_subsystem: None,
                remote_event_subsystem: None,
            },
            "slot:test".to_owned(),
            &config,
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
        let memory = Arc::new(MemorySshTransport::new("pinned-key"));
        memory.set_command_output(output(EepromHelperResult::Inspect(EepromInspectResult {
            state: device_state('a'),
            backup: vec![0; YG_STEREO_P24C64G_IMAGE_BYTES],
        })));
        let service = service(&memory, "pinned-key");

        let result = service
            .execute(
                EepromProvisionOperation {
                    sensor_mode: sensor_mode(),
                    action: EepromHelperAction::Inspect,
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
        let service = service(&memory, "pinned-key");

        let error = service
            .execute(
                EepromProvisionOperation {
                    sensor_mode: sensor_mode(),
                    action: EepromHelperAction::Inspect,
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
    fn remote_provision_is_blocked_before_ssh_execution() {
        let memory = Arc::new(MemorySshTransport::new("pinned-key"));
        let service = service(&memory, "pinned-key");
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
                    sensor_mode: sensor_mode(),
                    action: EepromHelperAction::Provision {
                        request,
                        expected_before_sha256: "a".repeat(64),
                        dry_run_token: "b".repeat(64),
                    },
                },
                control(),
            )
            .unwrap_err();

        assert_eq!(
            error,
            EepromProvisionServiceError::Protocol(
                EEPROM_REMOTE_PROVISION_DISABLED_REASON.to_owned()
            )
        );
        assert!(memory.captured_argv().is_empty());
        assert!(memory.captured_stdin().is_empty());
    }
}
