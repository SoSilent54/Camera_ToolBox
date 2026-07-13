//! GUI 顶层状态与界面编排。

use std::sync::Arc;

use camera_toolbox_adapters::LocalRawLoader;
use camera_toolbox_app::{LocalRawAnalyzeRequest, Workflow};
use eframe::egui;

use crate::{
    color_controls::{DisplayMode, render_color_controls},
    color_worker::{ColorRenderRequest, ColorRenderWorker},
    notification::{NotificationCenter, NotificationKey, NotificationScope, UiNotification},
    raw_dialog::RawOpenDialogState,
    viewer::{
        HoverNeighborhood, HoverViewSettings, ImageViewerState, LoadedRaw, bayer_label,
        render_viewer,
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

        egui::Panel::top("menu_bar").show(ui, |ui| {
            self.render_menu_bar(ui);
        });
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            self.render_status_bar(ui);
        });
        self.render_color_panel(ui);
        let viewer_rect = egui::CentralPanel::default()
            .show(ui, |ui| {
                render_viewer(
                    ui,
                    self.loaded.as_ref(),
                    &mut self.viewer,
                    self.display_mode,
                    self.hover_view,
                )
            })
            .inner;
        self.notifications
            .render(&context, viewer_rect, context.input(|input| input.time));
        self.render_raw_open_dialog(&context);
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
            }
            self.color_worker.cancel();
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
        ui.separator();
        ui.add_enabled(false, egui::Button::new("ROI Stats"));
        ui.add_enabled(false, egui::Button::new("Histogram"));
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
        if collapse {
            self.color_panel_expanded = false;
        }
        if should_submit {
            self.request_current_color();
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
        }
        self.loaded = Some(loaded);
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
    use super::color_result_matches;

    #[test]
    fn color_result_requires_matching_generation_and_revision() {
        assert!(color_result_matches(4, 7, 4, 7));
        assert!(!color_result_matches(4, 7, 3, 7));
        assert!(!color_result_matches(4, 7, 4, 6));
    }
}
