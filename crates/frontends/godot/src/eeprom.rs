//! EEPROM 读取/写入：SSH + helper sidecar + `FullEepromImage` 标定镜像。
//!
//! 复用 adapters 的 `SshEepromProvisionService`（helper 上传 + 三阶段协议）。
//! helper 二进制（camera-i2c-helper）运行时从本地编译产物读取；
//! 写入目标固定：CH0 → i2c-4、CH3 → i2c-6。

use camera_toolbox_adapters::platforms::ssh_managed::connection::{
    CredentialResolver, RusshTransportFactory, SshConnectionTarget, SshCredential,
};
use camera_toolbox_adapters::platforms::ssh_managed::eeprom::SshEepromProvisionService;
use camera_toolbox_app::platform::{
    EepromHelperAction, EepromHelperResult, EepromProvisionOperation, EepromProvisionService,
};
use camera_toolbox_app::platform::{
    DumpCancellation, RemoteOperationControl, RemoteTimeouts,
};
use camera_toolbox_core::calibration_eeprom::FullEepromImage;
use camera_toolbox_core::CalibrationSolution;
use secrecy::SecretString;
use std::sync::Arc;
use std::time::Duration;

/// 进程内密码凭据解析器（凭据仅存于本次调用）。
struct PasswordResolver {
    password: String,
}

impl CredentialResolver for PasswordResolver {
    fn resolve(&self, _credential_ref: &str) -> Result<SshCredential, String> {
        Ok(SshCredential::Password(SecretString::from(self.password.clone())))
    }
}

/// 读取当前 EEPROM 状态（Inspect：镜像 sha256、FLAG、SN）。
#[allow(clippy::too_many_arguments)]
pub fn inspect(
    host: &str,
    ssh_user: &str,
    ssh_password: &str,
    i2c_bus: u16,
    helper_payload: Arc<[u8]>,
) -> Result<EepromHelperResult, String> {
    let service = build_service(host, ssh_user, ssh_password, i2c_bus, helper_payload)?;
    let control = control();
    service
        .execute(
            EepromProvisionOperation {
                action: EepromHelperAction::Inspect,
            },
            control,
        )
        .map_err(|error| format!("读取 EEPROM 失败：{error}"))
}

/// 更新标定内参（UpdateCalibration：SN 只作身份校验，不写入）。
///
/// `serial` 与 `expected_before_sha256` 必须来自同一次 Inspect。
#[allow(clippy::too_many_arguments)]
pub fn provision_calibration(
    host: &str,
    ssh_user: &str,
    ssh_password: &str,
    i2c_bus: u16,
    helper_payload: Arc<[u8]>,
    solution: &CalibrationSolution,
    serial: &str,
    expected_before_sha256: &str,
) -> Result<EepromHelperResult, String> {
    let image = FullEepromImage::from_solution(solution, serial)
        .map_err(|error| format!("构造 EEPROM 镜像失败：{error}"))?;
    let request = image.update_calibration_request();
    let service = build_service(host, ssh_user, ssh_password, i2c_bus, helper_payload)?;
    let control = control();
    service
        .execute(
            EepromProvisionOperation {
                action: EepromHelperAction::Provision {
                    request,
                    expected_before_sha256: expected_before_sha256.to_owned(),
                },
            },
            control,
        )
        .map_err(|error| format!("写入 EEPROM 失败：{error}"))
}

/// 探测 helper sidecar 路径：`PONGBOT_HELPER` 或本地 release 编译产物。
pub fn locate_helper() -> Option<Vec<u8>> {
    if let Ok(path) = std::env::var("PONGBOT_HELPER") {
        if let Ok(bytes) = std::fs::read(&path) {
            return Some(bytes);
        }
    }
    let local = std::path::Path::new("target/release/camera-i2c-helper");
    if local.exists() {
        return std::fs::read(local).ok();
    }
    None
}

fn build_service(
    host: &str,
    ssh_user: &str,
    ssh_password: &str,
    i2c_bus: u16,
    helper_payload: Arc<[u8]>,
) -> Result<SshEepromProvisionService, String> {
    let target = SshConnectionTarget {
        host: host.to_owned(),
        port: 22,
        username: ssh_user.to_owned(),
        expected_host_key: None,
        command_subsystem: None,
        remote_event_subsystem: None,
    };
    SshEepromProvisionService::new(
        "pongbot-calib-tool".to_owned(),
        target,
        "pongbot-password".to_owned(),
        4096,
        i2c_bus,
        helper_payload,
        Arc::new(PasswordResolver {
            password: ssh_password.to_owned(),
        }),
        Arc::new(RusshTransportFactory),
    )
    .map_err(|error| format!("EEPROM 服务初始化失败：{error}"))
}

fn control() -> RemoteOperationControl {
    RemoteOperationControl::new(
        RemoteTimeouts {
            connect: Duration::from_secs(10),
            idle: Duration::from_secs(10),
            overall: Duration::from_secs(60),
        },
        DumpCancellation::default(),
    )
    .expect("EEPROM 超时参数合法")
}
