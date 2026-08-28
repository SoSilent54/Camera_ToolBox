//! EEPROM 读取/写入：SSH + helper sidecar + `FullEepromImage` 标定镜像。
//!
//! 复用 adapters 的 `SshEepromProvisionService`（helper 上传 + 三阶段协议）。
//! helper 二进制（camera-i2c-helper）运行时从本地编译产物读取；
//! 写入目标固定：CH0 → i2c-4、CH3 → i2c-6。

use camera_toolbox_adapters::platforms::ssh_managed::connection::{
    CredentialResolver, RusshTransportFactory, SshConnectionTarget, SshCredential,
};
use camera_toolbox_adapters::platforms::ssh_managed::eeprom::SshEepromProvisionService;
use camera_toolbox_app::platform::{DumpCancellation, RemoteOperationControl, RemoteTimeouts};
use camera_toolbox_app::platform::{
    EepromHelperAction, EepromHelperResult, EepromProvisionOperation, EepromProvisionService,
};
use camera_toolbox_core::CalibrationSolution;
use camera_toolbox_core::calibration_eeprom::FullEepromImage;
use secrecy::SecretString;
use std::sync::Arc;
use std::time::Duration;

/// 进程内密码凭据解析器（凭据仅存于本次调用）。
struct PasswordResolver {
    password: String,
}

impl CredentialResolver for PasswordResolver {
    fn resolve(&self, _credential_ref: &str) -> Result<SshCredential, String> {
        Ok(SshCredential::Password(SecretString::from(
            self.password.clone(),
        )))
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
/// 完整烧录：写入 FLAG + 内参 + SN。设备已有不同 SN 时允许覆盖（工具
/// 写入前置 inspect 展示现有 SN、UI 二次点击确认，覆盖属预期操作）。
#[allow(clippy::too_many_arguments)]
pub fn provision_full_calibration(
    host: &str,
    ssh_user: &str,
    ssh_password: &str,
    i2c_bus: u16,
    helper_payload: Arc<[u8]>,
    solution: &CalibrationSolution,
    serial: &str,
    expected_before_sha256: &str,
) -> Result<EepromHelperResult, String> {
    provision_with_mode(
        host,
        ssh_user,
        ssh_password,
        i2c_bus,
        helper_payload,
        solution,
        serial,
        expected_before_sha256,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn provision_with_mode(
    host: &str,
    ssh_user: &str,
    ssh_password: &str,
    i2c_bus: u16,
    helper_payload: Arc<[u8]>,
    solution: &CalibrationSolution,
    serial: &str,
    expected_before_sha256: &str,
    full: bool,
) -> Result<EepromHelperResult, String> {
    let image = FullEepromImage::from_solution(solution, serial)
        .map_err(|error| format!("构造 EEPROM 镜像失败：{error}"))?;
    let request = if full {
        image.full_provision_request(true)
    } else {
        image.update_calibration_request()
    };
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

/// 探测 helper sidecar 路径：`PONGBOT_HELPER` → 可执行文件同目录（打包发布，
/// 各平台 bundle 根均为 `camera-i2c-helper`）→ CWD 相对编译产物（开发）。
pub fn locate_helper() -> Option<Vec<u8>> {
    if let Ok(path) = std::env::var("PONGBOT_HELPER") {
        if let Ok(bytes) = std::fs::read(&path) {
            return Some(bytes);
        }
    }
    // 打包发布：helper 与可执行文件同目录（Windows/Linux/macOS bundle 根统一命名）。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Ok(bytes) = std::fs::read(dir.join("camera-i2c-helper")) {
                return Some(bytes);
            }
            // 兼容 v0.1.1/v0.1.2-calib Windows 包（曾以 linux-aarch64 后缀命名）。
            if let Ok(bytes) = std::fs::read(dir.join("camera-i2c-helper-linux-aarch64")) {
                return Some(bytes);
            }
        }
    }
    let release = std::path::Path::new("target/release/camera-i2c-helper");
    if release.exists() {
        return std::fs::read(release).ok();
    }
    let debug = std::path::Path::new("target/debug/camera-i2c-helper");
    if debug.exists() {
        return std::fs::read(debug).ok();
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
