//! 远端候选路径约束与稳定性检测。

use std::{thread, time::Duration};

use camera_toolbox_app::{RemoteFileStat, RemoteOperationControl, RemoteServiceError, RemoteStage};

use super::command::path_is_within_root;
use super::connection::{SshTransportError, SshTransportSession};

pub(crate) fn validate_candidate_path(
    path: &str,
    root: &str,
    glob: &str,
) -> Result<(), RemoteServiceError> {
    let basename = path.rsplit('/').next().unwrap_or_default();
    if !path_is_within_root(path, root)
        || path.trim_end_matches('/') == root.trim_end_matches('/')
        || basename.is_empty()
        || !glob_matches(glob.as_bytes(), basename.as_bytes())
    {
        return Err(RemoteServiceError::PathNotAllowed {
            path: path.to_owned(),
        });
    }
    Ok(())
}

/// `*`/`?` 只作用于 basename；profile 校验已禁止 `/`。
fn glob_matches(pattern: &[u8], value: &[u8]) -> bool {
    let mut previous = vec![false; value.len().saturating_add(1)];
    previous[0] = true;
    for token in pattern {
        let mut current = vec![false; value.len().saturating_add(1)];
        match token {
            b'*' => {
                current[0] = previous[0];
                for index in 1..current.len() {
                    current[index] = previous[index] || current[index - 1];
                }
            }
            b'?' => current[1..].copy_from_slice(&previous[..value.len()]),
            literal => {
                for index in 1..current.len() {
                    current[index] = previous[index - 1] && value[index - 1] == *literal;
                }
            }
        }
        previous = current;
    }
    previous[value.len()]
}

pub(crate) fn wait_for_stable(
    session: &mut dyn SshTransportSession,
    path: &str,
    stable_samples: u8,
    interval: Duration,
    control: &RemoteOperationControl,
) -> Result<RemoteFileStat, RemoteServiceError> {
    control.report(RemoteStage::WaitingStable);
    let mut previous = session
        .stat(path, control)
        .map_err(|error| map_transport(error, RemoteStage::WaitingStable))?;
    if previous
        .producer_marker
        .as_ref()
        .is_some_and(|marker| !marker.is_empty())
    {
        return Ok(previous);
    }
    let mut stable = 1_u8;
    while stable < stable_samples {
        wait_cancellable(interval, control)?;
        let current = session
            .stat(path, control)
            .map_err(|error| map_transport(error, RemoteStage::WaitingStable))?;
        if current.size < previous.size {
            return Err(RemoteServiceError::FileTruncatedBeforeStable {
                path: path.to_owned(),
                previous: previous.size,
                current: current.size,
            });
        }
        if current.size == previous.size && current.modified_seconds == previous.modified_seconds {
            stable = stable.saturating_add(1);
        } else {
            stable = 1;
        }
        previous = current;
    }
    Ok(previous)
}

pub(crate) fn wait_cancellable(
    interval: Duration,
    control: &RemoteOperationControl,
) -> Result<(), RemoteServiceError> {
    let mut remaining = interval;
    while !remaining.is_zero() {
        if control.cancellation.is_cancelled() {
            return Err(RemoteServiceError::Cancelled {
                stage: RemoteStage::WaitingStable,
            });
        }
        if control.deadline_expired() {
            return Err(RemoteServiceError::DeadlineExceeded {
                stage: RemoteStage::WaitingStable,
            });
        }
        let step = remaining
            .min(Duration::from_millis(20))
            .min(control.remaining_overall());
        if step.is_zero() {
            return Err(RemoteServiceError::DeadlineExceeded {
                stage: RemoteStage::WaitingStable,
            });
        }
        thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
    Ok(())
}

pub(crate) fn map_transport(error: SshTransportError, stage: RemoteStage) -> RemoteServiceError {
    match error {
        SshTransportError::HostKeyMismatch => RemoteServiceError::HostKeyMismatch,
        SshTransportError::AuthenticationFailed => RemoteServiceError::AuthenticationFailed,
        SshTransportError::Cancelled => RemoteServiceError::Cancelled { stage },
        SshTransportError::TimedOut => RemoteServiceError::DeadlineExceeded { stage },
        SshTransportError::Transport(reason) => RemoteServiceError::Transport { stage, reason },
        SshTransportError::HelperProtocol(reason) => RemoteServiceError::HelperProtocol(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_glob_and_root_are_both_enforced() {
        assert!(validate_candidate_path("/data/cap/a.raw", "/data/cap", "*.raw").is_ok());
        assert!(validate_candidate_path("/data/cap/a.jpg", "/data/cap", "*.raw").is_err());
        assert!(validate_candidate_path("/data/cap2/a.raw", "/data/cap", "*.raw").is_err());
        assert!(validate_candidate_path("/data/cap/../a.raw", "/data/cap", "*.raw").is_err());
    }
}
