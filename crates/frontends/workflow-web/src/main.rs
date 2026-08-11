mod workflow;

use std::{
    collections::HashMap,
    fs,
    net::IpAddr,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, Query, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use camera_toolbox_adapters::media::{
    FfmpegRtspDecoder, FfmpegRtspTransport, ffmpeg_rtsp::FfmpegRtspDecoderStatsSnapshot,
};
use camera_toolbox_app::{
    DecodedVideoFrame, LatestDecodedFrameSlot, RtspLatencyMode, StreamCancellation,
    StreamSessionId, host_monotonic_time_ns,
};
use clap::Parser;
use image::{ColorType, codecs::jpeg::JpegEncoder};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpListener,
    time::{Instant, sleep, sleep_until},
};
use tower_http::services::{ServeDir, ServeFile};
use workflow::{
    RuntimeGraphStatus, WorkflowGraph, node_catalog, normalize_workflow, runtime_graph_status,
    seed_workflow_graph, validate_workflow, workmode_templates,
};

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

    /// 工作流文件目录；保存为 .ctworkflow.json，运行时字段不会写入。
    #[arg(long)]
    workflow_dir: Option<PathBuf>,
}

#[derive(Clone)]
struct AppState {
    workflow_store: Arc<WorkflowStore>,
    runtime_sessions: Arc<Mutex<HashMap<String, RuntimeGraphSession>>>,
}

/// 运行时会话只存于服务进程内存；其图副本用于 Stop 后生成节点级诊断。
struct RuntimeGraphSession {
    graph: WorkflowGraph,
    status: RuntimeGraphStatus,
}

