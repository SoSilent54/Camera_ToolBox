//! 专家级 I²C 原始条目控制台。
//!
//! UI 以一行一个条目的方式编辑 bus、I²C 地址、offset、长度/类型、flags、写入值与读取值；
//! EEPROM 写入条目会在执行前按页边界拆成一个或多个 `I2C_RDWR` transaction。

use camera_toolbox_app::{
    I2C_HELPER_MAX_MESSAGE_BYTES, I2C_HELPER_MAX_TRANSACTIONS_PER_REQUEST, I2cBusInfo,
    I2cMessageData, I2cMessageDirection, I2cMessageFlag, I2cMessageResult, I2cMessageSpec,
    I2cTransactionResult, I2cTransactionSpec, validate_i2c_transfer_transactions,
};
#[cfg(test)]
use camera_toolbox_core::{
    CalibrationStorageMap, StorageField, baton_param_rw_native_lp64_le_v1,
    pueo_edu_df9_40_native_lp64_le_v1, yg_stereo_p24c64g_v1,
};
use camera_toolbox_core::{
    CompiledEepromMapConfig, CompiledEepromMapField, IMX219_EEPROM_CALIBRATION_CONFIG_NAME,
    PUEO_EDU_DF9_40_PINOUT_CONFIG_NAME, StorageEncoding, compile_builtin_eeprom_map_config,
    list_builtin_eeprom_map_configs,
};
use eframe::egui;

const I2C_TOOLS_FOOTER_ID: &str = "i2c_tools_execute_footer";

#[derive(Clone, Debug)]
struct I2cToolsColumnWidths {
    remark: f32,
    address: f32,
    offset: f32,
    length_type: f32,
    write_value: f32,
    flags: f32,
    read_value: f32,
    default_operation: f32,
    action: f32,
}

/// GUI 交给顶层 app 执行的 I²C 操作；实际 SSH/helper 调度不放在 UI 状态里。
#[derive(Clone, Debug)]
pub(crate) enum I2cToolsAction {
    Cancel,
    DiscoverBuses,
    ExecuteTransfer(Vec<I2cTransactionSpec>),
}

#[derive(Clone, Debug)]
struct TransferReport {
    transactions: Vec<I2cTransactionResult>,
}

#[derive(Clone, Debug)]
enum PendingTransfer {
    AllRead(Vec<usize>),
    RowRead(usize),
    RowWrite,
    Preview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EepromPresetSelection {
    None,
    BuiltIn(&'static str),
}

impl EepromPresetSelection {
    const ALL: [Self; 3] = [
        Self::None,
        Self::BuiltIn(IMX219_EEPROM_CALIBRATION_CONFIG_NAME),
        Self::BuiltIn(PUEO_EDU_DF9_40_PINOUT_CONFIG_NAME),
    ];

    fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::BuiltIn(name) => list_builtin_eeprom_map_configs()
                .iter()
                .find(|config| config.name == name)
                .map_or(name, |config| config.display_name),
        }
    }
}

/// I²C Tools 页面状态。UI 是扁平条目表；一个 UI 条目可在执行时拆成多个 helper transaction。
pub(crate) struct I2cToolsWorkspace {
    buses: Vec<I2cBusInfo>,
    selected_bus: u32,
    entries: Vec<RowDraft>,
    result: Option<TransferReport>,
    pending_transfer: Option<PendingTransfer>,
    selected_preset: EepromPresetSelection,
    loaded_config: Option<CompiledEepromMapConfig>,
    busy: bool,
    cancel_requested: bool,
    status: String,
}

impl Default for I2cToolsWorkspace {
    fn default() -> Self {
        Self {
            buses: Vec::new(),
            selected_bus: 0,
            entries: vec![RowDraft::read_template(0, 0x50)],
            result: None,
            pending_transfer: None,
            selected_preset: EepromPresetSelection::None,
            loaded_config: None,
            busy: false,
            cancel_requested: false,
            status: "Connect Explorer SFTP, refresh buses, choose one I²C bus, then use row Read/Write or All Read."
                .to_owned(),
        }
    }
}

impl I2cToolsWorkspace {
    pub(crate) fn render(
        &mut self,
        ui: &mut egui::Ui,
        sftp_source: Result<&str, &str>,
    ) -> Option<I2cToolsAction> {
        let mut action = None;
        let footer_id = egui::Id::new(I2C_TOOLS_FOOTER_ID);
        if egui::containers::panel::PanelState::load(ui.ctx(), footer_id).is_none() {
            ui.ctx().request_discard("initial I2C footer sizing");
        }
        egui::Panel::bottom(footer_id)
            .resizable(false)
            .show_separator_line(true)
            .show(ui, |ui| {
                self.render_execute_bar(ui, sftp_source, &mut action);
                ui.separator();
                self.render_footer_status(ui);
            });

        egui::ScrollArea::vertical()
            .id_salt("i2c_tools_body_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.heading("I²C Tools");
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "Expert raw I²C console: no register semantics, no rollback, no automatic verification.",
                );
                ui.weak(
                    "All rows use the single selected I²C bus. Read builds offset-write + read when Offset is set; Write encodes Write value by Type and sends Offset + payload. Validation rejects 0x00..=0x02 reserved/general-call addresses; 7-bit max is 0x7f, TenBit max is 0x03ff.",
                );
                match sftp_source {
                    Ok(label) => ui.label(format!("Explorer SFTP: {label}")),
                    Err(reason) => ui.colored_label(egui::Color32::YELLOW, reason),
                };

                self.render_bus_controls(ui, sftp_source, &mut action);

                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            !self.busy && self.entries.len() < I2C_HELPER_MAX_TRANSACTIONS_PER_REQUEST,
                            egui::Button::new("Add row"),
                        )
                        .clicked()
                    {
                        self.entries.push(RowDraft::read_template(self.default_bus(), 0x50));
                        self.result = None;
                        self.status = "Added a manual I²C row.".to_owned();
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Read template"))
                        .clicked()
                    {
                        self.entries = vec![RowDraft::read_template(self.default_bus(), 0x50)];
                        self.result = None;
                        self.status = "Loaded a single-row read template.".to_owned();
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Write template"))
                        .clicked()
                    {
                        self.entries = vec![RowDraft::write_template(self.default_bus(), 0x50)];
                        self.result = None;
                        self.status = "Loaded a single-row write template.".to_owned();
                    }
                    let all_read_response = ui
                        .add_enabled(
                            !self.busy && sftp_source.is_ok() && !self.entries.is_empty(),
                            egui::Button::new("All Read"),
                        )
                        .on_disabled_hover_text(
                            "Connect Explorer SFTP and keep at least one row before All Read.",
                        );
                    if all_read_response.clicked() {
                        match self.build_all_read_transfer() {
                            Ok((transactions, rows)) => self.start_transfer(
                                transactions,
                                Some(PendingTransfer::AllRead(rows)),
                                "Reading all rows...",
                                &mut action,
                            ),
                            Err(error) => {
                                self.status = format!("All Read is not executable: {error}");
                            }
                        }
                    }
                });

