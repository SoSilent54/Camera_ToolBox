//! GUI 平台运行时：静态 profile 选择、typed binding/resolution、后台 jobs 与 event 路由。

#[cfg(feature = "platform-ssh")]
use std::collections::BTreeSet;
use std::{collections::BTreeMap, sync::Arc};
#[cfg(feature = "platform-ssh")]
use std::{path::Path, time::Duration};

use camera_toolbox_adapters::PlatformRegistry;
#[cfg(feature = "platform-cv610")]
use camera_toolbox_adapters::platforms::hisilicon_cv610::HisiliconCv610Provider;
#[cfg(feature = "platform-ssh")]
use camera_toolbox_adapters::platforms::ssh_managed::{
    CommandRecipeRegistry, CredentialResolver, ProductionCredentialResolver,
    SshManagedPlatformProvider, production_recipe_registry_from_env,
};
#[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
use camera_toolbox_app::ResolvedTargetBindings;
#[cfg(feature = "platform-ssh")]
use camera_toolbox_app::{
    CapabilityKind, CommandParameter, CommandTerminal, EvidenceMaturity, RemoteDiscoveryRequest,
    RemoteFileRequest, RemoteJobState, RemoteOpenDisposition, RemoteTimeouts, RemoteWatchEvent,
    RemoteWatchRequest, TypedCommandRequest,
};
use camera_toolbox_app::{
    CaptureStore, CaptureStoreLimits, LatestDecodedFrameSlot, OperationId, PlatformBindings,
    PlatformConfig, PlatformController, PlatformProfileId, ProfileStore, SensorSelection,
    StreamSessionEvent, StreamSessionId, TargetResolutionSnapshot,
};
#[cfg(feature = "platform-cv610")]
use camera_toolbox_app::{
    DumpJobState, DumpTimeouts, StreamTimeouts, VerifiedDumpKind, VerifiedDumpRequest,
};
#[cfg(feature = "platform-ssh")]
use camera_toolbox_app::{EepromProvisionService, SensorModeKey};
use camera_toolbox_core::{AssetId, BayerPattern, EphemeralAsset, MediaFormat};
use eframe::egui;

