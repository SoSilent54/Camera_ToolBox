use std::{
    io::{Read, Write},
    net::TcpStream,
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

const X5_MAGIC: &[u8; 4] = b"X5D1";
const X5_PROTOCOL_VERSION: u16 = 1;
const X5_TYPE_REQUEST: u16 = 1;
const X5_TYPE_RESPONSE: u16 = 2;
const X5_TYPE_ERROR: u16 = 3;
const X5_HEADER_BYTES: usize = 36;
const X5_MAX_JSON_HEADER_BYTES: u32 = 64 * 1024;
const X5_MAX_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;
const X5_TIMEOUT: Duration = Duration::from_secs(3);
const X5_RTSP_READY_TIMEOUT: Duration = Duration::from_secs(5);
const X5_RTSP_READY_POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct X5RtspEncoderConfig {
    pub fps: u16,
    pub bitrate_kbps: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X5RtspApplySummary {
    pub apply_mode: String,
    pub fps: u16,
    pub bitrate_kbps: u32,
    pub pipeline_config_version: u64,
    pub action_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X5RtspStreamSummary {
    pub channel: Option<u16>,
    pub affected_channels: Vec<u16>,
    pub requested_enabled: bool,
    pub queued_action: String,
    pub worker_busy: bool,
    pub action_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X5DriverStatus {
    pub camera_running: bool,
    pub rtsp_started: bool,
    pub rtsp_tx_enabled: bool,
    pub rtsp_requested_enabled: bool,
    pub rtsp_control_busy: bool,
    pub rtsp_pending_action: String,
    pub rtsp_last_error: i64,
    pub rtsp_action_id: u64,
    pub rtsp_last_message: String,
    pub rtsp_channels: Vec<X5RtspChannelStatus>,
    pub rings: Vec<X5RingStatus>,
    pub fps: Option<u16>,
    pub bitrate_kbps: Option<u32>,
    pub pipeline_config_version: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X5RtspChannelStatus {
    pub channel: u16,
    pub runtime_enabled: bool,
    pub requested_enabled: bool,
    pub started: bool,
    pub tx_enabled: bool,
    pub busy: bool,
    pub pending_action: String,
    pub last_error: i64,
    pub action_id: u64,
    pub last_message: String,
    pub port: Option<u16>,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X5RingStatus {
    pub channel: u16,
    pub depth: u16,
    pub valid: u16,
    pub write_index: u16,
    pub min_frame_id: u64,
    pub max_frame_id: u64,
    pub last_frame_id: u64,
    pub min_timestamp_ns: u64,
    pub max_timestamp_ns: u64,
    pub last_timestamp_ns: u64,
    pub retention_ns: u64,
    pub dropped: u64,
    pub evicted: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X5YuvSnapshot {
    pub channel: u16,
    pub width: u32,
    pub height: u32,
    pub y_len: usize,
    pub uv_len: usize,
    pub frame_id: u64,
    pub timestamp_ns: u64,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X5RawSnapshot {
    pub camera: u16,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format_code: u32,
    pub frame_id: u64,
    pub timestamp_ns: u64,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X5ProbeSummary {
    pub protocol: u16,
    pub channels: Vec<u16>,
    pub fps: Option<u16>,
    pub bitrate_kbps: Option<u32>,
    pub pipeline_config_version: Option<u64>,
    pub rtsp_started: bool,
    pub rtsp_requested_enabled: bool,
    pub rtsp_channels: Vec<X5RtspChannelStatus>,
    pub rings: Vec<X5RingStatus>,
}

struct X5ResponseFrame {
    meta: Value,
    payload: Vec<u8>,
}

/// 查询 X5 TCP 控制面，确认协议、能力和当前 RTSP 编码/线程状态。
pub fn probe(host: &str, port: u16) -> Result<X5ProbeSummary, String> {
    let hello = request_json(host, port, 1, &json!({"cmd":"HELLO"}))?;
    ensure_ok(&hello)?;
    let protocol = value_u64(&hello, "protocol")
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(X5_PROTOCOL_VERSION);
    let capabilities = request_json(host, port, 2, &json!({"cmd":"GET_CAPABILITIES"}))?;
    ensure_ok(&capabilities)?;
    let channels = capabilities
        .get("channels")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_u64)
                .filter_map(|value| u16::try_from(value).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let status = status(host, port)?;
    Ok(X5ProbeSummary {
        protocol,
        channels,
        fps: status.fps,
        bitrate_kbps: status.bitrate_kbps,
        pipeline_config_version: status.pipeline_config_version,
        rtsp_started: status.rtsp_started,
        rtsp_requested_enabled: status.rtsp_requested_enabled,
        rtsp_channels: status.rtsp_channels,
        rings: status.rings,
    })
}

/// 读取驱动当前相机、RTSP worker、编码参数和 ring 状态。
pub fn status(host: &str, port: u16) -> Result<X5DriverStatus, String> {
    let response = request_json(host, port, 3, &json!({"cmd":"GET_STATUS"}))?;
    ensure_ok(&response)?;
    parse_status_response(&response)
}

/// 在打开 RTSP Viewer 前配置 X5 编码器参数，并用 GET_STATUS 复核服务端状态。
pub fn configure_rtsp(
    host: &str,
    port: u16,
    config: X5RtspEncoderConfig,
) -> Result<X5RtspApplySummary, String> {
    let probe = probe(host, port)?;
    if probe.fps == Some(config.fps) && probe.bitrate_kbps == Some(config.bitrate_kbps) {
        return Ok(X5RtspApplySummary {
            apply_mode: "unchanged".to_owned(),
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            pipeline_config_version: probe.pipeline_config_version.unwrap_or(0),
            action_id: None,
        });
    }

    let response = request_json(
        host,
        port,
        4,
        &json!({
            "cmd": "SET_RTSP_CONFIG",
            "fps": config.fps,
            "bitrate_kbps": config.bitrate_kbps,
        }),
    )?;
    ensure_ok(&response)?;
    let summary = X5RtspApplySummary {
        apply_mode: response
            .get("apply_mode")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        fps: required_u16(&response, "fps", "X5 SET_RTSP_CONFIG response")?,
        bitrate_kbps: required_u32(&response, "bitrate_kbps", "X5 SET_RTSP_CONFIG response")?,
        pipeline_config_version: required_u64(
            &response,
            "pipeline_config_version",
            "X5 SET_RTSP_CONFIG response",
        )?,
        action_id: value_u64(&response, "action_id"),
    };
    let status = status(host, port)?;
    let status_fps = status
        .fps
        .ok_or_else(|| "X5 GET_STATUS response missing venc_fps".to_owned())?;
    let status_bitrate = status
        .bitrate_kbps
        .ok_or_else(|| "X5 GET_STATUS response missing venc_bitrate_kbps".to_owned())?;
    if status_fps != config.fps || status_bitrate != config.bitrate_kbps {
        return Err(format!(
            "X5 RTSP config verification failed: status fps={status_fps}, bitrate={status_bitrate} kbps"
        ));
    }
    Ok(summary)
}

/// 请求驱动指定通道的 RTSP control worker 异步开启推流线程。
pub fn start_rtsp_channel(
    host: &str,
    port: u16,
    channel: u16,
) -> Result<X5RtspStreamSummary, String> {
    set_rtsp_stream_command(host, port, &json!({"cmd":"START_RTSP","channel":channel}))
}

/// 请求驱动指定通道的 RTSP control worker 异步关闭推流线程。
pub fn stop_rtsp_channel(
    host: &str,
    port: u16,
    channel: u16,
) -> Result<X5RtspStreamSummary, String> {
    set_rtsp_stream_command(host, port, &json!({"cmd":"STOP_RTSP","channel":channel}))
}

/// 等待指定 RTSP 通道开启完成；连接 RTSP URL 前必须先看到目标端口线程已就绪。
pub fn wait_for_rtsp_channels_ready(
    host: &str,
    port: u16,
    channels: &[u16],
) -> Result<X5DriverStatus, String> {
    let channels = normalized_channels(channels);
    let deadline = Instant::now() + X5_RTSP_READY_TIMEOUT;
    loop {
        let status = status(host, port)?;
        if channels.is_empty() {
            return Ok(status);
        }
        if status.rtsp_channels.is_empty() {
            if status.rtsp_started && status.rtsp_tx_enabled {
                return Ok(status);
            }
            if status.rtsp_last_error != 0 && !status.rtsp_control_busy {
                return Err(format!(
                    "X5 RTSP start failed: {}",
                    status.rtsp_last_message
                ));
            }
            if Instant::now() >= deadline {
                let detail = format!(
                    "started={} tx_enabled={} requested={} busy={} pending={} last={}",
                    status.rtsp_started,
                    status.rtsp_tx_enabled,
                    status.rtsp_requested_enabled,
                    status.rtsp_control_busy,
                    status.rtsp_pending_action,
                    status.rtsp_last_message
                );
                return Err(format!("X5 RTSP did not become ready within 5s: {detail}"));
            }
            thread::sleep(X5_RTSP_READY_POLL);
            continue;
        }

        let pending = match rtsp_readiness_pending(&status, &channels) {
            Ok(pending) => pending,
            Err(error) => return Err(error),
        };
        if pending.is_empty() {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "X5 RTSP channels {} did not become ready within 5s: {}",
                format_channel_list(&channels),
                pending.join("; ")
            ));
        }
        thread::sleep(X5_RTSP_READY_POLL);
    }
}

/// 抓取 RTSP 同源 ISP NV12 ring 中指定驱动通道的最新 YUV 帧；仅用于手动快照。
pub fn capture_yuv_snapshot(host: &str, port: u16, channel: u16) -> Result<X5YuvSnapshot, String> {
    let response = request_frame(
        host,
        port,
        7,
        &json!({"cmd":"SNAPSHOT","channel":channel,"mode":"latest"}),
    )?;
    ensure_ok(&response.meta).map_err(|error| {
        tracing::warn!(
            operation = "x5_yuv_snapshot",
            host = %host,
            port,
            channel,
            mode = "latest",
            error = %error,
            "X5 YUV snapshot failed"
        );
        error
    })?;
    parse_yuv_snapshot(response)
}

/// 按 X5 TCP ring 的精确帧号抓取同源原始 NV12；自动快门不得回退到 latest。
pub fn capture_yuv_snapshot_by_frame_id(
    host: &str,
    port: u16,
    channel: u16,
    frame_id: u64,
) -> Result<X5YuvSnapshot, String> {
    let response = request_frame(
        host,
        port,
        9,
        &json!({"cmd":"SNAPSHOT","channel":channel,"mode":"frame_id","frame_id":frame_id}),
    )?;
    ensure_ok(&response.meta).map_err(|error| {
        tracing::warn!(
            operation = "x5_yuv_snapshot",
            host = %host,
            port,
            channel,
            mode = "frame_id",
            request_frame_id = frame_id,
            error = %error,
            "X5 YUV snapshot failed"
        );
        error
    })?;
    let snapshot = parse_yuv_snapshot(response)?;
    if snapshot.frame_id != frame_id {
        return Err(format!(
            "X5 SNAPSHOT frame_id mismatch: requested {frame_id}, got {}",
            snapshot.frame_id
        ));
    }
    Ok(snapshot)
}

/// 按 X5 采集时间戳精确抓取同源原始 NV12；服务端找不到同 timestamp 时必须返回错误。
pub fn capture_yuv_snapshot_by_timestamp_ns(
    host: &str,
    port: u16,
    channel: u16,
    timestamp_ns: u64,
) -> Result<X5YuvSnapshot, String> {
    let response = request_frame(
        host,
        port,
        10,
        &json!({"cmd":"SNAPSHOT","channel":channel,"mode":"timestamp_ns","timestamp_ns":timestamp_ns}),
    )?;
    ensure_ok(&response.meta).map_err(|error| {
        tracing::warn!(
            operation = "x5_yuv_snapshot",
            host = %host,
            port,
            channel,
            mode = "timestamp_ns",
            request_timestamp_ns = timestamp_ns,
            error = %error,
            "X5 YUV snapshot failed"
        );
        error
    })?;
    let snapshot = parse_yuv_snapshot(response)?;
    if snapshot.timestamp_ns != timestamp_ns {
        return Err(format!(
            "X5 SNAPSHOT timestamp_ns mismatch: requested {timestamp_ns}, got {}",
            snapshot.timestamp_ns
        ));
    }
    Ok(snapshot)
}

/// 返回仍需要发送 START_RTSP 的通道；已 requested/running 的通道只等待稳定，不重复触发重启。
pub fn rtsp_channels_requiring_start(
    status: &X5DriverStatus,
    channels: &[u16],
) -> Result<Vec<u16>, String> {
    let channels = normalized_channels(channels);
    if channels.is_empty() {
        return Ok(Vec::new());
    }
    if status.rtsp_channels.is_empty() {
        return if status.camera_running && status.rtsp_started && status.rtsp_tx_enabled {
            Ok(Vec::new())
        } else {
            Ok(channels)
        };
    }
    let mut requiring_start = Vec::new();
    for channel in channels {
        let Some(state) = status
            .rtsp_channels
            .iter()
            .find(|state| state.channel == channel)
        else {
            requiring_start.push(channel);
            continue;
        };
        if !state.runtime_enabled {
            return Err(format!(
                "X5 RTSP CH{} is disabled by the current driver runtime mode",
                state.channel
            ));
        }
        if state.last_error != 0 && !state.busy {
            return Err(format!(
                "X5 RTSP CH{} start failed: {}",
                state.channel, state.last_message
            ));
        }
        if !state.requested_enabled {
            requiring_start.push(channel);
        }
    }
    Ok(requiring_start)
}
/// 抓取 VIN RAW 调试帧；该来源不等同于 RTSP 同源 NV12。
pub fn capture_raw_snapshot(
    host: &str,
    port: u16,
    camera: u16,
    timeout_ms: u32,
) -> Result<X5RawSnapshot, String> {
    let response = request_frame(
        host,
        port,
        8,
        &json!({"cmd":"SNAPSHOT_RAW","camera":camera,"timeout_ms":timeout_ms}),
    )?;
    ensure_ok(&response.meta)?;
    parse_raw_snapshot(response)
}

fn set_rtsp_stream_command(
    host: &str,
    port: u16,
    request: &Value,
) -> Result<X5RtspStreamSummary, String> {
    let response = request_json(host, port, 6, request)?;
    ensure_ok(&response)?;
    Ok(X5RtspStreamSummary {
        channel: response
            .get("channel")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok()),
        affected_channels: value_u16_array(&response, "affected_channels"),
        requested_enabled: required_bool(
            &response,
            "requested_enabled",
            "X5 SET_RTSP_STREAM response",
        )?,
        queued_action: response
            .get("queued_action")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        worker_busy: required_bool(&response, "worker_busy", "X5 SET_RTSP_STREAM response")?,
        action_id: required_u64(&response, "action_id", "X5 SET_RTSP_STREAM response")?,
    })
}

fn request_json(host: &str, port: u16, request_id: u64, request: &Value) -> Result<Value, String> {
    Ok(request_frame(host, port, request_id, request)?.meta)
}

fn request_frame(
    host: &str,
    port: u16,
    request_id: u64,
    request: &Value,
) -> Result<X5ResponseFrame, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("X5 host is required".to_owned());
    }
    let frame = build_request_frame(request_id, request)?;

    let mut stream = TcpStream::connect((host, port))
        .map_err(|error| format!("connect X5 TCP {host}:{port} failed: {error}"))?;
    stream
        .set_read_timeout(Some(X5_TIMEOUT))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(X5_TIMEOUT))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(&frame)
        .map_err(|error| format!("write X5 TCP request failed: {error}"))?;

    let mut header = [0_u8; X5_HEADER_BYTES];
    stream
        .read_exact(&mut header)
        .map_err(|error| format!("read X5 TCP response header failed: {error}"))?;
    parse_response_header(&header, request_id)?;
    let response_type = u16::from_be_bytes([header[6], header[7]]);
    let response_header_len = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    let payload_len = u64::from_be_bytes([
        header[12], header[13], header[14], header[15], header[16], header[17], header[18],
        header[19],
    ]);
    if response_header_len > X5_MAX_JSON_HEADER_BYTES {
        return Err(format!(
            "X5 response JSON is too large: {response_header_len} bytes"
        ));
    }
    if payload_len > X5_MAX_PAYLOAD_BYTES {
        return Err(format!(
            "X5 response payload is too large: {payload_len} bytes"
        ));
    }
    let mut meta = vec![0_u8; response_header_len as usize];
    stream
        .read_exact(&mut meta)
        .map_err(|error| format!("read X5 TCP response JSON failed: {error}"))?;
    let mut payload = vec![0_u8; payload_len as usize];
    if payload_len != 0 {
        stream
            .read_exact(&mut payload)
            .map_err(|error| format!("read X5 TCP response payload failed: {error}"))?;
    }
    let meta: Value = serde_json::from_slice(&meta)
        .map_err(|error| format!("parse X5 TCP response JSON failed: {error}"))?;
    match response_type {
        X5_TYPE_RESPONSE => Ok(X5ResponseFrame { meta, payload }),
        X5_TYPE_ERROR => Err(error_message(&meta)),
        other => Err(format!("X5 response has unexpected type {other}")),
    }
}

fn build_request_frame(request_id: u64, request: &Value) -> Result<Vec<u8>, String> {
    let body = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    let header_len = u32::try_from(body.len()).map_err(|_| "X5 request JSON is too large")?;
    let mut frame = Vec::with_capacity(X5_HEADER_BYTES + body.len());
    frame.extend_from_slice(X5_MAGIC);
    frame.extend_from_slice(&X5_PROTOCOL_VERSION.to_be_bytes());
    frame.extend_from_slice(&X5_TYPE_REQUEST.to_be_bytes());
    frame.extend_from_slice(&header_len.to_be_bytes());
    frame.extend_from_slice(&0_u64.to_be_bytes());
    frame.extend_from_slice(&request_id.to_be_bytes());
    frame.extend_from_slice(&0_u32.to_be_bytes());
    frame.extend_from_slice(&0_u32.to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn parse_response_header(header: &[u8; X5_HEADER_BYTES], request_id: u64) -> Result<(), String> {
    if &header[0..4] != X5_MAGIC {
        return Err("X5 response magic mismatch".to_owned());
    }
    let version = u16::from_be_bytes([header[4], header[5]]);
    if version != X5_PROTOCOL_VERSION {
        return Err(format!("X5 protocol version {version} is not supported"));
    }
    let returned_request_id = u64::from_be_bytes([
        header[20], header[21], header[22], header[23], header[24], header[25], header[26],
        header[27],
    ]);
    if returned_request_id != request_id {
        return Err(format!(
            "X5 response request_id mismatch: expected {request_id}, got {returned_request_id}"
        ));
    }
    Ok(())
}

fn parse_status_response(value: &Value) -> Result<X5DriverStatus, String> {
    let app = value.get("app").unwrap_or(&Value::Null);
    let rtsp_channels = app
        .get("rtsp_channels")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(parse_rtsp_channel_status)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let rings = value
        .get("rings")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(parse_ring_status)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(X5DriverStatus {
        camera_running: value_bool(app, "camera_running").unwrap_or(false),
        rtsp_started: value_bool(app, "rtsp_started").unwrap_or(false),
        rtsp_tx_enabled: value_bool(app, "rtsp_tx_enabled").unwrap_or(false),
        rtsp_requested_enabled: value_bool(app, "rtsp_requested_enabled").unwrap_or(false),
        rtsp_control_busy: value_bool(app, "rtsp_control_busy").unwrap_or(false),
        rtsp_pending_action: value_string(app, "rtsp_pending_action")
            .unwrap_or("none")
            .to_owned(),
        rtsp_last_error: value_i64(app, "rtsp_last_error").unwrap_or(0),
        rtsp_action_id: value_u64(app, "rtsp_action_id").unwrap_or(0),
        rtsp_last_message: value_string(app, "rtsp_last_message")
            .unwrap_or("unknown")
            .to_owned(),
        rtsp_channels,
        rings,
        fps: value_u64(app, "venc_fps").and_then(|value| u16::try_from(value).ok()),
        bitrate_kbps: value_u64(app, "venc_bitrate_kbps")
            .and_then(|value| u32::try_from(value).ok()),
        pipeline_config_version: value_u64(app, "pipeline_config_version"),
    })
}

fn parse_rtsp_channel_status(value: &Value) -> Option<X5RtspChannelStatus> {
    let channel = value_u64(value, "channel").and_then(|value| u16::try_from(value).ok())?;
    Some(X5RtspChannelStatus {
        channel,
        runtime_enabled: value_bool(value, "runtime_enabled").unwrap_or(false),
        requested_enabled: value_bool(value, "requested_enabled").unwrap_or(false),
        started: value_bool(value, "started").unwrap_or(false),
        tx_enabled: value_bool(value, "tx_enabled").unwrap_or(false),
        busy: value_bool(value, "busy").unwrap_or(false),
        pending_action: value_string(value, "pending_action")
            .unwrap_or("none")
            .to_owned(),
        last_error: value_i64(value, "last_error").unwrap_or(0),
        action_id: value_u64(value, "action_id").unwrap_or(0),
        last_message: value_string(value, "last_message")
            .unwrap_or("unknown")
            .to_owned(),
        port: value_u64(value, "port").and_then(|value| u16::try_from(value).ok()),
        path: value_string(value, "path").unwrap_or("/PRR").to_owned(),
    })
}

fn parse_ring_status(value: &Value) -> Option<X5RingStatus> {
    let channel = value_u64(value, "channel").and_then(|value| u16::try_from(value).ok())?;
    let depth = value_u64(value, "depth")
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(0);
    let valid = value_u64(value, "valid")
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(0);
    let write_index = value_u64(value, "write_index")
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(0);
    let last_frame_id = value_u64(value, "last_frame_id").unwrap_or(0);
    let last_timestamp_ns = value_u64(value, "last_timestamp_ns").unwrap_or(0);
    let min_frame_id =
        value_u64(value, "min_frame_id").unwrap_or(if valid > 0 { last_frame_id } else { 0 });
    let max_frame_id = value_u64(value, "max_frame_id").unwrap_or(last_frame_id);
    let min_timestamp_ns = value_u64(value, "min_timestamp_ns").unwrap_or(if valid > 0 {
        last_timestamp_ns
    } else {
        0
    });
    let max_timestamp_ns = value_u64(value, "max_timestamp_ns").unwrap_or(last_timestamp_ns);
    Some(X5RingStatus {
        channel,
        depth,
        valid,
        write_index,
        min_frame_id,
        max_frame_id,
        last_frame_id,
        min_timestamp_ns,
        max_timestamp_ns,
        last_timestamp_ns,
        retention_ns: value_u64(value, "retention_ns")
            .unwrap_or_else(|| max_timestamp_ns.saturating_sub(min_timestamp_ns)),
        dropped: value_u64(value, "dropped").unwrap_or(0),
        evicted: value_u64(value, "evicted").unwrap_or(0),
    })
}

fn parse_yuv_snapshot(response: X5ResponseFrame) -> Result<X5YuvSnapshot, String> {
    if response.meta.get("format").and_then(Value::as_str) != Some("nv12") {
        return Err("X5 SNAPSHOT response format is not nv12".to_owned());
    }
    let payload_len = required_usize(&response.meta, "payload_len", "X5 SNAPSHOT response")?;
    if response.payload.len() != payload_len {
        return Err(format!(
            "X5 SNAPSHOT payload length mismatch: header={payload_len}, actual={}",
            response.payload.len()
        ));
    }
    let y_len = required_usize(&response.meta, "y_len", "X5 SNAPSHOT response")?;
    let uv_len = required_usize(&response.meta, "uv_len", "X5 SNAPSHOT response")?;
    if y_len.saturating_add(uv_len) != payload_len {
        return Err(format!(
            "X5 SNAPSHOT plane length mismatch: y_len={y_len}, uv_len={uv_len}, payload_len={payload_len}"
        ));
    }
    Ok(X5YuvSnapshot {
        channel: required_u16(&response.meta, "channel", "X5 SNAPSHOT response")?,
        width: required_u32(&response.meta, "width", "X5 SNAPSHOT response")?,
        height: required_u32(&response.meta, "height", "X5 SNAPSHOT response")?,
        y_len,
        uv_len,
        frame_id: required_u64(&response.meta, "frame_id", "X5 SNAPSHOT response")?,
        timestamp_ns: required_u64(&response.meta, "timestamp_ns", "X5 SNAPSHOT response")?,
        payload: response.payload,
    })
}

fn parse_raw_snapshot(response: X5ResponseFrame) -> Result<X5RawSnapshot, String> {
    if response.meta.get("format").and_then(Value::as_str) != Some("raw_binary") {
        return Err("X5 SNAPSHOT_RAW response format is not raw_binary".to_owned());
    }
    let payload_len = required_usize(&response.meta, "payload_len", "X5 SNAPSHOT_RAW response")?;
    if response.payload.len() != payload_len {
        return Err(format!(
            "X5 SNAPSHOT_RAW payload length mismatch: header={payload_len}, actual={}",
            response.payload.len()
        ));
    }
    Ok(X5RawSnapshot {
        camera: required_u16(&response.meta, "camera", "X5 SNAPSHOT_RAW response")?,
        width: required_u32(&response.meta, "width", "X5 SNAPSHOT_RAW response")?,
        height: required_u32(&response.meta, "height", "X5 SNAPSHOT_RAW response")?,
        stride: required_usize(&response.meta, "stride", "X5 SNAPSHOT_RAW response")?,
        format_code: required_u32(&response.meta, "format_code", "X5 SNAPSHOT_RAW response")?,
        frame_id: required_u64(&response.meta, "frame_id", "X5 SNAPSHOT_RAW response")?,
        timestamp_ns: required_u64(&response.meta, "timestamp_ns", "X5 SNAPSHOT_RAW response")?,
        payload: response.payload,
    })
}

fn ensure_ok(value: &Value) -> Result<(), String> {
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(error_message(value))
    }
}

fn value_bool(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

fn value_i64(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn value_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn value_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn value_u16_array(value: &Value, key: &str) -> Vec<u16> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_u64)
                .filter_map(|value| u16::try_from(value).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn normalized_channels(channels: &[u16]) -> Vec<u16> {
    let mut normalized = Vec::new();
    for channel in channels {
        if !normalized.contains(channel) {
            normalized.push(*channel);
        }
    }
    normalized
}

fn format_channel_list(channels: &[u16]) -> String {
    channels
        .iter()
        .map(|channel| format!("CH{channel}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn rtsp_readiness_pending(
    status: &X5DriverStatus,
    channels: &[u16],
) -> Result<Vec<String>, String> {
    let mut pending = Vec::new();
    if !status.camera_running {
        pending.push("camera_running=false".to_owned());
    }
    for channel in channels {
        let Some(state) = status
            .rtsp_channels
            .iter()
            .find(|state| state.channel == *channel)
        else {
            pending.push(format!("CH{channel}: missing status"));
            continue;
        };
        if !state.runtime_enabled {
            return Err(format!(
                "X5 RTSP CH{} is disabled by the current driver runtime mode",
                state.channel
            ));
        }
        if state.last_error != 0 && !state.busy {
            return Err(format!(
                "X5 RTSP CH{} start failed: {}",
                state.channel, state.last_message
            ));
        }
        if state.busy || state.pending_action != "none" {
            pending.push(format!(
                "CH{} action pending={} busy={} requested={} last={}",
                state.channel,
                state.pending_action,
                state.busy,
                state.requested_enabled,
                state.last_message
            ));
        }
        if !(state.started && state.tx_enabled) {
            pending.push(format!(
                "CH{} started={} tx={} requested={} busy={} pending={} last={}",
                state.channel,
                state.started,
                state.tx_enabled,
                state.requested_enabled,
                state.busy,
                state.pending_action,
                state.last_message
            ));
        }
        if !status.rings.is_empty() {
            match status.rings.iter().find(|ring| ring.channel == *channel) {
                Some(ring) if ring.valid > 0 => {}
                Some(ring) => pending.push(format!(
                    "CH{} ring valid=0 last_frame_id={} dropped={}",
                    ring.channel, ring.last_frame_id, ring.dropped
                )),
                None => pending.push(format!("CH{channel}: missing ring status")),
            }
        }
    }
    Ok(pending)
}

fn required_bool(value: &Value, key: &str, context: &str) -> Result<bool, String> {
    value_bool(value, key).ok_or_else(|| format!("{context} missing {key}"))
}

fn required_u64(value: &Value, key: &str, context: &str) -> Result<u64, String> {
    value_u64(value, key).ok_or_else(|| format!("{context} missing {key}"))
}

fn required_u32(value: &Value, key: &str, context: &str) -> Result<u32, String> {
    required_u64(value, key, context).and_then(|value| {
        u32::try_from(value).map_err(|_| format!("{context} {key} does not fit u32"))
    })
}

fn required_u16(value: &Value, key: &str, context: &str) -> Result<u16, String> {
    required_u64(value, key, context).and_then(|value| {
        u16::try_from(value).map_err(|_| format!("{context} {key} does not fit u16"))
    })
}

fn required_usize(value: &Value, key: &str, context: &str) -> Result<usize, String> {
    required_u64(value, key, context).and_then(|value| {
        usize::try_from(value).map_err(|_| format!("{context} {key} does not fit usize"))
    })
}

fn error_message(value: &Value) -> String {
    let code = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("x5_error");
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("X5 TCP request failed");
    let mut details = Vec::new();

    if let Some(channel) = value_u64(value, "request_channel") {
        details.push(format!("request_channel=CH{channel}"));
    }
    if let Some(mode) = value.get("request_mode").and_then(Value::as_str) {
        match mode {
            "frame_id" => {
                if let Some(frame_id) = value_u64(value, "request_frame_id") {
                    details.push(format!("request=frame_id:{frame_id}"));
                }
            }
            "timestamp" | "timestamp_ns" => {
                if let Some(timestamp_ns) = value_u64(value, "request_timestamp_ns") {
                    details.push(format!("request=timestamp_ns:{timestamp_ns}"));
                }
            }
            other => details.push(format!("request_mode={other}")),
        }
    }
    if let (Some(valid), Some(depth)) = (
        value_u64(value, "ring_valid"),
        value_u64(value, "ring_depth"),
    ) {
        details.push(format!("ring_valid={valid}/{depth}"));
    }
    if let (Some(min), Some(max)) = (
        value_u64(value, "ring_min_frame_id"),
        value_u64(value, "ring_max_frame_id"),
    ) {
        details.push(format!("ring_frame_id={min}..{max}"));
    }
    if let (Some(min), Some(max)) = (
        value_u64(value, "ring_min_timestamp_ns"),
        value_u64(value, "ring_max_timestamp_ns"),
    ) {
        details.push(format!("ring_timestamp_ns={min}..{max}"));
    }
    if let Some(retention_ns) = value_u64(value, "ring_retention_ns") {
        details.push(format!("ring_retention_ns={retention_ns}"));
    }
    if let Some(evicted) = value_u64(value, "ring_evicted") {
        details.push(format!("ring_evicted={evicted}"));
    }
    if let Some(dropped) = value_u64(value, "ring_dropped") {
        details.push(format!("ring_dropped={dropped}"));
    }

    let summary = format!("{code}: {message}");
    if details.is_empty() {
        summary
    } else {
        format!("{summary} ({})", details.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_header(request_id: u64) -> [u8; X5_HEADER_BYTES] {
        let mut header = [0_u8; X5_HEADER_BYTES];
        header[0..4].copy_from_slice(X5_MAGIC);
        header[4..6].copy_from_slice(&X5_PROTOCOL_VERSION.to_be_bytes());
        header[6..8].copy_from_slice(&X5_TYPE_RESPONSE.to_be_bytes());
        header[8..12].copy_from_slice(&2_u32.to_be_bytes());
        header[20..28].copy_from_slice(&request_id.to_be_bytes());
        header
    }

    #[test]
    fn x5_request_frame_uses_big_endian_header() {
        let frame = build_request_frame(0x0102_0304_0506_0708, &json!({"cmd":"HELLO"}))
            .expect("request frame builds");

        assert_eq!(&frame[0..4], X5_MAGIC);
        assert_eq!(
            u16::from_be_bytes([frame[4], frame[5]]),
            X5_PROTOCOL_VERSION
        );
        assert_eq!(u16::from_be_bytes([frame[6], frame[7]]), X5_TYPE_REQUEST);
        assert_eq!(u64::from_be_bytes(frame[12..20].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_be_bytes(frame[20..28].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
        assert_eq!(u32::from_be_bytes(frame[28..32].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(frame[32..36].try_into().unwrap()), 0);
        let header_len = u32::from_be_bytes(frame[8..12].try_into().unwrap()) as usize;
        assert_eq!(header_len, frame.len() - X5_HEADER_BYTES);
    }

    #[test]
    fn x5_response_header_validates_magic_version_and_request_id() {
        let mut header = response_header(7);
        assert_eq!(parse_response_header(&header, 7), Ok(()));

        header[0] = b'Y';
        assert_eq!(
            parse_response_header(&header, 7),
            Err("X5 response magic mismatch".to_owned())
        );

        header = response_header(7);
        header[5] = 2;
        assert_eq!(
            parse_response_header(&header, 7),
            Err("X5 protocol version 2 is not supported".to_owned())
        );

        header = response_header(8);
        assert_eq!(
            parse_response_header(&header, 7),
            Err("X5 response request_id mismatch: expected 7, got 8".to_owned())
        );
    }

    #[test]
    fn x5_error_message_prefers_server_fields() {
        let value = json!({"ok":false,"error":"bad_request","message":"fps valid range is 1..240"});

        assert_eq!(
            ensure_ok(&value),
            Err("bad_request: fps valid range is 1..240".to_owned())
        );
    }

    #[test]
    fn x5_error_message_includes_ring_diagnostics() {
        let value = json!({
            "ok": false,
            "error": "frame_not_found",
            "message": "requested frame is not in ring buffer",
            "request_channel": 3,
            "request_mode": "timestamp_ns",
            "request_timestamp_ns": 1_700,
            "ring_depth": 24,
            "ring_valid": 24,
            "ring_min_frame_id": 41,
            "ring_max_frame_id": 64,
            "ring_min_timestamp_ns": 1_000,
            "ring_max_timestamp_ns": 1_600,
            "ring_retention_ns": 600,
            "ring_evicted": 9,
            "ring_dropped": 0,
        });

        let message = error_message(&value);

        assert!(message.contains("request=timestamp_ns:1700"));
        assert!(message.contains("ring_timestamp_ns=1000..1600"));
        assert!(message.contains("ring_valid=24/24"));
        assert!(message.contains("ring_evicted=9"));
    }
    #[test]
    fn x5_status_parses_rtsp_worker_fields() {
        let value = json!({
            "ok": true,
            "cmd": "GET_STATUS",
            "app": {
                "camera_running": true,
                "rtsp_started": false,
                "rtsp_tx_enabled": false,
                "rtsp_requested_enabled": true,
                "rtsp_control_busy": true,
                "rtsp_pending_action": "start",
                "rtsp_last_error": 0,
                "rtsp_action_id": 9,
                "rtsp_last_message": "per-channel",
                "rtsp_channels": [
                    {
                        "channel": 3,
                        "runtime_enabled": true,
                        "requested_enabled": true,
                        "started": false,
                        "tx_enabled": false,
                        "busy": true,
                        "pending_action": "start",
                        "last_error": 0,
                        "action_id": 4,
                        "last_message": "start_queued action_id=4",
                        "port": 557,
                        "path": "/PRR"
                    }
                ],
                "venc_fps": 60,
                "venc_bitrate_kbps": 12000,
                "pipeline_config_version": 7
            },
            "rings": [
                {
                    "channel": 3,
                    "depth": 24,
                    "valid": 0,
                    "write_index": 0,
                    "last_frame_id": 0,
                    "last_timestamp_ns": 0,
                    "min_frame_id": 0,
                    "max_frame_id": 0,
                    "min_timestamp_ns": 0,
                    "max_timestamp_ns": 0,
                    "retention_ns": 0,
                    "dropped": 0,
                    "evicted": 0
                }
            ]
        });

        let status = parse_status_response(&value).unwrap();

        assert!(status.camera_running);
        assert!(!status.rtsp_started);
        assert!(status.rtsp_requested_enabled);
        assert!(status.rtsp_control_busy);
        assert_eq!(status.rtsp_pending_action, "start");
        assert_eq!(status.rtsp_action_id, 9);
        assert_eq!(status.fps, Some(60));
        assert_eq!(status.bitrate_kbps, Some(12_000));
        assert_eq!(status.pipeline_config_version, Some(7));
        assert_eq!(status.rtsp_channels.len(), 1);
        assert_eq!(status.rtsp_channels[0].channel, 3);
        assert!(status.rtsp_channels[0].runtime_enabled);
        assert!(status.rtsp_channels[0].requested_enabled);
        assert!(status.rtsp_channels[0].busy);
        assert_eq!(status.rtsp_channels[0].port, Some(557));
        assert_eq!(status.rings.len(), 1);
        assert_eq!(status.rings[0].channel, 3);
        assert_eq!(status.rings[0].valid, 0);
        assert_eq!(
            rtsp_readiness_pending(&status, &[3]).unwrap(),
            vec![
                "CH3 action pending=start busy=true requested=true last=start_queued action_id=4"
                    .to_owned(),
                "CH3 started=false tx=false requested=true busy=true pending=start last=start_queued action_id=4"
                    .to_owned(),
                "CH3 ring valid=0 last_frame_id=0 dropped=0".to_owned()
            ]
        );

        let mut ready_status = status.clone();
        ready_status.rtsp_channels[0].started = true;
        ready_status.rtsp_channels[0].tx_enabled = true;
        ready_status.rtsp_channels[0].pending_action = "none".to_owned();
        ready_status.rtsp_channels[0].last_message = "start_done action_id=4".to_owned();
        ready_status.rtsp_channels[0].busy = false;
        ready_status.rings[0].valid = 8;
        ready_status.rings[0].last_frame_id = 42;
        assert!(
            rtsp_readiness_pending(&ready_status, &[3])
                .unwrap()
                .is_empty()
        );
        assert!(
            rtsp_channels_requiring_start(&ready_status, &[3])
                .unwrap()
                .is_empty()
        );

        let mut stopped_status = ready_status.clone();
        stopped_status.rtsp_channels[0].requested_enabled = false;
        stopped_status.rtsp_channels[0].started = false;
        stopped_status.rtsp_channels[0].tx_enabled = false;
        stopped_status.rings[0].valid = 0;
        assert_eq!(
            rtsp_channels_requiring_start(&stopped_status, &[3]).unwrap(),
            vec![3]
        );

        let mut queued_status = stopped_status.clone();
        queued_status.rtsp_channels[0].requested_enabled = true;
        queued_status.rtsp_channels[0].busy = true;
        queued_status.rtsp_channels[0].pending_action = "start".to_owned();
        assert!(
            rtsp_channels_requiring_start(&queued_status, &[3])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn x5_yuv_snapshot_validates_payload_metadata() {
        let response = X5ResponseFrame {
            meta: json!({
                "ok": true,
                "cmd": "SNAPSHOT",
                "channel": 3,
                "format": "nv12",
                "width": 2,
                "height": 2,
                "y_len": 4,
                "uv_len": 2,
                "payload_len": 6,
                "frame_id": 11,
                "timestamp_ns": 22,
            }),
            payload: vec![16, 16, 16, 16, 128, 128],
        };

        let snapshot = parse_yuv_snapshot(response).unwrap();

        assert_eq!(snapshot.channel, 3);
        assert_eq!(snapshot.width, 2);
        assert_eq!(snapshot.height, 2);
        assert_eq!(snapshot.y_len, 4);
        assert_eq!(snapshot.uv_len, 2);
        assert_eq!(snapshot.payload.len(), 6);
    }

    #[test]
    fn x5_raw_snapshot_validates_payload_metadata() {
        let response = X5ResponseFrame {
            meta: json!({
                "ok": true,
                "cmd": "SNAPSHOT_RAW",
                "source": "vin",
                "format": "raw_binary",
                "camera": 1,
                "width": 2,
                "height": 2,
                "stride": 4,
                "format_code": 24,
                "frame_id": 31,
                "timestamp_ns": 42,
                "payload_len": 8
            }),
            payload: vec![0; 8],
        };

        let snapshot = parse_raw_snapshot(response).unwrap();

        assert_eq!(snapshot.camera, 1);
        assert_eq!(snapshot.stride, 4);
        assert_eq!(snapshot.format_code, 24);
        assert_eq!(snapshot.payload.len(), 8);
    }
}
