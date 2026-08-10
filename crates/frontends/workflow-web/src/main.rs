mod workflow;

use std::{
    net::IpAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::Query,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use camera_toolbox_adapters::media::{FfmpegRtspDecoder, FfmpegRtspTransport};
use camera_toolbox_app::{
    DecodedVideoFrame, LatestDecodedFrameSlot, RtspLatencyMode, StreamCancellation, StreamSessionId,
};
use clap::Parser;
use image::{ColorType, codecs::jpeg::JpegEncoder};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, time::sleep};
use tower_http::services::{ServeDir, ServeFile};
use workflow::{WorkflowGraph, seed_workflow_graph, validate_edge};

static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Parser)]
#[command(name = "camera-toolbox-workflow-web")]
#[command(about = "Camera Toolbox browser workflow canvas server")]
struct ServerArgs {
    /// Web 服务绑定地址；默认允许局域网设备访问，生产环境需要另加认证或防火墙。
    #[arg(long, default_value = "0.0.0.0")]
    host: IpAddr,

    /// Web 服务端口；传 0 时由系统分配可用端口。
    #[arg(long, default_value_t = 8787)]
    port: u16,

    /// 前端静态资源目录；默认使用本 crate 下的 web/dist。
    #[arg(long)]
    static_dir: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    service: &'static str,
    status: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MjpegStreamQuery {
    url: String,
    fps: Option<u16>,
    width: Option<u16>,
    height: Option<u16>,
}

#[derive(Debug, Clone)]
struct MjpegStreamConfig {
    url: String,
    fps: u16,
    width: u16,
    height: u16,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let _logging = camera_toolbox_logging::init();
    let args = ServerArgs::parse();
    let static_dir = args.static_dir.unwrap_or_else(default_static_dir);
    ensure_static_dir(&static_dir)?;

    let listener = TcpListener::bind((args.host, args.port))
        .await
        .with_context(|| format!("failed to bind {}:{}", args.host, args.port))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read listener address")?;
    let router = app_router(static_dir.clone());

    println!("Camera Toolbox Workflow Web listening on http://{local_addr}");
    println!("Serving frontend assets from {}", static_dir.display());
    tracing::info!(operation = "workflow_web_start", address = %local_addr, static_dir = %static_dir.display());

    axum::serve(listener, router)
        .await
        .context("workflow web server stopped unexpectedly")
}

fn app_router(static_dir: PathBuf) -> Router {
    let index = static_dir.join("index.html");
    let frontend = ServeDir::new(static_dir).not_found_service(ServeFile::new(index));

    Router::new()
        .route("/api/health", get(health))
        .route("/api/workflow", get(workflow_graph))
        .route("/api/streams/mjpeg", get(mjpeg_stream))
        .fallback_service(frontend)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        service: "camera-toolbox-workflow-web",
        status: "ok",
    })
}

async fn workflow_graph() -> Json<WorkflowGraph> {
    let graph = seed_workflow_graph();
    for edge in &graph.edges {
        debug_assert!(validate_edge(&graph, edge).is_ok());
    }
    Json(graph)
}