struct WorkflowStore {
    dir: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    service: &'static str,
    status: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowSummary {
    id: String,
    title: String,
    revision: String,
    node_count: usize,
    edge_count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValidationResponse {
    ok: bool,
    error: Option<String>,
}

/// RuntimeGraph API 失败响应统一为 JSON，便于前端直接显示明确的诊断。
#[derive(Debug, Serialize)]
struct RuntimeApiError {
    error: String,
}

type RuntimeApiResult<T> = std::result::Result<Json<T>, (StatusCode, Json<RuntimeApiError>)>;

fn runtime_api_error(
    status: StatusCode,
    error: impl Into<String>,
) -> (StatusCode, Json<RuntimeApiError>) {
    (
        status,
        Json(RuntimeApiError {
            error: error.into(),
        }),
    )
}

/// I²C 预览请求仅描述一次可能的传输；它从不打开设备或建立远程会话。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct I2cPreviewRequest {
    node_id: String,
    profile_id: String,
    bus: String,
    address: u8,
    register: u16,
    payload: Vec<u8>,
    page_size: usize,
    operation: I2cPreviewOperation,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum I2cPreviewOperation {
    Read,
    Write,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EepromPreviewRequest {
    node_id: String,
    profile_id: String,
    bus: String,
    address: u8,
    register: u16,
    payload: Vec<u8>,
    page_size: usize,
    map_id: String,
    verify_after_write: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlPreview {
    target: ControlTarget,
    operation: &'static str,
    page_split_estimate: PageSplitEstimate,
    requires_confirmation: bool,
    execution: &'static str,
    map_id: Option<String>,
    verify_after_write: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlTarget {
    node_id: String,
    profile_id: String,
    bus: String,
    address: u8,
    register: u16,
    payload: Vec<u8>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PageSplitEstimate {
    page_size: usize,
    write_count: usize,
    segments: Vec<PageSplitSegment>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PageSplitSegment {
    register: u16,
    payload_length: usize,
}

#[derive(Debug, Serialize)]
struct PreviewApiError {
    error: String,
}

impl PreviewApiError {
    fn bad_request(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
        }
    }
}

impl IntoResponse for PreviewApiError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, Json(self)).into_response()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MjpegStreamQuery {
    url: String,
    fps: Option<u16>,
    width: Option<u16>,
    height: Option<u16>,
}

struct MjpegStreamConfig {
    url: String,
    fps_limit: Option<u16>,
    width: u16,
    height: u16,
}

/// 本地图像预览只接受相对路径，并在解析符号链接后仍限定于声明的 workspace 根目录。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalImageQuery {
    workspace_root: String,
    relative_path: String,
}

#[derive(Debug, Serialize)]
struct LocalImageApiError {
    error: String,
}

impl LocalImageApiError {
    fn new(status: StatusCode, error: impl Into<String>) -> (StatusCode, Json<Self>) {
        (
            status,
            Json(Self {
                error: error.into(),
            }),
        )
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let _logging = camera_toolbox_logging::init();
    let args = ServerArgs::parse();
    let static_dir = args.static_dir.unwrap_or_else(default_static_dir);
    let workflow_dir = args.workflow_dir.unwrap_or_else(default_workflow_dir);
    ensure_static_dir(&static_dir)?;
    fs::create_dir_all(&workflow_dir)
        .with_context(|| format!("failed to create workflow dir {}", workflow_dir.display()))?;

    let listener = TcpListener::bind((args.host, args.port))
        .await
        .with_context(|| format!("failed to bind {}:{}", args.host, args.port))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read listener address")?;
    let router = app_router(static_dir.clone(), workflow_dir.clone());

    println!("Camera Toolbox Workflow Web listening on http://{local_addr}");
    println!("Serving frontend assets from {}", static_dir.display());
    println!("Saving workflows under {}", workflow_dir.display());
    tracing::info!(operation = "workflow_web_start", address = %local_addr, static_dir = %static_dir.display(), workflow_dir = %workflow_dir.display());

    axum::serve(listener, router)
        .await
        .context("workflow web server stopped unexpectedly")
}

fn app_router(static_dir: PathBuf, workflow_dir: PathBuf) -> Router {
    let index = static_dir.join("index.html");
    let frontend = ServeDir::new(static_dir).not_found_service(ServeFile::new(index));
    let state = AppState {
        workflow_store: Arc::new(WorkflowStore { dir: workflow_dir }),
        runtime_sessions: Arc::new(Mutex::new(HashMap::new())),
    };

    Router::new()
        .route("/api/health", get(health))
        .route("/api/workflow", get(workflow_graph))
        .route("/api/node-catalog", get(node_catalog_api))
        .route("/api/workmode-templates", get(workmode_templates_api))
        .route("/api/workflows", get(list_workflows).post(create_workflow))
        .route("/api/workflows/{id}", get(get_workflow).put(put_workflow))
        .route("/api/workflows/{id}/validate", post(validate_workflow_api))
        .route("/api/workflows/{id}/runtime", get(get_workflow_runtime))
        .route(
            "/api/workflows/{id}/runtime/run",
            post(run_workflow_runtime),
        )
        .route(
            "/api/workflows/{id}/runtime/stop",
            post(stop_workflow_runtime),
        )
        .route("/api/control/i2c/preview", post(preview_i2c_transfer))
        .route(
            "/api/control/eeprom/preview",
            post(preview_eeprom_provision),
        )
        .route("/api/streams/mjpeg", get(mjpeg_stream))
        .route("/api/images/local", get(local_image_preview))
        .fallback_service(frontend)
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        service: "camera-toolbox-workflow-web",
        status: "ok",
    })
}

async fn workflow_graph() -> Json<WorkflowGraph> {
    let graph = seed_workflow_graph();
    debug_assert!(validate_workflow(&graph).is_ok());
    Json(graph)
}

async fn node_catalog_api() -> Json<Vec<workflow::NodeDefinition>> {
    Json(node_catalog())
}

async fn workmode_templates_api() -> Json<Vec<workflow::WorkmodeTemplate>> {
    Json(workmode_templates())
}

async fn list_workflows(
    State(state): State<AppState>,
) -> std::result::Result<Json<Vec<WorkflowSummary>>, (StatusCode, String)> {
    state.workflow_store.list().map(Json)
}

async fn create_workflow(
    State(state): State<AppState>,
    Json(mut graph): Json<WorkflowGraph>,
) -> std::result::Result<(StatusCode, Json<WorkflowGraph>), (StatusCode, String)> {
    if graph.id.trim().is_empty() {
        graph.id = format!("workflow-{}", next_revision());
    }
    let revision = next_revision();
    let graph = normalize_workflow(graph, revision).map_err(bad_request)?;
    state.workflow_store.save(&graph)?;
    Ok((StatusCode::CREATED, Json(graph)))
}

async fn get_workflow(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> std::result::Result<Json<WorkflowGraph>, (StatusCode, String)> {
    state.workflow_store.load(&id).map(Json)
}

async fn put_workflow(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(mut graph): Json<WorkflowGraph>,
) -> std::result::Result<Json<WorkflowGraph>, (StatusCode, String)> {
    graph.id = id.clone();
    if let Some(current) = state.workflow_store.load_optional(&id)? {
        if_match_revision(&headers, &current.revision)?;
    }
    let graph = normalize_workflow(graph, next_revision()).map_err(bad_request)?;
    state.workflow_store.save(&graph)?;
    Ok(Json(graph))
}

async fn validate_workflow_api(
    Json(graph): Json<WorkflowGraph>,
) -> (StatusCode, Json<ValidationResponse>) {
    match validate_workflow(&graph) {
        Ok(()) => (
            StatusCode::OK,
            Json(ValidationResponse {
                ok: true,
                error: None,
            }),
        ),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(ValidationResponse {
                ok: false,
                error: Some(error),
            }),
        ),
    }
}

/// 启动纯内存的 Stage 7 诊断会话；不会连接 RTSP/SSH/X5/I²C，也不会写 EEPROM。
async fn run_workflow_runtime(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    request: std::result::Result<Json<WorkflowGraph>, JsonRejection>,
) -> RuntimeApiResult<RuntimeGraphStatus> {
    let Json(graph) = request.map_err(|error| {
        runtime_api_error(
            StatusCode::BAD_REQUEST,
            format!("invalid runtime graph request: {error}"),
        )
    })?;
    if graph.id != id {
        return Err(runtime_api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "workflow ID in path `{id}` does not match request graph `{}`",
                graph.id
            ),
        ));
    }
    validate_workflow(&graph).map_err(|error| runtime_api_error(StatusCode::BAD_REQUEST, error))?;

    let status = runtime_graph_status(&graph, true);
    let mut sessions = state.runtime_sessions.lock().map_err(|_| {
        runtime_api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "runtime session state is unavailable",
        )
    })?;
    sessions.insert(
        id,
        RuntimeGraphSession {
            graph,
            status: status.clone(),
        },
    );
    Ok(Json(status))
}

