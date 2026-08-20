//! RTSP → RGBA 的进程内 FFmpeg 解码器；直接保留解码帧 PTS 与时间基。

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use camera_toolbox_app::{
    DecodedVideoFrame, LatestDecodedFrameSlot, RtspLatencyMode, SourcePts, SourcePtsProvenance,
    StreamCancellation, StreamFrameIdentity, StreamSessionId, host_monotonic_time_ns,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FfmpegRtspDecoderStatsSnapshot {
    pub decoded_frames: u64,
    pub io_bytes_available: bool,
    pub io_bytes: u64,
    pub media_packet_bytes: u64,
    pub codec_stage_ns: u64,
    pub scale_stage_ns: u64,
    pub copy_stage_ns: u64,
}

#[derive(Default)]
pub struct FfmpegRtspDecoderStats {
    decoded_frames: AtomicU64,
    io_bytes_available: AtomicBool,
    io_bytes: AtomicU64,
    media_packet_bytes: AtomicU64,
    codec_stage_ns: AtomicU64,
    scale_stage_ns: AtomicU64,
    copy_stage_ns: AtomicU64,
}

impl FfmpegRtspDecoderStats {
    #[must_use]
    pub fn decoded_frames(&self) -> u64 {
        self.decoded_frames.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn snapshot(&self) -> FfmpegRtspDecoderStatsSnapshot {
        FfmpegRtspDecoderStatsSnapshot {
            decoded_frames: self.decoded_frames.load(Ordering::Acquire),
            io_bytes_available: self.io_bytes_available.load(Ordering::Acquire),
            io_bytes: self.io_bytes.load(Ordering::Acquire),
            media_packet_bytes: self.media_packet_bytes.load(Ordering::Acquire),
            codec_stage_ns: self.codec_stage_ns.load(Ordering::Acquire),
            scale_stage_ns: self.scale_stage_ns.load(Ordering::Acquire),
            copy_stage_ns: self.copy_stage_ns.load(Ordering::Acquire),
        }
    }

    fn record_io_bytes(&self, bytes: Option<u64>) {
        let Some(bytes) = bytes else {
            return;
        };
        self.io_bytes.store(bytes, Ordering::Release);
        self.io_bytes_available.store(true, Ordering::Release);
    }

    fn record_media_packet_bytes(&self, bytes: usize) {
        self.media_packet_bytes
            .fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    fn record_codec_stage(&self, elapsed: Duration) {
        self.codec_stage_ns
            .fetch_add(duration_nanos(elapsed), Ordering::Relaxed);
    }

    fn record_scale_stage(&self, elapsed: Duration) {
        self.scale_stage_ns
            .fetch_add(duration_nanos(elapsed), Ordering::Relaxed);
    }

    fn record_copy_stage(&self, elapsed: Duration) {
        self.copy_stage_ns
            .fetch_add(duration_nanos(elapsed), Ordering::Relaxed);
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
        latency_mode: RtspLatencyMode,
        width: u32,
        height: u32,
        session_id: StreamSessionId,
        channel: u16,
        latest: Arc<LatestDecodedFrameSlot>,
        io_timeout: Duration,
        prefer_hardware_acceleration: bool,
        cancellation: &StreamCancellation,
    ) -> Result<Self, FfmpegRtspDecoderError> {
        frame_byte_len(width, height)?;
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
                    latency_mode,
                    width,
                    height,
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
    latency_mode: RtspLatencyMode,
    output_width: u32,
    output_height: u32,
    session_id: StreamSessionId,
    channel: u16,
    io_timeout: Duration,
    state: &Arc<DecoderState>,
    latest: &LatestDecodedFrameSlot,
    stats: &FfmpegRtspDecoderStats,
) -> Result<(), String> {
    let options = rtsp_input_options(transport, latency_mode, io_timeout);
    let interrupt_state = Arc::clone(state);
    let mut input = input_with_dictionary_and_interrupt(url, options, move || {
        interrupt_state.shutdown_requested.load(Ordering::Acquire)
    })
    .map_err(|error| format!("RTSP open failed: {error}"))?;
    stats.record_io_bytes(input.io_bytes());
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
    decoder.set_threading(rtsp_decoder_threading());
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
    // 当前 X5 H.264 编码器没有 B 帧：每个送入 decoder 的 access unit 至多产生一个
    // 对应解码帧，因此按提交顺序保存包侧 SEI 时间戳即可，不得从 PTS 推断。
    let mut pending_device_timestamps = VecDeque::new();

    loop {
        if state.shutdown_requested.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => stats.record_io_bytes(input.io_bytes()),
            Err(error) => {
                return packet_read_terminal(
                    error,
                    state.shutdown_requested.load(Ordering::Acquire),
                );
            }
        }
        stats.record_media_packet_bytes(packet.size());
        if packet.stream() != stream_index {
            continue;
        }
        let device_timestamp_ns = packet.data().and_then(parse_h264_annex_b_timestamp_ns);
        pending_device_timestamps.push_back(device_timestamp_ns);
        let codec_start = Instant::now();
        decoder
            .send_packet(&packet)
            .map_err(|error| format!("packet submission failed: {error}"))?;
        stats.record_codec_stage(codec_start.elapsed());
        loop {
            let codec_start = Instant::now();
            if decoder.receive_frame(&mut decoded).is_err() {
                stats.record_codec_stage(codec_start.elapsed());
                break;
            }
            stats.record_codec_stage(codec_start.elapsed());
            if state.shutdown_requested.load(Ordering::Acquire) {
                return Ok(());
            }
            let scale_start = Instant::now();
            scaler
                .run(&decoded, &mut rgba)
                .map_err(|error| format!("RGBA conversion failed: {error}"))?;
            stats.record_scale_stage(scale_start.elapsed());
            let copy_start = Instant::now();
            let bytes = copy_rgba_tight(&rgba, output_width, output_height)?;
            stats.record_copy_stage(copy_start.elapsed());
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
            let device_timestamp_ns = pending_device_timestamps.pop_front().unwrap_or(None);
            let published_host_time_ns = host_monotonic_time_ns();
            latest.publish(DecodedVideoFrame {
                width: output_width,
                height: output_height,
                rgba: bytes.into(),
                identity: StreamFrameIdentity::known_at_with_device_timestamp(
                    session_id.clone(),
                    channel,
                    frame_sequence,
                    source_pts,
                    published_host_time_ns,
                    device_timestamp_ns,
                ),
            });
            stats.decoded_frames.fetch_add(1, Ordering::Release);
            frame_sequence = frame_sequence.saturating_add(1);
        }
    }
}

const X5_TIMESTAMP_SEI_UUID: [u8; 16] = [
    0x58, 0x35, 0x54, 0x53, 0x50, 0x4e, 0x53, 0x00, 0x8a, 0x75, 0x42, 0x1e, 0x91, 0x0f, 0x20, 0x26,
];

/// 从 Annex-B H.264 access unit 读取 X5 注入的 user_data_unregistered SEI。
///
/// 发送端约定 payload type 为 5、payload 为 16-byte UUID 加大端 `timestamp_ns`。
/// 解析仅检查编码流明确携带的值，绝不以 PTS 或本机时钟填补缺失时间戳。
fn parse_h264_annex_b_timestamp_ns(access_unit: &[u8]) -> Option<u64> {
    let mut search_from = 0;
    while let Some((start_code_offset, nal_offset)) =
        h264_annex_b_start_code(access_unit, search_from)
    {
        let next_start_code = h264_annex_b_start_code(access_unit, nal_offset)
            .map_or(access_unit.len(), |(offset, _)| offset);
        let nal = &access_unit[nal_offset..next_start_code];
        if nal.first().is_some_and(|header| header & 0x1f == 6)
            && let Some(timestamp_ns) = parse_h264_timestamp_sei_rbsp(&nal[1..])
        {
            return Some(timestamp_ns);
        }
        search_from = next_start_code;
        if next_start_code == access_unit.len() || next_start_code <= start_code_offset {
            break;
        }
    }
    None
}

/// 返回起始码位置及其后第一个 NAL byte 的位置，兼容三、四字节 Annex-B 起始码。
fn h264_annex_b_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut offset = from;
    while offset + 3 <= data.len() {
        if data[offset] == 0 && data[offset + 1] == 0 {
            if data[offset + 2] == 1 {
                return Some((offset, offset + 3));
            }
            if offset + 4 <= data.len() && data[offset + 2] == 0 && data[offset + 3] == 1 {
                return Some((offset, offset + 4));
            }
        }
        offset += 1;
    }
    None
}

/// H.264 RBSP byte reader：跳过 emulation-prevention three-byte，避免复制 NAL payload。
struct H264RbspReader<'a> {
    data: &'a [u8],
    offset: usize,
    preceding_zeroes: u8,
}

impl<'a> H264RbspReader<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            offset: 0,
            preceding_zeroes: 0,
        }
    }

    fn next(&mut self) -> Option<u8> {
        while let Some(&byte) = self.data.get(self.offset) {
            self.offset += 1;
            if self.preceding_zeroes >= 2
                && byte == 3
                && self.data.get(self.offset).is_some_and(|next| *next <= 3)
            {
                self.preceding_zeroes = 0;
                continue;
            }
            self.preceding_zeroes = if byte == 0 {
                self.preceding_zeroes.saturating_add(1)
            } else {
                0
            };
            return Some(byte);
        }
        None
    }
}

