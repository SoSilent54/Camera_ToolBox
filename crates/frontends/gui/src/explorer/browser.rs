//! Explorer 文件表格、上下文菜单、编辑器、拖放和危险操作确认。

use std::collections::BTreeSet;

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
    NavigatePath(String),
    NavigateTo(SourcePath),
    Refresh,
    Open(FileRef),
    Mutate(MutationRequest),
}

#[derive(Debug, Default)]
pub(super) struct BrowserSelection {
    paths: BTreeSet<SourcePath>,
    anchor: Option<SourcePath>,
}

impl BrowserSelection {
    pub(super) fn clear(&mut self) {
        self.paths.clear();
        self.anchor = None;
    }

    pub(super) fn contains(&self, path: &SourcePath) -> bool {
        self.paths.contains(path)
    }

    pub(super) fn len(&self) -> usize {
        self.paths.len()
    }

    fn select_single(&mut self, path: &SourcePath) {
        self.paths.clear();
        self.paths.insert(path.clone());
        self.anchor = Some(path.clone());
    }

    pub(super) fn select(
        &mut self,
        entries: &[FileEntry],
        path: &SourcePath,
        modifiers: egui::Modifiers,
    ) {
        let additive = modifiers.ctrl || modifiers.command;
        if modifiers.shift
            && let Some(anchor) = self.anchor.as_ref()
            && let Some(anchor_index) = entries
                .iter()
                .position(|entry| &entry.reference.path == anchor)
            && let Some(clicked_index) = entries
                .iter()
                .position(|entry| &entry.reference.path == path)
        {
            if !additive {
                self.paths.clear();
            }
            let start = anchor_index.min(clicked_index);
            let end = anchor_index.max(clicked_index);
            self.paths.extend(
                entries[start..=end]
                    .iter()
                    .map(|entry| entry.reference.path.clone()),
            );
            return;
        }

        if additive {
            if !self.paths.insert(path.clone()) {
                self.paths.remove(path);
            }
            self.anchor = Some(path.clone());
        } else {
            self.select_single(path);
        }
    }

    fn retain_visible(&mut self, entries: &[FileEntry]) {
        self.paths
            .retain(|path| entries.iter().any(|entry| entry.reference.path == *path));
        if self
            .anchor
            .as_ref()
            .is_some_and(|anchor| !self.paths.contains(anchor))
        {
            self.anchor = self.paths.iter().next().cloned();
        }
    }
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
    navigation_identity: Option<(FileSourceId, SourcePath)>,
    navigation_value: String,
    navigation_dirty: bool,
    navigation_error: Option<String>,
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

    pub(super) fn set_navigation_error(&mut self, message: impl Into<String>) {
        self.navigation_error = Some(message.into());
    }