/// 获取指定工作流的进程内诊断快照。工作流文件及其 revision 不会被读取或修改。
async fn get_workflow_runtime(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> RuntimeApiResult<RuntimeGraphStatus> {
    let sessions = state.runtime_sessions.lock().map_err(|_| {
        runtime_api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "runtime session state is unavailable",
        )
    })?;
    let status = sessions
        .get(&id)
        .map(|session| session.status.clone())
        .ok_or_else(|| {
            runtime_api_error(
                StatusCode::NOT_FOUND,
                format!("no runtime session exists for workflow `{id}`"),
            )
        })?;
    Ok(Json(status))
}

/// 停止运行时标记并保留节点级 idle 诊断；不会执行外部停止命令。
async fn stop_workflow_runtime(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> RuntimeApiResult<RuntimeGraphStatus> {
    let mut sessions = state.runtime_sessions.lock().map_err(|_| {
        runtime_api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "runtime session state is unavailable",
        )
    })?;
    let session = sessions.get_mut(&id).ok_or_else(|| {
        runtime_api_error(
            StatusCode::NOT_FOUND,
            format!("no runtime session exists for workflow `{id}`"),
        )
    })?;
    let status = runtime_graph_status(&session.graph, false);
    session.status = status.clone();
    Ok(Json(status))
}

