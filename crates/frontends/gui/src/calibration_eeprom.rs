//! 标定 EEPROM 的 GUI 安全状态机；不接受 helper 路径、I²C bus 或任意命令。

use camera_toolbox_app::{
    EEPROM_REMOTE_PROVISION_DISABLED_REASON, EepromDeviceState, EepromDryRunResult,
    EepromHelperAction, EepromInspectResult, EepromPageWritePlan, EepromSerialState,
    EepromWriteResult,
};
use camera_toolbox_core::{
    CalibrationSolution, EepromProvisionRequest, EepromProvisioningMode, FullEepromImage,
};
use eframe::egui;

#[derive(Clone, Debug)]
pub(crate) enum CalibrationProvisionIntent {
    Cancel,
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
        if ui
            .add_enabled(
                false,
                egui::Button::new("Write EEPROM...").fill(egui::Color32::DARK_RED),
            )
            .on_disabled_hover_text(EEPROM_REMOTE_PROVISION_DISABLED_REASON)
            .clicked()
            && prepared_current
        {
            self.confirmation_open = true;
        }
        ui.colored_label(
            egui::Color32::YELLOW,
            EEPROM_REMOTE_PROVISION_DISABLED_REASON,
        );

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
                ui.weak("Provisioning cannot be cancelled after physical writes begin.");
            }
            None => {}
        }
        self.render_confirmation(context, target_label);
    }

    fn render_confirmation(&mut self, context: &egui::Context, target_label: Option<&str>) {
        if !self.confirmation_open {
            return;
        }
        let mut open = true;
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
                if let Some(prepared) = &self.prepared {
                    ui.label(format!("Mode: {:?}", prepared.request.mode));
                    ui.label(format!("Serial: {}", prepared.request.serial_number));
                    ui.label(format!(
                        "Expected before: {}",
                        prepared.result.state.image_sha256
                    ));
                    ui.label(format!("Backup: {}", prepared.backup_file));
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Write and verify"))
                        .clicked()
                    {
                        self.pending = Some(CalibrationProvisionIntent::Provision {
                            request: prepared.request.clone(),
                            expected_before_sha256: prepared.result.state.image_sha256.clone(),
                            dry_run_token: prepared.result.dry_run_token.clone(),
                        });
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
