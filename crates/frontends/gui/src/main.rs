mod app;
mod color_controls;
mod color_worker;
mod notification;
mod raw_dialog;
mod viewer;

use std::sync::Arc;

use anyhow::{Result, anyhow};
use app::CameraToolboxApp;
use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

const NOTO_SANS_CJK: &[u8] = include_bytes!("../assets/fonts/NotoSansCJK-Regular.ttc");

fn main() -> Result<()> {
    let _logging = camera_toolbox_logging::init();
    tracing::debug!(operation = "gui_start", "starting GUI frontend");
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Camera Toolbox",
        native_options,
        Box::new(|creation_context| {
            install_cjk_fonts(&creation_context.egui_ctx);
            CameraToolboxApp::new(&creation_context.egui_ctx)
                .map(|app| Box::new(app) as Box<dyn eframe::App>)
                .map_err(|error| {
                    tracing::error!(operation = "create_gui_app", error = %error, "GUI initialization failed");
                    error.into()
                })
        }),
    )
    .map_err(|error| {
        tracing::error!(operation = "run_gui", error = %error, "GUI frontend stopped with error");
        anyhow!(error.to_string())
    })?;
    tracing::debug!(operation = "gui_exit", "GUI frontend exited");
    Ok(())
}

/// 安装随程序发布的简体中文与等宽中文字体，避免依赖目标系统字体。
fn install_cjk_fonts(context: &egui::Context) {
    let mut definitions = FontDefinitions::default();

    let mut proportional = FontData::from_static(NOTO_SANS_CJK);
    proportional.index = 2; // Noto Sans CJK SC
    definitions
        .font_data
        .insert("noto-sans-cjk-sc".to_owned(), Arc::new(proportional));

    let mut monospace = FontData::from_static(NOTO_SANS_CJK);
    monospace.index = 7; // Noto Sans Mono CJK SC
    definitions
        .font_data
        .insert("noto-sans-mono-cjk-sc".to_owned(), Arc::new(monospace));

    definitions
        .families
        .get_mut(&FontFamily::Proportional)
        .expect("default proportional family exists")
        .push("noto-sans-cjk-sc".to_owned());
    definitions
        .families
        .get_mut(&FontFamily::Monospace)
        .expect("default monospace family exists")
        .push("noto-sans-mono-cjk-sc".to_owned());

    context.set_fonts(definitions);
}
