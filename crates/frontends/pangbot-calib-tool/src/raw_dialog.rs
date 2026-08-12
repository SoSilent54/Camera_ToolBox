//! Open RAW 参数窗口与候选确认交互。

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
    time::Duration,
};

use anyhow::{Result, anyhow};
use camera_toolbox_adapters::filesystem::LocalFileSystem;
use camera_toolbox_app::{
    FileRef, FileSourceId, FsCancellation, FsControl, LocalRawAnalyzeRequest, RawOpenPipeline,
    SourcePath,
};
use camera_toolbox_core::{
    BayerPattern, RawContainer, RawEncoding, RawEndian, RawProbeCandidate, RawProbeReport, RawSpec,
    Roi,
};
use eframe::egui;

const RAW_OPEN_INITIAL_SIZE: [f32; 2] = [640.0, 320.0];
const RAW_OPEN_MIN_SIZE: [f32; 2] = [520.0, 280.0];

#[derive(Debug)]
enum ProbeState {
    Idle,
    Running,
    Candidates(RawProbeReport),
    Failed(String),
}

struct ProbeResult {
    request_id: u64,
    result: std::result::Result<RawProbeReport, String>,
}

struct ActiveProbe {
    request_id: u64,
    cancellation: FsCancellation,
}

pub(crate) struct RawOpenDialogState {
    visible: bool,
    path: String,
    width: String,
    height: String,
    bit_depth: String,
    bayer: BayerUi,
    storage: RawStorageUi,
    endian: EndianUi,
    probe_state: ProbeState,
    selected_preset: Option<usize>,
    error: Option<String>,
    probe_sender: Sender<ProbeResult>,
    probe_receiver: Receiver<ProbeResult>,
    active_probe: Option<ActiveProbe>,
    next_probe_request: u64,
}

impl Default for RawOpenDialogState {
    fn default() -> Self {
        let (probe_sender, probe_receiver) = mpsc::channel();
        Self {
            visible: false,
            path: String::new(),
            width: String::new(),
            height: String::new(),
            bit_depth: String::new(),
            bayer: BayerUi::Rggb,
            storage: RawStorageUi::UnpackedU16,
            endian: EndianUi::Little,
            probe_state: ProbeState::Idle,
            selected_preset: None,
            error: None,
            probe_sender,
            probe_receiver,
            active_probe: None,
            next_probe_request: 1,
        }
    }
}

impl RawOpenDialogState {
    pub(crate) fn open(&mut self, context: &egui::Context) {
        self.cancel_probe();
        *self = Self {
            visible: true,
            ..Self::default()
        };
        context.send_viewport_cmd_to(raw_open_viewport_id(), egui::ViewportCommand::CancelClose);
    }
    pub(crate) fn close(&mut self, context: &egui::Context) {
        self.cancel_probe();
        self.visible = false;
        context.send_viewport_cmd_to(raw_open_viewport_id(), egui::ViewportCommand::Close);
    }