                self.render_map_loader(ui);
                ui.separator();
                self.render_entries(ui, sftp_source, &mut action);
                ui.separator();
                self.render_request_preview(ui);
                self.render_result(ui);
            });
        action
    }

    pub(crate) fn report_buses(&mut self, buses: Vec<I2cBusInfo>) {
        self.busy = false;
        self.cancel_requested = false;
        self.buses = buses;
        if self.buses.iter().all(|info| info.bus != self.selected_bus)
            && let Some(bus) = self.buses.first().map(|bus| bus.bus)
        {
            self.selected_bus = bus;
        }
        self.status = if self.buses.is_empty() {
            "No /dev/i2c-* buses were discovered.".to_owned()
        } else {
            format!(
                "Discovered {} I²C bus(es). Selected bus i2c-{} is applied to all rows.",
                self.buses.len(),
                self.selected_bus
            )
        };
    }

    pub(crate) fn report_transfer(&mut self, transactions: Vec<I2cTransactionResult>) {
        self.busy = false;
        if let Some(pending) = self.pending_transfer.take() {
            self.apply_transfer_to_rows(&pending, &transactions);
        }
        self.result = Some(TransferReport { transactions });
        self.status = "I²C transfer completed.".to_owned();
    }

    pub(crate) fn report_error(&mut self, message: impl Into<String>) {
        self.busy = false;
        self.cancel_requested = false;
        self.pending_transfer = None;
        self.status = format!("I²C operation failed: {}", message.into());
    }

    pub(crate) fn report_cancelled(&mut self) {
        self.cancel_requested = true;
        self.status = "Cancellation requested...".to_owned();
    }

    fn default_bus(&self) -> u32 {
        self.selected_bus
    }

    fn render_bus_controls(
        &mut self,
        ui: &mut egui::Ui,
        sftp_source: Result<&str, &str>,
        action: &mut Option<I2cToolsAction>,
    ) {
        ui.group(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong("I²C Bus");
                if ui
                    .add_enabled(
                        !self.busy && sftp_source.is_ok(),
                        egui::Button::new("Refresh buses"),
                    )
                    .clicked()
                {
                    self.busy = true;
                    self.cancel_requested = false;
                    self.pending_transfer = None;
                    self.status =
                        "Discovering I²C buses through the active Explorer SFTP connection..."
                            .to_owned();
                    *action = Some(I2cToolsAction::DiscoverBuses);
                }

                let before = self.selected_bus;
                if self.buses.is_empty() {
                    ui.label("Bus");
                    ui.add_enabled(
                        !self.busy,
                        egui::DragValue::new(&mut self.selected_bus)
                            .speed(1)
                            .range(0..=u32::from(u16::MAX)),
                    );
                } else {
                    let selected = self
                        .buses
                        .iter()
                        .find(|bus| bus.bus == self.selected_bus)
                        .map(format_bus_label)
                        .unwrap_or_else(|| format!("i2c-{}", self.selected_bus));
                    ui.label("Bus");
                    ui.add_enabled_ui(!self.busy, |ui| {
                        egui::ComboBox::from_id_salt("i2c_tools_global_bus")
                            .selected_text(selected)
                            .show_ui(ui, |ui| {
                                for bus in &self.buses {
                                    ui.selectable_value(
                                        &mut self.selected_bus,
                                        bus.bus,
                                        format_bus_label(bus),
                                    );
                                }
                            });
                    });
                }
                if self.selected_bus != before {
                    self.result = None;
                    self.status =
                        format!("Selected i2c-{} for all I²C Tools rows.", self.selected_bus);
                }
            });
            ui.weak("Preset labels such as I2C0 are map metadata only; this selected Linux bus is what every row uses.");
        });
    }

    fn render_map_loader(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("EEPROM config loader")
            .default_open(false)
            .show(ui, |ui| {
                ui.weak("EEPROM config rows are still raw I²C rows: short Remark, offset, byte count and type. Choosing a preset immediately replaces the table; it does not add provisioning safety semantics.");
                let previous = self.selected_preset;
                ui.add_enabled_ui(!self.busy, |ui| {
                    egui::ComboBox::from_label("Preset")
                        .selected_text(self.selected_preset.label())
                        .show_ui(ui, |ui| {
                            for preset in EepromPresetSelection::ALL {
                                ui.selectable_value(&mut self.selected_preset, preset, preset.label());
                            }
                        });
                });
                if self.selected_preset != previous {
                    self.apply_selected_preset();
                }
                if let Some(config) = &self.loaded_config {
                    self.render_loaded_map_summary(ui, config);
                } else {
                    ui.weak("Preset: None. Rows are manual raw I²C entries.");
                }
            });
    }

    fn apply_selected_preset(&mut self) {
        match self.selected_preset {
            EepromPresetSelection::None => self.clear_preset_binding(),
            EepromPresetSelection::BuiltIn(name) => match compile_builtin_eeprom_map_config(name) {
                Ok(config) => self.load_config_rows(config),
                Err(error) => {
                    self.status = format!("Failed to load EEPROM preset: {error}");
                }
            },
        }
    }

    fn clear_preset_binding(&mut self) {
        self.loaded_config = None;
        for row in &mut self.entries {
            row.writable = true;
            row.eeprom_address_width_bits = None;
            row.eeprom_page_size = None;
            row.eeprom_write_cycle_ms = None;
        }
        if self.entries.is_empty() {
            self.entries
                .push(RowDraft::read_template(self.default_bus(), 0x50));
        }
        self.status = "Preset cleared. Existing rows are now manual raw I²C entries.".to_owned();
    }

    fn load_config_rows(&mut self, config: CompiledEepromMapConfig) {
        self.entries = config
            .fields
            .iter()
            .map(|field| compiled_field_read_row(self.default_bus(), &config, field))
            .collect();
        self.status = format!(
            "Loaded {} EEPROM config row(s) from {}.",
            self.entries.len(),
            config.display_name
        );
        self.loaded_config = Some(config);
    }

    fn render_loaded_map_summary(&self, ui: &mut egui::Ui, config: &CompiledEepromMapConfig) {
        ui.label(format!(
            "{}: address=0x{:02x}, address-width={} bits, page={} bytes, write-cycle={} ms, fields={}, total={} bytes.",
            config.display_name,
            config.transport.i2c_address,
            config.transport.address_width_bits,
            config.transport.page_size_bytes,
            config.transport.write_cycle_ms,
            config.fields.len(),
            config.total_bytes
        ));
        ui.weak("Remark is the short row name from the config, e.g. SNID or fx/fy/cx/cy; Bytes is the configured data amount. EEPROM writes are split on page boundaries before execution.");
    }

    fn render_request_preview(&self, ui: &mut egui::Ui) {
        let preview = self.build_transfer();
        match &preview {
            Ok(transactions) => render_transfer_preview(ui, Ok(transactions.as_slice())),
            Err(error) => render_transfer_preview(ui, Err(error.as_str())),
        }
    }

    fn render_footer_status(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.busy {
                ui.spinner();
            }
            ui.strong("Status:");
            ui.add(egui::Label::new(self.status.as_str()).truncate())
                .on_hover_text(self.status.as_str());
        });
    }

    fn render_entries(
        &mut self,
        ui: &mut egui::Ui,
        sftp_source: Result<&str, &str>,
        action: &mut Option<I2cToolsAction>,
    ) {
        ui.horizontal(|ui| {
            ui.heading("Rows");
            ui.weak(format!("{} row(s)", self.entries.len()));
            if self.busy {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "Editing is locked while the cloned request is in flight.",
                );
            }
        });
        ui.weak("One row = one independent Read/Write item. The global I²C Bus selector controls every row; Read value is decoded from the latest raw bytes using the current Type.");

        let mut row_action = None;
        let available_width = finite_available_width(ui);
        if available_width < 960.0 {
            for index in 0..self.entries.len() {
                ui.group(|ui| {
                    ui.set_max_width(available_width);
                    if let Some(action) = render_row_card(
                        ui,
                        index,
                        &mut self.entries[index],
                        self.selected_bus,
                        self.busy,
                        sftp_source.is_ok(),
                    ) {
                        row_action = Some(action);
                    }
                });
            }
        } else {
            let widths = responsive_i2c_column_widths(available_width);
            egui::Grid::new("i2c_tools_rows")
                .num_columns(9)
                .striped(true)
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    ui.strong("Remark");
                    ui.strong("Address");
                    ui.strong("Offset");
                    ui.strong("Length / Type");
                    ui.strong("Write value");
                    ui.strong("Flags");
                    ui.strong("Read value");
                    ui.strong("Default");
                    ui.strong("Action");
                    ui.end_row();

                    for index in 0..self.entries.len() {
                        if let Some(action) = render_row(
                            ui,
                            index,
                            &mut self.entries[index],
                            &widths,
                            self.selected_bus,
                            self.busy,
                            sftp_source.is_ok(),
                        ) {
                            row_action = Some(action);
                        }
                        ui.end_row();
                    }
                });
        }

        match row_action {
            Some(RowUiAction::Read(index)) => match self.build_row_read_transfer(index) {
                Ok(transactions) => self.start_transfer(
                    transactions,
                    Some(PendingTransfer::RowRead(index)),
                    "Reading row...",
                    action,
                ),
                Err(error) => self.status = format!("Row read is not executable: {error}"),
            },
            Some(RowUiAction::Write(index)) => match self.build_row_write_transfer(index) {
                Ok(transactions) => self.start_transfer(
                    transactions,
                    Some(PendingTransfer::RowWrite),
                    "Writing row...",
                    action,
                ),
                Err(error) => self.status = format!("Row write is not executable: {error}"),
            },
            Some(RowUiAction::Remove(index)) if self.entries.len() > 1 => {
                self.entries.remove(index);
                self.status = "Removed row.".to_owned();
            }
            Some(RowUiAction::Remove(_)) => {
                self.status = "Keep at least one row in the table.".to_owned();
            }
            None => {}
        }
    }

    fn render_execute_bar(
        &mut self,
        ui: &mut egui::Ui,
        sftp_source: Result<&str, &str>,
        action: &mut Option<I2cToolsAction>,
    ) {
        let preview = self.build_transfer();
        let mut execute_request = None;

        let disabled_reason = Self::execute_disabled_reason(
            self.busy,
            sftp_source,
            preview.as_ref().map(Vec::as_slice).map_err(String::as_str),
        );

        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    self.busy && !self.cancel_requested,
                    egui::Button::new("Cancel"),
                )
                .clicked()
            {
                self.cancel_requested = true;
                self.status = "Cancellation requested...".to_owned();
                *action = Some(I2cToolsAction::Cancel);
            }
            let mut execute_response = ui.add_enabled(
                disabled_reason.is_none(),
                egui::Button::new("Execute Transfer").fill(egui::Color32::DARK_RED),
            );
            if let Some(reason) = &disabled_reason {
                execute_response = execute_response.on_disabled_hover_text(reason.as_str());
            }
            if execute_response.clicked()
                && let Ok(transactions) = &preview
            {
                execute_request = Some(transactions.clone());
            }
        });
        if let Some(transactions) = execute_request {
            self.start_transfer(
                transactions,
                Some(PendingTransfer::Preview),
                "Executing raw I²C transfer...",
                action,
            );
        }
    }

    fn execute_disabled_reason(
        busy: bool,
        sftp_source: Result<&str, &str>,
        preview: Result<&[I2cTransactionSpec], &str>,
    ) -> Option<String> {
        if let Err(reason) = sftp_source {
            return Some(format!("Connect Explorer SFTP first: {reason}"));
        }
        if let Err(error) = preview {
            return Some(format!("Fix the draft before executing: {error}"));
        }
        if busy {
            return Some(
                "An I²C operation is already in flight; cancel or wait for it before executing."
                    .to_owned(),
            );
        }
        None
    }

    fn render_result(&self, ui: &mut egui::Ui) {
        let Some(report) = &self.result else {
            return;
        };
        ui.separator();
        ui.heading("Last result");
        for (transaction_index, transaction) in report.transactions.iter().enumerate() {
            ui.group(|ui| {
                ui.label(format!(
                    "Transaction {transaction_index}: bus i2c-{}, transferred {} message(s)",
                    transaction.bus, transaction.transferred_messages
                ));
                egui::Grid::new(format!("i2c_tools_result_{transaction_index}"))
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong("#");
                        ui.strong("Address");
                        ui.strong("Direction");
                        ui.strong("Bytes");
                        ui.strong("Data");
                        ui.end_row();
                        for (message_index, message) in transaction.messages.iter().enumerate() {
                            render_message_result_row(ui, message_index, message);
                        }
                    });
            });
        }
    }
    fn build_transfer(&self) -> Result<Vec<I2cTransactionSpec>, String> {
        self.build_transfer_with(|entry| entry.default_operation)
    }

    fn build_all_read_transfer(&self) -> Result<(Vec<I2cTransactionSpec>, Vec<usize>), String> {
        if self.entries.is_empty() {
            return Err("at least one row is required".to_owned());
        }
        let mut rows = Vec::with_capacity(self.entries.len());
        let mut transactions = Vec::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter().enumerate() {
            rows.push(index);
            transactions.push(
                entry
                    .read_transaction(self.selected_bus)
                    .map_err(|error| format!("row {index}: {error}"))?,
            );
        }
        validate_rows(&transactions)?;
        Ok((transactions, rows))
    }

    fn build_row_read_transfer(&self, index: usize) -> Result<Vec<I2cTransactionSpec>, String> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| format!("row {index} does not exist"))?;
        let transactions = vec![
            entry
                .read_transaction(self.selected_bus)
                .map_err(|error| format!("row {index}: {error}"))?,
        ];
        validate_rows(&transactions)?;
        Ok(transactions)
    }

    fn build_row_write_transfer(&self, index: usize) -> Result<Vec<I2cTransactionSpec>, String> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| format!("row {index} does not exist"))?;
        let transactions = entry
            .write_transactions(self.selected_bus)
            .map_err(|error| format!("row {index}: {error}"))?;
        validate_rows(&transactions)?;
        Ok(transactions)
    }

    fn build_transfer_with(
        &self,
        operation_for: impl Fn(&RowDraft) -> RowOperation,
    ) -> Result<Vec<I2cTransactionSpec>, String> {
        if self.entries.is_empty() {
            return Err("at least one row is required".to_owned());
        }
        let mut transactions = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            transactions.extend(
                entry
                    .transactions(self.selected_bus, operation_for(entry))
                    .map_err(|error| format!("row {index}: {error}"))?,
            );
        }
        validate_rows(&transactions)?;
        Ok(transactions)
    }

    fn start_transfer(
        &mut self,
        transactions: Vec<I2cTransactionSpec>,
        pending: Option<PendingTransfer>,
        status: &str,
        action: &mut Option<I2cToolsAction>,
    ) {
        self.busy = true;
        self.cancel_requested = false;
        self.result = None;
        self.pending_transfer = pending;
        self.status = status.to_owned();
        *action = Some(I2cToolsAction::ExecuteTransfer(transactions));
    }

    fn apply_transfer_to_rows(
        &mut self,
        pending: &PendingTransfer,
        transactions: &[I2cTransactionResult],
    ) {
        match pending {
            PendingTransfer::AllRead(rows) => {
                for (transaction, row_index) in transactions.iter().zip(rows.iter().copied()) {
                    if let Some(bytes) = read_value_from_transaction(transaction)
                        && let Some(row) = self.entries.get_mut(row_index)
                    {
                        row.read_value_raw = Some(RowReadValue {
                            snapshot: row.read_request_snapshot(self.selected_bus),
                            bytes: bytes.to_vec(),
                        });
                    }
                }
            }
            PendingTransfer::RowRead(row_index) => {
                if let Some(transaction) = transactions.first()
                    && let Some(bytes) = read_value_from_transaction(transaction)
                    && let Some(row) = self.entries.get_mut(*row_index)
                {
                    row.read_value_raw = Some(RowReadValue {
                        snapshot: row.read_request_snapshot(self.selected_bus),
                        bytes: bytes.to_vec(),
                    });
                }
            }
            PendingTransfer::RowWrite | PendingTransfer::Preview => {}
        }
    }
}

