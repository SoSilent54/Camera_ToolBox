//! 标定 EEPROM 的 GUI 安全状态机；SSH 登录复用可用的 SSH/SFTP 控制源，仅选择 I²C bus。

use camera_toolbox_app::{
    EEPROM_EXPERIMENTAL_PROVISION_WARNING, EepromDeviceState, EepromHelperAction,
    EepromInspectResult, EepromSerialState, EepromWriteResult, I2cBusInfo,
};
use camera_toolbox_core::{
    CalibrationSolution, EepromProvisionRequest, EepromProvisioningMode, FullEepromImage,
    StorageEncoding, StorageField, yg_stereo_p24c64g_v1,
};
use eframe::egui;

#[cfg(feature = "platform-ssh")]
/// 复用当前 SSH/SFTP 控制源，仅选择目标物理 I²C bus。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CalibrationEepromTargetRequest {
    pub(crate) i2c_bus: u16,
}

pub(crate) enum CalibrationProvisionIntent {
    Cancel,
    #[cfg(feature = "platform-ssh")]
    ConfigureTarget(CalibrationEepromTargetRequest),
    #[cfg(feature = "platform-ssh")]
    DiscoverBuses,
    Inspect {
        expected_target_label: String,
    },
    Provision {
        expected_target_label: String,
        request: EepromProvisionRequest,
        expected_before_sha256: String,
    },
}