async fn mjpeg_stream(
    Query(query): Query<MjpegStreamQuery>,
) -> std::result::Result<Response, (StatusCode, String)> {
    let config = MjpegStreamConfig::from_query(query)?;
    let latest_frame = Arc::new(LatestDecodedFrameSlot::default());
    let cancellation = StreamCancellation::default();
    let session_id = StreamSessionId::new(format!(
        "workflow-mjpeg-{}",
        NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed)
    ))
    .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let decoder = FfmpegRtspDecoder::start(
        &config.url,
        FfmpegRtspTransport::Tcp,
        RtspLatencyMode::Low,
        u32::from(config.width),
        u32::from(config.height),
        session_id,
        0,
        Arc::clone(&latest_frame),
        Duration::from_secs(8),
        false,
        &cancellation,
    )
    .map_err(|error| {
        (
            StatusCode::BAD_GATEWAY,
            format!("failed to start internal RTSP decoder: {error}"),
        )
    })?;

    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(config.fps));
    let body_stream = async_stream::stream! {
        let _decoder = decoder;
        let _cancellation = cancellation;
        let mut last_sequence = None;
        loop {
            if let Some(completion) = _decoder.completion() {
                if let Err(error) = completion {
                    tracing::debug!(operation = "mjpeg_internal_decoder", error = %error);
                }
                break;
            }
            if let Some(frame) = latest_frame.latest()
                && last_sequence != Some(frame.identity.frame_sequence)
            {
                last_sequence = Some(frame.identity.frame_sequence);
                match mjpeg_chunk(&frame) {
                    Ok(chunk) => yield Ok::<Bytes, std::io::Error>(Bytes::from(chunk)),
                    Err(error) => yield Err(std::io::Error::other(error)),
                }
                sleep(frame_interval).await;
                continue;
            }
            sleep(Duration::from_millis(10)).await;
        }
    };

    let mut response = Body::from_stream(body_stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("multipart/x-mixed-replace; boundary=frame"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    Ok(response)
}

fn mjpeg_chunk(frame: &DecodedVideoFrame) -> Result<Vec<u8>, String> {
    let jpeg = encode_rgba_as_jpeg(frame)?;
    let mut chunk = Vec::with_capacity(jpeg.len() + 64);
    chunk.extend_from_slice(b"--frame\r\nContent-Type: image/jpeg\r\n\r\n");
    chunk.extend_from_slice(&jpeg);
    chunk.extend_from_slice(b"\r\n");
    Ok(chunk)
}

fn encode_rgba_as_jpeg(frame: &DecodedVideoFrame) -> Result<Vec<u8>, String> {
    let pixel_count = u64::from(frame.width)
        .checked_mul(u64::from(frame.height))
        .ok_or_else(|| "frame dimensions overflow".to_owned())?;
    let expected_rgba_len = usize::try_from(pixel_count.saturating_mul(4))
        .map_err(|_| "frame byte length overflows usize".to_owned())?;
    if frame.rgba.len() != expected_rgba_len {
        return Err(format!(
            "RGBA frame length mismatch: expected {expected_rgba_len}, got {}",
            frame.rgba.len()
        ));
    }
    let rgb_len = usize::try_from(pixel_count.saturating_mul(3))
        .map_err(|_| "RGB frame byte length overflows usize".to_owned())?;
    let mut rgb = Vec::with_capacity(rgb_len);
    for pixel in frame.rgba.chunks_exact(4) {
        rgb.extend_from_slice(&pixel[..3]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 82)
        .encode(&rgb, frame.width, frame.height, ColorType::Rgb8.into())
        .map_err(|error| format!("JPEG encode failed: {error}"))?;
    Ok(jpeg)
}

impl MjpegStreamConfig {
    fn from_query(query: MjpegStreamQuery) -> std::result::Result<Self, (StatusCode, String)> {
        let url = query.url.trim();
        if !(url.starts_with("rtsp://") || url.starts_with("rtsps://")) {
            return Err((
                StatusCode::BAD_REQUEST,
                "viewer stream URL must use rtsp:// or rtsps://".to_owned(),
            ));
        }
        let width = query.width.unwrap_or(960).clamp(160, 1920);
        let default_height = u16::try_from(u32::from(width).saturating_mul(9) / 16)
            .unwrap_or(u16::MAX)
            .clamp(90, 1080);
        Ok(Self {
            url: url.to_owned(),
            fps: query.fps.unwrap_or(30).clamp(1, 30),
            width,
            height: query.height.unwrap_or(default_height).clamp(90, 1080),
        })
    }
}

fn default_static_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web/dist")
}

fn ensure_static_dir(static_dir: &PathBuf) -> Result<()> {
    let index = static_dir.join("index.html");
    if !index.is_file() {
        bail!(
            "frontend build not found at `{}`; run `npm install && npm run build` in crates/frontends/workflow-web/web first, or pass --static-dir",
            static_dir.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_static_dir_points_to_web_dist() {
        let path = default_static_dir();
        assert!(path.ends_with("web/dist"));
    }

    #[test]
    fn mjpeg_config_rejects_non_rtsp_url() {
        let result = MjpegStreamConfig::from_query(MjpegStreamQuery {
            url: "http://camera.local/stream".to_owned(),
            fps: None,
            width: None,
            height: None,
        });
        assert!(matches!(result, Err((StatusCode::BAD_REQUEST, _))));
    }

    #[test]
    fn mjpeg_config_clamps_preview_cost() {
        let config = MjpegStreamConfig::from_query(MjpegStreamQuery {
            url: "rtsp://camera.local/stream".to_owned(),
            fps: Some(120),
            width: Some(4096),
            height: Some(4096),
        })
        .expect("valid RTSP URL");
        assert_eq!(config.fps, 30);
        assert_eq!(config.width, 1920);
        assert_eq!(config.height, 1080);

        let default_height = MjpegStreamConfig::from_query(MjpegStreamQuery {
            url: "rtsp://camera.local/stream".to_owned(),
            fps: None,
            width: Some(960),
            height: None,
        })
        .expect("valid RTSP URL");
        assert_eq!(default_height.fps, 30);
        assert_eq!(default_height.height, 540);
    }
}
