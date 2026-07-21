#[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
    thread,
    time::{Duration, Instant},
};
use std::{path::PathBuf, sync::Arc};

use crate::json::{Value, json};
use anyhow::{Context, Result, anyhow, bail};
use camera_toolbox_adapters::PlatformRegistry;
#[cfg(feature = "platform-cv610")]
use camera_toolbox_adapters::platforms::hisilicon_cv610::HisiliconCv610Provider;
#[cfg(feature = "platform-ssh")]
use camera_toolbox_adapters::platforms::ssh_managed::{
    CredentialResolver, ProductionCredentialResolver, SshManagedPlatformProvider,
    production_recipe_registry_from_env,
};
use camera_toolbox_app::{
    CapabilityKind, CapabilityVariant, EvidenceMaturity, PlatformConfig, PlatformKind,
    PlatformProfile, PlatformProfileId, ProfileStore, SensorId, SensorModeKey, SensorSelection,
    SnapshotHash, TargetResolutionSnapshot,
};
#[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
use camera_toolbox_app::{CaptureStore, CaptureStoreLimits, OperationId, PlatformController};
#[cfg(feature = "platform-ssh")]
use camera_toolbox_app::{
    CommandParameter, CommandResult, CommandTerminal, RemoteDiscoverySource, RemoteFileRequest,
    RemoteJobEvent, RemoteJobFailure, RemoteJobState, RemoteTimeouts, TypedCommandRequest,
};
#[cfg(feature = "platform-cv610")]
use camera_toolbox_app::{
    DumpJobEvent, DumpJobState, DumpTimeouts, RecordingBranch, RecordingState,
    ResolvedTargetBindings, StreamMetrics, StreamOpenRequest, StreamRecordingRequest,
    StreamServiceEvent, StreamSessionEvent, StreamSessionId, StreamTerminal, StreamTimeouts,
    VerifiedDumpKind, VerifiedDumpRequest,
};
#[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
use camera_toolbox_core::{AssetId, OwnedMediaPayload};
#[cfg(feature = "platform-ssh")]
use camera_toolbox_core::{ChromaOrder, MediaFormat};

#[cfg(feature = "platform-cv610")]
use crate::cli::{Cv610Command, Cv610DumpArgs, DumpKindArg, StreamRecordArgs};
#[cfg(feature = "platform-ssh")]
use crate::cli::{MediaFormatArg, SshCaptureArgs, SshCommand, SshFetchArgs};
use crate::cli::{PlatformCommand, ProfileCommand, ProfileStoreArgs, TargetArgs};

#[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(5);

pub(crate) fn run_profile(command: ProfileCommand) -> Result<()> {
    match command {
        ProfileCommand::List(args) => emit_result("profile_list", || profile_list(&args)),
        ProfileCommand::Validate(args) => {
            emit_result("profile_validate", || profile_validate(&args))
        }
    }
}

pub(crate) fn run_platform(command: PlatformCommand) -> Result<()> {
    match command {
        PlatformCommand::Probe(args) => emit_result("platform_probe", || platform_probe(&args)),
    }
}

#[cfg(feature = "platform-cv610")]
pub(crate) fn run_cv610(command: Cv610Command) -> Result<()> {
    match command {
        Cv610Command::Dump(args) => emit_result("cv610_dump", || cv610_dump(&args)),
    }
}

#[cfg(feature = "platform-cv610")]
pub(crate) fn run_stream_record(args: &StreamRecordArgs) -> Result<()> {
    emit_result("stream_record", || stream_record(args))
}

#[cfg(feature = "platform-ssh")]
pub(crate) fn run_ssh(command: SshCommand) -> Result<()> {
    match command {
        SshCommand::Capture(args) => emit_result("ssh_capture", || ssh_capture(args)),
        SshCommand::Fetch(args) => emit_result("ssh_fetch", || ssh_fetch(args)),
    }
}

struct CommandReport {
    json: Value,
    terminal_failure: Option<String>,
}

impl CommandReport {
    fn success(json: Value) -> Self {
        Self {
            json,
            terminal_failure: None,
        }
    }

    fn terminal(json: Value, failure: Option<String>) -> Self {
        Self {
            json,
            terminal_failure: failure,
        }
    }
}

fn emit_result(action: &'static str, run: impl FnOnce() -> Result<CommandReport>) -> Result<()> {
    match run() {
        Ok(report) => {
            println!("{}", report.json);
            if let Some(error) = report.terminal_failure {
                bail!("{error}");
            }
            Ok(())
        }
        Err(error) => {
            let message = format!("{error:#}");
            println!(
                "{}",
                json!({
                    "command": action,
                    "error": message,
                    "status": "failed"
                })
            );
            Err(error)
        }
    }
}

fn load_store(args: &ProfileStoreArgs) -> Result<(PathBuf, ProfileStore)> {
    let path = match &args.profile_store {
        Some(path) => path.clone(),
        None => ProfileStore::project_file_path()?,
    };
    let store = ProfileStore::load_from_path(&path)
        .with_context(|| format!("failed to load profile store {}", path.display()))?;
    Ok((path, store))
}

