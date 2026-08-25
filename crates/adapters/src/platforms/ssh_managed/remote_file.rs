//! SFTP stat/fetch 与三级 discovery orchestration。

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use camera_toolbox_app::{
    CaptureStore, OperationId, RemoteDiscoveryRequest, RemoteDiscoverySource,
    RemoteFileOperationResult, RemoteFileRequest, RemoteFileService, RemoteFileStat,
    RemoteOpenDisposition, RemoteOperationControl, RemoteServiceError, RemoteStage,
    RemoteWatchEvent, RemoteWatchReporter, RemoteWatchRequest, RemoteWatchTerminal,
};
use camera_toolbox_core::{
    AssetId, CaptureMetadata, EphemeralAsset, IntegrityState, OwnedMediaPayload,
};
use sha2::{Digest, Sha256};

use super::{
    connection::{
        CredentialResolver, SshConnectionTarget, SshTransportError, SshTransportFactory,
        SshTransportSession,
    },
    watcher::{map_transport, validate_candidate_path, wait_cancellable, wait_for_stable},
};

const MAX_WATCH_DEDUP_KEYS: usize = 4096;

pub struct SshRemoteFileService {
    service_id: String,
    target: SshConnectionTarget,
    credential_ref: String,
    resolver: Arc<dyn CredentialResolver>,
    transport: Arc<dyn SshTransportFactory>,
    remote_root: String,
    remote_glob: String,
    stable_samples: u8,
    stability_interval: Duration,
    passive_watch_auto_open: bool,
    max_fetch_bytes: u64,
}