fn validate_rows(transactions: &[I2cTransactionSpec]) -> Result<(), String> {
    validate_i2c_transfer_transactions(transactions).map_err(|error| {
        match (error.transaction_index, error.message_index) {
            (Some(transaction_index), Some(message_index)) => format!(
                "row {transaction_index} message {message_index}: {}",
                error.message
            ),
            (Some(transaction_index), None) => {
                format!("row {transaction_index}: {}", error.message)
            }
            (None, _) => error.message,
        }
    })
}

fn render_transfer_preview(ui: &mut egui::Ui, preview: Result<&[I2cTransactionSpec], &str>) {
    egui::CollapsingHeader::new("Request preview")
        .default_open(true)
        .show(ui, |ui| match preview {
            Ok(transactions) => {
                let (message_count, write_bytes, read_bytes) = transaction_preview_counts(transactions);
                ui.weak(format!(
                    "{} row transaction(s), {message_count} helper message(s), write {write_bytes} B, read {read_bytes} B",
                    transactions.len()
                ));
                for (transaction_index, transaction) in transactions.iter().take(8).enumerate() {
                    let messages = transaction
                        .messages
                        .iter()
                        .map(message_preview_label)
                        .collect::<Vec<_>>()
                        .join(" → ");
                    ui.monospace(format!(
                        "Row {transaction_index} i2c-{}: {messages}",
                        transaction.bus
                    ));
                }
                if transactions.len() > 8 {
                    ui.weak(format!(
                        "… {} more row transaction(s) hidden from preview",
                        transactions.len() - 8
                    ));
                }
            }
            Err(error) => {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    format!("Draft is not executable: {error}"),
                );
            }
        });
}

fn transaction_preview_counts(transactions: &[I2cTransactionSpec]) -> (usize, usize, usize) {
    let mut message_count = 0;
    let mut write_bytes = 0;
    let mut read_bytes = 0;
    for transaction in transactions {
        for message in &transaction.messages {
            message_count += 1;
            match &message.data {
                I2cMessageData::Write { bytes } => write_bytes += bytes.len(),
                I2cMessageData::Read { byte_len } => read_bytes += usize::from(*byte_len),
            }
        }
    }
    (message_count, write_bytes, read_bytes)
}

fn message_preview_label(message: &I2cMessageSpec) -> String {
    let flags = message_flags_suffix(&message.flags);
    match &message.data {
        I2cMessageData::Write { bytes } => format!(
            "W 0x{:x} [{} B: {}]{flags}",
            message.address,
            bytes.len(),
            format_preview_hex_bytes(bytes, 16)
        ),
        I2cMessageData::Read { byte_len } => {
            format!("R 0x{:x} [{byte_len} B]{flags}", message.address)
        }
    }
}

fn message_flags_suffix(flags: &[I2cMessageFlag]) -> String {
    if flags.is_empty() {
        return String::new();
    }
    let labels = flags
        .iter()
        .map(|flag| flag_label(*flag))
        .collect::<Vec<_>>();
    format!(" flags={}", labels.join("|"))
}

fn flag_label(flag: I2cMessageFlag) -> &'static str {
    match flag {
        I2cMessageFlag::TenBitAddress => "TenBit",
        I2cMessageFlag::Stop => "Stop",
        I2cMessageFlag::NoStart => "NoStart",
        I2cMessageFlag::IgnoreNack => "IgnoreNack",
        I2cMessageFlag::IgnoreAck => "IgnoreAck",
    }
}

