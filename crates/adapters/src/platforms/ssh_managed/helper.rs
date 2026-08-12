//! SSH-managed 平台复用的目标侧 helper 安装逻辑。

use std::{
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use camera_toolbox_app::{DumpCancellation, RemoteOperationControl, RemoteTimeouts};

use super::connection::{SshTransportError, SshTransportSession, TransportFileKind};
use sha2::{Digest, Sha256};

pub(super) const HELPER_PROGRAM: &str = "/usr/local/libexec/camera-i2c-helper";
pub(super) const HELPER_INSTALL_PROGRAM: &str = "/bin/chmod";
const HELPER_HASH_PROGRAM: &str = "/usr/bin/sha256sum";
pub(super) const HELPER_INSTALL_OUTPUT_LIMIT: usize = 4096;
const HELPER_HASH_OUTPUT_LIMIT: usize = 256;

static HELPER_UPLOAD_COUNTER: AtomicU64 = AtomicU64::new(0);

const HELPER_STAGING_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn install_helper(
    session: &mut dyn SshTransportSession,
    helper_payload: &[u8],
    control: &RemoteOperationControl,
    label: &str,
) -> Result<(), String> {
    ensure_remote_dir(session, "/usr/local/libexec", control, label)?;
    if remote_helper_is_current(session, helper_payload, control) {
        return chmod_helper(session, control, label);
    }

    let staging_path = helper_staging_path();
    match session.write_file_new(&staging_path, control, &mut |writer| {
        std::io::Write::write_all(writer, helper_payload).map_err(|error| {
            SshTransportError::Transport(format!("{label} helper upload write failed: {error}"))
        })
    }) {
        Ok(()) => {}
        Err(error) => {
            cleanup_staging_file(session, &staging_path, control);
            return Err(format!("{label} helper upload failed: {error}"));
        }
    }

    match session.remove_file(HELPER_PROGRAM, control) {
        Ok(()) | Err(SshTransportError::NotFound(_)) => {}
        Err(error) => {
            cleanup_staging_file(session, &staging_path, control);
            return Err(format!("{label} helper remove failed: {error}"));
        }
    }

    match session.rename(&staging_path, HELPER_PROGRAM, control) {
        Ok(()) => {}
        Err(error) => {
            cleanup_staging_file(session, &staging_path, control);
            return Err(format!("{label} helper publish failed: {error}"));
        }
    }

    chmod_helper(session, control, label)
}

fn chmod_helper(
    session: &mut dyn SshTransportSession,
    control: &RemoteOperationControl,
    label: &str,
) -> Result<(), String> {
    let output = session
        .execute_argv(
            &[
                HELPER_INSTALL_PROGRAM.to_owned(),
                "755".to_owned(),
                HELPER_PROGRAM.to_owned(),
            ],
            HELPER_INSTALL_OUTPUT_LIMIT,
            control,
        )
        .map_err(|error| format!("{label} helper chmod command failed: {error}"))?;
    if output.exit_status == Some(0) {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(format!(
        "{label} helper chmod failed: exit={:?}, stderr={}, stdout={}",
        output.exit_status,
        stderr.trim(),
        stdout.trim()
    ))
}

fn remote_helper_is_current(
    session: &mut dyn SshTransportSession,
    helper_payload: &[u8],
    control: &RemoteOperationControl,
) -> bool {
    let Ok(entry) = session.metadata(HELPER_PROGRAM, control) else {
        return false;
    };
    if !matches!(entry.kind, TransportFileKind::File)
        || u64::try_from(helper_payload.len()).ok() != Some(entry.size)
    {
        return false;
    }
    let Ok(output) = session.execute_argv(
        &[HELPER_HASH_PROGRAM.to_owned(), HELPER_PROGRAM.to_owned()],
        HELPER_HASH_OUTPUT_LIMIT,
        control,
    ) else {
        return false;
    };
    if output.exit_status != Some(0) || output.stdout_truncated {
        return false;
    }
    let Some(remote_hash) = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
    else {
        return false;
    };
    remote_hash.eq_ignore_ascii_case(&helper_payload_sha256(helper_payload))
}

fn helper_payload_sha256(helper_payload: &[u8]) -> String {
    let digest = Sha256::digest(helper_payload);
    format!("{digest:x}")
}
fn cleanup_staging_file(
    session: &mut dyn SshTransportSession,
    staging_path: &str,
    control: &RemoteOperationControl,
) {
    // cleanup 使用独立短 deadline；即使原 operation 已超时或被取消，也只尝试删除唯一 staging 文件。
    let cleanup_control = staging_cleanup_control(control);
    match session.remove_file(staging_path, &cleanup_control) {
        Ok(()) | Err(SshTransportError::NotFound(_)) => {}
        Err(_) => {}
    }
}

fn staging_cleanup_control(control: &RemoteOperationControl) -> RemoteOperationControl {
    let bounded = control
        .remaining_overall()
        .min(control.timeouts.idle)
        .min(HELPER_STAGING_CLEANUP_TIMEOUT);
    let timeout = if bounded.is_zero() {
        HELPER_STAGING_CLEANUP_TIMEOUT
    } else {
        bounded
    };
    RemoteOperationControl::new(
        RemoteTimeouts {
            connect: timeout,
            idle: timeout,
            overall: timeout,
        },
        DumpCancellation::default(),
    )
    .expect("non-zero staging cleanup timeouts are valid")
}

fn helper_staging_path() -> String {
    let upload_id = HELPER_UPLOAD_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{HELPER_PROGRAM}.upload-{pid}-{upload_id:016x}",
        pid = process::id(),
    )
}

fn ensure_remote_dir(
    session: &mut dyn SshTransportSession,
    path: &str,
    control: &RemoteOperationControl,
    label: &str,
) -> Result<(), String> {
    if path.is_empty() || path == "/" {
        return Ok(());
    }

    match session.metadata(path, control) {
        Ok(entry) if matches!(entry.kind, TransportFileKind::Directory) => return Ok(()),
        Ok(_) => {
            return Err(format!(
                "{label} helper directory setup failed: remote path {path} already exists and is not a directory"
            ));
        }
        Err(SshTransportError::NotFound(_)) => {}
        Err(error) => return Err(format!("{label} helper directory setup failed: {error}")),
    }

    match session.mkdir(path, control) {
        Ok(()) | Err(SshTransportError::AlreadyExists(_)) => Ok(()),
        Err(SshTransportError::NotFound(_)) => {
            let Some(parent) = parent_dir(path) else {
                return Err(format!(
                    "{label} helper upload failed: remote directory {path} has no parent"
                ));
            };
            ensure_remote_dir(session, parent, control, label)?;
            match session.mkdir(path, control) {
                Ok(()) | Err(SshTransportError::AlreadyExists(_)) => Ok(()),
                Err(error) => Err(format!("{label} helper directory setup failed: {error}")),
            }
        }
        Err(error) => Err(format!("{label} helper directory setup failed: {error}")),
    }
}

fn parent_dir(path: &str) -> Option<&str> {
    let (parent, _) = path.rsplit_once('/')?;
    if parent.is_empty() {
        Some("/")
    } else {
        Some(parent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use camera_toolbox_app::{DumpCancellation, RemoteFileStat};
    use secrecy::SecretString;

    use super::super::{
        connection::{
            SshConnectionTarget, SshCredential, SshTransportFactory, SshTransportSession,
            TransportCommandOutput,
        },
        memory_transport::{MemoryRemoteFile, MemorySshTransport},
    };

    const HELPER_PAYLOAD: &[u8] = b"test-helper-binary";

    fn target() -> SshConnectionTarget {
        SshConnectionTarget {
            host: "camera.local".to_owned(),
            port: 22,
            username: "root".to_owned(),
            expected_host_key: None,
            command_subsystem: None,
            remote_event_subsystem: None,
        }
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

    fn connect_session(
        memory: &MemorySshTransport,
        control: &RemoteOperationControl,
    ) -> Box<dyn SshTransportSession> {
        SshTransportFactory::connect(
            memory,
            &target(),
            SshCredential::Password(SecretString::from("memory-test-secret")),
            control,
        )
        .unwrap()
    }

    fn remote_file(bytes: impl Into<Vec<u8>>) -> MemoryRemoteFile {
        let bytes = bytes.into();
        let size = u64::try_from(bytes.len()).unwrap();
        MemoryRemoteFile {
            bytes,
            stats: VecDeque::from([RemoteFileStat {
                path: HELPER_PROGRAM.to_owned(),
                size,
                modified_seconds: 0,
                producer_marker: None,
                sha256: None,
            }]),
        }
    }

    fn staging_paths(memory: &MemorySshTransport) -> Vec<String> {
        memory.file_paths_with_prefix(&format!("{HELPER_PROGRAM}.upload-"))
    }

    fn successful_chmod_output() -> TransportCommandOutput {
        TransportCommandOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_status: Some(0),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    fn helper_hash_output() -> TransportCommandOutput {
        TransportCommandOutput {
            stdout: format!(
                "{}  {HELPER_PROGRAM}\n",
                helper_payload_sha256(HELPER_PAYLOAD)
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_status: Some(0),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[test]
    fn publish_success_leaves_only_final_helper() {
        let memory = MemorySshTransport::new("host-key");
        memory.set_command_output(successful_chmod_output());
        let control = control();
        let mut session = connect_session(&memory, &control);

        install_helper(&mut *session, HELPER_PAYLOAD, &control, "I2C").unwrap();

        assert_eq!(
            memory.file_bytes(HELPER_PROGRAM),
            Some(HELPER_PAYLOAD.to_vec())
        );
        assert!(staging_paths(&memory).is_empty());
        assert_eq!(
            memory.captured_argv(),
            vec![vec![
                HELPER_INSTALL_PROGRAM.to_owned(),
                "755".to_owned(),
                HELPER_PROGRAM.to_owned(),
            ]]
        );
    }

    #[test]
    fn current_remote_helper_skips_upload_and_only_chmods() {
        let memory = MemorySshTransport::new("host-key");
        memory.insert_file(HELPER_PROGRAM, remote_file(HELPER_PAYLOAD));
        memory.set_command_output(helper_hash_output());
        memory.fail_next_write_file_new_after_create(SshTransportError::TimedOut);
        let control = control();
        let mut session = connect_session(&memory, &control);

        install_helper(&mut *session, HELPER_PAYLOAD, &control, "I2C").unwrap();

        assert_eq!(
            memory.file_bytes(HELPER_PROGRAM),
            Some(HELPER_PAYLOAD.to_vec())
        );
        assert!(staging_paths(&memory).is_empty());
        assert_eq!(
            memory.captured_argv(),
            vec![
                vec![HELPER_HASH_PROGRAM.to_owned(), HELPER_PROGRAM.to_owned()],
                vec![
                    HELPER_INSTALL_PROGRAM.to_owned(),
                    "755".to_owned(),
                    HELPER_PROGRAM.to_owned(),
                ],
            ]
        );
    }

    #[test]
    fn upload_failure_removes_staging_file() {
        let memory = MemorySshTransport::new("host-key");
        memory.fail_next_write_file_new_after_create(SshTransportError::TimedOut);
        let control = control();
        let mut session = connect_session(&memory, &control);

        let error = install_helper(&mut *session, HELPER_PAYLOAD, &control, "I2C").unwrap_err();

        assert!(error.contains("I2C helper upload failed"));
        assert!(staging_paths(&memory).is_empty());
        assert!(memory.file_bytes(HELPER_PROGRAM).is_none());
        assert!(memory.captured_argv().is_empty());
    }

    #[test]
    fn remove_failure_removes_staging_file_and_preserves_old_helper() {
        let memory = MemorySshTransport::new("host-key");
        let old_helper = b"old-helper".to_vec();
        memory.insert_file(HELPER_PROGRAM, remote_file(old_helper.clone()));
        memory.fail_remove_file(
            HELPER_PROGRAM,
            SshTransportError::Transport("old helper is busy".to_owned()),
        );
        let control = control();
        let mut session = connect_session(&memory, &control);

        let error = install_helper(&mut *session, HELPER_PAYLOAD, &control, "I2C").unwrap_err();

        assert!(error.contains("I2C helper remove failed"));
        assert_eq!(memory.file_bytes(HELPER_PROGRAM), Some(old_helper));
        assert!(staging_paths(&memory).is_empty());
        assert!(memory.captured_argv().is_empty());
    }

    #[test]
    fn rename_failure_removes_staging_file() {
        let memory = MemorySshTransport::new("host-key");
        memory.fail_next_rename(SshTransportError::Transport("rename refused".to_owned()));
        let control = control();
        let mut session = connect_session(&memory, &control);

        let error = install_helper(&mut *session, HELPER_PAYLOAD, &control, "I2C").unwrap_err();

        assert!(error.contains("I2C helper publish failed"));
        assert!(memory.file_bytes(HELPER_PROGRAM).is_none());
        assert!(staging_paths(&memory).is_empty());
        assert!(memory.captured_argv().is_empty());
    }
}
