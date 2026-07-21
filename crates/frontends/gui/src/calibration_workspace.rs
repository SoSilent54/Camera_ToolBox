//! 同窗标定工作区；GUI 只编排 app 端口，文件读取和 OpenCV 均在后台执行。

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
    time::Duration,
};

use camera_toolbox_adapters::{ImageRasterCodec, OpenCvCalibrationBackend};
use camera_toolbox_app::{
    AddCalibrationItemOutcome, CalibrationBackend, CalibrationCancellation, CalibrationItemId,
    CalibrationItemStatus, CalibrationJobToken, CalibrationSession, CalibrationSnapshot,
    FileSystem, FsCancellation, FsControl, RasterFormat, RasterImageCodec, read_calibration_png,
};
use camera_toolbox_core::{
    BoardSpec, CalibrationImageSize, CalibrationPoint, CalibrationSolution, InitialIntrinsics,
    Rgba8Frame, ViewCalibrationResult,
};
use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::{explorer::CalibrationImportCandidate, viewer::pixel_inspection_texture_options};

const MAX_DATASET_ITEMS: usize = 256;
const MAX_ENCODED_PNG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DECODED_PREVIEW_BYTES: usize = 256 * 1024 * 1024;
const FILE_READ_TIMEOUT: Duration = Duration::from_secs(60);
const COVERAGE_WIDTH: usize = 192;
const COVERAGE_GAUSSIAN_SIGMA: f32 = 42.0 / 1920.0 * COVERAGE_WIDTH as f32;
const MIN_PREVIEW_ZOOM: f32 = 0.05;
const MAX_PREVIEW_ZOOM: f32 = 64.0;
const OBSERVED_POINT_COLOR: egui::Color32 = egui::Color32::from_rgb(120, 230, 140);
const REPROJECTED_POINT_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 96, 96);
const RMSE_TEXT_ON_FILL: egui::Color32 = egui::Color32::from_rgb(12, 32, 45);
const RMSE_TEXT_ON_TRACK: egui::Color32 = egui::Color32::WHITE;
const REPROJECTION_ARROW_WIDTH: f32 = 1.25;
const REPROJECTION_ARROW_HEAD_LENGTH: f32 = 5.0;
const REPROJECTION_ARROW_HEAD_HALF_WIDTH: f32 = 2.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CalibrationJobKind {
    Detect,
    Calibrate,
}

struct CalibrationSource {
    display_path: PathBuf,
    file_system: Arc<dyn FileSystem>,
    remote: bool,
    preview: Option<CalibrationPreview>,
}

struct CalibrationPreview {
    frame: Arc<Rgba8Frame>,
    texture: egui::TextureHandle,
}

#[derive(Default)]
struct CalibrationPreviewViewport {
    item_id: Option<CalibrationItemId>,
    zoom: f32,
    pan: egui::Vec2,
    fit_on_next_frame: bool,
}

impl CalibrationPreviewViewport {
    fn reset_for(&mut self, item_id: CalibrationItemId) {
        if self.item_id != Some(item_id) {
            self.item_id = Some(item_id);
            self.fit_on_next_frame = true;
        }
    }

    fn fit_to_rect(&mut self, rect: egui::Rect, image_size: egui::Vec2) {
        self.zoom = (rect.width() / image_size.x.max(1.0))
            .min(rect.height() / image_size.y.max(1.0))
            .clamp(MIN_PREVIEW_ZOOM, MAX_PREVIEW_ZOOM);
        self.pan = rect.center().to_vec2() - image_size * self.zoom * 0.5 - rect.min.to_vec2();
        self.fit_on_next_frame = false;
    }

    fn zoom_by(&mut self, factor: f32, anchor: egui::Pos2, viewport: egui::Rect) {
        let old_zoom = self.zoom;
        self.zoom = (self.zoom * factor).clamp(MIN_PREVIEW_ZOOM, MAX_PREVIEW_ZOOM);
        let scale = self.zoom / old_zoom;
        let local_anchor = anchor - viewport.min;
        self.pan = local_anchor + (self.pan - local_anchor) * scale;
    }

