//! GUI 平台运行时：静态 profile 选择、typed binding/resolution、后台 jobs 与 event 路由。

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use camera_toolbox_adapters::{
    PlatformRegistry,
    platforms::{
        hisilicon_cv610::HisiliconCv610Provider,
        ssh_managed::{
            CommandRecipeRegistry, CredentialResolver, ProductionCredentialResolver,
            SshManagedPlatformProvider, production_recipe_registry_from_env,
        },
    },
};
use camera_toolbox_app::{
    CaptureStore, CaptureStoreLimits, CommandParameter, CommandTerminal, DumpJobState,
    DumpTimeouts, LatestDecodedFrameSlot, OperationId, PlatformBindings, PlatformConfig,
    PlatformController, PlatformProfileId, ProfileStore, RemoteDiscoveryRequest, RemoteFileRequest,
    RemoteJobState, RemoteOpenDisposition, RemoteTimeouts, RemoteWatchEvent, RemoteWatchRequest,
    ResolvedTargetBindings, SensorSelection, StreamSessionEvent, StreamSessionId, StreamTimeouts,
    TargetResolutionSnapshot, TypedCommandRequest, VerifiedDumpKind, VerifiedDumpRequest,
};
use camera_toolbox_core::{AssetId, BayerPattern, EphemeralAsset, MediaFormat};
use eframe::egui;
use secrecy::SecretString;

use super::{
    DeviceManagerAction, DeviceManagerState, StreamPanelAction, StreamPanelState,
    render_stream_panel,
};

#[derive(Debug, Clone)]
pub(crate) struct AssetOpenSpec {
    pub(crate) bayer: BayerPattern,
}

pub(crate) enum PlatformEffect {
    OpenAsset {
        asset: Arc<EphemeralAsset>,
        snapshot: Arc<TargetResolutionSnapshot>,
        foreground: bool,
        spec: AssetOpenSpec,
    },
}

