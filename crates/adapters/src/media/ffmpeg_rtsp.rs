//! RTSP → RGBA 的进程内 FFmpeg 解码器；直接保留解码帧 PTS 与时间基。

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use camera_toolbox_app::{
    DecodedVideoFrame, LatestDecodedFrameSlot, SourcePts, SourcePtsProvenance, StreamCancellation,
    StreamFrameIdentity, StreamSessionId, host_monotonic_time_ns,
};
use camera_toolbox_ffmpeg_bridge::input_with_dictionary_and_interrupt;
use ffmpeg_next as ffmpeg;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FfmpegRtspDecoderError {
    #[error("decoded frame dimensions overflow host memory")]
    FrameSizeOverflow,
    #[error("FFmpeg library initialization failed: {0}")]
    Initialization(String),
    #[error("failed to start FFmpeg RTSP worker: {0}")]
    WorkerStart(String),
}

#[derive(Clone, Copy)]
pub enum FfmpegRtspTransport {
    Tcp,
    Udp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfmpegDecoderBackend {
    Software,
    SoftwareFallback,
}

impl FfmpegDecoderBackend {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Software => "Software",
            Self::SoftwareFallback => {
                "Software fallback (hardware backend unavailable in this build)"
            }
        }
    }
}

#[derive(Default)]
pub struct FfmpegRtspDecoderStats {
    decoded_frames: AtomicU64,
}

impl FfmpegRtspDecoderStats {
    #[must_use]
    pub fn decoded_frames(&self) -> u64 {
        self.decoded_frames.load(Ordering::Acquire)
    }
}

#[derive(Default)]
struct DecoderState {
    shutdown_requested: AtomicBool,
    finished: AtomicBool,
    completion: Mutex<Option<Result<(), String>>>,
}

pub struct FfmpegRtspDecoder {
    state: Arc<DecoderState>,
    latest: Arc<LatestDecodedFrameSlot>,
    stats: Arc<FfmpegRtspDecoderStats>,
    backend: FfmpegDecoderBackend,
    worker: Option<JoinHandle<()>>,
}