    fn interact(
        &mut self,
        ui: &egui::Ui,
        response: &egui::Response,
        viewport: egui::Rect,
        image_size: egui::Vec2,
    ) -> egui::Rect {
        if self.fit_on_next_frame || self.zoom <= 0.0 {
            self.fit_to_rect(viewport, image_size);
        }
        if response.dragged_by(egui::PointerButton::Primary) {
            self.pan += response.drag_delta();
        }
        if response.hovered() {
            let scroll_y = ui.input(|input| input.smooth_scroll_delta().y);
            if scroll_y.abs() > f32::EPSILON
                && let Some(anchor) = response.hover_pos()
            {
                self.zoom_by((scroll_y * 0.0015).exp(), anchor, viewport);
            }
        }
        egui::Rect::from_min_size(viewport.min + self.pan, image_size * self.zoom)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CalibrationPreviewMode {
    Heatmap,
    Overlay,
    #[default]
    InputImage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreviewLayers {
    input: bool,
    heatmap_alpha: Option<u8>,
}

const fn preview_layers(mode: CalibrationPreviewMode, heatmap_available: bool) -> PreviewLayers {
    match (mode, heatmap_available) {
        (CalibrationPreviewMode::Heatmap, true) => PreviewLayers {
            input: false,
            heatmap_alpha: Some(255),
        },
        (CalibrationPreviewMode::Overlay, true) => PreviewLayers {
            input: true,
            heatmap_alpha: Some(150),
        },
        (CalibrationPreviewMode::InputImage, _) | (_, false) => PreviewLayers {
            input: true,
            heatmap_alpha: None,
        },
    }
}
struct CoverageVisualization {
    density: egui::TextureHandle,
    enabled_views: usize,
}

struct ActiveCancellation {
    file_system: FsCancellation,
    calibration: CalibrationCancellation,
}

impl ActiveCancellation {
    fn cancel(&self) {
        self.file_system.cancel();
        self.calibration.cancel();
    }
}

struct DetectionCommand {
    token: CalibrationJobToken,
    file_system: Arc<dyn FileSystem>,
    reference: camera_toolbox_app::FileRef,
    file_cancellation: FsCancellation,
    calibration_cancellation: CalibrationCancellation,
}

enum WorkerCommand {
    Detect(DetectionCommand),
    Calibrate {
        snapshot: CalibrationSnapshot,
        cancellation: CalibrationCancellation,
    },
    Shutdown,
}

enum WorkerEvent {
    Detection {
        token: CalibrationJobToken,
        result: Result<DetectionProduct, String>,
    },
    Calibration {
        snapshot: CalibrationSnapshot,
        result: Result<CalibrationSolution, String>,
    },
}

struct DetectionProduct {
    source_version: camera_toolbox_app::FileVersion,
    outcome: camera_toolbox_core::ChessboardDetectionOutcome,
    preview: Option<Arc<Rgba8Frame>>,
}

struct CalibrationWorker {
    sender: Sender<WorkerCommand>,
    receiver: Receiver<WorkerEvent>,
}

impl CalibrationWorker {
    fn new(context: &egui::Context) -> std::io::Result<Self> {
        let (command_sender, command_receiver) = mpsc::channel();
        let (event_sender, event_receiver) = mpsc::channel();
        let repaint = context.clone();
        thread::Builder::new()
            .name("camera-toolbox-calibration".to_owned())
            .spawn(move || {
                let backend = OpenCvCalibrationBackend;
                let preview_codec = ImageRasterCodec;
                while let Ok(command) = command_receiver.recv() {
                    let event = match command {
                        WorkerCommand::Detect(command) => WorkerEvent::Detection {
                            token: command.token.clone(),
                            result: run_detection(&backend, &preview_codec, command),
                        },
                        WorkerCommand::Calibrate {
                            snapshot,
                            cancellation,
                        } => {
                            let result = backend
                                .calibrate(&snapshot.request, &cancellation)
                                .map_err(|error| error.to_string());
                            WorkerEvent::Calibration { snapshot, result }
                        }
                        WorkerCommand::Shutdown => break,
                    };
                    if event_sender.send(event).is_err() {
                        break;
                    }
                    repaint.request_repaint();
                }
            })?;
        Ok(Self {
            sender: command_sender,
            receiver: event_receiver,
        })
    }

    fn send(&self, command: WorkerCommand) -> Result<(), String> {
        self.sender
            .send(command)
            .map_err(|_| "calibration worker stopped".to_owned())
    }
}

impl Drop for CalibrationWorker {
    fn drop(&mut self) {
        let _ = self.sender.send(WorkerCommand::Shutdown);
    }
}

fn run_detection(
    backend: &dyn CalibrationBackend,
    preview_codec: &dyn RasterImageCodec,
    command: DetectionCommand,
) -> Result<DetectionProduct, String> {
    let mut control = FsControl::with_timeout(FILE_READ_TIMEOUT);
    control.cancellation = command.file_cancellation;
    let encoded = read_calibration_png(
        command.file_system.as_ref(),
        &command.reference,
        command.token.source_version(),
        MAX_ENCODED_PNG_BYTES,
        &control,
    )
    .map_err(|error| error.to_string())?;
    let outcome = backend
        .detect_png(
            &encoded.bytes,
            encoded.image_size,
            MAX_DECODED_PREVIEW_BYTES,
            command.token.board(),
            &command.calibration_cancellation,
        )
        .map_err(|error| error.to_string())?;
    let preview = preview_codec
        .decode_rgba8(RasterFormat::Png, &encoded.bytes, MAX_DECODED_PREVIEW_BYTES)
        .ok()
        .map(Arc::new);
    Ok(DetectionProduct {
        source_version: encoded.source_version,
        outcome,
        preview,
    })
}

pub(crate) struct CalibrationWorkspace {
    session: CalibrationSession,
    sources: HashMap<CalibrationItemId, CalibrationSource>,
    worker: CalibrationWorker,
    detect_queue: VecDeque<CalibrationItemId>,
    pending_imports: VecDeque<CalibrationImportCandidate>,
    active_job: Option<CalibrationJobKind>,
    active_cancellation: Option<ActiveCancellation>,
    cancel_requested: bool,
    status: String,
    board_cols: u16,
    board_rows: u16,
    square_size: f64,
    auto_intrinsics: bool,
    fx: f64,
    fy: f64,
    cx: f64,
    cy: f64,
    preview_viewport: CalibrationPreviewViewport,
    preview_mode: CalibrationPreviewMode,
    coverage: Option<CoverageVisualization>,
    coverage_dirty: bool,
}

impl CalibrationWorkspace {
    pub(crate) fn new(context: &egui::Context) -> std::io::Result<Self> {
        let board = BoardSpec::new(11, 8, 40.0).expect("default board is valid");
        Ok(Self {
            session: CalibrationSession::new(board),
            sources: HashMap::new(),
            worker: CalibrationWorker::new(context)?,
            detect_queue: VecDeque::new(),
            pending_imports: VecDeque::new(),
            active_job: None,
            active_cancellation: None,
            cancel_requested: false,
            status: "Add original PNG calibration images from Workspace Explorer.".to_owned(),
            board_cols: board.inner_cols,
            board_rows: board.inner_rows,
            square_size: board.square_size,
            auto_intrinsics: true,
            fx: 1000.0,
            fy: 1000.0,
            cx: 0.0,
            cy: 0.0,
            preview_viewport: CalibrationPreviewViewport::default(),
            preview_mode: CalibrationPreviewMode::default(),
            coverage: None,
            coverage_dirty: true,
        })
    }

    pub(crate) fn import(&mut self, candidates: Vec<CalibrationImportCandidate>) {
        match self.active_job {
            Some(CalibrationJobKind::Calibrate) => {
                self.status = "Cancel the active calibration before importing files.".to_owned();
            }
            Some(CalibrationJobKind::Detect) => {
                let queued = candidates.len();
                self.pending_imports.extend(candidates);
                self.status = format!("Queued {queued} image(s) for detection.");
            }
            None => self.import_candidates(candidates, true),
        }
    }

    fn import_candidates(
        &mut self,
        candidates: Vec<CalibrationImportCandidate>,
        auto_detect: bool,
    ) {
        let available = MAX_DATASET_ITEMS.saturating_sub(self.session.items().len());
        let offered = candidates.len();
        let mut added = 0_usize;
        let mut refreshed = 0_usize;
        let mut skipped = offered.saturating_sub(available);
        for candidate in candidates.into_iter().take(available) {
            let name = candidate.entry.name.as_str().to_owned();
            let outcome = self.session.add_or_refresh(
                candidate.entry.reference.clone(),
                candidate.entry.version,
                name,
            );
            let id = match outcome {
                AddCalibrationItemOutcome::Added(id) => {
                    added += 1;
                    id
                }
                AddCalibrationItemOutcome::SourceChanged(id) => {
                    refreshed += 1;
                    id
                }
                AddCalibrationItemOutcome::AlreadyPresent(id) => {
                    skipped += 1;
                    id
                }
            };
            self.sources.insert(
                id,
                CalibrationSource {
                    display_path: candidate.display_path,
                    file_system: candidate.file_system,
                    remote: candidate.remote,
                    preview: None,
                },
            );
            if auto_detect {
                self.enqueue_detection(id);
            }
        }
        if added > 0 || refreshed > 0 {
            self.coverage_dirty = true;
        }
        self.status =
            format!("Dataset updated: {added} added, {refreshed} refreshed, {skipped} unchanged.");
        if auto_detect && self.active_job.is_none() && !self.detect_queue.is_empty() {
            self.cancel_requested = false;
            self.dispatch_next_detection();
        }
    }

    fn enqueue_detection(&mut self, id: CalibrationItemId) {
        let already_active = self
            .session
            .items()
            .iter()
            .any(|item| item.id == id && item.status.is_busy());
        if !already_active && !self.detect_queue.contains(&id) {
            self.detect_queue.push_back(id);
        }
    }

    pub(crate) fn reject_import(&mut self, display_path: &std::path::Path) {
        self.status = format!(
            "Cannot add {}: PangbotCompatible calibration accepts original PNG files only.",
            display_path.display()
        );
    }

    pub(crate) fn render(&mut self, context: &egui::Context, ui: &mut egui::Ui) -> egui::Rect {
        self.poll_worker(context);
        self.sync_coverage(context);
        let rect = ui.available_rect_before_wrap();
        self.render_controls(ui);
        ui.separator();
        ui.columns(2, |columns| {
            self.render_dataset(&mut columns[0]);
            self.render_inspection(&mut columns[1]);
        });
        rect
    }

    pub(crate) fn render_status(&self, ui: &mut egui::Ui) {
        if self.active_job.is_some() {
            ui.spinner();
            ui.separator();
        }
        ui.label(&self.status);
        if let Some(installed) = self.session.installed() {
            ui.separator();
            ui.monospace(format!("RMS {:.4} px", installed.solution.rms_error));
        }
    }

    fn render_controls(&mut self, ui: &mut egui::Ui) {
        self.refresh_auto_intrinsics_fields();
        let idle = self.active_job.is_none();
        ui.horizontal_wrapped(|ui| {
            ui.heading("Intrinsic Calibration");
            ui.add_enabled_ui(idle, |ui| {
                ui.separator();
                ui.label("Inner corners");
                ui.add(egui::DragValue::new(&mut self.board_cols).range(2..=256));
                ui.label("×");
                ui.add(egui::DragValue::new(&mut self.board_rows).range(2..=256));
                ui.label("Square size (mm)");
                ui.add(
                    egui::DragValue::new(&mut self.square_size)
                        .speed(0.1)
                        .range(0.001..=1.0e6),
                );
                if ui.button("Apply board").clicked() {
                    self.apply_board();
                }
            });
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(idle, |ui| {
                ui.checkbox(&mut self.auto_intrinsics, "Auto initial intrinsics")
                    .on_hover_text("Auto: fx=fy=max(width,height), cx=width/2, cy=height/2");
                ui.label("fx");
                ui.add_enabled(
                    !self.auto_intrinsics,
                    egui::DragValue::new(&mut self.fx).speed(1.0),
                );
                ui.label("fy");
                ui.add_enabled(
                    !self.auto_intrinsics,
                    egui::DragValue::new(&mut self.fy).speed(1.0),
                );
                ui.label("cx");
                ui.add_enabled(
                    !self.auto_intrinsics,
                    egui::DragValue::new(&mut self.cx).speed(1.0),
                );
                ui.label("cy");
                ui.add_enabled(
                    !self.auto_intrinsics,
                    egui::DragValue::new(&mut self.cy).speed(1.0),
                );
            });
            ui.separator();
            if ui
                .add_enabled(
                    idle && self.session.items().iter().any(|item| item.enabled),
                    egui::Button::new("Detect"),
                )
                .clicked()
            {
                self.start_detection();
            }
            let readiness = self.initial_intrinsics().and_then(|initial| {
                self.session
                    .calibration_snapshot(initial)
                    .map_err(|error| error.to_string())
            });
            if ui
                .add_enabled(idle && readiness.is_ok(), egui::Button::new("Calibrate"))
                .on_disabled_hover_text(readiness.as_ref().err().cloned().unwrap_or_default())
                .clicked()
            {
                self.start_calibration();
            }
            if ui
                .add_enabled(self.active_job.is_some(), egui::Button::new("Cancel"))
                .clicked()
            {
                self.cancel_requested = true;
                self.detect_queue.clear();
                if let Some(cancellation) = &self.active_cancellation {
                    cancellation.cancel();
                }
                self.status =
                    "Cancel requested; waiting for the current OpenCV call boundary.".to_owned();
            }
            if ui
                .add_enabled(
                    idle && self.session.installed().is_some(),
                    egui::Button::new("Export JSON"),
                )
                .clicked()
            {
                self.export_json();
            }
        });
    }

    fn render_dataset(&mut self, ui: &mut egui::Ui) {
        ui.heading(format!("Dataset ({})", self.session.items().len()));
        let mut toggle = None;
        let mut select = None;
        let installed = self.session.installed();
        let selected = self.session.selected();
        let items = self.session.items();
        let idle = self.active_job.is_none();
        let max_rmse = installed
            .map(|installed| {
                installed
                    .solution
                    .views
                    .iter()
                    .map(|view| view.reprojection_rmse)
                    .fold(0.0_f64, f64::max)
            })
            .unwrap_or(0.0)
            .max(1e-9);

        TableBuilder::new(ui)
            .id_salt("calibration_dataset_table")
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(42.0).at_least(36.0).clip(true))
            .column(Column::initial(70.0).at_least(52.0).clip(true))
            .column(Column::initial(120.0).at_least(72.0).clip(true))
            .column(Column::initial(58.0).at_least(48.0).clip(true))
            .column(Column::initial(88.0).at_least(72.0).clip(true))
            .column(Column::initial(62.0).at_least(52.0).clip(true))
            .column(Column::initial(118.0).at_least(82.0).clip(true))
            .column(Column::initial(140.0).at_least(80.0).clip(true))
            .header(24.0, |mut header| {
                for heading in [
                    "Use",
                    "Status",
                    "Name",
                    "Source",
                    "Resolution",
                    "Corners",
                    "RMSE",
                    "Reason",
                ] {
                    header.col(|ui| {
                        ui.strong(heading);
                    });
                }
            })
            .body(|body| {
                body.rows(26.0, items.len(), |mut row| {
                    let item = &items[row.index()];
                    row.col(|ui| {
                        let mut enabled = item.enabled;
                        if ui
                            .add_enabled(idle, egui::Checkbox::new(&mut enabled, ""))
                            .changed()
                        {
                            toggle = Some((item.id, enabled));
                        }
                    });
                    row.col(|ui| {
                        let mut label = egui::RichText::new(status_label(&item.status));
                        if let Some(color) = status_color(&item.status) {
                            label = label.color(color);
                        }
                        let mut response = ui.selectable_label(selected == Some(item.id), label);
                        if let CalibrationItemStatus::Failed(reason) = &item.status {
                            response = response.on_hover_text(reason);
                        }
                        if response.clicked() {
                            select = Some(item.id);
                        }
                    });
                    row.col(|ui| {
                        if ui
                            .selectable_label(selected == Some(item.id), &item.display_name)
                            .clicked()
                        {
                            select = Some(item.id);
                        }
                    });
                    row.col(|ui| {
                        let source = self.sources.get(&item.id);
                        ui.label(if source.is_some_and(|source| source.remote) {
                            "SFTP"
                        } else {
                            "Local"
                        });
                    });
                    row.col(|ui| {
                        ui.monospace(detection_size(&item.status).map_or_else(
                            || "—".to_owned(),
                            |size| format!("{}×{}", size.width, size.height),
                        ));
                    });
                    row.col(|ui| {
                        ui.monospace(match &item.status {
                            CalibrationItemStatus::Found(detection) => {
                                detection.corners.len().to_string()
                            }
                            _ => "—".to_owned(),
                        });
                    });
                    row.col(|ui| {
                        let metric = calibration_metric(installed, item.id);
                        render_rmse_cell(ui, metric, max_rmse);
                    });
                    row.col(|ui| {
                        let reason = match &item.status {
                            CalibrationItemStatus::Failed(reason) => reason.as_str(),
                            CalibrationItemStatus::NotFound { .. } => "Chessboard not found",
                            _ => "—",
                        };
                        ui.label(reason).on_hover_text(reason);
                    });
                });
            });

        if let Some((id, enabled)) = toggle {
            if self.session.set_enabled(id, enabled).is_ok() {
                self.coverage_dirty = true;
            }
        }
        if let Some(id) = select {
            let _ = self.session.set_selected(id);
        }
        if ui
            .add_enabled(
                idle && !self.session.items().is_empty(),
                egui::Button::new("Clear"),
            )
            .clicked()
        {
            self.session.clear();
            self.sources.clear();
            self.coverage = None;
            self.preview_mode = CalibrationPreviewMode::InputImage;
            self.coverage_dirty = false;
            self.status = "Dataset cleared.".to_owned();
        }
    }

    fn render_inspection(&mut self, ui: &mut egui::Ui) {
        ui.heading("Preview and constraints");
        if let Some(id) = self.session.selected() {
            if let Some(source) = self.sources.get(&id) {
                ui.monospace(source.display_path.display().to_string());
            }
            ui.horizontal_wrapped(|ui| {
                let has_heatmap = self.coverage.is_some();
                ui.add_enabled_ui(has_heatmap, |ui| {
                    ui.selectable_value(
                        &mut self.preview_mode,
                        CalibrationPreviewMode::Heatmap,
                        "Heatmap",
                    );
                    ui.selectable_value(
                        &mut self.preview_mode,
                        CalibrationPreviewMode::Overlay,
                        "Overlay",
                    );
                });
                ui.selectable_value(
                    &mut self.preview_mode,
                    CalibrationPreviewMode::InputImage,
                    "Input image",
                );
                if let Some(coverage) = &self.coverage {
                    ui.monospace(format!("{} views", coverage.enabled_views));
                }
                ui.separator();
                ui.monospace(format!("{:.0}%", self.preview_viewport.zoom * 100.0));
                if ui.small_button("Fit").clicked() {
                    self.preview_viewport.fit_on_next_frame = true;
                }
            });
            let max_preview_height = (ui.available_height() - 170.0).max(180.0);
            let preview_height = (ui.available_width() * 9.0 / 16.0)
                .min(max_preview_height)
                .clamp(180.0, 520.0);
            let preview_size = egui::vec2(ui.available_width(), preview_height);
            let (rect, response) =
                ui.allocate_exact_size(preview_size, egui::Sense::click_and_drag());
            self.paint_preview(ui, &response, rect, id);
        } else {
            ui.weak("Select a dataset item to inspect its image and residuals.");
        }
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .id_salt("calibration_metrics")
            .show(ui, |ui| {
                render_calibration_result(ui, self.session.installed());
            });
    }

    fn paint_preview(
        &mut self,
        ui: &egui::Ui,
        response: &egui::Response,
        rect: egui::Rect,
        id: CalibrationItemId,
    ) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, egui::Color32::from_gray(20));
        let Some(source) = self.sources.get(&id) else {
            return;
        };
        let Some(preview) = &source.preview else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Run Detect to load preview",
                egui::FontId::proportional(14.0),
                egui::Color32::GRAY,
            );
            return;
        };
        let input_texture_id = preview.texture.id();
        let heatmap_texture_id = self.coverage.as_ref().map(|coverage| coverage.density.id());
        let width = preview.frame.width;
        let height = preview.frame.height;
        let image_size = egui::vec2(width as f32, height as f32);
        self.preview_viewport.reset_for(id);
        let image_rect = self
            .preview_viewport
            .interact(ui, response, rect, image_size);
        let unit_uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
        let layers = preview_layers(self.preview_mode, heatmap_texture_id.is_some());
        if layers.input {
            painter.image(input_texture_id, image_rect, unit_uv, egui::Color32::WHITE);
        }
        if let (Some(texture_id), Some(alpha)) = (heatmap_texture_id, layers.heatmap_alpha) {
            painter.image(
                texture_id,
                image_rect,
                unit_uv,
                egui::Color32::from_white_alpha(alpha),
            );
            paint_heatmap_guides(&painter, image_rect);
        }
        if !layers.input {
            return;
        }
        let Some(item) = self.session.items().iter().find(|item| item.id == id) else {
            return;
        };
        let CalibrationItemStatus::Found(detection) = &item.status else {
            return;
        };
        let map = |point| image_point_to_preview(point, image_rect, width, height);
        let projected = calibration_view(self.session.installed(), id)
            .map(|view| view.projected_points.as_slice());
        for (index, observed) in detection.corners.iter().copied().enumerate() {
            let observed_position = map(observed);
            if let Some(projected) = projected.and_then(|points| points.get(index)).copied() {
                paint_reprojection_vector(&painter, observed_position, map(projected));
            }
            painter.circle_stroke(
                observed_position,
                3.0,
                egui::Stroke::new(1.25, OBSERVED_POINT_COLOR),
            );
        }
    }

    fn apply_board(&mut self) -> bool {
        let board = match BoardSpec::new(self.board_cols, self.board_rows, self.square_size) {
            Ok(board) => board,
            Err(error) => {
                self.status = format!("Invalid board: {error}");
                return false;
            }
        };
        let previous = self.session.board();
        let corner_layout_changed =
            previous.inner_cols != board.inner_cols || previous.inner_rows != board.inner_rows;
        if let Err(error) = self.session.set_board(board) {
            self.status = format!("Invalid board: {error}");
            return false;
        }
        if corner_layout_changed {
            self.coverage_dirty = true;
            self.status =
                "Inner-corner layout applied; existing detections were invalidated.".to_owned();
        } else if previous != board {
            self.status =
                "Square size applied in mm; detections were preserved and calibration was invalidated."
                    .to_owned();
        } else {
            self.status = "Board unchanged; detections were preserved.".to_owned();
        }
        true
    }

    fn start_detection(&mut self) {
        if !self.apply_board() {
            return;
        }
        self.session.reset_detections();
        self.coverage = None;
        self.coverage_dirty = false;
        self.detect_queue = self
            .session
            .items()
            .iter()
            .filter(|item| item.enabled)
            .map(|item| item.id)
            .collect();
        self.cancel_requested = false;
        self.dispatch_next_detection();
    }

    fn dispatch_next_detection(&mut self) {
        let Some(id) = self.detect_queue.pop_front() else {
            self.active_job = None;
            self.active_cancellation = None;
            self.status = "Detection completed.".to_owned();
            return;
        };
        let Some(source) = self.sources.get(&id) else {
            self.status = "Dataset source binding is unavailable.".to_owned();
            self.detect_queue.clear();
            return;
        };
        let file_system = Arc::clone(&source.file_system);
        let reference = self
            .session
            .items()
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.reference.clone());
        let Some(reference) = reference else {
            return;
        };
        let token = match self.session.begin_detection(id) {
            Ok(token) => token,
            Err(error) => {
                self.status = error.to_string();
                self.detect_queue.clear();
                return;
            }
        };
        let file_cancellation = FsCancellation::default();
        let calibration_cancellation = CalibrationCancellation::default();
        if let Err(error) = self.worker.send(WorkerCommand::Detect(DetectionCommand {
            token: token.clone(),
            file_system,
            reference,
            file_cancellation: file_cancellation.clone(),
            calibration_cancellation: calibration_cancellation.clone(),
        })) {
            let _ = self.session.install_failure(&token, error.clone());
            self.status = error;
            self.detect_queue.clear();
            return;
        }
        self.active_job = Some(CalibrationJobKind::Detect);
        self.active_cancellation = Some(ActiveCancellation {
            file_system: file_cancellation,
            calibration: calibration_cancellation,
        });
        self.status = format!("Detecting item {}…", id.get());
    }

    fn start_calibration(&mut self) {
        let Ok(initial) = self.initial_intrinsics() else {
            return;
        };
        let snapshot = match self.session.calibration_snapshot(initial) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.status = error.to_string();
                return;
            }
        };
        let cancellation = CalibrationCancellation::default();
        if let Err(error) = self.worker.send(WorkerCommand::Calibrate {
            snapshot,
            cancellation: cancellation.clone(),
        }) {
            self.status = error;
            return;
        }
        self.active_job = Some(CalibrationJobKind::Calibrate);
        self.active_cancellation = Some(ActiveCancellation {
            file_system: FsCancellation::default(),
            calibration: cancellation,
        });
        self.cancel_requested = false;
        self.status = "Running Pangbot-compatible calibration…".to_owned();
    }

    fn poll_worker(&mut self, context: &egui::Context) {
        loop {
            let event = match self.worker.receiver.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.active_job = None;
                    self.active_cancellation = None;
                    self.detect_queue.clear();
                    self.status = "Calibration worker stopped unexpectedly.".to_owned();
                    break;
                }
            };
            self.active_job = None;
            self.active_cancellation = None;
            match event {
                WorkerEvent::Detection { token, result } => {
                    match result {
                        Ok(product) => {
                            let DetectionProduct {
                                source_version,
                                outcome,
                                preview,
                            } = product;
                            match self
                                .session
                                .install_detection(&token, source_version, outcome)
                            {
                                Ok(()) => {
                                    if let Some(frame) = preview {
                                        self.install_preview(context, token.item_id, frame);
                                    }
                                    let _ = self.session.set_selected(token.item_id);
                                }
                                Err(error) => self.status = error.to_string(),
                            }
                        }
                        Err(error) => {
                            let _ = self.session.install_failure(&token, error.clone());
                            self.status = error;
                        }
                    }
                    self.coverage_dirty = true;
                    if self.cancel_requested {
                        self.cancel_requested = false;
                        self.detect_queue.clear();
                        let pending = self.pending_imports.drain(..).collect();
                        self.import_candidates(pending, false);
                        self.status = "Detection cancelled.".to_owned();
                    } else if self.pending_imports.is_empty() {
                        self.dispatch_next_detection();
                    } else {
                        let pending = self.pending_imports.drain(..).collect();
                        self.import_candidates(pending, true);
                    }
                }
                WorkerEvent::Calibration { snapshot, result } => match result {
                    Ok(solution) => match self.session.install_solution(snapshot, solution) {
                        Ok(()) => {
                            self.status = "Calibration completed; result installed transactionally."
                                .to_owned()
                        }
                        Err(error) => self.status = format!("Calibration result rejected: {error}"),
                    },
                    Err(error) => self.status = error,
                },
            }
        }
    }

    fn install_preview(
        &mut self,
        context: &egui::Context,
        id: CalibrationItemId,
        frame: Arc<Rgba8Frame>,
    ) {
        let dimensions = [frame.width as usize, frame.height as usize];
        let image = egui::ColorImage::from_rgba_unmultiplied(dimensions, frame.pixels());
        let texture = context.load_texture(
            format!("calibration-preview-{}", id.get()),
            image,
            pixel_inspection_texture_options(),
        );
        if let Some(source) = self.sources.get_mut(&id) {
            source.preview = Some(CalibrationPreview { frame, texture });
        }
    }

    fn sync_coverage(&mut self, context: &egui::Context) {
        if !self.coverage_dirty {
            return;
        }
        self.coverage_dirty = false;
        self.coverage =
            build_coverage_image(self.session.items()).map(|image| CoverageVisualization {
                density: context.load_texture(
                    "calibration-coverage-density",
                    image.density,
                    egui::TextureOptions::LINEAR,
                ),
                enabled_views: image.enabled_views,
            });
    }

    fn refresh_auto_intrinsics_fields(&mut self) {
        if self.auto_intrinsics
            && let Some((fx, fy, cx, cy)) = self.auto_intrinsics_values()
        {
            self.fx = fx;
            self.fy = fy;
            self.cx = cx;
            self.cy = cy;
        }
    }

    fn auto_intrinsics_values(&self) -> Option<(f64, f64, f64, f64)> {
        let size = self
            .session
            .items()
            .iter()
            .filter(|item| item.enabled)
            .find_map(|item| match &item.status {
                CalibrationItemStatus::Found(detection) => Some(detection.image_size),
                _ => None,
            })?;
        let focal = f64::from(size.width.max(size.height));
        Some((
            focal,
            focal,
            f64::from(size.width) * 0.5,
            f64::from(size.height) * 0.5,
        ))
    }

    fn initial_intrinsics(&self) -> Result<InitialIntrinsics, String> {
        let (fx, fy, cx, cy) = if self.auto_intrinsics {
            self.auto_intrinsics_values()
                .ok_or_else(|| "Detect at least one enabled image first".to_owned())?
        } else {
            (self.fx, self.fy, self.cx, self.cy)
        };
        let initial = InitialIntrinsics {
            camera_matrix: [fx, 0.0, cx, 0.0, fy, cy, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 12],
        };
        initial.validate().map_err(|error| error.to_string())?;
        Ok(initial)
    }

    fn export_json(&mut self) {
        let Some(installed) = self.session.installed() else {
            return;
        };
        let items = self
            .session
            .items()
            .iter()
            .filter_map(|item| {
                let source = self.sources.get(&item.id)?;
                let (status, reason, corners) = match &item.status {
                    CalibrationItemStatus::Pending => ("pending", None, None),
                    CalibrationItemStatus::Reading => ("reading", None, None),
                    CalibrationItemStatus::Detecting => ("detecting", None, None),
                    CalibrationItemStatus::Found(detection) => {
                        ("found", None, Some(detection.corners.len()))
                    }
                    CalibrationItemStatus::NotFound { .. } => {
                        ("not_found", Some("Chessboard not found"), None)
                    }
                    CalibrationItemStatus::Failed(reason) => {
                        ("failed", Some(reason.as_str()), None)
                    }
                };
                Some(serde_json::json!({
                    "id": item.id.get(),
                    "source_id": item.reference.source_id,
                    "source_path": item.reference.path,
                    "display_path": source.display_path.display().to_string(),
                    "remote": source.remote,
                    "version": item.version,
                    "enabled": item.enabled,
                    "used": installed.item_ids.contains(&item.id),
                    "status": status,
                    "reason": reason,
                    "corners": corners,
                }))
            })
            .collect::<Vec<_>>();
        let payload = serde_json::json!({
            "schema_version": 1,
            "algorithm": "PangbotCompatible",
            "board": installed.request.board,
            "initial_intrinsics": installed.request.initial_intrinsics,
            "items": items,
            "solution": installed.solution,
        });
        let Some(path) = rfd::FileDialog::new()
            .set_file_name("camera_intrinsics.json")
            .add_filter("JSON", &["json"])
            .save_file()
        else {
            return;
        };
        match serde_json::to_vec_pretty(&payload) {
            Ok(bytes) => {
                let display_path = path.display().to_string();
                match thread::Builder::new()
                    .name("camera-toolbox-calibration-export".to_owned())
                    .spawn(move || {
                        if let Err(error) = std::fs::write(&path, bytes) {
                            tracing::error!(operation = "export_calibration_json", path = %path.display(), error = %error, "failed to export calibration result");
                        }
                    })
                {
                    Ok(_) => self.status = format!("Exporting calibration JSON to {display_path}."),
                    Err(error) => self.status = format!("Start calibration export failed: {error}"),
                }
            }
            Err(error) => self.status = format!("Serialize calibration result failed: {error}"),
        }
    }
}