pub(crate) enum PlatformUiAction {
    OpenRaw,
    Stream(StreamPanelAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenIntent {
    None,
    Foreground,
}

#[derive(Clone)]
struct JobContext {
    snapshot: Arc<TargetResolutionSnapshot>,
    intent: OpenIntent,
    format: MediaFormat,
    spec: AssetOpenSpec,
}

#[derive(Debug, Clone)]
struct CapturePanelState {
    dump_kind: VerifiedDumpKind,
    manual_bayer: BayerPattern,
    remote_path: String,
    remote_format: MediaFormat,
    remote_width: u32,
    remote_height: u32,
    remote_stride: usize,
    watcher_operation: Option<OperationId>,
}

impl Default for CapturePanelState {
    fn default() -> Self {
        Self {
            dump_kind: VerifiedDumpKind::Raw12,
            manual_bayer: BayerPattern::Rggb,
            remote_path: String::new(),
            remote_format: MediaFormat::Jpeg,
            remote_width: 1920,
            remote_height: 1080,
            remote_stride: 0,
            watcher_operation: None,
        }
    }
}

pub(crate) struct LiveRuntime {
    controller: PlatformController,
    capture_store: CaptureStore,
    registry: PlatformRegistry,
    credential_resolver: Arc<ProductionCredentialResolver>,
    recipes: Arc<CommandRecipeRegistry>,
    pub(crate) profiles: ProfileStore,
    selected_profile: Option<PlatformProfileId>,
    sensor_selection: SensorSelection,
    candidate: Option<PlatformBindings>,
    snapshot: Option<Arc<TargetResolutionSnapshot>>,
    binding_error: Option<String>,
    pub(crate) panel: StreamPanelState,
    capture_panel: CapturePanelState,
    device_manager: DeviceManagerState,
    jobs: BTreeMap<OperationId, String>,
    submissions: BTreeMap<OperationId, JobContext>,
    assets: BTreeMap<AssetId, Arc<TargetResolutionSnapshot>>,
    next_asset: u64,
    bind_count: u64,
    startup_message: Option<String>,
}

impl LiveRuntime {
    pub(crate) fn new() -> Result<Self, String> {
        let limits = CaptureStoreLimits::new(256 * 1024 * 1024, 1024 * 1024 * 1024)
            .map_err(|error| error.to_string())?;
        let capture_store = CaptureStore::new(limits);
        let credential_resolver = Arc::new(ProductionCredentialResolver::new());
        let resolver: Arc<dyn CredentialResolver> = credential_resolver.clone();
        let recipe_registry = production_recipe_registry_from_env()
            .map_err(|error| format!("invalid production SSH recipe configuration: {error}"))?;
        let recipes = Arc::new(recipe_registry);
        let registry = PlatformRegistry::new(
            HisiliconCv610Provider::default(),
            SshManagedPlatformProvider::production(resolver, Arc::clone(&recipes)),
        );
        let path = ProfileStore::project_file_path().map_err(|error| error.to_string())?;
        let (profiles, startup_message) = if path.exists() {
            match ProfileStore::load_from_path(&path) {
                Ok(store) => (store, None),
                Err(error) => (
                    ProfileStore::with_builtin_local().map_err(|error| error.to_string())?,
                    Some(format!(
                        "Profile store could not be loaded from {}: {error}",
                        path.display()
                    )),
                ),
            }
        } else {
            let store = ProfileStore::with_builtin_local().map_err(|error| error.to_string())?;
            let message = store
                .save_project()
                .err()
                .map(|error| format!("Initial profile store could not be saved: {error}"));
            (store, message)
        };
        let selected = profiles
            .platforms()
            .next()
            .map(|profile| profile.id.clone());
        let mut runtime = Self {
            controller: PlatformController::new(capture_store.clone()),
            capture_store,
            registry,
            credential_resolver,
            recipes,
            profiles,
            selected_profile: None,
            sensor_selection: SensorSelection::Unbound,
            candidate: None,
            snapshot: None,
            binding_error: None,
            panel: StreamPanelState::default(),
            capture_panel: CapturePanelState::default(),
            device_manager: DeviceManagerState::default(),
            jobs: BTreeMap::new(),
            submissions: BTreeMap::new(),
            assets: BTreeMap::new(),
            next_asset: 1,
            bind_count: 0,
            startup_message,
        };
        if let Some(selected) = selected {
            runtime.select_platform(selected);
        }
        Ok(runtime)
    }

    pub(crate) fn render_target_toolbar(&mut self, ui: &mut egui::Ui) {
        let profiles: Vec<_> = self
            .profiles
            .platforms()
            .map(|profile| (profile.id.clone(), profile.display_name.clone()))
            .collect();
        let current_name = self
            .selected_profile
            .as_ref()
            .and_then(|id| self.profiles.platform(id))
            .map_or("No platform", |profile| profile.display_name.as_str());
        let mut selected = self.selected_profile.clone();
        egui::ComboBox::from_id_salt("platform_profile_selector")
            .selected_text(current_name)
            .show_ui(ui, |ui| {
                for (id, name) in &profiles {
                    ui.selectable_value(&mut selected, Some(id.clone()), name);
                }
            });
        if selected != self.selected_profile
            && let Some(selected) = selected
        {
            self.select_platform(selected);
        }

        let mut selection = self.sensor_selection.clone();
        egui::ComboBox::from_id_salt("sensor_mode_selector")
            .selected_text(sensor_selection_label(&selection))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut selection, SensorSelection::Unbound, "Sensor: Unbound");
                for sensor in self.profiles.sensors() {
                    for mode in &sensor.profile.modes {
                        let value = SensorSelection::Mode(camera_toolbox_app::SensorModeKey {
                            sensor_id: sensor.id.clone(),
                            mode_id: mode.id.clone(),
                        });
                        ui.selectable_value(
                            &mut selection,
                            value,
                            format!("{} / {}", sensor.display_name, mode.id),
                        );
                    }
                }
            });
        if selection != self.sensor_selection {
            self.select_sensor(selection);
        }
        if ui.button("Device Manager...").clicked() {
            if let Some(profile) = self
                .selected_profile
                .as_ref()
                .and_then(|id| self.profiles.platform(id))
            {
                self.device_manager.edit(profile);
            } else {
                self.device_manager.open = true;
            }
        }
        if let Some(snapshot) = &self.snapshot {
            ui.monospace(format!(
                "target {}",
                &snapshot.aggregate_hash.to_hex()[..12]
            ));
            render_capability_chips(ui, snapshot);
        }
        if let Some(error) = self.binding_error.as_deref() {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }
    }

    pub(crate) fn render_panel(
        &mut self,
        ui: &mut egui::Ui,
        live_running: bool,
    ) -> Option<PlatformUiAction> {
        let profile = self
            .selected_profile
            .as_ref()
            .and_then(|id| self.profiles.platform(id))
            .cloned();
        let Some(profile) = profile else {
            ui.label("No platform profile configured.");
            return None;
        };
        let mut action = None;
        match profile.config {
            PlatformConfig::Local(_) => {
                ui.heading("Local");
                ui.label("Open local unpacked u16le RAW files.");
                if ui.button("Open RAW...").clicked() {
                    action = Some(PlatformUiAction::OpenRaw);
                }
            }
            PlatformConfig::HisiliconCv610(config) => {
                ui.heading("CV610 Capture");
                ui.label(format!(
                    "{} — Dump {} / Stream {}",
                    config.host, config.dump.port, config.stream.port
                ));
                self.render_dump_controls(ui);
                ui.separator();
                if let Some(stream_action) = render_stream_panel(ui, &mut self.panel, live_running)
                {
                    action = Some(PlatformUiAction::Stream(stream_action));
                }
            }
            PlatformConfig::SshManaged(config) => {
                self.render_ssh_controls(ui, &config);
            }
        }
        ui.separator();
        self.render_jobs_assets(ui);
        action
    }

    pub(crate) fn show_device_manager(&mut self, context: &egui::Context) {
        let actions = self.device_manager.show(context);
        for action in actions {
            let result = self.apply_device_manager_action(action);
            self.device_manager.message = Some(match result {
                Ok(message) => message,
                Err(error) => format!("Error: {error}"),
            });
        }
    }

    fn apply_device_manager_action(
        &mut self,
        action: DeviceManagerAction,
    ) -> Result<String, String> {
        match action {
            DeviceManagerAction::Commit(draft) => {
                let profile = draft.build()?;
                if let Some(original) = &draft.original_id {
                    if *original != profile.id {
                        return Err(
                            "profile ID is immutable while editing; duplicate it instead"
                                .to_owned(),
                        );
                    }
                    self.profiles
                        .replace_platform(profile.clone())
                        .map_err(|error| error.to_string())?;
                } else {
                    self.profiles
                        .insert_platform(profile.clone())
                        .map_err(|error| error.to_string())?;
                }
                self.profiles
                    .save_project()
                    .map_err(|error| error.to_string())?;
                if self.selected_profile.as_ref() == Some(&profile.id) {
                    self.select_platform(profile.id.clone());
                }
                Ok(format!("Saved profile {}", profile.id))
            }
            DeviceManagerAction::Delete(id) => {
                if self.profiles.platforms().count() <= 1 {
                    return Err("at least one platform profile must remain".to_owned());
                }
                self.profiles
                    .remove_platform(&id)
                    .map_err(|error| error.to_string())?;
                self.profiles
                    .save_project()
                    .map_err(|error| error.to_string())?;
                if self.selected_profile.as_ref() == Some(&id) {
                    let next = self
                        .profiles
                        .platforms()
                        .next()
                        .map(|profile| profile.id.clone());
                    if let Some(next) = next {
                        self.select_platform(next);
                    }
                }
                Ok(format!("Deleted profile {id}"))
            }
            DeviceManagerAction::Import(path) => {
                let imported =
                    ProfileStore::load_from_path(&path).map_err(|error| error.to_string())?;
                let next = imported
                    .platforms()
                    .next()
                    .map(|profile| profile.id.clone());
                self.profiles = imported;
                self.profiles
                    .save_project()
                    .map_err(|error| error.to_string())?;
                if let Some(next) = next {
                    self.select_platform(next);
                }
                Ok(format!("Imported {}", path.display()))
            }
            DeviceManagerAction::Export(path) => {
                self.profiles
                    .save_to_path(&path)
                    .map_err(|error| error.to_string())?;
                Ok(format!("Exported {}", path.display()))
            }
            DeviceManagerAction::RegisterSessionSecret { id, secret } => {
                if secret.is_empty() {
                    return Err("session secret must not be empty".to_owned());
                }
                let reference = self
                    .credential_resolver
                    .register_session_password(&id, SecretString::from(secret))?;
                if let Some(draft) = self.device_manager.draft.as_mut()
                    && let PlatformConfig::SshManaged(config) = &mut draft.config
                {
                    config.credential_ref = reference.clone();
                }
                Ok(format!("Registered {reference} for this process only"))
            }
        }
    }

    fn select_platform(&mut self, id: PlatformProfileId) {
        self.selected_profile = Some(id.clone());
        self.candidate = None;
        self.snapshot = None;
        self.binding_error = None;
        let Some(profile) = self.profiles.platform(&id) else {
            self.binding_error = Some(format!("platform profile {id} no longer exists"));
            return;
        };
        self.bind_count = self.bind_count.saturating_add(1);
        match self.registry.bind(profile) {
            Ok(candidate) => {
                self.candidate = Some(candidate);
                self.resolve_selected();
            }
            Err(error) => {
                self.binding_error = Some(match &profile.config {
                    PlatformConfig::SshManaged(config)
                        if !self.recipes.contains(&config.capture_recipe) =>
                    {
                        format!(
                            "SSH typed recipe '{}' is unavailable. Configure the complete CAMERA_TOOLBOX_SSH_RECIPE_* environment group with a verified absolute program, typed flags, allowed formats, remote root and path-on-stdout contract; no command was sent.",
                            config.capture_recipe
                        )
                    }
                    _ => error.to_string(),
                });
            }
        }
    }

    fn select_sensor(&mut self, selection: SensorSelection) {
        self.sensor_selection = selection;
        self.resolve_selected();
    }

    fn resolve_selected(&mut self) {
        let Some(candidate) = self.candidate.as_ref() else {
            self.snapshot = None;
            return;
        };
        match self
            .profiles
            .resolve_target(self.sensor_selection.clone(), candidate)
        {
            Ok(snapshot) => {
                self.snapshot = Some(Arc::new(snapshot));
                self.binding_error = None;
            }
            Err(error) => {
                self.snapshot = None;
                self.binding_error = Some(error.to_string());
            }
        }
    }

    fn render_dump_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (kind, label) in [
                (VerifiedDumpKind::Raw10, "RAW10"),
                (VerifiedDumpKind::Raw12, "RAW12"),
                (VerifiedDumpKind::Jpeg, "JPEG"),
                (VerifiedDumpKind::Nv21, "NV21"),
            ] {
                ui.selectable_value(&mut self.capture_panel.dump_kind, kind, label);
            }
        });
        if self.capture_panel.dump_kind.raw_bit_depth().is_some() {
            ui.horizontal(|ui| {
                ui.label("RAW Bayer interpretation");
                for (pattern, label) in [
                    (BayerPattern::Rggb, "RGGB"),
                    (BayerPattern::Grbg, "GRBG"),
                    (BayerPattern::Gbrg, "GBRG"),
                    (BayerPattern::Bggr, "BGGR"),
                ] {
                    ui.selectable_value(&mut self.capture_panel.manual_bayer, pattern, label);
                }
            });
            if matches!(self.sensor_selection, SensorSelection::Unbound) {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "Sensor is Unbound; Bayer choice above is an explicit manual interpretation.",
                );
            }
        }
        let enabled = matches!(
            self.snapshot
                .as_deref()
                .map(|snapshot| snapshot.bindings.as_ref()),
            Some(ResolvedTargetBindings::Cv610(_))
        );
        ui.horizontal(|ui| {
            if ui.add_enabled(enabled, egui::Button::new("Dump")).clicked() {
                self.submit_dump(OpenIntent::None);
            }
            if ui
                .add_enabled(enabled, egui::Button::new("Dump and Open"))
                .clicked()
            {
                self.submit_dump(OpenIntent::Foreground);
            }
        });
    }

    fn render_ssh_controls(
        &mut self,
        ui: &mut egui::Ui,
        config: &camera_toolbox_app::SshManagedConfig,
    ) {
        ui.heading("SSH-managed");
        ui.label(format!(
            "{}@{}:{}",
            config.username, config.host, config.port
        ));
        ui.label(format!("Remote root: {}", config.remote_artifact_dir));
        ui.label(format!("Typed recipe: {}", config.capture_recipe));
        ui.label(match config.command_subsystem.as_deref() {
            Some(subsystem) => format!("Command transport: CTARGV1 subsystem '{subsystem}'"),
            None => "Command transport: standard SSH exec (no argv helper)".to_owned(),
        });
        ui.label(match config.remote_event_subsystem.as_deref() {
            Some(subsystem) => format!("Discovery: event subsystem '{subsystem}'"),
            None => "Discovery: directory polling fallback".to_owned(),
        });
        if !self.recipes.contains(&config.capture_recipe) {
            ui.colored_label(
                egui::Color32::YELLOW,
                "Typed capture recipe unavailable; capture is disabled instead of reporting fake success.",
            );
        }
        let enabled = self.snapshot.is_some();
        if ui
            .add_enabled(enabled, egui::Button::new("Remote Capture"))
            .clicked()
        {
            self.submit_remote_command(config);
        }
        ui.separator();
        ui.heading("Remote Files");
        ui.text_edit_singleline(&mut self.capture_panel.remote_path);
        remote_format_selector(ui, &mut self.capture_panel.remote_format);
        self.render_remote_geometry(ui);
        if ui
            .add_enabled(
                enabled && !self.capture_panel.remote_path.is_empty(),
                egui::Button::new("Fetch and Open"),
            )
            .clicked()
        {
            self.submit_remote_fetch();
        }
        ui.separator();
        ui.heading("Watch");
        ui.label(if config.passive_watch_auto_open {
            "Stable watched assets open only in background."
        } else {
            "Stable watched assets are collected but never auto-opened."
        });
        if let Some(operation) = self.capture_panel.watcher_operation.clone() {
            if ui.button("Stop Watch").clicked() && self.controller.cancel(&operation) {
                self.capture_panel.watcher_operation = None;
            }
        } else if ui
            .add_enabled(enabled, egui::Button::new("Start Watch"))
            .clicked()
        {
            self.submit_watch();
        }
    }

    fn render_remote_geometry(&mut self, ui: &mut egui::Ui) {
        if !matches!(
            self.capture_panel.remote_format,
            MediaFormat::RawPacked { .. } | MediaFormat::Yuv420Sp { .. }
        ) {
            return;
        }
        egui::Grid::new("remote_media_geometry")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Width");
                ui.add(
                    egui::DragValue::new(&mut self.capture_panel.remote_width).range(1..=u32::MAX),
                );
                ui.end_row();
                ui.label("Height");
                ui.add(
                    egui::DragValue::new(&mut self.capture_panel.remote_height).range(1..=u32::MAX),
                );
                ui.end_row();
                ui.label("Stride bytes");
                ui.add(
                    egui::DragValue::new(&mut self.capture_panel.remote_stride)
                        .range(1..=usize::MAX),
                );
                ui.end_row();
            });
    }

    fn render_jobs_assets(&self, ui: &mut egui::Ui) {
        ui.collapsing(format!("Jobs ({})", self.jobs.len()), |ui| {
            for (id, state) in self.jobs.iter().rev().take(16) {
                ui.label(format!("{} — {}", id.as_str(), state));
            }
        });
        ui.collapsing(format!("Assets ({})", self.assets.len()), |ui| {
            for id in self.assets.keys().rev().take(16) {
                ui.label(id.as_str());
            }
        });
        if let Ok(stats) = self.capture_store.stats() {
            ui.weak(format!(
                "Ephemeral source {} MiB / {} assets",
                stats.published_bytes / (1024 * 1024),
                stats.asset_count
            ));
        }
        if let Some(message) = self.startup_message.as_deref() {
            ui.colored_label(egui::Color32::YELLOW, message);
        }
    }

    fn submit_dump(&mut self, intent: OpenIntent) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let asset_id = match self.next_asset_id("cv610") {
            Ok(id) => id,
            Err(error) => {
                self.binding_error = Some(error);
                return;
            }
        };
        let source_name = format!("{}-{:?}", asset_id, self.capture_panel.dump_kind).to_lowercase();
        let request =
            match VerifiedDumpRequest::new(self.capture_panel.dump_kind, asset_id, source_name) {
                Ok(request) => request,
                Err(error) => {
                    self.binding_error = Some(error.to_string());
                    return;
                }
            };
        match self
            .controller
            .submit_dump(Arc::clone(&snapshot), request, DumpTimeouts::default())
        {
            Ok(operation) => {
                self.jobs.insert(operation.clone(), "Queued".to_owned());
                self.submissions.insert(
                    operation,
                    JobContext {
                        snapshot,
                        intent,
                        format: dump_media_format(self.capture_panel.dump_kind),
                        spec: AssetOpenSpec {
                            bayer: self.selected_bayer(),
                        },
                    },
                );
            }
            Err(error) => self.binding_error = Some(error.to_string()),
        }
    }

    fn submit_remote_command(&mut self, config: &camera_toolbox_app::SshManagedConfig) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let mut request = match TypedCommandRequest::new(config.capture_recipe.clone()) {
            Ok(request) => request,
            Err(error) => {
                self.binding_error = Some(error.to_string());
                return;
            }
        };
        // Known typed parameters are supplied only when the registered recipe explicitly declares them.
        if let Some(recipe) = self.recipes.get(&config.capture_recipe) {
            for parameter in &recipe.parameters {
                match parameter.name.as_str() {
                    "output_dir" => {
                        request.parameters.insert(
                            parameter.name.clone(),
                            CommandParameter::RemotePath(config.remote_artifact_dir.clone()),
                        );
                    }
                    "format" => {
                        request.parameters.insert(
                            parameter.name.clone(),
                            CommandParameter::Choice(
                                remote_format_name(&self.capture_panel.remote_format).to_owned(),
                            ),
                        );
                    }
                    _ => {}
                }
            }
        }
        match self.controller.submit_command(
            Arc::clone(&snapshot),
            request,
            RemoteTimeouts::default(),
        ) {
            Ok(operation) => {
                self.jobs.insert(operation.clone(), "Queued".to_owned());
                self.submissions
                    .insert(operation, self.remote_job_context(snapshot));
            }
            Err(error) => self.binding_error = Some(error.to_string()),
        }
    }

    fn submit_remote_fetch(&mut self) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let asset_id = match self.next_asset_id("ssh-file") {
            Ok(id) => id,
            Err(error) => {
                self.binding_error = Some(error);
                return;
            }
        };
        let path = self.capture_panel.remote_path.clone();
        let source_name = Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("remote-asset")
            .to_owned();
        let request = RemoteFileRequest {
            asset_id,
            remote_path: path,
            expected_sha256: None,
            format: self.capture_panel.remote_format.clone(),
            source_name,
        };
        match self.controller.submit_remote_fetch(
            Arc::clone(&snapshot),
            request,
            RemoteTimeouts::default(),
        ) {
            Ok(operation) => {
                self.jobs.insert(operation.clone(), "Queued".to_owned());
                self.submissions
                    .insert(operation, self.remote_job_context(snapshot));
            }
            Err(error) => self.binding_error = Some(error.to_string()),
        }
    }

    fn submit_watch(&mut self) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let request = RemoteWatchRequest {
            asset_id_prefix: "ssh-watch".to_owned(),
            format: self.capture_panel.remote_format.clone(),
        };
        let timeouts = RemoteTimeouts {
            connect: Duration::from_secs(10),
            idle: Duration::from_secs(10),
            overall: Duration::from_secs(24 * 60 * 60),
        };
        match self
            .controller
            .submit_remote_watch(Arc::clone(&snapshot), request, timeouts)
        {
            Ok(operation) => {
                self.jobs.insert(operation.clone(), "Watching".to_owned());
                self.submissions.insert(
                    operation.clone(),
                    JobContext {
                        snapshot,
                        intent: OpenIntent::None,
                        format: self.capture_panel.remote_format.clone(),
                        spec: AssetOpenSpec {
                            bayer: self.capture_panel.manual_bayer,
                        },
                    },
                );
                self.capture_panel.watcher_operation = Some(operation);
            }
            Err(error) => self.binding_error = Some(error.to_string()),
        }
    }

    fn remote_job_context(&self, snapshot: Arc<TargetResolutionSnapshot>) -> JobContext {
        JobContext {
            snapshot,
            intent: OpenIntent::Foreground,
            format: self.capture_panel.remote_format.clone(),
            spec: AssetOpenSpec {
                bayer: self.capture_panel.manual_bayer,
            },
        }
    }

    pub(crate) fn poll_platform_events(&mut self) -> Vec<PlatformEffect> {
        let mut effects = Vec::new();
        while let Ok(Some(event)) = self.controller.try_recv() {
            self.jobs
                .insert(event.operation_id.clone(), format!("{:?}", event.state));
            if let DumpJobState::Succeeded { asset_id, .. } = &event.state {
                self.finish_asset_operation(&event.operation_id, asset_id, false, &mut effects);
            } else if event.state.is_terminal() {
                self.submissions.remove(&event.operation_id);
            }
        }
        while let Ok(Some(event)) = self.controller.try_recv_remote() {
            self.jobs
                .insert(event.operation_id.clone(), remote_state_label(&event.state));
            match event.state {
                RemoteJobState::CommandCompleted(result) => {
                    let context = self.submissions.remove(&event.operation_id);
                    if matches!(result.terminal, CommandTerminal::Succeeded)
                        && let (Some(path), Some(context)) = (result.artifact_path, context)
                    {
                        let request = RemoteDiscoveryRequest {
                            command_result_path: Some(path),
                            asset_id_prefix: "ssh-capture".to_owned(),
                            format: context.format.clone(),
                        };
                        match self.controller.submit_remote_discover(
                            Arc::clone(&context.snapshot),
                            request,
                            RemoteTimeouts::default(),
                        ) {
                            Ok(operation) => {
                                self.jobs.insert(operation.clone(), "Queued".to_owned());
                                self.submissions.insert(operation, context);
                            }
                            Err(error) => self.binding_error = Some(error.to_string()),
                        }
                    }
                }
                RemoteJobState::AssetReady { asset_id, .. } => {
                    self.finish_asset_operation(&event.operation_id, &asset_id, true, &mut effects);
                }
                RemoteJobState::Watch(RemoteWatchEvent::AssetReady { result, open }) => {
                    if let Some(context) = self.submissions.get(&event.operation_id).cloned() {
                        self.assets
                            .insert(result.asset.id.clone(), Arc::clone(&context.snapshot));
                        if matches!(open, RemoteOpenDisposition::Background) {
                            effects.push(PlatformEffect::OpenAsset {
                                asset: decorate_remote_asset(result.asset, &self.capture_panel),
                                snapshot: context.snapshot,
                                foreground: false,
                                spec: context.spec,
                            });
                        }
                    }
                }
                RemoteJobState::Watch(RemoteWatchEvent::Terminal(_))
                | RemoteJobState::Failed(_) => {
                    self.submissions.remove(&event.operation_id);
                    if self.capture_panel.watcher_operation.as_ref() == Some(&event.operation_id) {
                        self.capture_panel.watcher_operation = None;
                    }
                }
                RemoteJobState::Queued
                | RemoteJobState::Running(_)
                | RemoteJobState::Watch(RemoteWatchEvent::CandidateFailed { .. }) => {}
            }
        }
        effects
    }

    fn finish_asset_operation(
        &mut self,
        operation: &OperationId,
        asset_id: &AssetId,
        decorate_remote: bool,
        effects: &mut Vec<PlatformEffect>,
    ) {
        let Some(context) = self.submissions.remove(operation) else {
            return;
        };
        let Ok(Some(asset)) = self.capture_store.get(asset_id) else {
            self.binding_error = Some(format!(
                "completed asset {asset_id} is missing from CaptureStore"
            ));
            return;
        };
        self.assets
            .insert(asset_id.clone(), Arc::clone(&context.snapshot));
        if matches!(context.intent, OpenIntent::Foreground) {
            effects.push(PlatformEffect::OpenAsset {
                asset: if decorate_remote {
                    decorate_remote_asset(asset, &self.capture_panel)
                } else {
                    asset
                },
                snapshot: context.snapshot,
                foreground: true,
                spec: context.spec,
            });
        }
    }

    fn selected_bayer(&self) -> BayerPattern {
        if let SensorSelection::Mode(key) = &self.sensor_selection
            && let Some(sensor) = self.profiles.sensor(&key.sensor_id)
            && let Some(mode) = sensor.mode(&key.mode_id)
        {
            return mode.raw.bayer;
        }
        self.capture_panel.manual_bayer
    }

    fn next_asset_id(&mut self, prefix: &str) -> Result<AssetId, String> {
        let sequence = self.next_asset;
        self.next_asset = self.next_asset.saturating_add(1);
        AssetId::new(format!("{prefix}-{sequence}")).map_err(|error| error.to_string())
    }

    pub(crate) fn start(
        &mut self,
    ) -> Result<(StreamSessionId, Arc<LatestDecodedFrameSlot>), String> {
        let snapshot = self.snapshot.clone().ok_or_else(|| {
            self.binding_error
                .clone()
                .unwrap_or_else(|| "no resolved target".to_owned())
        })?;
        let profile = self
            .selected_profile
            .as_ref()
            .and_then(|id| self.profiles.platform(id))
            .ok_or_else(|| "selected profile no longer exists".to_owned())?;
        let PlatformConfig::HisiliconCv610(config) = &profile.config else {
            return Err("selected platform does not provide CV610 Stream".to_owned());
        };
        let request = self
            .panel
            .request(config.stream.channel, &config.stream.media)?;
        let session_id = self
            .controller
            .submit_stream(snapshot, request, StreamTimeouts::default())
            .map_err(|error| error.to_string())?;
        let latest = self
            .controller
            .latest_stream_frame(&session_id)
            .ok_or_else(|| "stream controller did not retain latest-frame slot".to_owned())?;
        self.panel.last_error = None;
        Ok((session_id, latest))
    }

    #[cfg(test)]
    pub(crate) fn start_resolved_for_test(
        &self,
        snapshot: Arc<TargetResolutionSnapshot>,
        request: camera_toolbox_app::StreamOpenRequest,
    ) -> Result<(StreamSessionId, Arc<LatestDecodedFrameSlot>), String> {
        let session_id = self
            .controller
            .submit_stream(snapshot, request, StreamTimeouts::default())
            .map_err(|error| error.to_string())?;
        let latest = self
            .controller
            .latest_stream_frame(&session_id)
            .ok_or_else(|| "stream controller did not retain latest-frame slot".to_owned())?;
        Ok((session_id, latest))
    }

    #[cfg(test)]
    pub(crate) fn snapshot_for_test(&self) -> Option<Arc<TargetResolutionSnapshot>> {
        self.snapshot.clone()
    }

    pub(crate) fn request_close(&self, session_id: &StreamSessionId) -> bool {
        self.controller.request_stream_close(session_id)
    }

    pub(crate) fn force_cleanup(&self, session_id: &StreamSessionId) -> bool {
        self.controller.force_stream_cleanup(session_id)
    }

    pub(crate) fn try_recv(&self) -> Result<Option<StreamSessionEvent>, String> {
        self.controller
            .try_recv_stream()
            .map_err(|error| error.to_string())
    }
}