impl CalibrationProvisionIntent {
    pub(crate) fn helper_action(&self) -> Option<EepromHelperAction> {
        match self {
            Self::Cancel => None,
            #[cfg(feature = "platform-ssh")]
            Self::ConfigureTarget(_) => None,
            #[cfg(feature = "platform-ssh")]
            Self::DiscoverBuses => None,
            Self::Inspect { .. } => Some(EepromHelperAction::Inspect),
            Self::Provision {
                request,
                expected_before_sha256,
                ..
            } => Some(EepromHelperAction::Provision {
                request: request.clone(),
                expected_before_sha256: expected_before_sha256.clone(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveEepromOperation {
    ReadOnly,
    Discovery,
    Provision,
}

pub(crate) struct CalibrationEepromState {
    mode: EepromProvisioningMode,
    device: Option<EepromDeviceState>,
    device_backup: Option<Vec<u8>>,
    #[cfg(feature = "platform-ssh")]
    target_i2c_bus: u16,
    #[cfg(feature = "platform-ssh")]
    discovered_buses: Vec<I2cBusInfo>,
    #[cfg(feature = "platform-ssh")]
    bus_discovery_requested: bool,
    #[cfg(feature = "platform-ssh")]
    bus_discovery_error: Option<String>,
    inspected_target: Option<String>,
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
            device_backup: None,
            #[cfg(feature = "platform-ssh")]
            target_i2c_bus: 0,
            #[cfg(feature = "platform-ssh")]
            discovered_buses: Vec::new(),
            #[cfg(feature = "platform-ssh")]
            bus_discovery_requested: false,
            #[cfg(feature = "platform-ssh")]
            bus_discovery_error: None,
            inspected_target: None,
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
#[cfg(test)]
impl CalibrationEepromState {
    pub(crate) fn set_pending_for_test(&mut self, intent: CalibrationProvisionIntent) {
        self.pending = Some(intent);
    }

    pub(crate) fn inspected_target_for_test(&self) -> Option<&str> {
        self.inspected_target.as_deref()
    }

    pub(crate) fn status_for_test(&self) -> &str {
        &self.status
    }

    pub(crate) fn busy_for_test(&self) -> bool {
        self.busy
    }

    pub(crate) fn device_for_test(&self) -> Option<&EepromDeviceState> {
        self.device.as_ref()
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn discovered_buses_for_test(&self) -> &[I2cBusInfo] {
        &self.discovered_buses
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn target_i2c_bus_for_test(&self) -> u16 {
        self.target_i2c_bus
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn bus_discovery_error_for_test(&self) -> Option<&str> {
        self.bus_discovery_error.as_deref()
    }
}

impl CalibrationEepromState {
    pub(crate) fn take_intent(&mut self) -> Option<CalibrationProvisionIntent> {
        self.pending.take()
    }

    #[cfg(feature = "platform-ssh")]
    fn request_bus_discovery(&mut self) {
        self.bus_discovery_requested = true;
        self.bus_discovery_error = None;
        self.busy = true;
        self.active_operation = Some(ActiveEepromOperation::Discovery);
        self.cancel_requested = false;
        self.pending = Some(CalibrationProvisionIntent::DiscoverBuses);
        self.status = if self.discovered_buses.is_empty() {
            "Discovering I²C buses from the active SSH/SFTP control connection...".to_owned()
        } else {
            "Refreshing the I²C bus list from the active SSH/SFTP control connection...".to_owned()
        };
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_bus_discovery(&mut self, buses: Vec<I2cBusInfo>) {
        self.busy = false;
        self.active_operation = None;
        self.cancel_requested = false;
        self.bus_discovery_error = None;
        let selected_bus_exists = buses
            .iter()
            .any(|bus| u16::try_from(bus.bus).ok() == Some(self.target_i2c_bus));
        if !selected_bus_exists {
            if let Some(first_bus) = buses.iter().find_map(|bus| u16::try_from(bus.bus).ok()) {
                self.target_i2c_bus = first_bus;
            }
        }
        if buses.is_empty() {
            self.status = "No I²C buses were discovered on the target.".to_owned();
        } else {
            self.status = format!(
                "Discovered {} I²C bus(es). Select the EEPROM target bus; changes auto-refresh the binding.",
                buses.len()
            );
        }
        self.discovered_buses = buses;
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_bus_discovery_failed(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.busy = false;
        self.active_operation = None;
        self.cancel_requested = false;
        self.bus_discovery_error = Some(message.clone());
        self.status = format!("I²C bus discovery failed: {message}");
    }

    pub(crate) fn report_target_configured(&mut self, label: &str) {
        self.clear_bound_eeprom_state();
        self.status = format!("EEPROM SSH target configured: {label}. Inspect before writing.");
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_target_configuration_failed(&mut self, message: impl Into<String>) {
        self.clear_bound_eeprom_state();
        self.status = format!("EEPROM SSH target configuration failed: {}", message.into());
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn report_target_invalidated(&mut self, message: impl Into<String>) {
        self.clear_target_dependent_state();
        self.status = message.into();
    }

    #[cfg(feature = "platform-ssh")]
    fn begin_target_configuration(&mut self) {
        self.clear_bound_eeprom_state();
        self.pending = Some(CalibrationProvisionIntent::ConfigureTarget(
            CalibrationEepromTargetRequest {
                i2c_bus: self.target_i2c_bus,
            },
        ));
        self.status = format!(
            "Configuring EEPROM target i2c-{} from the active SSH/SFTP control connection...",
            self.target_i2c_bus
        );
    }

    fn clear_bound_eeprom_state(&mut self) {
        self.busy = false;
        self.active_operation = None;
        self.cancel_requested = false;
        self.confirmation_open = false;
        self.device = None;
        self.device_backup = None;
        self.inspected_target = None;
        self.overwrite_existing_serial = false;
        self.pending = None;
    }

    fn clear_target_dependent_state(&mut self) {
        self.clear_bound_eeprom_state();
        #[cfg(feature = "platform-ssh")]
        {
            self.discovered_buses.clear();
            self.bus_discovery_requested = false;
            self.bus_discovery_error = None;
        }
    }
}

impl CalibrationEepromState {
    pub(crate) fn report_provision_unknown(&mut self, message: impl Into<String>) {
        self.busy = false;
        self.active_operation = None;
        self.cancel_requested = false;
        self.confirmation_open = false;
        self.device = None;
        self.device_backup = None;
        self.inspected_target = None;
        self.overwrite_existing_serial = false;
        self.pending = None;
        self.status = format!(
            "EEPROM state is UNKNOWN after the write attempt. Do not retry. Re-inspect the device before any recovery action. {}",
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
        self.device_backup = Some(result.backup);
        self.inspected_target = Some(target_label);
        self.overwrite_existing_serial = false;
        self.status = "EEPROM read completed. Review the current state before writing.".to_owned();
    }

    pub(crate) fn report_provision(
        &mut self,
        target_label: String,
        result: &EepromWriteResult,
        audit_file: String,
    ) {
        self.busy = false;
        self.device = Some(result.after.clone());
        self.device_backup = None;
        self.inspected_target = Some(target_label);
        self.active_operation = None;
        self.cancel_requested = false;
        self.confirmation_open = false;
        self.overwrite_existing_serial = false;
        self.status = format!(
            "EEPROM write and bytewise verification succeeded; write history saved as {audit_file}."
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
        self.device_backup = None;
        self.inspected_target = Some(target_label);
        self.active_operation = None;
        self.cancel_requested = false;
        self.confirmation_open = false;
        self.overwrite_existing_serial = false;
        self.status = format!(
            "EEPROM write and bytewise verification succeeded, but final audit save failed: {error}. Re-read the EEPROM before any recovery action."
        );
    }
    pub(crate) fn render_body(
        &mut self,
        _context: &egui::Context,
        ui: &mut egui::Ui,
        solution: Option<&CalibrationSolution>,
        serial_number: &str,
        sftp_source: Result<&str, &str>,
        target: Result<&str, &str>,
        _save_destination_error: Option<&str>,
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
            self.overwrite_existing_serial = false;
            self.status = "Target changed; inspect the newly selected EEPROM.".to_owned();
        }

        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.busy && target_label.is_some(),
                    egui::Button::new("Read"),
                )
                .clicked()
            {
                self.busy = true;
                self.active_operation = Some(ActiveEepromOperation::ReadOnly);
                self.cancel_requested = false;
                self.pending =
                    target_label.map(
                        |expected_target_label| CalibrationProvisionIntent::Inspect {
                            expected_target_label: expected_target_label.to_owned(),
                        },
                    );
                self.status = "Reading EEPROM...".to_owned();
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
            render_device_state(ui, device, self.device_backup.as_deref());
        }

        let image_error = match solution {
            None => Some(
                "Calibrate successfully or load a YAML result before EEPROM writes.".to_owned(),
            ),
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

        let request = self.current_request(solution, serial_number);
        let inspected_current =
            self.inspected_target.as_deref() == target_label && self.device.is_some();
        let write_enabled = !self.busy
            && target_label.is_some()
            && inspected_current
            && request.is_some()
            && (!requires_override || self.overwrite_existing_serial);
        if ui
            .add_enabled(
                write_enabled,
                egui::Button::new("Write...").fill(egui::Color32::DARK_RED),
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
                        egui::Button::new("Cancel read operation"),
                    )
                    .clicked()
                {
                    self.pending = Some(CalibrationProvisionIntent::Cancel);
                    self.cancel_requested = true;
                    self.status = "Cancellation requested...".to_owned();
                }
            }
            Some(ActiveEepromOperation::Discovery) => {
                if ui
                    .add_enabled(
                        !self.cancel_requested,
                        egui::Button::new("Cancel bus discovery"),
                    )
                    .clicked()
                {
                    self.pending = Some(CalibrationProvisionIntent::Cancel);
                    self.cancel_requested = true;
                    self.status = "Bus discovery cancellation requested...".to_owned();
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
        solution: Option<&CalibrationSolution>,
        serial_number: &str,
    ) {
        let request = self.current_request(solution, serial_number);
        self.render_confirmation_modal(context, target.ok(), request.as_ref());
    }

    #[cfg(feature = "platform-ssh")]
    fn render_target_editor(&mut self, ui: &mut egui::Ui, sftp_source: Result<&str, &str>) {
        ui.collapsing("EEPROM SSH Target", |ui| {
            ui.weak("Reuses the active SSH/SFTP endpoint and process-only password.");
            match sftp_source {
                Ok(label) => {
                    ui.label(format!("SSH/SFTP control: {label}"));
                }
                Err(reason) => {
                    ui.colored_label(egui::Color32::YELLOW, reason);
                }
            }
            if !self.busy
                && sftp_source.is_ok()
                && self.discovered_buses.is_empty()
                && !self.bus_discovery_requested
            {
                self.request_bus_discovery();
            }
            if let Some(error) = self.bus_discovery_error.as_deref() {
                ui.colored_label(egui::Color32::YELLOW, error);
            }
            let previous_bus = self.target_i2c_bus;
            ui.horizontal(|ui| {
                ui.label("I²C bus");
                if self.discovered_buses.is_empty() {
                    ui.label("Discovering bus list...");
                } else {
                    let selected_text = self
                        .discovered_buses
                        .iter()
                        .find_map(|bus| {
                            (u16::try_from(bus.bus).ok() == Some(self.target_i2c_bus))
                                .then(|| format_i2c_bus_label(bus))
                        })
                        .unwrap_or_else(|| format!("i2c-{}", self.target_i2c_bus));
                    ui.add_enabled_ui(!self.busy, |ui| {
                        egui::ComboBox::from_id_salt("eeprom_i2c_bus")
                            .selected_text(selected_text)
                            .show_ui(ui, |ui| {
                                for bus in &self.discovered_buses {
                                    if let Ok(bus_id) = u16::try_from(bus.bus) {
                                        ui.selectable_value(
                                            &mut self.target_i2c_bus,
                                            bus_id,
                                            format_i2c_bus_label(bus),
                                        );
                                    }
                                }
                            });
                    });
                }
                if ui
                    .add_enabled(
                        !self.busy && sftp_source.is_ok(),
                        egui::Button::new("Refresh bus list"),
                    )
                    .clicked()
                {
                    self.request_bus_discovery();
                }
            });
            if self.target_i2c_bus != previous_bus && !self.busy && sftp_source.is_ok() {
                self.begin_target_configuration();
            }
            if self.discovered_buses.is_empty() {
                ui.weak(
                    "ListBuses finds reachable /dev/i2c-* nodes; it does not prove the EEPROM chip is present.",
                );
            }
            ui.weak(
                "Before each EEPROM operation, Camera Toolbox uploads the bundled companion helper to /usr/local/libexec/camera-i2c-helper, runs chmod 755, then reuses the selected SSH/SFTP control password. SSH host keys are not saved or verified.",
            );
            if ui
                .add_enabled(
                    !self.busy && sftp_source.is_ok(),
                    egui::Button::new("Use SSH/SFTP control for EEPROM"),
                )
                .clicked()
            {
                self.begin_target_configuration();
            }
        });
    }

    fn current_request(
        &self,
        solution: Option<&CalibrationSolution>,
        serial_number: &str,
    ) -> Option<EepromProvisionRequest> {
        let image = solution
            .and_then(|solution| FullEepromImage::from_solution(solution, serial_number).ok())?;
        match self.mode {
            EepromProvisioningMode::FullProvision => {
                Some(image.full_provision_request(self.overwrite_existing_serial))
            }
            EepromProvisioningMode::UpdateCalibration => Some(image.update_calibration_request()),
        }
    }

    fn confirmed_provision_intent(
        &self,
        target_label: Option<&str>,
        request: Option<&EepromProvisionRequest>,
    ) -> Option<CalibrationProvisionIntent> {
        let target_label = target_label?;
        if self.busy || self.inspected_target.as_deref() != Some(target_label) {
            return None;
        }
        let request = request?;
        let device = self.device.as_ref()?;
        Some(CalibrationProvisionIntent::Provision {
            expected_target_label: target_label.to_owned(),
            request: request.clone(),
            expected_before_sha256: device.image_sha256.clone(),
        })
    }

    fn render_confirmation_modal(
        &mut self,
        context: &egui::Context,
        target_label: Option<&str>,
        request: Option<&EepromProvisionRequest>,
    ) {
        if !self.confirmation_open {
            return;
        }
        let mut open = true;
        let mut confirmed_intent = self.confirmed_provision_intent(target_label, request);
        egui::Window::new("Confirm EEPROM write")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(context, |ui| {
                ui.colored_label(
                    egui::Color32::RED,
                    "This modifies the physical module. Confirm the current read state before writing.",
                );
                if let Some(label) = target_label {
                    ui.label(format!("Target: {label}"));
                }
                ui.colored_label(egui::Color32::YELLOW, EEPROM_EXPERIMENTAL_PROVISION_WARNING);
                if let (Some(request), Some(device)) = (request, &self.device) {
                    ui.label(format!("Mode: {:?}", request.mode));
                    ui.label(format!("Serial: {}", request.serial_number));
                    ui.label(format!("Expected before: {}", device.image_sha256));
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

fn render_device_state(ui: &mut egui::Ui, device: &EepromDeviceState, backup: Option<&[u8]>) {
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
    if let Some(backup) = backup {
        render_eeprom_read_fields(ui, backup);
    }
}

fn render_eeprom_read_fields(ui: &mut egui::Ui, image: &[u8]) {
    ui.collapsing("EEPROM read fields", |ui| {
        let available_width = finite_available_width(ui);
        if available_width < 320.0 {
            for field in yg_stereo_p24c64g_v1().fields {
                let (raw_hex, parsed) = format_storage_field_read_value(field, image);
                ui.group(|ui| {
                    let width = finite_available_width(ui);
                    add_wrapped_break_anywhere_label(
                        ui,
                        format!("{} · 0x{:04x}", field.remark, field.offset),
                        false,
                        width,
                    )
                    .on_hover_text(field.name);
                    ui.weak("Raw hex");
                    add_wrapped_break_anywhere_label(ui, raw_hex, true, width)
                        .on_hover_text(field.name);
                    ui.weak("Parsed value");
                    add_wrapped_break_anywhere_label(ui, parsed, false, width)
                        .on_hover_text(field.name);
                });
            }
            return;
        }

        let spacing_x = 12.0;
        let field_width = (available_width * 0.24).min(160.0);
        let offset_width = (available_width * 0.18).min(72.0);
        let value_width = (available_width - field_width - offset_width - 2.0 * spacing_x).max(1.0);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(spacing_x, 4.0);
            ui.horizontal_top(|ui| {
                render_eeprom_read_header_cell(ui, field_width, "Field");
                render_eeprom_read_header_cell(ui, offset_width, "Offset");
                render_eeprom_read_header_cell(ui, value_width, "Value");
            });
            ui.separator();
            for field in yg_stereo_p24c64g_v1().fields {
                let (raw_hex, parsed) = format_storage_field_read_value(field, image);
                ui.horizontal_top(|ui| {
                    render_eeprom_read_text_cell(ui, field_width, |ui| {
                        add_wrapped_break_anywhere_label(ui, field.remark, false, field_width)
                            .on_hover_text(field.name);
                    });
                    render_eeprom_read_text_cell(ui, offset_width, |ui| {
                        ui.add_sized(
                            [offset_width, 0.0],
                            egui::Label::new(
                                egui::RichText::new(format!("0x{:04x}", field.offset)).monospace(),
                            )
                            .wrap(),
                        );
                    });
                    render_eeprom_read_value_cell(ui, value_width, &raw_hex, &parsed, field.name);
                });
                ui.separator();
            }
        });
    });
}

fn render_eeprom_read_header_cell(ui: &mut egui::Ui, width: f32, label: &str) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, 0.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.set_min_width(width);
            ui.set_max_width(width);
            ui.strong(label);
        },
    );
}

fn render_eeprom_read_text_cell(
    ui: &mut egui::Ui,
    width: f32,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, 0.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.set_min_width(width);
            ui.set_max_width(width);
            ui.spacing_mut().item_spacing.y = 2.0;
            add_contents(ui);
        },
    );
}

fn render_eeprom_read_value_cell(
    ui: &mut egui::Ui,
    width: f32,
    raw_hex: &str,
    parsed: &str,
    field_name: &str,
) {
    render_eeprom_read_text_cell(ui, width, |ui| {
        ui.weak("Raw hex");
        add_wrapped_break_anywhere_label(ui, raw_hex, true, width).on_hover_text(field_name);
        ui.weak("Parsed value");
        add_wrapped_break_anywhere_label(ui, parsed, false, width).on_hover_text(field_name);
    });
}

fn finite_available_width(ui: &egui::Ui) -> f32 {
    let width = ui.available_width();
    if width.is_finite() {
        width.max(1.0)
    } else {
        600.0
    }
}

fn add_wrapped_break_anywhere_label(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    monospace: bool,
    max_width: f32,
) -> egui::Response {
    let max_width = max_width.max(1.0);
    let font_id = if monospace {
        egui::TextStyle::Monospace.resolve(ui.style())
    } else {
        egui::TextStyle::Body.resolve(ui.style())
    };
    let mut job =
        egui::text::LayoutJob::simple(text.into(), font_id, ui.visuals().text_color(), max_width);
    job.wrap.break_anywhere = true;
    ui.add_sized([max_width, 0.0], egui::Label::new(job).wrap())
}

fn format_storage_field_read_value(field: &StorageField, image: &[u8]) -> (String, String) {
    let offset = usize::from(field.offset);
    let byte_len = usize::from(field.byte_len);
    let Some(end) = offset.checked_add(byte_len) else {
        return ("—".to_owned(), "offset overflow".to_owned());
    };
    if end > image.len() {
        return (
            "—".to_owned(),
            format!(
                "out of range: need 0x{end:04x}, image has {} B",
                image.len()
            ),
        );
    }
    let bytes = &image[offset..end];
    (
        format_hex_bytes(bytes),
        parse_storage_field_value(field.encoding, bytes),
    )
}

fn parse_storage_field_value(encoding: StorageEncoding, bytes: &[u8]) -> String {
    match encoding {
        StorageEncoding::Ascii | StorageEncoding::AsciiNulTerminated => {
            let visible = bytes.split(|byte| *byte == 0).next().unwrap_or(bytes);
            String::from_utf8_lossy(visible).to_string()
        }
        StorageEncoding::Raw | StorageEncoding::Reserved => format!("{} B", bytes.len()),
        StorageEncoding::U8 | StorageEncoding::SerialChecksum => {
            format_integer_array(bytes, 1, false)
        }
        StorageEncoding::U16Le => format_integer_array(bytes, 2, false),
        StorageEncoding::I16Le => format_integer_array(bytes, 2, true),
        StorageEncoding::U32Le => format_integer_array(bytes, 4, false),
        StorageEncoding::I32Le => format_integer_array(bytes, 4, true),
        StorageEncoding::F32Le => format_f32_array(bytes),
        StorageEncoding::F64Le => format_f64_array(bytes),
    }
}

fn format_integer_array(bytes: &[u8], width: usize, signed: bool) -> String {
    if bytes.len() % width != 0 {
        return format!(
            "invalid {}-byte integer field length: {}",
            width,
            bytes.len()
        );
    }
    let values = bytes
        .chunks_exact(width)
        .map(|chunk| match (width, signed) {
            (1, false) => chunk[0].to_string(),
            (1, true) => (chunk[0] as i8).to_string(),
            (2, false) => u16::from_le_bytes([chunk[0], chunk[1]]).to_string(),
            (2, true) => i16::from_le_bytes([chunk[0], chunk[1]]).to_string(),
            (4, false) => u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).to_string(),
            (4, true) => i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).to_string(),
            _ => unreachable!("unsupported integer field width"),
        })
        .collect::<Vec<_>>();
    format!("[{}]", values.join(", "))
}

fn format_f32_array(bytes: &[u8]) -> String {
    if bytes.len() % std::mem::size_of::<f32>() != 0 {
        return format!("invalid F32 field length: {}", bytes.len());
    }
    let values = bytes
        .chunks_exact(std::mem::size_of::<f32>())
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).to_string())
        .collect::<Vec<_>>();
    format!("[{}]", values.join(", "))
}

fn format_f64_array(bytes: &[u8]) -> String {
    if bytes.len() % std::mem::size_of::<f64>() != 0 {
        return format!("invalid F64 field length: {}", bytes.len());
    }
    let values = bytes
        .chunks_exact(std::mem::size_of::<f64>())
        .map(|chunk| {
            f64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ])
            .to_string()
        })
        .collect::<Vec<_>>();
    format!("[{}]", values.join(", "))
}

fn format_hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_i2c_bus_label(bus: &I2cBusInfo) -> String {
    let detail = bus.name.as_deref().unwrap_or(bus.dev_path.as_str());
    if bus.dev_node_exists {
        format!("i2c-{} — {}", bus.bus, detail)
    } else {
        format!("i2c-{} — {} (missing)", bus.bus, detail)
    }
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
    fn read_field_display_decodes_aggregate_raw_slices() {
        let map = yg_stereo_p24c64g_v1();
        let field = |name: &str| map.fields.iter().find(|field| field.name == name).unwrap();
        let image_size = field("image_size");
        let camera_matrix = field("camera_matrix");
        let distortion = field("distortion");
        let mut image = vec![0_u8; 308];
        image[usize::from(image_size.offset)..usize::from(image_size.offset) + 4]
            .copy_from_slice(&640_u32.to_le_bytes());
        image[usize::from(image_size.offset) + 4..usize::from(image_size.offset) + 8]
            .copy_from_slice(&480_u32.to_le_bytes());
        for (index, value) in [1.0_f32, 2.0, 3.0, 4.0].into_iter().enumerate() {
            let offset = usize::from(camera_matrix.offset) + index * 4;
            image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (index, value) in (0..12).map(|index| index as f32).enumerate() {
            let offset = usize::from(distortion.offset) + index * 4;
            image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }

        let (size_raw, size_value) = format_storage_field_read_value(image_size, &image);
        let (matrix_raw, matrix_value) = format_storage_field_read_value(camera_matrix, &image);
        let (_, distortion_value) = format_storage_field_read_value(distortion, &image);

        assert_eq!(size_raw, "80 02 00 00 e0 01 00 00");
        assert_eq!(size_value, "[640, 480]");
        assert!(matrix_raw.starts_with("00 00 80 3f 00 00 00 40"));
        assert_eq!(matrix_value, "[1, 2, 3, 4]");
        assert!(distortion_value.ends_with("10, 11]"));

        let short_image = vec![0_u8; usize::from(image_size.offset) + 1];
        let (raw, parsed) = format_storage_field_read_value(image_size, &short_image);
        assert_eq!(raw, "—");
        assert!(parsed.contains("out of range"));
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn bus_discovery_selects_first_available_bus() {
        let mut state = CalibrationEepromState::default();
        state.target_i2c_bus = 0;
        state.report_bus_discovery(vec![
            I2cBusInfo {
                bus: 7,
                dev_path: "/dev/i2c-7".to_owned(),
                name: Some("Synopsys DesignWare I2C adapter".to_owned()),
                dev_node_exists: true,
            },
            I2cBusInfo {
                bus: 8,
                dev_path: "/dev/i2c-8".to_owned(),
                name: None,
                dev_node_exists: true,
            },
        ]);

        assert_eq!(state.target_i2c_bus, 7);
        assert_eq!(state.discovered_buses.len(), 2);
        assert!(state.status.contains("Discovered 2 I²C bus(es)"));
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn bus_discovery_request_queues_intent() {
        let mut state = CalibrationEepromState::default();

        state.request_bus_discovery();

        assert!(state.busy);
        assert!(matches!(
            state.active_operation,
            Some(ActiveEepromOperation::Discovery)
        ));
        assert!(matches!(
            state.take_intent(),
            Some(CalibrationProvisionIntent::DiscoverBuses)
        ));
        assert!(state.status.contains("Discovering I²C buses"));
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn changing_selected_bus_queues_target_refresh_without_losing_bus_list() {
        let mut state = CalibrationEepromState::default();
        state.report_bus_discovery(vec![
            I2cBusInfo {
                bus: 4,
                dev_path: "/dev/i2c-4".to_owned(),
                name: None,
                dev_node_exists: true,
            },
            I2cBusInfo {
                bus: 6,
                dev_path: "/dev/i2c-6".to_owned(),
                name: None,
                dev_node_exists: true,
            },
        ]);

        state.target_i2c_bus = 6;
        state.begin_target_configuration();

        assert!(matches!(
            state.take_intent(),
            Some(CalibrationProvisionIntent::ConfigureTarget(
                CalibrationEepromTargetRequest { i2c_bus: 6 }
            ))
        ));
        assert_eq!(state.discovered_buses.len(), 2);
        assert!(state.status.contains("i2c-6"));

        state.report_target_configured("root@camera:22 / i2c-6 @test");
        assert_eq!(state.discovered_buses.len(), 2);
    }

    #[test]
    fn provision_transport_unknown_forces_fresh_inspection_before_retry() {
        let mut state = CalibrationEepromState::default();
        state.pending = Some(CalibrationProvisionIntent::Provision {
            expected_target_label: "root@camera / i2c-7".to_owned(),
            request: request(),
            expected_before_sha256: "a".repeat(64),
        });
        state.busy = true;
        state.active_operation = Some(ActiveEepromOperation::Provision);

        state.report_provision_unknown("SSH response was lost");

        assert!(state.device.is_none());
        assert!(state.inspected_target.is_none());
        assert!(state.pending.is_none());
        assert!(!state.busy);
        assert!(state.status.contains("UNKNOWN"));
        assert!(state.status.contains("Re-inspect"));
    }

    #[test]
    fn final_write_intent_requires_current_read_idle_state() {
        let mut state = CalibrationEepromState::default();
        let current_request = request();
        state.device = Some(device_state());
        state.inspected_target = Some("root@camera / i2c-7".to_owned());

        assert!(matches!(
            state.confirmed_provision_intent(Some("root@camera / i2c-7"), Some(&current_request)),
            Some(CalibrationProvisionIntent::Provision { .. })
        ));
        state.busy = true;
        assert!(
            state
                .confirmed_provision_intent(Some("root@camera / i2c-7"), Some(&current_request))
                .is_none()
        );
    }

    #[cfg(feature = "platform-ssh")]
    #[test]
    fn failed_reconfiguration_invalidates_existing_read_authorization() {
        let mut state = CalibrationEepromState::default();
        let old_request = request();
        state.device = Some(device_state());
        state.inspected_target = Some("root@old-camera / i2c-7".to_owned());
        assert!(
            state
                .confirmed_provision_intent(Some("root@old-camera / i2c-7"), Some(&old_request))
                .is_some()
        );

        state.begin_target_configuration();
        assert!(matches!(
            state.take_intent(),
            Some(CalibrationProvisionIntent::ConfigureTarget(_))
        ));
        state.report_target_configuration_failed("no active SFTP source");

        assert!(state.device.is_none());
        assert!(state.inspected_target.is_none());
        assert!(
            state
                .confirmed_provision_intent(Some("root@old-camera / i2c-7"), Some(&old_request))
                .is_none()
        );
        assert!(state.take_intent().is_none());
    }
}