async fn local_image_preview(
    Query(query): Query<LocalImageQuery>,
) -> std::result::Result<Response, (StatusCode, Json<LocalImageApiError>)> {
    let workspace_root = canonical_workspace_root(&query.workspace_root)?;
    let relative_path = validate_relative_image_path(&query.relative_path)?;
    let image_path = workspace_root.join(relative_path);
    let image_path = fs::canonicalize(&image_path).map_err(|error| {
        LocalImageApiError::new(
            StatusCode::NOT_FOUND,
            format!("image file could not be resolved: {error}"),
        )
    })?;
    if !image_path.starts_with(&workspace_root) {
        return Err(LocalImageApiError::new(
            StatusCode::FORBIDDEN,
            "image path resolves outside the configured workspace root",
        ));
    }
    if !image_path.is_file() {
        return Err(LocalImageApiError::new(
            StatusCode::BAD_REQUEST,
            "image path must resolve to a regular file",
        ));
    }
    let content_type = local_image_content_type(&image_path)?;
    let bytes = fs::read(&image_path).map_err(|error| {
        LocalImageApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read image file: {error}"),
        )
    })?;
    let mut response = Body::from(bytes).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn canonical_workspace_root(
    workspace_root: &str,
) -> std::result::Result<PathBuf, (StatusCode, Json<LocalImageApiError>)> {
    let workspace_root = workspace_root.trim();
    if workspace_root.is_empty() {
        return Err(LocalImageApiError::new(
            StatusCode::BAD_REQUEST,
            "workspaceRoot must not be empty",
        ));
    }
    let workspace_root = fs::canonicalize(workspace_root).map_err(|error| {
        LocalImageApiError::new(
            StatusCode::BAD_REQUEST,
            format!("workspaceRoot could not be resolved: {error}"),
        )
    })?;
    if !workspace_root.is_dir() {
        return Err(LocalImageApiError::new(
            StatusCode::BAD_REQUEST,
            "workspaceRoot must resolve to a directory",
        ));
    }
    Ok(workspace_root)
}

fn validate_relative_image_path(
    relative_path: &str,
) -> std::result::Result<&Path, (StatusCode, Json<LocalImageApiError>)> {
    let relative_path = relative_path.trim();
    if relative_path.is_empty() {
        return Err(LocalImageApiError::new(
            StatusCode::BAD_REQUEST,
            "relativePath must not be empty",
        ));
    }
    let path = Path::new(relative_path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(LocalImageApiError::new(
            StatusCode::BAD_REQUEST,
            "relativePath must stay below workspaceRoot without `..` or an absolute prefix",
        ));
    }
    Ok(path)
}

fn local_image_content_type(
    image_path: &Path,
) -> std::result::Result<&'static str, (StatusCode, Json<LocalImageApiError>)> {
    let extension = image_path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        "png" => Ok("image/png"),
        "jpg" | "jpeg" => Ok("image/jpeg"),
        "gif" => Ok("image/gif"),
        "webp" => Ok("image/webp"),
        _ => Err(LocalImageApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "only PNG, JPEG, GIF, and WebP image files can be previewed",
        )),
    }
}

async fn preview_i2c_transfer(
    request: std::result::Result<Json<I2cPreviewRequest>, JsonRejection>,
) -> std::result::Result<Json<ControlPreview>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid I²C preview request: {error}"))
    })?;
    let target = ControlTarget {
        node_id: request.node_id,
        profile_id: request.profile_id,
        bus: request.bus,
        address: request.address,
        register: request.register,
        payload: request.payload,
    };
    validate_preview_target(&target)?;
    let is_write = matches!(request.operation, I2cPreviewOperation::Write);
    if is_write && target.payload.is_empty() {
        return Err(PreviewApiError::bad_request(
            "I²C write preview requires at least one payload byte",
        ));
    }
    if !is_write && !target.payload.is_empty() {
        return Err(PreviewApiError::bad_request(
            "I²C read preview must not include a write payload",
        ));
    }
    let page_split_estimate =
        page_split_estimate(target.register, target.payload.len(), request.page_size)?;
    Ok(Json(ControlPreview {
        target,
        operation: if is_write { "write" } else { "read" },
        page_split_estimate,
        requires_confirmation: is_write,
        execution: "preview-only",
        map_id: None,
        verify_after_write: None,
    }))
}