impl Drop for LiveRuntime {
    fn drop(&mut self) {
        self.controller.force_close_all_streams();
    }
}

fn sensor_selection_label(selection: &SensorSelection) -> String {
    match selection {
        SensorSelection::Unbound => "Sensor: Unbound".to_owned(),
        SensorSelection::Mode(key) => format!("{} / {}", key.sensor_id, key.mode_id),
    }
}

fn render_capability_chips(ui: &mut egui::Ui, snapshot: &TargetResolutionSnapshot) {
    for capability in [
        camera_toolbox_app::CapabilityKind::LocalOpen,
        camera_toolbox_app::CapabilityKind::Dump,
        camera_toolbox_app::CapabilityKind::Stream,
        camera_toolbox_app::CapabilityKind::RemoteFile,
        camera_toolbox_app::CapabilityKind::Command,
    ] {
        if let Some(descriptor) = snapshot.bindings.capability(capability) {
            ui.label(format!(
                "{:?}: {:?} / {:?} / {}",
                capability, descriptor.supported_variants, descriptor.evidence, descriptor.driver
            ));
        }
    }
}

fn dump_media_format(kind: VerifiedDumpKind) -> MediaFormat {
    match kind {
        VerifiedDumpKind::Raw10 => MediaFormat::RawPacked { bit_depth: 10 },
        VerifiedDumpKind::Raw12 => MediaFormat::RawPacked { bit_depth: 12 },
        VerifiedDumpKind::Jpeg => MediaFormat::Jpeg,
        VerifiedDumpKind::Nv21 => MediaFormat::Yuv420Sp {
            chroma_order: camera_toolbox_core::ChromaOrder::Vu,
        },
    }
}

