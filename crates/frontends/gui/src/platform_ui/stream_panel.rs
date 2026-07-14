//! CV610 Stream 控件；所有落盘目标均由用户逐项显式选择。

use std::path::PathBuf;

use camera_toolbox_app::{StreamOpenRequest, StreamRecordingRequest};
use eframe::egui;

#[derive(Debug, Clone)]
pub(crate) struct StreamPanelState {
    pub(crate) quota_mib: u64,
    pub(crate) transport_destination: Option<PathBuf>,
    pub(crate) annexb_destination: Option<PathBuf>,
    pub(crate) timestamp_destination: Option<PathBuf>,
    pub(crate) last_error: Option<String>,
    pub(crate) expanded: bool,
}

impl Default for StreamPanelState {
    fn default() -> Self {
        Self {
            quota_mib: 512,
            transport_destination: None,
            annexb_destination: None,
            timestamp_destination: None,
            last_error: None,
            expanded: false,
        }
    }
}

impl StreamPanelState {
    pub(crate) fn request(&self, channel: u16, media: &str) -> Result<StreamOpenRequest, String> {
        let quota_bytes = self
            .quota_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| "record quota overflows u64".to_owned())?;
        let request = StreamOpenRequest {
            channel,
            media: media.to_owned(),
            cseq: 1,
            recording: StreamRecordingRequest {
                transport_destination: self.transport_destination.clone(),
                annexb_destination: self.annexb_destination.clone(),
                timestamp_sidecar_destination: self.timestamp_destination.clone(),
                quota_bytes,
            },
        };
        request.validate().map_err(|error| error.to_string())?;
        Ok(request)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamPanelAction {
    Start,
    RequestStop,
}

pub(crate) fn render_stream_panel(
    ui: &mut egui::Ui,
    state: &mut StreamPanelState,
    live_running: bool,
) -> Option<StreamPanelAction> {
    let mut action = None;
    ui.heading("CV610 Stream");
    ui.label("Endpoint, channel and media come from the selected CV610 profile.");
    ui.label("Codec: Auto (verified provider currently exposes H.265)");
    ui.separator();
    ui.label("Explicit recording targets");
    destination_row(
        ui,
        "Transport evidence",
        &mut state.transport_destination,
        "raw",
    );
    destination_row(ui, "Annex-B", &mut state.annexb_destination, "h265");
    destination_row(
        ui,
        "RTP timestamp sidecar",
        &mut state.timestamp_destination,
        "jsonl",
    );
    ui.horizontal(|ui| {
        ui.label("Quota MiB");
        ui.add(egui::DragValue::new(&mut state.quota_mib).range(1..=65_536));
    });
    if ui.button("Clear recording targets").clicked() {
        state.transport_destination = None;
        state.annexb_destination = None;
        state.timestamp_destination = None;
    }
    if let Some(error) = state.last_error.as_deref() {
        ui.colored_label(egui::Color32::LIGHT_RED, error);
    }
    ui.separator();
    if live_running {
        if ui.button("Stop Live").clicked() {
            action = Some(StreamPanelAction::RequestStop);
        }
    } else if ui.button("Start Live").clicked() {
        action = Some(StreamPanelAction::Start);
    }
    action
}

fn destination_row(
    ui: &mut egui::Ui,
    label: &str,
    destination: &mut Option<PathBuf>,
    extension: &str,
) {
    ui.label(label);
    ui.horizontal(|ui| {
        let text = destination
            .as_ref()
            .map_or_else(|| "Off".to_owned(), |path| path.display().to_string());
        ui.add(egui::Label::new(text).truncate());
        if ui.small_button("Choose...").clicked()
            && let Some(path) = rfd::FileDialog::new()
                .add_filter(extension, &[extension])
                .save_file()
        {
            *destination = Some(path);
        }
        if destination.is_some() && ui.small_button("×").clicked() {
            *destination = None;
        }
    });
}
