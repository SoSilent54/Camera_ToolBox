//! Explorer 文件表格、上下文菜单、编辑器、拖放和危险操作确认。

use camera_toolbox_app::{
    DirectoryRef, EntryName, FileEntry, FileKind, FileRef, FileSourceId, FileSystemCapabilities,
    SourcePath,
};
use eframe::egui;

#[derive(Debug, Clone)]
pub(super) enum MutationRequest {
    CreateDirectory {
        parent: DirectoryRef,
        name: EntryName,
    },
    Rename {
        reference: FileRef,
        new_name: EntryName,
    },
    Delete {
        entry: FileEntry,
    },
    Move {
        entry: FileEntry,
        destination: DirectoryRef,
    },
}

impl MutationRequest {
    pub(super) const fn capability_enabled(&self, capabilities: FileSystemCapabilities) -> bool {
        match self {
            Self::CreateDirectory { .. } => capabilities.create_directory,
            Self::Rename { .. } => capabilities.rename,
            Self::Delete { .. } => capabilities.delete,
            Self::Move { .. } => capabilities.move_entry,
        }
    }
}

#[derive(Debug)]
pub(super) enum BrowserCommand {
    NavigateUp,
    NavigateTo(SourcePath),
    Refresh,
    Open(FileRef),
    Mutate(MutationRequest),
}

#[derive(Debug, Clone)]
struct DraggedEntry {
    entry: FileEntry,
}

#[derive(Debug)]
enum EntryEditor {
    Rename {
        reference: FileRef,
        original_name: String,
        value: String,
        error: Option<String>,
        request_focus: bool,
    },
    NewFolder {
        parent: DirectoryRef,
        value: String,
        error: Option<String>,
        request_focus: bool,
    },
}

#[derive(Debug)]
enum PendingConfirmation {
    Delete(FileEntry),
    Move {
        entry: FileEntry,
        destination: DirectoryRef,
    },
}

#[derive(Default)]
pub(super) struct BrowserState {
    editor: Option<EntryEditor>,
    confirmation: Option<PendingConfirmation>,
    message: Option<String>,
}

impl BrowserState {
    pub(super) fn clear_transient(&mut self) {
        self.editor = None;
        self.confirmation = None;
        self.message = None;
    }

    pub(super) fn set_message(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
    }

    pub(super) fn complete_mutation(&mut self, request: &MutationRequest) {
        match (self.editor.as_ref(), request) {
            (
                Some(EntryEditor::Rename { reference, .. }),
                MutationRequest::Rename {
                    reference: completed,
                    ..
                },
            ) if reference == completed => self.editor = None,
            (
                Some(EntryEditor::NewFolder { parent, .. }),
                MutationRequest::CreateDirectory {
                    parent: completed, ..
                },
            ) if parent == completed => self.editor = None,
            _ => {}
        }
        self.message = None;
    }