fn format_preview_hex_bytes(bytes: &[u8], max_bytes: usize) -> String {
    let shown = bytes.len().min(max_bytes);
    let mut text = format_hex_bytes(&bytes[..shown]);
    if bytes.len() > shown {
        text.push_str(&format!(" … (+{} B)", bytes.len() - shown));
    }
    text
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RowReadRequestSnapshot {
    bus: u32,
    address: u16,
    offset_hex: String,
    byte_len: u16,
    flags: Vec<I2cMessageFlag>,
}

#[derive(Clone, Debug)]
struct RowReadValue {
    snapshot: RowReadRequestSnapshot,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RowReadDisplay {
    raw_hex: String,
    parsed: String,
}

#[derive(Clone, Debug)]
struct RowDraft {
    remark: String,
    address: u16,
    offset_hex: String,
    byte_len: u16,
    value_type: RowValueType,
    write_hex: String,
    flags: Vec<I2cMessageFlag>,
    read_value_raw: Option<RowReadValue>,
    default_operation: RowOperation,
    writable: bool,
    eeprom_address_width_bits: Option<u8>,
    eeprom_page_size: Option<u16>,
    eeprom_write_cycle_ms: Option<u16>,
}

impl RowDraft {
    fn read_template(_bus: u32, address: u16) -> Self {
        Self {
            remark: "manual".to_owned(),
            address,
            offset_hex: String::new(),
            byte_len: 1,
            value_type: RowValueType::Raw,
            write_hex: String::new(),
            flags: Vec::new(),
            read_value_raw: None,
            default_operation: RowOperation::Read,
            writable: true,
            eeprom_address_width_bits: None,
            eeprom_page_size: None,
            eeprom_write_cycle_ms: None,
        }
    }

    fn write_template(_bus: u32, address: u16) -> Self {
        Self {
            remark: "manual".to_owned(),
            address,
            offset_hex: String::new(),
            byte_len: 1,
            value_type: RowValueType::Raw,
            write_hex: "00".to_owned(),
            flags: Vec::new(),
            read_value_raw: None,
            default_operation: RowOperation::Write,
            writable: true,
            eeprom_address_width_bits: None,
            eeprom_page_size: None,
            eeprom_write_cycle_ms: None,
        }
    }

    fn sync_byte_len_to_type(&mut self) {
        if let Some(byte_len) = self.value_type.fixed_byte_len() {
            self.byte_len = byte_len;
        }
    }

    fn effective_byte_len(&self) -> u16 {
        self.value_type.fixed_byte_len().unwrap_or(self.byte_len)
    }

    fn transactions(
        &self,
        bus: u32,
        operation: RowOperation,
    ) -> Result<Vec<I2cTransactionSpec>, String> {
        match operation {
            RowOperation::Read => Ok(vec![self.read_transaction(bus)?]),
            RowOperation::Write => self.write_transactions(bus),
        }
    }

    fn read_transaction(&self, bus: u32) -> Result<I2cTransactionSpec, String> {
        let byte_len = self.effective_byte_len();
        self.value_type.validate_byte_len(byte_len)?;
        let offset = parse_optional_hex_bytes(&self.offset_hex)?;
        let mut messages = Vec::with_capacity(if offset.is_empty() { 1 } else { 2 });
        if !offset.is_empty() {
            messages.push(I2cMessageSpec {
                address: self.address,
                flags: Vec::new(),
                data: I2cMessageData::Write { bytes: offset },
            });
        }
        messages.push(I2cMessageSpec {
            address: self.address,
            flags: self.flags.clone(),
            data: I2cMessageData::Read { byte_len },
        });
        Ok(I2cTransactionSpec {
            bus,
            messages,
            settle_ms: None,
        })
    }

    fn write_transactions(&self, bus: u32) -> Result<Vec<I2cTransactionSpec>, String> {
        if !self.writable {
            return Err("row is read-only in the loaded EEPROM config".to_owned());
        }
        let byte_len = self.effective_byte_len();
        self.value_type.validate_byte_len(byte_len)?;
        let payload = encode_typed_write_payload(self.value_type, byte_len, &self.write_hex)?;
        if self.eeprom_page_size.is_some() {
            self.eeprom_page_write_transactions(bus, &payload)
        } else {
            let mut bytes = parse_optional_hex_bytes(&self.offset_hex)?;
            bytes.extend_from_slice(&payload);
            Ok(vec![self.write_transaction_from_bytes(bus, bytes, None)])
        }
    }

    fn eeprom_page_write_transactions(
        &self,
        bus: u32,
        payload: &[u8],
    ) -> Result<Vec<I2cTransactionSpec>, String> {
        let page_size = self
            .eeprom_page_size
            .ok_or_else(|| "EEPROM page size is unavailable".to_owned())?;
        if page_size == 0 {
            return Err("EEPROM page size must be at least 1 byte".to_owned());
        }
        let address_width_bits = self
            .eeprom_address_width_bits
            .ok_or_else(|| "EEPROM address width is unavailable".to_owned())?;
        let mut offset = parse_eeprom_offset(&self.offset_hex, address_width_bits)?;
        let mut remaining = payload;
        let mut transactions = Vec::new();

        while !remaining.is_empty() {
            let page_remaining =
                usize::from(page_size) - (usize::from(offset) % usize::from(page_size));
            let chunk_len = remaining.len().min(page_remaining);
            let mut bytes = eeprom_offset_bytes(offset, address_width_bits);
            bytes.extend_from_slice(&remaining[..chunk_len]);
            transactions.push(self.write_transaction_from_bytes(
                bus,
                bytes,
                self.eeprom_write_cycle_ms,
            ));
            offset = offset
                .checked_add(u16::try_from(chunk_len).map_err(|_| {
                    "EEPROM page-write chunk length cannot be represented as u16".to_owned()
                })?)
                .ok_or_else(|| "EEPROM write offset exceeds 16-bit address space".to_owned())?;
            remaining = &remaining[chunk_len..];
        }

        Ok(transactions)
    }

    fn write_transaction_from_bytes(
        &self,
        bus: u32,
        bytes: Vec<u8>,
        settle_ms: Option<u16>,
    ) -> I2cTransactionSpec {
        I2cTransactionSpec {
            bus,
            messages: vec![I2cMessageSpec {
                address: self.address,
                flags: self.flags.clone(),
                data: I2cMessageData::Write { bytes },
            }],
            settle_ms,
        }
    }

    fn read_request_snapshot(&self, bus: u32) -> RowReadRequestSnapshot {
        RowReadRequestSnapshot {
            bus,
            address: self.address,
            offset_hex: self.offset_hex.trim().to_owned(),
            byte_len: self.effective_byte_len(),
            flags: self.flags.clone(),
        }
    }

    fn read_display(&self, bus: u32) -> Option<RowReadDisplay> {
        let value = self.read_value_raw.as_ref()?;
        (value.snapshot == self.read_request_snapshot(bus)).then(|| {
            format_typed_read_display(self.value_type, self.effective_byte_len(), &value.bytes)
        })
    }

    #[cfg(test)]
    fn formatted_read_value(&self, bus: u32) -> Option<String> {
        self.read_display(bus).map(|display| {
            if display.parsed == "—" {
                display.raw_hex
            } else {
                format!("{} | {}", display.raw_hex, display.parsed)
            }
        })
    }

    fn write_payload_preview(&self) -> Option<Result<String, String>> {
        if self.write_hex.trim().is_empty() || !self.writable {
            return None;
        }
        let byte_len = self.effective_byte_len();
        Some(
            encode_typed_write_payload(self.value_type, byte_len, &self.write_hex)
                .map(|bytes| format_hex_bytes(&bytes)),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowOperation {
    Read,
    Write,
}

impl RowOperation {
    const ALL: [Self; 2] = [Self::Read, Self::Write];

    const fn label(self) -> &'static str {
        match self {
            Self::Read => "Read",
            Self::Write => "Write",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowValueType {
    Raw,
    Ascii,
    AsciiNulTerminated,
    Reserved,
    U8,
    U16Le,
    I16Le,
    U32Le,
    I32Le,
    F32Le,
    F64Le,
    SerialChecksum,
}

impl RowValueType {
    const ALL: [Self; 12] = [
        Self::Raw,
        Self::Ascii,
        Self::AsciiNulTerminated,
        Self::Reserved,
        Self::U8,
        Self::U16Le,
        Self::I16Le,
        Self::U32Le,
        Self::I32Le,
        Self::F32Le,
        Self::F64Le,
        Self::SerialChecksum,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Raw => "RAW",
            Self::Ascii => "ASCII",
            Self::AsciiNulTerminated => "ASCII-NUL",
            Self::Reserved => "RESERVED",
            Self::U8 => "U8",
            Self::U16Le => "U16LE",
            Self::I16Le => "I16LE",
            Self::U32Le => "U32LE",
            Self::I32Le => "I32LE",
            Self::F32Le => "F32LE",
            Self::F64Le => "F64LE",
            Self::SerialChecksum => "SN checksum",
        }
    }

    const fn write_hint(self) -> &'static str {
        match self {
            Self::Raw | Self::Reserved => "hex bytes",
            Self::Ascii | Self::AsciiNulTerminated => "ASCII text",
            Self::U8 | Self::U16Le | Self::U32Le | Self::SerialChecksum => {
                "unsigned number or 0x.."
            }
            Self::I16Le | Self::I32Le => "signed number or 0x..",
            Self::F32Le | Self::F64Le => "finite number",
        }
    }

    const fn fixed_byte_len(self) -> Option<u16> {
        match self {
            Self::U8 | Self::SerialChecksum => Some(1),
            Self::U16Le | Self::I16Le => Some(2),
            Self::U32Le | Self::I32Le | Self::F32Le => Some(4),
            Self::F64Le => Some(8),
            Self::Raw | Self::Ascii | Self::AsciiNulTerminated | Self::Reserved => None,
        }
    }

    fn validate_byte_len(self, byte_len: u16) -> Result<(), String> {
        if byte_len == 0 {
            return Err("length must be at least 1 byte".to_owned());
        }
        if let Some(expected) = self.fixed_byte_len()
            && byte_len != expected
        {
            return Err(format!(
                "{} rows must use exactly {expected} byte(s), got {byte_len}",
                self.label()
            ));
        }
        Ok(())
    }

    const fn from_storage(encoding: StorageEncoding) -> Self {
        match encoding {
            StorageEncoding::Ascii => Self::Ascii,
            StorageEncoding::AsciiNulTerminated => Self::AsciiNulTerminated,
            StorageEncoding::Raw => Self::Raw,
            StorageEncoding::Reserved => Self::Reserved,
            StorageEncoding::U8 => Self::U8,
            StorageEncoding::U16Le => Self::U16Le,
            StorageEncoding::I16Le => Self::I16Le,
            StorageEncoding::U32Le => Self::U32Le,
            StorageEncoding::I32Le => Self::I32Le,
            StorageEncoding::F32Le => Self::F32Le,
            StorageEncoding::F64Le => Self::F64Le,
            StorageEncoding::SerialChecksum => Self::SerialChecksum,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowUiAction {
    Read(usize),
    Write(usize),
    Remove(usize),
}

fn render_row(
    ui: &mut egui::Ui,
    index: usize,
    row: &mut RowDraft,
    widths: &I2cToolsColumnWidths,
    selected_bus: u32,
    busy: bool,
    sftp_connected: bool,
) -> Option<RowUiAction> {
    let mut action = None;
    row.sync_byte_len_to_type();
    if !row.writable && row.default_operation == RowOperation::Write {
        row.default_operation = RowOperation::Read;
    }
    ui.add_sized(
        [widths.remark, 0.0],
        egui::TextEdit::singleline(&mut row.remark).hint_text("SNID"),
    );
    ui.allocate_ui_with_layout(
        egui::vec2(widths.address, 0.0),
        egui::Layout::left_to_right(egui::Align::Min),
        |ui| {
            ui.monospace("0x");
            ui.add_enabled(
                !busy,
                egui::DragValue::new(&mut row.address)
                    .speed(1)
                    .range(0..=0x03ff)
                    .hexadecimal(2, false, true),
            );
        },
    );
    ui.add_sized(
        [widths.offset, 0.0],
        egui::TextEdit::singleline(&mut row.offset_hex).hint_text("00 18"),
    )
    .on_hover_text("Optional register/EEPROM offset bytes prepended to Read/Write.");
    ui.allocate_ui_with_layout(
        egui::vec2(widths.length_type, 0.0),
        egui::Layout::left_to_right(egui::Align::Min),
        |ui| render_length_type_controls(ui, index, row, busy),
    );
    render_write_value_cell(ui, row, busy, widths.write_value);
    ui.add_enabled_ui(!busy, |ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(widths.flags, 0.0),
            egui::Layout::left_to_right(egui::Align::Min).with_main_wrap(true),
            |ui| render_flag_controls(ui, row),
        );
    });
    render_read_display_cell(ui, row.read_display(selected_bus), widths.read_value);
    ui.allocate_ui_with_layout(
        egui::vec2(widths.default_operation, 0.0),
        egui::Layout::left_to_right(egui::Align::Min),
        |ui| render_default_operation(ui, index, row, busy),
    );
    ui.allocate_ui_with_layout(
        egui::vec2(widths.action, 0.0),
        egui::Layout::left_to_right(egui::Align::Min),
        |ui| {
            if ui
                .add_enabled(!busy && sftp_connected, egui::Button::new("Read"))
                .on_disabled_hover_text("Connect Explorer SFTP before reading this row.")
                .clicked()
            {
                action = Some(RowUiAction::Read(index));
            }
            if ui
                .add_enabled(
                    !busy && sftp_connected && row.writable,
                    egui::Button::new("Write"),
                )
                .on_disabled_hover_text("Connect SFTP and use a writable row before writing.")
                .clicked()
            {
                action = Some(RowUiAction::Write(index));
            }
            if ui.add_enabled(!busy, egui::Button::new("Remove")).clicked() {
                action = Some(RowUiAction::Remove(index));
            }
        },
    );
    action
}

fn finite_available_width(ui: &egui::Ui) -> f32 {
    let width = ui.available_width();
    if width.is_finite() {
        width.max(1.0)
    } else {
        1200.0
    }
}

fn responsive_i2c_column_widths(available_width: f32) -> I2cToolsColumnWidths {
    let spacing = 8.0 * 8.0;
    let content = (available_width - spacing).max(1.0);
    I2cToolsColumnWidths {
        remark: content * 0.07,
        address: content * 0.05,
        offset: content * 0.06,
        length_type: content * 0.10,
        write_value: content * 0.12,
        flags: content * 0.30,
        read_value: content * 0.13,
        default_operation: content * 0.07,
        action: content * 0.10,
    }
}

fn render_row_card(
    ui: &mut egui::Ui,
    index: usize,
    row: &mut RowDraft,
    selected_bus: u32,
    busy: bool,
    sftp_connected: bool,
) -> Option<RowUiAction> {
    let mut action = None;
    row.sync_byte_len_to_type();
    if !row.writable && row.default_operation == RowOperation::Write {
        row.default_operation = RowOperation::Read;
    }
    let width = finite_available_width(ui);
    ui.add_sized(
        [width, 0.0],
        egui::TextEdit::singleline(&mut row.remark).hint_text("Remark"),
    );
    ui.horizontal_wrapped(|ui| {
        ui.label("Address");
        ui.monospace("0x");
        ui.add_enabled(
            !busy,
            egui::DragValue::new(&mut row.address)
                .speed(1)
                .range(0..=0x03ff)
                .hexadecimal(2, false, true),
        );
        ui.label("Offset");
        ui.add_sized(
            [120.0_f32.min(width), 0.0],
            egui::TextEdit::singleline(&mut row.offset_hex).hint_text("00 18"),
        );
    });
    ui.horizontal_wrapped(|ui| render_length_type_controls(ui, index, row, busy));
    ui.label("Write value");
    render_write_value_cell(ui, row, busy, width);
    ui.label("Flags");
    ui.add_enabled_ui(!busy, |ui| {
        ui.horizontal_wrapped(|ui| render_flag_controls(ui, row))
    });
    ui.label("Read value");
    render_read_display_cell(ui, row.read_display(selected_bus), width);
    ui.horizontal_wrapped(|ui| {
        render_default_operation(ui, index, row, busy);
        if ui
            .add_enabled(!busy && sftp_connected, egui::Button::new("Read"))
            .on_disabled_hover_text("Connect Explorer SFTP before reading this row.")
            .clicked()
        {
            action = Some(RowUiAction::Read(index));
        }
        if ui
            .add_enabled(
                !busy && sftp_connected && row.writable,
                egui::Button::new("Write"),
            )
            .on_disabled_hover_text("Connect SFTP and use a writable row before writing.")
            .clicked()
        {
            action = Some(RowUiAction::Write(index));
        }
        if ui.add_enabled(!busy, egui::Button::new("Remove")).clicked() {
            action = Some(RowUiAction::Remove(index));
        }
    });
    action
}

fn render_length_type_controls(ui: &mut egui::Ui, index: usize, row: &mut RowDraft, busy: bool) {
    let fixed_len = row.value_type.fixed_byte_len();
    ui.add_enabled(
        !busy && fixed_len.is_none(),
        egui::DragValue::new(&mut row.byte_len).range(1..=I2C_HELPER_MAX_MESSAGE_BYTES as u16),
    );
    let previous_type = row.value_type;
    ui.add_enabled_ui(!busy, |ui| {
        egui::ComboBox::from_id_salt(format!("i2c_tools_type_{index}"))
            .selected_text(row.value_type.label())
            .show_ui(ui, |ui| {
                for value_type in RowValueType::ALL {
                    ui.selectable_value(&mut row.value_type, value_type, value_type.label());
                }
            });
    });
    if row.value_type != previous_type {
        row.sync_byte_len_to_type();
    }
}

fn render_write_value_cell(ui: &mut egui::Ui, row: &mut RowDraft, busy: bool, width: f32) {
    ui.vertical(|ui| {
        ui.add_enabled(
            !busy && row.writable,
            egui::TextEdit::singleline(&mut row.write_hex)
                .desired_width(width)
                .hint_text(row.value_type.write_hint()),
        )
        .on_disabled_hover_text("Config marks this row read-only.");
        if let Some(preview) = row.write_payload_preview() {
            match preview {
                Ok(hex) => {
                    ui.add(egui::Label::new(format!("hex: {hex}")).wrap())
                        .on_hover_text(hex);
                }
                Err(error) => {
                    ui.colored_label(egui::Color32::YELLOW, format!("invalid: {error}"));
                }
            }
        }
    });
}

fn render_flag_controls(ui: &mut egui::Ui, row: &mut RowDraft) {
    flag_checkbox(ui, &mut row.flags, I2cMessageFlag::TenBitAddress, "TenBit");
    flag_checkbox(ui, &mut row.flags, I2cMessageFlag::Stop, "Stop");
    flag_checkbox(ui, &mut row.flags, I2cMessageFlag::NoStart, "NoStart");
    flag_checkbox(ui, &mut row.flags, I2cMessageFlag::IgnoreNack, "IgnoreNack");
    flag_checkbox(ui, &mut row.flags, I2cMessageFlag::IgnoreAck, "IgnoreAck");
}

fn render_read_display_cell(ui: &mut egui::Ui, display: Option<RowReadDisplay>, width: f32) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, 0.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| match display {
            Some(display) => {
                ui.weak("Raw hex");
                ui.add(egui::Label::new(display.raw_hex.as_str()).wrap())
                    .on_hover_text(display.raw_hex.as_str());
                ui.weak("Parsed value");
                ui.add(egui::Label::new(display.parsed.as_str()).wrap())
                    .on_hover_text(display.parsed.as_str());
            }
            None => {
                ui.weak("Raw hex");
                ui.label("—").on_hover_text("No matching read result yet");
                ui.weak("Parsed value");
                ui.label("—").on_hover_text("No matching read result yet");
            }
        },
    );
}

fn render_default_operation(ui: &mut egui::Ui, index: usize, row: &mut RowDraft, busy: bool) {
    ui.add_enabled_ui(!busy, |ui| {
        egui::ComboBox::from_id_salt(format!("i2c_tools_op_{index}"))
            .selected_text(row.default_operation.label())
            .show_ui(ui, |ui| {
                for operation in RowOperation::ALL {
                    let enabled = operation == RowOperation::Read || row.writable;
                    ui.add_enabled_ui(enabled, |ui| {
                        ui.selectable_value(
                            &mut row.default_operation,
                            operation,
                            operation.label(),
                        );
                    });
                }
            });
    });
}

fn flag_checkbox(
    ui: &mut egui::Ui,
    flags: &mut Vec<I2cMessageFlag>,
    flag: I2cMessageFlag,
    label: &'static str,
) {
    let mut enabled = flags.contains(&flag);
    if ui.checkbox(&mut enabled, label).changed() {
        if enabled {
            if !flags.contains(&flag) {
                flags.push(flag);
            }
        } else {
            flags.retain(|candidate| *candidate != flag);
        }
    }
}

fn render_message_result_row(ui: &mut egui::Ui, index: usize, message: &I2cMessageResult) {
    ui.label(index.to_string());
    ui.monospace(format!("0x{:x}", message.address));
    ui.label(match message.direction {
        I2cMessageDirection::Write => "write",
        I2cMessageDirection::Read => "read",
    });
    ui.label(message.byte_len.to_string());
    if message.bytes.is_empty() {
        ui.weak("—");
    } else {
        ui.monospace(format_hex_bytes(&message.bytes));
    }
    ui.end_row();
}

fn read_value_from_transaction(transaction: &I2cTransactionResult) -> Option<&[u8]> {
    transaction
        .messages
        .iter()
        .rev()
        .find(|message| message.direction == I2cMessageDirection::Read)
        .map(|message| message.bytes.as_slice())
}

#[cfg(test)]
fn eeprom_field_read_row(_bus: u32, map: &CalibrationStorageMap, field: &StorageField) -> RowDraft {
    let mut value_type = RowValueType::from_storage(field.encoding);
    if value_type
        .fixed_byte_len()
        .is_some_and(|expected| expected != field.byte_len)
    {
        // 旧 YgStereo 聚合字段仍保持一行显示；基础类型只绑定 primitive 宽度。
        value_type = RowValueType::Raw;
    }
    RowDraft {
        remark: field.remark.to_owned(),
        address: map.transport.i2c_address.into(),
        offset_hex: format_hex_bytes(&eeprom_offset_bytes(
            field.offset,
            map.transport.address_width_bits,
        )),
        byte_len: field.byte_len,
        value_type,
        write_hex: String::new(),
        flags: Vec::new(),
        read_value_raw: None,
        default_operation: RowOperation::Read,
        writable: field.full_provision_writable || field.update_writable,
        eeprom_address_width_bits: Some(map.transport.address_width_bits),
        eeprom_page_size: Some(map.transport.page_size_bytes),
        eeprom_write_cycle_ms: Some(map.transport.write_cycle_ms),
    }
}

fn compiled_field_read_row(
    _bus: u32,
    config: &CompiledEepromMapConfig,
    field: &CompiledEepromMapField,
) -> RowDraft {
    let mut value_type = RowValueType::from_storage(field.encoding);
    if value_type
        .fixed_byte_len()
        .is_some_and(|expected| expected != field.byte_len)
    {
        // 配置可保留既有聚合字段；UI 基础类型仍只绑定 primitive 宽度。
        value_type = RowValueType::Raw;
    }
    RowDraft {
        remark: field.remark.clone(),
        address: config.transport.i2c_address.into(),
        offset_hex: format_hex_bytes(&eeprom_offset_bytes(
            field.offset,
            config.transport.address_width_bits,
        )),
        byte_len: field.byte_len,
        value_type,
        write_hex: String::new(),
        flags: Vec::new(),
        read_value_raw: None,
        default_operation: RowOperation::Read,
        writable: field.writable,
        eeprom_address_width_bits: Some(config.transport.address_width_bits),
        eeprom_page_size: Some(config.transport.page_size_bytes),
        eeprom_write_cycle_ms: Some(config.transport.write_cycle_ms),
    }
}

fn eeprom_offset_bytes(offset: u16, address_width_bits: u8) -> Vec<u8> {
    match address_width_bits {
        8 => vec![offset as u8],
        16 => offset.to_be_bytes().to_vec(),
        _ => offset.to_be_bytes().to_vec(),
    }
}

fn parse_eeprom_offset(input: &str, address_width_bits: u8) -> Result<u16, String> {
    let bytes = parse_optional_hex_bytes(input)?;
    match address_width_bits {
        8 if bytes.len() == 1 => Ok(u16::from(bytes[0])),
        16 if bytes.len() == 2 => Ok(u16::from_be_bytes([bytes[0], bytes[1]])),
        8 | 16 => Err(format!(
            "EEPROM offset must contain exactly {} byte(s) for a {address_width_bits}-bit address",
            usize::from(address_width_bits / 8)
        )),
        _ => Err(format!(
            "unsupported EEPROM address width {address_width_bits} bits"
        )),
    }
}

fn encode_typed_write_payload(
    value_type: RowValueType,
    byte_len: u16,
    input: &str,
) -> Result<Vec<u8>, String> {
    value_type.validate_byte_len(byte_len)?;
    match value_type {
        RowValueType::Raw => parse_exact_hex_payload(input, byte_len),
        RowValueType::Reserved => Err("reserved rows are read-only".to_owned()),
        RowValueType::Ascii => encode_ascii_payload(input, byte_len, false),
        RowValueType::AsciiNulTerminated => encode_ascii_payload(input, byte_len, true),
        RowValueType::U8 | RowValueType::SerialChecksum => {
            let value = parse_u64_value(input, value_type.label())?;
            let value = u8::try_from(value).map_err(|_| {
                format!(
                    "{} value must fit in 1 byte, got {value}",
                    value_type.label()
                )
            })?;
            Ok(vec![value])
        }
        RowValueType::U16Le => {
            let value = parse_u64_value(input, value_type.label())?;
            let value = u16::try_from(value)
                .map_err(|_| format!("U16LE value must fit in 2 bytes, got {value}"))?;
            Ok(value.to_le_bytes().to_vec())
        }
        RowValueType::I16Le => {
            let value = parse_i64_value(input, value_type.label())?;
            let value = i16::try_from(value)
                .map_err(|_| format!("I16LE value must fit in 2 signed bytes, got {value}"))?;
            Ok(value.to_le_bytes().to_vec())
        }
        RowValueType::U32Le => {
            let value = parse_u64_value(input, value_type.label())?;
            let value = u32::try_from(value)
                .map_err(|_| format!("U32LE value must fit in 4 bytes, got {value}"))?;
            Ok(value.to_le_bytes().to_vec())
        }
        RowValueType::I32Le => {
            let value = parse_i64_value(input, value_type.label())?;
            let value = i32::try_from(value)
                .map_err(|_| format!("I32LE value must fit in 4 signed bytes, got {value}"))?;
            Ok(value.to_le_bytes().to_vec())
        }
        RowValueType::F32Le => {
            let value = parse_f64_value(input, value_type.label())?;
            if value.abs() > f64::from(f32::MAX) {
                return Err(format!("f32 LE value is out of range: {value}"));
            }
            Ok((value as f32).to_le_bytes().to_vec())
        }
        RowValueType::F64Le => {
            let value = parse_f64_value(input, value_type.label())?;
            Ok(value.to_le_bytes().to_vec())
        }
    }
}

fn parse_exact_hex_payload(input: &str, byte_len: u16) -> Result<Vec<u8>, String> {
    let bytes = parse_hex_bytes(input)?;
    if bytes.len() != usize::from(byte_len) {
        return Err(format!(
            "raw payload must contain exactly {byte_len} byte(s), got {}",
            bytes.len()
        ));
    }
    Ok(bytes)
}

fn decode_ascii_escapes(input: &str) -> Result<Vec<u8>, String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte != b'\\' {
            decoded.push(byte);
            index += 1;
            continue;
        }
        index += 1;
        let Some(escape) = bytes.get(index).copied() else {
            return Err("truncated ASCII escape after backslash".to_owned());
        };
        match escape {
            b'\\' => decoded.push(b'\\'),
            b'n' => decoded.push(b'\n'),
            b'r' => decoded.push(b'\r'),
            b't' => decoded.push(b'\t'),
            b'0' => decoded.push(0),
            b'x' => {
                let hi = *bytes
                    .get(index + 1)
                    .ok_or_else(|| "truncated ASCII hex escape; expected \\xNN".to_owned())?;
                let lo = *bytes
                    .get(index + 2)
                    .ok_or_else(|| "truncated ASCII hex escape; expected \\xNN".to_owned())?;
                let hi = hex_nibble(hi).ok_or_else(|| {
                    format!("invalid ASCII hex escape digit: {:?}", char::from(hi))
                })?;
                let lo = hex_nibble(lo).ok_or_else(|| {
                    format!("invalid ASCII hex escape digit: {:?}", char::from(lo))
                })?;
                decoded.push((hi << 4) | lo);
                index += 2;
            }
            _ => {
                return Err(format!(
                    "unsupported ASCII escape: \\{}",
                    char::from(escape)
                ));
            }
        }
        index += 1;
    }
    Ok(decoded)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn encode_ascii_payload(
    input: &str,
    byte_len: u16,
    nul_terminated: bool,
) -> Result<Vec<u8>, String> {
    let bytes = decode_ascii_escapes(input)?;
    if !bytes.is_ascii() {
        return Err("ASCII payload escapes must resolve to ASCII bytes only".to_owned());
    }
    let byte_len = usize::from(byte_len);
    if nul_terminated {
        if bytes.len() >= byte_len {
            return Err(format!(
                "ASCII NUL payload must leave room for a terminator: capacity {byte_len}, got {} byte(s)",
                bytes.len()
            ));
        }
        let mut encoded = vec![0_u8; byte_len];
        encoded[..bytes.len()].copy_from_slice(&bytes);
        Ok(encoded)
    } else if bytes.len() == byte_len {
        Ok(bytes)
    } else {
        Err(format!(
            "ASCII payload must contain exactly {byte_len} byte(s), got {}",
            bytes.len()
        ))
    }
}

fn parse_u64_value(input: &str, label: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(format!("{label} value is empty"));
    }
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16)
            .map_err(|error| format!("invalid {label} hex value '{trimmed}': {error}"));
    }
    trimmed
        .parse::<u64>()
        .map_err(|error| format!("invalid {label} value '{trimmed}': {error}"))
}

