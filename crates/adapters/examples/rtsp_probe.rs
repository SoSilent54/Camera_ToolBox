use std::{
    env, process,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use camera_toolbox_adapters::media::{FfmpegRtspDecoder, FfmpegRtspTransport};
use camera_toolbox_app::{
    LatestDecodedFrameSlot, RtspLatencyMode, StreamCancellation, StreamSessionId,
};

fn parse_u32(name: &str, default: u32) -> u32 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn parse_duration(name: &str, default_secs: u64) -> Duration {
    Duration::from_secs(
        env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default_secs),
    )
}

fn parse_transport() -> FfmpegRtspTransport {
    match env::var("CAMERA_TOOLBOX_RTSP_PROBE_TRANSPORT")
        .unwrap_or_else(|_| "tcp".to_owned())
        .to_ascii_lowercase()
        .as_str()
    {
        "udp" => FfmpegRtspTransport::Udp,
        _ => FfmpegRtspTransport::Tcp,
    }
}

fn parse_latency() -> RtspLatencyMode {
    match env::var("CAMERA_TOOLBOX_RTSP_PROBE_LATENCY")
        .unwrap_or_else(|_| "stable".to_owned())
        .to_ascii_lowercase()
        .as_str()
    {
        "low" => RtspLatencyMode::Low,
        _ => RtspLatencyMode::Stable,
    }
}

fn main() {
    let url = env::var("CAMERA_TOOLBOX_RTSP_PROBE_URL")
        .unwrap_or_else(|_| "rtsp://10.21.12.108/PRR".to_owned());
    let width = parse_u32("CAMERA_TOOLBOX_RTSP_PROBE_WIDTH", 1920);
    let height = parse_u32("CAMERA_TOOLBOX_RTSP_PROBE_HEIGHT", 1080);
    let duration = parse_duration("CAMERA_TOOLBOX_RTSP_PROBE_SECONDS", 10);
    let transport = parse_transport();
    let latency = parse_latency();
    let latest = Arc::new(LatestDecodedFrameSlot::default());
    let cancellation = StreamCancellation::default();
    let session_id = StreamSessionId::new("rtsp-probe").expect("static probe session id is valid");
    let decoder = match FfmpegRtspDecoder::start(
        &url,
        transport,
        latency,
        width,
        height,
        session_id,
        0,
        Arc::clone(&latest),
        Duration::from_secs(5),
        false,
        &cancellation,
    ) {
        Ok(decoder) => decoder,
        Err(error) => {
            eprintln!("RTSP probe failed to start: {error}");
            process::exit(1);
        }
    };

    let started = Instant::now();
    let mut observed_frames = 0_u64;
    let mut skipped_frames = 0_u64;
    let mut last_sequence = None;
    while started.elapsed() < duration {
        if let Some(frame) = latest.latest() {
            let sequence = frame.identity.frame_sequence;
            if last_sequence != Some(sequence) {
                if let Some(last) = last_sequence {
                    skipped_frames =
                        skipped_frames.saturating_add(sequence.saturating_sub(last + 1));
                }
                last_sequence = Some(sequence);
                observed_frames = observed_frames.saturating_add(1);
            }
        }
        if let Some(completion) = decoder.completion() {
            eprintln!("RTSP decoder completed early: {completion:?}");
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let elapsed = started.elapsed().as_secs_f64();
    let stats = decoder.stats().snapshot();
    let decoded_frames = stats.decoded_frames;
    println!(
        "url={} width={} height={} duration_s={:.3} decoded_frames={} decoded_fps={:.3} observed_frames={} observed_fps={:.3} skipped_frames={} backend={} codec_avg_ms={:.3} scale_avg_ms={:.3} copy_avg_ms={:.3}",
        url,
        width,
        height,
        elapsed,
        decoded_frames,
        decoded_frames as f64 / elapsed,
        observed_frames,
        observed_frames as f64 / elapsed,
        skipped_frames,
        decoder.backend().label(),
        stats.codec_stage_ns as f64 / decoded_frames.max(1) as f64 / 1_000_000.0,
        stats.scale_stage_ns as f64 / decoded_frames.max(1) as f64 / 1_000_000.0,
        stats.copy_stage_ns as f64 / decoded_frames.max(1) as f64 / 1_000_000.0,
    );
    cancellation.cancel();
}