impl SshRemoteFileService {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        service_id: String,
        target: SshConnectionTarget,
        credential_ref: String,
        resolver: Arc<dyn CredentialResolver>,
        transport: Arc<dyn SshTransportFactory>,
        remote_root: String,
        remote_glob: String,
        stable_samples: u8,
        stability_interval: Duration,
        passive_watch_auto_open: bool,
        max_fetch_bytes: u64,
    ) -> Self {
        Self {
            service_id,
            target,
            credential_ref,
            resolver,
            transport,
            remote_root,
            remote_glob,
            stable_samples,
            stability_interval,
            passive_watch_auto_open,
            max_fetch_bytes,
        }
    }

    fn connect(
        &self,
        control: &RemoteOperationControl,
    ) -> Result<Box<dyn SshTransportSession>, RemoteServiceError> {
        check_control(control, RemoteStage::ResolvingCredential)?;
        control.report(RemoteStage::ResolvingCredential);
        let credential = self
            .resolver
            .resolve(&self.credential_ref)
            .map_err(RemoteServiceError::CredentialResolution)?;
        check_control(control, RemoteStage::Connecting)?;
        control.report(RemoteStage::Connecting);
        self.transport
            .connect(&self.target, credential, control)
            .map_err(|error| map_transport(error, RemoteStage::Connecting))
    }

    fn canonical_candidate(
        &self,
        session: &mut dyn SshTransportSession,
        path: &str,
        control: &RemoteOperationControl,
    ) -> Result<String, RemoteServiceError> {
        validate_candidate_path(path, &self.remote_root, &self.remote_glob)?;
        let canonical_root = session
            .canonicalize(&self.remote_root, control)
            .map_err(|error| map_transport(error, RemoteStage::Discovering))?;
        let canonical_path = session
            .canonicalize(path, control)
            .map_err(|error| map_transport(error, RemoteStage::Discovering))?;
        validate_candidate_path(&canonical_path, &canonical_root, &self.remote_glob)?;
        Ok(canonical_path)
    }

    #[allow(clippy::too_many_lines)]
    fn fetch_with_session(
        &self,
        session: &mut dyn SshTransportSession,
        operation_id: OperationId,
        mut request: RemoteFileRequest,
        discovery_source: RemoteDiscoverySource,
        control: &RemoteOperationControl,
        store: &CaptureStore,
    ) -> Result<RemoteFileOperationResult, RemoteServiceError> {
        request.validate()?;
        request.remote_path = self.canonical_candidate(session, &request.remote_path, control)?;
        let stable = wait_for_stable(
            session,
            &request.remote_path,
            self.stable_samples,
            self.stability_interval,
            control,
        )?;
        if stable.size == 0 {
            return Err(RemoteServiceError::InvalidRequest(
                "empty remote artifacts cannot be published".to_owned(),
            ));
        }
        if stable.size > self.max_fetch_bytes {
            return Err(RemoteServiceError::PayloadTooLarge {
                declared: stable.size,
                limit: self.max_fetch_bytes,
            });
        }
        let declared = usize::try_from(stable.size).map_err(|_| {
            RemoteServiceError::PayloadLengthUnsupported {
                declared: stable.size,
            }
        })?;
        control.report(RemoteStage::Reserving);
        let reservation = store.reserve(operation_id, declared)?;
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(declared)
            .map_err(|_| RemoteServiceError::AllocationFailed { bytes: declared })?;
        let mut hasher = Sha256::new();
        let mut overrun = None;
        control.report(RemoteStage::Fetching);
        let read_result = session.read_file(&request.remote_path, control, &mut |chunk| {
            if control.cancellation.is_cancelled() {
                return Err(SshTransportError::Cancelled);
            }
            if control.deadline_expired() {
                return Err(SshTransportError::TimedOut);
            }
            let next = payload.len().checked_add(chunk.len()).ok_or_else(|| {
                SshTransportError::Transport("payload length overflow".to_owned())
            })?;
            if next > declared {
                overrun = Some(next as u64);
                return Err(SshTransportError::HelperProtocol(
                    "remote file exceeded preflight size".to_owned(),
                ));
            }
            payload.extend_from_slice(chunk);
            hasher.update(chunk);
            Ok(())
        });
        if let Some(received) = overrun {
            return Err(RemoteServiceError::GrewDuringFetch {
                expected: stable.size,
                received,
            });
        }
        let reported = read_result.map_err(|error| map_transport(error, RemoteStage::Fetching))?;
        let received = u64::try_from(payload.len()).map_err(|_| {
            RemoteServiceError::PayloadLengthUnsupported {
                declared: stable.size,
            }
        })?;
        if reported != received || received < stable.size {
            return Err(RemoteServiceError::Truncated {
                expected: stable.size,
                received: received.min(reported),
            });
        }
        if received > stable.size {
            return Err(RemoteServiceError::GrewDuringFetch {
                expected: stable.size,
                received,
            });
        }

        control.report(RemoteStage::Verifying);
        let calculated = format!("{:x}", hasher.finalize());
        let expected = request.expected_sha256.as_ref().or(stable.sha256.as_ref());
        if let Some(expected) = expected
            && !expected.eq_ignore_ascii_case(&calculated)
        {
            return Err(RemoteServiceError::HashMismatch {
                expected: expected.clone(),
                calculated,
            });
        }
        let post = session
            .stat(&request.remote_path, control)
            .map_err(|error| map_transport(error, RemoteStage::Verifying))?;
        if post.size != stable.size || post.modified_seconds != stable.modified_seconds {
            return Err(RemoteServiceError::ChangedDuringFetch);
        }

        let mut attributes = BTreeMap::new();
        attributes.insert("remote_path".to_owned(), request.remote_path.clone());
        attributes.insert(
            "remote_modified_seconds".to_owned(),
            stable.modified_seconds.to_string(),
        );
        attributes.insert(
            "remote_discovery".to_owned(),
            match discovery_source {
                RemoteDiscoverySource::CommandResultPath => "command_result_path",
                RemoteDiscoverySource::RemoteEventWatch => "remote_event_watch",
                RemoteDiscoverySource::DirectoryPolling => "directory_polling",
                RemoteDiscoverySource::ExplicitPath => "explicit_path",
            }
            .to_owned(),
        );
        let asset = EphemeralAsset::new(
            request.asset_id,
            OwnedMediaPayload::from_bytes(payload),
            CaptureMetadata {
                format: request.format,
                source_name: request.source_name,
                attributes,
            },
            IntegrityState::Verified {
                algorithm: "sha256".to_owned(),
                digest: calculated.clone(),
            },
        );
        control.report(RemoteStage::Publishing);
        let asset = store.publish_validated(reservation, asset)?;
        Ok(RemoteFileOperationResult {
            asset,
            remote_stat: post,
            payload_sha256: calculated,
            discovery_source,
        })
    }
}

impl RemoteFileService for SshRemoteFileService {
    fn service_id(&self) -> &str {
        &self.service_id
    }

    fn stat(
        &self,
        remote_path: &str,
        control: RemoteOperationControl,
    ) -> Result<RemoteFileStat, RemoteServiceError> {
        validate_candidate_path(remote_path, &self.remote_root, &self.remote_glob)?;
        let mut session = self.connect(&control)?;
        let _interrupt = InterruptRegistration(&control);
        control.report(RemoteStage::Discovering);
        let canonical = self.canonical_candidate(session.as_mut(), remote_path, &control)?;
        session
            .stat(&canonical, &control)
            .map_err(|error| map_transport(error, RemoteStage::Discovering))
    }

