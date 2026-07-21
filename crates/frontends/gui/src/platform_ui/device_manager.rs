//! Device Manager 独立 viewport；所有编辑先落在 draft，验证成功后才提交。

use std::path::PathBuf;

#[cfg(feature = "platform-ssh")]
use camera_toolbox_app::SshManagedConfig;
#[cfg(feature = "platform-cv610")]
use camera_toolbox_app::{Cv610Config, Cv610DumpConfig, Cv610StreamConfig};
use camera_toolbox_app::{LocalConfig, PlatformConfig, PlatformProfile, PlatformProfileId};
use eframe::egui;

#[cfg(feature = "platform-ssh")]
use super::ssh_profile::{SshCommitAuth, SshEditorState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DraftVariant {
    Local,
    #[cfg(feature = "platform-cv610")]
    Cv610,
    #[cfg(feature = "platform-ssh")]
    SshManaged,
}

/// Clone/Debug draft 只含非敏感配置，绝不持有 password。
#[derive(Debug, Clone)]
pub(crate) struct ProfileDraft {
    pub(crate) original_id: Option<PlatformProfileId>,
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) config: PlatformConfig,
}

impl ProfileDraft {
    pub(super) fn new(variant: DraftVariant) -> Self {
        let mut draft = Self {
            original_id: None,
            id: String::new(),
            display_name: String::new(),
            config: PlatformConfig::Local(LocalConfig::default()),
        };
        draft.switch_variant(variant);
        draft
    }

    pub(crate) fn from_profile(profile: &PlatformProfile) -> Self {
        Self {
            original_id: Some(profile.id.clone()),
            id: profile.id.as_str().to_owned(),
            display_name: profile.display_name.clone(),
            config: profile.config.clone(),
        }
    }

    pub(crate) const fn variant(&self) -> Option<DraftVariant> {
        match self.config {
            PlatformConfig::Local(_) => Some(DraftVariant::Local),
            PlatformConfig::HisiliconCv610(_) => {
                #[cfg(feature = "platform-cv610")]
                {
                    Some(DraftVariant::Cv610)
                }
                #[cfg(not(feature = "platform-cv610"))]
                {
                    None
                }
            }
            PlatformConfig::SshManaged(_) => {
                #[cfg(feature = "platform-ssh")]
                {
                    Some(DraftVariant::SshManaged)
                }
                #[cfg(not(feature = "platform-ssh"))]
                {
                    None
                }
            }
        }
    }

    pub(crate) fn switch_variant(&mut self, variant: DraftVariant) {
        self.config = match variant {
            DraftVariant::Local => PlatformConfig::Local(LocalConfig::default()),
            #[cfg(feature = "platform-cv610")]
            DraftVariant::Cv610 => PlatformConfig::HisiliconCv610(Cv610Config {
                host: String::new(),
                dump: Cv610DumpConfig::default(),
                stream: Cv610StreamConfig::default(),
            }),
            #[cfg(feature = "platform-ssh")]
            DraftVariant::SshManaged => PlatformConfig::SshManaged(incomplete_ssh_template()),
        };
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn rdk_x5_unknown() -> Self {
        let mut draft = Self::new(DraftVariant::SshManaged);
        draft.display_name = "RDK X5 (SSH template, Unknown)".to_owned();
        draft
    }

    pub(crate) fn build(&self) -> Result<PlatformProfile, String> {
        let profile = PlatformProfile {
            id: PlatformProfileId::new(self.id.clone()).map_err(|error| error.to_string())?,
            display_name: self.display_name.clone(),
            config: self.config.clone(),
        };
        profile.validate().map_err(|error| error.to_string())?;
        Ok(profile)
    }
}

#[cfg(feature = "platform-ssh")]
pub(super) fn incomplete_ssh_template() -> SshManagedConfig {
    SshManagedConfig {
        host: String::new(),
        port: 22,
        username: String::new(),
        expected_host_key: String::new(),
        credential_ref: String::new(),
        command_subsystem: None,
        capture_recipe: String::new(),
        remote_artifact_dir: String::new(),
        remote_artifact_glob: String::new(),
        remote_event_subsystem: None,
        passive_watch_auto_open: false,
        stable_samples: 2,
        stability_interval_ms: 500,
        max_fetch_bytes: 256 * 1024 * 1024,
        command_output_bytes: 64 * 1024,
    }
}

/// Secret-bearing action intentionally has no Clone/Debug implementation.
pub(crate) enum DeviceManagerAction {
    Commit {
        draft: ProfileDraft,
        #[cfg(feature = "platform-ssh")]
        ssh_auth: Option<SshCommitAuth>,
    },
    ValidationError(String),
    Delete(PlatformProfileId),
    Import(PathBuf),
    Export(PathBuf),
}

pub(crate) struct DeviceManagerState {
    pub(crate) open: bool,
    pub(crate) draft: Option<ProfileDraft>,
    pub(crate) message: Option<String>,
    #[cfg(feature = "platform-ssh")]
    ssh: SshEditorState,
}

impl Default for DeviceManagerState {
    fn default() -> Self {
        Self {
            open: false,
            draft: None,
            message: None,
            #[cfg(feature = "platform-ssh")]
            ssh: SshEditorState::new(),
        }
    }
}

impl DeviceManagerState {
    pub(crate) fn edit(&mut self, profile: &PlatformProfile, password_registered: bool) {
        #[cfg(feature = "platform-ssh")]
        {
            self.ssh.clear_sensitive();
            self.ssh = SshEditorState::from_profile(profile, password_registered);
        }
        #[cfg(not(feature = "platform-ssh"))]
        let _ = password_registered;
        self.draft = Some(ProfileDraft::from_profile(profile));
        self.open = true;
        self.message = None;
    }