fn profile_list(args: &ProfileStoreArgs) -> Result<CommandReport> {
    let (path, store) = load_store(args)?;
    let platforms = store
        .platforms()
        .map(|profile| {
            json!({
                "display_name": profile.display_name.as_str(),
                "id": profile.id.as_str(),
                "provider": provider_name(&profile.config)
            })
        })
        .collect::<Vec<_>>();
    let sensors = store
        .sensors()
        .map(|sensor| {
            json!({
                "display_name": sensor.display_name.as_str(),
                "id": sensor.id.as_str(),
                "modes": sensor.profile.modes.iter().map(|mode| mode.id.as_str()).collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    Ok(CommandReport::success(json!({
        "command": "profile_list",
        "platforms": platforms,
        "profile_store": path,
        "sensors": sensors,
        "status": "success"
    })))
}

fn profile_validate(args: &ProfileStoreArgs) -> Result<CommandReport> {
    let (path, store) = load_store(args)?;
    Ok(CommandReport::success(json!({
        "capability_binding_count": store.matrix().iter().count(),
        "command": "profile_validate",
        "platform_count": store.platforms().count(),
        "profile_store": path,
        "sensor_count": store.sensors().count(),
        "status": "success",
        "valid": true
    })))
}

struct ResolvedTarget {
    profile: PlatformProfile,
    snapshot: Arc<TargetResolutionSnapshot>,
}

fn resolve_target(args: &TargetArgs) -> Result<ResolvedTarget> {
    let store_args = ProfileStoreArgs {
        profile_store: args.profile_store.clone(),
    };
    let (_, store) = load_store(&store_args)?;
    let platform_id = PlatformProfileId::new(args.platform.clone())?;
    let profile = store
        .platform(&platform_id)
        .cloned()
        .ok_or_else(|| anyhow!("platform profile not found: {platform_id}"))?;
    let selection = target_selection(args)?;

    let mut registry = PlatformRegistry::default();
    #[cfg(feature = "platform-cv610")]
    registry.register_cv610(HisiliconCv610Provider::default());
    #[cfg(feature = "platform-ssh")]
    if let PlatformConfig::SshManaged(config) = &profile.config {
        if config.credential_ref.starts_with("session:") {
            bail!(
                "session credential {} is not registered in this non-interactive CLI process; use key-file:/absolute/path or run the operation from Device Manager after entering the session secret",
                config.credential_ref
            );
        }
        let resolver: Arc<dyn CredentialResolver> = Arc::new(ProductionCredentialResolver::new());
        let recipes = Arc::new(
            production_recipe_registry_from_env()
                .context("invalid production SSH recipe environment")?,
        );
        registry.register_ssh(SshManagedPlatformProvider::production(resolver, recipes));
    }
    let bindings = registry
        .bind(&profile)
        .with_context(|| format!("failed to bind platform profile {platform_id}"))?;
    let snapshot = Arc::new(
        store
            .resolve_target(selection, &bindings)
            .with_context(|| format!("failed to resolve platform profile {platform_id}"))?,
    );
    Ok(ResolvedTarget { profile, snapshot })
}

fn target_selection(args: &TargetArgs) -> Result<SensorSelection> {
    match (&args.sensor_id, &args.mode_id) {
        (None, None) => Ok(SensorSelection::Unbound),
        (Some(sensor_id), Some(mode_id)) => Ok(SensorSelection::Mode(SensorModeKey {
            sensor_id: SensorId::new(sensor_id.clone())?,
            mode_id: mode_id.clone(),
        })),
        _ => bail!("--sensor-id and --mode-id must be supplied together"),
    }
}

fn platform_probe(args: &TargetArgs) -> Result<CommandReport> {
    let target = resolve_target(args)?;
    Ok(CommandReport::success(probe_json(
        &target.profile,
        &target.snapshot,
    )))
}

fn probe_json(profile: &PlatformProfile, snapshot: &TargetResolutionSnapshot) -> Value {
    let capabilities = [
        CapabilityKind::LocalOpen,
        CapabilityKind::Dump,
        CapabilityKind::Stream,
        CapabilityKind::RemoteFile,
        CapabilityKind::Command,
        CapabilityKind::RegisterRead,
        CapabilityKind::RegisterWrite,
        CapabilityKind::ExposureControl,
    ]
    .into_iter()
    .filter_map(|kind| snapshot.bindings.capability(kind))
    .map(|descriptor| {
        json!({
            "capability": capability_name(descriptor.capability),
            "concurrency": format!("{:?}", descriptor.concurrency).to_ascii_lowercase(),
            "driver": descriptor.driver.as_str(),
            "evidence": evidence_name(descriptor.evidence),
            "initialization": format!("{:?}", descriptor.initialization),
            "max_concurrent_operations": descriptor.hard_limits.max_concurrent_operations,
            "max_payload_bytes": descriptor.hard_limits.max_payload_bytes,
            "provider": platform_kind_name(descriptor.provider),
            "requirement": format!("{:?}", descriptor.requirement).to_ascii_lowercase(),
            "supported_variants": descriptor.supported_variants.iter().copied().map(capability_variant_name).collect::<Vec<_>>()
        })
    })
    .collect::<Vec<_>>();
    json!({
        "aggregate_hash": snapshot.aggregate_hash.to_hex(),
        "capabilities": capabilities,
        "capability_cell_snapshot_hash": snapshot.capability_cell_snapshot_hash.map(SnapshotHash::to_hex),
        "command": "platform_probe",
        "display_name": profile.display_name.as_str(),
        "platform_id": profile.id.as_str(),
        "platform_snapshot_hash": snapshot.platform_snapshot_hash.to_hex(),
        "provider": provider_name(&profile.config),
        "selection": selection_json(&snapshot.key.sensor),
        "sensor_mode_snapshot_hash": snapshot.sensor_mode_snapshot_hash.map(SnapshotHash::to_hex),
        "status": "success"
    })
}

#[cfg(feature = "platform-cv610")]
fn cv610_dump(args: &Cv610DumpArgs) -> Result<CommandReport> {
    let target = resolve_target(&args.target)?;
    if !matches!(target.profile.config, PlatformConfig::HisiliconCv610(_)) {
        bail!("selected profile is not a Hisilicon CV610 platform");
    }
    let output = DirectOutput::reserve(args.output.as_deref())?;
    let (store, controller) = controller_for(&target.snapshot)?;
    let kind: VerifiedDumpKind = args.kind.into();
    let asset_id = AssetId::new("cli-cv610-dump")?;
    let request = VerifiedDumpRequest::new(
        kind,
        asset_id,
        format!("cli-cv610-{}", dump_kind_name(kind)),
    )?;
    let timeouts = DumpTimeouts::default();
    let operation_id = controller.submit_dump(Arc::clone(&target.snapshot), request, timeouts)?;
    wait_dump(
        &controller,
        &store,
        &target.snapshot,
        &operation_id,
        output,
        timeouts,
    )
}

#[cfg(feature = "platform-cv610")]
fn wait_dump(
    controller: &PlatformController,
    store: &CaptureStore,
    snapshot: &TargetResolutionSnapshot,
    operation_id: &OperationId,
    output: Option<DirectOutput>,
    timeouts: DumpTimeouts,
) -> Result<CommandReport> {
    let deadline = Instant::now()
        .checked_add(timeouts.overall + Duration::from_secs(1))
        .ok_or_else(|| anyhow!("dump watchdog deadline overflow"))?;
    loop {
        if let Some(event) = controller.try_recv()?
            && event.operation_id == *operation_id
        {
            tracing::debug!(operation = %operation_id.as_str(), state = ?event.state, "CV610 Dump event");
            if event.state.is_terminal() {
                return dump_terminal_report(store, snapshot, event, output);
            }
        }
        if Instant::now() >= deadline {
            let _ = controller.cancel(operation_id);
            bail!("CV610 Dump did not publish a terminal event before the CLI watchdog deadline");
        }
        thread::sleep(EVENT_POLL_INTERVAL);
    }
}

#[cfg(feature = "platform-cv610")]
fn dump_terminal_report(
    store: &CaptureStore,
    snapshot: &TargetResolutionSnapshot,
    event: DumpJobEvent,
    output: Option<DirectOutput>,
) -> Result<CommandReport> {
    let base = job_base(
        "cv610_dump",
        event.operation_id.as_str(),
        &event.service_id,
        snapshot,
    );
    match event.state {
        DumpJobState::Succeeded {
            asset_id,
            payload_sha256,
        } => {
            let persisted = persist_asset(store, &asset_id, output)?;
            Ok(CommandReport::success(merge_json(
                base,
                json!({
                    "asset_id": asset_id.as_str(),
                    "output": persisted,
                    "payload_sha256": payload_sha256,
                    "status": "success"
                }),
            )))
        }
        DumpJobState::Failed(error) => {
            let failure = error.to_string();
            Ok(CommandReport::terminal(
                merge_json(base, json!({"error": &failure, "status": "failed"})),
                Some(failure),
            ))
        }
        DumpJobState::Queued | DumpJobState::Running(_) => {
            bail!("non-terminal Dump event reached terminal mapper")
        }
    }
}

#[cfg(feature = "platform-cv610")]
fn stream_record(args: &StreamRecordArgs) -> Result<CommandReport> {
    if args.transport_output.is_none() && args.annexb_output.is_none() {
        bail!(
            "stream-record requires --transport-output and/or the --annexb-output/--timestamp-output pair"
        );
    }
    let target = resolve_target(&args.target)?;
    let PlatformConfig::HisiliconCv610(config) = &target.profile.config else {
        bail!("selected profile is not a Hisilicon CV610 platform");
    };
    let destinations = [
        args.transport_output.as_deref(),
        args.annexb_output.as_deref(),
        args.timestamp_output.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let reservation = OutputReservation::reserve(&destinations)?;
    let request = StreamOpenRequest {
        channel: config.stream.channel,
        media: config.stream.media.clone(),
        cseq: 1,
        recording: StreamRecordingRequest {
            transport_destination: args.transport_output.clone(),
            annexb_destination: args.annexb_output.clone(),
            timestamp_sidecar_destination: args.timestamp_output.clone(),
            quota_bytes: args.quota_bytes.get(),
        },
    };
    let ResolvedTargetBindings::Cv610(bindings) = target.snapshot.bindings.as_ref() else {
        bail!("resolved target does not contain CV610 bindings");
    };
    let service_id = bindings
        .stream
        .as_ref()
        .ok_or_else(|| anyhow!("resolved target does not provide CV610 stream"))?
        .service
        .service_id()
        .to_owned();
    let (_, controller) = controller_for(&target.snapshot)?;
    let timeouts = StreamTimeouts::default();
    let session_id = controller.submit_stream(Arc::clone(&target.snapshot), request, timeouts)?;
    reservation.commit();
    wait_stream(
        &controller,
        &target.snapshot,
        &session_id,
        &service_id,
        Duration::from_secs(args.duration.get()),
        timeouts,
        &destinations,
    )
}

#[cfg(feature = "platform-cv610")]
fn wait_stream(
    controller: &PlatformController,
    snapshot: &TargetResolutionSnapshot,
    session_id: &StreamSessionId,
    service_id: &str,
    duration: Duration,
    timeouts: StreamTimeouts,
    destinations: &[&Path],
) -> Result<CommandReport> {
    let stop_at = Instant::now()
        .checked_add(duration)
        .ok_or_else(|| anyhow!("stream recording duration overflows the host monotonic clock"))?;
    let mut state = StreamReportState::default();
    while Instant::now() < stop_at {
        if let Some(event) = controller.try_recv_stream()?
            && event.session_id == *session_id
        {
            tracing::debug!(session = %session_id.as_str(), event = ?event.event, "stream event");
            if let Some(report) = state.accept(event, false, snapshot, service_id, destinations) {
                return Ok(report);
            }
        }
        thread::sleep(EVENT_POLL_INTERVAL);
    }

    if !controller.request_stream_close(session_id) {
        bail!("stream session disappeared before the finite-duration close request");
    }
    let close_deadline = Instant::now()
        .checked_add(timeouts.idle + Duration::from_secs(1))
        .ok_or_else(|| anyhow!("stream close watchdog deadline overflow"))?;
    loop {
        if let Some(event) = controller.try_recv_stream()?
            && event.session_id == *session_id
        {
            tracing::debug!(session = %session_id.as_str(), event = ?event.event, "stream close event");
            if let Some(report) = state.accept(event, true, snapshot, service_id, destinations) {
                return Ok(report);
            }
        }
        if Instant::now() >= close_deadline {
            let _ = controller.force_stream_cleanup(session_id);
            let failure =
                "stream did not publish a terminal event after the finite-duration close request"
                    .to_owned();
            return Ok(state.finish(
                snapshot,
                session_id,
                service_id,
                destinations,
                "forced",
                Some(failure),
            ));
        }
        thread::sleep(EVENT_POLL_INTERVAL);
    }
}

#[cfg(feature = "platform-cv610")]
#[derive(Default)]
struct StreamReportState {
    negotiated: Option<Value>,
    metrics: Option<StreamMetrics>,
    recordings: Vec<Value>,
    recording_failure: Option<String>,
}

#[cfg(feature = "platform-cv610")]
impl StreamReportState {
    fn accept(
        &mut self,
        event: StreamSessionEvent,
        close_requested: bool,
        snapshot: &TargetResolutionSnapshot,
        service_id: &str,
        destinations: &[&Path],
    ) -> Option<CommandReport> {
        match event.event {
            StreamServiceEvent::Negotiated(info) => {
                self.negotiated = Some(json!({
                    "bitrate_value": info.bitrate_value,
                    "clock_rate": info.clock_rate,
                    "codec": format!("{:?}", info.codec).to_ascii_lowercase(),
                    "frame_rate": info.frame_rate,
                    "height": info.height,
                    "payload_type": info.payload_type,
                    "rtp_channel": info.rtp_channel,
                    "ssrc": info.ssrc,
                    "width": info.width
                }));
            }
            StreamServiceEvent::Metrics(metrics) => self.metrics = Some(metrics),
            StreamServiceEvent::Recording { branch, state } => {
                if let RecordingState::FailedIncomplete { reason, .. } = &state {
                    self.recording_failure = Some(reason.clone());
                }
                self.recordings.push(recording_json(branch, &state));
            }
            StreamServiceEvent::Terminal(terminal) => {
                let (terminal_name, terminal_failure) = stream_terminal(&terminal, close_requested);
                let failure = self.recording_failure.clone().or(terminal_failure);
                return Some(self.finish(
                    snapshot,
                    &event.session_id,
                    service_id,
                    destinations,
                    terminal_name,
                    failure,
                ));
            }
            StreamServiceEvent::Stage(_) | StreamServiceEvent::DecoderUnavailable { .. } => {}
        }
        None
    }

    fn finish(
        &self,
        snapshot: &TargetResolutionSnapshot,
        session_id: &StreamSessionId,
        service_id: &str,
        destinations: &[&Path],
        terminal: &str,
        failure: Option<String>,
    ) -> CommandReport {
        let status = if failure.is_some() {
            "failed"
        } else {
            "success"
        };
        CommandReport::terminal(
            merge_json(
                job_base("stream_record", session_id.as_str(), service_id, snapshot),
                json!({
                    "destinations": destinations,
                    "error": failure.clone(),
                    "metrics": self.metrics.as_ref().map(metrics_json),
                    "negotiated": self.negotiated.clone(),
                    "recordings": self.recordings.clone(),
                    "status": status,
                    "terminal": terminal
                }),
            ),
            failure,
        )
    }
}

#[cfg(feature = "platform-ssh")]
fn ssh_capture(args: SshCaptureArgs) -> Result<CommandReport> {
    let target = resolve_target(&args.target)?;
    let PlatformConfig::SshManaged(config) = &target.profile.config else {
        bail!("selected profile is not an SSH-managed platform");
    };
    if args.format.trim().is_empty() {
        bail!("--format must not be empty");
    }
    let request = TypedCommandRequest::new(config.capture_recipe.clone())?
        .with_parameter(
            "output_dir",
            CommandParameter::RemotePath(config.remote_artifact_dir.clone()),
        )
        .with_parameter("format", CommandParameter::Choice(args.format));
    let (_, controller) = controller_for(&target.snapshot)?;
    let timeouts = RemoteTimeouts::default();
    let operation_id =
        controller.submit_command(Arc::clone(&target.snapshot), request, timeouts)?;
    wait_remote(
        &controller,
        None,
        &target.snapshot,
        &operation_id,
        timeouts,
        "ssh_capture",
    )
}

#[cfg(feature = "platform-ssh")]
fn ssh_fetch(args: SshFetchArgs) -> Result<CommandReport> {
    let target = resolve_target(&args.target)?;
    if !matches!(target.profile.config, PlatformConfig::SshManaged(_)) {
        bail!("selected profile is not an SSH-managed platform");
    }
    let output = DirectOutput::reserve(args.output.as_deref())?;
    let source_name = Path::new(&args.remote_path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("remote-asset")
        .to_owned();
    let request = RemoteFileRequest {
        asset_id: AssetId::new("cli-ssh-fetch")?,
        remote_path: args.remote_path,
        expected_sha256: args.expected_sha256,
        format: args.format.into(),
        source_name,
    };
    let (store, controller) = controller_for(&target.snapshot)?;
    let timeouts = RemoteTimeouts::default();
    let operation_id =
        controller.submit_remote_fetch(Arc::clone(&target.snapshot), request, timeouts)?;
    wait_remote(
        &controller,
        Some((&store, output)),
        &target.snapshot,
        &operation_id,
        timeouts,
        "ssh_fetch",
    )
}

#[cfg(feature = "platform-ssh")]
fn wait_remote(
    controller: &PlatformController,
    persistence: Option<(&CaptureStore, Option<DirectOutput>)>,
    snapshot: &TargetResolutionSnapshot,
    operation_id: &OperationId,
    timeouts: RemoteTimeouts,
    command: &'static str,
) -> Result<CommandReport> {
    let deadline = Instant::now()
        .checked_add(timeouts.overall + Duration::from_secs(1))
        .ok_or_else(|| anyhow!("remote watchdog deadline overflow"))?;
    loop {
        if let Some(event) = controller.try_recv_remote()?
            && event.operation_id == *operation_id
        {
            tracing::debug!(operation = %operation_id.as_str(), state = ?event.state, "SSH event");
            if event.state.is_terminal() {
                return remote_terminal_report(persistence, snapshot, event, command);
            }
        }
        if Instant::now() >= deadline {
            let _ = controller.cancel(operation_id);
            bail!(
                "SSH operation did not publish a terminal event before the CLI watchdog deadline"
            );
        }
        thread::sleep(EVENT_POLL_INTERVAL);
    }
}

#[cfg(feature = "platform-ssh")]
fn remote_terminal_report(
    persistence: Option<(&CaptureStore, Option<DirectOutput>)>,
    snapshot: &TargetResolutionSnapshot,
    event: RemoteJobEvent,
    command: &'static str,
) -> Result<CommandReport> {
    let base = job_base(
        command,
        event.operation_id.as_str(),
        &event.service_id,
        snapshot,
    );
    match event.state {
        RemoteJobState::CommandCompleted(result) => Ok(command_result_report(base, result)),
        RemoteJobState::AssetReady {
            asset_id,
            payload_sha256,
            source,
            ..
        } => {
            let (store, output) = persistence
                .ok_or_else(|| anyhow!("remote asset terminal is missing persistence context"))?;
            let persisted = persist_asset(store, &asset_id, output)?;
            Ok(CommandReport::success(merge_json(
                base,
                json!({
                    "asset_id": asset_id.as_str(),
                    "output": persisted,
                    "payload_sha256": payload_sha256,
                    "source": discovery_source_name(source),
                    "status": "success"
                }),
            )))
        }
        RemoteJobState::Failed(failure) => {
            let failure = remote_failure_string(&failure);
            Ok(CommandReport::terminal(
                merge_json(base, json!({"error": &failure, "status": "failed"})),
                Some(failure),
            ))
        }
        RemoteJobState::Queued | RemoteJobState::Running(_) | RemoteJobState::Watch(_) => {
            bail!("non-terminal remote event reached terminal mapper")
        }
    }
}

#[cfg(feature = "platform-ssh")]
fn command_result_report(base: Value, result: CommandResult) -> CommandReport {
    let CommandResult {
        terminal: command_terminal,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        artifact_path,
    } = result;
    let (terminal, failure) = match command_terminal {
        CommandTerminal::Succeeded => ("succeeded", None),
        CommandTerminal::ExitFailure { status } => (
            "exit_failure",
            Some(format!("remote capture recipe exited with status {status}")),
        ),
        CommandTerminal::OutputTruncated { status } => (
            "output_truncated",
            Some(format!(
                "remote capture recipe output was truncated (status {status:?})"
            )),
        ),
        CommandTerminal::RemoteStateUnknown => (
            "remote_state_unknown",
            Some("remote capture recipe state is unknown".to_owned()),
        ),
    };
    let status = if failure.is_some() {
        "failed"
    } else {
        "success"
    };
    CommandReport::terminal(
        merge_json(
            base,
            json!({
                "artifact_path": artifact_path,
                "error": failure.clone(),
                "status": status,
                "stderr": String::from_utf8_lossy(&stderr),
                "stderr_truncated": stderr_truncated,
                "stdout": String::from_utf8_lossy(&stdout),
                "stdout_truncated": stdout_truncated,
                "terminal": terminal
            }),
        ),
        failure,
    )
}

#[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
fn controller_for(
    snapshot: &TargetResolutionSnapshot,
) -> Result<(CaptureStore, PlatformController)> {
    let maximum = [
        CapabilityKind::Dump,
        CapabilityKind::Stream,
        CapabilityKind::RemoteFile,
        CapabilityKind::Command,
        CapabilityKind::LocalOpen,
    ]
    .into_iter()
    .filter_map(|kind| snapshot.bindings.capability(kind))
    .filter_map(|descriptor| descriptor.hard_limits.max_payload_bytes)
    .max()
    .ok_or_else(|| anyhow!("resolved target has no bounded payload capability"))?;
    let maximum = usize::try_from(maximum)
        .map_err(|_| anyhow!("resolved payload limit {maximum} cannot fit this host"))?;
    let limits = CaptureStoreLimits::new(maximum, maximum)?;
    let store = CaptureStore::new(limits);
    let controller = PlatformController::new(store.clone());
    Ok((store, controller))
}

#[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
fn persist_asset(
    store: &CaptureStore,
    asset_id: &AssetId,
    output: Option<DirectOutput>,
) -> Result<Option<Value>> {
    let Some(output) = output else {
        return Ok(None);
    };
    let asset = store
        .get(asset_id)?
        .ok_or_else(|| anyhow!("terminal asset {asset_id} is missing from CaptureStore"))?;
    let path = output.path.clone();
    let bytes = output
        .write_source(&asset.source)
        .with_context(|| format!("failed to write explicit output {}", path.display()))?;
    Ok(Some(json!({
        "bytes": bytes,
        "path": path
    })))
}

#[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
struct DirectOutput {
    path: PathBuf,
    file: Option<fs::File>,
    committed: bool,
}

#[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
impl DirectOutput {
    fn reserve(path: Option<&Path>) -> io::Result<Option<Self>> {
        let Some(path) = path else {
            return Ok(None);
        };
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        Ok(Some(Self {
            path: path.to_path_buf(),
            file: Some(file),
            committed: false,
        }))
    }

    fn write_source(mut self, source: &OwnedMediaPayload) -> io::Result<u64> {
        let file = self.file.as_mut().expect("reserved output file is present");
        let mut written = 0_u64;
        match source {
            OwnedMediaPayload::Bytes(bytes) => {
                file.write_all(bytes)?;
                written = u64::try_from(bytes.len()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "payload length exceeds u64")
                })?;
            }
            OwnedMediaPayload::Planes(planes) => {
                for plane in planes {
                    file.write_all(&plane.bytes)?;
                    let length = u64::try_from(plane.bytes.len()).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "plane length exceeds u64")
                    })?;
                    written = written.checked_add(length).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "payload length exceeds u64")
                    })?;
                }
            }
        }
        file.flush()?;
        self.committed = true;
        Ok(written)
    }
}

