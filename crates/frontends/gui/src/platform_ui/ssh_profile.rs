//! SSH profile draft editor：密码优先认证、远端文件与 server identity。

use std::path::{Path, PathBuf};

use camera_toolbox_adapters::platforms::ssh_managed::{
    HostKeyAssessment, HostKeyTarget, ServerHostKey, discover_private_key_files,
    discover_private_key_files_in,
};
use camera_toolbox_app::{PlatformConfig, PlatformProfile, PlatformProfileId, SshManagedConfig};
use eframe::egui::{self, TextBuffer};
use secrecy::{ExposeSecret, ExposeSecretMut, SecretBox, SecretString, zeroize::Zeroize};

use super::host_key_worker::{HostKeyWorker, HostKeyWorkerResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SshAuthMode {
    Password,
    PrivateKeyFile,
}

/// Commit 动作独占 secret，刻意不实现 Clone/Debug。
pub(crate) enum SshCommitAuth {
    Password { password: Option<SecretString> },
    PrivateKeyFile { path: PathBuf },
}

struct SecretInput {
    value: SecretBox<String>,
}

impl Default for SecretInput {
    fn default() -> Self {
        Self {
            value: SecretBox::new(Box::default()),
        }
    }
}

impl SecretInput {
    fn is_empty(&self) -> bool {
        self.value.expose_secret().is_empty()
    }

    fn clear(&mut self) {
        self.value.zeroize();
    }

    fn take(&mut self) -> Option<SecretString> {
        let value = std::mem::take(self.value.expose_secret_mut());
        (!value.is_empty()).then(|| SecretString::from(value))
    }
}

impl TextBuffer for SecretInput {
    fn is_mutable(&self) -> bool {
        true
    }

    fn as_str(&self) -> &str {
        self.value.expose_secret()
    }

    fn insert_text(&mut self, text: &str, char_index: egui::text::CharIndex) -> usize {
        self.value.expose_secret_mut().insert_text(text, char_index)
    }

    fn delete_char_range(&mut self, char_range: std::ops::Range<egui::text::CharIndex>) {
        self.value.expose_secret_mut().delete_char_range(char_range);
    }

    fn type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<Self>()
    }
}

#[derive(Clone)]
enum HostIdentityStatus {
    Unverified,
    Pinned {
        algorithm: String,
        fingerprint: String,
    },
    Scanning,
    Trusted {
        algorithm: String,
        fingerprint: String,
    },
    Unknown {
        key: ServerHostKey,
        verified_out_of_band: bool,
    },
    Trusting,
    Changed {
        algorithm: String,
        fingerprint: String,
    },
    Error(String),
}

pub(super) struct SshEditorState {
    auth_mode: SshAuthMode,
    password_required: bool,
    password: SecretInput,
    private_keys: Vec<PathBuf>,
    selected_private_key: Option<PathBuf>,
    key_discovery_message: Option<String>,
    exact_remote_file: String,
    advanced_remote_path: bool,
    capture_enabled: bool,
    rdk_template: bool,
    host_generation: u64,
    host_status: HostIdentityStatus,
    host_worker: Option<HostKeyWorker>,
}

impl Default for SshEditorState {
    fn default() -> Self {
        Self::new()
    }
}

impl SshEditorState {
    pub(super) fn new() -> Self {
        #[cfg(test)]
        {
            return Self::for_test();
        }
        #[cfg(not(test))]
        {
            let (host_worker, host_status) = match HostKeyWorker::production() {
                Ok(worker) => (Some(worker), HostIdentityStatus::Unverified),
                Err(error) => (None, HostIdentityStatus::Error(error)),
            };
            let mut state = Self::base(host_worker, host_status);
            state.refresh_private_keys();
            state
        }
    }

