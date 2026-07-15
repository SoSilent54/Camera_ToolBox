//! Device Manager 独立 viewport；所有编辑先落在 draft，验证成功后才提交。

use std::path::PathBuf;

use camera_toolbox_app::{
    Cv610Config, Cv610DumpConfig, Cv610StreamConfig, LocalConfig, PlatformConfig, PlatformProfile,
    PlatformProfileId, SshManagedConfig,
};
use eframe::egui;

use super::ssh_profile::{SshCommitAuth, SshEditorState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DraftVariant {
    Local,
    Cv610,
    SshManaged,
}

/// Clone/Debug draft 只含可持久化配置，绝不持有 password。
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
    ssh: SshEditorState,
}

impl Default for DeviceManagerState {
    fn default() -> Self {
        Self {
            open: false,
            draft: None,
            message: None,
            ssh: SshEditorState::new(),
        }
    }
}

impl DeviceManagerState {
    pub(crate) fn edit(&mut self, profile: &PlatformProfile, password_registered: bool) {
        self.ssh.clear_sensitive();
        self.ssh = SshEditorState::from_profile(profile, password_registered);
        self.draft = Some(ProfileDraft::from_profile(profile));
        self.open = true;
        self.message = None;
    }

    pub(crate) fn mark_saved(&mut self, profile: &PlatformProfile, password_registered: bool) {
        self.ssh.clear_sensitive();
        self.ssh = SshEditorState::from_profile(profile, password_registered);
        self.draft = Some(ProfileDraft::from_profile(profile));
    }

    pub(crate) fn mark_deleted(&mut self) {
        self.ssh.clear_sensitive();
        self.ssh = SshEditorState::new();
        self.draft = None;
    }

    fn close(&mut self) {
        self.open = false;
        self.ssh.clear_sensitive();
        self.draft = None;
    }

    pub(crate) fn show(&mut self, context: &egui::Context) -> Vec<DeviceManagerAction> {
        if !self.open {
            return Vec::new();
        }
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
                        render_draft(ui, draft, &mut self.ssh, &mut actions);
                    });
                });
            },
        );
        actions
    }

    fn render_toolbar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<DeviceManagerAction>) {
        ui.horizontal(|ui| {
            if ui.button("New Local").clicked() {
                self.ssh.clear_sensitive();
                self.ssh = SshEditorState::new();
                self.draft = Some(ProfileDraft::new(DraftVariant::Local));
            }
            if ui.button("New CV610").clicked() {
                self.ssh.clear_sensitive();
                self.ssh = SshEditorState::new();
                self.draft = Some(ProfileDraft::new(DraftVariant::Cv610));
            }
            if ui.button("New SSH-managed").clicked() {
                self.ssh.clear_sensitive();
                self.ssh = SshEditorState::new();
                self.draft = Some(ProfileDraft::new(DraftVariant::SshManaged));
            }
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

fn render_draft(
    ui: &mut egui::Ui,
    draft: &mut ProfileDraft,
    ssh: &mut SshEditorState,
    actions: &mut Vec<DeviceManagerAction>,
) {
    ui.heading("Profile draft");
    ui.horizontal(|ui| {
        ui.label("Variant");
        let current = draft.variant();
        for (variant, label) in [
            (DraftVariant::Local, "Local"),
            (DraftVariant::Cv610, "Hisilicon CV610"),
            (DraftVariant::SshManaged, "SSH-managed"),
        ] {
            if ui.selectable_label(current == variant, label).clicked() && current != variant {
                ssh.reset_for_variant();
                draft.switch_variant(variant);
            }
        }
    });
    ui.collapsing("Profile details", |ui| {
        ui.weak("For a new SSH profile, blanks are generated from username@host when saved.");
        egui::Grid::new("profile_common_fields")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Profile ID");
                ui.add_enabled(
                    draft.original_id.is_none(),
                    egui::TextEdit::singleline(&mut draft.id),
                );
                ui.end_row();
                ui.label("Display name");
                ui.text_edit_singleline(&mut draft.display_name);
                ui.end_row();
            });
    });
    ui.separator();
    match &mut draft.config {
        PlatformConfig::Local(_) => {
            ui.label("Local profile has no network or credential fields.");
        }
        PlatformConfig::HisiliconCv610(config) => render_cv610(ui, config),
        PlatformConfig::SshManaged(config) => ssh.render(ui, config),
    }
    ui.separator();
    ui.horizontal(|ui| {
        if ui.button("Validate and Save").clicked() {
            let mut candidate = draft.clone();
            let auth = match &mut candidate.config {
                PlatformConfig::SshManaged(config) => {
                    match ssh.prepare_commit(config, candidate.original_id.is_none()) {
                        Ok(auth) => Some(auth),
                        Err(error) => {
                            actions.push(DeviceManagerAction::ValidationError(error));
                            return;
                        }
                    }
                }
                _ => None,
            };
            actions.push(DeviceManagerAction::Commit {
                draft: candidate,
                ssh_auth: auth,
            });
        }
        if let Some(id) = draft.original_id.clone()
            && ui.button("Delete profile").clicked()
        {
            ssh.clear_sensitive();
            actions.push(DeviceManagerAction::Delete(id));
        }
        if ui.button("Discard draft").clicked() {
            discard_draft(draft, ssh);
        }
    });
}
fn discard_draft(draft: &mut ProfileDraft, ssh: &mut SshEditorState) {
    ssh.clear_sensitive();
    *ssh = SshEditorState::new();
    draft.original_id = None;
    draft.id.clear();
    draft.display_name.clear();
    draft.switch_variant(DraftVariant::Local);
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

#[cfg(test)]
mod tests {
    use camera_toolbox_app::ProfileStore;

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

    #[test]
    fn draft_clone_debug_and_profile_json_never_contain_password() {
        let mut draft = ProfileDraft::new(DraftVariant::SshManaged);
        draft.id = "camera".to_owned();
        draft.display_name = "camera".to_owned();
        let PlatformConfig::SshManaged(config) = &mut draft.config else {
            panic!()
        };
        config.host = "camera.example".to_owned();
        config.username = "root".to_owned();
        config.expected_host_key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ"
                .to_owned();
        config.credential_ref = "session:camera".to_owned();
        config.remote_artifact_dir = "/data".to_owned();
        config.remote_artifact_glob = "frame.raw".to_owned();
        let mut manager = DeviceManagerState::default();
        manager.ssh.set_password_for_test("super-secret");
        assert!(!manager.ssh.password_is_empty());
        manager.draft = Some(draft.clone());
        let debug = format!("{:?}", manager.draft.as_ref().unwrap().clone());
        assert!(!debug.contains("super-secret"));
        let profile = manager.draft.as_ref().unwrap().build().unwrap();
        let mut store = ProfileStore::new();
        store.insert_platform(profile).unwrap();
        let json = String::from_utf8(store.export_json().unwrap()).unwrap();
        assert!(!json.contains("super-secret"));
        assert!(json.contains("session:camera"));

        let mut discard = manager.draft.take().unwrap();
        discard_draft(&mut discard, &mut manager.ssh);
        assert!(manager.ssh.password_is_empty());
        manager.ssh.set_password_for_test("super-secret");
        manager.ssh.reset_for_variant();
        assert!(manager.ssh.password_is_empty());
        manager.ssh.set_password_for_test("super-secret");
        manager.draft = Some(draft);
        manager.close();
        assert!(manager.ssh.password_is_empty());
        assert!(manager.draft.is_none());
    }
}
