//! EEPROM provisioning 状态机；设备 IO 通过窄 trait 注入，便于无硬件验证。

use camera_toolbox_app::{
    EEPROM_HELPER_SCHEMA_VERSION, EepromDeviceState, EepromDryRunResult, EepromHelperAction,
    EepromHelperFailure, EepromHelperOutput, EepromHelperRequest, EepromHelperResult,
    EepromInspectResult, EepromPageWritePlan, EepromRollbackState, EepromSerialState,
    EepromWriteResult,
};
use camera_toolbox_core::{
    CalibrationStorageMap, EepromProvisionRequest, EepromProvisioningMode, EepromWriteSegment,
    YG_STEREO_P24C64G_FLAG, YG_STEREO_P24C64G_IMAGE_BYTES, YG_STEREO_P24C64G_V1_MAP_ID,
    serial_checksum, yg_stereo_p24c64g_v1,
};
use sha2::{Digest, Sha256};

pub(crate) trait EepromDevice {
    fn read_range(&mut self, offset: u16, bytes: &mut [u8]) -> Result<(), String>;
    fn write_page(&mut self, offset: u16, bytes: &[u8]) -> Result<(), String>;
}

#[derive(Debug)]
struct EngineError {
    code: &'static str,
    message: String,
}

impl EngineError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub(crate) fn execute(
    request: EepromHelperRequest,
    device: &mut dyn EepromDevice,
) -> EepromHelperOutput {
    if request.schema_version != EEPROM_HELPER_SCHEMA_VERSION {
        return failure(
            "unsupported_schema",
            format!(
                "helper supports schema {}, got {}",
                EEPROM_HELPER_SCHEMA_VERSION, request.schema_version
            ),
            None,
            Vec::new(),
            EepromRollbackState::NotRequired,
            None,
        );
    }
    if request.target.map_id != YG_STEREO_P24C64G_V1_MAP_ID {
        return failure(
            "unsupported_map",
            format!("unsupported EEPROM map {}", request.target.map_id),
            None,
            Vec::new(),
            EepromRollbackState::NotRequired,
            None,
        );
    }
    let map = yg_stereo_p24c64g_v1();
    match request.action {
        EepromHelperAction::Inspect => inspect(device, map),
        EepromHelperAction::DryRun { request } => dry_run(device, map, &request),
        EepromHelperAction::Provision {
            request,
            expected_before_sha256,
            dry_run_token,
        } => provision(
            device,
            map,
            &request,
            &expected_before_sha256,
            &dry_run_token,
        ),
    }
}

fn inspect(device: &mut dyn EepromDevice, map: &CalibrationStorageMap) -> EepromHelperOutput {
    match read_image(device) {
        Ok(backup) => EepromHelperOutput::Success {
            result: EepromHelperResult::Inspect(EepromInspectResult {
                state: device_state(map, &backup),
                backup,
            }),
        },
        Err(error) => failure(
            error.code,
            error.message,
            None,
            Vec::new(),
            EepromRollbackState::NotRequired,
            None,
        ),
    }
}

fn dry_run(
    device: &mut dyn EepromDevice,
    map: &CalibrationStorageMap,
    request: &EepromProvisionRequest,
) -> EepromHelperOutput {
    if let Err(error) = request.validate() {
        return failure(
            "invalid_request",
            error.to_string(),
            None,
            Vec::new(),
            EepromRollbackState::NotRequired,
            None,
        );
    }
    let backup = match read_image(device) {
        Ok(backup) => backup,
        Err(error) => {
            return failure(
                error.code,
                error.message,
                None,
                Vec::new(),
                EepromRollbackState::NotRequired,
                None,
            );
        }
    };
    let state = device_state(map, &backup);
    if let Err(error) = validate_device_state(request, &state) {
        return failure(
            error.code,
            error.message,
            Some(state),
            backup,
            EepromRollbackState::NotRequired,
            None,
        );
    }
    let page_plan = page_plan(map, request);
    let dry_run_token = dry_run_token(request, &state.image_sha256);
    EepromHelperOutput::Success {
        result: EepromHelperResult::DryRun(EepromDryRunResult {
            state,
            backup,
            page_plan,
            dry_run_token,
        }),
    }
}

