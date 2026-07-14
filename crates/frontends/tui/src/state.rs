//! 平台控制台状态机：profile bind 与 Sensor resolve 分离，job 仅经 typed controller 提交。

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use camera_toolbox_adapters::platforms::ssh_managed::CommandRecipeRegistry;
use camera_toolbox_app::{
    CapabilityKind, CapabilityVariant, CaptureStore, CaptureStoreLimits, CommandParameter,
    CommandTerminal, DumpJobEvent, DumpJobState, DumpTimeouts, OperationId, PlatformBindings,
    PlatformConfig, PlatformController, PlatformProfile, ProfileStore, RemoteDiscoveryRequest,
    RemoteFileRequest, RemoteJobEvent, RemoteJobState, RemoteTimeouts, RemoteWatchEvent,
    RemoteWatchRequest, ResolvedTargetBindings, SensorModeKey, SensorSelection, StreamOpenRequest,
    StreamRecordingRequest, StreamServiceEvent, StreamSessionEvent, StreamSessionId,
    StreamTimeouts, TargetResolutionSnapshot, TypedCommandRequest, VerifiedDumpKind,
    VerifiedDumpRequest,
};
use camera_toolbox_core::{AssetId, MediaFormat};

use crate::args::Args;

const EVENT_LOG_LIMIT: usize = 128;
const JOB_HISTORY_LIMIT: usize = 64;
const EVENTS_PER_QUEUE_TICK: usize = 32;
const CAPABILITY_KINDS: [CapabilityKind; 8] = [
    CapabilityKind::LocalOpen,
    CapabilityKind::Dump,
    CapabilityKind::Stream,
    CapabilityKind::RemoteFile,
    CapabilityKind::Command,
    CapabilityKind::RegisterRead,
    CapabilityKind::RegisterWrite,
    CapabilityKind::ExposureControl,
];

/// provider bind seam；生产实现仍唯一委派 `PlatformRegistry`。
pub(crate) trait BindingBackend {
    fn bind(&self, profile: &PlatformProfile) -> Result<PlatformBindings, String>;
}

#[derive(Debug, Clone)]
pub(crate) struct SensorChoice {
    pub(crate) label: String,
    pub(crate) selection: SensorSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsoleAction {
    Dump,
    StreamRecord,
    SshCapture,
    RemoteFetch,
    RemoteWatch,
    CancelAll,
}

impl ConsoleAction {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Dump => "CV610 Dump",
            Self::StreamRecord => "Finite stream record",
            Self::SshCapture => "SSH typed capture",
            Self::RemoteFetch => "Remote fetch",
            Self::RemoteWatch => "Remote watch",
            Self::CancelAll => "Cancel jobs/sessions",
        }
    }

    pub(crate) const fn key(self) -> char {
        match self {
            Self::Dump => 'd',
            Self::StreamRecord => 'r',
            Self::SshCapture => 'c',
            Self::RemoteFetch => 'f',
            Self::RemoteWatch => 'w',
            Self::CancelAll => 'x',
        }
    }
}

#[derive(Debug)]
pub(crate) enum ControllerEvent {
    Dump(DumpJobEvent),
    Stream(StreamSessionEvent),
    Remote(RemoteJobEvent),
}

#[derive(Clone)]
enum OperationContext {
    Dump,
    Capture {
        snapshot: Arc<TargetResolutionSnapshot>,
        format: MediaFormat,
    },
    Remote,
    Watch,
}

struct StreamContext {
    deadline: Instant,
    close_requested: bool,
}

pub(crate) struct ConsoleState {
    pub(crate) args: Args,
    pub(crate) profiles: ProfileStore,
    backend: Box<dyn BindingBackend>,
    recipes: Arc<CommandRecipeRegistry>,
    pub(crate) selected_profile_index: Option<usize>,
    pub(crate) selected_sensor_index: usize,
    pub(crate) sensor_choices: Vec<SensorChoice>,
    candidate: Option<PlatformBindings>,
    pub(crate) snapshot: Option<Arc<TargetResolutionSnapshot>>,
    pub(crate) error: Option<String>,
    pub(crate) startup_message: Option<String>,
    pub(crate) bind_count: u64,
    controller: PlatformController,
    pub(crate) capture_store: CaptureStore,
    operations: BTreeMap<OperationId, OperationContext>,
    sessions: BTreeMap<StreamSessionId, StreamContext>,
    pub(crate) jobs: VecDeque<(String, String)>,
    pub(crate) assets: BTreeSet<AssetId>,
    pub(crate) event_log: VecDeque<String>,
    next_asset: u64,
    shutdown_started: bool,
}

