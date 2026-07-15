//! SSH profile draft editor：密码优先认证、远端文件与 server identity。

use std::path::PathBuf;

use camera_toolbox_adapters::platforms::ssh_managed::{
    HostKeyAssessment, HostKeyTarget, ServerHostKey, discover_private_key_files,
};
use camera_toolbox_app::{PlatformConfig, PlatformProfile, PlatformProfileId, SshManagedConfig};
use eframe::egui::{self, TextBuffer};
use secrecy::{ExposeSecret, ExposeSecretMut, SecretBox, SecretString, zeroize::Zeroize};

#[path = "ssh_profile_path.rs"]
mod remote_path;
pub(super) use remote_path::{combine_remote_file, is_literal_remote_glob};
use remote_path::{split_exact_remote_file, validate_client_private_key};

use super::host_key_worker::{
    HostKeyWorker, HostKeyWorkerResult, SshConnectionTestAuth, SshConnectionTestRequest,
    SshConnectionTestResult,
};

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

    /// 为连接测试复制一次 secret，保留界面输入，直到用户显式清除或提交保存。
    fn clone_secret(&self) -> Option<SecretString> {
        (!self.is_empty()).then(|| SecretString::from(self.value.expose_secret().clone()))
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

enum ConnectionTestStatus {
    Idle,
    WaitingIdentity,
    Testing,
    Success {
        path: String,
        size: u64,
        modified_seconds: u64,
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
    connection_generation: u64,
    connection_status: ConnectionTestStatus,
    pending_connection_test: Option<SshConnectionTestRequest>,
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
            connection_generation: 1,
            connection_status: ConnectionTestStatus::Idle,
            pending_connection_test: None,
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
        self.invalidate_connection_test();
    }

    pub(super) fn reset_for_variant(&mut self) {
        self.clear_sensitive();
        *self = Self::new();
    }

    pub(super) fn poll(&mut self, config: &mut SshManagedConfig, dialog_open: bool) {
        loop {
            let host_result = self.host_worker.as_mut().and_then(HostKeyWorker::try_recv);
            let connection_result = self
                .host_worker
                .as_mut()
                .and_then(HostKeyWorker::try_recv_connection_test);
            if host_result.is_none() && connection_result.is_none() {
                break;
            }
            if dialog_open {
                if let Some(result) = host_result {
                    self.apply_worker_result(config, result);
                }
                if let Some(result) = connection_result {
                    self.apply_connection_test_result(config, result);
                }
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
        let original_username = config.username.clone();
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
            self.invalidate_connection_test();
        } else if original_username != config.username {
            self.invalidate_connection_test();
        }

        if self.render_authentication(ui) {
            self.invalidate_connection_test();
        }

        ui.separator();
        ui.label("Remote file");
        let original_remote = (
            self.advanced_remote_path,
            self.exact_remote_file.clone(),
            config.remote_artifact_dir.clone(),
            config.remote_artifact_glob.clone(),
        );
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
                    text_row(ui, "测试文件（精确路径）", &mut self.exact_remote_file);
                });
            ui.weak("测试文件只用于登录与 SFTP 元数据检查；保存仍保留 root / basename glob。");
        } else {
            ui.add(
                egui::TextEdit::singleline(&mut self.exact_remote_file)
                    .hint_text("/absolute/path/to/capture.raw"),
            );
            ui.weak("Exact absolute remote file; saved as parent root plus literal basename.");
        }
        if original_remote
            != (
                self.advanced_remote_path,
                self.exact_remote_file.clone(),
                config.remote_artifact_dir.clone(),
                config.remote_artifact_glob.clone(),
            )
        {
            self.invalidate_connection_test();
        }

        let test_in_progress = self
            .host_worker
            .as_ref()
            .is_some_and(HostKeyWorker::is_in_flight)
            || matches!(
                self.connection_status,
                ConnectionTestStatus::WaitingIdentity | ConnectionTestStatus::Testing
            )
            || matches!(
                self.host_status,
                HostIdentityStatus::Scanning | HostIdentityStatus::Trusting
            );
        if ui
            .add_enabled(
                !test_in_progress,
                egui::Button::new("测试 SSH 登录与远程路径").fill(ui.visuals().selection.bg_fill),
            )
            .clicked()
        {
            self.start_connection_test(config);
        }
        self.render_connection_status(ui);
        self.render_server_identity(ui, config);

        ui.collapsing("Advanced", |ui| self.render_advanced(ui, config));
    }

    fn render_authentication(&mut self, ui: &mut egui::Ui) -> bool {
        ui.separator();
        ui.label("Client authentication");
        let previous = self.auth_mode;
        let previous_key = self.selected_private_key.clone();
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

        let mut changed = previous != self.auth_mode;
        match self.auth_mode {
            SshAuthMode::Password => {
                changed |= ui
                    .add(egui::TextEdit::singleline(&mut self.password).password(true))
                    .changed();
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
                changed |= previous_key != self.selected_private_key;
            }
        }
        changed
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

    fn render_connection_status(&self, ui: &mut egui::Ui) {
        match &self.connection_status {
            ConnectionTestStatus::Idle => {
                if self
                    .host_worker
                    .as_ref()
                    .is_some_and(HostKeyWorker::is_in_flight)
                {
                    ui.spinner();
                    ui.label("正在结束上一次 SSH 操作，请稍候...");
                }
            }
            ConnectionTestStatus::WaitingIdentity => {
                ui.spinner();
                ui.label("正在核对 SSH 服务器身份；尚未发送登录凭据...");
            }
            ConnectionTestStatus::Testing => {
                ui.spinner();
                ui.label("正在测试 SSH 登录与远程路径的 SFTP 元数据可见性...");
            }
            ConnectionTestStatus::Success {
                path,
                size,
                modified_seconds,
            } => {
                ui.colored_label(
                    egui::Color32::LIGHT_GREEN,
                    format!(
                        "SSH 登录与 SFTP 元数据可见：{path}，{size} bytes，mtime {modified_seconds}"
                    ),
                );
            }
            ConnectionTestStatus::Error(error) => {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
        }
        ui.weak("测试验证服务器身份、SSH 登录、规范化路径和 SFTP 元数据可见性；不读取文件内容。");
    }

    fn render_server_identity(&mut self, ui: &mut egui::Ui, config: &mut SshManagedConfig) {
        ui.separator();
        ui.strong("服务器身份");
        let mut trust = None;
        match &mut self.host_status {
            HostIdentityStatus::Unverified => {
                ui.colored_label(egui::Color32::YELLOW, "等待测试时读取 SSH 服务器密钥");
            }
            HostIdentityStatus::Pinned {
                algorithm,
                fingerprint,
            } => {
                ui.label(format!("Profile 已固定：{algorithm} {fingerprint}"));
            }
            HostIdentityStatus::Scanning => {
                ui.spinner();
                ui.label("正在读取并核对 SSH 服务端密钥，不发送账号密码...");
            }
            HostIdentityStatus::Trusted {
                algorithm,
                fingerprint,
            } => {
                ui.colored_label(
                    egui::Color32::LIGHT_GREEN,
                    format!("服务器身份可信：{algorithm} {fingerprint}"),
                );
            }
            HostIdentityStatus::Unknown {
                key,
                verified_out_of_band,
            } => {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    format!("未知服务器密钥：{} {}", key.algorithm(), key.fingerprint()),
                );
                ui.checkbox(
                    verified_out_of_band,
                    "我已通过设备控制台或管理员核对该 fingerprint",
                );
                if ui
                    .add_enabled(
                        unknown_trust_allowed(*verified_out_of_band),
                        egui::Button::new("信任并测试"),
                    )
                    .clicked()
                {
                    trust = Some(key.clone());
                }
            }
            HostIdentityStatus::Trusting => {
                ui.spinner();
                ui.label("正在保存已确认的服务器密钥...");
            }
            HostIdentityStatus::Changed {
                algorithm,
                fingerprint,
            } => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("服务器密钥已变化：{algorithm} {fingerprint}"),
                );
                ui.strong("连接已阻止；请通过可信渠道检查 known_hosts 与设备身份。");
            }
            HostIdentityStatus::Error(error) => {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
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

    fn prepare_connection_test_request(
        &self,
        config: &SshManagedConfig,
        generation: u64,
    ) -> Result<SshConnectionTestRequest, String> {
        let mut config = config.clone();
        let remote_path = if self.advanced_remote_path {
            if config.remote_artifact_dir.is_empty() || config.remote_artifact_glob.is_empty() {
                return Err("请先填写远程 root 与 basename glob。".to_owned());
            }
            split_exact_remote_file(&self.exact_remote_file)?;
            self.exact_remote_file.clone()
        } else {
            let (root, basename) = split_exact_remote_file(&self.exact_remote_file)?;
            config.remote_artifact_dir = root;
            config.remote_artifact_glob = basename;
            combine_remote_file(&config.remote_artifact_dir, &config.remote_artifact_glob)
        };
        let auth = match self.auth_mode {
            SshAuthMode::Password => SshConnectionTestAuth::Password(
                self.password
                    .clone_secret()
                    .ok_or_else(|| "请输入密码后再测试 SSH 登录。".to_owned())?,
            ),
            SshAuthMode::PrivateKeyFile => SshConnectionTestAuth::PrivateKeyFile(
                self.selected_private_key
                    .clone()
                    .ok_or_else(|| "请选择 SSH 客户端私钥文件后再测试。".to_owned())?,
            ),
        };
        Ok(SshConnectionTestRequest {
            generation,
            config,
            remote_path,
            auth,
        })
    }

    fn start_connection_test(&mut self, config: &SshManagedConfig) {
        self.invalidate_connection_test();
        self.connection_generation = self.connection_generation.wrapping_add(1);
        let generation = self.connection_generation;
        let request = match self.prepare_connection_test_request(config, generation) {
            Ok(request) => request,
            Err(error) => {
                self.connection_status = ConnectionTestStatus::Error(error);
                return;
            }
        };
        self.pending_connection_test = Some(request);

        if config.expected_host_key.is_empty() {
            self.connection_status = ConnectionTestStatus::WaitingIdentity;
            self.request_scan(config);
            if !matches!(self.host_status, HostIdentityStatus::Scanning) {
                self.fail_pending_connection_test("无法开始 SSH 服务器身份验证。".to_owned());
            }
            return;
        }

        // 已保存 profile 直接使用固定 pin 登录；russh 握手仍会严格拒绝不匹配的服务端密钥。
        match ServerHostKey::from_openssh(&config.expected_host_key) {
            Ok(key) => self.submit_pending_connection_test(&key),
            Err(error) => self.fail_pending_connection_test(format!(
                "Profile 中的服务器密钥无效，无法测试连接：{error}"
            )),
        }
    }

    fn invalidate_connection_test(&mut self) {
        self.connection_generation = self.connection_generation.wrapping_add(1);
        self.pending_connection_test = None;
        self.connection_status = ConnectionTestStatus::Idle;
    }

    fn fail_pending_connection_test(&mut self, error: String) {
        if self.pending_connection_test.take().is_some()
            || matches!(
                self.connection_status,
                ConnectionTestStatus::WaitingIdentity | ConnectionTestStatus::Testing
            )
        {
            self.connection_status = ConnectionTestStatus::Error(error);
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
                self.fail_pending_connection_test(error.to_string());
                return;
            }
        };
        self.host_generation = self.host_generation.wrapping_add(1);
        let generation = self.host_generation;
        let failure = match self.host_worker.as_mut() {
            Some(worker) => match worker.request_trust(generation, target, key) {
                Ok(()) => {
                    self.host_status = HostIdentityStatus::Trusting;
                    None
                }
                Err(error) => {
                    self.host_status = HostIdentityStatus::Error(error.clone());
                    Some(error)
                }
            },
            None => {
                let error = "host-key worker is unavailable".to_owned();
                self.host_status = HostIdentityStatus::Error(error.clone());
                Some(error)
            }
        };
        if let Some(error) = failure {
            self.fail_pending_connection_test(error);
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
                let trusted_key = match result {
                    Ok(assessment) => {
                        let key = assessment.server_key().clone();
                        if !config.expected_host_key.is_empty() {
                            if config.expected_host_key == key.openssh() {
                                config.expected_host_key = key.openssh().to_owned();
                                self.host_status = trusted_status(&key);
                                Some(key)
                            } else {
                                self.host_status = HostIdentityStatus::Changed {
                                    algorithm: key.algorithm().to_owned(),
                                    fingerprint: key.fingerprint().to_owned(),
                                };
                                None
                            }
                        } else {
                            match assessment {
                                HostKeyAssessment::Trusted(key) => {
                                    config.expected_host_key = key.openssh().to_owned();
                                    self.host_status = trusted_status(&key);
                                    Some(key)
                                }
                                HostKeyAssessment::Unknown(key) => {
                                    self.host_status = HostIdentityStatus::Unknown {
                                        key,
                                        verified_out_of_band: false,
                                    };
                                    None
                                }
                                HostKeyAssessment::Changed(key) => {
                                    self.host_status = HostIdentityStatus::Changed {
                                        algorithm: key.algorithm().to_owned(),
                                        fingerprint: key.fingerprint().to_owned(),
                                    };
                                    None
                                }
                            }
                        }
                    }
                    Err(error) => {
                        self.host_status = HostIdentityStatus::Error(error.clone());
                        self.fail_pending_connection_test(error);
                        return;
                    }
                };
                if let Some(key) = trusted_key {
                    self.submit_pending_connection_test(&key);
                } else if matches!(self.host_status, HostIdentityStatus::Changed { .. }) {
                    self.fail_pending_connection_test(
                        "服务器密钥已变化，连接测试已阻止。".to_owned(),
                    );
                }
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
                match result {
                    Ok(_) => {
                        config.expected_host_key = key.openssh().to_owned();
                        self.host_status = trusted_status(&key);
                        self.submit_pending_connection_test(&key);
                    }
                    Err(error) => {
                        self.host_status = HostIdentityStatus::Error(error.clone());
                        self.fail_pending_connection_test(error);
                    }
                }
            }
        }
    }

    fn submit_pending_connection_test(&mut self, key: &ServerHostKey) {
        let Some(mut request) = self.pending_connection_test.take() else {
            return;
        };
        // 扫描完成后再将已核对的 pin 写入测试副本，绝不以空 pin 登录。
        request.config.expected_host_key = key.openssh().to_owned();
        match self.host_worker.as_mut() {
            Some(worker) => match worker.request_connection_test(request) {
                Ok(()) => self.connection_status = ConnectionTestStatus::Testing,
                Err(error) => self.connection_status = ConnectionTestStatus::Error(error),
            },
            None => {
                self.connection_status =
                    ConnectionTestStatus::Error("host-key worker is unavailable".to_owned())
            }
        }
    }

    fn apply_connection_test_result(
        &mut self,
        config: &SshManagedConfig,
        result: SshConnectionTestResult,
    ) {
        if result.generation != self.connection_generation
            || !target_matches(&result.endpoint, config)
        {
            return;
        }
        self.connection_status = match result.result {
            Ok(stat) => ConnectionTestStatus::Success {
                path: stat.path,
                size: stat.size,
                modified_seconds: stat.modified_seconds,
            },
            Err(error) => ConnectionTestStatus::Error(error),
        };
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

fn text_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.text_edit_singleline(value);
    ui.end_row();
}

#[cfg(test)]
#[path = "ssh_profile_tests.rs"]
mod tests;