#[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
impl Drop for DirectOutput {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(feature = "platform-cv610")]
struct OutputReservation {
    paths: Vec<PathBuf>,
    committed: bool,
}

#[cfg(feature = "platform-cv610")]
impl OutputReservation {
    fn reserve(paths: &[&Path]) -> io::Result<Self> {
        let mut reservation = Self {
            paths: Vec::with_capacity(paths.len()),
            committed: false,
        };
        for path in paths {
            OpenOptions::new().write(true).create_new(true).open(path)?;
            reservation.paths.push((*path).to_path_buf());
        }
        Ok(reservation)
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

#[cfg(feature = "platform-cv610")]
impl Drop for OutputReservation {
    fn drop(&mut self) {
        if !self.committed {
            for path in &self.paths {
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
fn job_base(
    command: &'static str,
    operation_id: &str,
    service_id: &str,
    snapshot: &TargetResolutionSnapshot,
) -> Value {
    json!({
        "aggregate_hash": snapshot.aggregate_hash.to_hex(),
        "command": command,
        "operation_id": operation_id,
        "platform_id": snapshot.key.platform_id.as_str(),
        "selection": selection_json(&snapshot.key.sensor),
        "service_id": service_id
    })
}

#[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
fn merge_json(base: Value, fields: Value) -> Value {
    let Value::Object(mut base) = base else {
        unreachable!("job base is an object");
    };
    let Value::Object(fields) = fields else {
        unreachable!("job fields are an object");
    };
    base.extend(fields);
    Value::Object(base)
}

fn selection_json(selection: &SensorSelection) -> Value {
    match selection {
        SensorSelection::Unbound => json!({"selection": "unbound"}),
        SensorSelection::Mode(key) => json!({
            "mode_id": key.mode_id.as_str(),
            "selection": "mode",
            "sensor_id": key.sensor_id.as_str()
        }),
    }
}

fn provider_name(config: &PlatformConfig) -> &'static str {
    match config {
        PlatformConfig::Local(_) => "local",
        PlatformConfig::HisiliconCv610(_) => "hisilicon-cv610-pq",
        PlatformConfig::SshManaged(_) => "ssh-managed",
    }
}

fn capability_name(capability: CapabilityKind) -> &'static str {
    match capability {
        CapabilityKind::LocalOpen => "local_open",
        CapabilityKind::Dump => "dump",
        CapabilityKind::Stream => "stream",
        CapabilityKind::RemoteFile => "remote_file",
        CapabilityKind::Command => "command",
        CapabilityKind::RegisterRead => "register_read",
        CapabilityKind::RegisterWrite => "register_write",
        CapabilityKind::ExposureControl => "exposure_control",
        CapabilityKind::EepromProvision => "eeprom_provision",
    }
}

fn capability_variant_name(variant: CapabilityVariant) -> &'static str {
    match variant {
        CapabilityVariant::LocalRaw => "local_raw",
        CapabilityVariant::Raw10 => "raw10",
        CapabilityVariant::Raw12 => "raw12",
        CapabilityVariant::Jpeg => "jpeg",
        CapabilityVariant::Nv21 => "nv21",
        CapabilityVariant::H264 => "h264",
        CapabilityVariant::H265 => "h265",
        CapabilityVariant::RemoteFetch => "remote_fetch",
        CapabilityVariant::RemoteWatch => "remote_watch",
        CapabilityVariant::StandardExecCommand => "standard_exec_command",
        CapabilityVariant::ArgvSubsystemCommand => "argv_subsystem_command",
        CapabilityVariant::RegisterAccess => "register_access",
        CapabilityVariant::Exposure => "exposure",
        CapabilityVariant::EepromInspectDryRun => "eeprom_inspect_dry_run",
        CapabilityVariant::EepromFullProvision => "eeprom_full_provision",
        CapabilityVariant::EepromUpdateCalibration => "eeprom_update_calibration",
    }
}

fn evidence_name(evidence: EvidenceMaturity) -> &'static str {
    match evidence {
        EvidenceMaturity::Unknown => "unknown",
        EvidenceMaturity::ProfileDeclared => "profile_declared",
        EvidenceMaturity::UserConfirmed => "user_confirmed",
        EvidenceMaturity::ProtocolVerified => "protocol_verified",
    }
}

fn platform_kind_name(kind: PlatformKind) -> &'static str {
    match kind {
        PlatformKind::Local => "local",
        PlatformKind::HisiliconCv610 => "hisilicon_cv610",
        PlatformKind::SshManaged => "ssh_managed",
    }
}

#[cfg(feature = "platform-cv610")]
fn dump_kind_name(kind: VerifiedDumpKind) -> &'static str {
    match kind {
        VerifiedDumpKind::Raw10 => "raw10",
        VerifiedDumpKind::Raw12 => "raw12",
        VerifiedDumpKind::Jpeg => "jpeg",
        VerifiedDumpKind::Nv21 => "nv21",
    }
}