impl ConsoleState {
    pub(crate) fn new(
        args: Args,
        profiles: ProfileStore,
        backend: Box<dyn BindingBackend>,
        recipes: Arc<CommandRecipeRegistry>,
        startup_message: Option<String>,
    ) -> Result<Self> {
        let limits = CaptureStoreLimits::new(256 * 1024 * 1024, 1024 * 1024 * 1024)
            .context("invalid TUI CaptureStore limits")?;
        let capture_store = CaptureStore::new(limits);
        let sensor_choices = sensor_choices(&profiles);
        let mut state = Self {
            args,
            profiles,
            backend,
            recipes,
            selected_profile_index: None,
            selected_sensor_index: 0,
            sensor_choices,
            candidate: None,
            snapshot: None,
            error: None,
            startup_message,
            bind_count: 0,
            controller: PlatformController::new(capture_store.clone()),
            capture_store,
            operations: BTreeMap::new(),
            sessions: BTreeMap::new(),
            jobs: VecDeque::new(),
            assets: BTreeSet::new(),
            event_log: VecDeque::new(),
            next_asset: 1,
            shutdown_started: false,
        };
        if state.profile_count() == 0 {
            state.error = Some("profile store contains no platform profiles".to_owned());
        } else {
            state.select_platform_index(0);
        }
        Ok(state)
    }

    pub(crate) fn profile_count(&self) -> usize {
        self.profiles.platforms().count()
    }

    pub(crate) fn profile_at(&self, index: usize) -> Option<&PlatformProfile> {
        self.profiles.platforms().nth(index)
    }

    pub(crate) fn selected_profile(&self) -> Option<&PlatformProfile> {
        self.selected_profile_index
            .and_then(|index| self.profile_at(index))
    }

    pub(crate) fn select_platform_index(&mut self, index: usize) {
        if self.selected_profile_index == Some(index) {
            return;
        }
        let Some(profile) = self.profile_at(index).cloned() else {
            self.set_error(format!("platform selection index {index} is out of range"));
            return;
        };
        self.selected_profile_index = Some(index);
        self.candidate = None;
        self.snapshot = None;
        self.error = None;
        self.bind_count = self.bind_count.saturating_add(1);
        match self.backend.bind(&profile) {
            Ok(candidate) => {
                self.candidate = Some(candidate);
                self.resolve_selected();
                self.push_event(format!("bound platform {}", profile.id));
            }
            Err(error) => self.set_error(format!("failed to bind {}: {error}", profile.id)),
        }
    }

    /// Sensor/Mode 切换只重跑 resolver，复用当前 provider candidate。
    pub(crate) fn select_sensor_index(&mut self, index: usize) {
        if index >= self.sensor_choices.len() {
            self.set_error(format!("Sensor selection index {index} is out of range"));
            return;
        }
        if self.selected_sensor_index == index {
            return;
        }
        self.selected_sensor_index = index;
        self.resolve_selected();
        let label = self.sensor_choices[index].label.clone();
        self.push_event(format!("resolved Sensor selection {label}"));
    }

    fn resolve_selected(&mut self) {
        let Some(candidate) = self.candidate.as_ref() else {
            self.snapshot = None;
            return;
        };
        let selection = self.sensor_choices[self.selected_sensor_index]
            .selection
            .clone();
        match self.profiles.resolve_target(selection, candidate) {
            Ok(snapshot) => {
                self.snapshot = Some(Arc::new(snapshot));
                self.error = None;
            }
            Err(error) => {
                self.snapshot = None;
                self.set_error(format!("target resolution failed: {error}"));
            }
        }
    }

