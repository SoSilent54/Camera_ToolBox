//! 无 header YUV420SP 参数确认窗口。

use std::path::PathBuf;

use camera_toolbox_core::{ChromaOrder, Yuv420SpSpec, YuvMatrix, YuvRange};
use eframe::egui;

pub(crate) struct YuvOpenDialogState {
    open: bool,
    path: PathBuf,
    width: u32,
    height: u32,
    y_stride: usize,
    chroma_stride: usize,
    chroma_order: ChromaOrder,
    chroma_order_hint: Option<ChromaOrder>,
    matrix: YuvMatrix,
    range: YuvRange,
}

impl Default for YuvOpenDialogState {
    fn default() -> Self {
        Self {
            open: false,
            path: PathBuf::new(),
            width: 1920,
            height: 1080,
            y_stride: 1920,
            chroma_stride: 1920,
            chroma_order: ChromaOrder::Uv,
            chroma_order_hint: None,
            matrix: YuvMatrix::Bt601,
            range: YuvRange::Limited,
        }
    }
}

impl YuvOpenDialogState {
    pub(crate) fn open(&mut self, path: PathBuf, chroma_order_hint: Option<ChromaOrder>) {
        self.path = path;
        self.chroma_order_hint = chroma_order_hint;
        if let Some(order) = chroma_order_hint {
            self.chroma_order = order;
        }
        self.open = true;
    }

    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
    }

    pub(crate) fn show(&mut self, context: &egui::Context) -> Option<Yuv420SpSpec> {
        if !self.open {
            return None;
        }
        let mut window_open = true;
        let mut confirmed = None;
        let mut cancel = false;
        egui::Window::new("Open Headerless YUV420SP")
            .collapsible(false)
            .resizable(false)
            .open(&mut window_open)
            .show(context, |ui| {
                ui.label(self.path.display().to_string());
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "Headerless YUV has no dimensions or color metadata. Confirm every field before decoding.",
                );
                egui::Grid::new("yuv_open_parameters")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label("Width");
                        ui.add(egui::DragValue::new(&mut self.width).range(1..=u32::MAX));
                        ui.end_row();
                        ui.label("Height");
                        ui.add(egui::DragValue::new(&mut self.height).range(1..=u32::MAX));
                        ui.end_row();
                        ui.label("Y stride (bytes)");
                        ui.add(egui::DragValue::new(&mut self.y_stride).range(1..=usize::MAX));
                        ui.end_row();
                        ui.label("Chroma stride (bytes)");
                        ui.add(
                            egui::DragValue::new(&mut self.chroma_stride).range(1..=usize::MAX),
                        );
                        ui.end_row();
                    });
                if ui.button("Use tight strides").clicked() {
                    self.y_stride = self.width as usize;
                    self.chroma_stride = self.width as usize;
                }

                ui.horizontal(|ui| {
                    ui.label("Chroma order");
                    ui.add_enabled_ui(self.chroma_order_hint.is_none(), |ui| {
                        ui.selectable_value(&mut self.chroma_order, ChromaOrder::Uv, "UV / NV12");
                        ui.selectable_value(&mut self.chroma_order, ChromaOrder::Vu, "VU / NV21");
                    });
                });
                ui.horizontal(|ui| {
                    ui.label("Matrix");
                    ui.selectable_value(&mut self.matrix, YuvMatrix::Bt601, "BT.601");
                    ui.selectable_value(&mut self.matrix, YuvMatrix::Bt709, "BT.709");
                });
                ui.horizontal(|ui| {
                    ui.label("Range");
                    ui.selectable_value(&mut self.range, YuvRange::Limited, "Limited");
                    ui.selectable_value(&mut self.range, YuvRange::Full, "Full");
                });
                ui.weak("BT.601 Limited is only an unconfirmed prefill; the button below confirms it.");

                let spec = Yuv420SpSpec {
                    width: self.width,
                    height: self.height,
                    y_stride: self.y_stride,
                    chroma_stride: self.chroma_stride,
                    chroma_order: self.chroma_order,
                    matrix: self.matrix,
                    range: self.range,
                };
                let validation = spec.validate();
                if let Err(error) = &validation {
                    ui.colored_label(egui::Color32::LIGHT_RED, error.to_string());
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            validation.is_ok(),
                            egui::Button::new("Open with confirmed parameters"),
                        )
                        .clicked()
                    {
                        confirmed = Some(spec);
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if confirmed.is_some() || cancel || !window_open {
            self.open = false;
        }
        confirmed
    }
}