#[cfg(feature = "platform-cv610")]
fn stream_terminal(
    terminal: &StreamTerminal,
    close_requested: bool,
) -> (&'static str, Option<String>) {
    match terminal {
        StreamTerminal::BoundaryClosed => ("boundary_closed", None),
        StreamTerminal::Cancelled if close_requested => ("cancelled", None),
        StreamTerminal::Cancelled => (
            "cancelled",
            Some("stream was cancelled before the finite-duration close request".to_owned()),
        ),
        StreamTerminal::Failed(error) => ("failed", Some(error.to_string())),
        StreamTerminal::Forced {
            remote_state_unknown,
        } => (
            "forced",
            Some(format!(
                "stream required forced cleanup; remote_state_unknown={remote_state_unknown}"
            )),
        ),
    }
}

#[cfg(feature = "platform-cv610")]
fn recording_json(branch: RecordingBranch, state: &RecordingState) -> Value {
    let branch = match branch {
        RecordingBranch::TransportEvidence => "transport_evidence",
        RecordingBranch::AnnexB => "annex_b",
    };
    match state {
        RecordingState::Started => json!({"branch": branch, "state": "started"}),
        RecordingState::FailedIncomplete {
            reason,
            bytes_written,
        } => json!({
            "branch": branch,
            "bytes_written": *bytes_written,
            "reason": reason,
            "state": "failed_incomplete"
        }),
        RecordingState::Completed { bytes_written } => json!({
            "branch": branch,
            "bytes_written": *bytes_written,
            "state": "completed"
        }),
    }
}