    pub(crate) fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.visible = true;
    }

    pub(crate) fn show(
        &mut self,
        context: &egui::Context,
        pipeline: &RawOpenPipeline,
    ) -> Option<LocalRawAnalyzeRequest> {
        if !self.visible {
            return None;
        }
        self.poll_probe_results();

        let viewport_id = raw_open_viewport_id();
        let builder = egui::ViewportBuilder::default()
            .with_title("Open RAW")
            .with_inner_size(RAW_OPEN_INITIAL_SIZE)
            .with_min_inner_size(RAW_OPEN_MIN_SIZE)
            .with_resizable(true);
        let mut should_probe = false;
        let mut should_open = false;
        let mut should_close = false;
        context.show_viewport_immediate(viewport_id, builder, |ui, _viewport_class| {
            if ui.input(|input| input.viewport().close_requested()) {
                should_close = true;
                return;
            }
            egui::Panel::bottom("raw_open_actions").show(ui, |ui| {
                Self::render_actions(ui, &mut should_open, &mut should_close);
            });
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.render_content(ui, &mut should_probe);
                });
            });
        });

        if should_close {
            self.close(context);
            return None;
        }
        if should_probe {
            self.start_probe(pipeline, context);
        }
        if !should_open {
            return None;
        }

        match self.to_request() {
            Ok(request) => Some(request),
            Err(error) => {
                self.error = Some(error.to_string());
                None
            }
        }
    }

    fn render_content(&mut self, ui: &mut egui::Ui, should_probe: &mut bool) {
        *should_probe |= self.render_path_row(ui);
        self.render_presets(ui);

        ui.separator();
        self.render_parameter_grid(ui);

        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::RED, error);
        }
    }

    fn render_actions(ui: &mut egui::Ui, should_open: &mut bool, should_close: &mut bool) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Open").clicked() {
                *should_open = true;
            }
            if ui.button("Cancel").clicked() {
                *should_close = true;
            }
        });
    }

    fn render_path_row(&mut self, ui: &mut egui::Ui) -> bool {
        let mut should_probe = false;
        ui.horizontal(|ui| {
            ui.label("File Path");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Select").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_title("Select RAW File")
                        .add_filter("RAW", &["raw", "bin"])
                        .pick_file()
                {
                    self.set_path(&path);
                    should_probe = true;
                }

                let response = ui.add_sized(
                    [ui.available_width(), ui.spacing().interact_size.y],
                    egui::TextEdit::singleline(&mut self.path)
                        .hint_text("Select or enter a RAW file path"),
                );
                if response.changed() {
                    self.invalidate_probe();
                }
                if response.lost_focus() && !self.path.trim().is_empty() {
                    should_probe = true;
                }
            });
        });
        should_probe
    }

    fn render_parameter_grid(&mut self, ui: &mut egui::Ui) {
        let mut parameters_changed = false;
        egui::Grid::new("raw_open_grid")
            .num_columns(2)
            .spacing(egui::vec2(12.0, 8.0))
            .show(ui, |ui| {
                ui.label("Width");
                parameters_changed |= ui
                    .add(egui::TextEdit::singleline(&mut self.width).id_source("raw_width_input"))
                    .changed();
                ui.end_row();

                ui.label("Height");
                parameters_changed |= ui
                    .add(egui::TextEdit::singleline(&mut self.height).id_source("raw_height_input"))
                    .changed();
                ui.end_row();

                ui.label("Effective bit depth");
                parameters_changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut self.bit_depth)
                            .id_source("raw_bit_depth_input"),
                    )
                    .changed();
                ui.end_row();

                ui.label("Bayer");
                egui::ComboBox::from_id_salt("raw_bayer")
                    .selected_text(self.bayer.label())
                    .show_ui(ui, |ui| {
                        for bayer in BayerUi::ALL {
                            ui.selectable_value(&mut self.bayer, bayer, bayer.label());
                        }
                    });
                ui.end_row();

                ui.label("Storage container");
                egui::ComboBox::from_id_salt("raw_storage")
                    .selected_text(self.storage.label())
                    .show_ui(ui, |ui| {
                        parameters_changed |= ui
                            .selectable_value(
                                &mut self.storage,
                                RawStorageUi::UnpackedU16,
                                RawStorageUi::UnpackedU16.label(),
                            )
                            .changed();
                    });
                ui.end_row();

                ui.label("Endian");
                egui::ComboBox::from_id_salt("raw_endian")
                    .selected_text(self.endian.label())
                    .show_ui(ui, |ui| {
                        parameters_changed |= ui
                            .selectable_value(
                                &mut self.endian,
                                EndianUi::Little,
                                EndianUi::Little.label(),
                            )
                            .changed();
                        parameters_changed |= ui
                            .selectable_value(
                                &mut self.endian,
                                EndianUi::Big,
                                EndianUi::Big.label(),
                            )
                            .changed();
                    });
                ui.end_row();
            });
        if parameters_changed {
            self.select_preset(None);
        }
    }

    fn render_presets(&mut self, ui: &mut egui::Ui) {
        let report = match &self.probe_state {
            ProbeState::Candidates(report) => Some(report),
            ProbeState::Idle | ProbeState::Running | ProbeState::Failed(_) => None,
        };
        let mut selected = self.selected_preset;
        let selected_text = selected
            .and_then(|index| report?.candidates.get(index))
            .map_or_else(|| "Custom".to_owned(), candidate_label);

        ui.horizontal(|ui| {
            ui.label("Preset");
            egui::ComboBox::from_id_salt("raw_presets")
                .width(ui.available_width())
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut selected, None, "Custom");
                    if let Some(report) = report {
                        for (index, candidate) in report.candidates.iter().enumerate() {
                            if candidate.is_supported() {
                                ui.selectable_value(
                                    &mut selected,
                                    Some(index),
                                    candidate_label(candidate),
                                );
                            }
                        }
                    }
                });
        });

        match &self.probe_state {
            ProbeState::Idle => {
                ui.small("选择文件后自动应用最佳 Preset；也可直接手工填写参数。");
            }
            ProbeState::Running => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("正在检查 RAW 文件...");
                });
            }
            ProbeState::Failed(message) => {
                ui.colored_label(egui::Color32::RED, message);
            }
            ProbeState::Candidates(report) => {
                for diagnostic in &report.diagnostics {
                    ui.small(diagnostic);
                }
                if let Some(candidate) = selected.and_then(|index| report.candidates.get(index)) {
                    for reason in &candidate.reasons {
                        ui.small(format!("- {reason}"));
                    }
                }
                ui.small("Preset 不推断 Bayer；请人工确认 Bayer 排列。");
            }
        }

        if selected != self.selected_preset {
            self.select_preset(selected);
        }
    }

    fn start_probe(&mut self, pipeline: &RawOpenPipeline, context: &egui::Context) {
        let path = self.path.trim().to_owned();
        if path.is_empty() {
            self.probe_state = ProbeState::Idle;
            return;
        }

        self.cancel_probe();
        let request_id = self.next_probe_request;
        self.next_probe_request = self.next_probe_request.saturating_add(1);
        let cancellation = FsCancellation::default();
        self.active_probe = Some(ActiveProbe {
            request_id,
            cancellation: cancellation.clone(),
        });
        self.probe_state = ProbeState::Running;
        self.error = None;
        let path = PathBuf::from(path);
        let pipeline = pipeline.clone();
        let sender = self.probe_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let result = local_file_source(&path).and_then(|(file_system, reference)| {
                let mut control = FsControl::with_timeout(Duration::from_secs(30));
                control.cancellation = cancellation;
                pipeline
                    .inspect(&file_system, &reference, &control)
                    .map(|session| session.report)
                    .map_err(anyhow::Error::from)
            });
            let _ = sender.send(ProbeResult {
                request_id,
                result: result.map_err(|error: anyhow::Error| error.to_string()),
            });
            context.request_repaint();
        });
    }

    fn poll_probe_results(&mut self) {
        loop {
            match self.probe_receiver.try_recv() {
                Ok(result)
                    if self
                        .active_probe
                        .as_ref()
                        .is_some_and(|active| active.request_id == result.request_id) =>
                {
                    self.active_probe = None;
                    match result.result {
                        Ok(report) => self.apply_probe_report(report),
                        Err(error) => {
                            self.selected_preset = None;
                            let message = format!("检查 RAW 失败：{error}");
                            self.probe_state = ProbeState::Failed(message.clone());
                            self.error = Some(message);
                        }
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }

    fn cancel_probe(&mut self) {
        if let Some(active) = self.active_probe.take() {
            active.cancellation.cancel();
        }
    }

    fn select_preset(&mut self, selected: Option<usize>) {
        let candidate = match &self.probe_state {
            ProbeState::Candidates(report) => selected
                .and_then(|index| report.candidates.get(index))
                .cloned(),
            ProbeState::Idle | ProbeState::Running | ProbeState::Failed(_) => None,
        };
        self.selected_preset = selected;
        if let Some(candidate) = candidate {
            self.apply_candidate(&candidate);
        }
    }

    fn apply_probe_report(&mut self, report: RawProbeReport) {
        let selected = report
            .candidates
            .iter()
            .position(RawProbeCandidate::is_supported);
        self.probe_state = ProbeState::Candidates(report);
        self.select_preset(selected);
    }

    fn set_path(&mut self, path: &Path) {
        self.path = path.to_string_lossy().into_owned();
        self.invalidate_probe();
    }

    fn invalidate_probe(&mut self) {
        self.cancel_probe();
        let had_applied_preset = self.selected_preset.is_some();
        self.probe_state = ProbeState::Idle;
        self.selected_preset = None;
        self.error = None;
        if had_applied_preset {
            self.width.clear();
            self.height.clear();
            self.bit_depth.clear();
            self.storage = RawStorageUi::UnpackedU16;
            self.endian = EndianUi::Little;
        }
    }

    fn apply_candidate(&mut self, candidate: &RawProbeCandidate) {
        self.width = candidate.width.to_string();
        self.height = candidate.height.to_string();
        self.bit_depth = candidate.effective_bit_depth.to_string();
        self.storage = match candidate.container {
            RawContainer::UnpackedU16 => RawStorageUi::UnpackedU16,
        };
        self.endian = match candidate.endian {
            RawEndian::Little => EndianUi::Little,
            RawEndian::Big => EndianUi::Big,
        };
        self.error = None;
    }

    fn to_request(&self) -> Result<LocalRawAnalyzeRequest> {
        let path = self.path.trim();
        if path.is_empty() {
            return Err(anyhow!("file path must not be empty"));
        }
        if self.endian != EndianUi::Little {
            return Err(anyhow!("only little-endian unpacked u16 RAW is supported"));
        }
        if self.storage != RawStorageUi::UnpackedU16 {
            return Err(anyhow!("only unpacked u16 RAW storage is supported"));
        }

        let width = parse_u32_field("width", &self.width)?;
        let height = parse_u32_field("height", &self.height)?;
        let bit_depth = parse_u8_field("bit depth", &self.bit_depth)?;
        Ok(LocalRawAnalyzeRequest {
            path: PathBuf::from(path),
            spec: RawSpec {
                width,
                height,
                bit_depth,
                bayer: self.bayer.into(),
            },
            encoding: RawEncoding::U16Le,
            roi: Roi {
                x: 0,
                y: 0,
                width,
                height,
            },
        })
    }
}

pub(crate) fn local_file_source(path: &Path) -> Result<(LocalFileSystem, FileRef)> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("RAW path must end in a UTF-8 file name"))?;
    let root = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_root = std::fs::canonicalize(root)?;
    let mut hasher = DefaultHasher::new();
    canonical_root.hash(&mut hasher);
    let source_id = FileSourceId::new(format!("local-{:016x}", hasher.finish()))?;
    let file_system = LocalFileSystem::new(source_id.clone(), canonical_root)?;
    let reference = FileRef::new(source_id, SourcePath::new(file_name)?);
    Ok((file_system, reference))
}

fn raw_open_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("camera-toolbox-open-raw")
}

