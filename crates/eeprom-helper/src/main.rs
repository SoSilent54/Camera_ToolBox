//! `camera-toolbox-eeprom-helper --json-stdin`：目标侧 EEPROM 与通用 I²C JSON helper 入口。

#[cfg(target_os = "linux")]
mod device_lock;
mod engine;
mod i2c_engine;
#[cfg(target_os = "linux")]
mod i2c_linux;
#[cfg(target_os = "linux")]
mod linux_i2c;

use std::{io::Read, process::ExitCode};

use camera_toolbox_app::{
    EEPROM_HELPER_SCHEMA_VERSION, EepromHelperFailure, EepromHelperOutput, EepromHelperRequest,
    EepromRollbackState, I2C_HELPER_MAX_REQUEST_BYTES, I2cHelperAction, I2cHelperOutput,
    I2cHelperRequest, I2cMessageFlag,
};
use camera_toolbox_core::{
    YG_STEREO_P24C64G_V1_MAP_ID, dump_builtin_eeprom_map_config, list_builtin_eeprom_map_configs,
    yg_stereo_p24c64g_v1,
};

const MAX_REQUEST_BYTES: usize = I2C_HELPER_MAX_REQUEST_BYTES;

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if let Some(exit_code) = run_config_cli(&args) {
        return exit_code;
    }

    let output = run(&args);
    let success = output.is_success();
    let mut stdout = std::io::stdout().lock();
    if output.write_json(&mut stdout).is_err()
        || std::io::Write::write_all(&mut stdout, b"\n").is_err()
    {
        return ExitCode::from(3);
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

fn run_config_cli(args: &[String]) -> Option<ExitCode> {
    match args {
        [flag] if flag == "--list-configs" => {
            let configs = list_builtin_eeprom_map_configs()
                .iter()
                .map(|config| {
                    serde_json::json!({
                        "name": config.name,
                        "display_name": config.display_name,
                        "source_map_id": config.source_map_id,
                    })
                })
                .collect::<Vec<_>>();
            match serde_json::to_writer(
                std::io::stdout().lock(),
                &serde_json::json!({"configs": configs}),
            ) {
                Ok(()) => {
                    println!();
                    Some(ExitCode::SUCCESS)
                }
                Err(_) => Some(ExitCode::from(3)),
            }
        }
        [flag, name] if flag == "--dump-config" => match dump_builtin_eeprom_map_config(name) {
            Ok(text) => {
                print!("{text}");
                Some(ExitCode::SUCCESS)
            }
            Err(error) => {
                let failure = failed_eeprom("config_dump_failed", error.to_string());
                let mut stdout = std::io::stdout().lock();
                let wrote = serde_json::to_writer(&mut stdout, &failure).is_ok()
                    && std::io::Write::write_all(&mut stdout, b"\n").is_ok();
                Some(if wrote {
                    ExitCode::from(2)
                } else {
                    ExitCode::from(3)
                })
            }
        },
        _ => None,
    }
}

enum HelperOutput {
    Eeprom(EepromHelperOutput),
    I2c(I2cHelperOutput),
}

impl HelperOutput {
    fn is_success(&self) -> bool {
        matches!(
            self,
            Self::Eeprom(EepromHelperOutput::Success { .. })
                | Self::I2c(I2cHelperOutput::Success { .. })
        )
    }

    fn write_json(&self, writer: &mut impl std::io::Write) -> serde_json::Result<()> {
        match self {
            Self::Eeprom(output) => serde_json::to_writer(writer, output),
            Self::I2c(output) => serde_json::to_writer(writer, output),
        }
    }
}

fn run(args: &[String]) -> HelperOutput {
    if args != ["--json-stdin"] {
        return HelperOutput::Eeprom(failed_eeprom(
            "invalid_invocation",
            "usage: camera-toolbox-eeprom-helper --json-stdin | --list-configs | --dump-config <name>",
        ));
    }
    let mut bytes = Vec::new();
    if let Err(error) = std::io::stdin()
        .lock()
        .take((MAX_REQUEST_BYTES as u64) + 1)
        .read_to_end(&mut bytes)
    {
        return HelperOutput::Eeprom(failed_eeprom("stdin_read_failed", error.to_string()));
    }
    run_request_bytes(&bytes)
}

fn run_request_bytes(bytes: &[u8]) -> HelperOutput {
    if bytes.len() > MAX_REQUEST_BYTES {
        return HelperOutput::Eeprom(failed_eeprom(
            "request_too_large",
            format!("request exceeds {MAX_REQUEST_BYTES} bytes"),
        ));
    }

    match serde_json::from_slice::<I2cHelperRequest>(bytes) {
        Ok(request) => return HelperOutput::I2c(run_i2c_request(request)),
        Err(i2c_error) => match serde_json::from_slice::<EepromHelperRequest>(bytes) {
            Ok(request) => return HelperOutput::Eeprom(run_eeprom_request(request)),
            Err(eeprom_error) => {
                return HelperOutput::Eeprom(failed_eeprom(
                    "invalid_request_json",
                    format!(
                        "invalid I2C request JSON: {i2c_error}; invalid EEPROM request JSON: {eeprom_error}"
                    ),
                ));
            }
        },
    }
}

fn run_i2c_request(request: I2cHelperRequest) -> I2cHelperOutput {
    #[cfg(target_os = "linux")]
    {
        if let Err(output) = i2c_engine::validate_request(&request) {
            return output;
        }
        let _device_locks =
            match device_lock::I2cDeviceLocks::acquire(i2c_lock_keys_for_request(&request)) {
                Ok(locks) => locks,
                Err(error) => return failed_i2c(error.code(), error.message()),
            };
        let mut backend = i2c_linux::LinuxI2cBackend;
        i2c_engine::execute(request, &mut backend)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = request;
        failed_i2c("unsupported_platform", "I2C helper requires Linux i2c-dev")
    }
}

#[cfg(target_os = "linux")]
fn i2c_lock_keys_for_request(request: &I2cHelperRequest) -> Vec<device_lock::I2cLockKey> {
    match &request.action {
        I2cHelperAction::ListBuses => Vec::new(),
        I2cHelperAction::Transfer { transactions } => transactions
            .iter()
            .flat_map(|transaction| {
                transaction.messages.iter().map(|message| {
                    let address_mode = if message
                        .flags
                        .iter()
                        .any(|flag| matches!(flag, I2cMessageFlag::TenBitAddress))
                    {
                        device_lock::I2cAddressMode::TenBit
                    } else {
                        device_lock::I2cAddressMode::SevenBit
                    };
                    device_lock::I2cLockKey::new(transaction.bus, message.address, address_mode)
                })
            })
            .collect(),
    }
}

fn run_eeprom_request(request: EepromHelperRequest) -> EepromHelperOutput {
    if request.schema_version != EEPROM_HELPER_SCHEMA_VERSION {
        return engine::execute(request, &mut UnavailableDevice);
    }
    if request.target.map_id != YG_STEREO_P24C64G_V1_MAP_ID {
        return engine::execute(request, &mut UnavailableDevice);
    }

    #[cfg(target_os = "linux")]
    {
        let bus = request.target.i2c_bus;
        let map = yg_stereo_p24c64g_v1();
        let _device_lock =
            match device_lock::I2cDeviceLocks::acquire_eeprom(bus, map.transport.i2c_address) {
                Ok(device_lock) => device_lock,
                Err(error) => return failed_eeprom(error.code(), error.message()),
            };
        let mut device = match linux_i2c::LinuxEepromDevice::open(bus, map) {
            Ok(device) => device,
            Err(error) => return failed_eeprom("device_open_failed", error),
        };
        engine::execute(request, &mut device)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = request;
        failed_eeprom(
            "unsupported_platform",
            "EEPROM helper requires Linux i2c-dev",
        )
    }
}

struct UnavailableDevice;

impl engine::EepromDevice for UnavailableDevice {
    fn read_range(&mut self, _offset: u16, _bytes: &mut [u8]) -> Result<(), String> {
        Err("device is unavailable".to_owned())
    }

    fn write_page(&mut self, _offset: u16, _bytes: &[u8]) -> Result<(), String> {
        Err("device is unavailable".to_owned())
    }
}

fn failed_eeprom(code: impl Into<String>, message: impl Into<String>) -> EepromHelperOutput {
    EepromHelperOutput::Failure {
        failure: EepromHelperFailure {
            code: code.into(),
            message: message.into(),
            before: None,
            backup: Vec::new(),
            rollback: EepromRollbackState::NotRequired,
            rollback_error: None,
        },
    }
}

fn failed_i2c(code: impl Into<String>, message: impl Into<String>) -> I2cHelperOutput {
    I2cHelperOutput::Failure {
        failure: camera_toolbox_app::I2cHelperFailure {
            code: code.into(),
            message: message.into(),
            transaction_index: None,
            message_index: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use camera_toolbox_app::{
        I2C_HELPER_SCHEMA_VERSION, I2cHelperAction, I2cHelperFailure, I2cHelperResult,
        I2cMessageData, I2cMessageSpec, I2cTransactionSpec,
    };

    use super::*;

    #[test]
    fn dispatches_i2c_schema_before_eeprom_fallback() {
        let request = I2cHelperRequest {
            schema_version: I2C_HELPER_SCHEMA_VERSION,
            action: I2cHelperAction::ListBuses,
        };
        let bytes = serde_json::to_vec(&request).unwrap();

        let output = run_request_bytes(&bytes);

        assert!(matches!(
            output,
            HelperOutput::I2c(I2cHelperOutput::Success {
                result: I2cHelperResult::BusList { .. }
            })
        ));
    }

    #[test]
    fn keeps_eeprom_request_fallback() {
        let request = EepromHelperRequest {
            schema_version: EEPROM_HELPER_SCHEMA_VERSION + 1,
            target: camera_toolbox_app::EepromHelperTarget {
                map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
                i2c_bus: 7,
            },
            action: camera_toolbox_app::EepromHelperAction::Inspect,
        };
        let bytes = serde_json::to_vec(&request).unwrap();

        let output = run_request_bytes(&bytes);

        assert!(matches!(
            output,
            HelperOutput::Eeprom(EepromHelperOutput::Failure {
                failure: EepromHelperFailure { ref code, .. }
            }) if code == "unsupported_schema"
        ));
    }

    #[test]
    fn transfer_validation_failure_uses_i2c_failure_schema() {
        let request = I2cHelperRequest {
            schema_version: I2C_HELPER_SCHEMA_VERSION,
            action: I2cHelperAction::Transfer {
                transactions: vec![I2cTransactionSpec {
                    bus: 7,
                    messages: vec![I2cMessageSpec {
                        address: 0x50,
                        flags: Vec::new(),
                        data: I2cMessageData::Read { byte_len: 0 },
                    }],
                    settle_ms: None,
                }],
            },
        };
        let bytes = serde_json::to_vec(&request).unwrap();

        let output = run_request_bytes(&bytes);

        assert!(matches!(
            output,
            HelperOutput::I2c(I2cHelperOutput::Failure {
                failure: I2cHelperFailure {
                    ref code,
                    transaction_index: Some(0),
                    message_index: Some(0),
                    ..
                }
            }) if code == "invalid_message"
        ));
    }
}