/// OpenCV 以整数坐标表示像素中心；egui 的 `[0, 1]` UV 则覆盖纹理边界。
/// 因此先加半个像素，才能把检测点准确落到纹理中的连续图像坐标。
fn image_point_to_preview(
    point: CalibrationPoint,
    image_rect: egui::Rect,
    image_width: u32,
    image_height: u32,
) -> egui::Pos2 {
    egui::pos2(
        image_rect.left() + (point.x + 0.5) / image_width as f32 * image_rect.width(),
        image_rect.top() + (point.y + 0.5) / image_height as f32 * image_rect.height(),
    )
}

fn paint_reprojection_vector(painter: &egui::Painter, observed: egui::Pos2, projected: egui::Pos2) {
    let vector = projected - observed;
    let stroke = egui::Stroke::new(REPROJECTION_ARROW_WIDTH, REPROJECTED_POINT_COLOR);
    painter.line_segment([observed, projected], stroke);
    if vector.length_sq() > f32::EPSILON {
        let direction = vector / vector.length();
        let normal = egui::vec2(-direction.y, direction.x);
        let arrow_base = projected - direction * REPROJECTION_ARROW_HEAD_LENGTH;
        painter.line_segment(
            [
                projected,
                arrow_base + normal * REPROJECTION_ARROW_HEAD_HALF_WIDTH,
            ],
            stroke,
        );
        painter.line_segment(
            [
                projected,
                arrow_base - normal * REPROJECTION_ARROW_HEAD_HALF_WIDTH,
            ],
            stroke,
        );
    }
    painter.circle_filled(projected, 2.0, REPROJECTED_POINT_COLOR);
}