#[cfg(feature = "platform-cv610")]
fn metrics_json(metrics: &StreamMetrics) -> Value {
    json!({
        "decoded_frames": metrics.decoded_frames,
        "decoder_input_dropped": metrics.decoder_input_dropped,
        "decoder_input_queue_depth": metrics.decoder_queue_depth,
        "decoder_resyncs": metrics.decoder_resyncs,
        "network_bytes": metrics.network_bytes,
        "presented_frames": metrics.presented_frames,
        "preview_dropped": metrics.preview_dropped,
        "preview_queue_depth": metrics.preview_queue_depth,
        "record_bytes": metrics.record_bytes,
        "recorder_queue_bytes": metrics.recorder_queue_bytes,
        "rtp_gaps": metrics.rtp_gaps,
        "rtp_packets": metrics.rtp_packets,
        "transport_queue_bytes": metrics.transport_queue_bytes
    })
}

#[cfg(feature = "platform-ssh")]
fn remote_failure_string(failure: &RemoteJobFailure) -> String {
    match failure {
        RemoteJobFailure::Command(error) => error.to_string(),
        RemoteJobFailure::RemoteFile(error) => error.to_string(),
    }
}

#[cfg(feature = "platform-ssh")]
fn discovery_source_name(source: RemoteDiscoverySource) -> &'static str {
    match source {
        RemoteDiscoverySource::CommandResultPath => "command_result_path",
        RemoteDiscoverySource::RemoteEventWatch => "remote_event_watch",
        RemoteDiscoverySource::DirectoryPolling => "directory_polling",
        RemoteDiscoverySource::ExplicitPath => "explicit_path",
    }
}