    pub(crate) fn mark_applied(&mut self, profile: &PlatformProfile, password_registered: bool) {
        #[cfg(feature = "platform-ssh")]
        {
            self.ssh.clear_sensitive();
            self.ssh = SshEditorState::from_profile(profile, password_registered);
        }
        #[cfg(not(feature = "platform-ssh"))]
        let _ = password_registered;
        self.draft = Some(ProfileDraft::from_profile(profile));
    }

    pub(crate) fn mark_deleted(&mut self) {
        #[cfg(feature = "platform-ssh")]
        {
            self.ssh.clear_sensitive();
            self.ssh = SshEditorState::new();
        }
        self.draft = None;
    }

    fn close(&mut self) {
        self.open = false;
        #[cfg(feature = "platform-ssh")]
        self.ssh.clear_sensitive();
        self.draft = None;
    }

    pub(crate) fn show(&mut self, context: &egui::Context) -> Vec<DeviceManagerAction> {
        if !self.open {
            return Vec::new();
        }
        #[cfg(feature = "platform-ssh")]
        if let Some(ProfileDraft {
            config: PlatformConfig::SshManaged(config),
            ..
        }) = self.draft.as_mut()
        {
            self.ssh.poll(config, true);
        }
        let mut actions = Vec::new();
        let viewport_id = egui::ViewportId::from_hash_of("camera-toolbox-device-manager");
        context.show_viewport_immediate(
            viewport_id,
            egui::ViewportBuilder::default()
                .with_title("Camera Toolbox Device Manager")
                .with_inner_size([720.0, 720.0])
                .with_min_inner_size([560.0, 460.0]),
            |context, _class| {
                if context.input(|input| input.viewport().close_requested()) {
                    self.close();
                    return;
                }
                egui::CentralPanel::default().show(context, |ui| {
                    self.render_toolbar(ui, &mut actions);
                    ui.separator();
                    if let Some(message) = self.message.as_deref() {
                        ui.label(message);
                    }
                    let Some(draft) = self.draft.as_mut() else {
                        ui.label(
                            "Choose New or edit the selected profile from the target toolbar.",
                        );
                        return;
                    };
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        #[cfg(feature = "platform-ssh")]
                        render_draft(ui, draft, &mut self.ssh, &mut actions);
                        #[cfg(not(feature = "platform-ssh"))]
                        render_draft(ui, draft, &mut actions);
                    });
                });
            },
        );
        actions
    }

    fn render_toolbar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<DeviceManagerAction>) {
        ui.horizontal(|ui| {
            if ui.button("New Local").clicked() {
                #[cfg(feature = "platform-ssh")]
                {
                    self.ssh.clear_sensitive();
                    self.ssh = SshEditorState::new();
                }
                self.draft = Some(ProfileDraft::new(DraftVariant::Local));
            }
            #[cfg(feature = "platform-cv610")]
            if ui.button("New CV610").clicked() {
                #[cfg(feature = "platform-ssh")]
                {
                    self.ssh.clear_sensitive();
                    self.ssh = SshEditorState::new();
                }
                self.draft = Some(ProfileDraft::new(DraftVariant::Cv610));
            }
            #[cfg(feature = "platform-ssh")]
            if ui.button("New SSH-managed").clicked() {
                self.ssh.clear_sensitive();
                self.ssh = SshEditorState::new();
                self.draft = Some(ProfileDraft::new(DraftVariant::SshManaged));
            }
            #[cfg(feature = "platform-ssh")]
            if ui.button("RDK X5 template (Unknown)").clicked() {
                self.ssh.clear_sensitive();
                self.ssh = SshEditorState::rdk_template();
                self.draft = Some(ProfileDraft::rdk_x5_unknown());
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Import profiles...").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Camera Toolbox profiles", &["json"])
                    .pick_file()
            {
                actions.push(DeviceManagerAction::Import(path));
            }
            if ui.button("Export profiles...").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Camera Toolbox profiles", &["json"])
                    .set_file_name("platform-profiles.json")
                    .save_file()
            {
                actions.push(DeviceManagerAction::Export(path));
            }
        });
    }
}

