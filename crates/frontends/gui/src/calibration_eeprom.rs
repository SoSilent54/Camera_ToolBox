//! 标定 EEPROM 的 GUI 安全状态机；不接受 helper 路径、I²C bus 或任意命令。

use camera_toolbox_app::{
    EEPROM_EXPERIMENTAL_PROVISION_WARNING, EepromDeviceState, EepromDryRunResult,
    EepromHelperAction, EepromInspectResult, EepromPageWritePlan, EepromSerialState,
    EepromWriteResult,
};
use camera_toolbox_core::{
    CalibrationSolution, EepromProvisionRequest, EepromProvisioningMode, FullEepromImage,
};
use eframe::egui;
#[cfg(feature = "platform-ssh")]
use eframe::egui::TextBuffer;
#[cfg(feature = "platform-ssh")]
use secrecy::{ExposeSecret, ExposeSecretMut, SecretBox, SecretString};

#[cfg(feature = "platform-ssh")]
/// Calibration 工作区独占的 EEPROM SSH target；不暴露旧 Platform/Profile/Sensor 概念。
pub(crate) struct CalibrationEepromTargetRequest {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) username: String,
    pub(crate) expected_host_key: String,
    pub(crate) password: SecretString,
    pub(crate) helper_program: String,
    pub(crate) i2c_bus: u16,
}

#[cfg(feature = "platform-ssh")]
#[derive(Default)]
struct EepromSecretInput {
    value: SecretBox<String>,
}

#[cfg(feature = "platform-ssh")]
impl EepromSecretInput {
    fn is_empty(&self) -> bool {
        self.value.expose_secret().is_empty()
    }

    fn take(&mut self) -> Option<SecretString> {
        let value = std::mem::take(self.value.expose_secret_mut());
        (!value.is_empty()).then(|| SecretString::from(value))
    }
}

#[cfg(feature = "platform-ssh")]
impl TextBuffer for EepromSecretInput {
    fn is_mutable(&self) -> bool {
        true
    }

    fn as_str(&self) -> &str {
        self.value.expose_secret()
    }

    fn insert_text(&mut self, text: &str, char_index: egui::text::CharIndex) -> usize {
        self.value.expose_secret_mut().insert_text(text, char_index)
    }

    fn delete_char_range(&mut self, range: std::ops::Range<egui::text::CharIndex>) {
        self.value.expose_secret_mut().delete_char_range(range);
    }

    fn type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<Self>()
    }
}

pub(crate) enum CalibrationProvisionIntent {
    Cancel,
    #[cfg(feature = "platform-ssh")]
    ConfigureTarget(CalibrationEepromTargetRequest),
    Inspect,
    DryRun {
        request: EepromProvisionRequest,
    },
    Provision {
        request: EepromProvisionRequest,
        expected_before_sha256: String,
        dry_run_token: String,
        experimental_disconnect_risk_acknowledged: bool,
    },
}

