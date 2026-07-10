use anyhow::Result;
use eframe::egui;

fn main() -> Result<()> {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Camera Toolbox",
        native_options,
        Box::new(|_cc| Ok(Box::<CameraToolboxApp>::default())),
    )
    .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    Ok(())
}

#[derive(Default)]
struct CameraToolboxApp;

impl eframe::App for CameraToolboxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Camera Toolbox");
            ui.label(
                "egui GUI placeholder: image viewer and ROI tools will attach to app workflow.",
            );
        });
    }
}