async fn preview_eeprom_provision(
    request: std::result::Result<Json<EepromPreviewRequest>, JsonRejection>,
) -> std::result::Result<Json<ControlPreview>, PreviewApiError> {
    let Json(request) = request.map_err(|error| {
        PreviewApiError::bad_request(format!("invalid EEPROM preview request: {error}"))
    })?;
    if request.map_id.trim().is_empty() {
        return Err(PreviewApiError::bad_request(
            "EEPROM mapId must not be empty",
        ));
    }
    let target = ControlTarget {
        node_id: request.node_id,
        profile_id: request.profile_id,
        bus: request.bus,
        address: request.address,
        register: request.register,
        payload: request.payload,
    };
    validate_preview_target(&target)?;
    if target.payload.is_empty() {
        return Err(PreviewApiError::bad_request(
            "EEPROM provision preview requires at least one payload byte",
        ));
    }
    let page_split_estimate =
        page_split_estimate(target.register, target.payload.len(), request.page_size)?;
    Ok(Json(ControlPreview {
        target,
        operation: "provision",
        page_split_estimate,
        requires_confirmation: true,
        execution: "preview-only",
        map_id: Some(request.map_id),
        verify_after_write: Some(request.verify_after_write),
    }))
}

fn validate_preview_target(target: &ControlTarget) -> std::result::Result<(), PreviewApiError> {
    if target.node_id.trim().is_empty() {
        return Err(PreviewApiError::bad_request("nodeId must not be empty"));
    }
    if target.profile_id.trim().is_empty() {
        return Err(PreviewApiError::bad_request("profileId must not be empty"));
    }
    if target.bus.trim().is_empty() || target.bus.chars().any(char::is_control) {
        return Err(PreviewApiError::bad_request(
            "bus must be a non-empty printable identifier",
        ));
    }
    if !(0x03..=0x77).contains(&target.address) {
        return Err(PreviewApiError::bad_request(
            "address must be a 7-bit I²C address in 0x03..=0x77",
        ));
    }
    if target.payload.len() > 4096 {
        return Err(PreviewApiError::bad_request(
            "payload exceeds the 4096-byte preview limit",
        ));
    }
    Ok(())
}

fn page_split_estimate(
    register: u16,
    payload_length: usize,
    page_size: usize,
) -> std::result::Result<PageSplitEstimate, PreviewApiError> {
    if !(1..=256).contains(&page_size) {
        return Err(PreviewApiError::bad_request(
            "pageSize must be in 1..=256 bytes",
        ));
    }
    let mut segments = Vec::new();
    let mut next_register = usize::from(register);
    let end_register = next_register
        .checked_add(payload_length)
        .ok_or_else(|| PreviewApiError::bad_request("payload register range overflows"))?;
    if end_register > usize::from(u16::MAX) + 1 {
        return Err(PreviewApiError::bad_request(
            "payload exceeds the 16-bit register range",
        ));
    }
    while next_register < end_register {
        let page_remaining = page_size - (next_register % page_size);
        let payload_length = page_remaining.min(end_register - next_register);
        segments.push(PageSplitSegment {
            register: next_register as u16,
            payload_length,
        });
        next_register += payload_length;
    }
    Ok(PageSplitEstimate {
        page_size,
        write_count: segments.len(),
        segments,
    })
}

