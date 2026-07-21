//! Explorer 目录内显式导出的文件名确认对话框。

use camera_toolbox_app::EntryName;
use eframe::egui;

#[derive(Default)]
pub(crate) struct ExportNameDialogState {
    open: bool,
    title: String,
    file_name: String,
    error: Option<String>,
}

impl ExportNameDialogState {
    pub(crate) fn open(&mut self, title: impl Into<String>, suggested_name: impl Into<String>) {
        self.open = true;
        self.title = title.into();
        self.file_name = suggested_name.into();
        self.error = None;
    }

    #[must_use]
    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    /// 返回经 `EntryName` 校验的单层文件名；目录始终由 Explorer 当前 view 决定。
    pub(crate) fn show(
        &mut self,
        context: &egui::Context,
        destination_label: &str,
    ) -> Option<EntryName> {
        if !self.open {
            return None;
        }

        let mut accepted = None;
        let mut close_after_submit = false;
        let mut open = self.open;
        egui::Window::new(self.title.clone())
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(context, |ui| {
                ui.label("Current Explorer directory");
                ui.monospace(destination_label);
                ui.separator();
                ui.label("File name");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.file_name)
                        .desired_width(360.0)
                        .hint_text("result.yaml"),
                );
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::RED, error);
                }
                ui.horizontal(|ui| {
                    if ui.button("Save new file").clicked()
                        || (response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                    {
                        match EntryName::new(&self.file_name) {
                            Ok(name) => {
                                accepted = Some(name);
                                close_after_submit = true;
                            }
                            Err(error) => self.error = Some(error.to_string()),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        close_after_submit = true;
                    }
                });
            });
        self.open = open && !close_after_submit;
        accepted
    }
}
