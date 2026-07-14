//! Device Manager 独立 viewport；所有编辑先落在 draft，验证成功后才提交。

use std::path::PathBuf;

use camera_toolbox_app::{
    Cv610Config, Cv610DumpConfig, Cv610StreamConfig, LocalConfig, PlatformConfig, PlatformProfile,
    PlatformProfileId, SshManagedConfig,
};
use eframe::egui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DraftVariant {
    Local,
    Cv610,
    SshManaged,
}

#[derive(Debug, Clone)]
pub(crate) struct ProfileDraft {
    pub(crate) original_id: Option<PlatformProfileId>,
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) config: PlatformConfig,
    pub(crate) session_secret: String,
    pub(crate) session_id: String,
}

impl ProfileDraft {
    fn new(variant: DraftVariant) -> Self {
        let mut draft = Self {
            original_id: None,
            id: String::new(),
            display_name: String::new(),
            config: PlatformConfig::Local(LocalConfig::default()),
            session_secret: String::new(),
            session_id: String::new(),
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
            session_secret: String::new(),
            session_id: profile.id.as_str().to_owned(),
        }
    }

    pub(crate) const fn variant(&self) -> DraftVariant {
        match self.config {
            PlatformConfig::Local(_) => DraftVariant::Local,
            PlatformConfig::HisiliconCv610(_) => DraftVariant::Cv610,
            PlatformConfig::SshManaged(_) => DraftVariant::SshManaged,
        }
    }

    pub(crate) fn switch_variant(&mut self, variant: DraftVariant) {
        self.config = match variant {
            DraftVariant::Local => PlatformConfig::Local(LocalConfig::default()),
            DraftVariant::Cv610 => PlatformConfig::HisiliconCv610(Cv610Config {
                host: String::new(),
                dump: Cv610DumpConfig::default(),
                stream: Cv610StreamConfig::default(),
            }),
            DraftVariant::SshManaged => PlatformConfig::SshManaged(incomplete_ssh_template()),
        };
        self.session_secret.clear();
    }

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

fn incomplete_ssh_template() -> SshManagedConfig {
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

pub(crate) enum DeviceManagerAction {
    Commit(ProfileDraft),
    Delete(PlatformProfileId),
    Import(PathBuf),
    Export(PathBuf),
    RegisterSessionSecret { id: String, secret: String },
}

#[derive(Default)]
pub(crate) struct DeviceManagerState {
    pub(crate) open: bool,
    pub(crate) draft: Option<ProfileDraft>,
    pub(crate) message: Option<String>,
}

impl DeviceManagerState {
    pub(crate) fn edit(&mut self, profile: &PlatformProfile) {
        self.draft = Some(ProfileDraft::from_profile(profile));
        self.open = true;
        self.message = None;
    }

    pub(crate) fn show(&mut self, context: &egui::Context) -> Vec<DeviceManagerAction> {
        if !self.open {
            return Vec::new();
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
                    self.open = false;
                }
                egui::CentralPanel::default().show(context, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("New Local").clicked() {
                            self.draft = Some(ProfileDraft::new(DraftVariant::Local));
                        }
                        if ui.button("New CV610").clicked() {
                            self.draft = Some(ProfileDraft::new(DraftVariant::Cv610));
                        }
                        if ui.button("New SSH-managed").clicked() {
                            self.draft = Some(ProfileDraft::new(DraftVariant::SshManaged));
                        }
                        if ui.button("RDK X5 template (Unknown)").clicked() {
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
                        render_draft(ui, draft, &mut actions);
                    });
                });
            },
        );
        actions
    }
}