#[cfg(feature = "platform-cv610")]
impl From<DumpKindArg> for VerifiedDumpKind {
    fn from(value: DumpKindArg) -> Self {
        match value {
            DumpKindArg::Raw10 => Self::Raw10,
            DumpKindArg::Raw12 => Self::Raw12,
            DumpKindArg::Jpeg => Self::Jpeg,
            DumpKindArg::Nv21 => Self::Nv21,
        }
    }
}

#[cfg(feature = "platform-ssh")]
impl From<MediaFormatArg> for MediaFormat {
    fn from(value: MediaFormatArg) -> Self {
        match value {
            MediaFormatArg::Raw10Packed => Self::RawPacked { bit_depth: 10 },
            MediaFormatArg::Raw12Packed => Self::RawPacked { bit_depth: 12 },
            MediaFormatArg::Raw10U16le => Self::RawU16Le { bit_depth: 10 },
            MediaFormatArg::Raw12U16le => Self::RawU16Le { bit_depth: 12 },
            MediaFormatArg::Jpeg => Self::Jpeg,
            MediaFormatArg::Nv12 => Self::Yuv420Sp {
                chroma_order: ChromaOrder::Uv,
            },
            MediaFormatArg::Nv21 => Self::Yuv420Sp {
                chroma_order: ChromaOrder::Vu,
            },
            MediaFormatArg::H264 => Self::H264AnnexB,
            MediaFormatArg::H265 => Self::H265AnnexB,
            MediaFormatArg::Binary => Self::Binary,
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
    use std::{collections::BTreeMap, time::SystemTime};

    #[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
    use camera_toolbox_app::LocalConfig;
    use camera_toolbox_app::SensorSelection;
    #[cfg(feature = "platform-cv610")]
    use camera_toolbox_app::{Cv610Config, Cv610DumpConfig, Cv610StreamConfig, DumpServiceError};
    #[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
    use camera_toolbox_core::{CaptureMetadata, EphemeralAsset, IntegrityState, MediaFormat};

    use super::*;

    fn target_args(sensor_id: Option<&str>, mode_id: Option<&str>) -> TargetArgs {
        TargetArgs {
            profile_store: None,
            platform: "local".to_owned(),
            sensor_id: sensor_id.map(str::to_owned),
            mode_id: mode_id.map(str::to_owned),
        }
    }

    #[cfg(feature = "platform-cv610")]
    fn cv610_profile() -> PlatformProfile {
        PlatformProfile {
            id: PlatformProfileId::new("cv610").unwrap(),
            display_name: "CV610".to_owned(),
            config: PlatformConfig::HisiliconCv610(Cv610Config {
                host: "127.0.0.1".to_owned(),
                dump: Cv610DumpConfig::default(),
                stream: Cv610StreamConfig::default(),
            }),
        }
    }

    #[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
    fn unique_directory() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("camera-toolbox-command-{suffix}"))
    }