fn if_match_revision(
    headers: &HeaderMap,
    current_revision: &str,
) -> std::result::Result<(), (StatusCode, String)> {
    let Some(raw) = headers.get(header::IF_MATCH) else {
        return Ok(());
    };
    let expected = raw
        .to_str()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "invalid If-Match header".to_owned(),
            )
        })?
        .trim_matches('"');
    if expected != current_revision {
        return Err((
            StatusCode::CONFLICT,
            format!("workflow revision conflict: current `{current_revision}`, got `{expected}`"),
        ));
    }
    Ok(())
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

    let frame_interval = config
        .fps_limit
        .map(|fps| Duration::from_secs_f64(1.0 / f64::from(fps)));
    let body_stream = async_stream::stream! {
        let _decoder = decoder;
        let _cancellation = cancellation;
        let mut last_sequence = None;
        let mut next_frame_at = Instant::now();
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
                if let Some(interval) = frame_interval {
                    let now = Instant::now();
                    if now < next_frame_at {
                        sleep_until(next_frame_at).await;
                        continue;
                    }
                    next_frame_at += interval;
                    let now = Instant::now();
                    while next_frame_at <= now {
                        next_frame_at += interval;
                    }
                }
                last_sequence = Some(frame.identity.frame_sequence);
                let stats = _decoder.stats().snapshot();
                match mjpeg_chunk(&frame, &stats) {
                    Ok(chunk) => yield Ok::<Bytes, std::io::Error>(Bytes::from(chunk)),
                    Err(error) => yield Err(std::io::Error::other(error)),
                }
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

fn mjpeg_chunk(
    frame: &DecodedVideoFrame,
    stats: &FfmpegRtspDecoderStatsSnapshot,
) -> Result<Vec<u8>, String> {
    let encode_start = Instant::now();
    let jpeg = encode_rgba_as_jpeg(frame)?;
    let encode_ns = duration_nanos(encode_start.elapsed());
    let sent_at_ns = host_monotonic_time_ns();
    let headers = format!(
        "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nX-Frame-Sequence: {}\r\nX-Frame-Published-At-Ns: {}\r\nX-Mjpeg-Sent-At-Ns: {}\r\nX-Decoder-Frames: {}\r\nX-Decoder-Codec-Ns: {}\r\nX-Decoder-Scale-Ns: {}\r\nX-Decoder-Copy-Ns: {}\r\nX-Mjpeg-Encode-Ns: {}\r\nX-Mjpeg-Jpeg-Bytes: {}\r\n\r\n",
        jpeg.len(),
        frame.identity.frame_sequence,
        frame.identity.host_monotonic_time_ns,
        sent_at_ns,
        stats.decoded_frames,
        stats.codec_stage_ns,
        stats.scale_stage_ns,
        stats.copy_stage_ns,
        encode_ns,
        jpeg.len(),
    );
    let mut chunk = Vec::with_capacity(headers.len() + jpeg.len() + 2);
    chunk.extend_from_slice(headers.as_bytes());
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

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
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
            // 不传 fps 时跟随 RTSP/decoder 发布的新帧；显式传入时才做预览降采样。
            fps_limit: query.fps.map(|fps| fps.clamp(1, 120)),
            width,
            height: query.height.unwrap_or(default_height).clamp(90, 1080),
        })
    }
}

impl WorkflowStore {
    fn list(&self) -> std::result::Result<Vec<WorkflowSummary>, (StatusCode, String)> {
        let mut summaries = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(internal_error)? {
            let entry = entry.map_err(internal_error)?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let graph = read_workflow_file(&path)?;
            summaries.push(WorkflowSummary {
                id: graph.id,
                title: graph.title,
                revision: graph.revision,
                node_count: graph.nodes.len(),
                edge_count: graph.edges.len(),
            });
        }
        summaries.sort_by(|left, right| left.title.cmp(&right.title));
        Ok(summaries)
    }

    fn load(&self, id: &str) -> std::result::Result<WorkflowGraph, (StatusCode, String)> {
        self.load_optional(id)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("workflow `{id}` not found")))
    }

    fn load_optional(
        &self,
        id: &str,
    ) -> std::result::Result<Option<WorkflowGraph>, (StatusCode, String)> {
        let path = self.path_for_id(id)?;
        if !path.exists() {
            return Ok(None);
        }
        read_workflow_file(&path).map(Some)
    }

    fn save(&self, graph: &WorkflowGraph) -> std::result::Result<(), (StatusCode, String)> {
        let path = self.path_for_id(&graph.id)?;
        fs::create_dir_all(&self.dir).map_err(internal_error)?;
        let tmp = path.with_extension("json.tmp");
        let content = serde_json::to_vec_pretty(graph).map_err(internal_error)?;
        fs::write(&tmp, content).map_err(internal_error)?;
        fs::rename(&tmp, path).map_err(internal_error)?;
        Ok(())
    }

    fn path_for_id(&self, id: &str) -> std::result::Result<PathBuf, (StatusCode, String)> {
        if !is_safe_workflow_id(id) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("invalid workflow id `{id}`"),
            ));
        }
        Ok(self.dir.join(format!("{id}.ctworkflow.json")))
    }
}