#[cfg(feature = "platform-ssh")]
fn render_draft(
    ui: &mut egui::Ui,
    draft: &mut ProfileDraft,
    ssh: &mut SshEditorState,
    actions: &mut Vec<DeviceManagerAction>,
) {
    render_draft_body(ui, draft, ssh, actions);
}

#[cfg(not(feature = "platform-ssh"))]
fn render_draft(
    ui: &mut egui::Ui,
    draft: &mut ProfileDraft,
    actions: &mut Vec<DeviceManagerAction>,
) {
    render_draft_body(ui, draft, actions);
}

#[cfg(feature = "platform-ssh")]
fn render_draft_body(
    ui: &mut egui::Ui,
    draft: &mut ProfileDraft,
    ssh: &mut SshEditorState,
    actions: &mut Vec<DeviceManagerAction>,
) {
    ui.heading("Profile draft");
    ui.horizontal(|ui| {
        ui.label("Variant");
        let current = draft.variant();
        for (variant, label) in available_variants() {
            if ui
                .selectable_label(current == Some(variant), label)
                .clicked()
                && current != Some(variant)
            {
                ssh.reset_for_variant();
                draft.switch_variant(variant);
            }
        }
    });
    if draft.variant().is_none() {
        ui.colored_label(egui::Color32::YELLOW, "This imported platform is not compiled into this build. Its schema remains available for display, import, and export.");
    }
    render_profile_details(ui, draft);
    ui.separator();
    match &mut draft.config {
        PlatformConfig::Local(_) => {
            ui.label("Local profile has no network or credential fields.");
        }
        #[cfg(feature = "platform-cv610")]
        PlatformConfig::HisiliconCv610(config) => render_cv610(ui, config),
        #[cfg(not(feature = "platform-cv610"))]
        PlatformConfig::HisiliconCv610(_) => render_not_compiled(ui, "CV610"),
        PlatformConfig::SshManaged(config) => ssh.render(ui, config),
    }
    let editable = draft.variant().is_some();
    render_draft_actions(ui, draft, actions, editable, |mut candidate, actions| {
        let is_new = candidate.original_id.is_none();
        let ssh_auth = match &mut candidate.config {
            PlatformConfig::SshManaged(config) => match ssh.prepare_commit(config, is_new) {
                Ok(auth) => Some(auth),
                Err(error) => {
                    actions.push(DeviceManagerAction::ValidationError(error));
                    return;
                }
            },
            _ => None,
        };
        actions.push(DeviceManagerAction::Commit {
            draft: candidate,
            ssh_auth,
        });
    });
}

#[cfg(not(feature = "platform-ssh"))]
fn render_draft_body(
    ui: &mut egui::Ui,
    draft: &mut ProfileDraft,
    actions: &mut Vec<DeviceManagerAction>,
) {
    render_draft_common(ui, draft, actions, |ui, draft| match &mut draft.config {
        PlatformConfig::Local(_) => {
            ui.label("Local profile has no network or credential fields.");
        }
        #[cfg(feature = "platform-cv610")]
        PlatformConfig::HisiliconCv610(config) => render_cv610(ui, config),
        #[cfg(not(feature = "platform-cv610"))]
        PlatformConfig::HisiliconCv610(_) => render_not_compiled(ui, "CV610"),
        PlatformConfig::SshManaged(_) => render_not_compiled(ui, "SSH-managed"),
    });
}