fn candidate_label(candidate: &RawProbeCandidate) -> String {
    let endian = match candidate.endian {
        RawEndian::Little => "little-endian",
        RawEndian::Big => "big-endian",
    };
    format!(
        "{}x{}, {}-bit in uint16, {}, score {}",
        candidate.width, candidate.height, candidate.effective_bit_depth, endian, candidate.score
    )
}

fn parse_u32_field(name: &str, value: &str) -> Result<u32> {
    let parsed = value
        .trim()
        .parse::<u32>()
        .map_err(|error| anyhow!("invalid {name}: {error}"))?;
    if parsed == 0 {
        return Err(anyhow!("{name} must be > 0"));
    }
    Ok(parsed)
}

fn parse_u8_field(name: &str, value: &str) -> Result<u8> {
    let parsed = value
        .trim()
        .parse::<u8>()
        .map_err(|error| anyhow!("invalid {name}: {error}"))?;
    if !(1..=16).contains(&parsed) {
        return Err(anyhow!("{name} must be in 1..=16"));
    }
    Ok(parsed)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BayerUi {
    Rggb,
    Grbg,
    Gbrg,
    Bggr,
}

impl BayerUi {
    const ALL: [Self; 4] = [Self::Rggb, Self::Grbg, Self::Gbrg, Self::Bggr];

    const fn label(self) -> &'static str {
        match self {
            Self::Rggb => "rggb",
            Self::Grbg => "grbg",
            Self::Gbrg => "gbrg",
            Self::Bggr => "bggr",
        }
    }
}