fn status_label(status: &CalibrationItemStatus) -> &'static str {
    match status {
        CalibrationItemStatus::Pending => "Pending",
        CalibrationItemStatus::Reading => "Reading",
        CalibrationItemStatus::Detecting => "Detecting",
        CalibrationItemStatus::Found(_) => "Found",
        CalibrationItemStatus::NotFound { .. } => "Not found",
        CalibrationItemStatus::Failed(_) => "Failed",
    }
}

fn status_color(status: &CalibrationItemStatus) -> Option<egui::Color32> {
    match status {
        CalibrationItemStatus::Found(_) => Some(OBSERVED_POINT_COLOR),
        CalibrationItemStatus::NotFound { .. } | CalibrationItemStatus::Failed(_) => {
            Some(REPROJECTED_POINT_COLOR)
        }
        CalibrationItemStatus::Pending
        | CalibrationItemStatus::Reading
        | CalibrationItemStatus::Detecting => None,
    }
}

fn detection_size(status: &CalibrationItemStatus) -> Option<CalibrationImageSize> {
    match status {
        CalibrationItemStatus::Found(detection) => Some(detection.image_size),
        CalibrationItemStatus::NotFound { image_size } => Some(*image_size),
        _ => None,
    }
}

fn calibration_view(
    installed: Option<&camera_toolbox_app::InstalledCalibration>,
    item_id: CalibrationItemId,
) -> Option<&ViewCalibrationResult> {
    let installed = installed?;
    let index = installed.item_ids.iter().position(|id| *id == item_id)?;
    installed.solution.views.get(index)
}