impl Drop for FfmpegRtspDecoder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl FfmpegRtspDecoder {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        url: &str,
        transport: FfmpegRtspTransport,
        width: u32,
        height: u32,
        session_id: StreamSessionId,
        channel: u16,
        latest: Arc<LatestDecodedFrameSlot>,
        io_timeout: Duration,
        prefer_hardware_acceleration: bool,
        cancellation: &StreamCancellation,
    ) -> Result<Self, FfmpegRtspDecoderError> {
        let frame_bytes = frame_byte_len(width, height)?;
        ffmpeg::init()
            .map_err(|error| FfmpegRtspDecoderError::Initialization(error.to_string()))?;
        ffmpeg::format::network::init();

        let state = Arc::new(DecoderState::default());
        let stats = Arc::new(FfmpegRtspDecoderStats::default());
        let backend = if prefer_hardware_acceleration {
            FfmpegDecoderBackend::SoftwareFallback
        } else {
            FfmpegDecoderBackend::Software
        };
        let interrupt_state = Arc::clone(&state);
        cancellation.register_interrupt(Arc::new(move || {
            interrupt_state
                .shutdown_requested
                .store(true, Ordering::Release);
        }));
        let force_state = Arc::clone(&state);
        cancellation.register_force_cleanup(Arc::new(move || {
            force_state
                .shutdown_requested
                .store(true, Ordering::Release);
        }));

        let worker_state = Arc::clone(&state);
        let worker_latest = Arc::clone(&latest);
        let worker_stats = Arc::clone(&stats);
        let worker_url = url.to_owned();
        let worker = std::thread::Builder::new()
            .name(format!("rtsp-{}-decode", session_id.as_str()))
            .spawn(move || {
                let result = decode_rtsp(
                    &worker_url,
                    transport,
                    width,
                    height,
                    frame_bytes,
                    session_id,
                    channel,
                    io_timeout,
                    &worker_state,
                    &worker_latest,
                    &worker_stats,
                );
                *worker_state
                    .completion
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
                worker_state.finished.store(true, Ordering::Release);
            })
            .map_err(|error| FfmpegRtspDecoderError::WorkerStart(error.to_string()))?;

        Ok(Self {
            state,
            latest,
            stats,
            backend,
            worker: Some(worker),
        })
    }

    #[must_use]
    pub fn backend(&self) -> FfmpegDecoderBackend {
        self.backend
    }

    #[must_use]
    pub fn stats(&self) -> &Arc<FfmpegRtspDecoderStats> {
        &self.stats
    }

    pub(crate) fn shutdown(&mut self) {
        self.state.shutdown_requested.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.latest.clear();
    }

    #[must_use]
    pub fn completion(&self) -> Option<Result<(), String>> {
        if !self.state.finished.load(Ordering::Acquire) {
            return None;
        }
        self.state
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_rtsp(
    url: &str,
    transport: FfmpegRtspTransport,
    output_width: u32,
    output_height: u32,
    frame_bytes: usize,
    session_id: StreamSessionId,
    channel: u16,
    io_timeout: Duration,
    state: &Arc<DecoderState>,
    latest: &LatestDecodedFrameSlot,
    stats: &FfmpegRtspDecoderStats,
) -> Result<(), String> {
    let mut options = ffmpeg::Dictionary::new();
    options.set(
        "rtsp_transport",
        match transport {
            FfmpegRtspTransport::Tcp => "tcp",
            FfmpegRtspTransport::Udp => "udp",
        },
    );
    options.set("fflags", "nobuffer");
    options.set("max_delay", "0");
    let timeout_micros = ffmpeg_socket_timeout_micros(io_timeout).to_string();
    options.set("stimeout", &timeout_micros);
    options.set("rw_timeout", &timeout_micros);
    let interrupt_state = Arc::clone(state);
    let mut input = input_with_dictionary_and_interrupt(url, options, move || {
        interrupt_state.shutdown_requested.load(Ordering::Acquire)
    })
    .map_err(|error| format!("RTSP open failed: {error}"))?;
    let (stream_index, time_base, parameters) = {
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| "RTSP source has no video stream".to_owned())?;
        (stream.index(), stream.time_base(), stream.parameters())
    };
    let decoder_context = ffmpeg::codec::context::Context::from_parameters(parameters)
        .map_err(|error| format!("video decoder context failed: {error}"))?;
    let mut decoder = decoder_context.decoder();
    // `avcodec_parameters_to_context` 不复制 pkt_timebase。best-effort PTS 的单位
    // 必须由 demuxed video stream 明确传入，才可按该 stream time base 对外报告。
    decoder.set_packet_time_base(time_base);
    let mut decoder = decoder
        .video()
        .map_err(|error| format!("video decoder open failed: {error}"))?;
    let mut scaler = ffmpeg::software::scaling::Context::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        ffmpeg::format::Pixel::RGBA,
        output_width,
        output_height,
        ffmpeg::software::scaling::flag::Flags::BILINEAR,
    )
    .map_err(|error| format!("RGBA scaler initialization failed: {error}"))?;
    let mut decoded = ffmpeg::util::frame::video::Video::empty();
    let mut rgba = ffmpeg::util::frame::video::Video::empty();
    let mut frame_sequence = 0_u64;

    loop {
        if state.shutdown_requested.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {}
            Err(error) => {
                return packet_read_terminal(
                    error,
                    state.shutdown_requested.load(Ordering::Acquire),
                );
            }
        }
        if packet.stream() != stream_index {
            continue;
        }
        decoder
            .send_packet(&packet)
            .map_err(|error| format!("packet submission failed: {error}"))?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            if state.shutdown_requested.load(Ordering::Acquire) {
                return Ok(());
            }
            scaler
                .run(&decoded, &mut rgba)
                .map_err(|error| format!("RGBA conversion failed: {error}"))?;
            let mut bytes = vec![0_u8; frame_bytes];
            copy_rgba_tight(&rgba, output_width, output_height, &mut bytes)?;
            let source_pts = match (decoded.timestamp(), rational_parts(time_base)) {
                (Some(ticks), Some((numerator, denominator))) => SourcePts::Known {
                    ticks,
                    time_base_numerator: numerator,
                    time_base_denominator: denominator,
                    provenance: SourcePtsProvenance::FfmpegDecodedFrame,
                },
                (Some(_), None) => SourcePts::Unavailable {
                    reason: "FFmpeg decoded frame PTS has an invalid stream time base".to_owned(),
                },
                (None, _) => SourcePts::Unavailable {
                    reason: "FFmpeg decoded frame has no best-effort presentation timestamp"
                        .to_owned(),
                },
            };
            let published_host_time_ns = host_monotonic_time_ns();
            latest.publish(DecodedVideoFrame {
                width: output_width,
                height: output_height,
                rgba: bytes.into(),
                identity: StreamFrameIdentity::known_at(
                    session_id.clone(),
                    channel,
                    frame_sequence,
                    source_pts,
                    published_host_time_ns,
                ),
            });
            stats.decoded_frames.fetch_add(1, Ordering::Release);
            frame_sequence = frame_sequence.saturating_add(1);
        }
    }
}

/// 将 `av_read_frame` 结果转换为明确的 stream terminal 语义。
///
/// 只有本地已请求取消时的 `AVERROR_EXIT` 才是正常关闭；所有 EOF、timeout 和
/// transport 错误都会结束 worker，不能被迭代器吞掉后无限重试。
fn packet_read_terminal(error: ffmpeg::Error, shutdown_requested: bool) -> Result<(), String> {
    match error {
        ffmpeg::Error::Exit if shutdown_requested => Ok(()),
        ffmpeg::Error::Exit => {
            Err("RTSP packet read interrupted without local cancellation".to_owned())
        }
        ffmpeg::Error::Eof => Err("RTSP packet source ended".to_owned()),
        ffmpeg::Error::Other { errno } => {
            let io_error = std::io::Error::from_raw_os_error(errno);
            match io_error.kind() {
                std::io::ErrorKind::TimedOut => {
                    Err(format!("RTSP packet read timed out: {io_error}"))
                }
                std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::NotConnected => {
                    Err(format!("RTSP packet transport failed: {io_error}"))
                }
                _ => Err(format!("RTSP packet read failed: {io_error}")),
            }
        }
        error => Err(format!("RTSP packet read failed: {error}")),
    }
}