impl From<BayerUi> for BayerPattern {
    fn from(value: BayerUi) -> Self {
        match value {
            BayerUi::Rggb => Self::Rggb,
            BayerUi::Grbg => Self::Grbg,
            BayerUi::Gbrg => Self::Gbrg,
            BayerUi::Bggr => Self::Bggr,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RawStorageUi {
    UnpackedU16,
}

impl RawStorageUi {
    const fn label(self) -> &'static str {
        match self {
            Self::UnpackedU16 => "unpacked uint16 container",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EndianUi {
    Little,
    Big,
}

impl EndianUi {
    const fn label(self) -> &'static str {
        match self {
            Self::Little => "little endian",
            Self::Big => "big endian (unsupported)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> RawProbeCandidate {
        RawProbeCandidate {
            width: 640,
            height: 480,
            effective_bit_depth: 10,
            container: RawContainer::UnpackedU16,
            endian: RawEndian::Little,
            score: 100,
            out_of_range_samples: 0,
            total_samples: 4,
            reasons: vec!["test".to_owned()],
        }
    }

    #[test]
    fn raw_open_viewport_never_targets_root_window() {
        assert_ne!(raw_open_viewport_id(), egui::ViewportId::ROOT);
    }

    #[test]
    fn opening_dialog_starts_with_editable_empty_parameters() {
        let mut dialog = RawOpenDialogState::default();
        dialog.open(&egui::Context::default());

        assert!(dialog.visible);
        assert!(dialog.path.is_empty());
        assert!(dialog.width.is_empty());
        assert!(dialog.height.is_empty());
        assert!(dialog.bit_depth.is_empty());
    }

    #[test]
    fn probe_report_automatically_applies_best_supported_preset() {
        let mut dialog = RawOpenDialogState {
            path: "frame.raw".to_owned(),
            ..RawOpenDialogState::default()
        };

        let mut unsupported = candidate();
        unsupported.width = 800;
        unsupported.height = 600;
        unsupported.endian = RawEndian::Big;
        unsupported.score = 200;

        dialog.apply_probe_report(RawProbeReport {
            file_len: 640 * 480 * 2,
            candidates: vec![unsupported, candidate()],
            diagnostics: Vec::new(),
        });

        assert_eq!(dialog.selected_preset, Some(1));
        assert_eq!(dialog.width, "640");
        assert_eq!(dialog.height, "480");
        assert_eq!(dialog.bit_depth, "10");
    }

    #[test]
    fn switching_preset_applies_values_immediately() {
        let mut second = candidate();
        second.width = 800;
        second.height = 600;
        second.effective_bit_depth = 12;
        let mut dialog = RawOpenDialogState::default();
        dialog.apply_probe_report(RawProbeReport {
            file_len: 640 * 480 * 2,
            candidates: vec![candidate(), second],
            diagnostics: Vec::new(),
        });

        dialog.select_preset(Some(1));

        assert_eq!(dialog.selected_preset, Some(1));
        assert_eq!(dialog.width, "800");
        assert_eq!(dialog.height, "600");
        assert_eq!(dialog.bit_depth, "12");
    }

    #[test]
    fn custom_keeps_last_applied_values() {
        let mut dialog = RawOpenDialogState::default();
        dialog.apply_probe_report(RawProbeReport {
            file_len: 640 * 480 * 2,
            candidates: vec![candidate()],
            diagnostics: Vec::new(),
        });

        dialog.select_preset(None);

        assert_eq!(dialog.selected_preset, None);
        assert_eq!(dialog.width, "640");
        assert_eq!(dialog.height, "480");
        assert_eq!(dialog.bit_depth, "10");
    }

    #[test]
    fn path_change_invalidates_applied_probe_values() {
        let mut dialog = RawOpenDialogState::default();
        dialog.apply_probe_report(RawProbeReport {
            file_len: 640 * 480 * 2,
            candidates: vec![candidate()],
            diagnostics: Vec::new(),
        });

        dialog.set_path(Path::new("missing.raw"));

        assert!(matches!(dialog.probe_state, ProbeState::Idle));
        assert_eq!(dialog.selected_preset, None);
        assert!(dialog.width.is_empty());
        assert!(dialog.height.is_empty());
        assert!(dialog.bit_depth.is_empty());
    }

    #[test]
    fn path_change_preserves_custom_values() {
        let mut dialog = RawOpenDialogState::default();
        dialog.apply_probe_report(RawProbeReport {
            file_len: 640 * 480 * 2,
            candidates: vec![candidate()],
            diagnostics: Vec::new(),
        });
        dialog.select_preset(None);
        dialog.width = "320".to_owned();

        dialog.set_path(Path::new("another.raw"));

        assert_eq!(dialog.selected_preset, None);
        assert_eq!(dialog.width, "320");
        assert_eq!(dialog.height, "480");
        assert_eq!(dialog.bit_depth, "10");
    }

    #[test]
    fn request_uses_tightly_packed_dimensions() {
        let dialog = RawOpenDialogState {
            path: "frame.raw".to_owned(),
            width: "4".to_owned(),
            height: "3".to_owned(),
            bit_depth: "10".to_owned(),
            ..RawOpenDialogState::default()
        };

        let request = dialog.to_request().unwrap();

        assert_eq!(request.spec.pixel_count().unwrap(), 12);
        assert_eq!(request.roi.width, 4);
        assert_eq!(request.roi.height, 3);
    }

    #[test]
    fn request_rejects_unsupported_big_endian() {
        let dialog = RawOpenDialogState {
            path: "frame.raw".to_owned(),
            endian: EndianUi::Big,
            ..RawOpenDialogState::default()
        };

        let error = dialog.to_request().unwrap_err();

        assert!(error.to_string().contains("little-endian"));
    }
}