    pub(super) fn fail_mutation(&mut self, request: &MutationRequest, error: String) {
        match (self.editor.as_mut(), request) {
            (
                Some(EntryEditor::Rename {
                    reference,
                    error: editor_error,
                    ..
                }),
                MutationRequest::Rename {
                    reference: failed, ..
                },
            ) if reference == failed => *editor_error = Some(error),
            (
                Some(EntryEditor::NewFolder {
                    parent,
                    error: editor_error,
                    ..
                }),
                MutationRequest::CreateDirectory { parent: failed, .. },
            ) if parent == failed => *editor_error = Some(error),
            _ => self.message = Some(error),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(
        &mut self,
        context: &egui::Context,
        ui: &mut egui::Ui,
        source_id: &FileSourceId,
        current_directory: &SourcePath,
        entries: &[FileEntry],
        selected: &mut Option<SourcePath>,
        capabilities: FileSystemCapabilities,
        busy: bool,
    ) -> Option<BrowserCommand> {
        let mut command = None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !current_directory.is_root() && !busy,
                    egui::Button::new("Up"),
                )
                .clicked()
            {
                command = Some(BrowserCommand::NavigateUp);
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Refresh"))
                .clicked()
            {
                command = Some(BrowserCommand::Refresh);
            }
            if ui
                .add_enabled(
                    capabilities.create_directory && !busy,
                    egui::Button::new("New Folder"),
                )
                .clicked()
            {
                self.begin_new_folder(DirectoryRef::new(
                    source_id.clone(),
                    current_directory.clone(),
                ));
            }
        });

        if let Some(message) = &self.message {
            ui.colored_label(egui::Color32::RED, message);
        }
        if let Some(editor_command) = self.render_new_folder_editor(ui, busy) {
            command = Some(editor_command);
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("Name");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.strong("Size");
            });
        });
        ui.separator();

        let current = DirectoryRef::new(source_id.clone(), current_directory.clone());
        let table = ui.vertical(|ui| {
            ui.set_min_height(ui.available_height());
            egui::ScrollArea::vertical().show(ui, |ui| {
                for entry in entries {
                    if command.is_none() {
                        command = self.render_entry_row(
                            ui,
                            entry,
                            selected,
                            &current,
                            capabilities,
                            busy,
                        );
                    } else {
                        self.render_entry_row(ui, entry, selected, &current, capabilities, busy);
                    }
                }
            });
        });
        table.response.context_menu(|ui| {
            if ui
                .add_enabled(
                    capabilities.create_directory && !busy,
                    egui::Button::new("New Folder"),
                )
                .clicked()
            {
                self.begin_new_folder(current.clone());
                ui.close();
            }
        });
        if self.confirmation.is_none()
            && let Some(payload) = table.response.dnd_release_payload::<DraggedEntry>()
        {
            match validate_move(&payload.entry, &current) {
                Ok(()) => {
                    self.confirmation = Some(PendingConfirmation::Move {
                        entry: payload.entry.clone(),
                        destination: current,
                    });
                }
                Err(error) => self.message = Some(error),
            }
        }

        if command.is_none() {
            command = self.render_confirmation(context, busy);
        } else {
            self.render_confirmation(context, busy);
        }
        command
    }

    fn render_entry_row(
        &mut self,
        ui: &mut egui::Ui,
        entry: &FileEntry,
        selected: &mut Option<SourcePath>,
        current_directory: &DirectoryRef,
        capabilities: FileSystemCapabilities,
        busy: bool,
    ) -> Option<BrowserCommand> {
        if self.editor_matches(entry) {
            return self.render_rename_editor(ui, busy);
        }

        let mut command = None;
        let icon = match entry.kind {
            FileKind::Directory => "[D]",
            FileKind::File => "[F]",
            FileKind::Symlink => "[L]",
            FileKind::Other => "[?]",
        };
        let selected_row = selected.as_ref() == Some(&entry.reference.path);
        let row = ui.horizontal(|ui| {
            let size_width = 76.0;
            let name_width =
                (ui.available_width() - size_width - ui.spacing().item_spacing.x).max(48.0);
            let response = ui.add_sized(
                [name_width, 22.0],
                egui::Button::selectable(selected_row, format!("{icon} {}", entry.name))
                    .sense(egui::Sense::click_and_drag()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.monospace(format_entry_size(entry));
            });
            response
        });
        let response = row.inner;
        if response.clicked_by(egui::PointerButton::Primary) {
            *selected = Some(entry.reference.path.clone());
        }
        if response.double_clicked_by(egui::PointerButton::Primary)
            && !busy
            && matches!(entry.kind, FileKind::File | FileKind::Directory)
        {
            command = open_command(entry);
        }
        if response.drag_started() && !busy {
            response.dnd_set_drag_payload(DraggedEntry {
                entry: entry.clone(),
            });
        }
        if entry.kind == FileKind::Directory
            && let Some(payload) = response.dnd_release_payload::<DraggedEntry>()
        {
            let destination = DirectoryRef::new(
                current_directory.source_id.clone(),
                entry.reference.path.clone(),
            );
            match validate_move(&payload.entry, &destination) {
                Ok(()) => {
                    self.confirmation = Some(PendingConfirmation::Move {
                        entry: payload.entry.clone(),
                        destination,
                    });
                }
                Err(error) => self.message = Some(error),
            }
        }

        let mut menu_action = None;
        response.context_menu(|ui| {
            if ui
                .add_enabled(
                    matches!(entry.kind, FileKind::File | FileKind::Directory) && !busy,
                    egui::Button::new("Open"),
                )
                .clicked()
            {
                menu_action = open_command(entry);
                ui.close();
            }
            if ui
                .add_enabled(capabilities.rename && !busy, egui::Button::new("Rename"))
                .clicked()
            {
                menu_action = Some(BrowserCommand::Refresh);
                self.begin_rename(entry);
                ui.close();
            }
            if ui
                .add_enabled(capabilities.delete && !busy, egui::Button::new("Delete"))
                .clicked()
            {
                self.confirmation = Some(PendingConfirmation::Delete(entry.clone()));
                ui.close();
            }
            ui.separator();
            if ui
                .add_enabled(
                    capabilities.create_directory && !busy,
                    egui::Button::new("New Folder"),
                )
                .clicked()
            {
                self.begin_new_folder(current_directory.clone());
                ui.close();
            }
        });
        if matches!(menu_action, Some(BrowserCommand::Refresh)) {
            None
        } else {
            menu_action.or(command)
        }
    }

    fn begin_rename(&mut self, entry: &FileEntry) {
        self.editor = Some(EntryEditor::Rename {
            reference: entry.reference.clone(),
            original_name: entry.name.as_str().to_owned(),
            value: entry.name.as_str().to_owned(),
            error: None,
            request_focus: true,
        });
        self.message = None;
    }

    fn begin_new_folder(&mut self, parent: DirectoryRef) {
        self.editor = Some(EntryEditor::NewFolder {
            parent,
            value: String::new(),
            error: None,
            request_focus: true,
        });
        self.message = None;
    }

    fn editor_matches(&self, entry: &FileEntry) -> bool {
        matches!(
            self.editor.as_ref(),
            Some(EntryEditor::Rename { reference, .. }) if reference == &entry.reference
        )
    }

    fn render_rename_editor(&mut self, ui: &mut egui::Ui, busy: bool) -> Option<BrowserCommand> {
        let Some(EntryEditor::Rename {
            value,
            error,
            request_focus,
            ..
        }) = self.editor.as_mut()
        else {
            return None;
        };
        let mut commit = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            let response = ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(value)
                    .id_source("explorer_inline_rename")
                    .desired_width(ui.available_width() - 76.0),
            );
            if *request_focus {
                response.request_focus();
                *request_focus = false;
            }
            commit = response.has_focus()
                && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            cancel = response.has_focus()
                && ui
                    .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        });
        if let Some(error) = error {
            ui.colored_label(egui::Color32::RED, error);
        }
        self.resolve_rename_editor(commit, cancel, busy)
    }

    fn resolve_rename_editor(
        &mut self,
        commit: bool,
        cancel: bool,
        busy: bool,
    ) -> Option<BrowserCommand> {
        if cancel {
            self.editor = None;
            return None;
        }
        if !commit || busy {
            return None;
        }
        let Some(EntryEditor::Rename {
            reference,
            original_name,
            value,
            error,
            ..
        }) = self.editor.as_mut()
        else {
            return None;
        };
        let name = match EntryName::new(value.trim().to_owned()) {
            Ok(name) => name,
            Err(validation) => {
                *error = Some(validation.to_string());
                return None;
            }
        };
        if name.as_str() == original_name {
            self.editor = None;
            return None;
        }
        Some(BrowserCommand::Mutate(MutationRequest::Rename {
            reference: reference.clone(),
            new_name: name,
        }))
    }

    fn render_new_folder_editor(
        &mut self,
        ui: &mut egui::Ui,
        busy: bool,
    ) -> Option<BrowserCommand> {
        let Some(EntryEditor::NewFolder {
            value,
            error,
            request_focus,
            ..
        }) = self.editor.as_mut()
        else {
            return None;
        };
        let mut commit = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            ui.label("[D]");
            let response = ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(value)
                    .id_source("explorer_new_folder")
                    .hint_text("New folder name"),
            );
            if *request_focus {
                response.request_focus();
                *request_focus = false;
            }
            commit = response.has_focus()
                && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            cancel = response.has_focus()
                && ui
                    .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        });
        if let Some(error) = error {
            ui.colored_label(egui::Color32::RED, error);
        }
        self.resolve_new_folder_editor(commit, cancel, busy)
    }

    fn resolve_new_folder_editor(
        &mut self,
        commit: bool,
        cancel: bool,
        busy: bool,
    ) -> Option<BrowserCommand> {
        if cancel {
            self.editor = None;
            return None;
        }
        if !commit || busy {
            return None;
        }
        let Some(EntryEditor::NewFolder {
            parent,
            value,
            error,
            ..
        }) = self.editor.as_mut()
        else {
            return None;
        };
        let name = match EntryName::new(value.trim().to_owned()) {
            Ok(name) => name,
            Err(validation) => {
                *error = Some(validation.to_string());
                return None;
            }
        };
        Some(BrowserCommand::Mutate(MutationRequest::CreateDirectory {
            parent: parent.clone(),
            name,
        }))
    }

    fn render_confirmation(
        &mut self,
        context: &egui::Context,
        busy: bool,
    ) -> Option<BrowserCommand> {
        let mut confirm = false;
        let mut cancel = false;
        let confirmation = self.confirmation.as_ref()?;
        let title = match confirmation {
            PendingConfirmation::Delete(_) => "Confirm Delete",
            PendingConfirmation::Move { .. } => "Confirm Move",
        };
        egui::Window::new(title)
            .id(egui::Id::new("explorer_confirmation"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(context, |ui| {
                match confirmation {
                    PendingConfirmation::Delete(entry) => {
                        ui.label(format!("Delete '{}' ?", entry.name));
                        if entry.kind == FileKind::Directory {
                            ui.colored_label(
                                egui::Color32::YELLOW,
                                "The directory and all descendants will be deleted.",
                            );
                        }
                    }
                    PendingConfirmation::Move { entry, destination } => {
                        ui.label(format!("Move '{}' to this directory?", entry.name));
                        ui.monospace(if destination.path.is_root() {
                            "/"
                        } else {
                            destination.path.as_str()
                        });
                    }
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!busy, egui::Button::new("Confirm"))
                        .clicked()
                    {
                        confirm = true;
                    }
                    if ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked() {
                        cancel = true;
                    }
                });
            });
        self.resolve_confirmation(confirm, cancel, busy)
    }

    fn resolve_confirmation(
        &mut self,
        confirm: bool,
        cancel: bool,
        busy: bool,
    ) -> Option<BrowserCommand> {
        if cancel {
            self.confirmation = None;
            return None;
        }
        if !confirm || busy {
            return None;
        }
        let request = match self.confirmation.take().expect("confirmation exists") {
            PendingConfirmation::Delete(entry) => MutationRequest::Delete { entry },
            PendingConfirmation::Move { entry, destination } => {
                MutationRequest::Move { entry, destination }
            }
        };
        Some(BrowserCommand::Mutate(request))
    }
}

