//! Live stream 文档：session 状态、latest-frame texture 与异步关闭生命周期。

use std::{collections::VecDeque, sync::Arc, time::Instant};

use camera_toolbox_app::{
    DecodedVideoFrame, LatestDecodedFrameSlot, PlatformProfileId, RecordingBranch, RecordingState,
    RtspTransport, StreamMediaInfo, StreamMetrics, StreamServiceEvent, StreamSessionId,
    StreamStage, StreamTerminal, host_monotonic_time_ns,
};
use eframe::egui;

use super::DocumentId;

/// Workspace 所需的脱敏实时输入身份；URL 和认证信息永不进入文档状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LiveStreamSource {
    Cv610 {
        profile_id: PlatformProfileId,
        profile_label: String,
        channel: u16,
    },
    Rtsp {
        label: String,
        channel: u16,
        transport: RtspTransport,
    },
}

impl LiveStreamSource {
    pub(crate) fn title(&self) -> String {
        match self {
            Self::Cv610 {
                profile_label,
                channel,
                ..
            } => {
                format!("Stream · {profile_label} · CH{channel}")
            }
            Self::Rtsp { label, channel, .. } => format!("RTSP · {label} · CH{channel}"),
        }
    }

    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Cv610 {
                profile_id,
                channel,
                ..
            } => {
                format!("CV610 · profile {profile_id} · channel {channel}")
            }
            Self::Rtsp {
                channel, transport, ..
            } => {
                format!("RTSP/{transport:?} · channel {channel}")
            }
        }
    }
}

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
    pub(crate) source: LiveStreamSource,
    pub(crate) stage: StreamStage,
    pub(crate) transport_recording: Option<RecordingState>,
    pub(crate) annexb_recording: Option<RecordingState>,
    pub(crate) presented_frames: u64,
    pub(crate) last_snapshot: Option<String>,
    texture: Option<egui::TextureHandle>,
    displayed_frame: Option<Arc<DecodedVideoFrame>>,
    /// 最近一秒实际安装纹理的时刻；只计最新帧，不为预览建立队列。
    presentation_samples_ns: VecDeque<u64>,
}