fn parse_i64_value(input: &str, label: &str) -> Result<i64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(format!("{label} value is empty"));
    }
    if let Some(hex) = trimmed
        .strip_prefix("-0x")
        .or_else(|| trimmed.strip_prefix("-0X"))
    {
        let value = i64::from_str_radix(hex, 16)
            .map_err(|error| format!("invalid {label} hex value '{trimmed}': {error}"))?;
        return value
            .checked_neg()
            .ok_or_else(|| format!("{label} value is out of range: {trimmed}"));
    }
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return i64::from_str_radix(hex, 16)
            .map_err(|error| format!("invalid {label} hex value '{trimmed}': {error}"));
    }
    trimmed
        .parse::<i64>()
        .map_err(|error| format!("invalid {label} value '{trimmed}': {error}"))
}

fn parse_f64_value(input: &str, label: &str) -> Result<f64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(format!("{label} value is empty"));
    }
    let value = trimmed
        .parse::<f64>()
        .map_err(|error| format!("invalid {label} value '{trimmed}': {error}"))?;
    if !value.is_finite() {
        return Err(format!("{label} value must be finite"));
    }
    Ok(value)
}

fn format_typed_read_display(
    value_type: RowValueType,
    byte_len: u16,
    bytes: &[u8],
) -> RowReadDisplay {
    let raw_hex = format_hex_bytes(bytes);
    let parsed = if matches!(value_type, RowValueType::Raw | RowValueType::Reserved) {
        "—".to_owned()
    } else if bytes.len() != usize::from(byte_len) {
        format!(
            "length mismatch: expected {byte_len} B, got {} B",
            bytes.len()
        )
    } else {
        match value_type {
            RowValueType::Raw | RowValueType::Reserved => {
                unreachable!("raw/reserved returned above")
            }
            RowValueType::Ascii => format!("\"{}\"", escape_ascii_bytes(bytes)),
            RowValueType::AsciiNulTerminated => format_ascii_nul_read_value(bytes),
            RowValueType::U8 | RowValueType::SerialChecksum => bytes[0].to_string(),
            RowValueType::U16Le => {
                let mut array = [0_u8; 2];
                array.copy_from_slice(bytes);
                u16::from_le_bytes(array).to_string()
            }
            RowValueType::I16Le => {
                let mut array = [0_u8; 2];
                array.copy_from_slice(bytes);
                i16::from_le_bytes(array).to_string()
            }
            RowValueType::U32Le => {
                let mut array = [0_u8; 4];
                array.copy_from_slice(bytes);
                u32::from_le_bytes(array).to_string()
            }
            RowValueType::I32Le => {
                let mut array = [0_u8; 4];
                array.copy_from_slice(bytes);
                i32::from_le_bytes(array).to_string()
            }
            RowValueType::F32Le => {
                let mut array = [0_u8; 4];
                array.copy_from_slice(bytes);
                f32::from_le_bytes(array).to_string()
            }
            RowValueType::F64Le => {
                let mut array = [0_u8; 8];
                array.copy_from_slice(bytes);
                f64::from_le_bytes(array).to_string()
            }
        }
    };
    RowReadDisplay { raw_hex, parsed }
}