fn parse_h264_timestamp_sei_rbsp(rbsp: &[u8]) -> Option<u64> {
    let mut reader = H264RbspReader::new(rbsp);
    while let Some(payload_type) = read_h264_sei_value(&mut reader) {
        let payload_size = read_h264_sei_value(&mut reader)?;
        if payload_type == 5 && payload_size == X5_TIMESTAMP_SEI_UUID.len() + 8 {
            let mut payload = [0_u8; 24];
            for byte in &mut payload {
                *byte = reader.next()?;
            }
            if payload[..X5_TIMESTAMP_SEI_UUID.len()] != X5_TIMESTAMP_SEI_UUID {
                continue;
            }
            let timestamp_bytes: [u8; 8] =
                payload[X5_TIMESTAMP_SEI_UUID.len()..].try_into().ok()?;
            return Some(u64::from_be_bytes(timestamp_bytes));
        }
        for _ in 0..payload_size {
            reader.next()?;
        }
    }
    None
}

/// `payloadType` 和 `payloadSize` 使用连续 `0xff` 加尾字节的可变长编码。
fn read_h264_sei_value(reader: &mut H264RbspReader<'_>) -> Option<usize> {
    let mut value = 0_usize;
    loop {
        let byte = reader.next()?;
        value = value.checked_add(usize::from(byte))?;
        if byte != u8::MAX {
            return Some(value);
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

fn rtsp_input_options(
    transport: FfmpegRtspTransport,
    latency_mode: RtspLatencyMode,
    io_timeout: Duration,
) -> ffmpeg::Dictionary<'static> {
    let mut options = ffmpeg::Dictionary::new();
    options.set(
        "rtsp_transport",
        match transport {
            FfmpegRtspTransport::Tcp => "tcp",
            FfmpegRtspTransport::Udp => "udp",
        },
    );
    if latency_mode == RtspLatencyMode::Low {
        options.set("fflags", "nobuffer");
        options.set("max_delay", "0");
    }
    let timeout_micros = ffmpeg_socket_timeout_micros(io_timeout).to_string();
    options.set("stimeout", &timeout_micros);
    options.set("rw_timeout", &timeout_micros);
    options
}

fn rtsp_decoder_threading() -> ffmpeg::codec::threading::Config {
    rtsp_decoder_threading_with_count(rtsp_decoder_thread_count())
}

fn rtsp_decoder_thread_count() -> usize {
    std::env::var("CAMERA_TOOLBOX_RTSP_DECODER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(default_rtsp_decoder_thread_count)
}

fn default_rtsp_decoder_thread_count() -> usize {
    std::thread::available_parallelism().map_or(2, |threads| threads.get().clamp(2, 4))
}

fn rtsp_decoder_threading_with_count(count: usize) -> ffmpeg::codec::threading::Config {
    let mut config = ffmpeg::codec::threading::Config::kind(ffmpeg::codec::threading::Type::Frame);
    config.count = count;
    config
}

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
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
) -> Result<Vec<u8>, String> {
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| "RGBA row width overflowed host memory".to_owned())?;
    let rows = usize::try_from(height).map_err(|_| "RGBA height overflowed host memory")?;
    let frame_bytes = row_bytes
        .checked_mul(rows)
        .ok_or_else(|| "RGBA frame size overflowed host memory".to_owned())?;
    let stride = frame.stride(0);
    let source = frame.data(0);
    if stride < row_bytes || source.len() < stride.saturating_mul(rows) {
        return Err(
            "FFmpeg RGBA frame layout does not match the configured output extent".to_owned(),
        );
    }
    let mut destination = Vec::with_capacity(frame_bytes);
    for row in 0..rows {
        let source_start = row * stride;
        destination.extend_from_slice(&source[source_start..source_start + row_bytes]);
    }
    Ok(destination)
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
    fn rtsp_decoder_uses_bounded_ffmpeg_frame_threading() {
        let threading = rtsp_decoder_threading_with_count(2);

        assert_eq!(threading.kind, ffmpeg::codec::threading::Type::Frame);
        assert_eq!(threading.count, 2);
    }

    #[test]
    fn rtsp_decoder_stats_publish_ffmpeg_io_bytes() {
        let stats = FfmpegRtspDecoderStats::default();

        stats.record_io_bytes(Some(12_345));

        assert_eq!(stats.snapshot().io_bytes, 12_345);
        assert!(stats.snapshot().io_bytes_available);
    }

    #[test]
    fn rtsp_decoder_stats_publish_media_packet_bytes() {
        let stats = FfmpegRtspDecoderStats::default();

        stats.record_media_packet_bytes(123);
        stats.record_media_packet_bytes(456);

        assert_eq!(stats.snapshot().media_packet_bytes, 579);
    }

    #[test]
    fn low_latency_rtsp_options_keep_nobuffer_and_zero_delay() {
        let options = rtsp_input_options(
            FfmpegRtspTransport::Tcp,
            RtspLatencyMode::Low,
            Duration::from_secs(2),
        );

        assert_eq!(options.get("rtsp_transport"), Some("tcp"));
        assert_eq!(options.get("fflags"), Some("nobuffer"));
        assert_eq!(options.get("max_delay"), Some("0"));
        assert_eq!(options.get("stimeout"), Some("2000000"));
        assert_eq!(options.get("rw_timeout"), Some("2000000"));
    }

    #[test]
    fn stable_rtsp_options_omit_low_latency_reorder_bypass() {
        let options = rtsp_input_options(
            FfmpegRtspTransport::Udp,
            RtspLatencyMode::Stable,
            Duration::from_secs(2),
        );

        assert_eq!(options.get("rtsp_transport"), Some("udp"));
        assert_eq!(options.get("fflags"), None);
        assert_eq!(options.get("max_delay"), None);
        assert_eq!(options.get("stimeout"), Some("2000000"));
        assert_eq!(options.get("rw_timeout"), Some("2000000"));
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
    #[test]
    fn h264_annex_b_timestamp_sei_reads_x5_user_data() {
        let timestamp_ns = 0x0102_0304_0506_0708_u64;
        let mut access_unit = vec![0, 0, 0, 1, 0x06, 5, 24];
        access_unit.extend_from_slice(&X5_TIMESTAMP_SEI_UUID);
        access_unit.extend_from_slice(&timestamp_ns.to_be_bytes());
        access_unit.push(0x80);
        access_unit.extend_from_slice(&[0, 0, 1, 0x65, 0x88]);

        assert_eq!(
            parse_h264_annex_b_timestamp_ns(&access_unit),
            Some(timestamp_ns)
        );
    }

    #[test]
    fn h264_annex_b_timestamp_sei_unescapes_timestamp_payload() {
        let timestamp_ns = 0x0000_0102_0304_0506_u64;
        let mut access_unit = vec![0, 0, 1, 0x06, 5, 24];
        access_unit.extend_from_slice(&X5_TIMESTAMP_SEI_UUID);
        access_unit.extend_from_slice(&[0, 0, 3, 1, 2, 3, 4, 5, 6]);
        access_unit.push(0x80);
        access_unit.extend_from_slice(&[0, 0, 1, 0x65]);

        assert_eq!(
            parse_h264_annex_b_timestamp_ns(&access_unit),
            Some(timestamp_ns)
        );
    }

    #[test]
    fn h264_annex_b_timestamp_sei_rejects_truncated_or_foreign_payload() {
        let mut truncated = vec![0, 0, 1, 0x06, 5, 24];
        truncated.extend_from_slice(&X5_TIMESTAMP_SEI_UUID);
        truncated.extend_from_slice(&[0; 7]);
        assert_eq!(parse_h264_annex_b_timestamp_ns(&truncated), None);

        let mut foreign = vec![0, 0, 1, 0x06, 5, 24];
        foreign.extend_from_slice(&[0; 16]);
        foreign.extend_from_slice(&1_u64.to_be_bytes());
        assert_eq!(parse_h264_annex_b_timestamp_ns(&foreign), None);
    }
}