fn provision(
    device: &mut dyn EepromDevice,
    map: &CalibrationStorageMap,
    request: &EepromProvisionRequest,
    expected_before_sha256: &str,
    expected_dry_run_token: &str,
) -> EepromHelperOutput {
    if let Err(error) = request.validate() {
        return failure(
            "invalid_request",
            error.to_string(),
            None,
            Vec::new(),
            EepromRollbackState::NotRequired,
            None,
        );
    }
    let backup = match read_image(device) {
        Ok(backup) => backup,
        Err(error) => {
            return failure(
                error.code,
                error.message,
                None,
                Vec::new(),
                EepromRollbackState::NotRequired,
                None,
            );
        }
    };
    let before = device_state(map, &backup);
    if let Err(error) = validate_device_state(request, &before) {
        return failure(
            error.code,
            error.message,
            Some(before),
            backup,
            EepromRollbackState::NotRequired,
            None,
        );
    }
    if before.image_sha256 != expected_before_sha256 {
        return failure(
            "stale_device",
            "EEPROM contents changed after dry-run".to_owned(),
            Some(before),
            backup,
            EepromRollbackState::NotRequired,
            None,
        );
    }
    let actual_token = dry_run_token(request, &before.image_sha256);
    if actual_token != expected_dry_run_token {
        return failure(
            "dry_run_token_mismatch",
            "provision request does not match the completed dry-run".to_owned(),
            Some(before),
            backup,
            EepromRollbackState::NotRequired,
            None,
        );
    }

    let mut expected = backup.clone();
    apply_segments(&mut expected, &request.segments);
    let plan = page_plan(map, request);
    if let Err(write_error) = execute_write_plan(device, map, request, &expected) {
        let (rollback, rollback_error) = rollback(device, map, request, &backup);
        return failure(
            write_error.code,
            write_error.message,
            Some(before),
            backup,
            rollback,
            rollback_error,
        );
    }
    let readback = match read_image(device) {
        Ok(readback) => readback,
        Err(error) => {
            let (rollback, rollback_error) = rollback(device, map, request, &backup);
            return failure(
                error.code,
                error.message,
                Some(before),
                backup,
                rollback,
                rollback_error,
            );
        }
    };
    if readback != expected {
        let (rollback, rollback_error) = rollback(device, map, request, &backup);
        return failure(
            "verification_failed",
            first_difference(&expected, &readback),
            Some(before),
            backup,
            rollback,
            rollback_error,
        );
    }
    let after = device_state(map, &readback);
    EepromHelperOutput::Success {
        result: EepromHelperResult::Provision(EepromWriteResult {
            before,
            after,
            backup,
            page_plan: plan,
            bytewise_verified: true,
            rollback: EepromRollbackState::NotRequired,
        }),
    }
}

fn execute_write_plan(
    device: &mut dyn EepromDevice,
    map: &CalibrationStorageMap,
    request: &EepromProvisionRequest,
    expected: &[u8],
) -> Result<(), EngineError> {
    let flag = field(map, "flag")?;
    write_verified_page(device, flag.offset, &vec![0_u8; usize::from(flag.byte_len)])?;
    for segment in request
        .segments
        .iter()
        .filter(|segment| segment.offset != flag.offset)
    {
        for page in segment_pages(map, segment) {
            write_verified_page(device, page.offset, &page.bytes)?;
        }
    }
    let start = usize::from(flag.offset);
    let end = start + usize::from(flag.byte_len);
    write_verified_page(device, flag.offset, &expected[start..end])
}