fn open_command(entry: &FileEntry) -> Option<BrowserCommand> {
    match entry.kind {
        FileKind::Directory => Some(BrowserCommand::NavigateTo(entry.reference.path.clone())),
        FileKind::File => Some(BrowserCommand::Open(entry.reference.clone())),
        FileKind::Symlink | FileKind::Other => None,
    }
}

fn validate_move(entry: &FileEntry, destination: &DirectoryRef) -> Result<(), String> {
    if entry.reference.source_id != destination.source_id {
        return Err("Move is limited to the current file source".to_owned());
    }
    if entry.reference.parent() == *destination {
        return Err("The entry is already in that directory".to_owned());
    }
    if entry.kind != FileKind::Directory {
        return Ok(());
    }
    let source = entry.reference.path.as_str();
    let target = destination.path.as_str();
    if target == source
        || target
            .strip_prefix(source)
            .is_some_and(|suffix| suffix.starts_with('/'))
    {
        return Err("A directory cannot be moved into itself or its descendant".to_owned());
    }
    Ok(())
}

fn format_entry_size(entry: &FileEntry) -> String {
    if entry.kind == FileKind::Directory {
        "—".to_owned()
    } else {
        format_file_size(entry.version.size)
    }
}

pub(super) fn format_file_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    match bytes {
        0..=1023 => format!("{bytes} B"),
        KIB..=1_048_575 => format_scaled_size(bytes, KIB, "KiB"),
        MIB..=1_073_741_823 => format_scaled_size(bytes, MIB, "MiB"),
        _ => format_scaled_size(bytes, GIB, "GiB"),
    }
}