#[cfg(feature = "platform-cv610")]
use super::render_stream_panel;
use super::{
    DeviceManagerAction, DeviceManagerState, StreamPanelAction, StreamPanelState,
    profile_commit::{apply_profile_candidate, normalize_profile_commit},
};
#[cfg(feature = "platform-ssh")]
use super::{
    ssh_profile::{combine_remote_file, is_literal_remote_glob},
    ssh_runtime::{
        SshRuntimeAction, decorate_remote_asset, remote_format_name, remote_state_label,
        render_ssh_controls,
    },
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

#[cfg(feature = "platform-ssh")]
#[derive(Clone)]
pub(crate) struct EepromProvisioningTarget {
    pub(crate) service: Arc<dyn EepromProvisionService>,
    pub(crate) sensor_mode: SensorModeKey,
    pub(crate) snapshot_hash: camera_toolbox_app::SnapshotHash,
    pub(crate) label: String,
}

#[cfg(feature = "platform-ssh")]
fn eeprom_target_keys(profile: &camera_toolbox_app::PlatformProfile) -> BTreeSet<SensorModeKey> {
    let PlatformConfig::SshManaged(config) = &profile.config else {
        return BTreeSet::new();
    };
    config
        .eeprom
        .as_ref()
        .map(|eeprom| {
            eeprom
                .targets
                .iter()
                .map(|target| target.sensor_mode.clone())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(feature = "platform-ssh")]
fn sync_eeprom_capability_declarations(
    store: &mut ProfileStore,
    profile: &camera_toolbox_app::PlatformProfile,
    old_targets: &BTreeSet<SensorModeKey>,
) -> Result<(), String> {
    let new_targets = eeprom_target_keys(profile);
    for target in old_targets.difference(&new_targets) {
        store
            .set_capability_declaration(
                target.clone(),
                profile.id.clone(),
                CapabilityKind::EepromProvision,
                None,
            )
            .map_err(|error| error.to_string())?;
    }
    for target in &new_targets {
        store
            .set_capability_declaration(
                target.clone(),
                profile.id.clone(),
                CapabilityKind::EepromProvision,
                Some((true, EvidenceMaturity::UserConfirmed)),
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(super) struct CapturePanelState {
    #[cfg(feature = "platform-cv610")]
    pub(super) dump_kind: VerifiedDumpKind,
    pub(super) manual_bayer: BayerPattern,
    #[cfg(feature = "platform-ssh")]
    pub(super) remote_path: String,
    #[cfg(feature = "platform-ssh")]
    pub(super) remote_format: MediaFormat,
    #[cfg(feature = "platform-ssh")]
    pub(super) remote_width: u32,
    #[cfg(feature = "platform-ssh")]
    pub(super) remote_height: u32,
    #[cfg(feature = "platform-ssh")]
    pub(super) remote_stride: usize,
    #[cfg(feature = "platform-ssh")]
    pub(super) watcher_operation: Option<OperationId>,
}

impl Default for CapturePanelState {
    fn default() -> Self {
        Self {
            #[cfg(feature = "platform-cv610")]
            dump_kind: VerifiedDumpKind::Raw12,
            manual_bayer: BayerPattern::Rggb,
            #[cfg(feature = "platform-ssh")]
            remote_path: String::new(),
            #[cfg(feature = "platform-ssh")]
            remote_format: MediaFormat::Jpeg,
            #[cfg(feature = "platform-ssh")]
            remote_width: 1920,
            #[cfg(feature = "platform-ssh")]
            remote_height: 1080,
            #[cfg(feature = "platform-ssh")]
            remote_stride: 0,
            #[cfg(feature = "platform-ssh")]
            watcher_operation: None,
        }
    }
}

pub(crate) struct LiveRuntime {
    controller: PlatformController,
    capture_store: CaptureStore,
    registry: PlatformRegistry,
    #[cfg(feature = "platform-ssh")]
    credential_resolver: Arc<ProductionCredentialResolver>,
    #[cfg(feature = "platform-ssh")]
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
        #[cfg(feature = "platform-ssh")]
        let credential_resolver = Arc::new(ProductionCredentialResolver::new());
        #[cfg(feature = "platform-ssh")]
        let resolver: Arc<dyn CredentialResolver> = credential_resolver.clone();
        #[cfg(feature = "platform-ssh")]
        let recipes =
            Arc::new(production_recipe_registry_from_env().map_err(|error| {
                format!("invalid production SSH recipe configuration: {error}")
            })?);
        let mut registry = PlatformRegistry::new();
        #[cfg(feature = "platform-cv610")]
        registry.register_cv610(HisiliconCv610Provider::default());
        #[cfg(feature = "platform-ssh")]
        registry.register_ssh(SshManagedPlatformProvider::production(
            resolver,
            Arc::clone(&recipes),
        ));
        let profiles = ProfileStore::with_builtin_local().map_err(|error| error.to_string())?;
        let startup_message = None;
        let selected = profiles
            .platforms()
            .next()
            .map(|profile| profile.id.clone());
        let mut runtime = Self {
            controller: PlatformController::new(capture_store.clone()),
            capture_store,
            registry,
            #[cfg(feature = "platform-ssh")]
            credential_resolver,
            #[cfg(feature = "platform-ssh")]
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

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn ssh_credential_resolver(&self) -> Arc<ProductionCredentialResolver> {
        Arc::clone(&self.credential_resolver)
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn ssh_resolver(&self) -> Arc<dyn CredentialResolver> {
        self.credential_resolver.clone()
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
                .cloned()
            {
                #[cfg(feature = "platform-ssh")]
                {
                    let readiness = match &profile.config {
                        PlatformConfig::SshManaged(config)
                            if config.credential_ref.starts_with("session:") =>
                        {
                            self.credential_resolver.has_session(&config.credential_ref)
                        }
                        _ => Ok(true),
                    };
                    self.device_manager
                        .edit(&profile, readiness.as_ref().copied().unwrap_or(false));
                    if let Err(error) = readiness {
                        self.device_manager.message = Some(format!(
                            "Password registry status is unavailable: {error}; re-enter the password"
                        ));
                    }
                }
                #[cfg(not(feature = "platform-ssh"))]
                self.device_manager.edit(&profile, false);
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
                #[cfg(feature = "platform-cv610")]
                {
                    ui.heading("CV610 Capture");
                    ui.label(format!(
                        "{} — Dump {} / Stream {}",
                        config.host, config.dump.port, config.stream.port
                    ));
                    self.render_dump_controls(ui);
                    ui.separator();
                    if let Some(stream_action) =
                        render_stream_panel(ui, &mut self.panel, live_running)
                    {
                        action = Some(PlatformUiAction::Stream(stream_action));
                    }
                }
                #[cfg(not(feature = "platform-cv610"))]
                render_platform_not_compiled(ui, "CV610");
            }
            PlatformConfig::SshManaged(config) => {
                #[cfg(feature = "platform-ssh")]
                if let Some(ssh_action) = render_ssh_controls(
                    ui,
                    &config,
                    &mut self.capture_panel,
                    self.snapshot.as_deref(),
                ) {
                    match ssh_action {
                        SshRuntimeAction::Capture => self.submit_remote_command(&config),
                        SshRuntimeAction::Fetch => self.submit_remote_fetch(),
                        SshRuntimeAction::StartWatch => self.submit_watch(),
                        SshRuntimeAction::StopWatch(operation) => {
                            if self.controller.cancel(&operation) {
                                self.capture_panel.watcher_operation = None;
                            }
                        }
                    }
                }
                #[cfg(not(feature = "platform-ssh"))]
                render_platform_not_compiled(ui, "SSH-managed");
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
            DeviceManagerAction::Commit {
                draft,
                #[cfg(feature = "platform-ssh")]
                ssh_auth,
            } => {
                #[cfg(feature = "platform-ssh")]
                let old_reference = draft
                    .original_id
                    .as_ref()
                    .and_then(|id| self.profiles.platform(id))
                    .and_then(|profile| match &profile.config {
                        PlatformConfig::SshManaged(config) => Some(config.credential_ref.clone()),
                        _ => None,
                    });
                #[cfg(feature = "platform-ssh")]
                let old_eeprom_targets = draft
                    .original_id
                    .as_ref()
                    .and_then(|id| self.profiles.platform(id))
                    .map(eeprom_target_keys)
                    .unwrap_or_default();
                #[cfg(feature = "platform-ssh")]
                let normalized = normalize_profile_commit(&self.profiles, draft, ssh_auth)?;
                #[cfg(not(feature = "platform-ssh"))]
                let normalized = normalize_profile_commit(&self.profiles, draft)?;
                let profile = normalized.profile;
                let original_id = normalized.original_id;
                let mut candidate =
                    apply_profile_candidate(&self.profiles, profile.clone(), original_id.as_ref())?;
                #[cfg(feature = "platform-ssh")]
                sync_eeprom_capability_declarations(&mut candidate, &profile, &old_eeprom_targets)?;
                self.profiles = candidate;
                #[cfg(feature = "platform-ssh")]
                {
                    let new_reference = match &profile.config {
                        PlatformConfig::SshManaged(config) => Some(config.credential_ref.clone()),
                        _ => None,
                    };
                    if old_reference.as_deref().is_some_and(|reference| {
                        reference.starts_with("session:")
                            && Some(reference) != new_reference.as_deref()
                    }) && let Some(reference) = old_reference.as_deref()
                    {
                        let _ = self.credential_resolver.remove_session(reference);
                    }
                    let registration = normalized.session_password.map(|(session_id, password)| {
                        self.credential_resolver
                            .register_session_password(&session_id, password)
                    });
                    let registration_error = registration.and_then(Result::err);
                    let readiness = match new_reference.as_deref() {
                        Some(reference) if reference.starts_with("session:") => {
                            self.credential_resolver.has_session(reference)
                        }
                        _ => Ok(true),
                    };
                    self.select_platform(profile.id.clone());
                    self.device_manager
                        .mark_applied(&profile, readiness.as_ref().copied().unwrap_or(false));
                    if let Some(error) = registration_error {
                        return Err(format!(
                            "profile {} was applied for this session, but process-only password registration failed: {error}; remote operations remain unavailable until the password is re-entered",
                            profile.id
                        ));
                    }
                    let password_registered = readiness.map_err(|error| format!(
                        "profile {} is active for this session, but password registry status is unavailable: {error}",
                        profile.id
                    ))?;
                    let password_note = if password_registered {
                        ""
                    } else {
                        "; password is not registered in this process"
                    };
                    return Ok(format!("Applied profile {}{password_note}", profile.id));
                }
                #[cfg(not(feature = "platform-ssh"))]
                {
                    self.select_platform(profile.id.clone());
                    self.device_manager.mark_applied(&profile, false);
                    Ok(format!("Applied profile {}", profile.id))
                }
            }
            DeviceManagerAction::ValidationError(error) => Err(error),
            DeviceManagerAction::Delete(id) => {
                if self.profiles.platforms().count() <= 1 {
                    return Err("at least one platform profile must remain".to_owned());
                }
                #[cfg(feature = "platform-ssh")]
                let old_reference = self
                    .profiles
                    .platform(&id)
                    .and_then(|profile| match &profile.config {
                        PlatformConfig::SshManaged(config) => Some(config.credential_ref.clone()),
                        _ => None,
                    });
                #[cfg(feature = "platform-ssh")]
                let old_eeprom_targets = self
                    .profiles
                    .platform(&id)
                    .map(eeprom_target_keys)
                    .unwrap_or_default();
                let mut candidate = self.profiles.clone();
                #[cfg(feature = "platform-ssh")]
                for target in &old_eeprom_targets {
                    candidate
                        .set_capability_declaration(
                            target.clone(),
                            id.clone(),
                            CapabilityKind::EepromProvision,
                            None,
                        )
                        .map_err(|error| error.to_string())?;
                }
                candidate
                    .remove_platform(&id)
                    .map_err(|error| error.to_string())?;
                self.profiles = candidate;
                #[cfg(feature = "platform-ssh")]
                if let Some(reference) = old_reference.as_deref()
                    && reference.starts_with("session:")
                {
                    let _ = self.credential_resolver.remove_session(reference);
                }
                self.device_manager.mark_deleted();
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
                self.device_manager.mark_deleted();
                if let Some(next) = next {
                    self.select_platform(next);
                }
                Ok(format!("Imported {} for this session", path.display()))
            }
            DeviceManagerAction::Export(path) => {
                self.profiles
                    .save_to_path(&path)
                    .map_err(|error| error.to_string())?;
                Ok(format!("Exported {}", path.display()))
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
        #[cfg(feature = "platform-ssh")]
        if let PlatformConfig::SshManaged(config) = &profile.config
            && is_literal_remote_glob(&config.remote_artifact_glob)
        {
            self.capture_panel.remote_path =
                combine_remote_file(&config.remote_artifact_dir, &config.remote_artifact_glob);
        }
        self.bind_count = self.bind_count.saturating_add(1);
        match self.registry.bind(profile) {
            Ok(candidate) => {
                self.candidate = Some(candidate);
                self.resolve_selected();
            }
            Err(error) => {
                #[cfg(feature = "platform-ssh")]
                {
                    self.binding_error = Some(binding_error_message(
                        &profile.config,
                        &error.to_string(),
                        &self.recipes,
                    ));
                }
                #[cfg(not(feature = "platform-ssh"))]
                {
                    self.binding_error =
                        Some(binding_error_message(&profile.config, &error.to_string()));
                }
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

    #[cfg(feature = "platform-cv610")]
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

    #[cfg(feature = "platform-cv610")]
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

    #[cfg(feature = "platform-ssh")]
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

    #[cfg(feature = "platform-ssh")]
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

    #[cfg(feature = "platform-ssh")]
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

    #[cfg(feature = "platform-ssh")]
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
        #[cfg(feature = "platform-cv610")]
        while let Ok(Some(event)) = self.controller.try_recv() {
            self.jobs
                .insert(event.operation_id.clone(), format!("{:?}", event.state));
            if let DumpJobState::Succeeded { asset_id, .. } = &event.state {
                self.finish_asset_operation(&event.operation_id, asset_id, false, &mut effects);
            } else if event.state.is_terminal() {
                self.submissions.remove(&event.operation_id);
            }
        }
        #[cfg(feature = "platform-ssh")]
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
                asset: {
                    #[cfg(feature = "platform-ssh")]
                    if decorate_remote {
                        decorate_remote_asset(asset, &self.capture_panel)
                    } else {
                        asset
                    }
                    #[cfg(not(feature = "platform-ssh"))]
                    {
                        let _ = decorate_remote;
                        asset
                    }
                },
                snapshot: context.snapshot,
                foreground: true,
                spec: context.spec,
            });
        }
    }

    #[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
    fn selected_bayer(&self) -> BayerPattern {
        if let SensorSelection::Mode(key) = &self.sensor_selection
            && let Some(sensor) = self.profiles.sensor(&key.sensor_id)
            && let Some(mode) = sensor.mode(&key.mode_id)
        {
            return mode.raw.bayer;
        }
        self.capture_panel.manual_bayer
    }

    #[cfg(any(feature = "platform-cv610", feature = "platform-ssh"))]
    fn next_asset_id(&mut self, prefix: &str) -> Result<AssetId, String> {
        let sequence = self.next_asset;
        self.next_asset = self.next_asset.saturating_add(1);
        AssetId::new(format!("{prefix}-{sequence}")).map_err(|error| error.to_string())
    }

    pub(crate) fn start(
        &mut self,
    ) -> Result<(StreamSessionId, Arc<LatestDecodedFrameSlot>), String> {
        #[cfg(feature = "platform-cv610")]
        {
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
            return Ok((session_id, latest));
        }
        #[cfg(not(feature = "platform-cv610"))]
        Err("CV610 platform is not compiled in this build".to_owned())
    }

    #[cfg(all(test, feature = "platform-cv610"))]
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

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn eeprom_provisioning_target(&self) -> Result<EepromProvisioningTarget, String> {
        let snapshot = self.snapshot.clone().ok_or_else(|| {
            self.binding_error
                .clone()
                .unwrap_or_else(|| "Select a resolved SSH Sensor/Mode target.".to_owned())
        })?;
        let SensorSelection::Mode(sensor_mode) = &snapshot.key.sensor else {
            return Err("Select a Sensor/Mode before EEPROM provisioning.".to_owned());
        };
        let ResolvedTargetBindings::SshManaged(bindings) = snapshot.bindings.as_ref() else {
            return Err("Selected platform is not SSH-managed.".to_owned());
        };
        let handle = bindings.eeprom.as_ref().ok_or_else(|| {
            "Selected Sensor×Platform cell does not expose EEPROM provisioning.".to_owned()
        })?;
        let profile = self
            .profiles
            .platform(&snapshot.key.platform_id)
            .ok_or_else(|| "Selected platform profile no longer exists.".to_owned())?;
        Ok(EepromProvisioningTarget {
            service: Arc::clone(&handle.service),
            sensor_mode: sensor_mode.clone(),
            snapshot_hash: snapshot.aggregate_hash,
            label: format!(
                "{} / {}:{} @{}",
                profile.display_name,
                sensor_mode.sensor_id.as_str(),
                sensor_mode.mode_id,
                &snapshot.aggregate_hash.to_hex()[..12],
            ),
        })
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

#[cfg(any(not(feature = "platform-cv610"), not(feature = "platform-ssh")))]
fn render_platform_not_compiled(ui: &mut egui::Ui, platform: &str) {
    ui.heading(format!("{platform} profile"));
    ui.colored_label(
        egui::Color32::YELLOW,
        "Platform not compiled in this build. The profile remains available for display, import, and export.",
    );
}

#[cfg(feature = "platform-ssh")]
fn binding_error_message(
    config: &PlatformConfig,
    fallback: &str,
    recipes: &CommandRecipeRegistry,
) -> String {
    match config {
        PlatformConfig::SshManaged(config)
            if !config.capture_recipe.is_empty() && !recipes.contains(&config.capture_recipe) =>
        {
            format!(
                "SSH typed recipe '{}' is unavailable. Configure the complete CAMERA_TOOLBOX_SSH_RECIPE_* environment group with a verified absolute program, typed flags, allowed formats, remote root and path-on-stdout contract; no command was sent.",
                config.capture_recipe
            )
        }
        _ => fallback.to_owned(),
    }
}

#[cfg(not(feature = "platform-ssh"))]
fn binding_error_message(_config: &PlatformConfig, fallback: &str) -> String {
    fallback.to_owned()
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

#[cfg(feature = "platform-cv610")]
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

#[cfg(test)]
#[path = "live_runtime_tests.rs"]
mod tests;