fn rollback(
    device: &mut dyn EepromDevice,
    map: &CalibrationStorageMap,
    request: &EepromProvisionRequest,
    backup: &[u8],
) -> (EepromRollbackState, Option<String>) {
    let result = (|| {
        let flag = field(map, "flag")?;
        write_verified_page(device, flag.offset, &vec![0_u8; usize::from(flag.byte_len)])?;
        for segment in request
            .segments
            .iter()
            .filter(|segment| segment.offset != flag.offset)
        {
            let start = usize::from(segment.offset);
            let restored = EepromWriteSegment {
                offset: segment.offset,
                bytes: backup[start..start + segment.bytes.len()].to_vec(),
            };
            for page in segment_pages(map, &restored) {
                write_verified_page(device, page.offset, &page.bytes)?;
            }
        }
        let start = usize::from(flag.offset);
        let end = start + usize::from(flag.byte_len);
        write_verified_page(device, flag.offset, &backup[start..end])?;
        let restored = read_image(device)?;
        if restored != backup {
            return Err(EngineError::new(
                "rollback_verification_failed",
                first_difference(backup, &restored),
            ));
        }
        Ok(())
    })();
    match result {
        Ok(()) => (EepromRollbackState::Restored, None),
        Err(error) => (EepromRollbackState::Failed, Some(error.message)),
    }
}

fn validate_device_state(
    request: &EepromProvisionRequest,
    state: &EepromDeviceState,
) -> Result<(), EngineError> {
    match request.mode {
        EepromProvisioningMode::FullProvision => match &state.serial {
            EepromSerialState::Empty => Ok(()),
            EepromSerialState::Valid { value } if value == &request.serial_number => Ok(()),
            EepromSerialState::Valid { value } => {
                if request.overwrite_existing_serial {
                    Ok(())
                } else {
                    Err(EngineError::new(
                        "serial_overwrite_confirmation_required",
                        format!(
                            "device SN {value} differs from requested SN {}",
                            request.serial_number
                        ),
                    ))
                }
            }
            EepromSerialState::Invalid { .. } => {
                if request.overwrite_existing_serial {
                    Ok(())
                } else {
                    Err(EngineError::new(
                        "serial_overwrite_confirmation_required",
                        "device contains a non-empty invalid SN region".to_owned(),
                    ))
                }
            }
        },
        EepromProvisioningMode::UpdateCalibration => {
            if !state.flag_valid {
                return Err(EngineError::new(
                    "invalid_device_flag",
                    "UpdateCalibration requires a valid existing calibration FLAG",
                ));
            }
            match &state.serial {
                EepromSerialState::Valid { value } if value == &request.serial_number => Ok(()),
                EepromSerialState::Valid { value } => Err(EngineError::new(
                    "serial_mismatch",
                    format!(
                        "device SN {value} differs from requested SN {}",
                        request.serial_number
                    ),
                )),
                EepromSerialState::Empty => Err(EngineError::new(
                    "serial_missing",
                    "UpdateCalibration requires an existing valid SN",
                )),
                EepromSerialState::Invalid { .. } => Err(EngineError::new(
                    "serial_checksum_invalid",
                    "UpdateCalibration requires a valid existing SN checksum",
                )),
            }
        }
    }
}

fn page_plan(
    map: &CalibrationStorageMap,
    request: &EepromProvisionRequest,
) -> Vec<EepromPageWritePlan> {
    let flag = field(map, "flag").expect("built-in map has flag field");
    let mut plan = vec![EepromPageWritePlan {
        stage: "invalidate_flag".to_owned(),
        offset: flag.offset,
        byte_len: flag.byte_len,
    }];
    for segment in request
        .segments
        .iter()
        .filter(|segment| segment.offset != flag.offset)
    {
        plan.extend(
            segment_pages(map, segment)
                .into_iter()
                .map(|page| EepromPageWritePlan {
                    stage: "write_payload".to_owned(),
                    offset: page.offset,
                    byte_len: u16::try_from(page.bytes.len()).expect("page size fits u16"),
                }),
        );
    }
    plan.push(EepromPageWritePlan {
        stage: "commit_flag".to_owned(),
        offset: flag.offset,
        byte_len: flag.byte_len,
    });
    plan
}

