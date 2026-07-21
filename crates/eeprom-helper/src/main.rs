//! `camera-toolbox-eeprom-helper --json-stdin`：目标侧唯一受支持入口。

#[cfg(target_os = "linux")]
mod device_lock;
mod engine;
#[cfg(target_os = "linux")]
mod linux_i2c;

use std::{io::Read, process::ExitCode};

use camera_toolbox_app::{
    EEPROM_HELPER_SCHEMA_VERSION, EepromHelperFailure, EepromHelperOutput, EepromRollbackState,
};
use camera_toolbox_core::{YG_STEREO_P24C64G_V1_MAP_ID, yg_stereo_p24c64g_v1};

const MAX_REQUEST_BYTES: u64 = 64 * 1024;

fn main() -> ExitCode {
    let output = run();
    let success = matches!(output, EepromHelperOutput::Success { .. });
    let mut stdout = std::io::stdout().lock();
    if serde_json::to_writer(&mut stdout, &output).is_err()
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

fn run() -> EepromHelperOutput {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args != ["--json-stdin"] {
        return failed(
            "invalid_invocation",
            "usage: camera-toolbox-eeprom-helper --json-stdin",
        );
    }
    let mut bytes = Vec::new();
    if let Err(error) = std::io::stdin()
        .lock()
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
    {
        return failed("stdin_read_failed", error.to_string());
    }
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return failed(
            "request_too_large",
            format!("request exceeds {MAX_REQUEST_BYTES} bytes"),
        );
    }
    let request: camera_toolbox_app::EepromHelperRequest = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(error) => return failed("invalid_request_json", error.to_string()),
    };
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
            match device_lock::EepromDeviceLock::acquire(bus, map.transport.i2c_address) {
                Ok(device_lock) => device_lock,
                Err(error) => return failed(error.code(), error.message()),
            };
        let mut device = match linux_i2c::LinuxEepromDevice::open(bus, map) {
            Ok(device) => device,
            Err(error) => return failed("device_open_failed", error),
        };
        engine::execute(request, &mut device)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = request;
        failed(
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

fn failed(code: impl Into<String>, message: impl Into<String>) -> EepromHelperOutput {
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
