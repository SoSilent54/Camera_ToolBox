//! GUI 顶层状态与界面编排。

use camera_toolbox_adapters::LocalRawLoader;
use camera_toolbox_app::{LocalRawAnalyzeRequest, Workflow};
use eframe::egui;

use crate::{
    raw_dialog::RawOpenDialogState,
    viewer::{ImageViewerState, LoadedRaw, bayer_label, render_viewer},
};

#[derive(Default)]
pub(crate) struct CameraToolboxApp {
    loaded: Option<LoadedRaw>,
    viewer: ImageViewerState,
    raw_dialog: RawOpenDialogState,
    status_message: String,
}

impl eframe::App for CameraToolboxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();

        egui::Panel::top("menu_bar").show(ui, |ui| {
            self.render_menu_bar(ui);
        });
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            self.render_status_bar(ui);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            render_viewer(ui, self.loaded.as_ref(), &mut self.viewer);
        });
        self.render_raw_open_dialog(&context);
    }
}

impl CameraToolboxApp {
    fn render_menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open Raw...").clicked() {
                    self.raw_dialog.open(ui.ctx());
                    self.set_status("Configure RAW parameters");
                    ui.close();
                }
                if ui
                    .add_enabled(self.loaded.is_some(), egui::Button::new("Close Image"))
                    .clicked()
                {
                    self.loaded = None;
                    self.viewer = ImageViewerState::default();
                    self.set_status("Closed image");
                    ui.close();
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });

            ui.menu_button("View", |ui| {
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
            });

            ui.menu_button("Tools", |ui| {
                ui.add_enabled(false, egui::Button::new("ROI Stats"));
                ui.add_enabled(false, egui::Button::new("Histogram"));
            });

            ui.menu_button("Help", |ui| {
                ui.label("Camera Toolbox");
                ui.label("Local RAW viewer foundation");
            });
        });
    }

    fn render_status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if let Some(loaded) = &self.loaded {
                let file_name = loaded
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("<raw>");
                ui.label(file_name);
                ui.separator();
                ui.label(format!(
                    "{}x{} bit={} {} u16le",
                    loaded.frame.spec.width,
                    loaded.frame.spec.height,
                    loaded.frame.spec.bit_depth,
                    bayer_label(loaded.frame.spec.bayer)
                ));
                ui.separator();
                ui.label(format!("zoom={:.0}%", self.viewer.zoom * 100.0));
                ui.separator();
                if let Some(cursor) = self.viewer.cursor {
                    ui.label(format!(
                        "x={} y={} raw={}",
                        cursor.x, cursor.y, cursor.value
                    ));
                } else {
                    ui.label("x=- y=- raw=-");
                }
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
        let loaded = LoadedRaw::from_report(context, report);
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
        Ok(())
    }
}