fn calibration_metric(
    installed: Option<&camera_toolbox_app::InstalledCalibration>,
    item_id: CalibrationItemId,
) -> Option<f64> {
    calibration_view(installed, item_id).map(|view| view.reprojection_rmse)
}

fn render_rmse_cell(ui: &mut egui::Ui, metric: Option<f64>, max_metric: f64) {
    let Some(metric) = metric else {
        ui.weak("—");
        return;
    };
    let size = egui::vec2(ui.available_width().max(24.0), 16.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, egui::Color32::DARK_GRAY);
    let ratio = (metric / max_metric).clamp(0.0, 1.0) as f32;
    let fill_rect =
        egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * ratio, rect.height()));
    painter.rect_filled(fill_rect, 2.0, egui::Color32::LIGHT_BLUE);

    // 同一数值按进度边界分色，保证浅色填充区和深色轨道区都保持高对比度。
    let galley = painter.layout_no_wrap(
        format!("{metric:.3}"),
        egui::FontId::monospace(11.0),
        egui::Color32::PLACEHOLDER,
    );
    let text_position = rect.center() - galley.size() * 0.5;
    painter
        .with_clip_rect(fill_rect)
        .galley_with_override_text_color(text_position, Arc::clone(&galley), RMSE_TEXT_ON_FILL);
    let track_rect = egui::Rect::from_min_max(
        egui::pos2(fill_rect.right(), rect.top()),
        rect.right_bottom(),
    );
    painter
        .with_clip_rect(track_rect)
        .galley_with_override_text_color(text_position, galley, RMSE_TEXT_ON_TRACK);
    response.on_hover_text(format!("Reprojection RMSE: {metric:.6} px"));
}