impl CalibrationProvisionIntent {
    pub(crate) fn helper_action(&self) -> Option<EepromHelperAction> {
        match self {
            Self::Cancel => None,
            #[cfg(feature = "platform-ssh")]
            Self::ConfigureTarget(_) => None,
            Self::Inspect => Some(EepromHelperAction::Inspect),
            Self::DryRun { request } => Some(EepromHelperAction::DryRun {
                request: request.clone(),
            }),
            Self::Provision {
                request,
                expected_before_sha256,
                dry_run_token,
                experimental_disconnect_risk_acknowledged: _,
            } => Some(EepromHelperAction::Provision {
                request: request.clone(),
                expected_before_sha256: expected_before_sha256.clone(),
                dry_run_token: dry_run_token.clone(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveEepromOperation {
    ReadOnly,
    Provision,
}

#[derive(Clone, Debug)]
struct PreparedProvision {
    request: EepromProvisionRequest,
    result: EepromDryRunResult,
    target_label: String,
    backup_file: String,
    manifest_file: String,
}

pub(crate) struct CalibrationEepromState {
    mode: EepromProvisioningMode,
    device: Option<EepromDeviceState>,
    #[cfg(feature = "platform-ssh")]
    target_host: String,
    #[cfg(feature = "platform-ssh")]
    target_port: u16,
    #[cfg(feature = "platform-ssh")]
    target_username: String,
    #[cfg(feature = "platform-ssh")]
    target_host_key: String,
    #[cfg(feature = "platform-ssh")]
    target_password: EepromSecretInput,
    #[cfg(feature = "platform-ssh")]
    target_helper_program: String,
    #[cfg(feature = "platform-ssh")]
    target_i2c_bus: u16,
    experimental_risk_acknowledged: bool,
    inspected_target: Option<String>,
    prepared: Option<PreparedProvision>,
    overwrite_existing_serial: bool,
    confirmation_open: bool,
    busy: bool,
    active_operation: Option<ActiveEepromOperation>,
    cancel_requested: bool,
    pending: Option<CalibrationProvisionIntent>,
    status: String,
}

impl Default for CalibrationEepromState {
    fn default() -> Self {
        Self {
            mode: EepromProvisioningMode::FullProvision,
            device: None,
            #[cfg(feature = "platform-ssh")]
            target_host: String::new(),
            #[cfg(feature = "platform-ssh")]
            target_port: 22,
            #[cfg(feature = "platform-ssh")]
            target_username: String::new(),
            #[cfg(feature = "platform-ssh")]
            target_host_key: String::new(),
            #[cfg(feature = "platform-ssh")]
            target_password: EepromSecretInput::default(),
            #[cfg(feature = "platform-ssh")]
            target_helper_program: "/usr/local/libexec/camera-toolbox-eeprom-helper".to_owned(),
            #[cfg(feature = "platform-ssh")]
            target_i2c_bus: 0,
            experimental_risk_acknowledged: false,
            inspected_target: None,
            prepared: None,
            overwrite_existing_serial: false,
            confirmation_open: false,
            busy: false,
            active_operation: None,
            cancel_requested: false,
            pending: None,
            status: "Inspect the selected EEPROM before preparing a write.".to_owned(),
        }
    }
}

impl CalibrationEepromState {
    pub(crate) fn take_intent(&mut self) -> Option<CalibrationProvisionIntent> {
        self.pending.take()
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_target_configured(&mut self, label: &str) {
        self.device = None;
        self.inspected_target = None;
        self.prepared = None;
        self.overwrite_existing_serial = false;
        self.experimental_risk_acknowledged = false;
        self.status = format!("EEPROM SSH target configured: {label}. Inspect before writing.");
    }

    pub(crate) fn report_provision_unknown(&mut self, message: impl Into<String>) {
        self.busy = false;
        self.active_operation = None;
        self.cancel_requested = false;
        self.confirmation_open = false;
        self.device = None;
        self.inspected_target = None;
        self.prepared = None;
        self.overwrite_existing_serial = false;
        self.experimental_risk_acknowledged = false;
        self.pending = None;
        self.status = format!(
            "EEPROM state is UNKNOWN after the write attempt. Do not retry. Re-inspect the device and use the saved backup if required. {}",
            message.into()
        );
    }

    pub(crate) fn report_error(&mut self, message: impl Into<String>) {
        self.busy = false;
        self.active_operation = None;
        self.cancel_requested = false;
        self.confirmation_open = false;
        self.status = message.into();
    }

    pub(crate) fn report_inspect(&mut self, target_label: String, result: EepromInspectResult) {
        self.busy = false;
        self.active_operation = None;
        self.cancel_requested = false;
        self.device = Some(result.state);
        self.inspected_target = Some(target_label);
        self.prepared = None;
        self.overwrite_existing_serial = false;
        self.status = "EEPROM inspection complete. No bytes were written.".to_owned();
    }

    pub(crate) fn report_dry_run(
        &mut self,
        target_label: String,
        request: EepromProvisionRequest,
        result: EepromDryRunResult,
        backup_file: String,
        manifest_file: String,
    ) {
        self.busy = false;
        self.active_operation = None;
        self.cancel_requested = false;
        self.device = Some(result.state.clone());
        self.inspected_target = Some(target_label.clone());
        self.prepared = Some(PreparedProvision {
            request,
            result,
            target_label,
            backup_file,
            manifest_file,
        });
        self.experimental_risk_acknowledged = false;
        self.status = "Dry-run accepted; backup and manifest were saved create-new.".to_owned();
    }

    pub(crate) fn report_provision(
        &mut self,
        target_label: String,
        result: &EepromWriteResult,
        audit_file: String,
    ) {
        self.busy = false;
        self.device = Some(result.after.clone());
        self.inspected_target = Some(target_label);
        self.prepared = None;
        self.active_operation = None;
        self.cancel_requested = false;
        self.confirmation_open = false;
        self.overwrite_existing_serial = false;
        self.experimental_risk_acknowledged = false;
        self.status = format!(
            "EEPROM write and bytewise verification succeeded; audit saved as {audit_file}."
        );
    }

    pub(crate) fn report_provision_audit_error(
        &mut self,
        target_label: String,
        result: &EepromWriteResult,
        error: &str,
    ) {
        self.busy = false;
        self.device = Some(result.after.clone());
        self.inspected_target = Some(target_label);
        self.prepared = None;
        self.active_operation = None;
        self.cancel_requested = false;
        self.confirmation_open = false;
        self.overwrite_existing_serial = false;
        self.experimental_risk_acknowledged = false;
        self.status = format!(
            "EEPROM write and bytewise verification succeeded, but final audit save failed: {error}. The pre-write dry-run manifest remains authoritative."
        );
    }

    pub(crate) fn render(
        &mut self,
        context: &egui::Context,
        ui: &mut egui::Ui,
        solution: Option<&CalibrationSolution>,
        serial_number: &str,
        target: Result<&str, &str>,
        save_destination_error: Option<&str>,
    ) {
        ui.separator();
        ui.strong("EEPROM Provisioning");
        #[cfg(feature = "platform-ssh")]
        self.render_target_editor(ui);
        let target_label = target.ok();
        match target {
            Ok(label) => ui.label(format!("Resolved target: {label}")),
            Err(reason) => ui.colored_label(egui::Color32::YELLOW, reason),
        };
        if self
            .inspected_target
            .as_deref()
            .is_some_and(|inspected| Some(inspected) != target_label)
        {
            self.device = None;
            self.inspected_target = None;
            self.prepared = None;
            self.overwrite_existing_serial = false;
            self.experimental_risk_acknowledged = false;
            self.status = "Target changed; inspect the newly selected EEPROM.".to_owned();
        }

        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.busy && target_label.is_some(),
                    egui::Button::new("Inspect EEPROM"),
                )
                .clicked()
            {
                self.busy = true;
                self.active_operation = Some(ActiveEepromOperation::ReadOnly);
                self.cancel_requested = false;
                self.prepared = None;
                self.pending = Some(CalibrationProvisionIntent::Inspect);
                self.status = "Inspecting EEPROM...".to_owned();
            }
            ui.add_enabled_ui(!self.busy, |ui| {
                ui.radio_value(
                    &mut self.mode,
                    EepromProvisioningMode::FullProvision,
                    "Full provision",
                );
                ui.radio_value(
                    &mut self.mode,
                    EepromProvisioningMode::UpdateCalibration,
                    "Update calibration only",
                );
            });
        });

        if let Some(device) = &self.device {
            render_device_state(ui, device);
        }

        let image = solution
            .and_then(|solution| FullEepromImage::from_solution(solution, serial_number).ok());
        let image_error = match solution {
            None => Some("Calibrate successfully before EEPROM writes.".to_owned()),
            Some(solution) => FullEepromImage::from_solution(solution, serial_number)
                .err()
                .map(|error| error.to_string()),
        };
        if let Some(error) = &image_error {
            ui.colored_label(egui::Color32::YELLOW, error);
        }

        let requires_override = self
            .device
            .as_ref()
            .is_some_and(|device| serial_override_required(device, serial_number));
        if self.mode == EepromProvisioningMode::FullProvision && requires_override {
            ui.checkbox(
                &mut self.overwrite_existing_serial,
                "I confirm replacing the existing different or damaged serial number",
            );
        } else {
            self.overwrite_existing_serial = false;
        }

        let mut request = image.as_ref().and_then(|image| match self.mode {
            EepromProvisioningMode::FullProvision => {
                Some(image.full_provision_request(self.overwrite_existing_serial))
            }
            EepromProvisioningMode::UpdateCalibration => Some(image.update_calibration_request()),
        });
        let backup_ready = save_destination_error.is_none();
        if let Some(reason) = save_destination_error {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!("Backup destination unavailable: {reason}"),
            );
        }
        let inspected_current =
            self.inspected_target.as_deref() == target_label && self.device.is_some();
        let dry_run_enabled = !self.busy
            && target_label.is_some()
            && inspected_current
            && request.is_some()
            && backup_ready
            && (!requires_override || self.overwrite_existing_serial);
        let prepared_current = self.prepared.as_ref().is_some_and(|prepared| {
            Some(prepared.target_label.as_str()) == target_label
                && request.as_ref() == Some(&prepared.request)
        });
        if ui
            .add_enabled(dry_run_enabled, egui::Button::new("Dry Run + Save Backup"))
            .clicked()
        {
            self.busy = true;
            self.active_operation = Some(ActiveEepromOperation::ReadOnly);
            self.cancel_requested = false;
            self.prepared = None;
            self.pending = Some(CalibrationProvisionIntent::DryRun {
                request: request.take().expect("enabled only with a valid request"),
            });
            self.status = "Running same-state dry-run and persisting backup...".to_owned();
        }

        if let Some(prepared) = &self.prepared {
            ui.label(format!("Backup: {}", prepared.backup_file));
            ui.label(format!("Dry-run manifest: {}", prepared.manifest_file));
            ui.label(format!(
                "Token: {}...",
                &prepared.result.dry_run_token[..prepared.result.dry_run_token.len().min(12)]
            ));
            render_page_plan(ui, &prepared.result.page_plan);
        }
        ui.colored_label(egui::Color32::YELLOW, EEPROM_EXPERIMENTAL_PROVISION_WARNING);
        ui.checkbox(
            &mut self.experimental_risk_acknowledged,
            "I understand that SSH interruption can leave the EEPROM state unknown",
        );
        if ui
            .add_enabled(
                !self.busy && prepared_current && self.experimental_risk_acknowledged,
                egui::Button::new("Experimental Write EEPROM...").fill(egui::Color32::DARK_RED),
            )
            .clicked()
        {
            self.confirmation_open = true;
        }

        ui.label(&self.status);
        if self.busy {
            ui.spinner();
        }
        match self.active_operation {
            Some(ActiveEepromOperation::ReadOnly) => {
                if ui
                    .add_enabled(
                        !self.cancel_requested,
                        egui::Button::new("Cancel read-only operation"),
                    )
                    .clicked()
                {
                    self.pending = Some(CalibrationProvisionIntent::Cancel);
                    self.cancel_requested = true;
                    self.status = "Cancellation requested...".to_owned();
                }
            }
            Some(ActiveEepromOperation::Provision) => {
                ui.weak("Experimental write is running. Do not disconnect SSH or start another EEPROM operation.");
            }
            None => {}
        }
        self.render_confirmation(context, target_label);
    }

    #[cfg(feature = "platform-ssh")]
    fn render_target_editor(&mut self, ui: &mut egui::Ui) {
        ui.collapsing("EEPROM SSH Target", |ui| {
            ui.weak("Calibration-only target. It does not create a Platform/Profile/Sensor entry.");
            egui::Grid::new("calibration_eeprom_ssh_target")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Host / IP");
                    ui.text_edit_singleline(&mut self.target_host);
                    ui.end_row();
                    ui.label("SSH port");
                    ui.add(egui::DragValue::new(&mut self.target_port).range(1..=u16::MAX));
                    ui.end_row();
                    ui.label("Username");
                    ui.text_edit_singleline(&mut self.target_username);
                    ui.end_row();
                    ui.label("Pinned host key");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.target_host_key)
                            .hint_text("ssh-ed25519 AAAA..."),
                    );
                    ui.end_row();
                    ui.label("Password");
                    ui.add(egui::TextEdit::singleline(&mut self.target_password).password(true));
                    ui.end_row();
                    ui.label("Helper program");
                    ui.text_edit_singleline(&mut self.target_helper_program);
                    ui.end_row();
                    ui.label("I²C bus");
                    ui.add(egui::DragValue::new(&mut self.target_i2c_bus));
                    ui.end_row();
                });
            ui.weak(
                "The server host key is mandatory and the password remains in process memory only.",
            );
            let complete = !self.busy
                && !self.target_host.trim().is_empty()
                && !self.target_username.trim().is_empty()
                && !self.target_host_key.trim().is_empty()
                && !self.target_helper_program.trim().is_empty()
                && !self.target_password.is_empty();
            if ui
                .add_enabled(complete, egui::Button::new("Apply EEPROM target"))
                .clicked()
            {
                let password = self
                    .target_password
                    .take()
                    .expect("enabled only while password is present");
                self.pending = Some(CalibrationProvisionIntent::ConfigureTarget(
                    CalibrationEepromTargetRequest {
                        host: self.target_host.trim().to_owned(),
                        port: self.target_port,
                        username: self.target_username.trim().to_owned(),
                        expected_host_key: self.target_host_key.trim().to_owned(),
                        password,
                        helper_program: self.target_helper_program.trim().to_owned(),
                        i2c_bus: self.target_i2c_bus,
                    },
                ));
                self.status = "Validating the EEPROM SSH target...".to_owned();
            }
        });
    }

    fn confirmed_provision_intent(&self) -> Option<CalibrationProvisionIntent> {
        if self.busy || !self.experimental_risk_acknowledged {
            return None;
        }
        let prepared = self.prepared.as_ref()?;
        Some(CalibrationProvisionIntent::Provision {
            request: prepared.request.clone(),
            expected_before_sha256: prepared.result.state.image_sha256.clone(),
            dry_run_token: prepared.result.dry_run_token.clone(),
            experimental_disconnect_risk_acknowledged: self.experimental_risk_acknowledged,
        })
    }

    fn render_confirmation(&mut self, context: &egui::Context, target_label: Option<&str>) {
        if !self.confirmation_open {
            return;
        }
        let mut open = true;
        let mut confirmed_intent = self.confirmed_provision_intent();
        egui::Window::new("Confirm EEPROM write")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(context, |ui| {
                ui.colored_label(
                    egui::Color32::RED,
                    "This modifies the physical module. The saved backup is required for recovery.",
                );
                if let Some(label) = target_label {
                    ui.label(format!("Target: {label}"));
                }
                ui.colored_label(egui::Color32::YELLOW, EEPROM_EXPERIMENTAL_PROVISION_WARNING);
                if let Some(prepared) = &self.prepared {
                    ui.label(format!("Mode: {:?}", prepared.request.mode));
                    ui.label(format!("Serial: {}", prepared.request.serial_number));
                    ui.label(format!(
                        "Expected before: {}",
                        prepared.result.state.image_sha256
                    ));
                    ui.label(format!("Backup: {}", prepared.backup_file));
                    if ui
                        .add_enabled(
                            confirmed_intent.is_some(),
                            egui::Button::new("Write and verify"),
                        )
                        .clicked()
                    {
                        self.pending = confirmed_intent.take();
                        self.busy = true;
                        self.active_operation = Some(ActiveEepromOperation::Provision);
                        self.cancel_requested = false;
                        self.confirmation_open = false;
                        self.status = "Writing, reading back, and verifying EEPROM...".to_owned();
                    }
                }
            });
        self.confirmation_open &= open;
    }
}