    fn fetch(
        &self,
        operation_id: OperationId,
        request: RemoteFileRequest,
        control: RemoteOperationControl,
        store: &CaptureStore,
    ) -> Result<RemoteFileOperationResult, RemoteServiceError> {
        request.validate()?;
        validate_candidate_path(&request.remote_path, &self.remote_root, &self.remote_glob)?;
        let mut session = self.connect(&control)?;
        let _interrupt = InterruptRegistration(&control);
        self.fetch_with_session(
            session.as_mut(),
            operation_id,
            request,
            RemoteDiscoverySource::ExplicitPath,
            &control,
            store,
        )
    }

    fn discover_and_fetch(
        &self,
        operation_id: OperationId,
        request: RemoteDiscoveryRequest,
        control: RemoteOperationControl,
        store: &CaptureStore,
    ) -> Result<Vec<RemoteFileOperationResult>, RemoteServiceError> {
        if request.asset_id_prefix.trim().is_empty() {
            return Err(RemoteServiceError::InvalidRequest(
                "asset id prefix must not be empty".to_owned(),
            ));
        }
        let mut session = self.connect(&control)?;
        let _interrupt = InterruptRegistration(&control);
        control.report(RemoteStage::Discovering);
        let (mut paths, source) = if let Some(path) = request.command_result_path {
            (vec![path], RemoteDiscoverySource::CommandResultPath)
        } else {
            discover_passive_paths(
                session.as_mut(),
                self.target.remote_event_subsystem.is_some(),
                &self.remote_root,
                &self.remote_glob,
                &control,
            )?
        };
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            return Err(RemoteServiceError::NoCandidate);
        }
        for path in &paths {
            validate_candidate_path(path, &self.remote_root, &self.remote_glob)?;
        }

