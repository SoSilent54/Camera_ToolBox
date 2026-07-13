//! GUI 顶层状态与界面编排。

use std::{fmt::Write as _, sync::Arc};

use camera_toolbox_adapters::LocalRawLoader;
use camera_toolbox_app::{LocalRawAnalyzeRequest, Workflow};
use eframe::egui;

use crate::{
    color_controls::{DisplayMode, render_color_controls},
    color_worker::{ColorRenderRequest, ColorRenderWorker},
    raw_dialog::RawOpenDialogState,
    viewer::{ImageViewerState, LoadedRaw, bayer_label, render_viewer},
};

pub(crate) struct CameraToolboxApp {
    loaded: Option<LoadedRaw>,
    viewer: ImageViewerState,
    raw_dialog: RawOpenDialogState,
    status_message: String,
    display_mode: DisplayMode,
    show_color_controls: bool,
    color_worker: ColorRenderWorker,
    next_generation: u64,
}

impl CameraToolboxApp {
    pub(crate) fn new(context: &egui::Context) -> std::io::Result<Self> {
        Ok(Self {
            loaded: None,
            viewer: ImageViewerState::default(),
            raw_dialog: RawOpenDialogState::default(),
            status_message: String::new(),
            display_mode: DisplayMode::Color,
            show_color_controls: true,
            color_worker: ColorRenderWorker::new(context)?,
            next_generation: 1,
        })
    }
}