fn remote_format_selector(ui: &mut egui::Ui, format: &mut MediaFormat) {
    egui::ComboBox::from_id_salt("remote_media_format")
        .selected_text(remote_format_name(format))
        .show_ui(ui, |ui| {
            ui.selectable_value(format, MediaFormat::Jpeg, "JPEG");
            ui.selectable_value(format, MediaFormat::RawPacked { bit_depth: 10 }, "RAW10");
            ui.selectable_value(format, MediaFormat::RawPacked { bit_depth: 12 }, "RAW12");
            ui.selectable_value(
                format,
                MediaFormat::Yuv420Sp {
                    chroma_order: camera_toolbox_core::ChromaOrder::Vu,
                },
                "NV21",
            );
        });
}

fn remote_format_name(format: &MediaFormat) -> &'static str {
    match format {
        MediaFormat::Jpeg => "jpeg",
        MediaFormat::RawPacked { bit_depth: 10 } => "raw10",
        MediaFormat::RawPacked { bit_depth: 12 } => "raw12",
        MediaFormat::Yuv420Sp {
            chroma_order: camera_toolbox_core::ChromaOrder::Vu,
        } => "nv21",
        _ => "binary",
    }
}

fn decorate_remote_asset(
    asset: Arc<EphemeralAsset>,
    panel: &CapturePanelState,
) -> Arc<EphemeralAsset> {
    if matches!(asset.metadata.format, MediaFormat::Jpeg) {
        return asset;
    }
    let mut owned = (*asset).clone();
    owned
        .metadata
        .attributes
        .insert("width".to_owned(), panel.remote_width.to_string());
    owned
        .metadata
        .attributes
        .insert("height".to_owned(), panel.remote_height.to_string());
    match owned.metadata.format {
        MediaFormat::RawPacked { .. } => {
            owned
                .metadata
                .attributes
                .insert("stride".to_owned(), panel.remote_stride.to_string());
        }
        MediaFormat::Yuv420Sp { .. } => {
            owned
                .metadata
                .attributes
                .insert("y_stride".to_owned(), panel.remote_stride.to_string());
            owned
                .metadata
                .attributes
                .insert("chroma_stride".to_owned(), panel.remote_stride.to_string());
        }
        _ => {}
    }
    Arc::new(owned)
}