    fn base(host_worker: Option<HostKeyWorker>, host_status: HostIdentityStatus) -> Self {
        Self {
            auth_mode: SshAuthMode::Password,
            password_required: true,
            password: SecretInput::default(),
            private_keys: Vec::new(),
            selected_private_key: None,
            key_discovery_message: None,
            exact_remote_file: String::new(),
            advanced_remote_path: false,
            capture_enabled: false,
            rdk_template: false,
            host_generation: 1,
            host_status,
            host_worker,
        }
    }

    #[cfg(test)]
    fn for_test() -> Self {
        Self::base(None, HostIdentityStatus::Unverified)
    }

    pub(super) fn from_profile(profile: &PlatformProfile, password_registered: bool) -> Self {
        let mut state = Self::new();
        let PlatformConfig::SshManaged(config) = &profile.config else {
            return state;
        };
        state.auth_mode = if let Some(path) = config.credential_ref.strip_prefix("key-file:") {
            state.selected_private_key = Some(PathBuf::from(path));
            state.password_required = false;
            SshAuthMode::PrivateKeyFile
        } else {
            state.password_required = !password_registered;
            SshAuthMode::Password
        };
        state.capture_enabled = !config.capture_recipe.is_empty();
        if is_literal_remote_glob(&config.remote_artifact_glob) {
            state.exact_remote_file =
                combine_remote_file(&config.remote_artifact_dir, &config.remote_artifact_glob);
        } else {
            state.advanced_remote_path = true;
        }
        state.host_status = status_from_existing_pin(&config.expected_host_key);
        state
    }

    pub(super) fn rdk_template() -> Self {
        let mut state = Self::new();
        state.rdk_template = true;
        state
    }

    pub(super) fn clear_sensitive(&mut self) {
        self.password.clear();
        self.host_generation = self.host_generation.wrapping_add(1);
    }

    pub(super) fn reset_for_variant(&mut self) {
        self.clear_sensitive();
        *self = Self::new();
    }

    pub(super) fn poll(&mut self, config: &mut SshManagedConfig, dialog_open: bool) {
        loop {
            let result = self.host_worker.as_mut().and_then(HostKeyWorker::try_recv);
            let Some(result) = result else { break };
            if dialog_open {
                self.apply_worker_result(config, result);
            }
        }
    }

    pub(super) fn render(&mut self, ui: &mut egui::Ui, config: &mut SshManagedConfig) {
        ui.heading("SSH-managed");
        if self.rdk_template {
            ui.colored_label(
                egui::Color32::YELLOW,
                "RDK X5 acceptance is Unknown; fill fields from deployment evidence.",
            );
        }

        let original_endpoint = (config.host.clone(), config.port);
        egui::Grid::new("ssh_basic_fields")
            .num_columns(2)
            .show(ui, |ui| {
                text_row(ui, "Host / IP", &mut config.host);
                ui.label("SSH port");
                ui.add(egui::DragValue::new(&mut config.port).range(1..=u16::MAX));
                ui.end_row();
                text_row(ui, "Username", &mut config.username);
            });
        if original_endpoint != (config.host.clone(), config.port) {
            config.expected_host_key.clear();
            self.host_generation = self.host_generation.wrapping_add(1);
            self.host_status = HostIdentityStatus::Unverified;
        }

        self.render_authentication(ui);
        self.render_server_identity(ui, config);

        ui.separator();
        ui.label("Remote file");
        ui.checkbox(
            &mut self.advanced_remote_path,
            "Advanced root / basename glob mode",
        );
        if self.advanced_remote_path {
            egui::Grid::new("ssh_remote_advanced")
                .num_columns(2)
                .show(ui, |ui| {
                    text_row(ui, "Remote root", &mut config.remote_artifact_dir);
                    text_row(ui, "Basename glob", &mut config.remote_artifact_glob);
                });
        } else {
            ui.add(
                egui::TextEdit::singleline(&mut self.exact_remote_file)
                    .hint_text("/absolute/path/to/capture.raw"),
            );
            ui.weak("Exact absolute remote file; saved as parent root plus literal basename.");
        }

        ui.collapsing("Advanced", |ui| self.render_advanced(ui, config));
    }