impl eframe::App for CameraToolboxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.poll_color_result(&context);
        self.viewer.refresh_cursor(&context, self.loaded.as_ref());

        egui::Panel::top("menu_bar").show(ui, |ui| {
            self.render_menu_bar(ui);
        });
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            self.render_status_bar(ui);
        });
        self.render_color_panel(ui);
        egui::CentralPanel::default().show(ui, |ui| {
            render_viewer(
                ui,
                self.loaded.as_ref(),
                &mut self.viewer,
                self.display_mode,
            );
        });
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
            ui.menu_button("Tools", |ui| {
                ui.add_enabled(false, egui::Button::new("ROI Stats"));
                ui.add_enabled(false, egui::Button::new("Histogram"));
            });
            ui.menu_button("Help", |ui| {
                ui.label("Camera Toolbox");
                ui.label("Local Bayer RAW color viewer");
            });
        });
        if request_color {
            self.request_current_color();
        }
    }

    fn render_file_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Open Raw...").clicked() {
            self.raw_dialog.open(ui.ctx());
            self.set_status("Configure RAW parameters");
            ui.close();
        }
        if ui
            .add_enabled(self.loaded.is_some(), egui::Button::new("Close Image"))
            .clicked()
        {
            self.color_worker.cancel();
            self.loaded = None;
            self.viewer = ImageViewerState::default();
            self.set_status("Closed image");
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
        ui.add_enabled_ui(self.loaded.is_some(), |ui| {
            ui.checkbox(&mut self.show_color_controls, "Show Color Controls");
        });
        ui.separator();
        self.render_view_navigation(ui);
        request_color
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
        if !self.show_color_controls || self.loaded.is_none() {
            return;
        }
        let mut should_submit = false;
        egui::Panel::right("color_processing")
            .resizable(true)
            .default_size(280.0)
            .min_size(240.0)
            .max_size(420.0)
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
        if should_submit {
            self.request_current_color();
        }
    }

    fn render_status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if let Some(loaded) = &self.loaded {
                self.render_loaded_status(ui, loaded);
            } else {
                ui.label("Ready");
                ui.separator();
                ui.label("No image loaded");
            }
            if !self.status_message.is_empty() {
                ui.separator();
                ui.label(&self.status_message);
            }
        });
    }

    fn render_loaded_status(&self, ui: &mut egui::Ui, loaded: &LoadedRaw) {
        let file_name = loaded
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<raw>");
        let displayed_bayer = if self.display_mode == DisplayMode::Color {
            loaded
                .installed_color
                .as_ref()
                .map_or(loaded.frame.spec.bayer, |preview| {
                    preview.rendered_params.bayer
                })
        } else {
            loaded.frame.spec.bayer
        };
        ui.label(file_name);
        ui.separator();
        ui.label(format!(
            "{}x{} bit={} {} u16le",
            loaded.frame.spec.width,
            loaded.frame.spec.height,
            loaded.frame.spec.bit_depth,
            bayer_label(displayed_bayer)
        ));
        ui.separator();
        ui.label(self.display_mode.label());
        ui.separator();
        ui.label(format!("zoom={:.0}%", self.viewer.zoom * 100.0));
        ui.separator();
        self.render_cursor_status(ui, loaded);
        ui.separator();
        ui.label(format!(
            "roi={},{},{},{} min={} max={} mean={:.2} sat={}/{}",
            loaded.roi.x,
            loaded.roi.y,
            loaded.roi.width,
            loaded.roi.height,
            loaded.stats.min,
            loaded.stats.max,
            loaded.stats.mean,
            loaded.stats.saturated_pixels,
            loaded.stats.total_pixels
        ));
        if loaded.diagnostics.has_out_of_range() {
            ui.separator();
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "out-of-range={} observed-max={}",
                    loaded.diagnostics.out_of_range_pixels, loaded.diagnostics.observed_max
                ),
            );
        }
        self.render_color_diagnostics(ui, loaded);
    }

    fn render_cursor_status(&self, ui: &mut egui::Ui, loaded: &LoadedRaw) {
        let Some(cursor) = self.viewer.cursor else {
            ui.label("x=- y=- raw=-");
            return;
        };
        let mut text = format!("x={} y={} raw={}", cursor.x, cursor.y, cursor.value);
        if let Some(color) = loaded.rendered_pixel(self.display_mode, cursor)
            && let Some(preview) = &loaded.installed_color
        {
            let site = preview.rendered_params.bayer.site_at(cursor.x, cursor.y);
            let _ = write!(
                text,
                " cfa={site:?} rgb8={},{},{}",
                color.srgb.r, color.srgb.g, color.srgb.b
            );
        }
        ui.label(text);
    }

    fn render_color_diagnostics(&self, ui: &mut egui::Ui, loaded: &LoadedRaw) {
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
                    "color-clipped-channels={}",
                    preview.diagnostics.display_clipped_channels
                ),
            );
        }
        if preview.diagnostics.missing_neighbor_channels > 0 {
            ui.separator();
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "missing-color-neighbors={}",
                    preview.diagnostics.missing_neighbor_channels
                ),
            );
        }
    }

    fn render_raw_open_dialog(&mut self, context: &egui::Context) {
        let Some(request) = self.raw_dialog.show(context) else {
            return;
        };
        match self.load_raw(context, request) {
            Ok(()) => self.raw_dialog.close(context),
            Err(error) => {
                let message = error.to_string();
                self.status_message = format!("Open RAW failed: {message}");
                self.raw_dialog.set_error(message);
            }
        }
    }

    fn set_status(&mut self, message: &str) {
        self.status_message.clear();
        self.status_message.push_str(message);
    }

    fn load_raw(
        &mut self,
        context: &egui::Context,
        request: LocalRawAnalyzeRequest,
    ) -> anyhow::Result<()> {
        let raw_loader = LocalRawLoader;
        let report = Workflow::load_raw_and_analyze(&raw_loader, request)?;
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let loaded = LoadedRaw::from_report(context, report, generation);
        self.status_message = if loaded.diagnostics.has_out_of_range() {
            format!(
                "Loaded {} with {} out-of-range pixels; original values preserved",
                loaded.path.display(),
                loaded.diagnostics.out_of_range_pixels
            )
        } else {
            format!("Loaded {}", loaded.path.display())
        };
        self.loaded = Some(loaded);
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
        self.color_worker.submit(request);
    }

    fn poll_color_result(&mut self, context: &egui::Context) {
        let Some(result) = self.color_worker.take_ready() else {
            return;
        };
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        if !color_result_matches(
            loaded.generation,
            loaded.color_edit.revision,
            result.frame_generation,
            result.revision,
        ) {
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
                self.status_message =
                    format!("Rendered color preview revision {}", result.revision);
            }
            Err(error) => {
                loaded.color_edit.mark_error(error.clone());
                self.status_message = format!("Color preview failed: {error}");
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
