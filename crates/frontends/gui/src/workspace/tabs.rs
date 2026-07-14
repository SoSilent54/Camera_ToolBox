//! 文档 Tab bar 绘制与无副作用用户动作。

use eframe::egui;

use super::{DocumentId, WorkspaceState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabBarAction {
    Activate(DocumentId),
    Close(DocumentId),
}

pub(crate) fn render_tab_bar(
    ui: &mut egui::Ui,
    workspace: &WorkspaceState,
) -> Option<TabBarAction> {
    let active = workspace.active_id();
    let mut action = None;
    egui::ScrollArea::horizontal()
        .id_salt("document_tab_scroll")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if workspace.is_empty() {
                    ui.weak("No documents");
                    return;
                }
                for document in workspace.documents() {
                    let selected = active == Some(document.id);
                    let frame = if selected {
                        egui::Frame::group(ui.style()).fill(ui.visuals().selection.bg_fill)
                    } else {
                        egui::Frame::group(ui.style())
                    };
                    frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let title = format!(
                                "{}  {}",
                                document.title,
                                document.resource_status().label()
                            );
                            if ui.selectable_label(selected, title).clicked() {
                                action = Some(TabBarAction::Activate(document.id));
                            }
                            if ui
                                .small_button("×")
                                .on_hover_text(format!("Close {}", document.title))
                                .clicked()
                            {
                                action = Some(TabBarAction::Close(document.id));
                            }
                        });
                    });
                }
                for document in workspace.asset_documents() {
                    let selected = active == Some(document.id);
                    let frame = if selected {
                        egui::Frame::group(ui.style()).fill(ui.visuals().selection.bg_fill)
                    } else {
                        egui::Frame::group(ui.style())
                    };
                    frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let status = if document.saved { "Saved" } else { "Unsaved" };
                            let title = format!("{}  {}", document.title, status);
                            if ui.selectable_label(selected, title).clicked() {
                                action = Some(TabBarAction::Activate(document.id));
                            }
                            if ui
                                .small_button("×")
                                .on_hover_text(format!("Close {}", document.title))
                                .clicked()
                            {
                                action = Some(TabBarAction::Close(document.id));
                            }
                        });
                    });
                }
                for document in workspace.live_documents() {
                    let selected = active == Some(document.id);
                    let frame = if selected {
                        egui::Frame::group(ui.style()).fill(ui.visuals().selection.bg_fill)
                    } else {
                        egui::Frame::group(ui.style())
                    };
                    frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let title = format!("{}  {}", document.title, document.status_label());
                            if ui.selectable_label(selected, title).clicked() {
                                action = Some(TabBarAction::Activate(document.id));
                            }
                            if ui
                                .small_button("×")
                                .on_hover_text(format!("Close {}", document.title))
                                .clicked()
                            {
                                action = Some(TabBarAction::Close(document.id));
                            }
                        });
                    });
                }
            });
        });
    action
}