    pub(super) fn confirm_navigation(&mut self) {
        self.navigation_dirty = false;
        self.navigation_error = None;
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
        navigation_path: &str,
        entries: &[FileEntry],
        selection: &mut BrowserSelection,
        capabilities: FileSystemCapabilities,
        busy: bool,
    ) -> Option<BrowserCommand> {
        self.sync_navigation(source_id, current_directory, navigation_path);
        selection.retain_visible(entries);
        let current = DirectoryRef::new(source_id.clone(), current_directory.clone());
        let mut command = self.render_navigation_row(ui, current_directory, busy);

        if let Some(message) = &self.message {
            ui.colored_label(egui::Color32::RED, message);
        }

        ui.separator();
        ui.horizontal(|ui| {
            let (name_width, size_width) =
                file_table_column_widths(ui.available_width(), ui.spacing().item_spacing.x);
            let _ = render_clipped_cell(
                ui,
                egui::vec2(name_width, 24.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| ui.strong("Name"),
            );
            let _ = render_clipped_cell(
                ui,
                egui::vec2(size_width, 24.0),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| ui.strong("Size"),
            );
        });
        ui.separator();

        let table = ui.vertical(|ui| {
            ui.set_min_height(ui.available_height());
            egui::ScrollArea::vertical().show(ui, |ui| {
                let parent_command = ui.horizontal(|ui| {
                    let (name_width, size_width) =
                        file_table_column_widths(ui.available_width(), ui.spacing().item_spacing.x);
                    let parent_command = render_clipped_cell(
                        ui,
                        egui::vec2(name_width, 22.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| Self::render_parent_name_cell(ui, current_directory, busy),
                    )
                    .1;
                    let _ = render_clipped_cell(
                        ui,
                        egui::vec2(size_width, 22.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| ui.monospace("—"),
                    );
                    parent_command
                });
                if command.is_none() {
                    command = parent_command.inner;
                }
                if matches!(self.editor, Some(EntryEditor::NewFolder { .. })) {
                    let editor_command = ui.horizontal(|ui| {
                        let (name_width, size_width) = file_table_column_widths(
                            ui.available_width(),
                            ui.spacing().item_spacing.x,
                        );
                        let editor_command = render_clipped_cell(
                            ui,
                            egui::vec2(name_width, 22.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| self.render_new_folder_editor(ui, busy),
                        )
                        .1;
                        let _ = render_clipped_cell(
                            ui,
                            egui::vec2(size_width, 22.0),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| ui.monospace("—"),
                        );
                        editor_command
                    });
                    if command.is_none() {
                        command = editor_command.inner;
                    }
                }
                for entry in entries {
                    let entry_command = ui.horizontal(|ui| {
                        let (name_width, size_width) = file_table_column_widths(
                            ui.available_width(),
                            ui.spacing().item_spacing.x,
                        );
                        let entry_command = render_clipped_cell(
                            ui,
                            egui::vec2(name_width, 22.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                if self.editor_matches(entry) {
                                    self.render_rename_editor(ui, busy)
                                } else {
                                    self.render_entry_name_cell(
                                        ui,
                                        entry,
                                        entries,
                                        selection,
                                        &current,
                                        capabilities,
                                        busy,
                                    )
                                }
                            },
                        )
                        .1;
                        let _ = render_clipped_cell(
                            ui,
                            egui::vec2(size_width, 22.0),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| ui.monospace(format_entry_size(entry)),
                        );
                        entry_command
                    });
                    if command.is_none() {
                        command = entry_command.inner;
                    }
                }
            });
        });
        let background_response = table.response.interact(egui::Sense::click());
        background_response.context_menu(|ui| {
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

    fn sync_navigation(
        &mut self,
        source_id: &FileSourceId,
        current_directory: &SourcePath,
        navigation_path: &str,
    ) {
        let identity_matches =
            self.navigation_identity
                .as_ref()
                .is_some_and(|(source, directory)| {
                    source == source_id && directory == current_directory
                });
        if !identity_matches || (!self.navigation_dirty && self.navigation_value != navigation_path)
        {
            self.navigation_identity = Some((source_id.clone(), current_directory.clone()));
            navigation_path.clone_into(&mut self.navigation_value);
            self.navigation_dirty = false;
            self.navigation_error = None;
        }
    }

    fn render_navigation_row(
        &mut self,
        ui: &mut egui::Ui,
        current_directory: &SourcePath,
        busy: bool,
    ) -> Option<BrowserCommand> {
        let mut command = None;
        let mut commit_path = false;
        ui.horizontal(|ui| {
            ui.label("Path");
            let response = ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(&mut self.navigation_value)
                    .id_source("explorer_current_path")
                    .desired_width((ui.available_width() - 96.0).max(72.0)),
            );
            if response.changed() {
                self.navigation_dirty = true;
                self.navigation_error = None;
            }
            let enter = ui.input(|input| input.key_pressed(egui::Key::Enter));
            commit_path =
                self.navigation_dirty && (response.lost_focus() || (response.has_focus() && enter));
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
        });
        if let Some(error) = &self.navigation_error {
            ui.colored_label(egui::Color32::RED, error);
        }
        resolve_navigation_command(command, commit_path, &self.navigation_value)
    }

    fn render_parent_name_cell(
        ui: &mut egui::Ui,
        current_directory: &SourcePath,
        busy: bool,
    ) -> Option<BrowserCommand> {
        let enabled = !current_directory.is_root() && !busy;
        let response = ui.add_enabled(
            enabled,
            egui::Button::selectable(false, ())
                .left_text("[D] ...")
                .min_size(egui::vec2(ui.available_width(), 22.0))
                .sense(egui::Sense::click()),
        );
        (response.double_clicked_by(egui::PointerButton::Primary) && enabled)
            .then_some(BrowserCommand::NavigateUp)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_entry_name_cell(
        &mut self,
        ui: &mut egui::Ui,
        entry: &FileEntry,
        entries: &[FileEntry],
        selection: &mut BrowserSelection,
        current_directory: &DirectoryRef,
        capabilities: FileSystemCapabilities,
        busy: bool,
    ) -> Option<BrowserCommand> {
        let icon = match entry.kind {
            FileKind::Directory => "[D]",
            FileKind::File => "[F]",
            FileKind::Symlink => "[L]",
            FileKind::Other => "[?]",
        };
        let selected_row = selection.contains(&entry.reference.path);
        let response = ui.add_sized(
            [ui.available_width(), 22.0],
            egui::Button::selectable(selected_row, ())
                .left_text(format!("{icon} {}", entry.name))
                .sense(egui::Sense::click_and_drag()),
        );
        let mut command = None;
        if response.clicked_by(egui::PointerButton::Primary) {
            let modifiers = ui.input(|input| input.modifiers);
            selection.select(entries, &entry.reference.path, modifiers);
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

        if response.secondary_clicked() && !selection.contains(&entry.reference.path) {
            selection.select_single(&entry.reference.path);
        }
        let open_label = if selection.contains(&entry.reference.path) && selection.len() > 1 {
            format!("Open selected ({})", selection.len())
        } else {
            "Open".to_owned()
        };
        let mut menu_action = None;
        response.context_menu(|ui| {
            if ui
                .add_enabled(
                    matches!(entry.kind, FileKind::File | FileKind::Directory) && !busy,
                    egui::Button::new(open_label),
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
            value: "New Folder".to_owned(),
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
                    .desired_width((ui.available_width() - 104.0).max(48.0)),
            );
            if *request_focus {
                response.request_focus();
                *request_focus = false;
            }
            let active = response.has_focus() || response.lost_focus();
            commit |= active && ui.input(|input| input.key_pressed(egui::Key::Enter));
            cancel |= active && ui.input(|input| input.key_pressed(egui::Key::Escape));
            commit |= ui.add_enabled(!busy, egui::Button::new("OK")).clicked();
            cancel |= ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked();
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
                    .hint_text("New folder name")
                    .desired_width((ui.available_width() - 128.0).max(48.0)),
            );
            if *request_focus {
                response.request_focus();
                *request_focus = false;
            }
            let active = response.has_focus() || response.lost_focus();
            commit |= active && ui.input(|input| input.key_pressed(egui::Key::Enter));
            cancel |= active && ui.input(|input| input.key_pressed(egui::Key::Escape));
            commit |= ui.add_enabled(!busy, egui::Button::new("OK")).clicked();
            cancel |= ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked();
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

fn resolve_navigation_command(
    command: Option<BrowserCommand>,
    commit_path: bool,
    navigation_value: &str,
) -> Option<BrowserCommand> {
    if commit_path {
        Some(BrowserCommand::NavigatePath(navigation_value.to_owned()))
    } else {
        command
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

/// 所有宽度均由当前行的可用空间切分；内容长度不会改变列几何关系。
fn file_table_column_widths(available_width: f32, spacing: f32) -> (f32, f32) {
    const PREFERRED_SIZE_WIDTH: f32 = 80.0;
    const RESERVED_NAME_WIDTH: f32 = 48.0;

    let usable_width = (available_width - spacing).max(0.0);
    let size_width = (usable_width - RESERVED_NAME_WIDTH).clamp(0.0, PREFERRED_SIZE_WIDTH);
    let name_width = usable_width - size_width;
    (name_width, size_width)
}

/// 先在父 UI 预留精确列宽，再在固定矩形内创建子 UI，防止子控件的最小尺寸扩张父布局。
fn render_clipped_cell<T>(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    layout: egui::Layout,
    add_contents: impl FnOnce(&mut egui::Ui) -> T,
) -> (egui::Rect, T) {
    let parent_clip = ui.clip_rect();
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let mut cell = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    cell.set_clip_rect(parent_clip.intersect(rect));
    (rect, add_contents(&mut cell))
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

    fn render_browser_frame(
        context: &egui::Context,
        browser: &mut BrowserState,
        entries: &[FileEntry],
        time: f64,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Option<BrowserCommand>) {
        let source_id = entries.first().map_or_else(
            || FileSourceId::new("source").unwrap(),
            |entry| entry.reference.source_id.clone(),
        );
        let current_directory = SourcePath::root();
        let mut selection = BrowserSelection::default();
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(440.0, 360.0),
            )),
            time: Some(time),
            ..Default::default()
        };
        input.events = events;
        let mut command = None;
        let output = context.run_ui(input, |ui| {
            command = browser.render(
                context,
                ui,
                &source_id,
                &current_directory,
                "/workspace",
                entries,
                &mut selection,
                FileSystemCapabilities::READ_WRITE,
                false,
            );
        });
        (output, command)
    }

    fn accessibility_text(output: &egui::FullOutput) -> String {
        output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("accessibility tree is enabled")
            .nodes
            .iter()
            .filter_map(|(_, node)| node.label().or_else(|| node.value()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn accessibility_center(
        output: &egui::FullOutput,
        role: egui::accesskit::Role,
        label: Option<&str>,
    ) -> egui::Pos2 {
        let bounds = output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("accessibility tree is enabled")
            .nodes
            .iter()
            .find_map(|(_, node)| {
                (node.role() == role && label.is_none_or(|label| node.label() == Some(label)))
                    .then(|| node.bounds())
                    .flatten()
            })
            .unwrap_or_else(|| panic!("accessibility node {role:?} {label:?} is visible"));
        #[allow(clippy::cast_possible_truncation)]
        egui::pos2(
            ((bounds.x0 + bounds.x1) * 0.5) as f32,
            ((bounds.y0 + bounds.y1) * 0.5) as f32,
        )
    }

    fn pointer_button(
        position: egui::Pos2,
        button: egui::PointerButton,
        pressed: bool,
    ) -> egui::Event {
        egui::Event::PointerButton {
            pos: position,
            button,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn key_event(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }
    }

    #[test]
    fn selection_supports_shift_ranges_and_ctrl_toggles() {
        let entries = [file("a.png"), file("b.png"), file("c.png"), file("d.png")];
        let mut selection = BrowserSelection::default();

        selection.select(&entries, &entries[1].reference.path, egui::Modifiers::NONE);
        selection.select(
            &entries,
            &entries[3].reference.path,
            egui::Modifiers {
                shift: true,
                ..egui::Modifiers::NONE
            },
        );
        assert!(!selection.contains(&entries[0].reference.path));
        for entry in &entries[1..=3] {
            assert!(selection.contains(&entry.reference.path));
        }

        selection.select(
            &entries,
            &entries[0].reference.path,
            egui::Modifiers {
                ctrl: true,
                command: true,
                ..egui::Modifiers::NONE
            },
        );
        assert_eq!(selection.len(), 4);
        selection.select(
            &entries,
            &entries[2].reference.path,
            egui::Modifiers {
                ctrl: true,
                command: true,
                ..egui::Modifiers::NONE
            },
        );
        assert!(!selection.contains(&entries[2].reference.path));
        assert_eq!(selection.len(), 3);
    }

    #[test]
    fn narrow_columns_keep_long_name_and_size_in_separate_cells() {
        let mut entry = file(&format!("{}.raw", "long_file_name_".repeat(24)));
        entry.version.size = u64::MAX;
        let long_size = format_entry_size(&entry);
        assert!(entry.name.as_str().len() > 80);
        assert!(long_size.len() > 8);

        for available_width in [24.0, 56.0, 96.0, 160.0, 480.0] {
            let spacing = 8.0;
            let (name_width, size_width) = file_table_column_widths(available_width, spacing);
            assert!(name_width >= 0.0);
            assert!(size_width >= 0.0);
            assert!(name_width + size_width + spacing <= available_width + f32::EPSILON);
        }

        let context = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(96.0, 64.0),
            )),
            ..Default::default()
        };
        let mut cells = None;
        context.run_ui(input, |ui| {
            ui.horizontal(|ui| {
                let (name_width, size_width) =
                    file_table_column_widths(ui.available_width(), ui.spacing().item_spacing.x);
                let (name_rect, _) = render_clipped_cell(
                    ui,
                    egui::vec2(name_width, 22.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| ui.add(egui::Button::new(entry.name.as_str())),
                );
                let (size_rect, _) = render_clipped_cell(
                    ui,
                    egui::vec2(size_width, 22.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| ui.monospace(&long_size),
                );
                cells = Some((name_rect, size_rect));
            });
        });
        let (name_rect, size_rect) = cells.expect("both narrow cells are rendered");
        assert!(
            name_rect.right() <= size_rect.left(),
            "long content must not move the size cell: {name_rect:?}, {size_rect:?}"
        );
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
    fn dirty_path_commit_precedes_up_and_refresh_buttons() {
        for button_command in [BrowserCommand::NavigateUp, BrowserCommand::Refresh] {
            let command = resolve_navigation_command(Some(button_command), true, "/next");
            assert!(matches!(
                command,
                Some(BrowserCommand::NavigatePath(path)) if path == "/next"
            ));
        }

        assert!(matches!(
            resolve_navigation_command(Some(BrowserCommand::Refresh), false, "/next"),
            Some(BrowserCommand::Refresh)
        ));
    }

    #[test]
    fn clicking_refresh_after_dirty_path_commits_path_on_focus_loss() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut browser = BrowserState::default();
        let (output, _) = render_browser_frame(&context, &mut browser, &[], 0.0, Vec::new());
        let path_position = accessibility_center(&output, egui::accesskit::Role::TextInput, None);
        render_browser_frame(
            &context,
            &mut browser,
            &[],
            0.1,
            vec![
                egui::Event::PointerMoved(path_position),
                pointer_button(path_position, egui::PointerButton::Primary, true),
            ],
        );
        let (output, _) = render_browser_frame(
            &context,
            &mut browser,
            &[],
            0.11,
            vec![pointer_button(
                path_position,
                egui::PointerButton::Primary,
                false,
            )],
        );
        browser.navigation_value = "/workspace/archive".to_owned();
        browser.navigation_dirty = true;

        let refresh_position =
            accessibility_center(&output, egui::accesskit::Role::Button, Some("Refresh"));
        let (_, press_command) = render_browser_frame(
            &context,
            &mut browser,
            &[],
            0.2,
            vec![
                egui::Event::PointerMoved(refresh_position),
                pointer_button(refresh_position, egui::PointerButton::Primary, true),
            ],
        );
        assert!(press_command.is_none());
        let (_, release_command) = render_browser_frame(
            &context,
            &mut browser,
            &[],
            0.21,
            vec![pointer_button(
                refresh_position,
                egui::PointerButton::Primary,
                false,
            )],
        );
        assert!(matches!(
            release_command,
            Some(BrowserCommand::NavigatePath(path)) if path == "/workspace/archive"
        ));
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
    fn inline_editors_accept_enter_and_expose_buttons() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let entry = file("frame.raw");
        let mut browser = BrowserState::default();

        browser.begin_rename(&entry);
        if let Some(EntryEditor::Rename { value, .. }) = browser.editor.as_mut() {
            *value = "renamed.raw".to_owned();
        }
        let (output, _) = render_browser_frame(
            &context,
            &mut browser,
            std::slice::from_ref(&entry),
            0.0,
            Vec::new(),
        );
        let text = accessibility_text(&output);
        assert!(text.contains("OK"));
        assert!(text.contains("Cancel"));
        let (_, command) = render_browser_frame(
            &context,
            &mut browser,
            std::slice::from_ref(&entry),
            0.1,
            vec![key_event(egui::Key::Enter)],
        );
        assert!(matches!(
            command,
            Some(BrowserCommand::Mutate(MutationRequest::Rename {
                new_name,
                ..
            })) if new_name.as_str() == "renamed.raw"
        ));

        browser.clear_transient();
        browser.begin_new_folder(DirectoryRef::root(entry.reference.source_id.clone()));
        if let Some(EntryEditor::NewFolder { value, .. }) = browser.editor.as_mut() {
            *value = "capture".to_owned();
        }
        let (output, _) = render_browser_frame(
            &context,
            &mut browser,
            std::slice::from_ref(&entry),
            0.2,
            Vec::new(),
        );
        let text = accessibility_text(&output);
        assert!(text.contains("[D] ..."));
        assert!(text.contains("capture"));
        assert!(text.contains("OK"));
        let (_, command) = render_browser_frame(
            &context,
            &mut browser,
            std::slice::from_ref(&entry),
            0.3,
            vec![key_event(egui::Key::Enter)],
        );
        assert!(matches!(
            command,
            Some(BrowserCommand::Mutate(MutationRequest::CreateDirectory {
                name,
                ..
            })) if name.as_str() == "capture"
        ));
    }

    #[test]
    fn browser_toolbar_omits_new_folder_and_keeps_parent_row() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut browser = BrowserState::default();
        let (output, _) = render_browser_frame(&context, &mut browser, &[], 0.0, Vec::new());
        let text = accessibility_text(&output);
        assert!(text.contains("[D] ..."));
        assert!(!text.contains("New Folder"));
    }

    #[test]
    fn empty_browser_context_menu_still_offers_new_folder() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut browser = BrowserState::default();
        let (output, _) = render_browser_frame(&context, &mut browser, &[], 0.0, Vec::new());
        let parent_bounds = output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("accessibility tree is enabled")
            .nodes
            .iter()
            .find_map(|(_, node)| {
                (node.label() == Some("[D] ..."))
                    .then(|| node.bounds())
                    .flatten()
            })
            .expect("parent row is visible");
        #[allow(clippy::cast_possible_truncation)]
        let position = egui::pos2(
            ((parent_bounds.x0 + parent_bounds.x1) * 0.5) as f32,
            (parent_bounds.y1 + 40.0) as f32,
        );
        render_browser_frame(
            &context,
            &mut browser,
            &[],
            0.1,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Secondary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        );
        let (output, _) = render_browser_frame(
            &context,
            &mut browser,
            &[],
            0.11,
            vec![egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Secondary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
        );
        assert!(accessibility_text(&output).contains("New Folder"));
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