struct CoverageImage {
    density: egui::ColorImage,
    enabled_views: usize,
}

fn build_coverage_image(
    items: &[camera_toolbox_app::CalibrationDatasetItem],
) -> Option<CoverageImage> {
    let image_size =
        items
            .iter()
            .filter(|item| item.enabled)
            .find_map(|item| match &item.status {
                CalibrationItemStatus::Found(detection) => Some(detection.image_size),
                _ => None,
            })?;
    let coverage_height = ((COVERAGE_WIDTH as f64 * f64::from(image_size.height)
        / f64::from(image_size.width))
    .round() as usize)
        .clamp(64, 256);
    let mut corner_hits = vec![0.0_f32; COVERAGE_WIDTH * coverage_height];
    let mut enabled_views = 0_usize;
    for detection in items.iter().filter(|item| item.enabled).filter_map(|item| {
        if let CalibrationItemStatus::Found(detection) = &item.status {
            Some(detection)
        } else {
            None
        }
    }) {
        enabled_views += 1;
        let width = detection.image_size.width as f32;
        let height = detection.image_size.height as f32;
        for point in &detection.corners {
            let x = ((point.x / width) * COVERAGE_WIDTH as f32)
                .floor()
                .clamp(0.0, (COVERAGE_WIDTH - 1) as f32) as usize;
            let y = ((point.y / height) * coverage_height as f32)
                .floor()
                .clamp(0.0, (coverage_height - 1) as f32) as usize;
            corner_hits[y * COVERAGE_WIDTH + x] += 1.0;
        }
    }
    if enabled_views == 0 {
        return None;
    }
    let density = gaussian_blur(
        &corner_hits,
        COVERAGE_WIDTH,
        coverage_height,
        COVERAGE_GAUSSIAN_SIGMA,
    );
    Some(CoverageImage {
        density: colorize_heatmap(&density, COVERAGE_WIDTH, coverage_height),
        enabled_views,
    })
}

fn gaussian_blur(values: &[f32], width: usize, height: usize, sigma: f32) -> Vec<f32> {
    let radius = (sigma * 3.0).ceil() as isize;
    let kernel = (-radius..=radius)
        .map(|offset| (-(offset as f32).powi(2) / (2.0 * sigma * sigma)).exp())
        .collect::<Vec<_>>();
    let mut horizontal = vec![0.0_f32; values.len()];
    for y in 0..height {
        for x in 0..width {
            let mut sum = 0.0;
            let mut weight_sum = 0.0;
            for (kernel_index, offset) in (-radius..=radius).enumerate() {
                let sample_x = x as isize + offset;
                if (0..width as isize).contains(&sample_x) {
                    let weight = kernel[kernel_index];
                    sum += values[y * width + sample_x as usize] * weight;
                    weight_sum += weight;
                }
            }
            horizontal[y * width + x] = sum / weight_sum.max(f32::EPSILON);
        }
    }
    let mut output = vec![0.0_f32; values.len()];
    for y in 0..height {
        for x in 0..width {
            let mut sum = 0.0;
            let mut weight_sum = 0.0;
            for (kernel_index, offset) in (-radius..=radius).enumerate() {
                let sample_y = y as isize + offset;
                if (0..height as isize).contains(&sample_y) {
                    let weight = kernel[kernel_index];
                    sum += horizontal[sample_y as usize * width + x] * weight;
                    weight_sum += weight;
                }
            }
            output[y * width + x] = sum / weight_sum.max(f32::EPSILON);
        }
    }
    output
}

fn colorize_heatmap(values: &[f32], width: usize, height: usize) -> egui::ColorImage {
    let peak = values.iter().copied().fold(0.0_f32, f32::max);
    let mut rgba = Vec::with_capacity(values.len() * 4);
    for value in values {
        let strength = if peak <= f32::EPSILON {
            0.0
        } else {
            (*value / peak).clamp(0.0, 1.0)
        };
        let color = heatmap_color(strength);
        rgba.extend_from_slice(&[color.r(), color.g(), color.b(), 255]);
    }
    egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba)
}

fn heatmap_color(value: f32) -> egui::Color32 {
    const STOPS: [(f32, [u8; 3]); 6] = [
        (0.0, [48, 18, 59]),
        (0.2, [50, 92, 210]),
        (0.4, [24, 199, 210]),
        (0.6, [86, 230, 104]),
        (0.8, [246, 220, 52]),
        (1.0, [180, 32, 18]),
    ];
    let value = value.clamp(0.0, 1.0);
    let upper = STOPS
        .iter()
        .position(|(position, _)| *position >= value)
        .unwrap_or(STOPS.len() - 1);
    let lower = upper.saturating_sub(1);
    let span = (STOPS[upper].0 - STOPS[lower].0).max(f32::EPSILON);
    let mix = (value - STOPS[lower].0) / span;
    let channel = |index: usize| {
        (f32::from(STOPS[lower].1[index])
            + (f32::from(STOPS[upper].1[index]) - f32::from(STOPS[lower].1[index])) * mix)
            .round() as u8
    };
    egui::Color32::from_rgb(channel(0), channel(1), channel(2))
}

fn paint_heatmap_guides(painter: &egui::Painter, rect: egui::Rect) {
    for index in 1..3 {
        let x = egui::lerp(rect.x_range(), index as f32 / 3.0);
        let y = egui::lerp(rect.y_range(), index as f32 / 3.0);
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(0.7, egui::Color32::from_white_alpha(90)),
        );
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(0.7, egui::Color32::from_white_alpha(90)),
        );
    }
    let legend = egui::Rect::from_min_size(
        egui::pos2(rect.right() - rect.width() * 0.34, rect.bottom() - 12.0),
        egui::vec2(rect.width() * 0.30, 7.0),
    );
    for index in 0..32 {
        let x0 = egui::lerp(legend.x_range(), index as f32 / 32.0);
        let x1 = egui::lerp(legend.x_range(), (index + 1) as f32 / 32.0);
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x0, legend.top()),
                egui::pos2(x1, legend.bottom()),
            ),
            0.0,
            heatmap_color(index as f32 / 31.0),
        );
    }
    painter.text(
        egui::pos2(legend.left() - 3.0, legend.center().y),
        egui::Align2::RIGHT_CENTER,
        "low",
        egui::FontId::monospace(9.0),
        egui::Color32::WHITE,
    );
    painter.text(
        egui::pos2(legend.right() + 3.0, legend.center().y),
        egui::Align2::LEFT_CENTER,
        "high",
        egui::FontId::monospace(9.0),
        egui::Color32::WHITE,
    );
    painter.text(
        rect.left_top() + egui::vec2(8.0, 8.0),
        egui::Align2::LEFT_TOP,
        "Inner-corner density",
        egui::FontId::proportional(12.0),
        egui::Color32::WHITE,
    );
}