fn segment_pages(
    map: &CalibrationStorageMap,
    segment: &EepromWriteSegment,
) -> Vec<EepromWriteSegment> {
    let page_size = usize::from(map.transport.page_size_bytes);
    let mut pages = Vec::new();
    let mut consumed = 0_usize;
    while consumed < segment.bytes.len() {
        let offset = usize::from(segment.offset) + consumed;
        let room = page_size - offset % page_size;
        let count = room.min(segment.bytes.len() - consumed);
        pages.push(EepromWriteSegment {
            offset: u16::try_from(offset).expect("built-in EEPROM offset fits u16"),
            bytes: segment.bytes[consumed..consumed + count].to_vec(),
        });
        consumed += count;
    }
    pages
}

fn write_verified_page(
    device: &mut dyn EepromDevice,
    offset: u16,
    bytes: &[u8],
) -> Result<(), EngineError> {
    device
        .write_page(offset, bytes)
        .map_err(|error| EngineError::new("write_failed", error))?;
    let mut readback = vec![0_u8; bytes.len()];
    device
        .read_range(offset, &mut readback)
        .map_err(|error| EngineError::new("readback_failed", error))?;
    if readback != bytes {
        return Err(EngineError::new(
            "readback_mismatch",
            first_difference(bytes, &readback),
        ));
    }
    Ok(())
}

fn read_image(device: &mut dyn EepromDevice) -> Result<Vec<u8>, EngineError> {
    let mut image = vec![0_u8; YG_STEREO_P24C64G_IMAGE_BYTES];
    device
        .read_range(0, &mut image)
        .map_err(|error| EngineError::new("device_read_failed", error))?;
    Ok(image)
}

fn apply_segments(image: &mut [u8], segments: &[EepromWriteSegment]) {
    for segment in segments {
        let start = usize::from(segment.offset);
        image[start..start + segment.bytes.len()].copy_from_slice(&segment.bytes);
    }
}

fn device_state(map: &CalibrationStorageMap, image: &[u8]) -> EepromDeviceState {
    let flag = field(map, "flag").expect("built-in map has flag field");
    let serial = field(map, "serial_number").expect("built-in map has serial field");
    let checksum = field(map, "serial_checksum").expect("built-in map has checksum field");
    let flag_start = usize::from(flag.offset);
    let serial_start = usize::from(serial.offset);
    let serial_end = serial_start + usize::from(serial.byte_len);
    let serial_bytes = &image[serial_start..serial_end];
    let stored_checksum = image[usize::from(checksum.offset)];
    let serial_state = if serial_bytes.iter().all(|byte| *byte == 0)
        || serial_bytes.iter().all(|byte| *byte == 0xff)
    {
        EepromSerialState::Empty
    } else if serial_bytes.is_ascii() {
        let mut fixed = [0_u8; 14];
        fixed.copy_from_slice(serial_bytes);
        if serial_checksum(&fixed) == stored_checksum {
            EepromSerialState::Valid {
                value: std::str::from_utf8(serial_bytes)
                    .expect("ASCII bytes are UTF-8")
                    .to_owned(),
            }
        } else {
            EepromSerialState::Invalid {
                raw_hex: hex(serial_bytes),
                checksum: stored_checksum,
            }
        }
    } else {
        EepromSerialState::Invalid {
            raw_hex: hex(serial_bytes),
            checksum: stored_checksum,
        }
    };
    EepromDeviceState {
        image_sha256: sha256(image),
        flag_valid: image[flag_start..flag_start + usize::from(flag.byte_len)]
            == YG_STEREO_P24C64G_FLAG,
        serial: serial_state,
    }
}

fn field<'a>(
    map: &'a CalibrationStorageMap,
    name: &'static str,
) -> Result<&'a camera_toolbox_core::StorageField, EngineError> {
    map.fields
        .iter()
        .find(|field| field.name == name)
        .ok_or_else(|| {
            EngineError::new(
                "invalid_builtin_map",
                format!("built-in map is missing field {name}"),
            )
        })
}

