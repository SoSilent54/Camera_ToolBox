//! 标定 EEPROM 的 GUI 安全状态机；SSH 登录复用 Explorer SFTP，仅选择 I²C bus。

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
/// 复用当前 Explorer SFTP 连接，仅选择目标物理 I²C bus。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CalibrationEepromTargetRequest {
    pub(crate) i2c_bus: u16,
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
    target_i2c_bus: u16,
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
            target_i2c_bus: 0,
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
        self.clear_target_dependent_state();
        self.status = format!("EEPROM SSH target configured: {label}. Inspect before writing.");
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_target_configuration_failed(&mut self, message: impl Into<String>) {
        self.clear_target_dependent_state();
        self.status = format!("EEPROM SSH target configuration failed: {}", message.into());
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_target_invalidated(&mut self, message: impl Into<String>) {
        self.clear_target_dependent_state();
        self.status = message.into();
    }

    #[cfg(feature = "platform-ssh")]
    fn begin_target_configuration(&mut self) {
        self.clear_target_dependent_state();
        self.pending = Some(CalibrationProvisionIntent::ConfigureTarget(
            CalibrationEepromTargetRequest {
                i2c_bus: self.target_i2c_bus,
            },
        ));
        self.status = "Configuring EEPROM from the active Explorer SFTP connection...".to_owned();
    }

    fn clear_target_dependent_state(&mut self) {
        self.busy = false;
        self.active_operation = None;
        self.cancel_requested = false;
        self.confirmation_open = false;
        self.device = None;
        self.inspected_target = None;
        self.prepared = None;
        self.overwrite_existing_serial = false;
        self.pending = None;
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
        self.status = format!(
            "EEPROM write and bytewise verification succeeded, but final audit save failed: {error}. The pre-write dry-run manifest remains authoritative."
        );
    }

    pub(crate) fn render_body(
        &mut self,
        context: &egui::Context,
        ui: &mut egui::Ui,
        solution: Option<&CalibrationSolution>,
        serial_number: &str,
        sftp_source: Result<&str, &str>,
        target: Result<&str, &str>,
        save_destination_error: Option<&str>,
    ) {
        #[cfg(not(feature = "platform-ssh"))]
        let _ = sftp_source;
        #[cfg(feature = "platform-ssh")]
        self.render_target_editor(ui, sftp_source);
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
        if ui
            .add_enabled(
                !self.busy && prepared_current,
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
    }

    /// 确认写入弹窗必须独立于折叠内容持续渲染，避免已打开的物理写入确认被隐藏。
    pub(crate) fn render_confirmation(
        &mut self,
        context: &egui::Context,
        target: Result<&str, &str>,
    ) {
        self.render_confirmation_modal(context, target.ok());
    }

    #[cfg(feature = "platform-ssh")]
    fn render_target_editor(&mut self, ui: &mut egui::Ui, sftp_source: Result<&str, &str>) {
        ui.collapsing("EEPROM SSH Target", |ui| {
            ui.weak("Reuses the active Explorer SFTP endpoint and process-only password.");
            match sftp_source {
                Ok(label) => {
                    ui.label(format!("Explorer SFTP: {label}"));
                }
                Err(reason) => {
                    ui.colored_label(egui::Color32::YELLOW, reason);
                }
            }
            ui.horizontal(|ui| {
                ui.label("I²C bus");
                ui.add(egui::DragValue::new(&mut self.target_i2c_bus));
            });
            ui.weak(
                "Before each EEPROM operation, Camera Toolbox uploads the bundled companion helper to /usr/local/libexec/camera-toolbox-eeprom-helper, runs chmod 755, then reuses the Explorer SFTP password. SSH host keys are not saved or verified.",
            );
            if ui
                .add_enabled(
                    !self.busy && sftp_source.is_ok(),
                    egui::Button::new("Use Explorer SFTP for EEPROM"),
                )
                .clicked()
            {
                self.begin_target_configuration();
            }
        });
    }

    fn confirmed_provision_intent(&self) -> Option<CalibrationProvisionIntent> {
        if self.busy {
            return None;
        }
        let prepared = self.prepared.as_ref()?;
        Some(CalibrationProvisionIntent::Provision {
            request: prepared.request.clone(),
            expected_before_sha256: prepared.result.state.image_sha256.clone(),
            dry_run_token: prepared.result.dry_run_token.clone(),
        })
    }

    fn render_confirmation_modal(&mut self, context: &egui::Context, target_label: Option<&str>) {
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
        });
        state.busy = true;
        state.active_operation = Some(ActiveEepromOperation::Provision);

        state.report_provision_unknown("SSH response was lost");

        assert!(state.device.is_none());
        assert!(state.inspected_target.is_none());
        assert!(state.prepared.is_none());
        assert!(state.pending.is_none());
        assert!(!state.busy);
        assert!(state.status.contains("UNKNOWN"));
        assert!(state.status.contains("Re-inspect"));
    }

    #[test]
    fn final_write_intent_requires_prepared_idle_state() {
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

        assert!(matches!(
            state.confirmed_provision_intent(),
            Some(CalibrationProvisionIntent::Provision { .. })
        ));
        state.busy = true;
        assert!(state.confirmed_provision_intent().is_none());
    }

    #[test]
    fn failed_reconfiguration_invalidates_existing_prepared_provision() {
        let mut state = CalibrationEepromState::default();
        state.prepared = Some(PreparedProvision {
            request: request(),
            result: EepromDryRunResult {
                state: device_state(),
                backup: vec![0; 308],
                page_plan: Vec::new(),
                dry_run_token: "b".repeat(64),
            },
            target_label: "root@old-camera / i2c-7".to_owned(),
            backup_file: "backup.bin".to_owned(),
            manifest_file: "manifest.json".to_owned(),
        });
        state.device = Some(device_state());
        state.inspected_target = Some("root@old-camera / i2c-7".to_owned());
        assert!(state.confirmed_provision_intent().is_some());

        state.begin_target_configuration();
        assert!(matches!(
            state.take_intent(),
            Some(CalibrationProvisionIntent::ConfigureTarget(_))
        ));
        state.report_target_configuration_failed("no active SFTP source");

        assert!(state.device.is_none());
        assert!(state.inspected_target.is_none());
        assert!(state.prepared.is_none());
        assert!(state.confirmed_provision_intent().is_none());
        assert!(state.take_intent().is_none());
    }
}
