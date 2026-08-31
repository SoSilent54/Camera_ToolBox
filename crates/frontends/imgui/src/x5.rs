//! X5_233 驱动控制接线：SSH 启动板端 DEMO233 + TCP 探测。
//!
//! 命令与超时语义对齐 pangbot-calib-tool（Camera_Toolbox 现有实现），
//! 通过 adapters 的 russh transport 执行，不触碰 Godot 对象。

use camera_toolbox_adapters::platforms::ssh_managed::connection::{
    RusshTransportFactory, SshConnectionTarget, SshCredential, SshTransportFactory,
};
use camera_toolbox_adapters::x5_tcp_client::{self, X5ProbeSummary};
use camera_toolbox_app::platform::DumpCancellation;
use camera_toolbox_app::platform::{RemoteOperationControl, RemoteTimeouts};
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 板端标定驱动路径：新版本一律部署为 DEMO233_Calib，保留原厂 /opt/DEMO233 不动，
/// 便于回退；启动命令、版本检查与替换都只针对该文件。
const X5_233_DRIVER_REMOTE_PATH: &str = "/opt/DEMO233_Calib";

/// 板端启动标定驱动的命令：
/// - 固定 LD_LIBRARY_PATH / SC233_CALIBRATION_MODE=1 / TCP 控制 9073；
/// - X5_TCP_RING_DEPTH=96 扩大抓帧 ring（默认 32 → 60fps 缓存 ~1.6s，降低
///   RTSP 延迟下 SNAPSHOT timestamp 超窗导致的取图失败）；
/// - 幂等：DEMO233_Calib 已在跑则跳过；原版 DEMO233 占用 9073 时先停掉，
///   避免端口冲突导致标定驱动起不来。
const X5_233_DRIVER_BOOTSTRAP_COMMAND: &str = "cd /opt || exit 1\nif [ ! -x ./DEMO233_Calib ]; then echo 'missing executable /opt/DEMO233_Calib' >&2; exit 2; fi\nif pgrep -x DEMO233_Calib >/dev/null 2>&1; then echo 'DEMO233_Calib already running'; else pkill -x DEMO233 2>/dev/null; sleep 1; nohup env LD_LIBRARY_PATH=/usr/hobot/lib:/usr/hobot/lib/sensor:/usr/lib:/lib:/lib64:${LD_LIBRARY_PATH:-} SC233_CALIBRATION_MODE=1 X5_TCP_CONTROL_ENABLE=1 X5_TCP_CONTROL_PORT=9073 X5_TCP_RING_DEPTH=96 ./DEMO233_Calib >/tmp/pangbot-calib-tool-DEMO233.log 2>&1 </dev/null & echo 'DEMO233_Calib start queued'; fi";

/// 本地驱动二进制路径：`PONGBOT_DRIVER_BINARY` 优先，其次发布包可执行文件
/// 同目录 `DEMO233_Calib`，最后开发仓库 `driver-sidecar/DEMO233_Calib`。
fn local_driver_binary_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PONGBOT_DRIVER_BINARY") {
        let candidate = PathBuf::from(&path);
        if candidate.exists() {
            return Some(candidate);
        }
        tracing::warn!("PONGBOT_DRIVER_BINARY 指向的文件不存在：{path}");
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bundled = dir.join("DEMO233_Calib");
            if bundled.exists() {
                return Some(bundled);
            }
        }
    }
    let dev = PathBuf::from("driver-sidecar/DEMO233_Calib");
    dev.exists().then_some(dev)
}

/// 二进制内容的十六进制 SHA-256（无外部 hex 依赖）。
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// TCP 探测 X5 驱动（同步、带协议握手）。
pub fn probe(host: &str, port: u16) -> Result<X5ProbeSummary, String> {
    x5_tcp_client::probe(host, port)
}