fn dry_run_token(request: &EepromProvisionRequest, before_sha256: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"camera-toolbox/eeprom-dry-run/v1\0");
    hash.update(before_sha256.as_bytes());
    let encoded = serde_json::to_vec(request).expect("validated request is serializable");
    hash.update(encoded);
    hex(&hash.finalize())
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn first_difference(expected: &[u8], actual: &[u8]) -> String {
    if let Some((index, (expected, actual))) = expected
        .iter()
        .zip(actual)
        .enumerate()
        .find(|(_, (expected, actual))| expected != actual)
    {
        format!("byte mismatch at 0x{index:04x}: expected 0x{expected:02x}, got 0x{actual:02x}")
    } else {
        format!(
            "byte length mismatch: expected {}, got {}",
            expected.len(),
            actual.len()
        )
    }
}

fn failure(
    code: impl Into<String>,
    message: impl Into<String>,
    before: Option<EepromDeviceState>,
    backup: Vec<u8>,
    rollback: EepromRollbackState,
    rollback_error: Option<String>,
) -> EepromHelperOutput {
    EepromHelperOutput::Failure {
        failure: EepromHelperFailure {
            code: code.into(),
            message: message.into(),
            before,
            backup,
            rollback,
            rollback_error,
        },
    }
}

#[cfg(test)]
mod tests {
    use camera_toolbox_core::{
        CalibrationImageSize, CalibrationSolution, FullEepromImage, PANGBOT_CALIBRATION_FLAGS,
    };

    use super::*;

    struct MemoryDevice {
        bytes: Vec<u8>,
        writes: Vec<(u16, Vec<u8>)>,
        fail_once_at: Option<u16>,
    }