#[cfg(test)]
fn format_typed_read_value(value_type: RowValueType, byte_len: u16, bytes: &[u8]) -> String {
    let display = format_typed_read_display(value_type, byte_len, bytes);
    if display.parsed == "—" {
        display.raw_hex
    } else {
        format!("{} | {}", display.raw_hex, display.parsed)
    }
}

fn format_ascii_nul_read_value(bytes: &[u8]) -> String {
    match bytes.iter().position(|byte| *byte == 0) {
        Some(nul_index) => format!("\"{}\"", escape_ascii_bytes(&bytes[..nul_index])),
        None => format!("\"{}\" (missing NUL)", escape_ascii_bytes(bytes)),
    }
}

fn escape_ascii_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect()
}

fn format_bus_label(bus: &I2cBusInfo) -> String {
    let detail = bus.name.as_deref().unwrap_or(bus.dev_path.as_str());
    if bus.dev_node_exists {
        format!("i2c-{} — {}", bus.bus, detail)
    } else {
        format!("i2c-{} — {} (missing)", bus.bus, detail)
    }
}

fn parse_optional_hex_bytes(input: &str) -> Result<Vec<u8>, String> {
    if input.trim().is_empty() {
        return Ok(Vec::new());
    }
    parse_hex_bytes(input)
}

fn parse_hex_bytes(input: &str) -> Result<Vec<u8>, String> {
    let compact = input
        .chars()
        .filter(|character| {
            !character.is_ascii_whitespace() && !matches!(character, ',' | ';' | '_')
        })
        .collect::<String>();
    if compact.is_empty() {
        return Err("write payload must contain at least one byte".to_owned());
    }
    if compact.contains("0x") || compact.contains("0X") {
        return input
            .split(|character: char| {
                character.is_ascii_whitespace() || matches!(character, ',' | ';')
            })
            .filter(|token| !token.trim().is_empty())
            .map(|token| parse_hex_byte_token(token.trim()))
            .collect();
    }
    if !compact
        .chars()
        .all(|character| character.is_ascii_hexdigit())
    {
        return Err("hex payload must contain only ASCII hex digits and separators".to_owned());
    }
    if compact.len() % 2 != 0 {
        return Err("hex payload must contain an even number of digits".to_owned());
    }
    (0..compact.len())
        .step_by(2)
        .map(|index| parse_hex_byte_token(&compact[index..index + 2]))
        .collect()
}

fn parse_hex_byte_token(token: &str) -> Result<u8, String> {
    let token = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
        .unwrap_or(token);
    if token.is_empty() {
        return Err("hex byte must contain 1 or 2 ASCII hex digits".to_owned());
    }
    if !token.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err(format!(
            "hex byte '{token}' must contain only ASCII hex digits"
        ));
    }
    if token.len() > 2 {
        return Err(format!("hex byte '{token}' must contain 1 or 2 digits"));
    }
    u8::from_str_radix(token, 16).map_err(|error| format!("invalid hex byte '{token}': {error}"))
}