        let mut results = Vec::with_capacity(paths.len());
        for (index, path) in paths.into_iter().enumerate() {
            check_control(&control, RemoteStage::Discovering)?;
            let asset_id = AssetId::new(format!("{}-{index}", request.asset_id_prefix))
                .map_err(|error| RemoteServiceError::InvalidRequest(error.to_string()))?;
            let source_name = path.rsplit('/').next().unwrap_or(&path).to_owned();
            results.push(self.fetch_with_session(
                session.as_mut(),
                operation_id.clone(),
                RemoteFileRequest {
                    asset_id,
                    remote_path: path,
                    expected_sha256: None,
                    format: request.format.clone(),
                    source_name,
                },
                source,
                &control,
                store,
            )?);
        }
        Ok(results)
    }

    #[allow(clippy::too_many_lines)]
    fn watch(
        &self,
        operation_id: OperationId,
        request: RemoteWatchRequest,
        control: RemoteOperationControl,
        store: &CaptureStore,
        reporter: RemoteWatchReporter,
    ) -> Result<RemoteWatchTerminal, RemoteServiceError> {
        if request.asset_id_prefix.trim().is_empty() {
            return Err(RemoteServiceError::InvalidRequest(
                "watch asset id prefix must not be empty".to_owned(),
            ));
        }
        let mut session = self.connect(&control)?;
        let _interrupt = InterruptRegistration(&control);
        let mut seen = BTreeSet::new();
        let mut rejected_paths = BTreeSet::new();
        let mut sequence = 0_u64;
        loop {
            if control.cancellation.is_cancelled() {
                reporter(RemoteWatchEvent::Terminal(RemoteWatchTerminal::Cancelled));
                return Ok(RemoteWatchTerminal::Cancelled);
            }
            if control.deadline_expired() {
                reporter(RemoteWatchEvent::Terminal(
                    RemoteWatchTerminal::DeadlineReached,
                ));
                return Ok(RemoteWatchTerminal::DeadlineReached);
            }
            control.report(RemoteStage::Discovering);
            let (paths, source) = discover_passive_paths(
                session.as_mut(),
                self.target.remote_event_subsystem.is_some(),
                &self.remote_root,
                &self.remote_glob,
                &control,
            )?;
            for path in paths {
                if rejected_paths.contains(&path) {
                    continue;
                }
                let canonical = match self.canonical_candidate(session.as_mut(), &path, &control) {
                    Ok(path) => path,
                    Err(error) => {
                        insert_bounded(&mut rejected_paths, path.clone());
                        reporter(RemoteWatchEvent::CandidateFailed { path, error });
                        continue;
                    }
                };
                let observed = match session.stat(&canonical, &control) {
                    Ok(stat) => stat,
                    Err(error) => {
                        insert_bounded(&mut rejected_paths, path.clone());
                        reporter(RemoteWatchEvent::CandidateFailed {
                            path,
                            error: map_transport(error, RemoteStage::Discovering),
                        });
                        continue;
                    }
                };
                let key = (canonical.clone(), observed.size, observed.modified_seconds);
                if seen.contains(&key) {
                    continue;
                }
                let asset_id = AssetId::new(format!("{}-{sequence}", request.asset_id_prefix))
                    .map_err(|error| RemoteServiceError::InvalidRequest(error.to_string()))?;
                sequence = sequence.checked_add(1).ok_or_else(|| {
                    RemoteServiceError::InvalidRequest("watch sequence exhausted".to_owned())
                })?;
                let source_name = path.rsplit('/').next().unwrap_or(&path).to_owned();
                match self.fetch_with_session(
                    session.as_mut(),
                    operation_id.clone(),
                    RemoteFileRequest {
                        asset_id,
                        remote_path: path.clone(),
                        expected_sha256: observed.sha256,
                        format: request.format.clone(),
                        source_name,
                    },
                    source,
                    &control,
                    store,
                ) {
                    Ok(result) => {
                        insert_bounded(&mut seen, key);
                        reporter(RemoteWatchEvent::AssetReady {
                            result,
                            open: if self.passive_watch_auto_open {
                                RemoteOpenDisposition::Background
                            } else {
                                RemoteOpenDisposition::None
                            },
                        });
                    }
                    Err(RemoteServiceError::Cancelled { .. }) => {
                        reporter(RemoteWatchEvent::Terminal(RemoteWatchTerminal::Cancelled));
                        return Ok(RemoteWatchTerminal::Cancelled);
                    }
                    Err(RemoteServiceError::DeadlineExceeded { .. }) => {
                        reporter(RemoteWatchEvent::Terminal(
                            RemoteWatchTerminal::DeadlineReached,
                        ));
                        return Ok(RemoteWatchTerminal::DeadlineReached);
                    }
                    Err(error) => {
                        insert_bounded(&mut seen, key);
                        reporter(RemoteWatchEvent::CandidateFailed { path, error });
                    }
                }
            }
            match wait_cancellable(self.stability_interval, &control) {
                Ok(()) => {}
                Err(RemoteServiceError::Cancelled { .. }) => {
                    reporter(RemoteWatchEvent::Terminal(RemoteWatchTerminal::Cancelled));
                    return Ok(RemoteWatchTerminal::Cancelled);
                }
                Err(RemoteServiceError::DeadlineExceeded { .. }) => {
                    reporter(RemoteWatchEvent::Terminal(
                        RemoteWatchTerminal::DeadlineReached,
                    ));
                    return Ok(RemoteWatchTerminal::DeadlineReached);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn discover_passive_paths(
    session: &mut dyn SshTransportSession,
    event_helper_configured: bool,
    root: &str,
    glob: &str,
    control: &RemoteOperationControl,
) -> Result<(Vec<String>, RemoteDiscoverySource), RemoteServiceError> {
    if event_helper_configured {
        match session.event_paths(root, glob, control) {
            Ok(paths) if !paths.is_empty() => {
                return Ok((paths, RemoteDiscoverySource::RemoteEventWatch));
            }
            Ok(_) | Err(SshTransportError::Transport(_) | SshTransportError::HelperProtocol(_)) => {
            }
            Err(error) => {
                return Err(map_transport(error, RemoteStage::Discovering));
            }
        }
    }
    Ok((
        poll_paths(session, root, glob, control)?,
        RemoteDiscoverySource::DirectoryPolling,
    ))
}

fn poll_paths(
    session: &mut dyn SshTransportSession,
    root: &str,
    glob: &str,
    control: &RemoteOperationControl,
) -> Result<Vec<String>, RemoteServiceError> {
    let entries = session
        .read_dir(root, control)
        .map_err(|error| map_transport(error, RemoteStage::Discovering))?;
    let mut paths = entries
        .into_iter()
        .filter_map(|entry| {
            validate_candidate_path(&entry.path, root, glob)
                .ok()
                .map(|()| entry.path)
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn insert_bounded<T: Ord>(set: &mut BTreeSet<T>, value: T) {
    if set.len() == MAX_WATCH_DEDUP_KEYS {
        // 有界 epoch：清空后允许仍存在的候选重新验证，但 RSS 不随 watcher 生命周期增长。
        set.clear();
    }
    set.insert(value);
}

fn check_control(
    control: &RemoteOperationControl,
    stage: RemoteStage,
) -> Result<(), RemoteServiceError> {
    if control.cancellation.is_cancelled() {
        Err(RemoteServiceError::Cancelled { stage })
    } else if control.deadline_expired() {
        Err(RemoteServiceError::DeadlineExceeded { stage })
    } else {
        Ok(())
    }
}

struct InterruptRegistration<'a>(&'a RemoteOperationControl);

impl Drop for InterruptRegistration<'_> {
    fn drop(&mut self) {
        self.0.cancellation.clear_interrupt();
    }
}