    impl MemoryDevice {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes,
                writes: Vec::new(),
                fail_once_at: None,
            }
        }
    }

    impl EepromDevice for MemoryDevice {
        fn read_range(&mut self, offset: u16, bytes: &mut [u8]) -> Result<(), String> {
            let start = usize::from(offset);
            let source = self
                .bytes
                .get(start..start + bytes.len())
                .ok_or_else(|| "read out of range".to_owned())?;
            bytes.copy_from_slice(source);
            Ok(())
        }

        fn write_page(&mut self, offset: u16, bytes: &[u8]) -> Result<(), String> {
            if self.fail_once_at == Some(offset) {
                self.fail_once_at = None;
                return Err(format!("injected write failure at 0x{offset:04x}"));
            }
            let start = usize::from(offset);
            let destination = self
                .bytes
                .get_mut(start..start + bytes.len())
                .ok_or_else(|| "write out of range".to_owned())?;
            destination.copy_from_slice(bytes);
            self.writes.push((offset, bytes.to_vec()));
            Ok(())
        }
    }

    fn solution(fx: f64) -> CalibrationSolution {
        CalibrationSolution {
            image_size: CalibrationImageSize::new(1920, 1080).unwrap(),
            camera_matrix: [fx, 0.0, 960.0, 0.0, fx, 540.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.1, -0.05, 0.001, -0.002, 0.003, 0.0, 0.0, 0.0],
            rms_error: 0.1,
            calibration_flags: PANGBOT_CALIBRATION_FLAGS,
            views: Vec::new(),
        }
    }

    fn helper_request(action: EepromHelperAction) -> EepromHelperRequest {
        EepromHelperRequest {
            schema_version: EEPROM_HELPER_SCHEMA_VERSION,
            target: camera_toolbox_app::EepromHelperTarget {
                map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
                i2c_bus: 7,
            },
            action,
        }
    }

    fn dry_run(device: &mut MemoryDevice, request: EepromProvisionRequest) -> EepromDryRunResult {
        let EepromHelperOutput::Success {
            result: EepromHelperResult::DryRun(result),
        } = execute(
            helper_request(EepromHelperAction::DryRun { request }),
            device,
        )
        else {
            panic!("dry-run failed")
        };
        result
    }

    #[test]
    fn full_provision_invalidates_flag_writes_pages_and_commits_flag_last() {
        let image = FullEepromImage::from_solution(&solution(1200.0), "2T02D2567K0042").unwrap();
        let request = image.full_provision_request(false);
        let mut initial = vec![0x5a; YG_STEREO_P24C64G_IMAGE_BYTES];
        initial[..8].fill(0);
        initial[0x125..0x133].fill(0xff);
        let reserved_before = initial[0x58..0x125].to_vec();
        let mut device = MemoryDevice::new(initial);
        let dry = dry_run(&mut device, request.clone());
        assert_eq!(
            dry.page_plan
                .iter()
                .map(|page| (page.stage.as_str(), page.offset, page.byte_len))
                .collect::<Vec<_>>(),
            [
                ("invalidate_flag", 0x0000, 8),
                ("write_payload", 0x0010, 16),
                ("write_payload", 0x0020, 32),
                ("write_payload", 0x0040, 24),
                ("write_payload", 0x0125, 15),
                ("commit_flag", 0x0000, 8),
            ]
        );

        let output = execute(
            helper_request(EepromHelperAction::Provision {
                request,
                expected_before_sha256: dry.state.image_sha256,
                dry_run_token: dry.dry_run_token,
            }),
            &mut device,
        );
        let EepromHelperOutput::Success {
            result: EepromHelperResult::Provision(result),
        } = output
        else {
            panic!("provision failed")
        };
        assert!(result.bytewise_verified);
        assert_eq!(device.writes.first().unwrap(), &(0, vec![0; 8]));
        assert_eq!(device.writes.last().unwrap(), &(0, b"hessian\0".to_vec()));
        assert_eq!(&device.bytes[0x58..0x125], reserved_before);
        assert_eq!(&device.bytes[0x125..0x133], b"2T02D2567K0042");
    }

    #[test]
    fn update_requires_matching_sn_and_preserves_identity_and_reserved_bytes() {
        let existing = FullEepromImage::from_solution(&solution(1000.0), "2T02D2567K0042").unwrap();
        let update = FullEepromImage::from_solution(&solution(1300.0), "2T02D2567K0042")
            .unwrap()
            .update_calibration_request();
        let mut initial = existing.as_bytes().to_vec();
        initial[0x58..0x125].fill(0xa5);
        let identity_before = initial[0x125..0x134].to_vec();
        let reserved_before = initial[0x58..0x125].to_vec();
        let mut device = MemoryDevice::new(initial);
        let dry = dry_run(&mut device, update.clone());
        let output = execute(
            helper_request(EepromHelperAction::Provision {
                request: update,
                expected_before_sha256: dry.state.image_sha256,
                dry_run_token: dry.dry_run_token,
            }),
            &mut device,
        );
        assert!(matches!(
            output,
            EepromHelperOutput::Success {
                result: EepromHelperResult::Provision(_)
            }
        ));
        assert_eq!(&device.bytes[0x58..0x125], reserved_before);
        assert_eq!(&device.bytes[0x125..0x134], identity_before);
    }

    #[test]
    fn different_serial_requires_explicit_overwrite_and_failed_write_rolls_back() {
        let existing = FullEepromImage::from_solution(&solution(1000.0), "2T02D2567K0042").unwrap();
        let replacement =
            FullEepromImage::from_solution(&solution(1400.0), "2T02D2567K9999").unwrap();
        let mut device = MemoryDevice::new(existing.as_bytes().to_vec());
        let rejected = execute(
            helper_request(EepromHelperAction::DryRun {
                request: replacement.full_provision_request(false),
            }),
            &mut device,
        );
        assert!(matches!(
            rejected,
            EepromHelperOutput::Failure {
                failure: EepromHelperFailure { ref code, .. }
            } if code == "serial_overwrite_confirmation_required"
        ));

        let request = replacement.full_provision_request(true);
        let dry = dry_run(&mut device, request.clone());
        let before = device.bytes.clone();
        device.fail_once_at = Some(0x20);
        let failed = execute(
            helper_request(EepromHelperAction::Provision {
                request,
                expected_before_sha256: dry.state.image_sha256,
                dry_run_token: dry.dry_run_token,
            }),
            &mut device,
        );
        assert!(matches!(
            failed,
            EepromHelperOutput::Failure {
                failure: EepromHelperFailure {
                    rollback: EepromRollbackState::Restored,
                    ..
                }
            }
        ));
        assert_eq!(device.bytes, before);
    }
}