fn format_hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_workspace_for_test(
        context: &egui::Context,
        workspace: &mut I2cToolsWorkspace,
        viewport: egui::Vec2,
    ) -> egui::FullOutput {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
            ..Default::default()
        };
        context.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let _ = workspace.render(ui, Ok("root@camera.local:22"));
            });
        })
    }

    fn accesskit_bounds(output: &egui::FullOutput, label: &str) -> egui::accesskit::Rect {
        output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("accessibility tree is enabled")
            .nodes
            .iter()
            .find_map(|(_, node)| {
                (node.label() == Some(label) || node.value() == Some(label))
                    .then(|| node.bounds())
                    .flatten()
            })
            .unwrap_or_else(|| panic!("accessibility node {label:?} is visible"))
    }

    fn accesskit_bounds_containing(output: &egui::FullOutput, text: &str) -> egui::accesskit::Rect {
        output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("accessibility tree is enabled")
            .nodes
            .iter()
            .find_map(|(_, node)| {
                let matches = node
                    .label()
                    .or_else(|| node.value())
                    .is_some_and(|label| label.contains(text));
                matches.then(|| node.bounds()).flatten()
            })
            .unwrap_or_else(|| panic!("accessibility node containing {text:?} is visible"))
    }

    fn assert_accesskit_text_in_viewport(
        output: &egui::FullOutput,
        text: &str,
        viewport: egui::Vec2,
    ) {
        let bounds = accesskit_bounds_containing(output, text);
        assert!(
            bounds.x0 >= 0.0
                && bounds.y0 >= 0.0
                && bounds.x1 <= f64::from(viewport.x) + 0.5
                && bounds.y1 <= f64::from(viewport.y) + 0.5,
            "{text} should stay inside {viewport:?}, bounds {bounds:?}"
        );
    }

    fn assert_accesskit_label_in_viewport(
        output: &egui::FullOutput,
        label: &str,
        viewport: egui::Vec2,
    ) {
        let bounds = accesskit_bounds(output, label);
        assert!(
            bounds.x0 >= 0.0
                && bounds.y0 >= 0.0
                && bounds.x1 <= f64::from(viewport.x) + 0.5
                && bounds.y1 <= f64::from(viewport.y) + 0.5,
            "{label} should stay inside {viewport:?}, bounds {bounds:?}"
        );
    }

    fn assert_accesskit_labels_share_row(output: &egui::FullOutput, labels: &[&str]) {
        let bounds = labels
            .iter()
            .map(|label| (*label, accesskit_bounds(output, label)))
            .collect::<Vec<_>>();
        let min_y = bounds
            .iter()
            .map(|(_, bounds)| bounds.y0)
            .fold(f64::INFINITY, f64::min);
        let max_y = bounds
            .iter()
            .map(|(_, bounds)| bounds.y0)
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(
            (max_y - min_y) <= 1.0,
            "labels should share one row, got {bounds:?}"
        );
    }

    fn crowded_workspace() -> I2cToolsWorkspace {
        let entries = (0..32)
            .map(|index| {
                let mut row = RowDraft::read_template(8, 0x50);
                row.remark = format!("row{index}");
                row.offset_hex = format!("{index:04x}");
                row
            })
            .collect::<Vec<_>>();
        I2cToolsWorkspace {
            entries,
            ..Default::default()
        }
    }

    fn valid_read_transfer() -> Vec<I2cTransactionSpec> {
        vec![I2cTransactionSpec {
            bus: 8,
            messages: vec![I2cMessageSpec {
                address: 0x50,
                flags: Vec::new(),
                data: I2cMessageData::Read { byte_len: 1 },
            }],
            settle_ms: None,
        }]
    }

    #[test]
    fn execute_disabled_reason_follows_user_priority() {
        let transfer = valid_read_transfer();

        let disconnected = I2cToolsWorkspace::execute_disabled_reason(
            true,
            Err("no active Explorer SFTP connection"),
            Err("row 0: invalid draft"),
        )
        .unwrap();
        assert!(disconnected.starts_with("Connect Explorer SFTP first"));

        let invalid_draft = I2cToolsWorkspace::execute_disabled_reason(
            true,
            Ok("root@camera.local:22"),
            Err("row 0 message 0: address 0x00 is reserved"),
        )
        .unwrap();
        assert!(invalid_draft.starts_with("Fix the draft before executing"));

        let busy = I2cToolsWorkspace::execute_disabled_reason(
            true,
            Ok("root@camera.local:22"),
            Ok(transfer.as_slice()),
        )
        .unwrap();
        assert!(busy.contains("already in flight"));

        assert!(
            I2cToolsWorkspace::execute_disabled_reason(
                false,
                Ok("root@camera.local:22"),
                Ok(transfer.as_slice()),
            )
            .is_none()
        );
    }

    #[test]
    fn flat_row_layout_exposes_columns_and_horizontal_flags() {
        let context = egui::Context::default();
        context.all_styles_mut(|style| style.animation_time = 0.0);
        context.enable_accesskit();
        let viewport = egui::vec2(1400.0, 420.0);
        let mut workspace = I2cToolsWorkspace {
            selected_bus: 8,
            entries: vec![RowDraft::read_template(8, 0x50)],
            ..Default::default()
        };
        workspace.entries[0].remark = "SNID".to_owned();
        workspace.entries[0].offset_hex = "01 25".to_owned();
        workspace.entries[0].byte_len = 14;
        workspace.entries[0].write_hex = "31 32".to_owned();
        workspace.entries[0].read_value_raw = Some(RowReadValue {
            snapshot: workspace.entries[0].read_request_snapshot(8),
            bytes: vec![0x31, 0x32, 0x33, 0x34],
        });

        let output = render_workspace_for_test(&context, &mut workspace, viewport);

        for label in [
            "Remark",
            "Address",
            "Offset",
            "Length / Type",
            "Write value",
            "Flags",
            "Read value",
            "Default",
            "Action",
            "All Read",
        ] {
            assert_accesskit_label_in_viewport(&output, label, viewport);
        }
        assert_accesskit_text_in_viewport(&output, "31 32 33 34", viewport);
        assert_accesskit_labels_share_row(
            &output,
            &["TenBit", "Stop", "NoStart", "IgnoreNack", "IgnoreAck"],
        );
    }

    #[test]
    fn execute_footer_stays_visible_in_short_viewport() {
        let context = egui::Context::default();
        context.all_styles_mut(|style| style.animation_time = 0.0);
        context.enable_accesskit();
        let viewport = egui::vec2(320.0, 220.0);
        let mut workspace = crowded_workspace();

        let output = render_workspace_for_test(&context, &mut workspace, viewport);

        assert_accesskit_label_in_viewport(&output, "Execute Transfer", viewport);
        assert_accesskit_label_in_viewport(&output, "Status:", viewport);
    }

    #[test]
    fn cancel_footer_stays_visible_in_short_viewport() {
        let context = egui::Context::default();
        context.all_styles_mut(|style| style.animation_time = 0.0);
        context.enable_accesskit();
        let viewport = egui::vec2(320.0, 220.0);
        let mut workspace = crowded_workspace();
        workspace.busy = true;

        let output = render_workspace_for_test(&context, &mut workspace, viewport);

        assert_accesskit_label_in_viewport(&output, "Cancel", viewport);
        assert_accesskit_label_in_viewport(&output, "Status:", viewport);
    }

    #[test]
    fn long_status_footer_stays_visible_in_short_viewport() {
        let context = egui::Context::default();
        context.all_styles_mut(|style| style.animation_time = 0.0);
        context.enable_accesskit();
        let viewport = egui::vec2(320.0, 220.0);
        let mut workspace = crowded_workspace();
        workspace.report_error(format!(
            "{} {}",
            "transport failed while executing raw transfer".repeat(4),
            "I2C helper failure details remain available via hover"
        ));

        let output = render_workspace_for_test(&context, &mut workspace, viewport);

        assert_accesskit_label_in_viewport(&output, "Status:", viewport);
        assert_accesskit_text_in_viewport(&output, "I²C operation failed", viewport);
    }

    #[test]
    fn parses_grouped_and_compact_hex() {
        assert_eq!(parse_hex_bytes("00 10 ff").unwrap(), [0x00, 0x10, 0xff]);
        assert_eq!(parse_hex_bytes("0010ff").unwrap(), [0x00, 0x10, 0xff]);
        assert_eq!(
            parse_hex_bytes("0x00,0x10,0xff").unwrap(),
            [0x00, 0x10, 0xff]
        );
        assert_eq!(parse_optional_hex_bytes("  ").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn rejects_unicode_hex_payload_without_panic() {
        for payload in ["€0", "中0", "0x€0", "0x中0"] {
            let error = parse_hex_bytes(payload).unwrap_err();

            assert!(
                error.contains("ASCII hex"),
                "payload {payload:?} should be rejected as non-ASCII hex, got {error}"
            );
        }
    }

    #[test]
    fn ascii_write_decodes_supported_escapes_only_in_ascii_modes() {
        assert_eq!(
            encode_typed_write_payload(RowValueType::Ascii, 5, "A\\n\\t\\x30\\\\").unwrap(),
            [b'A', b'\n', b'\t', b'0', b'\\']
        );
        assert_eq!(
            encode_typed_write_payload(RowValueType::AsciiNulTerminated, 6, "SN\\0A").unwrap(),
            [b'S', b'N', 0, b'A', 0, 0]
        );
        assert!(encode_typed_write_payload(RowValueType::Ascii, 2, "\\q").is_err());
        assert!(encode_typed_write_payload(RowValueType::Ascii, 2, "A\\").is_err());
        assert!(encode_typed_write_payload(RowValueType::Ascii, 2, "\\x4").is_err());
        assert!(parse_hex_bytes("41\\n").is_err());
        assert_eq!(parse_hex_bytes("0x41").unwrap(), [0x41]);
    }

    #[test]
    fn preview_counts_and_labels_include_flags() {
        let transaction = I2cTransactionSpec {
            bus: 8,
            messages: vec![
                I2cMessageSpec {
                    address: 0x50,
                    flags: vec![I2cMessageFlag::Stop],
                    data: I2cMessageData::Write {
                        bytes: (0u8..20).collect(),
                    },
                },
                I2cMessageSpec {
                    address: 0x50,
                    flags: Vec::new(),
                    data: I2cMessageData::Read { byte_len: 4 },
                },
            ],
            settle_ms: None,
        };

        assert_eq!(
            transaction_preview_counts(&[transaction.clone()]),
            (2, 20, 4)
        );
        let write_label = message_preview_label(&transaction.messages[0]);
        let read_label = message_preview_label(&transaction.messages[1]);

        assert!(write_label.contains("W 0x50 [20 B"));
        assert!(write_label.contains("(+4 B)"));
        assert!(write_label.contains("flags=Stop"));
        assert_eq!(read_label, "R 0x50 [4 B]");
    }

    #[test]
    fn row_write_builds_current_request_without_manual_unlock() {
        let workspace = I2cToolsWorkspace {
            entries: vec![RowDraft::write_template(8, 0x50)],
            ..Default::default()
        };

        let transactions = workspace.build_row_write_transfer(0).unwrap();

        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0].messages.len(), 1);
        assert_eq!(transactions[0].bus, workspace.selected_bus);
    }

    #[test]
    fn map_field_read_uses_offset_write_then_read() {
        let map = yg_stereo_p24c64g_v1();
        let transaction = eeprom_field_read_row(8, map, &map.fields[0])
            .read_transaction(8)
            .unwrap();

        assert_eq!(transaction.bus, 8);
        assert_eq!(transaction.messages.len(), 2);
        assert_eq!(
            transaction.messages[0].address,
            u16::from(map.transport.i2c_address)
        );
        assert_eq!(
            transaction.messages[0].data,
            I2cMessageData::Write { bytes: vec![0, 0] }
        );
        assert_eq!(
            transaction.messages[1].data,
            I2cMessageData::Read {
                byte_len: map.fields[0].byte_len
            }
        );
    }

    #[test]
    fn eeprom_config_rows_include_short_remark_and_byte_count() {
        let map = yg_stereo_p24c64g_v1();
        let rows = map
            .fields
            .iter()
            .map(|field| eeprom_field_read_row(8, map, field))
            .collect::<Vec<_>>();

        assert_eq!(rows[2].remark, "fx/fy/cx/cy");
        assert_eq!(rows[2].byte_len, 16);
        assert_eq!(rows[4].remark, "SNID");
        assert_eq!(rows[4].byte_len, 14);
    }

    #[test]
    fn pueo_preset_is_offered_and_populates_current_layout_rows() {
        assert!(
            EepromPresetSelection::ALL.contains(&EepromPresetSelection::BuiltIn(
                PUEO_EDU_DF9_40_PINOUT_CONFIG_NAME
            ))
        );

        let map = pueo_edu_df9_40_native_lp64_le_v1();
        assert!(
            map.fields
                .iter()
                .any(|field| field.name == "rgb_camera.fps")
        );

        let mut workspace = I2cToolsWorkspace::default();
        workspace.selected_preset =
            EepromPresetSelection::BuiltIn(PUEO_EDU_DF9_40_PINOUT_CONFIG_NAME);
        workspace.apply_selected_preset();

        assert_eq!(
            workspace
                .loaded_config
                .as_ref()
                .map(|config| config.id.as_str()),
            Some(PUEO_EDU_DF9_40_PINOUT_CONFIG_NAME)
        );
        assert!(workspace.entries.iter().any(|row| row.remark == "IMU.AB0"));
        assert!(workspace.entries.iter().any(|row| row.remark == "RGB.FPS"));
        assert!(workspace.entries.iter().any(|row| row.remark == "RGB.AE"));
    }

    #[test]
    fn preset_selection_immediately_populates_rows_and_none_clears_binding() {
        let mut workspace = I2cToolsWorkspace::default();

        workspace.selected_preset =
            EepromPresetSelection::BuiltIn(IMX219_EEPROM_CALIBRATION_CONFIG_NAME);
        workspace.apply_selected_preset();

        assert!(workspace.loaded_config.is_some());
        assert!(workspace.entries.iter().any(|row| row.remark == "SNID"));
        assert!(
            workspace
                .entries
                .iter()
                .any(|row| row.eeprom_page_size.is_some())
        );

        workspace.selected_preset = EepromPresetSelection::None;
        workspace.apply_selected_preset();

        assert!(workspace.loaded_config.is_none());
        assert!(!workspace.entries.is_empty());
        assert!(workspace.entries.iter().all(|row| row.writable));
        assert!(
            workspace
                .entries
                .iter()
                .all(|row| row.eeprom_page_size.is_none())
        );
    }

    #[test]
    fn typed_numeric_write_encodes_little_endian_payload() {
        let mut row = RowDraft::write_template(8, 0x50);
        row.value_type = RowValueType::F64Le;
        row.write_hex = "1.5".to_owned();

        let transaction = row.write_transactions(8).unwrap().remove(0);
        let I2cMessageData::Write { bytes } = &transaction.messages[0].data else {
            panic!("expected write message")
        };

        assert_eq!(bytes, &1.5_f64.to_le_bytes());
    }

    #[test]
    fn typed_integer_writes_encode_little_endian_and_check_bounds() {
        assert_eq!(
            encode_typed_write_payload(RowValueType::U16Le, 2, "0x1234").unwrap(),
            [0x34, 0x12]
        );
        assert_eq!(
            encode_typed_write_payload(RowValueType::I16Le, 2, "-2").unwrap(),
            (-2_i16).to_le_bytes()
        );
        assert_eq!(
            encode_typed_write_payload(RowValueType::I32Le, 4, "-123456").unwrap(),
            (-123_456_i32).to_le_bytes()
        );

        assert!(
            encode_typed_write_payload(RowValueType::U16Le, 2, "65536")
                .unwrap_err()
                .contains("fit in 2 bytes")
        );
        assert!(
            encode_typed_write_payload(RowValueType::I16Le, 2, "-32769")
                .unwrap_err()
                .contains("fit in 2 signed bytes")
        );
    }

    #[test]
    fn typed_integer_reads_decode_little_endian() {
        assert_eq!(
            format_typed_read_value(RowValueType::U16Le, 2, &[0x34, 0x12]),
            "34 12 | 4660"
        );
        assert_eq!(
            format_typed_read_value(RowValueType::I16Le, 2, &(-2_i16).to_le_bytes()),
            "fe ff | -2"
        );
        assert_eq!(
            format_typed_read_value(RowValueType::I32Le, 4, &(-123_456_i32).to_le_bytes()),
            "c0 1d fe ff | -123456"
        );
    }

    #[test]
    fn typed_read_keeps_hex_and_decoded_value() {
        let value = format_typed_read_value(RowValueType::F64Le, 8, &1.5_f64.to_le_bytes());

        assert!(value.starts_with("00 00 00 00 00 00 f8 3f"));
        assert!(value.ends_with("| 1.5"));
    }

    #[test]
    fn ascii_nul_write_splits_at_eeprom_page_boundary() {
        let map = baton_param_rw_native_lp64_le_v1();
        let field = map
            .fields
            .iter()
            .find(|field| field.name == "md_sn")
            .unwrap();
        let mut row = eeprom_field_read_row(4, map, field);
        row.write_hex = "MD001".to_owned();

        let transactions = row.write_transactions(4).unwrap();

        assert_eq!(transactions.len(), 2);
        assert_eq!(transactions[0].settle_ms, Some(5));
        assert_eq!(transactions[1].settle_ms, Some(5));
        let I2cMessageData::Write { bytes: first } = &transactions[0].messages[0].data else {
            panic!("expected first page write")
        };
        let I2cMessageData::Write { bytes: second } = &transactions[1].messages[0].data else {
            panic!("expected second page write")
        };
        assert_eq!(first, &[0x03, 0x19, b'M', b'D', b'0', b'0', b'1', 0, 0]);
        assert_eq!(&second[..2], &[0x03, 0x20]);
        assert_eq!(second.len(), 16);
        assert!(second[2..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn all_read_builds_one_read_transaction_per_row_on_global_bus() {
        let mut workspace = I2cToolsWorkspace {
            selected_bus: 6,
            entries: vec![
                RowDraft::read_template(8, 0x50),
                RowDraft::read_template(9, 0x51),
            ],
            ..Default::default()
        };
        workspace.entries[0].offset_hex = "00 10".to_owned();
        workspace.entries[0].byte_len = 2;
        workspace.entries[1].byte_len = 3;

        let (transactions, rows) = workspace.build_all_read_transfer().unwrap();

        assert_eq!(rows, [0, 1]);
        assert_eq!(transactions.len(), 2);
        assert_eq!(transactions[0].messages.len(), 2);
        assert_eq!(transactions[1].messages.len(), 1);
        assert_eq!(transactions[0].bus, 6);
        assert_eq!(transactions[1].bus, 6);
    }

    #[test]
    fn read_result_is_snapshot_guarded_but_type_redecodes_raw_bytes() {
        let mut workspace = I2cToolsWorkspace {
            selected_bus: 8,
            entries: vec![RowDraft::read_template(8, 0x50)],
            pending_transfer: Some(PendingTransfer::RowRead(0)),
            ..Default::default()
        };
        workspace.entries[0].byte_len = 2;

        workspace.report_transfer(vec![I2cTransactionResult {
            bus: 8,
            transferred_messages: 1,
            messages: vec![I2cMessageResult {
                address: 0x50,
                direction: I2cMessageDirection::Read,
                byte_len: 2,
                bytes: vec![0x34, 0x12],
            }],
        }]);

        assert_eq!(
            workspace.entries[0].formatted_read_value(8).as_deref(),
            Some("34 12")
        );
        workspace.entries[0].value_type = RowValueType::U16Le;
        assert_eq!(
            workspace.entries[0].formatted_read_value(8).as_deref(),
            Some("34 12 | 4660")
        );
        workspace.selected_bus = 9;
        assert!(workspace.entries[0].formatted_read_value(9).is_none());
        workspace.selected_bus = 8;
        workspace.entries[0].address = 0x51;
        assert!(workspace.entries[0].formatted_read_value(8).is_none());
    }

    #[test]
    fn generated_transfer_hits_shared_validation() {
        let mut row = RowDraft::read_template(8, 0x00);
        row.byte_len = 1;
        let spec = row.read_transaction(8).unwrap();

        let error = validate_i2c_transfer_transactions(&[spec]).unwrap_err();

        assert_eq!(error.transaction_index, Some(0));
        assert_eq!(error.message_index, Some(0));
    }

    #[test]
    fn transfer_builder_preserves_row_order_addresses_and_global_bus() {
        let workspace = I2cToolsWorkspace {
            selected_bus: 6,
            entries: vec![
                RowDraft::read_template(8, 0x50),
                RowDraft::read_template(9, 0x51),
            ],
            ..Default::default()
        };
        let spec = workspace.build_transfer().unwrap();

        assert_eq!(spec[0].bus, 6);
        assert_eq!(spec[1].bus, 6);
        assert_eq!(spec[0].messages[0].address, 0x50);
        assert_eq!(spec[1].messages[0].address, 0x51);
    }
}