    pub(crate) fn capability_lines(&self) -> Vec<String> {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return vec![
                self.error
                    .clone()
                    .unwrap_or_else(|| "No resolved target".to_owned()),
            ];
        };
        let mut lines = Vec::new();
        for kind in CAPABILITY_KINDS {
            if let Some(descriptor) = snapshot.bindings.capability(kind) {
                let variants = descriptor
                    .supported_variants
                    .iter()
                    .map(|variant| format!("{variant:?}"))
                    .collect::<Vec<_>>()
                    .join(",");
                lines.push(format!(
                    "{:?} | {:?} | {} | [{}]",
                    descriptor.capability, descriptor.evidence, descriptor.driver, variants
                ));
            }
        }
        if lines.is_empty() {
            lines.push("Resolved target exposes no capability handles".to_owned());
        }
        lines
    }

    pub(crate) fn action_status(&self, action: ConsoleAction) -> Result<(), String> {
        if matches!(action, ConsoleAction::CancelAll) {
            return if self.operations.is_empty() && self.sessions.is_empty() {
                Err("no active jobs or sessions".to_owned())
            } else {
                Ok(())
            };
        }
        let snapshot = self.snapshot.as_deref().ok_or_else(|| {
            self.error
                .clone()
                .unwrap_or_else(|| "no resolved target".to_owned())
        })?;
        match action {
            ConsoleAction::Dump => {
                let ResolvedTargetBindings::Cv610(bindings) = snapshot.bindings.as_ref() else {
                    return Err("resolved target is not CV610".to_owned());
                };
                let handle = bindings
                    .dump
                    .as_ref()
                    .ok_or_else(|| "resolved target has no Dump handle".to_owned())?;
                let kind = self.preferred_dump_kind()?;
                let variant = match kind {
                    VerifiedDumpKind::Raw10 => CapabilityVariant::Raw10,
                    VerifiedDumpKind::Raw12 => CapabilityVariant::Raw12,
                    VerifiedDumpKind::Jpeg => CapabilityVariant::Jpeg,
                    VerifiedDumpKind::Nv21 => CapabilityVariant::Nv21,
                };
                if !handle.descriptor.supported_variants.contains(&variant) {
                    return Err(format!("preferred {kind:?} is not resolved"));
                }
                Ok(())
            }
            ConsoleAction::StreamRecord => {
                let ResolvedTargetBindings::Cv610(bindings) = snapshot.bindings.as_ref() else {
                    return Err("resolved target is not CV610".to_owned());
                };
                if bindings.stream.is_none() {
                    return Err("resolved target has no Stream handle".to_owned());
                }
                self.stream_request().map(|_| ())
            }
            ConsoleAction::SshCapture => {
                let ResolvedTargetBindings::SshManaged(bindings) = snapshot.bindings.as_ref()
                else {
                    return Err("resolved target is not SSH-managed".to_owned());
                };
                if bindings.command.is_none() {
                    return Err("resolved target has no typed Command handle".to_owned());
                }
                if bindings.remote_file.is_none() {
                    return Err(
                        "resolved target has no RemoteFile handle for capture discovery".to_owned(),
                    );
                }
                self.capture_request().map(|_| ())
            }
            ConsoleAction::RemoteFetch => {
                self.require_remote_handle(snapshot)?;
                self.remote_fetch_inputs().map(|_| ())
            }
            ConsoleAction::RemoteWatch => {
                self.require_remote_handle(snapshot)?;
                self.args
                    .remote_format
                    .ok_or_else(|| "--remote-format is required for watch".to_owned())?;
                Ok(())
            }
            ConsoleAction::CancelAll => unreachable!(),
        }
    }

    pub(crate) fn perform(&mut self, action: ConsoleAction) {
        if let Err(error) = self.action_status(action) {
            self.set_error(format!("{} disabled: {error}", action.label()));
            return;
        }
        let result = match action {
            ConsoleAction::Dump => self.submit_dump(),
            ConsoleAction::StreamRecord => self.submit_stream(),
            ConsoleAction::SshCapture => self.submit_capture(),
            ConsoleAction::RemoteFetch => self.submit_remote_fetch(),
            ConsoleAction::RemoteWatch => self.submit_remote_watch(),
            ConsoleAction::CancelAll => {
                self.cancel_all();
                Ok(())
            }
        };
        if let Err(error) = result {
            self.set_error(error);
        }
    }

    fn submit_dump(&mut self) -> Result<(), String> {
        let snapshot = self
            .snapshot
            .clone()
            .ok_or_else(|| "no resolved target".to_owned())?;
        let kind = self.preferred_dump_kind()?;
        let asset_id = self.next_asset_id("tui-cv610")?;
        let request = VerifiedDumpRequest::new(
            kind,
            asset_id,
            format!("tui-cv610-{}", self.next_asset.saturating_sub(1)),
        )
        .map_err(|error| error.to_string())?;
        let operation = self
            .controller
            .submit_dump(snapshot, request, DumpTimeouts::default())
            .map_err(|error| error.to_string())?;
        self.operations
            .insert(operation.clone(), OperationContext::Dump);
        self.update_job(operation.as_str(), "Queued");
        Ok(())
    }

    fn submit_stream(&mut self) -> Result<(), String> {
        let snapshot = self
            .snapshot
            .clone()
            .ok_or_else(|| "no resolved target".to_owned())?;
        let request = self.stream_request()?;
        let duration = Duration::from_secs(
            self.args
                .stream_duration_secs
                .ok_or_else(|| "--stream-duration-secs is required".to_owned())?
                .get(),
        );
        let session = self
            .controller
            .submit_stream(snapshot, request, StreamTimeouts::default())
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now()
            .checked_add(duration)
            .ok_or_else(|| "stream duration deadline overflow".to_owned())?;
        self.sessions.insert(
            session.clone(),
            StreamContext {
                deadline,
                close_requested: false,
            },
        );
        self.update_job(session.as_str(), "Streaming (finite)");
        Ok(())
    }

    fn submit_capture(&mut self) -> Result<(), String> {
        let snapshot = self
            .snapshot
            .clone()
            .ok_or_else(|| "no resolved target".to_owned())?;
        let request = self.capture_request()?;
        let format = self
            .args
            .remote_format
            .ok_or_else(|| "--remote-format is required for capture discovery".to_owned())?
            .into();
        let operation = self
            .controller
            .submit_command(Arc::clone(&snapshot), request, RemoteTimeouts::default())
            .map_err(|error| error.to_string())?;
        self.operations.insert(
            operation.clone(),
            OperationContext::Capture { snapshot, format },
        );
        self.update_job(operation.as_str(), "Queued typed command");
        Ok(())
    }

    fn submit_remote_fetch(&mut self) -> Result<(), String> {
        let snapshot = self
            .snapshot
            .clone()
            .ok_or_else(|| "no resolved target".to_owned())?;
        let request = self.remote_fetch_request()?;
        let operation = self
            .controller
            .submit_remote_fetch(snapshot, request, RemoteTimeouts::default())
            .map_err(|error| error.to_string())?;
        self.operations
            .insert(operation.clone(), OperationContext::Remote);
        self.update_job(operation.as_str(), "Queued remote fetch");
        Ok(())
    }

    fn submit_remote_watch(&mut self) -> Result<(), String> {
        let snapshot = self
            .snapshot
            .clone()
            .ok_or_else(|| "no resolved target".to_owned())?;
        let format = self
            .args
            .remote_format
            .ok_or_else(|| "--remote-format is required for watch".to_owned())?
            .into();
        let request = RemoteWatchRequest {
            asset_id_prefix: "tui-ssh-watch".to_owned(),
            format,
        };
        let timeouts = RemoteTimeouts {
            connect: Duration::from_secs(10),
            idle: Duration::from_secs(10),
            overall: Duration::from_secs(24 * 60 * 60),
        };
        let operation = self
            .controller
            .submit_remote_watch(snapshot, request, timeouts)
            .map_err(|error| error.to_string())?;
        self.operations
            .insert(operation.clone(), OperationContext::Watch);
        self.update_job(operation.as_str(), "Watching profile remote root");
        Ok(())
    }

    fn preferred_dump_kind(&self) -> Result<VerifiedDumpKind, String> {
        let profile = self
            .selected_profile()
            .ok_or_else(|| "selected platform no longer exists".to_owned())?;
        let PlatformConfig::HisiliconCv610(config) = &profile.config else {
            return Err("selected profile is not CV610".to_owned());
        };
        match config.dump.preferred_raw_bits {
            10 => Ok(VerifiedDumpKind::Raw10),
            12 => Ok(VerifiedDumpKind::Raw12),
            bits => Err(format!("invalid profile preferred RAW bits: {bits}")),
        }
    }

    fn stream_request(&self) -> Result<StreamOpenRequest, String> {
        let profile = self
            .selected_profile()
            .ok_or_else(|| "selected platform no longer exists".to_owned())?;
        let PlatformConfig::HisiliconCv610(config) = &profile.config else {
            return Err("selected profile is not CV610".to_owned());
        };
        let duration = self
            .args
            .stream_duration_secs
            .ok_or_else(|| "--stream-duration-secs must be explicit".to_owned())?;
        let quota = self
            .args
            .stream_quota_bytes
            .ok_or_else(|| "--stream-quota-bytes must be explicit".to_owned())?;
        if duration.get() == 0 {
            return Err("stream duration must be finite and non-zero".to_owned());
        }
        if self.args.transport_destination.is_none() && self.args.annexb_destination.is_none() {
            return Err(
                "explicit --transport-destination or --annexb-destination is required".to_owned(),
            );
        }
        let request = StreamOpenRequest {
            channel: config.stream.channel,
            media: config.stream.media.clone(),
            cseq: 1,
            recording: StreamRecordingRequest {
                transport_destination: self.args.transport_destination.clone(),
                annexb_destination: self.args.annexb_destination.clone(),
                timestamp_sidecar_destination: self.args.timestamp_destination.clone(),
                quota_bytes: quota.get(),
            },
        };
        request.validate().map_err(|error| error.to_string())?;
        Ok(request)
    }

    fn capture_request(&self) -> Result<TypedCommandRequest, String> {
        let profile = self
            .selected_profile()
            .ok_or_else(|| "selected platform no longer exists".to_owned())?;
        let PlatformConfig::SshManaged(config) = &profile.config else {
            return Err("selected profile is not SSH-managed".to_owned());
        };
        if !self.recipes.contains(&config.capture_recipe) {
            return Err(format!(
                "typed capture recipe '{}' is not registered",
                config.capture_recipe
            ));
        }
        let format = self
            .args
            .capture_format
            .as_ref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "--capture-format must be explicit".to_owned())?;
        self.args.remote_format.ok_or_else(|| {
            "--remote-format is required to interpret the captured asset".to_owned()
        })?;
        TypedCommandRequest::new(config.capture_recipe.clone())
            .map(|request| {
                request
                    .with_parameter(
                        "output_dir",
                        CommandParameter::RemotePath(config.remote_artifact_dir.clone()),
                    )
                    .with_parameter("format", CommandParameter::Choice(format.clone()))
            })
            .map_err(|error| error.to_string())
    }

    fn remote_fetch_request(&mut self) -> Result<RemoteFileRequest, String> {
        let (path, format, source_name) = self.remote_fetch_inputs()?;
        Ok(RemoteFileRequest {
            asset_id: self.next_asset_id("tui-ssh-fetch")?,
            remote_path: path,
            expected_sha256: None,
            format,
            source_name,
        })
    }

    fn remote_fetch_inputs(&self) -> Result<(String, MediaFormat, String), String> {
        let path = self
            .args
            .remote_path
            .as_ref()
            .filter(|path| !path.is_empty() && !path.contains('\0'))
            .ok_or_else(|| "--remote-path must be explicit and contain no NUL".to_owned())?
            .clone();
        let format = self
            .args
            .remote_format
            .ok_or_else(|| "--remote-format must be explicit".to_owned())?
            .into();
        let source_name = Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("remote-asset")
            .to_owned();
        Ok((path, format, source_name))
    }

    fn require_remote_handle(&self, snapshot: &TargetResolutionSnapshot) -> Result<(), String> {
        let ResolvedTargetBindings::SshManaged(bindings) = snapshot.bindings.as_ref() else {
            return Err("resolved target is not SSH-managed".to_owned());
        };
        if bindings.remote_file.is_none() {
            return Err("resolved target has no RemoteFile handle".to_owned());
        }
        Ok(())
    }

    pub(crate) fn poll_controller_events(&mut self) {
        // 每类队列按 tick 限额，持续高流量也必须把控制权还给输入循环。
        for _ in 0..EVENTS_PER_QUEUE_TICK {
            match self.controller.try_recv() {
                Ok(Some(event)) => self.route_event(ControllerEvent::Dump(event)),
                Ok(None) => break,
                Err(error) => {
                    self.set_error(format!("Dump event poll failed: {error}"));
                    break;
                }
            }
        }
        for _ in 0..EVENTS_PER_QUEUE_TICK {
            match self.controller.try_recv_stream() {
                Ok(Some(event)) => self.route_event(ControllerEvent::Stream(event)),
                Ok(None) => break,
                Err(error) => {
                    self.set_error(format!("Stream event poll failed: {error}"));
                    break;
                }
            }
        }
        for _ in 0..EVENTS_PER_QUEUE_TICK {
            match self.controller.try_recv_remote() {
                Ok(Some(event)) => self.route_event(ControllerEvent::Remote(event)),
                Ok(None) => break,
                Err(error) => {
                    self.set_error(format!("Remote event poll failed: {error}"));
                    break;
                }
            }
        }
    }

    pub(crate) fn route_event(&mut self, event: ControllerEvent) {
        match event {
            ControllerEvent::Dump(event) => self.route_dump(event),
            ControllerEvent::Stream(event) => self.route_stream(event),
            ControllerEvent::Remote(event) => self.route_remote(event),
        }
    }

    fn route_dump(&mut self, event: DumpJobEvent) {
        self.update_job(event.operation_id.as_str(), &format!("{:?}", event.state));
        self.push_event(format!(
            "Dump {} {}: {:?}",
            event.operation_id.as_str(),
            event.service_id,
            event.state
        ));
        match &event.state {
            DumpJobState::Succeeded { asset_id, .. } => self.record_asset(asset_id),
            DumpJobState::Failed(error) => self.set_error(format!(
                "Dump {} failed: {error}",
                event.operation_id.as_str()
            )),
            DumpJobState::Queued | DumpJobState::Running(_) => {}
        }
        if event.state.is_terminal() {
            self.operations.remove(&event.operation_id);
        }
    }

    fn route_stream(&mut self, event: StreamSessionEvent) {
        self.update_job(event.session_id.as_str(), &format!("{:?}", event.event));
        self.push_event(format!(
            "Stream {} {}: {:?}",
            event.session_id.as_str(),
            event.service_id,
            event.event
        ));
        if let StreamServiceEvent::Terminal(terminal) = &event.event {
            if matches!(
                terminal,
                camera_toolbox_app::StreamTerminal::Failed(_)
                    | camera_toolbox_app::StreamTerminal::Forced { .. }
            ) {
                self.set_error(format!(
                    "Stream {} ended with {terminal:?}",
                    event.session_id.as_str()
                ));
            }
            self.sessions.remove(&event.session_id);
        }
    }

    fn route_remote(&mut self, event: RemoteJobEvent) {
        self.update_job(
            event.operation_id.as_str(),
            &remote_state_label(&event.state),
        );
        self.push_event(format!(
            "Remote {} {}: {}",
            event.operation_id.as_str(),
            event.service_id,
            remote_state_label(&event.state)
        ));
        match &event.state {
            RemoteJobState::CommandCompleted(result) => {
                let context = self.operations.remove(&event.operation_id);
                if matches!(result.terminal, CommandTerminal::Succeeded) {
                    if let (Some(path), Some(OperationContext::Capture { snapshot, format })) =
                        (result.artifact_path.clone(), context)
                    {
                        let request = RemoteDiscoveryRequest {
                            command_result_path: Some(path),
                            asset_id_prefix: "tui-ssh-capture".to_owned(),
                            format,
                        };
                        match self.controller.submit_remote_discover(
                            snapshot,
                            request,
                            RemoteTimeouts::default(),
                        ) {
                            Ok(operation) => {
                                self.operations
                                    .insert(operation.clone(), OperationContext::Remote);
                                self.update_job(operation.as_str(), "Discovering captured asset");
                            }
                            Err(error) => self.set_error(format!(
                                "typed capture succeeded but artifact discovery failed: {error}"
                            )),
                        }
                    } else {
                        self.set_error(
                            "typed capture succeeded without one explicit artifact path".to_owned(),
                        );
                    }
                } else {
                    self.set_error(format!("typed command ended as {:?}", result.terminal));
                }
            }
            RemoteJobState::AssetReady { asset_id, .. } => {
                self.record_asset(asset_id);
                self.operations.remove(&event.operation_id);
            }
            RemoteJobState::Watch(RemoteWatchEvent::AssetReady { result, .. }) => {
                self.record_asset(&result.asset.id);
            }
            RemoteJobState::Watch(RemoteWatchEvent::Terminal(_)) => {
                self.operations.remove(&event.operation_id);
            }
            RemoteJobState::Failed(failure) => {
                self.set_error(format!(
                    "Remote {} failed: {failure:?}",
                    event.operation_id.as_str()
                ));
                self.operations.remove(&event.operation_id);
            }
            RemoteJobState::Queued
            | RemoteJobState::Running(_)
            | RemoteJobState::Watch(RemoteWatchEvent::CandidateFailed { .. }) => {}
        }
    }

    fn record_asset(&mut self, asset_id: &AssetId) {
        match self.capture_store.get(asset_id) {
            Ok(Some(_)) => {
                self.assets.insert(asset_id.clone());
                self.push_event(format!("asset retained in CaptureStore: {asset_id}"));
            }
            Ok(None) => self.set_error(format!(
                "terminal asset {asset_id} is missing from CaptureStore"
            )),
            Err(error) => self.set_error(format!("CaptureStore lookup failed: {error}")),
        }
    }

    pub(crate) fn close_expired_streams(&mut self, now: Instant) {
        let expired = self
            .sessions
            .iter()
            .filter(|(_, context)| !context.close_requested && now >= context.deadline)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            if self.controller.request_stream_close(&id) {
                if let Some(context) = self.sessions.get_mut(&id) {
                    context.close_requested = true;
                }
                self.update_job(id.as_str(), "Finite duration reached; closing");
            }
        }
    }

    pub(crate) fn cancel_all(&mut self) {
        for operation in self.operations.keys() {
            let _ = self.controller.cancel(operation);
        }
        for session in self.sessions.keys() {
            let _ = self.controller.request_stream_close(session);
        }
        self.push_event("cancellation requested for all active jobs/sessions".to_owned());
    }

    pub(crate) fn shutdown(&mut self) {
        if self.shutdown_started {
            return;
        }
        self.shutdown_started = true;
        self.cancel_all();
        self.controller.force_close_all_streams();
    }

    pub(crate) fn snapshot_text(&self) -> String {
        let mut output = String::from("Camera Toolbox Platform Console Snapshot\n");
        output.push_str(&format!("profiles: {}\n", self.profile_count()));
        if let Some(profile) = self.selected_profile() {
            output.push_str(&format!(
                "platform: {} | {} | {}\n",
                profile.id,
                profile.display_name,
                platform_variant_name(&profile.config)
            ));
        } else {
            output.push_str("platform: none\n");
        }
        let sensor = self
            .sensor_choices
            .get(self.selected_sensor_index)
            .map_or("none", |choice| choice.label.as_str());
        output.push_str(&format!("sensor: {sensor}\n"));
        output.push_str(if self.snapshot.is_some() {
            "resolution: resolved\n"
        } else {
            "resolution: unavailable\n"
        });
        output.push_str("capabilities:\n");
        for line in self.capability_lines() {
            output.push_str(&format!("  - {line}\n"));
        }
        output.push_str("actions:\n");
        for action in [
            ConsoleAction::Dump,
            ConsoleAction::StreamRecord,
            ConsoleAction::SshCapture,
            ConsoleAction::RemoteFetch,
            ConsoleAction::RemoteWatch,
        ] {
            let status = self.action_status(action).map_or_else(
                |reason| format!("disabled ({reason})"),
                |()| "enabled".to_owned(),
            );
            output.push_str(&format!("  - {}: {status}\n", action.label()));
        }
        let stats = self.capture_store.stats();
        match stats {
            Ok(stats) => output.push_str(&format!(
                "jobs: {} active | assets: {} | capture_store_bytes: {}\n",
                self.operations.len() + self.sessions.len(),
                stats.asset_count,
                stats.published_bytes
            )),
            Err(error) => output.push_str(&format!("capture_store_error: {error}\n")),
        }
        if let Some(error) = self.error.as_deref() {
            output.push_str(&format!("error: {error}\n"));
        }
        if let Some(message) = self.startup_message.as_deref() {
            output.push_str(&format!("notice: {message}\n"));
        }
        output
    }

    fn next_asset_id(&mut self, prefix: &str) -> Result<AssetId, String> {
        let sequence = self.next_asset;
        self.next_asset = self
            .next_asset
            .checked_add(1)
            .ok_or_else(|| "asset id sequence exhausted".to_owned())?;
        AssetId::new(format!("{prefix}-{sequence}")).map_err(|error| error.to_string())
    }

    fn update_job(&mut self, id: &str, state: &str) {
        if let Some(existing) = self.jobs.iter_mut().find(|(job_id, _)| job_id == id) {
            existing.1 = state.to_owned();
            return;
        }
        if self.jobs.len() == JOB_HISTORY_LIMIT {
            self.jobs.pop_front();
        }
        self.jobs.push_back((id.to_owned(), state.to_owned()));
    }

    fn push_event(&mut self, message: String) {
        if self.event_log.len() == EVENT_LOG_LIMIT {
            self.event_log.pop_front();
        }
        self.event_log.push_back(message);
    }

    pub(crate) fn set_error(&mut self, error: String) {
        self.push_event(format!("ERROR: {error}"));
        self.error = Some(error);
    }
}

