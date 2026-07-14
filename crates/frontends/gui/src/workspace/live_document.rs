//! Live stream 文档：session 状态、latest-frame texture 与异步关闭生命周期。

use std::{sync::Arc, time::Instant};

use camera_toolbox_app::{
    LatestDecodedFrameSlot, RecordingBranch, RecordingState, StreamMediaInfo, StreamMetrics,
    StreamServiceEvent, StreamSessionId, StreamTerminal,
};
use eframe::egui;

use super::DocumentId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LiveDocumentLifecycle {
    Open,
    CloseRequested,
    Closing { stop_deadline: Instant },
    ForcedCleanup { terminal: StreamTerminal },
    Terminal { terminal: StreamTerminal },
}

pub(crate) struct LiveDocument {
    pub(crate) id: DocumentId,
    pub(crate) title: String,
    pub(crate) session_id: StreamSessionId,
    pub(crate) latest_frame: Arc<LatestDecodedFrameSlot>,
    pub(crate) lifecycle: LiveDocumentLifecycle,
    pub(crate) media: Option<StreamMediaInfo>,
    pub(crate) metrics: StreamMetrics,
    pub(crate) decoder_unavailable: Option<String>,
    pub(crate) transport_recording: Option<RecordingState>,
    pub(crate) annexb_recording: Option<RecordingState>,
    pub(crate) presented_frames: u64,
    pub(crate) last_snapshot: Option<String>,
    texture: Option<egui::TextureHandle>,
    installed_revision: u64,
}

impl LiveDocument {
    pub(crate) fn new(
        id: DocumentId,
        session_id: StreamSessionId,
        latest_frame: Arc<LatestDecodedFrameSlot>,
    ) -> Self {
        Self {
            id,
            title: format!("CV610 Live {}", session_id.as_str()),
            session_id,
            latest_frame,
            lifecycle: LiveDocumentLifecycle::Open,
            media: None,
            metrics: StreamMetrics::default(),
            decoder_unavailable: None,
            transport_recording: None,
            annexb_recording: None,
            presented_frames: 0,
            last_snapshot: None,
            texture: None,
            installed_revision: 0,
        }
    }

    pub(crate) fn apply_event(&mut self, event: StreamServiceEvent) {
        match event {
            StreamServiceEvent::Negotiated(media) => self.media = Some(media),
            StreamServiceEvent::DecoderUnavailable { reason } => {
                self.decoder_unavailable = Some(reason);
            }
            StreamServiceEvent::Recording { branch, state } => match branch {
                RecordingBranch::TransportEvidence => self.transport_recording = Some(state),
                RecordingBranch::AnnexB => self.annexb_recording = Some(state),
            },
            StreamServiceEvent::Metrics(metrics) => self.metrics = metrics,
            StreamServiceEvent::Terminal(terminal) => {
                self.lifecycle = if matches!(terminal, StreamTerminal::Forced { .. }) {
                    LiveDocumentLifecycle::ForcedCleanup { terminal }
                } else {
                    LiveDocumentLifecycle::Terminal { terminal }
                };
            }
            StreamServiceEvent::Stage(_) => {}
        }
    }

    pub(crate) fn install_latest_texture(&mut self, context: &egui::Context) {
        let Some(frame) = self.latest_frame.latest() else {
            return;
        };
        if frame.revision == self.installed_revision {
            return;
        }
        let Ok(width) = usize::try_from(frame.width) else {
            return;
        };
        let Ok(height) = usize::try_from(frame.height) else {
            return;
        };
        let expected = width
            .checked_mul(height)
            .and_then(|value| value.checked_mul(4));
        if expected != Some(frame.rgba.len()) {
            return;
        }
        let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &frame.rgba);
        self.texture = Some(context.load_texture(
            format!("live-{}-{}", self.id, frame.revision),
            image,
            egui::TextureOptions::LINEAR,
        ));
        self.installed_revision = frame.revision;
        self.presented_frames = self.presented_frames.saturating_add(1);
        self.metrics.presented_frames = self.presented_frames;
    }

    pub(crate) fn texture(&self) -> Option<&egui::TextureHandle> {
        self.texture.as_ref()
    }

    /// inactive live Tab 不停止会话，只释放 GPU texture；latest slot 仍固定为单槽。
    pub(crate) fn release_texture(&mut self) {
        self.texture = None;
        self.installed_revision = 0;
    }

    pub(crate) fn status_label(&self) -> &'static str {
        match self.lifecycle {
            LiveDocumentLifecycle::Open => "Live",
            LiveDocumentLifecycle::CloseRequested => "Close?",
            LiveDocumentLifecycle::Closing { .. } => "Closing",
            LiveDocumentLifecycle::ForcedCleanup { .. } => "Forced",
            LiveDocumentLifecycle::Terminal { .. } => "Stopped",
        }
    }
}
