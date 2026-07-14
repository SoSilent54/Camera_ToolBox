//! GUI 顶层状态与界面编排。

use std::sync::Arc;

use camera_toolbox_adapters::LocalRawLoader;
use camera_toolbox_app::{LocalRawAnalyzeRequest, Workflow};
use camera_toolbox_core::Roi;
use eframe::egui;

use crate::{
    analysis_panel::{AnalysisPanelState, DesiredAnalysis, render_analysis_panel},
    analysis_worker::{AnalysisDomain, AnalysisKey, AnalysisRequest, AnalysisWorker},
    color_controls::{DisplayMode, render_color_controls},
    color_worker::{ColorRenderRequest, ColorRenderWorker},
    histogram_link::{
        HistogramBinSelection, HistogramPixelSample, ImageHistogramHover, SpatialHighlight,
        SpatialHighlightRequest, SpatialHighlightWorker, display_histogram_sample,
    },
    notification::{NotificationCenter, NotificationKey, NotificationScope, UiNotification},
    raw_dialog::RawOpenDialogState,
    viewer::{
        HoverNeighborhood, HoverViewSettings, ImageViewerState, LoadedRaw, ViewerAction,
        bayer_label, render_viewer,
    },
};

pub(crate) struct CameraToolboxApp {
    loaded: Option<LoadedRaw>,
    viewer: ImageViewerState,
    raw_dialog: RawOpenDialogState,
    display_mode: DisplayMode,
    color_panel_expanded: bool,
    hover_view: HoverViewSettings,
    color_worker: ColorRenderWorker,
    analysis_worker: AnalysisWorker,
    spatial_worker: SpatialHighlightWorker,
    spatial_requested: Option<HistogramBinSelection>,
    spatial_highlight: Option<SpatialHighlight>,
    analysis_panel: AnalysisPanelState,
    analysis_pending_active: Option<(u64, Roi)>,
    notifications: NotificationCenter,
    next_generation: u64,
    next_load_attempt: u64,
}

impl CameraToolboxApp {
    pub(crate) fn new(context: &egui::Context) -> std::io::Result<Self> {
        Ok(Self {
            loaded: None,
            viewer: ImageViewerState::default(),
            raw_dialog: RawOpenDialogState::default(),
            display_mode: DisplayMode::Color,
            color_panel_expanded: true,
            hover_view: HoverViewSettings::default(),
            color_worker: ColorRenderWorker::new(context)?,
            analysis_worker: AnalysisWorker::new(context)?,
            spatial_worker: SpatialHighlightWorker::new(context)?,
            spatial_requested: None,
            spatial_highlight: None,
            analysis_panel: AnalysisPanelState::default(),
            analysis_pending_active: None,
            notifications: NotificationCenter::default(),
            next_generation: 1,
            next_load_attempt: 1,
        })
    }
}

impl eframe::App for CameraToolboxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.poll_color_result(&context);
        self.poll_analysis_result();
        self.poll_spatial_highlight_result();
        // 先使 Display chart 与刚安装的纹理 revision 对齐，再绘制面板。
        self.ensure_analysis();
        if let Some(loaded) = &self.loaded {
            self.viewer.refresh_cursor(&context, &loaded.frame);
        }

        egui::Panel::top("menu_bar").show(ui, |ui| {
            self.render_menu_bar(ui);
        });
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            self.render_status_bar(ui);
        });
        self.render_analysis_panel_ui(ui);
        self.render_color_panel(ui);
        let viewer_output = egui::CentralPanel::default()
            .show(ui, |ui| {
                render_viewer(
                    ui,
                    self.loaded.as_ref(),
                    &mut self.viewer,
                    self.display_mode,
                    self.hover_view,
                    self.spatial_highlight.as_ref(),
                )
            })
            .inner;
        let viewer_rect = viewer_output.rect;
        if let Some(action) = viewer_output.action {
            self.handle_viewer_action(action);
        }
        self.notifications
            .render(&context, viewer_rect, context.input(|input| input.time));
        self.render_raw_open_dialog(&context);
        self.ensure_analysis();
    }
}