fn render_draft(
    ui: &mut egui::Ui,
    draft: &mut ProfileDraft,
    actions: &mut Vec<DeviceManagerAction>,
) {
    ui.heading("Profile draft");
    egui::Grid::new("profile_common_fields")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("Profile ID");
            ui.text_edit_singleline(&mut draft.id);
            ui.end_row();
            ui.label("Display name");
            ui.text_edit_singleline(&mut draft.display_name);
            ui.end_row();
        });
    ui.horizontal(|ui| {
        ui.label("Variant");
        let current = draft.variant();
        for (variant, label) in [
            (DraftVariant::Local, "Local"),
            (DraftVariant::Cv610, "Hisilicon CV610"),
            (DraftVariant::SshManaged, "SSH-managed"),
        ] {
            if ui.selectable_label(current == variant, label).clicked() && current != variant {
                draft.switch_variant(variant);
            }
        }
    });
    ui.separator();
    let session_id = &mut draft.session_id;
    let session_secret = &mut draft.session_secret;
    match &mut draft.config {
        PlatformConfig::Local(_) => {
            ui.label("Local profile has no network or credential fields.");
        }
        PlatformConfig::HisiliconCv610(config) => render_cv610(ui, config),
        PlatformConfig::SshManaged(config) => {
            render_ssh(ui, config, session_id, session_secret, actions);
        }
    }
    ui.separator();
    ui.horizontal(|ui| {
        if ui.button("Validate and Save").clicked() {
            actions.push(DeviceManagerAction::Commit(draft.clone()));
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

fn render_ssh(
    ui: &mut egui::Ui,
    config: &mut SshManagedConfig,
    session_id: &mut String,
    session_secret: &mut String,
    actions: &mut Vec<DeviceManagerAction>,
) {
    ui.heading("SSH-managed");
    ui.colored_label(
        egui::Color32::YELLOW,
        "RDK X5 acceptance is Unknown. Blank template fields must be filled from your deployment evidence.",
    );
    egui::Grid::new("ssh_profile_fields")
        .num_columns(2)
        .show(ui, |ui| {
            text_row(ui, "Host", &mut config.host);
            ui.label("SSH port");
            ui.add(egui::DragValue::new(&mut config.port).range(1..=u16::MAX));
            ui.end_row();
            text_row(ui, "Username", &mut config.username);
            text_row(ui, "Pinned host public key", &mut config.expected_host_key);
            text_row(ui, "Credential reference", &mut config.credential_ref);
            let mut command_subsystem = config.command_subsystem.clone().unwrap_or_default();
            text_row(ui, "CTARGV1 subsystem (optional)", &mut command_subsystem);
            config.command_subsystem = (!command_subsystem.is_empty()).then_some(command_subsystem);
            ui.label("");
            ui.weak("Blank: standard SSH exec; no remote argv helper required");
            ui.end_row();
            text_row(ui, "Capture recipe ID", &mut config.capture_recipe);
            text_row(ui, "Remote artifact root", &mut config.remote_artifact_dir);
            text_row(
                ui,
                "Artifact basename glob",
                &mut config.remote_artifact_glob,
            );
            let mut event = config.remote_event_subsystem.clone().unwrap_or_default();
            text_row(ui, "Event subsystem (optional)", &mut event);
            config.remote_event_subsystem = (!event.is_empty()).then_some(event);
            ui.label("");
            ui.weak("Blank: directory polling fallback");
            ui.end_row();
            ui.label("Passive watcher");
            ui.checkbox(
                &mut config.passive_watch_auto_open,
                "Auto-open stable watched files in background",
            );
            ui.end_row();
            ui.label("");
            ui.weak("Unchecked: Assets only; never open a tab");
            ui.end_row();
            ui.label("Stable samples");
            ui.add(egui::DragValue::new(&mut config.stable_samples).range(2..=10));
            ui.end_row();
            ui.label("Stability interval ms");
            ui.add(egui::DragValue::new(&mut config.stability_interval_ms).range(100..=5000));
            ui.end_row();
            ui.label("Fetch hard limit bytes");
            ui.add(egui::DragValue::new(&mut config.max_fetch_bytes).range(1..=1_073_741_824_u64));
            ui.end_row();
        });
    ui.group(|ui| {
        ui.label("Register session-only password (never saved)");
        ui.horizontal(|ui| {
            ui.label("Session ID");
            ui.text_edit_singleline(session_id);
            ui.label("Secret");
            ui.add(egui::TextEdit::singleline(session_secret).password(true));
            if ui.button("Register for this session").clicked() {
                actions.push(DeviceManagerAction::RegisterSessionSecret {
                    id: session_id.clone(),
                    secret: std::mem::take(session_secret),
                });
            }
        });
    });
}

fn text_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.text_edit_singleline(value);
    ui.end_row();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_variant_discards_cross_platform_fields() {
        let mut draft = ProfileDraft::new(DraftVariant::Cv610);
        let PlatformConfig::HisiliconCv610(config) = &mut draft.config else {
            panic!()
        };
        config.host = "10.0.0.1".to_owned();
        draft.switch_variant(DraftVariant::SshManaged);
        let PlatformConfig::SshManaged(config) = &draft.config else {
            panic!()
        };
        assert!(config.host.is_empty());
        assert!(config.capture_recipe.is_empty());
        assert!(config.command_subsystem.is_none());
        draft.switch_variant(DraftVariant::Local);
        assert!(matches!(draft.config, PlatformConfig::Local(_)));
    }

    #[test]
    fn rdk_template_is_incomplete_and_cannot_claim_acceptance() {
        let draft = ProfileDraft::rdk_x5_unknown();
        assert!(draft.display_name.contains("Unknown"));
        assert!(draft.build().is_err());
    }
}