#[cfg(not(feature = "platform-ssh"))]
fn render_draft_common(
    ui: &mut egui::Ui,
    draft: &mut ProfileDraft,
    actions: &mut Vec<DeviceManagerAction>,
    render_config: impl FnOnce(&mut egui::Ui, &mut ProfileDraft),
) {
    ui.heading("Profile draft");
    ui.horizontal(|ui| {
        ui.label("Variant");
        let current = draft.variant();
        for (variant, label) in available_variants() {
            if ui
                .selectable_label(current == Some(variant), label)
                .clicked()
                && current != Some(variant)
            {
                draft.switch_variant(variant);
            }
        }
    });
    if draft.variant().is_none() {
        ui.colored_label(egui::Color32::YELLOW, "This imported platform is not compiled into this build. Its schema remains available for display, import, and export.");
    }
    render_profile_details(ui, draft);
    ui.separator();
    render_config(ui, draft);
    let editable = draft.variant().is_some();
    render_draft_actions(ui, draft, actions, editable, |candidate, actions| {
        actions.push(DeviceManagerAction::Commit { draft: candidate });
    });
}

fn render_profile_details(ui: &mut egui::Ui, draft: &mut ProfileDraft) {
    let editable = draft.variant().is_some();
    ui.collapsing("Profile details", |ui| {
        egui::Grid::new("profile_common_fields")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Profile ID");
                ui.add_enabled(
                    editable && draft.original_id.is_none(),
                    egui::TextEdit::singleline(&mut draft.id),
                );
                ui.end_row();
                ui.label("Display name");
                ui.add_enabled(
                    editable,
                    egui::TextEdit::singleline(&mut draft.display_name),
                );
                ui.end_row();
            });
    });
}

fn render_draft_actions(
    ui: &mut egui::Ui,
    draft: &mut ProfileDraft,
    actions: &mut Vec<DeviceManagerAction>,
    editable: bool,
    mut apply: impl FnMut(ProfileDraft, &mut Vec<DeviceManagerAction>),
) {
    ui.separator();
    ui.horizontal(|ui| {
        if ui
            .add_enabled(editable, egui::Button::new("Validate and Apply"))
            .clicked()
        {
            apply(draft.clone(), actions);
        }
        if let Some(id) = draft.original_id.clone()
            && ui.button("Delete profile").clicked()
        {
            actions.push(DeviceManagerAction::Delete(id));
        }
        if ui.button("Discard draft").clicked() {
            draft.original_id = None;
            draft.id.clear();
            draft.display_name.clear();
            draft.switch_variant(DraftVariant::Local);
        }
    });
}

fn available_variants() -> Vec<(DraftVariant, &'static str)> {
    let mut variants = vec![(DraftVariant::Local, "Local")];
    #[cfg(feature = "platform-cv610")]
    variants.push((DraftVariant::Cv610, "Hisilicon CV610"));
    #[cfg(feature = "platform-ssh")]
    variants.push((DraftVariant::SshManaged, "SSH-managed"));
    variants
}

#[cfg(any(not(feature = "platform-cv610"), not(feature = "platform-ssh")))]
fn render_not_compiled(ui: &mut egui::Ui, platform: &str) {
    ui.heading(format!("{platform} profile"));
    ui.colored_label(egui::Color32::YELLOW, "Platform not compiled in this build. The profile is read-only here and can still be imported or exported.");
}

#[cfg(feature = "platform-cv610")]
fn render_cv610(ui: &mut egui::Ui, config: &mut Cv610Config) {
    ui.heading("CV610 direct TCP");
    ui.label("One host is shared by Dump and Stream; transports retain separate ports/lifecycles.");
    egui::Grid::new("cv610_profile_fields")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("Host");
            ui.text_edit_singleline(&mut config.host);
            ui.end_row();
            ui.label("Dump port");
            ui.add(egui::DragValue::new(&mut config.dump.port).range(1..=u16::MAX));
            ui.end_row();
            ui.label("Preferred RAW bits");
            ui.selectable_value(&mut config.dump.preferred_raw_bits, 10, "10");
            ui.selectable_value(&mut config.dump.preferred_raw_bits, 12, "12");
            ui.end_row();
            ui.label("Stream port");
            ui.add(egui::DragValue::new(&mut config.stream.port).range(1..=u16::MAX));
            ui.end_row();
            ui.label("Stream channel");
            ui.add(egui::DragValue::new(&mut config.stream.channel).range(0..=255));
            ui.end_row();
            ui.label("Stream media");
            ui.text_edit_singleline(&mut config.stream.media);
            ui.end_row();
        });
}