impl CameraToolboxApp {
    fn render_menu_bar(&mut self, ui: &mut egui::Ui) {
        let mut request_color = false;
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| self.render_file_menu(ui));
            ui.menu_button("View", |ui| {
                request_color |= self.render_view_menu(ui);
            });
            ui.menu_button("Tools", |ui| self.render_tools_menu(ui));
            ui.menu_button("Help", |ui| {
                ui.label("Camera Toolbox");
                ui.label("Local Bayer RAW color viewer");
                ui.separator();
                ui.label("Log directory");
                if let Some(directory) = camera_toolbox_logging::logging_directory() {
                    let path = directory.display().to_string();
                    ui.monospace(&path);
                    if ui.button("Copy log path").clicked() {
                        ui.ctx().copy_text(path);
                    }
                } else {
                    ui.label("Unavailable on this platform");
                }
            });
        });
        if request_color {
            self.request_current_color();
        }
    }

    fn render_file_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Open Raw...").clicked() {
            self.raw_dialog.open(ui.ctx());
            ui.close();
        }
        if ui
            .add_enabled(self.loaded.is_some(), egui::Button::new("Close Image"))
            .clicked()
        {
            if let Some(loaded) = &self.loaded {
                self.notifications
                    .clear_scope(NotificationScope::ImageGeneration(loaded.generation));
                self.analysis_panel.clear_generation(loaded.generation);
            }
            self.color_worker.cancel();
            self.analysis_worker.cancel();
            self.clear_spatial_highlight();
            self.analysis_pending_active = None;
            self.analysis_panel.clear();
            self.loaded = None;
            self.viewer = ImageViewerState::default();
            ui.close();
        }
        ui.separator();
        if ui.button("Quit").clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn render_view_menu(&mut self, ui: &mut egui::Ui) -> bool {
        let mut request_color = false;
        if ui
            .add_enabled(
                self.loaded.is_some(),
                egui::Button::selectable(
                    self.display_mode == DisplayMode::RawMono,
                    DisplayMode::RawMono.label(),
                ),
            )
            .clicked()
        {
            self.display_mode = DisplayMode::RawMono;
            ui.close();
        }
        if ui
            .add_enabled(
                self.loaded.is_some(),
                egui::Button::selectable(
                    self.display_mode == DisplayMode::Color,
                    DisplayMode::Color.label(),
                ),
            )
            .clicked()
        {
            self.display_mode = DisplayMode::Color;
            request_color = true;
            ui.close();
        }
        ui.separator();
        self.render_view_navigation(ui);
        request_color
    }

    fn render_tools_menu(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.hover_view.enabled, "Hover View");
        if self.hover_view.enabled {
            ui.menu_button("Hover View Settings", |ui| {
                ui.label("Neighborhood");
                ui.separator();
                for neighborhood in HoverNeighborhood::ALL {
                    if ui
                        .selectable_value(
                            &mut self.hover_view.neighborhood,
                            neighborhood,
                            neighborhood.label(),
                        )
                        .clicked()
                    {
                        ui.close();
                    }
                }
            });
        }
        let analysis_enabled = self.loaded.is_some();
        ui.add_enabled_ui(analysis_enabled, |ui| {
            ui.checkbox(&mut self.analysis_panel.expanded, "Analysis Panel");
            if ui.button("Reset ROI to Full Frame").clicked() {
                self.reset_roi_to_full_frame();
                ui.close();
            }
            ui.weak("Right-drag image to select ROI");
        });
    }

    fn render_view_navigation(&mut self, ui: &mut egui::Ui) {
        if ui
            .add_enabled(self.loaded.is_some(), egui::Button::new("Fit to Window"))
            .clicked()
        {
            self.viewer.fit_on_next_frame = true;
            ui.close();
        }
        if ui
            .add_enabled(
                self.loaded.is_some(),
                egui::Button::new("Actual Size / 100%"),
            )
            .clicked()
        {
            self.viewer.zoom = 1.0;
            self.viewer.fit_on_next_frame = false;
            ui.close();
        }
        if ui
            .add_enabled(self.loaded.is_some(), egui::Button::new("Zoom In"))
            .clicked()
        {
            self.viewer.zoom_by(1.25, None, egui::Rect::NOTHING);
            ui.close();
        }
        if ui
            .add_enabled(self.loaded.is_some(), egui::Button::new("Zoom Out"))
            .clicked()
        {
            self.viewer.zoom_by(0.8, None, egui::Rect::NOTHING);
            ui.close();
        }
        if ui
            .add_enabled(self.loaded.is_some(), egui::Button::new("Reset View"))
            .clicked()
        {
            self.viewer = ImageViewerState::default();
            ui.close();
        }
    }

    fn render_color_panel(&mut self, ui: &mut egui::Ui) {
        if self.loaded.is_none() {
            return;
        }
        if !self.color_panel_expanded {
            let mut expand = false;
            egui::Panel::right("color_processing_rail")
                .resizable(false)
                .min_size(32.0)
                .max_size(32.0)
                .show(ui, |ui| {
                    let response =
                        ui.add_sized(egui::vec2(28.0, 28.0), egui::Button::new("‹").frame(false));
                    expand = response.clicked();
                    response.on_hover_text("Expand Color Processing");
                });
            if expand {
                self.color_panel_expanded = true;
            }
            return;
        }

        let mut should_submit = false;
        let mut collapse = false;
        egui::Panel::right("color_processing")
            .resizable(true)
            .default_size(280.0)
            .min_size(240.0)
            .max_size(420.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Color Processing");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let response = ui.button("›");
                        collapse = response.clicked();
                        response.on_hover_text("Collapse Color Processing");
                    });
                });
                egui::ScrollArea::vertical()
                    .id_salt("color_processing_controls")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let loaded = self.loaded.as_mut().expect("checked above");
                        let source_spec = loaded.frame.spec.clone();
                        let installed_revision = loaded.installed_revision();
                        let response = render_color_controls(
                            ui,
                            &mut loaded.color_edit,
                            &source_spec,
                            &mut self.display_mode,
                            installed_revision,
                        );
                        should_submit = response.params_changed
                            || (response.mode_changed && self.display_mode == DisplayMode::Color);
                    });
            });
        if collapse {
            self.color_panel_expanded = false;
        }
        if should_submit {
            self.request_current_color();
        }
    }

    fn render_analysis_panel_ui(&mut self, ui: &mut egui::Ui) {
        if self.loaded.is_none() {
            return;
        }
        let image_hover = self.image_histogram_hover();
        let expanded = self.analysis_panel.expanded;
        let default_size = self.analysis_panel.panel_height();
        let min_size = self.analysis_panel.min_height();
        let max_size = if expanded {
            (ui.available_height() * 0.45).max(min_size)
        } else {
            min_size
        };
        let response = egui::Panel::bottom("analysis_panel")
            .resizable(expanded)
            .default_size(default_size)
            .min_size(min_size)
            .max_size(max_size)
            .show(ui, |ui| {
                render_analysis_panel(ui, &mut self.analysis_panel, image_hover)
            })
            .inner;
        if response.selection_changed {
            self.analysis_pending_active = None;
        }
        self.update_spatial_highlight(
            response.hovered_bin,
            response.selection_changed || response.view_interacting,
        );
    }

    fn update_spatial_highlight(
        &mut self,
        selection: Option<HistogramBinSelection>,
        suppress: bool,
    ) {
        let Some(selection) = selection.filter(|_| !suppress) else {
            self.clear_spatial_highlight();
            return;
        };
        if self.spatial_requested == Some(selection) {
            return;
        }
        let Some(loaded) = self.loaded.as_ref() else {
            self.clear_spatial_highlight();
            return;
        };
        if self.analysis_panel.current_key() != Some(selection.key)
            || loaded.generation != selection.key.generation
        {
            self.clear_spatial_highlight();
            return;
        }
        let display_image = match selection.key.domain {
            AnalysisDomain::RawBayer => None,
            AnalysisDomain::DisplayRgb => {
                let Some(preview) = loaded.installed_color.as_ref() else {
                    self.clear_spatial_highlight();
                    return;
                };
                if Some(preview.rendered_revision) != selection.key.source_revision {
                    self.clear_spatial_highlight();
                    return;
                }
                Some(Arc::clone(&preview.image))
            }
        };
        self.spatial_highlight = None;
        self.spatial_requested = Some(selection);
        self.spatial_worker.submit(SpatialHighlightRequest {
            selection,
            frame: Arc::clone(&loaded.frame),
            display_image,
        });
    }

    fn clear_spatial_highlight(&mut self) {
        if self.spatial_requested.is_none() && self.spatial_highlight.is_none() {
            return;
        }
        self.spatial_worker.cancel();
        self.spatial_requested = None;
        self.spatial_highlight = None;
    }

    fn poll_spatial_highlight_result(&mut self) {
        let Some(result) = self.spatial_worker.take_ready() else {
            return;
        };
        if self.spatial_requested != Some(result.selection)
            || self.analysis_panel.current_key() != Some(result.selection.key)
        {
            return;
        }
        match result.result {
            Ok(payload) => {
                self.spatial_highlight = Some(SpatialHighlight {
                    selection: result.selection,
                    mask: payload.mask,
                    overlay_image: payload.overlay_image,
                });
            }
            Err(error) => {
                tracing::warn!(
                    operation = "build_histogram_spatial_highlight",
                    generation = result.selection.key.generation,
                    error = %error,
                    "spatial highlight failed"
                );
                self.spatial_requested = None;
                self.spatial_highlight = None;
            }
        }
    }

    fn image_histogram_hover(&self) -> Option<ImageHistogramHover> {
        let key = self.analysis_panel.current_key()?;
        let loaded = self.loaded.as_ref()?;
        let cursor = self.viewer.cursor?;
        if loaded.generation != key.generation || !key.roi.contains(cursor.x, cursor.y) {
            return None;
        }
        let row_width = usize::try_from(loaded.frame.spec.width).ok()?;
        let x = usize::try_from(cursor.x).ok()?;
        let y = usize::try_from(cursor.y).ok()?;
        let index = y.checked_mul(row_width)?.checked_add(x)?;
        let sample = match key.domain {
            AnalysisDomain::RawBayer => HistogramPixelSample::Raw {
                site: loaded.frame.spec.bayer.site_at(cursor.x, cursor.y),
                value: *loaded.frame.pixels().get(index)?,
            },
            AnalysisDomain::DisplayRgb => {
                let preview = loaded.installed_color.as_ref()?;
                if Some(preview.rendered_revision) != key.source_revision {
                    return None;
                }
                HistogramPixelSample::Display(display_histogram_sample(
                    *preview.image.pixels.get(index)?,
                ))
            }
        };
        Some(ImageHistogramHover {
            key,
            x: cursor.x,
            y: cursor.y,
            sample,
        })
    }

    fn handle_viewer_action(&mut self, action: ViewerAction) {
        match action {
            ViewerAction::CommitRoi(roi) => self.commit_roi(roi),
            ViewerAction::ResetRoi => self.reset_roi_to_full_frame(),
        }
    }

    fn reset_roi_to_full_frame(&mut self) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        self.commit_roi(Roi {
            x: 0,
            y: 0,
            width: loaded.frame.spec.width,
            height: loaded.frame.spec.height,
        });
    }

    fn commit_roi(&mut self, roi: Roi) {
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        let Some(roi) = roi.clamped_to(loaded.frame.spec.width, loaded.frame.spec.height) else {
            return;
        };
        if loaded.roi == roi {
            return;
        }
        tracing::debug!(
            operation = "commit_roi",
            generation = loaded.generation,
            roi_x = roi.x,
            roi_y = roi.y,
            roi_width = roi.width,
            roi_height = roi.height,
            "committed viewer ROI"
        );
        loaded.roi = roi;
        loaded.stats = None;
        self.analysis_pending_active = None;
    }

    fn ensure_analysis(&mut self) {
        let Some(loaded) = self.loaded.as_ref() else {
            return;
        };
        let chart_roi = self.analysis_panel.scope.resolve(
            loaded.roi,
            loaded.frame.spec.width,
            loaded.frame.spec.height,
        );
        let (source_revision, display_image) = match self.analysis_panel.domain {
            AnalysisDomain::RawBayer => (None, None),
            AnalysisDomain::DisplayRgb => {
                let Some(preview) = &loaded.installed_color else {
                    self.analysis_panel.wait_for_source();
                    return;
                };
                (
                    Some(preview.rendered_revision),
                    Some(Arc::clone(&preview.image)),
                )
            }
        };
        let key = AnalysisKey {
            generation: loaded.generation,
            source_revision,
            roi: chart_roi,
            domain: self.analysis_panel.domain,
        };
        let desired = self.analysis_panel.set_desired(key);
        let chart_ready = self.analysis_panel.has_current(key);
        let stats_ready = loaded.stats.is_some();
        let stats_pending = self.analysis_pending_active == Some((loaded.generation, loaded.roi));
        if desired != DesiredAnalysis::Submit && (stats_ready || stats_pending) {
            return;
        }
        let compute_chart = !chart_ready;
        self.analysis_worker.submit(AnalysisRequest {
            key,
            active_roi: loaded.roi,
            compute_chart,
            frame: Arc::clone(&loaded.frame),
            display_image,
        });
        self.analysis_pending_active = Some((loaded.generation, loaded.roi));
    }

    fn poll_analysis_result(&mut self) {
        let Some(result) = self.analysis_worker.take_ready() else {
            return;
        };
        let key = result.key;
        let accepted = self.analysis_panel.accept_result(result);
        let Some((active_roi, stats)) = accepted else {
            tracing::debug!(
                operation = "poll_histogram_analysis",
                generation = key.generation,
                revision = ?key.source_revision,
                domain = key.domain.label(),
                "dropped stale or failed histogram analysis"
            );
            return;
        };
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        if loaded.generation == key.generation && loaded.roi == active_roi {
            loaded.stats = Some(stats);
            self.analysis_pending_active = None;
            tracing::debug!(
                operation = "install_histogram_analysis",
                generation = key.generation,
                revision = ?key.source_revision,
                domain = key.domain.label(),
                "installed histogram analysis"
            );
        }
    }

    fn render_status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let Some(loaded) = &self.loaded else {
                ui.label("Ready");
                return;
            };
            self.render_loaded_status(ui, loaded);
        });
    }

    fn render_loaded_status(&self, ui: &mut egui::Ui, loaded: &LoadedRaw) {
        let file_name = loaded
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<raw>");
        let displayed_bayer = loaded.displayed_bayer(self.display_mode);

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(format!("{:.0}%", self.viewer.zoom * 100.0));
            ui.separator();
            ui.label(self.display_mode.label());
            self.render_diagnostic_badges(ui, loaded);
            ui.separator();
            ui.label(format!("ROI: {}×{}", loaded.roi.width, loaded.roi.height));
            ui.separator();
            ui.allocate_ui_with_layout(
                ui.available_size(),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add(egui::Label::new(file_name).truncate());
                    ui.separator();
                    ui.label(format!(
                        "{}×{} · {}-bit · {}",
                        loaded.frame.spec.width,
                        loaded.frame.spec.height,
                        loaded.frame.spec.bit_depth,
                        bayer_label(displayed_bayer).to_uppercase()
                    ));
                },
            );
        });
    }

    fn render_diagnostic_badges(&self, ui: &mut egui::Ui, loaded: &LoadedRaw) {
        if loaded.diagnostics.has_out_of_range() {
            ui.separator();
            ui.colored_label(
                egui::Color32::YELLOW,
                format!("RAW range: {}", loaded.diagnostics.out_of_range_pixels),
            );
        }
        if self.display_mode != DisplayMode::Color {
            return;
        }
        let Some(preview) = &loaded.installed_color else {
            return;
        };
        if preview.diagnostics.display_clipped_channels > 0 {
            ui.separator();
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "RGB clipped: {}",
                    preview.diagnostics.display_clipped_channels
                ),
            );
        }
        if preview.diagnostics.missing_neighbor_channels > 0 {
            ui.separator();
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "Demosaic edge: {}",
                    preview.diagnostics.missing_neighbor_channels
                ),
            );
        }
    }

    fn render_raw_open_dialog(&mut self, context: &egui::Context) {
        let Some(request) = self.raw_dialog.show(context) else {
            return;
        };
        let attempt = self.next_load_attempt;
        self.next_load_attempt = self.next_load_attempt.saturating_add(1);
        if attempt > 1 {
            self.notifications
                .clear_scope(NotificationScope::LoadAttempt(attempt - 1));
        }
        let path = request.path.clone();
        match self.load_raw(context, attempt, request) {
            Ok(()) => self.raw_dialog.close(context),
            Err(error) => {
                let message = error.to_string();
                tracing::error!(
                    operation = "load_raw",
                    attempt,
                    path = %path.display(),
                    error = %format_args!("{error:#}"),
                    "RAW loading failed"
                );
                self.notifications.push_once(UiNotification::error(
                    NotificationKey::RawLoadFailed { attempt },
                    "RAW load failed",
                    &message,
                ));
                self.raw_dialog.set_error(message);
            }
        }
    }

    fn load_raw(
        &mut self,
        context: &egui::Context,
        attempt: u64,
        request: LocalRawAnalyzeRequest,
    ) -> anyhow::Result<()> {
        tracing::debug!(operation = "load_raw", attempt, path = %request.path.display(), "starting RAW load");
        let raw_loader = LocalRawLoader;
        let report = Workflow::load_raw_and_analyze(&raw_loader, request)?;
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let loaded = LoadedRaw::from_report(context, report, generation);
        let notification = loaded.diagnostics.first_out_of_range().map(|first| {
            UiNotification::raw_range(
                generation,
                loaded.frame.spec.bit_depth,
                loaded.frame.spec.max_code_value(),
                loaded.diagnostics.out_of_range_pixels,
                loaded.diagnostics.observed_max,
                (first.x, first.y, first.value),
                context.input(|input| input.time),
            )
        });
        tracing::debug!(
            operation = "load_raw",
            attempt,
            generation,
            path = %loaded.path.display(),
            width = loaded.frame.spec.width,
            height = loaded.frame.spec.height,
            bit_depth = loaded.frame.spec.bit_depth,
            "RAW loaded"
        );
        if loaded.diagnostics.has_out_of_range() {
            tracing::warn!(
                attempt,
                operation = "load_raw",
                generation,
                path = %loaded.path.display(),
                bit_depth = loaded.frame.spec.bit_depth,
                out_of_range_pixels = loaded.diagnostics.out_of_range_pixels,
                observed_max = loaded.diagnostics.observed_max,
                "RAW samples exceed declared bit-depth range"
            );
        }
        if let Some(previous) = &self.loaded {
            self.notifications
                .clear_scope(NotificationScope::ImageGeneration(previous.generation));
            self.analysis_panel.clear_generation(previous.generation);
            self.analysis_worker.cancel();
            self.clear_spatial_highlight();
            self.analysis_pending_active = None;
        }
        self.loaded = Some(loaded);
        self.analysis_panel.open_for_first_image();
        if let Some(notification) = notification {
            self.notifications.push_once(notification);
        }
        self.viewer = ImageViewerState::default();
        self.display_mode = DisplayMode::Color;
        self.request_current_color();
        Ok(())
    }

    fn request_current_color(&mut self) {
        let request = {
            let Some(loaded) = self.loaded.as_mut() else {
                return;
            };
            let revision = loaded.color_edit.revision;
            if loaded.color_edit.submitted_revision == Some(revision) {
                return;
            }
            if let Err(error) = loaded
                .color_edit
                .params
                .validate(loaded.frame.spec.max_code_value())
            {
                tracing::warn!(
                    operation = "validate_color_params",
                    generation = loaded.generation,
                    revision,
                    error = %error,
                    "color parameter validation rejected"
                );
                loaded.color_edit.mark_error(error.to_string());
                return;
            }
            let request = ColorRenderRequest {
                frame_generation: loaded.generation,
                revision,
                frame: Arc::clone(&loaded.frame),
                params: loaded.color_edit.params,
            };
            loaded.color_edit.mark_submitted();
            request
        };
        tracing::debug!(
            operation = "submit_color_render",
            generation = request.frame_generation,
            revision = request.revision,
            "submitted color render"
        );
        self.color_worker.submit(request);
    }

    fn poll_color_result(&mut self, context: &egui::Context) {
        let Some(result) = self.color_worker.take_ready() else {
            return;
        };
        let Some(loaded) = self.loaded.as_mut() else {
            tracing::debug!(
                operation = "poll_color_result",
                generation = result.frame_generation,
                revision = result.revision,
                "dropped color result because image is closed"
            );
            return;
        };
        if !color_result_matches(
            loaded.generation,
            loaded.color_edit.revision,
            result.frame_generation,
            result.revision,
        ) {
            tracing::debug!(
                operation = "poll_color_result",
                generation = result.frame_generation,
                revision = result.revision,
                "dropped stale color result"
            );
            return;
        }
        match result.rendered {
            Ok(rendered) => {
                loaded.install_color(
                    context,
                    result.revision,
                    result.params,
                    rendered.image,
                    rendered.diagnostics,
                );
                loaded.color_edit.render_error = None;
                tracing::debug!(
                    operation = "install_color_render",
                    generation = loaded.generation,
                    revision = result.revision,
                    "installed color render"
                );
            }
            Err(error) => {
                loaded.color_edit.mark_error(error.clone());
                tracing::error!(
                    operation = "render_color",
                    generation = loaded.generation,
                    revision = result.revision,
                    error = %error,
                    "accepted color render failed"
                );
                self.notifications.push_once(UiNotification::error(
                    NotificationKey::ColorRenderFailed {
                        generation: loaded.generation,
                        revision: result.revision,
                    },
                    "Color preview failed",
                    error,
                ));
            }
        }
    }
}