fn format_scaled_size(bytes: u64, unit: u64, suffix: &str) -> String {
    let unit = u128::from(unit);
    let tenths = (u128::from(bytes) * 10 + unit / 2) / unit;
    format!("{}.{:01} {suffix}", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_app::{FileVersion, SourcePath};

    fn directory(path: &str) -> FileEntry {
        let source_id = FileSourceId::new("source").unwrap();
        let source_path = SourcePath::new(path).unwrap();
        FileEntry {
            reference: FileRef::new(source_id, source_path),
            name: EntryName::new(path.rsplit('/').next().unwrap()).unwrap(),
            kind: FileKind::Directory,
            version: FileVersion {
                size: 0,
                modified_millis: None,
            },
        }
    }

    fn file(path: &str) -> FileEntry {
        let mut entry = directory(path);
        entry.kind = FileKind::File;
        entry.version.size = 4;
        entry
    }

    #[test]
    fn formats_sizes_with_iec_units() {
        assert_eq!(format_file_size(17), "17 B");
        assert_eq!(format_file_size(1536), "1.5 KiB");
        assert_eq!(format_file_size(2 * 1024 * 1024), "2.0 MiB");
    }

    #[test]
    fn move_rejects_same_parent_and_descendant() {
        let entry = directory("capture");
        let source = entry.reference.source_id.clone();
        assert!(validate_move(&entry, &DirectoryRef::root(source.clone())).is_err());
        assert!(
            validate_move(
                &entry,
                &DirectoryRef::new(source, SourcePath::directory("capture/nested").unwrap())
            )
            .is_err()
        );
    }
    #[test]
    fn inline_editors_commit_cancel_and_retain_invalid_name_errors() {
        let entry = file("frame.raw");
        let mut browser = BrowserState::default();

        browser.begin_rename(&entry);
        if let Some(EntryEditor::Rename { value, .. }) = browser.editor.as_mut() {
            *value = "renamed.raw".to_owned();
        }
        let command = browser
            .resolve_rename_editor(true, false, false)
            .expect("valid rename commits");
        assert!(matches!(
            command,
            BrowserCommand::Mutate(MutationRequest::Rename {
                new_name,
                ..
            }) if new_name.as_str() == "renamed.raw"
        ));

        browser.clear_transient();
        browser.begin_rename(&entry);
        if let Some(EntryEditor::Rename { value, .. }) = browser.editor.as_mut() {
            *value = "../invalid".to_owned();
        }
        assert!(browser.resolve_rename_editor(true, false, false).is_none());
        assert!(matches!(
            browser.editor,
            Some(EntryEditor::Rename { error: Some(_), .. })
        ));
        assert!(browser.resolve_rename_editor(false, true, false).is_none());
        assert!(browser.editor.is_none());

        let parent = DirectoryRef::root(entry.reference.source_id.clone());
        browser.begin_new_folder(parent);
        if let Some(EntryEditor::NewFolder { value, .. }) = browser.editor.as_mut() {
            *value = "capture".to_owned();
        }
        let command = browser
            .resolve_new_folder_editor(true, false, false)
            .expect("valid folder name commits");
        assert!(matches!(
            command,
            BrowserCommand::Mutate(MutationRequest::CreateDirectory {
                name,
                ..
            }) if name.as_str() == "capture"
        ));
    }

    #[test]
    fn delete_and_move_require_explicit_confirmation() {
        let entry = directory("capture");
        let source = entry.reference.source_id.clone();
        let destination = DirectoryRef::new(source, SourcePath::directory("archive").unwrap());
        let mut browser = BrowserState::default();

        browser.confirmation = Some(PendingConfirmation::Delete(entry.clone()));
        assert!(browser.resolve_confirmation(false, false, false).is_none());
        assert!(matches!(
            browser.confirmation,
            Some(PendingConfirmation::Delete(_))
        ));
        assert!(browser.resolve_confirmation(false, true, false).is_none());
        assert!(browser.confirmation.is_none());

        browser.confirmation = Some(PendingConfirmation::Move {
            entry: entry.clone(),
            destination: destination.clone(),
        });
        assert!(browser.resolve_confirmation(true, false, true).is_none());
        assert!(browser.confirmation.is_some());
        let command = browser
            .resolve_confirmation(true, false, false)
            .expect("explicit confirmation emits mutation");
        assert!(matches!(
            command,
            BrowserCommand::Mutate(MutationRequest::Move {
                entry: moved,
                destination: moved_to,
            }) if moved == entry && moved_to == destination
        ));
        assert!(browser.confirmation.is_none());
    }
}