    #[test]
    fn selection_requires_an_exact_sensor_mode_pair() {
        assert!(matches!(
            target_selection(&target_args(None, None)).unwrap(),
            SensorSelection::Unbound
        ));
        assert!(matches!(
            target_selection(&target_args(Some("imx415"), Some("4k30"))).unwrap(),
            SensorSelection::Mode(_)
        ));
        assert!(target_selection(&target_args(Some("imx415"), None)).is_err());
        assert!(target_selection(&target_args(None, Some("4k30"))).is_err());
    }

    #[cfg(feature = "platform-cv610")]
    #[test]
    fn unbound_resolution_keeps_platform_only_cv610_capabilities_and_json_shape() {
        let profile = cv610_profile();
        let mut store = ProfileStore::new();
        store.insert_platform(profile.clone()).unwrap();
        let mut registry = PlatformRegistry::default();
        registry.register_cv610(HisiliconCv610Provider::default());
        let bindings = registry.bind(&profile).unwrap();
        let snapshot = store
            .resolve_target(SensorSelection::Unbound, &bindings)
            .unwrap();
        assert!(snapshot.bindings.capability(CapabilityKind::Dump).is_some());
        assert!(
            snapshot
                .bindings
                .capability(CapabilityKind::Stream)
                .is_some()
        );

        let output = probe_json(&profile, &snapshot);
        assert_eq!(output["command"], "platform_probe");
        assert_eq!(output["status"], "success");
        assert_eq!(output["selection"]["selection"], "unbound");
        assert_eq!(output["capabilities"].as_array().unwrap().len(), 2);
        assert_eq!(output["aggregate_hash"].as_str().unwrap().len(), 64);
    }