fn remote_state_label(state: &RemoteJobState) -> String {
    match state {
        RemoteJobState::Queued => "Queued".to_owned(),
        RemoteJobState::Running(stage) => format!("Running: {stage:?}"),
        RemoteJobState::CommandCompleted(result) => format!("Command: {:?}", result.terminal),
        RemoteJobState::AssetReady { asset_id, .. } => format!("Asset ready: {asset_id}"),
        RemoteJobState::Watch(RemoteWatchEvent::AssetReady { result, open }) => {
            format!("Watched asset: {} ({open:?})", result.asset.id)
        }
        RemoteJobState::Watch(RemoteWatchEvent::CandidateFailed { path, error }) => {
            format!("Watch candidate {path}: {error}")
        }
        RemoteJobState::Watch(RemoteWatchEvent::Terminal(terminal)) => {
            format!("Watch stopped: {terminal:?}")
        }
        RemoteJobState::Failed(error) => format!("Failed: {error:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use camera_toolbox_app::{
        CapabilityKind, LocalConfig, OperationId, PlatformConfig, PlatformProfile,
        PlatformProfileId, SensorDescriptor, SensorId, SensorModeKey, SensorSelection,
    };
    use camera_toolbox_core::{
        AssetId, BayerPattern, CaptureMetadata, EphemeralAsset, IntegrityState, MediaFormat,
        OwnedMediaPayload, RawSpec,
        sensor::{SensorMode, SensorProfile},
    };

    use super::{AssetOpenSpec, JobContext, LiveRuntime, OpenIntent, PlatformEffect};

    fn local_profile(id: &str) -> PlatformProfile {
        PlatformProfile {
            id: PlatformProfileId::new(id).unwrap(),
            display_name: id.to_owned(),
            config: PlatformConfig::Local(LocalConfig::default()),
        }
    }

    fn sensor() -> SensorDescriptor {
        SensorDescriptor {
            id: SensorId::new("sensor-a").unwrap(),
            display_name: "Sensor A".to_owned(),
            profile: SensorProfile {
                model: "sensor-a".to_owned(),
                modes: vec![SensorMode {
                    id: "mode-a".to_owned(),
                    raw: RawSpec {
                        width: 4,
                        height: 2,
                        bit_depth: 12,
                        bayer: BayerPattern::Rggb,
                    },
                    is_wdr: false,
                }],
                registers: Vec::new(),
            },
        }
    }

    #[test]
    fn platform_and_sensor_selectors_are_independent_and_unbound_remains_usable() {
        let mut runtime = LiveRuntime::new().unwrap();
        runtime.profiles = camera_toolbox_app::ProfileStore::new();
        runtime
            .profiles
            .insert_platform(local_profile("local-a"))
            .unwrap();
        runtime
            .profiles
            .insert_platform(local_profile("local-b"))
            .unwrap();
        runtime.profiles.insert_sensor(sensor()).unwrap();

        runtime.select_platform(PlatformProfileId::new("local-a").unwrap());
        let unbound = runtime.snapshot.as_ref().unwrap();
        assert_eq!(runtime.sensor_selection, SensorSelection::Unbound);
        assert!(
            unbound
                .bindings
                .capability(CapabilityKind::LocalOpen)
                .is_some()
        );

        let selection = SensorSelection::Mode(SensorModeKey {
            sensor_id: SensorId::new("sensor-a").unwrap(),
            mode_id: "mode-a".to_owned(),
        });
        runtime.select_sensor(selection.clone());
        let first_hash = runtime.snapshot.as_ref().unwrap().aggregate_hash;
        runtime.select_platform(PlatformProfileId::new("local-b").unwrap());

        assert_eq!(runtime.sensor_selection, selection);
        assert_ne!(
            runtime.snapshot.as_ref().unwrap().aggregate_hash,
            first_hash
        );
        assert!(
            runtime
                .snapshot
                .as_ref()
                .unwrap()
                .bindings
                .capability(CapabilityKind::LocalOpen)
                .is_some()
        );
    }
    #[test]
    fn background_asset_is_retained_without_opening_while_foreground_emits_open_effect() {
        let mut runtime = LiveRuntime::new().unwrap();
        runtime.profiles = camera_toolbox_app::ProfileStore::new();
        runtime
            .profiles
            .insert_platform(local_profile("local-a"))
            .unwrap();
        runtime.select_platform(PlatformProfileId::new("local-a").unwrap());
        let snapshot = runtime.snapshot.clone().unwrap();

        for (suffix, intent, expected_effects) in [
            ("background", OpenIntent::None, 0),
            ("foreground", OpenIntent::Foreground, 1),
        ] {
            let operation = OperationId::new(format!("operation-{suffix}")).unwrap();
            let asset_id = AssetId::new(format!("asset-{suffix}")).unwrap();
            let bytes = Arc::<[u8]>::from(&b"abc"[..]);
            let reservation = runtime
                .capture_store
                .reserve(operation.clone(), bytes.len())
                .unwrap();
            runtime
                .capture_store
                .publish_validated(
                    reservation,
                    EphemeralAsset::new(
                        asset_id.clone(),
                        OwnedMediaPayload::from_bytes(bytes),
                        CaptureMetadata {
                            format: MediaFormat::Binary,
                            source_name: suffix.to_owned(),
                            attributes: Default::default(),
                        },
                        IntegrityState::Verified {
                            algorithm: "test".to_owned(),
                            digest: suffix.to_owned(),
                        },
                    ),
                )
                .unwrap();
            runtime.submissions.insert(
                operation.clone(),
                JobContext {
                    snapshot: Arc::clone(&snapshot),
                    intent,
                    format: MediaFormat::Binary,
                    spec: AssetOpenSpec {
                        bayer: BayerPattern::Rggb,
                    },
                },
            );

            let mut effects = Vec::new();
            runtime.finish_asset_operation(&operation, &asset_id, false, &mut effects);
            assert_eq!(effects.len(), expected_effects);
            assert!(runtime.assets.contains_key(&asset_id));
            if expected_effects == 1 {
                assert!(matches!(
                    effects[0],
                    PlatformEffect::OpenAsset {
                        foreground: true,
                        ..
                    }
                ));
            }
        }
    }
}