    fn render_authentication(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label("Client authentication");
        let previous = self.auth_mode;
        egui::ComboBox::from_id_salt("ssh_auth_mode")
            .selected_text(match self.auth_mode {
                SshAuthMode::Password => "Password",
                SshAuthMode::PrivateKeyFile => "SSH private key file",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.auth_mode, SshAuthMode::Password, "Password");
                ui.selectable_value(
                    &mut self.auth_mode,
                    SshAuthMode::PrivateKeyFile,
                    "SSH private key file",
                );
            });
        self.apply_auth_change(previous);

        match self.auth_mode {
            SshAuthMode::Password => {
                ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                ui.weak("Process memory only / never saved");
            }
            SshAuthMode::PrivateKeyFile => {
                ui.strong("Client login key (not the server host key)");
                let selected = self
                    .selected_private_key
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "Choose a private key".to_owned());
                egui::ComboBox::from_id_salt("ssh_private_key_file")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for path in &self.private_keys {
                            ui.selectable_value(
                                &mut self.selected_private_key,
                                Some(path.clone()),
                                path.display().to_string(),
                            );
                        }
                    });
                ui.horizontal(|ui| {
                    if ui.button("Refresh discovered keys").clicked() {
                        self.refresh_private_keys();
                    }
                    if ui.button("Choose key file...").clicked()
                        && let Some(path) = rfd::FileDialog::new().pick_file()
                    {
                        self.select_private_key(path);
                    }
                });
                if let Some(message) = self.key_discovery_message.as_deref() {
                    ui.weak(message);
                }
            }
        }
    }
    fn apply_auth_change(&mut self, previous: SshAuthMode) {
        if previous == self.auth_mode {
            return;
        }
        self.password.clear();
        if matches!(self.auth_mode, SshAuthMode::Password) {
            self.password_required = true;
        }
        if matches!(self.auth_mode, SshAuthMode::PrivateKeyFile) {
            self.refresh_private_keys();
        }
    }

    fn render_server_identity(&mut self, ui: &mut egui::Ui, config: &mut SshManagedConfig) {
        ui.separator();
        ui.label("Server identity (separate from client authentication)");
        let mut scan = false;
        let mut trust = None;
        match &mut self.host_status {
            HostIdentityStatus::Unverified => {
                ui.colored_label(egui::Color32::YELLOW, "Server identity not verified");
                scan = ui.button("Verify server identity").clicked();
            }
            HostIdentityStatus::Pinned {
                algorithm,
                fingerprint,
            } => {
                ui.label(format!("Pinned profile key: {algorithm} {fingerprint}"));
                scan = ui.button("Re-verify without authentication").clicked();
            }
            HostIdentityStatus::Scanning => {
                ui.spinner();
                ui.label("Scanning server key without authentication...");
            }
            HostIdentityStatus::Trusted {
                algorithm,
                fingerprint,
            } => {
                ui.colored_label(
                    egui::Color32::LIGHT_GREEN,
                    format!("Trusted: {algorithm} {fingerprint}"),
                );
            }
            HostIdentityStatus::Unknown {
                key,
                verified_out_of_band,
            } => {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    format!("Unknown: {} {}", key.algorithm(), key.fingerprint()),
                );
                ui.checkbox(
                    verified_out_of_band,
                    "I verified this fingerprint out of band",
                );
                if ui
                    .add_enabled(
                        unknown_trust_allowed(*verified_out_of_band),
                        egui::Button::new("Trust this server key"),
                    )
                    .clicked()
                {
                    trust = Some(key.clone());
                }
            }
            HostIdentityStatus::Trusting => {
                ui.spinner();
                ui.label("Recording explicitly trusted server key...");
            }
            HostIdentityStatus::Changed {
                algorithm,
                fingerprint,
            } => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("Server key changed: {algorithm} {fingerprint}"),
                );
                ui.strong("Connection is blocked; investigate known_hosts out of band.");
            }
            HostIdentityStatus::Error(error) => {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
                scan = ui.button("Retry verification").clicked();
            }
        }
        if scan {
            self.request_scan(config);
        }
        if let Some(key) = trust {
            self.request_trust(config, key);
        }
    }

    fn render_advanced(&mut self, ui: &mut egui::Ui, config: &mut SshManagedConfig) {
        ui.checkbox(&mut self.capture_enabled, "Enable capture automation");
        if self.capture_enabled {
            text_row(ui, "Capture recipe ID", &mut config.capture_recipe);
            let mut command = config.command_subsystem.clone().unwrap_or_default();
            text_row(ui, "Command subsystem (optional)", &mut command);
            config.command_subsystem = (!command.is_empty()).then_some(command);
            ui.weak("Blank subsystem uses standard SSH exec.");
        } else {
            config.capture_recipe.clear();
            config.command_subsystem = None;
            ui.weak("Fetch-only profile; capture command is not bound.");
        }
        let mut event = config.remote_event_subsystem.clone().unwrap_or_default();
        text_row(ui, "Discovery subsystem (optional)", &mut event);
        config.remote_event_subsystem = (!event.is_empty()).then_some(event);
        ui.checkbox(
            &mut config.passive_watch_auto_open,
            "Auto-open stable watched files in background",
        );
        ui.label("Stable samples");
        ui.add(egui::DragValue::new(&mut config.stable_samples).range(2..=10));
        ui.label("Stability interval ms");
        ui.add(egui::DragValue::new(&mut config.stability_interval_ms).range(100..=5000));
        ui.label("Fetch hard limit bytes");
        ui.add(egui::DragValue::new(&mut config.max_fetch_bytes).range(1..=1_073_741_824_u64));
        ui.label("Command output limit bytes");
        ui.add(egui::DragValue::new(&mut config.command_output_bytes).range(1..=16_777_216_u64));
    }

    pub(super) fn prepare_commit(
        &mut self,
        config: &mut SshManagedConfig,
        is_new: bool,
    ) -> Result<SshCommitAuth, String> {
        if !matches!(
            self.host_status,
            HostIdentityStatus::Pinned { .. } | HostIdentityStatus::Trusted { .. }
        ) || config.expected_host_key.is_empty()
        {
            return Err("verify and trust the SSH server identity before saving".to_owned());
        }
        if self.advanced_remote_path {
            if config.remote_artifact_dir.is_empty() || config.remote_artifact_glob.is_empty() {
                return Err("advanced remote root and basename glob are required".to_owned());
            }
        } else {
            let (root, basename) = split_exact_remote_file(&self.exact_remote_file)?;
            config.remote_artifact_dir = root;
            config.remote_artifact_glob = basename;
        }
        let selected_key = match self.auth_mode {
            SshAuthMode::Password => {
                if (is_new || self.password_required) && self.password.is_empty() {
                    return Err(
                        "password is required after selecting password authentication".to_owned(),
                    );
                }
                None
            }
            SshAuthMode::PrivateKeyFile => {
                let path = self
                    .selected_private_key
                    .as_deref()
                    .ok_or_else(|| "choose a valid SSH client private key file".to_owned())?;
                Some(validate_client_private_key(path)?)
            }
        };
        let mut validation = config.clone();
        validation.credential_ref = match selected_key.as_ref() {
            Some(path) => format!("key-file:{}", path.display()),
            None => "session:validation".to_owned(),
        };
        PlatformProfile {
            id: PlatformProfileId::new("validation").expect("constant ID is valid"),
            display_name: "validation".to_owned(),
            config: PlatformConfig::SshManaged(validation),
        }
        .validate()
        .map_err(|error| error.to_string())?;

        match selected_key {
            Some(path) => {
                self.password.clear();
                Ok(SshCommitAuth::PrivateKeyFile { path })
            }
            None => Ok(SshCommitAuth::Password {
                password: self.password.take(),
            }),
        }
    }

    fn request_scan(&mut self, config: &SshManagedConfig) {
        let target = match HostKeyTarget::new(config.host.clone(), config.port) {
            Ok(target) => target,
            Err(error) => {
                self.host_status = HostIdentityStatus::Error(error.to_string());
                return;
            }
        };
        self.host_generation = self.host_generation.wrapping_add(1);
        let generation = self.host_generation;
        match self.host_worker.as_mut() {
            Some(worker) => match worker.request_scan(generation, target) {
                Ok(()) => self.host_status = HostIdentityStatus::Scanning,
                Err(error) => self.host_status = HostIdentityStatus::Error(error),
            },
            None => {
                self.host_status =
                    HostIdentityStatus::Error("host-key worker is unavailable".to_owned())
            }
        }
    }

    fn request_trust(&mut self, config: &SshManagedConfig, key: ServerHostKey) {
        let target = match HostKeyTarget::new(config.host.clone(), config.port) {
            Ok(target) => target,
            Err(error) => {
                self.host_status = HostIdentityStatus::Error(error.to_string());
                return;
            }
        };
        self.host_generation = self.host_generation.wrapping_add(1);
        let generation = self.host_generation;
        match self.host_worker.as_mut() {
            Some(worker) => match worker.request_trust(generation, target, key) {
                Ok(()) => self.host_status = HostIdentityStatus::Trusting,
                Err(error) => self.host_status = HostIdentityStatus::Error(error),
            },
            None => {
                self.host_status =
                    HostIdentityStatus::Error("host-key worker is unavailable".to_owned())
            }
        }
    }

    fn apply_worker_result(&mut self, config: &mut SshManagedConfig, result: HostKeyWorkerResult) {
        match result {
            HostKeyWorkerResult::Scan {
                generation,
                target,
                result,
            } => {
                if generation != self.host_generation || !target_matches(&target, config) {
                    return;
                }
                self.host_status = match result {
                    Ok(assessment) => {
                        let key = assessment.server_key().clone();
                        if !config.expected_host_key.is_empty() {
                            if config.expected_host_key == key.openssh() {
                                config.expected_host_key = key.openssh().to_owned();
                                trusted_status(&key)
                            } else {
                                HostIdentityStatus::Changed {
                                    algorithm: key.algorithm().to_owned(),
                                    fingerprint: key.fingerprint().to_owned(),
                                }
                            }
                        } else {
                            match assessment {
                                HostKeyAssessment::Trusted(key) => {
                                    config.expected_host_key = key.openssh().to_owned();
                                    trusted_status(&key)
                                }
                                HostKeyAssessment::Unknown(key) => HostIdentityStatus::Unknown {
                                    key,
                                    verified_out_of_band: false,
                                },
                                HostKeyAssessment::Changed(key) => HostIdentityStatus::Changed {
                                    algorithm: key.algorithm().to_owned(),
                                    fingerprint: key.fingerprint().to_owned(),
                                },
                            }
                        }
                    }
                    Err(error) => HostIdentityStatus::Error(error),
                };
            }
            HostKeyWorkerResult::Trust {
                generation,
                target,
                key,
                result,
            } => {
                if generation != self.host_generation || !target_matches(&target, config) {
                    return;
                }
                self.host_status = match result {
                    Ok(_) => {
                        config.expected_host_key = key.openssh().to_owned();
                        trusted_status(&key)
                    }
                    Err(error) => HostIdentityStatus::Error(error),
                };
            }
        }
    }

    fn refresh_private_keys(&mut self) {
        self.refresh_private_keys_with(|| {
            discover_private_key_files().map_err(|error| error.to_string())
        });
    }

    fn refresh_private_keys_with(
        &mut self,
        discover: impl FnOnce() -> Result<Vec<PathBuf>, String>,
    ) {
        match discover() {
            Ok(paths) => {
                self.private_keys = paths;
                self.key_discovery_message = if self.private_keys.is_empty() {
                    Some("No valid OpenSSH private keys discovered in ~/.ssh".to_owned())
                } else {
                    None
                };
            }
            Err(error) => self.key_discovery_message = Some(error),
        }
    }

    fn select_private_key(&mut self, path: PathBuf) {
        match validate_client_private_key(&path) {
            Ok(canonical) => {
                if !self.private_keys.contains(&canonical) {
                    self.private_keys.push(canonical.clone());
                    self.private_keys.sort();
                }
                self.selected_private_key = Some(canonical);
                self.key_discovery_message = None;
            }
            Err(error) => self.key_discovery_message = Some(error),
        }
    }

    #[cfg(test)]
    pub(super) fn set_password_for_test(&mut self, value: &str) {
        self.password.clear();
        self.password
            .insert_text(value, egui::text::CharIndex::default());
    }

    #[cfg(test)]
    pub(super) fn password_is_empty(&self) -> bool {
        self.password.is_empty()
    }
}
fn validate_client_private_key(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("SSH client private key path must be absolute".to_owned());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "private-key file has no parent directory".to_owned())?;
    let candidates = discover_private_key_files_in(parent).map_err(|error| error.to_string())?;
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("cannot normalize private-key path: {error}"))?;
    candidates
        .contains(&canonical)
        .then_some(canonical)
        .ok_or_else(|| "Selected file is not a valid protected OpenSSH private key".to_owned())
}