#[allow(clippy::too_many_arguments)]
pub fn bootstrap_driver(
    host: &str,
    ssh_port: u16,
    ssh_user: &str,
    ssh_password: &str,
    tcp_port: u16,
) -> Result<X5ProbeSummary, String> {
    let local_path = local_driver_binary_path();
    if let Ok(summary) = x5_tcp_client::probe(host, tcp_port) {
        if local_path.is_some() {
            let mut session = connect_password_session(host, ssh_port, ssh_user, ssh_password)?;
            let control = control()?;
            sync_driver_binary(&mut session, &control, local_path.as_deref())?;
        }
        return Ok(summary);
    }

    let mut session = connect_password_session(host, ssh_port, ssh_user, ssh_password)?;
    let control = control()?;
    sync_driver_binary(&mut session, &control, local_path.as_deref())?;
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

fn control() -> Result<RemoteOperationControl, String> {
    RemoteOperationControl::new(
        RemoteTimeouts {
            connect: Duration::from_secs(10),
            idle: Duration::from_secs(10),
            overall: Duration::from_secs(20),
        },
        DumpCancellation::default(),
    )
    .map_err(|error| format!("操作控制初始化失败：{error}"))
}

fn connect_password_session(
    host: &str,
    ssh_port: u16,
    ssh_user: &str,
    ssh_password: &str,
) -> Result<
    Box<dyn camera_toolbox_adapters::platforms::ssh_managed::connection::SshTransportSession>,
    String,
> {
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
    let control = control()?;
    transport
        .connect(&target, credential, &control)
        .map_err(|error| format!("SSH 连接 {host} 失败：{error}"))
}

/// 版本检查并同步板端标定驱动（`PONGBOT_DRIVER_BINARY` 指向本地编译产物）。
///
/// 本地 SHA-256 与板端 `DEMO233_Calib` 不一致时，删除旧文件后经 SFTP 上传替换；
/// 未设置环境变量时跳过（直接启动板端已有版本）。
fn sync_driver_binary(
    session: &mut Box<
        dyn camera_toolbox_adapters::platforms::ssh_managed::connection::SshTransportSession,
    >,
    control: &RemoteOperationControl,
    local_path: Option<&Path>,
) -> Result<(), String> {
    let Some(local_path) = local_path else {
        tracing::warn!("PONGBOT_DRIVER_BINARY 未设置，跳过标定驱动版本检查与上传");
        return Ok(());
    };
    let local_bytes = std::fs::read(local_path).map_err(|error| {
        format!(
            "读取本地标定驱动 {} 失败：{error}（PONGBOT_DRIVER_BINARY）",
            local_path.display()
        )
    })?;
    let local_sha = sha256_hex(&local_bytes);
    let argv = vec![
        "sh".to_owned(),
        "-lc".to_owned(),
        format!(
            "if [ -x {X5_233_DRIVER_REMOTE_PATH} ]; then sha256sum {X5_233_DRIVER_REMOTE_PATH} 2>/dev/null | cut -d' ' -f1; else echo __missing__; fi"
        ),
    ];

    let output = session
        .execute_argv(&argv, 4096, control)
        .map_err(|error| format!("查询板端驱动版本失败：{error}"))?;
    let remote_sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if remote_sha == local_sha {
        tracing::info!("板端标定驱动已是最新（sha256 {local_sha}）");
        return Ok(());
    }
    tracing::info!(
        "更新板端标定驱动：{remote_sha} -> {local_sha}（{} 字节）",
        local_bytes.len()
    );
    // write_file_new 排他创建，先删除旧文件；不存在时忽略删除错误。
    if session
        .remove_file(X5_233_DRIVER_REMOTE_PATH, control)
        .is_err()
    {
        // 首次部署或旧文件不存在，继续。
    }
    session
        .write_file_new(X5_233_DRIVER_REMOTE_PATH, control, &mut |writer| {
            writer.write_all(&local_bytes).map_err(|error| {
                camera_toolbox_adapters::platforms::ssh_managed::connection::SshTransportError::Transport(
                    error.to_string(),
                )
            })
        })
        .map_err(|error| format!("上传板端标定驱动失败：{error}"))?;
    // write_file_new 默认权限无执行位，补 chmod +x。
    let chmod_argv = vec![
        "sh".to_owned(),
        "-lc".to_owned(),
        format!("chmod +x {X5_233_DRIVER_REMOTE_PATH}"),
    ];
    let chmod_output = session
        .execute_argv(&chmod_argv, 4096, control)
        .map_err(|error| format!("设置板端驱动执行权限失败：{error}"))?;
    if chmod_output.exit_status.is_some_and(|status| status != 0) {
        return Err(format!(
            "设置板端驱动执行权限失败：stdout={} stderr={}",
            String::from_utf8_lossy(&chmod_output.stdout).trim(),
            String::from_utf8_lossy(&chmod_output.stderr).trim()
        ));
    }
    tracing::info!("板端标定驱动已更新");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_adapters::platforms::ssh_managed::connection::{
        SshConnectionTarget, SshTransportFactory, SshTransportSession, TransportCommandOutput,
    };
    use camera_toolbox_adapters::platforms::ssh_managed::memory_transport::{
        MemoryRemoteFile, MemorySshTransport,
    };
    use camera_toolbox_app::platform::{DumpCancellation, RemoteOperationControl, RemoteTimeouts};
    use secrecy::SecretString;
    use sha2::{Digest, Sha256};
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn control() -> RemoteOperationControl {
        RemoteOperationControl::new(
            RemoteTimeouts {
                connect: Duration::from_secs(1),
                idle: Duration::from_secs(1),
                overall: Duration::from_secs(1),
            },
            DumpCancellation::default(),
        )
        .expect("control")
    }

    fn connect_session(memory: &MemorySshTransport) -> Box<dyn SshTransportSession> {
        memory.allow_credential("session:test");
        let target = SshConnectionTarget {
            host: "board".to_owned(),
            port: 22,
            username: "root".to_owned(),
            expected_host_key: None,
            command_subsystem: None,
            remote_event_subsystem: None,
        };
        let credential = SshCredential::Password(SecretString::from("pw".to_owned()));
        memory
            .connect(&target, credential, &control())
            .expect("connect session")
    }

    fn temp_binary(name: &str, bytes: &[u8]) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pongbot-{name}-{stamp}.bin"));
        std::fs::write(&path, bytes).expect("write local");
        path
    }

    fn sha(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn sync_driver_binary_skips_upload_when_remote_matches() {
        let memory = MemorySshTransport::new("host-key");
        let local_bytes = b"demo233-calib".to_vec();
        let local = temp_binary("same", &local_bytes);
        memory.insert_file(
            X5_233_DRIVER_REMOTE_PATH,
            MemoryRemoteFile {
                bytes: local_bytes.clone(),
                stats: VecDeque::new(),
            },
        );
        memory.set_command_output(TransportCommandOutput {
            stdout: format!("{}\n", sha(&local_bytes)).into_bytes(),
            stderr: Vec::new(),
            exit_status: Some(0),
            stdout_truncated: false,
            stderr_truncated: false,
        });
        let mut session = connect_session(&memory);

        sync_driver_binary(&mut session, &control(), Some(local.as_path())).expect("sync");

        assert_eq!(
            memory.file_bytes(X5_233_DRIVER_REMOTE_PATH),
            Some(local_bytes)
        );
        let argv = memory.captured_argv();
        assert_eq!(argv.len(), 1);
        assert!(argv[0][2].contains("sha256sum"));
        let _ = std::fs::remove_file(local);
    }

    #[test]
    fn sync_driver_binary_replaces_mismatched_remote_file() {
        let memory = MemorySshTransport::new("host-key");
        let local = temp_binary("mismatch", b"local-demo233");
        memory.insert_file(
            X5_233_DRIVER_REMOTE_PATH,
            MemoryRemoteFile {
                bytes: b"old-demo233".to_vec(),
                stats: VecDeque::new(),
            },
        );
        memory.set_command_output(TransportCommandOutput {
            stdout: b"\n".to_vec(),
            stderr: Vec::new(),
            exit_status: Some(0),
            stdout_truncated: false,
            stderr_truncated: false,
        });
        let mut session = connect_session(&memory);

        sync_driver_binary(&mut session, &control(), Some(local.as_path())).expect("sync");

        assert_eq!(
            memory.file_bytes(X5_233_DRIVER_REMOTE_PATH),
            Some(b"local-demo233".to_vec())
        );
        let argv = memory.captured_argv();
        assert!(argv.iter().any(|cmd| cmd[2].contains("sha256sum")));
        assert!(argv.iter().any(|cmd| cmd[2].contains("chmod +x")));
        let _ = std::fs::remove_file(local);
    }
}
