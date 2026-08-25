//! X5_233 驱动控制接线：SSH 启动板端 DEMO233 + TCP 探测。
//!
//! 命令与超时语义对齐 pangbot-calib-tool（Camera_Toolbox 现有实现），
//! 通过 adapters 的 russh transport 执行，不触碰 Godot 对象。

use camera_toolbox_adapters::platforms::ssh_managed::connection::{
    RusshTransportFactory, SshConnectionTarget, SshCredential,SshTransportFactory,
};
use camera_toolbox_adapters::x5_tcp_client::{self, X5ProbeSummary};
use camera_toolbox_app::platform::DumpCancellation;
use camera_toolbox_app::platform::{RemoteOperationControl, RemoteTimeouts};
use secrecy::SecretString;
use std::time::{Duration, Instant};

/// 板端启动 DEMO233 的命令（与 pangbot-calib-tool 一致）：
/// 固定 LD_LIBRARY_PATH / SC233_CALIBRATION_MODE=1 / TCP 控制 9073；幂等（已运行则跳过）。
const X5_233_DRIVER_BOOTSTRAP_COMMAND: &str = "cd /opt || exit 1\nif [ ! -x ./DEMO233 ]; then echo 'missing executable /opt/DEMO233' >&2; exit 2; fi\nif pgrep -f '[D]EMO233' >/dev/null 2>&1; then echo 'DEMO233 already running'; else nohup env LD_LIBRARY_PATH=/usr/hobot/lib:/usr/hobot/lib/sensor:/usr/lib:/lib:/lib64:${LD_LIBRARY_PATH:-} SC233_CALIBRATION_MODE=1 X5_TCP_CONTROL_ENABLE=1 X5_TCP_CONTROL_PORT=9073 ./DEMO233 >/tmp/pangbot-calib-tool-DEMO233.log 2>&1 </dev/null & echo 'DEMO233 start queued'; fi";

/// TCP 探测 X5 驱动（同步、带协议握手）。
pub fn probe(host: &str, port: u16) -> Result<X5ProbeSummary, String> {
    x5_tcp_client::probe(host, port)
}

/// SSH 启动板端驱动并等待 TCP 控制口就绪（最多 20s）。
///
/// 现场工具语义：接受板端任意主机密钥（后续如需 pin 可加字段）；
/// 凭据仅存在于本次调用，不持久化。
#[allow(clippy::too_many_arguments)]
pub fn bootstrap_driver(
    host: &str,
    ssh_port: u16,
    ssh_user: &str,
    ssh_password: &str,
    tcp_port: u16,
) -> Result<X5ProbeSummary, String> {
    let transport = RusshTransportFactory;
    let target = SshConnectionTarget {
        host: host.to_owned(),
        port: ssh_port,
        username: ssh_user.to_owned(),
        expected_host_key: None,
        command_subsystem: None,
        remote_event_subsystem: None,
    };
    let credential = SshCredential::Password(SecretString::from(ssh_password.to_owned()));
    let timeouts = RemoteTimeouts {
        connect: Duration::from_secs(10),
        idle: Duration::from_secs(10),
        overall: Duration::from_secs(20),
    };
    let control = RemoteOperationControl::new(timeouts, DumpCancellation::default())
        .map_err(|error| format!("操作控制初始化失败：{error}"))?;
    let mut session = transport
        .connect(&target, credential, &control)
        .map_err(|error| format!("SSH 连接 {host} 失败：{error}"))?;
    let argv = vec![
        "sh".to_owned(),
        "-lc".to_owned(),
        X5_233_DRIVER_BOOTSTRAP_COMMAND.to_owned(),
    ];
    let output = session
        .execute_argv(&argv, 4096, &control)
        .map_err(|error| format!("远端启动 DEMO233 失败：{error}"))?;
    if output.exit_status.is_some_and(|status| status != 0) {
        return Err(format!(
            "远端启动 DEMO233 退出 {:?}：stdout={} stderr={}",
            output.exit_status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // 轮询 TCP 控制口直到可用（20s）。
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_error = None;
    while Instant::now() < deadline {
        match x5_tcp_client::probe(host, tcp_port) {
            Ok(summary) => return Ok(summary),
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!(
        "DEMO233 已发起启动，但 TCP {tcp_port} 在 20 秒内不可用：{}",
        last_error.unwrap_or_else(|| "无 probe 结果".to_owned())
    ))
}