const fn color_result_matches(
    loaded_generation: u64,
    desired_revision: u64,
    result_generation: u64,
    result_revision: u64,
) -> bool {
    loaded_generation == result_generation && desired_revision == result_revision
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use camera_toolbox_app::LocalRawAnalyzeReport;
    use camera_toolbox_core::{BayerPattern, RawFrame, RawSpec, Roi, analyze_roi};
    use eframe::egui::{self, accesskit::Role};

    use super::{CameraToolboxApp, LoadedRaw, color_result_matches};

    const TEST_VIEWPORT: egui::Vec2 = egui::vec2(640.0, 360.0);

    /// `AccessKit` 使用 `f64` 屏幕坐标；测试 viewport 有界，可安全转换为 `egui` 的 `f32` 坐标。
    #[allow(clippy::cast_possible_truncation)]
    fn accesskit_rect_center(rect: egui::accesskit::Rect) -> egui::Pos2 {
        egui::pos2(
            ((rect.x0 + rect.x1) * 0.5) as f32,
            ((rect.y0 + rect.y1) * 0.5) as f32,
        )
    }

    fn run_app_frame(
        context: &egui::Context,
        app: &mut CameraToolboxApp,
        frame: &mut eframe::Frame,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, TEST_VIEWPORT)),
            ..Default::default()
        };
        input.events = events;
        context.run_ui(input, |ui| eframe::App::ui(app, ui, frame))
    }

    fn app_with_loaded_raw(context: &egui::Context) -> CameraToolboxApp {
        let spec = RawSpec {
            width: 2,
            height: 2,
            bit_depth: 10,
            bayer: BayerPattern::Rggb,
        };
        let frame = RawFrame::new(spec, vec![64, 128, 256, 512]).unwrap();
        let roi = Roi {
            x: 0,
            y: 0,
            width: frame.spec.width,
            height: frame.spec.height,
        };
        let report = LocalRawAnalyzeReport {
            path: PathBuf::from("fixture.raw"),
            stats: analyze_roi(&frame, roi).unwrap(),
            frame,
            roi,
        };
        let mut app = CameraToolboxApp::new(context).unwrap();
        app.loaded = Some(LoadedRaw::from_report(context, report, 1));
        app.analysis_panel.open_for_first_image();
        app
    }

    #[test]
    fn color_panel_bottom_gain_remains_reachable_in_short_viewport() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut app = app_with_loaded_raw(&context);
        let mut frame = eframe::Frame::_new_kittest();
        let panel_position = egui::pos2(500.0, 100.0);

        run_app_frame(&context, &mut app, &mut frame, Vec::new());
        run_app_frame(
            &context,
            &mut app,
            &mut frame,
            vec![egui::Event::PointerMoved(panel_position)],
        );
        run_app_frame(
            &context,
            &mut app,
            &mut frame,
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -1_000.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::default(),
            }],
        );
        let output = run_app_frame(&context, &mut app, &mut frame, Vec::new());
        let target = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree is enabled")
            .nodes
            .into_iter()
            .filter_map(|(_, node)| {
                (node.role() == Role::SpinButton)
                    .then(|| node.bounds())
                    .flatten()
            })
            .filter(|bounds| bounds.x0 >= 360.0 && bounds.y0 >= 0.0 && bounds.y1 <= 200.0)
            .max_by(|left, right| left.y1.total_cmp(&right.y1))
            .expect("scrolled Channel gain control is visible");
        let start = accesskit_rect_center(target);
        let end = start + egui::vec2(20.0, 0.0);
        let before = app.loaded.as_ref().unwrap().color_edit.params.gain.b;

        run_app_frame(
            &context,
            &mut app,
            &mut frame,
            vec![
                egui::Event::PointerMoved(start),
                egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        );
        run_app_frame(
            &context,
            &mut app,
            &mut frame,
            vec![egui::Event::PointerMoved(end)],
        );
        run_app_frame(
            &context,
            &mut app,
            &mut frame,
            vec![egui::Event::PointerButton {
                pos: end,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
        );

        let loaded = app.loaded.as_ref().unwrap();
        assert!((loaded.color_edit.params.gain.b - before).abs() > f32::EPSILON);
        assert!(loaded.color_edit.revision > 0);
    }

    #[test]
    fn color_result_requires_matching_generation_and_revision() {
        assert!(color_result_matches(4, 7, 4, 7));
        assert!(!color_result_matches(4, 7, 3, 7));
        assert!(!color_result_matches(4, 7, 4, 6));
    }
}