impl Drop for ConsoleState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn sensor_choices(store: &ProfileStore) -> Vec<SensorChoice> {
    let mut choices = vec![SensorChoice {
        label: "Unbound".to_owned(),
        selection: SensorSelection::Unbound,
    }];
    for sensor in store.sensors() {
        for mode in &sensor.profile.modes {
            choices.push(SensorChoice {
                label: format!("{} / {}", sensor.display_name, mode.id),
                selection: SensorSelection::Mode(SensorModeKey {
                    sensor_id: sensor.id.clone(),
                    mode_id: mode.id.clone(),
                }),
            });
        }
    }
    choices
}

pub(crate) fn platform_variant_name(config: &PlatformConfig) -> &'static str {
    match config {
        PlatformConfig::Local(_) => "local",
        PlatformConfig::HisiliconCv610(_) => "hisilicon-cv610-pq",
        PlatformConfig::SshManaged(_) => "ssh-managed",
    }
}

fn remote_state_label(state: &RemoteJobState) -> String {
    match state {
        RemoteJobState::Queued => "Queued".to_owned(),
        RemoteJobState::Running(stage) => format!("Running {stage:?}"),
        RemoteJobState::CommandCompleted(result) => {
            format!("Command completed: {:?}", result.terminal)
        }
        RemoteJobState::AssetReady { asset_id, .. } => format!("Asset ready: {asset_id}"),
        RemoteJobState::Watch(RemoteWatchEvent::AssetReady { result, .. }) => {
            format!("Watched asset: {}", result.asset.id)
        }
        RemoteJobState::Watch(RemoteWatchEvent::CandidateFailed { path, error }) => {
            format!("Watch candidate failed {path}: {error}")
        }
        RemoteJobState::Watch(RemoteWatchEvent::Terminal(terminal)) => {
            format!("Watch terminal: {terminal:?}")
        }
        RemoteJobState::Failed(error) => format!("Failed: {error:?}"),
    }
}