impl LiveDocument {
    pub(crate) fn new(
        id: DocumentId,
        session_id: StreamSessionId,
        latest_frame: Arc<LatestDecodedFrameSlot>,
        source: LiveStreamSource,
    ) -> Self {
        Self {
            id,
            title: source.title(),
            session_id,
            latest_frame,
            source,
            lifecycle: LiveDocumentLifecycle::Open,
            media: None,
            metrics: StreamMetrics::default(),
            stage: StreamStage::Connecting,
            decoder_unavailable: None,
            transport_recording: None,
            annexb_recording: None,
            presented_frames: 0,
            last_snapshot: None,
            texture: None,
            displayed_frame: None,
            presentation_samples_ns: VecDeque::new(),
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
            StreamServiceEvent::Metrics(mut metrics) => {
                // service 报告 decode/transport；texture install 才能知道本机展示指标。
                metrics.presented_frames = self.presented_frames;
                metrics.presented_fps_millihz = self.metrics.presented_fps_millihz;
                metrics.host_presentation_delay_ns = self.metrics.host_presentation_delay_ns;
                self.metrics = metrics;
            }
            StreamServiceEvent::Terminal(terminal) => {
                self.latest_frame.clear();
                self.release_texture();
                self.lifecycle = if matches!(terminal, StreamTerminal::Forced { .. }) {
                    LiveDocumentLifecycle::ForcedCleanup { terminal }
                } else {
                    LiveDocumentLifecycle::Terminal { terminal }
                };
            }
            StreamServiceEvent::Stage(stage) => self.stage = stage,
        }
    }
    pub(crate) fn install_latest_texture(&mut self, context: &egui::Context) {
        if !matches!(self.lifecycle, LiveDocumentLifecycle::Open) {
            return;
        }
        let Some(frame) = self.latest_frame.latest() else {
            return;
        };
        if self.displayed_frame.as_ref().is_some_and(|displayed| {
            displayed.identity.frame_sequence == frame.identity.frame_sequence
        }) {
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
            format!("live-{}-{}", self.id, frame.identity.frame_sequence),
            image,
            egui::TextureOptions::LINEAR,
        ));
        let installed_host_time_ns = host_monotonic_time_ns();
        let host_presentation_delay_ns =
            installed_host_time_ns.saturating_sub(frame.identity.host_monotonic_time_ns);
        self.displayed_frame = Some(frame);
        self.presented_frames = self.presented_frames.saturating_add(1);
        self.metrics.presented_frames = self.presented_frames;
        self.metrics.host_presentation_delay_ns = host_presentation_delay_ns;
        self.presentation_samples_ns
            .push_back(installed_host_time_ns);
        const PRESENTATION_WINDOW_NS: u64 = 1_000_000_000;
        while self.presentation_samples_ns.front().is_some_and(|sample| {
            installed_host_time_ns.saturating_sub(*sample) > PRESENTATION_WINDOW_NS
        }) {
            self.presentation_samples_ns.pop_front();
        }
        self.metrics.presented_fps_millihz = match (
            self.presentation_samples_ns.front(),
            self.presentation_samples_ns.back(),
        ) {
            (Some(first), Some(last)) if last > first => u64::try_from(
                (self.presentation_samples_ns.len().saturating_sub(1) as u128)
                    .saturating_mul(1_000_000_000_000)
                    / u128::from(last.saturating_sub(*first)),
            )
            .unwrap_or(u64::MAX),
            _ => 0,
        };
    }

    pub(crate) fn texture(&self) -> Option<&egui::TextureHandle> {
        self.texture.as_ref()
    }

    /// 当前纹理、provenance 与显式 Snapshot 共享同一个不可变帧对象。
    pub(crate) fn displayed_frame(&self) -> Option<&Arc<DecodedVideoFrame>> {
        self.displayed_frame.as_ref()
    }

    /// inactive live Tab 不停止会话，只释放 GPU texture；latest slot 仍固定为单槽。
    pub(crate) fn release_texture(&mut self) {
        self.texture = None;
        self.displayed_frame = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_app::StreamFrameIdentity;

    fn frame(sequence: u64, value: u8) -> DecodedVideoFrame {
        DecodedVideoFrame {
            width: 1,
            height: 1,
            rgba: vec![value, value, value, u8::MAX].into(),
            identity: StreamFrameIdentity::unavailable(
                StreamSessionId::new("live-document-test").unwrap(),
                0,
                sequence,
                "test frame has no source PTS",
            ),
        }
    }

    fn source() -> LiveStreamSource {
        LiveStreamSource::Rtsp {
            label: "Test".to_owned(),
            channel: 0,
            transport: RtspTransport::Tcp,
        }
    }

    #[test]
    fn displayed_frame_remains_the_texture_frame_until_next_install() {
        let latest = Arc::new(LatestDecodedFrameSlot::default());
        latest.publish(frame(1, 17));
        let mut document = LiveDocument::new(
            DocumentId::from_raw(1),
            StreamSessionId::new("live-document-test").unwrap(),
            Arc::clone(&latest),
            source(),
        );
        document.install_latest_texture(&egui::Context::default());

        latest.publish(frame(2, 34));
        let displayed = document
            .displayed_frame()
            .expect("frame A must stay installed");
        assert_eq!(displayed.identity.frame_sequence, 1);
        assert_eq!(displayed.rgba[0], 17);
    }

    #[test]
    fn terminal_event_clears_latest_and_displayed_frame() {
        let latest = Arc::new(LatestDecodedFrameSlot::default());
        latest.publish(frame(1, 17));
        let mut document = LiveDocument::new(
            DocumentId::from_raw(1),
            StreamSessionId::new("live-document-test").unwrap(),
            Arc::clone(&latest),
            source(),
        );
        document.install_latest_texture(&egui::Context::default());
        document.apply_event(StreamServiceEvent::Terminal(StreamTerminal::Cancelled));
        assert!(latest.latest().is_none());
        assert!(document.displayed_frame().is_none());
        assert!(document.texture().is_none());
    }
    #[test]
    fn presentation_delay_uses_shared_monotonic_clock_and_is_non_negative() {
        let latest = Arc::new(LatestDecodedFrameSlot::default());
        let published_host_time_ns = host_monotonic_time_ns();
        latest.publish(DecodedVideoFrame {
            width: 1,
            height: 1,
            rgba: vec![17, 17, 17, u8::MAX].into(),
            identity: StreamFrameIdentity::known_at(
                StreamSessionId::new("live-document-test").unwrap(),
                0,
                1,
                camera_toolbox_app::SourcePts::Unavailable {
                    reason: "test frame has no source PTS".to_owned(),
                },
                published_host_time_ns,
            ),
        });
        let mut document = LiveDocument::new(
            DocumentId::from_raw(1),
            StreamSessionId::new("live-document-test").unwrap(),
            latest,
            source(),
        );
        document.install_latest_texture(&egui::Context::default());
        let latest_now = host_monotonic_time_ns();
        assert!(
            document.metrics.host_presentation_delay_ns
                <= latest_now.saturating_sub(published_host_time_ns)
        );
        assert_eq!(document.metrics.presented_fps_millihz, 0);
    }
}