fn status_from_existing_pin(pin: &str) -> HostIdentityStatus {
    if pin.is_empty() {
        return HostIdentityStatus::Unverified;
    }
    match ServerHostKey::from_openssh(pin) {
        Ok(key) => HostIdentityStatus::Pinned {
            algorithm: key.algorithm().to_owned(),
            fingerprint: key.fingerprint().to_owned(),
        },
        Err(_) => HostIdentityStatus::Error(
            "Stored server key is invalid; verify the server identity again".to_owned(),
        ),
    }
}

fn trusted_status(key: &ServerHostKey) -> HostIdentityStatus {
    HostIdentityStatus::Trusted {
        algorithm: key.algorithm().to_owned(),
        fingerprint: key.fingerprint().to_owned(),
    }
}

fn target_matches(target: &HostKeyTarget, config: &SshManagedConfig) -> bool {
    target.host() == config.host && target.port() == config.port
}

fn unknown_trust_allowed(verified_out_of_band: bool) -> bool {
    verified_out_of_band
}

pub(super) fn is_literal_remote_glob(glob: &str) -> bool {
    !glob.is_empty() && !glob.bytes().any(|byte| matches!(byte, b'*' | b'?'))
}

pub(super) fn combine_remote_file(root: &str, basename: &str) -> String {
    if root == "/" {
        format!("/{basename}")
    } else {
        format!("{}/{basename}", root.trim_end_matches('/'))
    }
}

pub(super) fn split_exact_remote_file(path: &str) -> Result<(String, String), String> {
    if !path.starts_with('/') || path.ends_with('/') || path.contains('\0') {
        return Err("remote file must be an exact absolute file path".to_owned());
    }
    let (root, basename) = path
        .rsplit_once('/')
        .ok_or_else(|| "remote file must include a literal basename".to_owned())?;
    if basename.is_empty() || basename.bytes().any(|byte| matches!(byte, b'*' | b'?')) {
        return Err("remote file basename must be literal (no '*' or '?')".to_owned());
    }
    if path.split('/').any(|component| component == "..") {
        return Err("remote file must not contain '..' path segments".to_owned());
    }
    Ok((
        if root.is_empty() { "/" } else { root }.to_owned(),
        basename.to_owned(),
    ))
}

fn text_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.text_edit_singleline(value);
    ui.end_row();
}

#[cfg(test)]
#[path = "ssh_profile_tests.rs"]
mod tests;