fn serial_override_required(device: &EepromDeviceState, desired: &str) -> bool {
    match &device.serial {
        EepromSerialState::Empty => false,
        EepromSerialState::Valid { value } => value != desired,
        EepromSerialState::Invalid { .. } => true,
    }
}

fn render_device_state(ui: &mut egui::Ui, device: &EepromDeviceState) {
    let serial = match &device.serial {
        EepromSerialState::Empty => "empty".to_owned(),
        EepromSerialState::Valid { value } => value.clone(),
        EepromSerialState::Invalid { raw_hex, checksum } => {
            format!("INVALID raw={raw_hex}, checksum=0x{checksum:02x}")
        }
    };
    ui.label(format!(
        "Device: FLAG={}, SN={serial}",
        if device.flag_valid {
            "valid"
        } else {
            "invalid"
        }
    ));
    ui.monospace(format!("SHA-256: {}", device.image_sha256));
}

fn render_page_plan(ui: &mut egui::Ui, page_plan: &[EepromPageWritePlan]) {
    egui::Grid::new("eeprom_page_plan")
        .striped(true)
        .show(ui, |ui| {
            ui.strong("Stage");
            ui.strong("Offset");
            ui.strong("Bytes");
            ui.end_row();
            for page in page_plan {
                ui.label(&page.stage);
                ui.monospace(format!("0x{:04x}", page.offset));
                ui.label(page.byte_len.to_string());
                ui.end_row();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_app::EepromSerialState;
    use camera_toolbox_core::YG_STEREO_P24C64G_V1_MAP_ID;

    fn device_state() -> EepromDeviceState {
        EepromDeviceState {
            image_sha256: "a".repeat(64),
            flag_valid: true,
            serial: EepromSerialState::Valid {
                value: "2T02D2567K0042".to_owned(),
            },
        }
    }

    fn request() -> EepromProvisionRequest {
        EepromProvisionRequest {
            map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
            mode: EepromProvisioningMode::UpdateCalibration,
            serial_number: "2T02D2567K0042".to_owned(),
            overwrite_existing_serial: false,
            segments: Vec::new(),
        }
    }

    #[test]
    fn provision_transport_unknown_forces_fresh_inspection_before_retry() {
        let mut state = CalibrationEepromState::default();
        state.device = Some(device_state());
        state.inspected_target = Some("root@camera / i2c-7".to_owned());
        state.prepared = Some(PreparedProvision {
            request: request(),
            result: EepromDryRunResult {
                state: device_state(),
                backup: vec![0; 308],
                page_plan: Vec::new(),
                dry_run_token: "b".repeat(64),
            },
            target_label: "root@camera / i2c-7".to_owned(),
            backup_file: "backup.bin".to_owned(),
            manifest_file: "manifest.json".to_owned(),
        });
        state.pending = Some(CalibrationProvisionIntent::Provision {
            request: request(),
            expected_before_sha256: "a".repeat(64),
            dry_run_token: "b".repeat(64),
            experimental_disconnect_risk_acknowledged: true,
        });
        state.busy = true;
        state.active_operation = Some(ActiveEepromOperation::Provision);
        state.experimental_risk_acknowledged = true;

        state.report_provision_unknown("SSH response was lost");

        assert!(state.device.is_none());
        assert!(state.inspected_target.is_none());
        assert!(state.prepared.is_none());
        assert!(state.pending.is_none());
        assert!(!state.busy);
        assert!(!state.experimental_risk_acknowledged);
        assert!(state.status.contains("UNKNOWN"));
        assert!(state.status.contains("Re-inspect"));
    }

    #[test]
    fn final_write_intent_requires_current_disconnect_risk_acknowledgement() {
        let mut state = CalibrationEepromState::default();
        state.prepared = Some(PreparedProvision {
            request: request(),
            result: EepromDryRunResult {
                state: device_state(),
                backup: vec![0; 308],
                page_plan: Vec::new(),
                dry_run_token: "b".repeat(64),
            },
            target_label: "root@camera / i2c-7".to_owned(),
            backup_file: "backup.bin".to_owned(),
            manifest_file: "manifest.json".to_owned(),
        });

        assert!(state.confirmed_provision_intent().is_none());
        state.experimental_risk_acknowledged = true;
        assert!(matches!(
            state.confirmed_provision_intent(),
            Some(CalibrationProvisionIntent::Provision {
                experimental_disconnect_risk_acknowledged: true,
                ..
            })
        ));
        state.busy = true;
        assert!(state.confirmed_provision_intent().is_none());
    }
}
