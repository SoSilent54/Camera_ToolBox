//! 直接 RTSP Viewer service；FFmpeg 在主机解码，输入不依赖 SSH 或文件系统。

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use camera_toolbox_app::{
    LatestDecodedFrameSlot, RtspStreamConfig, RtspTransport, StreamMetrics, StreamOpenRequest,
    StreamOperationControl, StreamService, StreamServiceError, StreamServiceEvent, StreamSession,
    StreamSessionId, StreamStage, StreamTerminal,
};

use crate::media::{FfmpegRtspDecoder, FfmpegRtspDecoderError, FfmpegRtspTransport};

pub struct FfmpegRtspStreamService {
    service_id: String,
    config: RtspStreamConfig,
}

impl FfmpegRtspStreamService {
    #[must_use]
    pub fn new(service_id: String, config: RtspStreamConfig) -> Self {
        Self { service_id, config }
    }
}

impl StreamService for FfmpegRtspStreamService {
    fn service_id(&self) -> &str {
        &self.service_id
    }

    fn open(
        &self,
        session_id: StreamSessionId,
        request: StreamOpenRequest,
        control: StreamOperationControl,
    ) -> Result<StreamSession, StreamServiceError> {
        request.validate()?;
        if request.channel != self.config.channel || request.media != "rtsp" {
            return Err(StreamServiceError::InvalidRequest(
                "RTSP service request does not match the resolved profile source".to_owned(),
            ));
        }
        if request.recording.transport_destination.is_some()
            || request.recording.annexb_destination.is_some()
            || request.recording.timestamp_sidecar_destination.is_some()
        {
            return Err(StreamServiceError::InvalidRequest(
                "RTSP Viewer does not record transport or Annex-B artifacts".to_owned(),
            ));
        }
        let latest_frame = Arc::new(LatestDecodedFrameSlot::default());
        let worker_latest = Arc::clone(&latest_frame);
        let worker_config = self.config.clone();
        let worker_prefer_hardware_acceleration = request.prefer_hardware_acceleration;
        let worker_control = control.clone();
        let worker_session = session_id.clone();
        let monitor_latest = Arc::clone(&worker_latest);
        std::thread::Builder::new()
            .name(format!("rtsp-{}", session_id.as_str()))
            .spawn(move || {
                worker_control.report(camera_toolbox_app::StreamServiceEvent::Stage(
                    StreamStage::Connecting,
                ));
                let decoder = FfmpegRtspDecoder::start(
                    &worker_config.url,
                    match worker_config.transport {
                        RtspTransport::Tcp => FfmpegRtspTransport::Tcp,
                        RtspTransport::Udp => FfmpegRtspTransport::Udp,
                    },
                    worker_config.width,
                    worker_config.height,
                    worker_session,
                    worker_config.channel,
                    worker_latest,
                    worker_control
                        .timeouts
                        .connect
                        .max(worker_control.timeouts.idle),
                    worker_prefer_hardware_acceleration,
                    &worker_control.cancellation,
                );
                let terminal = match decoder {
                    Ok(decoder) => monitor_rtsp_decoder(decoder, monitor_latest, &worker_control),
                    Err(error) => StreamTerminal::Failed(decoder_error(error)),
                };
                worker_control.finish(terminal);
            })
            .map_err(|error| StreamServiceError::WorkerStart(error.to_string()))?;
        Ok(StreamSession::new(session_id, latest_frame, control))
    }
}

fn monitor_rtsp_decoder(
    mut decoder: FfmpegRtspDecoder,
    latest: Arc<LatestDecodedFrameSlot>,
    control: &StreamOperationControl,
) -> StreamTerminal {
    let started = Instant::now();
    let mut observed_sequence = None;
    let mut last_frame = started;
    control.report(StreamServiceEvent::Metrics(StreamMetrics {
        decoder_backend: Some(decoder.backend().label().to_owned()),
        ..StreamMetrics::default()
    }));
    let mut last_metric_at = started;
    let mut last_metric_frames = 0_u64;
    loop {
        if control.cancellation.is_cancelled() {
            decoder.shutdown();
            return StreamTerminal::Cancelled;
        }
        let now = Instant::now();
        if let Some(frame) = latest.latest()
            && observed_sequence != Some(frame.identity.frame_sequence)
        {
            let first_frame = observed_sequence.is_none();
            observed_sequence = Some(frame.identity.frame_sequence);
            last_frame = now;
            if first_frame {
                control.report(camera_toolbox_app::StreamServiceEvent::Stage(
                    StreamStage::Playing,
                ));
            }
        }
        let decoded_frames = decoder.stats().decoded_frames();
        if now.saturating_duration_since(last_metric_at) >= Duration::from_millis(500) {
            let elapsed_ns =
                u64::try_from(now.saturating_duration_since(last_metric_at).as_nanos())
                    .unwrap_or(u64::MAX);
            let decoded_fps_millihz = if elapsed_ns == 0 {
                0
            } else {
                u64::try_from(
                    u128::from(decoded_frames.saturating_sub(last_metric_frames))
                        .saturating_mul(1_000_000_000_000)
                        / u128::from(elapsed_ns),
                )
                .unwrap_or(u64::MAX)
            };
            control.report(StreamServiceEvent::Metrics(StreamMetrics {
                decoded_frames,
                decoded_fps_millihz,
                decoder_backend: Some(decoder.backend().label().to_owned()),
                ..StreamMetrics::default()
            }));
            last_metric_at = now;
            last_metric_frames = decoded_frames;
        }
        if let Some(completion) = decoder.completion() {
            let stage = if observed_sequence.is_some() {
                StreamStage::Playing
            } else {
                StreamStage::Connecting
            };
            decoder.shutdown();
            return match completion {
                Ok(()) if observed_sequence.is_some() => StreamTerminal::BoundaryClosed,
                Ok(()) => StreamTerminal::Failed(StreamServiceError::Transport {
                    stage,
                    reason: "FFmpeg decoder stopped before publishing a frame".to_owned(),
                }),
                Err(reason) => {
                    StreamTerminal::Failed(StreamServiceError::Transport { stage, reason })
                }
            };
        }
        if observed_sequence.is_none()
            && now.saturating_duration_since(started) >= control.timeouts.connect
        {
            decoder.shutdown();
            return StreamTerminal::Failed(StreamServiceError::ConnectTimeout {
                timeout_ms: duration_millis(control.timeouts.connect),
            });
        }
        if observed_sequence.is_some()
            && now.saturating_duration_since(last_frame) >= control.timeouts.idle
        {
            decoder.shutdown();
            return StreamTerminal::Failed(StreamServiceError::IdleTimeout {
                stage: StreamStage::Playing,
                timeout_ms: duration_millis(control.timeouts.idle),
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn decoder_error(error: FfmpegRtspDecoderError) -> StreamServiceError {
    match error {
        FfmpegRtspDecoderError::FrameSizeOverflow => {
            StreamServiceError::InvalidRequest("RTSP frame extent overflows host memory".to_owned())
        }
        FfmpegRtspDecoderError::Initialization(reason)
        | FfmpegRtspDecoderError::WorkerStart(reason) => StreamServiceError::WorkerStart(reason),
    }
}