    #[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
    #[test]
    fn direct_output_refuses_existing_target_without_temp_files() {
        let directory = unique_directory();
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("capture.bin");
        fs::write(&target, b"existing").unwrap();

        let error = match DirectOutput::reserve(Some(&target)) {
            Err(error) => error,
            Ok(_) => panic!("existing output target was not refused"),
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).unwrap(), b"existing");
        let names = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, vec![std::ffi::OsString::from("capture.bin")]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(feature = "platform-cv610")]
    #[test]
    fn stream_reservation_cleans_only_this_invocations_setup_files() {
        let directory = unique_directory();
        fs::create_dir_all(&directory).unwrap();
        let first = directory.join("transport.bin");
        let existing = directory.join("annexb.bin");
        fs::write(&existing, b"keep").unwrap();

        let error = match OutputReservation::reserve(&[&first, &existing]) {
            Err(error) => error,
            Ok(_) => panic!("existing stream output target was not refused"),
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!first.exists());
        assert_eq!(fs::read(&existing).unwrap(), b"keep");
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(feature = "platform-cv610")]
    #[test]
    fn cv610_terminal_failures_map_to_nonzero_reports() {
        let profile = cv610_profile();
        let mut store = ProfileStore::new();
        store.insert_platform(profile.clone()).unwrap();
        let mut registry = PlatformRegistry::default();
        registry.register_cv610(HisiliconCv610Provider::default());
        let bindings = registry.bind(&profile).unwrap();
        let snapshot = store
            .resolve_target(SensorSelection::Unbound, &bindings)
            .unwrap();
        let capture_store = CaptureStore::new(CaptureStoreLimits::new(1, 1).unwrap());
        let event = DumpJobEvent {
            operation_id: OperationId::new("dump-test").unwrap(),
            service_id: "fake-dump".to_owned(),
            state: DumpJobState::Failed(DumpServiceError::ServerRejected {
                message: "fixture rejection".to_owned(),
            }),
        };
        let report = dump_terminal_report(&capture_store, &snapshot, event, None).unwrap();
        assert!(report.terminal_failure.is_some());
        assert_eq!(report.json["status"], "failed");

        let (_, failure) = stream_terminal(
            &StreamTerminal::Failed(camera_toolbox_app::StreamServiceError::Negotiation(
                "fixture".to_owned(),
            )),
            false,
        );
        assert!(failure.is_some());
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn ssh_terminal_failures_map_to_nonzero_reports() {
        let command = command_result_report(
            json!({}),
            CommandResult {
                terminal: CommandTerminal::ExitFailure { status: 7 },
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                artifact_path: None,
            },
        );
        assert!(command.terminal_failure.is_some());
    }

    #[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
    #[test]
    fn test_asset_fixture_is_valid_for_direct_writer() {
        let asset = EphemeralAsset::new(
            AssetId::new("asset").unwrap(),
            OwnedMediaPayload::from_bytes(Arc::<[u8]>::from(&b"payload"[..])),
            CaptureMetadata {
                format: MediaFormat::Binary,
                source_name: "fixture".to_owned(),
                attributes: BTreeMap::new(),
            },
            IntegrityState::Verified {
                algorithm: "sha256".to_owned(),
                digest: "fixture".to_owned(),
            },
        );
        assert_eq!(asset.byte_len().unwrap(), 7);
        let local = PlatformProfile {
            id: PlatformProfileId::new("local").unwrap(),
            display_name: "Local".to_owned(),
            config: PlatformConfig::Local(LocalConfig::default()),
        };
        assert_eq!(provider_name(&local.config), "local");
    }
}