fn read_workflow_file(path: &Path) -> std::result::Result<WorkflowGraph, (StatusCode, String)> {
    let raw = fs::read(path).map_err(internal_error)?;
    let graph: WorkflowGraph = serde_json::from_slice(&raw).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("failed to parse workflow `{}`: {error}", path.display()),
        )
    })?;
    validate_workflow(&graph).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid workflow `{}`: {error}", path.display()),
        )
    })?;
    Ok(graph)
}

fn is_safe_workflow_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn next_revision() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("rev-{nanos}")
}

fn bad_request(error: String) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, error)
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn default_static_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web/dist")
}

fn default_workflow_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".workflow-web/workflows")
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
    fn default_workflow_dir_points_to_crate_local_store() {
        let path = default_workflow_dir();
        assert!(path.ends_with(".workflow-web/workflows"));
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
    fn mjpeg_chunk_includes_runtime_metrics_headers() {
        let frame = DecodedVideoFrame {
            width: 1,
            height: 1,
            rgba: vec![16, 32, 48, 255].into(),
            identity: camera_toolbox_app::StreamFrameIdentity::unavailable(
                StreamSessionId::new("workflow-mjpeg-test").unwrap(),
                0,
                42,
                "unit test",
            ),
        };
        let stats = FfmpegRtspDecoderStatsSnapshot {
            decoded_frames: 7,
            io_bytes_available: false,
            io_bytes: 0,
            media_packet_bytes: 0,
            codec_stage_ns: 10,
            scale_stage_ns: 20,
            copy_stage_ns: 30,
        };

        let chunk = mjpeg_chunk(&frame, &stats).unwrap();
        let header_end = chunk
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("headers are terminated");
        let headers = std::str::from_utf8(&chunk[..header_end]).unwrap();

        assert!(headers.contains("Content-Type: image/jpeg"));
        assert!(headers.contains("X-Frame-Sequence: 42"));
        assert!(headers.contains("X-Mjpeg-Sent-At-Ns:"));
        assert!(headers.contains("X-Decoder-Frames: 7"));
        assert!(headers.contains("X-Decoder-Codec-Ns: 10"));
        assert!(headers.contains("X-Decoder-Scale-Ns: 20"));
        assert!(headers.contains("X-Decoder-Copy-Ns: 30"));
        assert!(headers.contains("X-Mjpeg-Encode-Ns:"));
        assert!(headers.contains("X-Mjpeg-Jpeg-Bytes:"));
    }

    #[test]
    fn mjpeg_config_preserves_source_rate_by_default() {
        let explicit_limit = MjpegStreamConfig::from_query(MjpegStreamQuery {
            url: "rtsp://camera.local/stream".to_owned(),
            fps: Some(300),
            width: Some(4096),
            height: Some(4096),
        })
        .expect("valid RTSP URL");
        assert_eq!(explicit_limit.fps_limit, Some(120));
        assert_eq!(explicit_limit.width, 1920);
        assert_eq!(explicit_limit.height, 1080);

        let source_rate = MjpegStreamConfig::from_query(MjpegStreamQuery {
            url: "rtsp://camera.local/stream".to_owned(),
            fps: None,
            width: Some(960),
            height: None,
        })
        .expect("valid RTSP URL");
        assert_eq!(source_rate.fps_limit, None);
        assert_eq!(source_rate.height, 540);
    }

    #[test]
    fn workflow_store_roundtrips_normalized_graph() {
        let dir = std::env::temp_dir().join(format!("workflow-store-test-{}", next_revision()));
        let store = WorkflowStore { dir: dir.clone() };
        let mut graph = seed_workflow_graph();
        graph.id = "roundtrip".to_owned();
        graph.revision = "rev-test".to_owned();
        store.save(&graph).expect("workflow saved");

        let loaded = store.load("roundtrip").expect("workflow loaded");
        assert_eq!(loaded.id, "roundtrip");
        assert_eq!(loaded.revision, "rev-test");
        assert_eq!(loaded.nodes.len(), graph.nodes.len());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn unsafe_workflow_ids_are_rejected() {
        assert!(!is_safe_workflow_id("../escape"));
        assert!(!is_safe_workflow_id(""));
        assert!(is_safe_workflow_id("camera_toolbox-1"));
    }
}