fn frame_byte_len(width: u32, height: u32) -> Result<usize, FfmpegRtspDecoderError> {
    usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(FfmpegRtspDecoderError::FrameSizeOverflow)
}

fn ffmpeg_socket_timeout_micros(timeout: Duration) -> u64 {
    let micros = u64::try_from(timeout.as_micros()).unwrap_or(u64::MAX);
    micros.min(u64::try_from(i32::MAX).unwrap_or(u64::MAX))
}

fn rational_parts(time_base: ffmpeg::Rational) -> Option<(u32, u32)> {
    let numerator = u32::try_from(time_base.numerator()).ok()?;
    let denominator = u32::try_from(time_base.denominator()).ok()?;
    (denominator != 0).then_some((numerator, denominator))
}

fn copy_rgba_tight(
    frame: &ffmpeg::util::frame::video::Video,
    width: u32,
    height: u32,
    destination: &mut [u8],
) -> Result<(), String> {
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| "RGBA row width overflowed host memory".to_owned())?;
    let rows = usize::try_from(height).map_err(|_| "RGBA height overflowed host memory")?;
    let stride = frame.stride(0);
    let source = frame.data(0);
    if stride < row_bytes
        || source.len() < stride.saturating_mul(rows)
        || destination.len() != row_bytes * rows
    {
        return Err(
            "FFmpeg RGBA frame layout does not match the configured output extent".to_owned(),
        );
    }
    for row in 0..rows {
        let source_start = row * stride;
        let destination_start = row * row_bytes;
        destination[destination_start..destination_start + row_bytes]
            .copy_from_slice(&source[source_start..source_start + row_bytes]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_byte_len_rejects_overflow() {
        assert!(matches!(
            frame_byte_len(u32::MAX, u32::MAX),
            Err(FfmpegRtspDecoderError::FrameSizeOverflow)
        ));
    }

    #[test]
    fn hardware_preference_is_never_misreported_as_active_hardware() {
        assert_eq!(
            FfmpegDecoderBackend::SoftwareFallback.label(),
            "Software fallback (hardware backend unavailable in this build)"
        );
    }

    #[test]
    fn packet_read_interrupt_is_normal_only_after_local_cancellation() {
        assert_eq!(packet_read_terminal(ffmpeg::Error::Exit, true), Ok(()));
        assert!(
            packet_read_terminal(ffmpeg::Error::Exit, false)
                .expect_err("unexpected interrupt must fail")
                .contains("without local cancellation")
        );
    }

    #[test]
    fn packet_read_eof_and_other_errors_are_terminal_failures() {
        assert!(
            packet_read_terminal(ffmpeg::Error::Eof, false)
                .expect_err("EOF must terminate the worker")
                .contains("source ended")
        );
        assert!(
            packet_read_terminal(ffmpeg::Error::Other { errno: 0 }, false)
                .expect_err("transport error must terminate the worker")
                .contains("packet read failed")
        );
    }
    #[test]
    fn decoded_best_effort_pts_preserves_non_default_stream_time_base() {
        ffmpeg::init().expect("initialize FFmpeg");
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pts_1_90000.mp4");
        let mut input = ffmpeg::format::input(&fixture).expect("open PTS fixture");
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .expect("fixture video stream");
        let stream_index = stream.index();
        let time_base = stream.time_base();
        assert_eq!(
            (time_base.numerator(), time_base.denominator()),
            (1, 90_000)
        );
        let decoder_context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .expect("construct decoder context");
        let mut decoder = decoder_context.decoder();
        decoder.set_packet_time_base(time_base);
        assert_eq!(
            (
                decoder.packet_time_base().numerator(),
                decoder.packet_time_base().denominator(),
            ),
            (1, 90_000)
        );
        let mut decoder = decoder.video().expect("open fixture decoder");
        let mut frame = ffmpeg::util::frame::video::Video::empty();
        let mut timestamps = Vec::new();
        for (stream, packet) in input.packets() {
            if stream.index() != stream_index {
                continue;
            }
            decoder.send_packet(&packet).expect("submit fixture packet");
            while decoder.receive_frame(&mut frame).is_ok() {
                timestamps.push(frame.timestamp());
            }
        }
        decoder.send_eof().expect("flush fixture decoder");
        while decoder.receive_frame(&mut frame).is_ok() {
            timestamps.push(frame.timestamp());
        }
        assert_eq!(timestamps, vec![Some(0), Some(18_000), Some(36_000)]);
    }
}