fn render_calibration_result(
    ui: &mut egui::Ui,
    installed: Option<&camera_toolbox_app::InstalledCalibration>,
) {
    ui.strong("Calibration result");
    let Some(installed) = installed else {
        ui.weak("Run Calibrate to display final intrinsics and distortion coefficients.");
        return;
    };
    let solution = &installed.solution;
    let matrix = solution.camera_matrix;
    ui.horizontal_wrapped(|ui| {
        for (name, value) in [
            ("fx", matrix[0]),
            ("fy", matrix[4]),
            ("cx", matrix[2]),
            ("cy", matrix[5]),
        ] {
            ui.group(|ui| {
                ui.label(name);
                ui.monospace(format!("{value:.8}"));
            });
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.monospace(format!(
            "{}×{} · RMS {:.6} px · flags {}",
            solution.image_size.width,
            solution.image_size.height,
            solution.rms_error,
            solution.calibration_flags
        ));
    });
    ui.label("Camera matrix (row-major)");
    egui::Grid::new("calibration_result_matrix")
        .num_columns(3)
        .show(ui, |ui| {
            for row in matrix.chunks_exact(3) {
                for value in row {
                    ui.monospace(format!("{value:.10}"));
                }
                ui.end_row();
            }
        });
    ui.label("Distortion coefficients (OpenCV order)");
    const NAMES: [&str; 12] = [
        "k1", "k2", "p1", "p2", "k3", "k4", "k5", "k6", "s1", "s2", "s3", "s4",
    ];
    egui::Grid::new("calibration_result_distortion")
        .num_columns(4)
        .striped(true)
        .show(ui, |ui| {
            for (index, value) in solution.distortion_coefficients.iter().enumerate() {
                let name = NAMES.get(index).copied().unwrap_or("d");
                ui.monospace(format!("{name}[{index}] = {value:.10}"));
                if (index + 1) % 4 == 0 {
                    ui.end_row();
                }
            }
            if solution.distortion_coefficients.len() % 4 != 0 {
                ui.end_row();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_adapters::filesystem::LocalFileSystem;
    use camera_toolbox_app::{FileSourceId, FileSystem, SourcePath};
    use std::time::Instant;

    #[test]
    fn opened_pngs_are_detected_in_order_and_last_result_is_selected() {
        let root = std::env::temp_dir().join(format!(
            "camera-toolbox-calibration-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let fixture = include_bytes!("../../../adapters/tests/data/chessboard_11x8_clean.png");
        let first_path = root.join("first.png");
        let second_path = root.join("second.png");
        std::fs::write(&first_path, fixture).unwrap();
        std::fs::write(&second_path, fixture).unwrap();

        let source_id = FileSourceId::new("calibration-test").unwrap();
        let file_system: Arc<dyn FileSystem> =
            Arc::new(LocalFileSystem::new(source_id.clone(), &root).unwrap());
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        let candidate = |name: &str, display_path: PathBuf| {
            let reference =
                camera_toolbox_app::FileRef::new(source_id.clone(), SourcePath::new(name).unwrap());
            let control = FsControl::with_timeout(Duration::from_secs(2));
            CalibrationImportCandidate {
                display_path,
                file_system: Arc::clone(&file_system),
                entry: file_system.stat(&reference, &control).unwrap(),
                remote: false,
            }
        };

        workspace.import(vec![candidate("first.png", first_path)]);
        workspace.preview_mode = CalibrationPreviewMode::Overlay;
        assert_eq!(workspace.active_job, Some(CalibrationJobKind::Detect));
        workspace.import(vec![candidate("second.png", second_path)]);

        let deadline = Instant::now() + Duration::from_secs(10);
        while workspace.active_job.is_some() && Instant::now() < deadline {
            workspace.poll_worker(&context);
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            workspace.active_job.is_none(),
            "worker did not finish: {}",
            workspace.status
        );
        assert_eq!(workspace.session.items().len(), 2);
        assert!(
            workspace
                .session
                .items()
                .iter()
                .all(|item| matches!(item.status, CalibrationItemStatus::Found(_)))
        );
        workspace.sync_coverage(&context);
        assert_eq!(workspace.preview_mode, CalibrationPreviewMode::Overlay);
        let last_id = workspace.session.items()[1].id;
        assert_eq!(workspace.session.selected(), Some(last_id));
        assert!(workspace.sources[&last_id].preview.is_some());
        let texture_id = workspace.sources[&last_id]
            .preview
            .as_ref()
            .unwrap()
            .texture
            .id();
        let texture_manager = context.tex_manager();
        let options = texture_manager.read().meta(texture_id).unwrap().options;
        assert_eq!(options.magnification, egui::TextureFilter::Nearest);
        assert_eq!(options.minification, egui::TextureFilter::Linear);
        assert_eq!(options.mipmap_mode, Some(egui::TextureFilter::Linear));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_detection_resets_results_for_enabled_and_disabled_items() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        install_detection_outcome(&mut workspace, "enabled.png", found_detection(640, 480));
        let disabled =
            install_detection_outcome(&mut workspace, "disabled.png", found_detection(640, 480));
        workspace.session.set_enabled(disabled, false).unwrap();
        workspace.preview_mode = CalibrationPreviewMode::Heatmap;

        workspace.start_detection();

        assert!(
            workspace
                .session
                .items()
                .iter()
                .all(|item| matches!(item.status, CalibrationItemStatus::Pending))
        );
        assert!(workspace.coverage.is_none());
        assert_eq!(workspace.preview_mode, CalibrationPreviewMode::Heatmap);
    }

    fn install_detection_outcome(
        workspace: &mut CalibrationWorkspace,
        name: &str,
        outcome: camera_toolbox_core::ChessboardDetectionOutcome,
    ) -> CalibrationItemId {
        let version = camera_toolbox_app::FileVersion {
            size: 128,
            modified_millis: Some(1),
        };
        let reference = camera_toolbox_app::FileRef::new(
            FileSourceId::new("calibration-session-test").unwrap(),
            SourcePath::new(name).unwrap(),
        );
        let AddCalibrationItemOutcome::Added(id) =
            workspace
                .session
                .add_or_refresh(reference, version, name.to_owned())
        else {
            panic!("expected a new calibration item");
        };
        let token = workspace.session.begin_detection(id).unwrap();
        workspace.session.mark_detecting(&token).unwrap();
        workspace
            .session
            .install_detection(&token, version, outcome)
            .unwrap();
        id
    }

    fn found_detection(width: u32, height: u32) -> camera_toolbox_core::ChessboardDetectionOutcome {
        let corners = (0..8)
            .flat_map(|row| {
                (0..11).map(move |col| CalibrationPoint {
                    x: 60.0 + col as f32 * 40.0,
                    y: 60.0 + row as f32 * 40.0,
                })
            })
            .collect();
        camera_toolbox_core::ChessboardDetectionOutcome::Found(
            camera_toolbox_core::ChessboardDetection {
                image_size: CalibrationImageSize { width, height },
                corners,
            },
        )
    }

    #[test]
    fn automatic_intrinsics_use_first_enabled_found_view() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        install_detection_outcome(
            &mut workspace,
            "not-found.png",
            camera_toolbox_core::ChessboardDetectionOutcome::NotFound {
                image_size: CalibrationImageSize {
                    width: 1920,
                    height: 1080,
                },
            },
        );
        install_detection_outcome(&mut workspace, "found.png", found_detection(640, 480));

        workspace.refresh_auto_intrinsics_fields();
        assert_eq!((workspace.fx, workspace.fy), (640.0, 640.0));
        assert_eq!((workspace.cx, workspace.cy), (320.0, 240.0));
        let initial = workspace.initial_intrinsics().unwrap();
        assert_eq!(
            initial.camera_matrix,
            [640.0, 0.0, 320.0, 0.0, 640.0, 240.0, 0.0, 0.0, 1.0]
        );
    }

    #[test]
    fn coverage_heatmap_uses_enabled_found_views_only() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        install_detection_outcome(
            &mut workspace,
            "not-found.png",
            camera_toolbox_core::ChessboardDetectionOutcome::NotFound {
                image_size: CalibrationImageSize {
                    width: 640,
                    height: 480,
                },
            },
        );
        let found_id =
            install_detection_outcome(&mut workspace, "found.png", found_detection(640, 480));

        let coverage = build_coverage_image(workspace.session.items()).unwrap();
        assert_eq!(coverage.enabled_views, 1);
        assert_eq!(coverage.density.size, [COVERAGE_WIDTH, 144]);
        workspace.session.set_enabled(found_id, false).unwrap();
        assert!(build_coverage_image(workspace.session.items()).is_none());
    }

    #[test]
    fn preview_modes_select_expected_layers() {
        assert_eq!(
            preview_layers(CalibrationPreviewMode::Heatmap, true),
            PreviewLayers {
                input: false,
                heatmap_alpha: Some(255),
            }
        );
        assert_eq!(
            preview_layers(CalibrationPreviewMode::Overlay, true),
            PreviewLayers {
                input: true,
                heatmap_alpha: Some(150),
            }
        );
        assert_eq!(
            preview_layers(CalibrationPreviewMode::InputImage, true),
            PreviewLayers {
                input: true,
                heatmap_alpha: None,
            }
        );
        assert_eq!(
            preview_layers(CalibrationPreviewMode::Heatmap, false),
            PreviewLayers {
                input: true,
                heatmap_alpha: None,
            }
        );
    }

    #[test]
    fn preview_zoom_keeps_anchor_image_position_stable() {
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 200.0));
        let image_size = egui::vec2(400.0, 200.0);
        let anchor = egui::pos2(150.0, 100.0);
        let mut state = CalibrationPreviewViewport::default();
        state.fit_to_rect(viewport, image_size);
        let before = (anchor - viewport.min - state.pan) / state.zoom;
        state.zoom_by(2.0, anchor, viewport);
        let after = (anchor - viewport.min - state.pan) / state.zoom;
        assert!((before - after).length() < 1e-4);
    }

    #[test]
    fn preview_mapping_aligns_opencv_coordinates_with_texel_centers_at_64x() {
        let scale = 64.0;
        let image_rect = egui::Rect::from_min_size(
            egui::pos2(13.0, 17.0),
            egui::vec2(640.0 * scale, 480.0 * scale),
        );

        assert_eq!(
            image_point_to_preview(CalibrationPoint::new(0.0, 0.0), image_rect, 640, 480),
            image_rect.min + egui::vec2(0.5 * scale, 0.5 * scale)
        );
        assert_eq!(
            image_point_to_preview(CalibrationPoint::new(119.5, 99.5), image_rect, 640, 480,),
            image_rect.min + egui::vec2(120.0 * scale, 100.0 * scale)
        );
        assert_eq!(
            image_point_to_preview(CalibrationPoint::new(639.0, 479.0), image_rect, 640, 480,),
            image_rect.max - egui::vec2(0.5 * scale, 0.5 * scale)
        );
    }

    #[test]
    fn square_size_apply_preserves_detections_and_inner_corner_change_invalidates_them() {
        let context = egui::Context::default();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        assert_eq!(workspace.session.board().square_size, 40.0);
        install_detection_outcome(&mut workspace, "view.png", found_detection(640, 480));

        workspace.square_size = 45.0;
        assert!(workspace.apply_board());
        assert!(matches!(
            workspace.session.items()[0].status,
            CalibrationItemStatus::Found(_)
        ));

        assert!(workspace.status.contains("detections were preserved"));

        workspace.board_cols = 12;
        assert!(workspace.apply_board());
        assert!(matches!(
            workspace.session.items()[0].status,
            CalibrationItemStatus::Pending
        ));
        assert!(workspace.status.contains("detections were invalidated"));
    }

    #[test]
    fn rmse_text_colors_meet_contrast_on_both_progress_regions() {
        fn relative_luminance(color: egui::Color32) -> f32 {
            let linear = |channel: u8| {
                let value = f32::from(channel) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
        }

        fn contrast_ratio(first: egui::Color32, second: egui::Color32) -> f32 {
            let first = relative_luminance(first);
            let second = relative_luminance(second);
            (first.max(second) + 0.05) / (first.min(second) + 0.05)
        }

        assert!(contrast_ratio(RMSE_TEXT_ON_FILL, egui::Color32::LIGHT_BLUE) >= 4.5);
        assert!(contrast_ratio(RMSE_TEXT_ON_TRACK, egui::Color32::DARK_GRAY) >= 4.5);
    }

    #[test]
    fn installed_solution_renders_intrinsics_distortion_and_inline_rmse() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut workspace = CalibrationWorkspace::new(&context).unwrap();
        for index in 0..3 {
            install_detection_outcome(
                &mut workspace,
                &format!("view-{index}.png"),
                found_detection(640, 480),
            );
        }
        let initial = workspace.initial_intrinsics().unwrap();
        let snapshot = workspace.session.calibration_snapshot(initial).unwrap();
        let views = snapshot
            .request
            .image_points
            .iter()
            .enumerate()
            .map(
                |(index, points)| camera_toolbox_core::ViewCalibrationResult {
                    rotation_vector: [0.0; 3],
                    translation_vector: [0.0, 0.0, 1.0],
                    projected_points: points.clone(),
                    reprojection_rmse: 0.1 + index as f64 * 0.05,
                    max_reprojection_error: 0.2 + index as f64 * 0.05,
                },
            )
            .collect();
        let solution = CalibrationSolution {
            image_size: snapshot.request.image_size,
            camera_matrix: [620.0, 0.0, 318.0, 0.0, 621.0, 241.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![
                0.1, -0.2, 0.001, -0.002, 0.03, 0.01, -0.01, 0.005, 0.0001, -0.0001, 0.0002,
                -0.0002,
            ],
            rms_error: 0.15,
            calibration_flags: camera_toolbox_core::PANGBOT_CALIBRATION_FLAGS,
            views,
        };
        workspace
            .session
            .install_solution(snapshot, solution)
            .unwrap();
        let installed = workspace.session.installed().unwrap();
        let second_view = calibration_view(Some(installed), installed.item_ids[1]).unwrap();
        assert!((second_view.reprojection_rmse - 0.15).abs() < 1e-12);

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        let output = context.run_ui(input, |ui| {
            workspace.render(&context, ui);
        });
        let text = output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .into_iter()
            .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "RMSE",
            "Heatmap",
            "Overlay",
            "Input image",
            "Calibration result",
            "fx",
            "620.00000000",
            "Distortion coefficients (OpenCV order)",
            "k1[0] = 0.1000000000",
            "Square size (mm)",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text}");
        }
        assert!(!text.contains("Reprojection RMSE"));
        assert!(!text.contains("Wheel: zoom"));
        assert!(!text.contains("Remove selected"));
    }

    #[test]
    fn status_colors_distinguish_unprocessed_success_and_failure() {
        let camera_toolbox_core::ChessboardDetectionOutcome::Found(detection) =
            found_detection(640, 480)
        else {
            unreachable!();
        };
        assert_eq!(status_color(&CalibrationItemStatus::Pending), None);
        assert_eq!(status_color(&CalibrationItemStatus::Reading), None);
        assert_eq!(
            status_color(&CalibrationItemStatus::Found(detection)),
            Some(OBSERVED_POINT_COLOR)
        );
        assert_eq!(
            status_color(&CalibrationItemStatus::NotFound {
                image_size: CalibrationImageSize {
                    width: 640,
                    height: 480,
                },
            }),
            Some(REPROJECTED_POINT_COLOR)
        );
        assert_eq!(
            status_color(&CalibrationItemStatus::Failed("decode failed".to_owned())),
            Some(REPROJECTED_POINT_COLOR)
        );
    }
}
