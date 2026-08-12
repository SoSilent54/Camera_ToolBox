//! 显式导出的目标确认对话框；文件名与目录路径分开校验，避免把 SFTP 路径误当本地路径。

use camera_toolbox_app::EntryName;
use eframe::egui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExportPathSource {
    Local,
    #[cfg(feature = "platform-ssh")]
    Sftp,
}

impl ExportPathSource {
    const fn label(self) -> &'static str {
        match self {
            Self::Local => "Local",
            #[cfg(feature = "platform-ssh")]
            Self::Sftp => "SFTP",
        }
    }
}

pub(crate) struct ExportPathDialogPrefill {
    pub(crate) source: ExportPathSource,
    pub(crate) local_directory: Option<String>,
    #[cfg(feature = "platform-ssh")]
    pub(crate) sftp_directory: Option<String>,
}

pub(crate) struct ExportPathSelection {
    pub(crate) source: ExportPathSource,
    pub(crate) directory_path: String,
    pub(crate) file_name: EntryName,
}

pub(crate) struct ExportNameDialogState {
    open: bool,
    title: String,
    source: ExportPathSource,
    local_directory: Option<String>,
    #[cfg(feature = "platform-ssh")]
    sftp_directory: Option<String>,
    file_name: String,
    error: Option<String>,
}

impl Default for ExportNameDialogState {
    fn default() -> Self {
        Self {
            open: false,
            title: String::new(),
            source: ExportPathSource::Local,
            local_directory: None,
            #[cfg(feature = "platform-ssh")]
            sftp_directory: None,
            file_name: String::new(),
            error: None,
        }
    }
}

impl ExportNameDialogState {
    pub(crate) fn open(
        &mut self,
        title: impl Into<String>,
        suggested_name: impl Into<String>,
        prefill: ExportPathDialogPrefill,
    ) {
        let requested_source = prefill.source;
        self.open = true;
        self.title = title.into();
        self.local_directory = prefill.local_directory;
        #[cfg(feature = "platform-ssh")]
        {
            self.sftp_directory = prefill.sftp_directory;
        }
        self.source = if self.source_available(requested_source) {
            requested_source
        } else {
            self.fallback_source()
        };
        self.file_name = suggested_name.into();
        self.error = None;
    }

    #[must_use]
    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn reject(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
        self.open = true;
    }

    /// 返回源类型、目录路径与经 `EntryName` 校验的单层文件名；目录由 Explorer 按源类型解析。
    pub(crate) fn show(&mut self, context: &egui::Context) -> Option<ExportPathSelection> {
        if !self.open {
            return None;
        }

        if !self.selected_source_available() {
            self.source = ExportPathSource::Local;
        }

        let mut accepted = None;
        let mut close_after_submit = false;
        let mut open = self.open;
        egui::Window::new(self.title.clone())
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(context, |ui| {
                ui.label("Destination source");
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(self.local_directory.is_some(), |ui| {
                        ui.selectable_value(
                            &mut self.source,
                            ExportPathSource::Local,
                            ExportPathSource::Local.label(),
                        );
                    });
                    #[cfg(feature = "platform-ssh")]
                    ui.add_enabled_ui(self.sftp_directory.is_some(), |ui| {
                        ui.selectable_value(
                            &mut self.source,
                            ExportPathSource::Sftp,
                            ExportPathSource::Sftp.label(),
                        );
                    });
                });
                ui.separator();

                ui.label(match self.source {
                    ExportPathSource::Local => "Local directory path",
                    #[cfg(feature = "platform-ssh")]
                    ExportPathSource::Sftp => "SFTP directory path",
                });
                let path_hint = match self.source {
                    ExportPathSource::Local => "/home/user/output",
                    #[cfg(feature = "platform-ssh")]
                    ExportPathSource::Sftp => "/tmp/output",
                };
                if let Some(directory) = self.selected_directory_mut() {
                    ui.add(
                        egui::TextEdit::singleline(directory)
                            .desired_width(420.0)
                            .hint_text(path_hint),
                    );
                } else {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        "This destination source is not available in the current session.",
                    );
                }

                ui.label("File name");
                let file_response = ui.add(
                    egui::TextEdit::singleline(&mut self.file_name)
                        .desired_width(300.0)
                        .hint_text("result.yaml"),
                );
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::RED, error);
                }
                ui.horizontal(|ui| {
                    let submit = ui.button("Save new file").clicked()
                        || (file_response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                    if submit {
                        match self.accept_selection() {
                            Ok(selection) => {
                                accepted = Some(selection);
                                close_after_submit = true;
                            }
                            Err(error) => self.error = Some(error),
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

    fn selected_source_available(&self) -> bool {
        self.source_available(self.source)
    }

    fn source_available(&self, source: ExportPathSource) -> bool {
        match source {
            ExportPathSource::Local => self.local_directory.is_some(),
            #[cfg(feature = "platform-ssh")]
            ExportPathSource::Sftp => self.sftp_directory.is_some(),
        }
    }

    fn fallback_source(&self) -> ExportPathSource {
        if self.local_directory.is_some() {
            ExportPathSource::Local
        } else {
            #[cfg(feature = "platform-ssh")]
            if self.sftp_directory.is_some() {
                return ExportPathSource::Sftp;
            }
            ExportPathSource::Local
        }
    }

    fn selected_directory(&self) -> Option<&String> {
        match self.source {
            ExportPathSource::Local => self.local_directory.as_ref(),
            #[cfg(feature = "platform-ssh")]
            ExportPathSource::Sftp => self.sftp_directory.as_ref(),
        }
    }

    fn selected_directory_mut(&mut self) -> Option<&mut String> {
        match self.source {
            ExportPathSource::Local => self.local_directory.as_mut(),
            #[cfg(feature = "platform-ssh")]
            ExportPathSource::Sftp => self.sftp_directory.as_mut(),
        }
    }

    fn accept_selection(&self) -> Result<ExportPathSelection, String> {
        let directory_path = self
            .selected_directory()
            .ok_or_else(|| "Selected destination source is not available.".to_owned())?
            .trim()
            .to_owned();
        if directory_path.is_empty() {
            return Err("Destination directory path must not be empty.".to_owned());
        }
        let file_name = EntryName::new(&self.file_name).map_err(|error| error.to_string())?;
        Ok(ExportPathSelection {
            source: self.source,
            directory_path,
            file_name,
        })
    }
}
