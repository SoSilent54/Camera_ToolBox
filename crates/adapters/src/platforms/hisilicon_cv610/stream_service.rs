//! CV610 TCP 80 PQStream 会话、独立有界 recording/preview/decoder 链路与聚合 metrics。

use std::{
    collections::VecDeque,
    fs::File,
    io::{BufRead, BufReader, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use camera_toolbox_app::{
    LatestDecodedFrameSlot, RecordingBranch, RecordingState, StreamCodec, StreamMediaInfo,
    StreamMetrics, StreamOpenRequest, StreamOperationControl, StreamService, StreamServiceError,
    StreamServiceEvent, StreamSession, StreamSessionId, StreamStage, StreamTerminal,
};

use crate::media::{DecoderOfferError, FfmpegDecoder};

use super::pqstream_protocol::{
    AccessUnit, AccessUnitAssembler, DEFAULT_MAX_HEADER_BYTES, DEFAULT_MAX_MEDIA_DESCRIPTION_BYTES,
    DEFAULT_MAX_RECORD_BYTES, DEFAULT_RTP_CONFIRMATION_PACKETS, H26xDepacketizer, MediaDescription,
    MediaRequest, PqStreamProtocolError, PreviewResync, RecordTruncationStage, RtpPacket,
    RtpValidator, VideoCodec, parse_http_response, parse_media_description, read_pq_record,
};

pub const DEFAULT_PREVIEW_AU_CAPACITY: usize = 4;
pub const DEFAULT_DECODER_INPUT_CAPACITY: usize = 4;
pub const DEFAULT_RECORDER_QUEUE_BYTES: usize = 16 * 1024 * 1024;
const METRICS_INTERVAL: Duration = Duration::from_millis(250);
const LOCAL_SIDECAR_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cv610StreamEndpoint {
    pub address: IpAddr,
    pub port: u16,
}

pub struct Cv610StreamService {
    service_id: String,
    endpoint: Cv610StreamEndpoint,
    ffmpeg_path: Option<PathBuf>,
    preview_capacity: usize,
    decoder_capacity: usize,
    recorder_queue_bytes: usize,
}

impl Cv610StreamService {
    pub fn new(
        service_id: impl Into<String>,
        endpoint: Cv610StreamEndpoint,
    ) -> Result<Self, StreamServiceError> {
        let service_id = service_id.into();
        if service_id.trim().is_empty() || endpoint.port == 0 {
            return Err(StreamServiceError::InvalidRequest(
                "stream service id and endpoint must be valid".to_owned(),
            ));
        }
        Ok(Self {
            service_id,
            endpoint,
            ffmpeg_path: None,
            preview_capacity: DEFAULT_PREVIEW_AU_CAPACITY,
            decoder_capacity: DEFAULT_DECODER_INPUT_CAPACITY,
            recorder_queue_bytes: DEFAULT_RECORDER_QUEUE_BYTES,
        })
    }

    #[must_use]
    pub fn with_ffmpeg_path(mut self, path: PathBuf) -> Self {
        self.ffmpeg_path = Some(path);
        self
    }

    #[cfg(test)]
    fn with_queue_capacities(
        mut self,
        preview_capacity: usize,
        decoder_capacity: usize,
        recorder_queue_bytes: usize,
    ) -> Self {
        self.preview_capacity = preview_capacity;
        self.decoder_capacity = decoder_capacity;
        self.recorder_queue_bytes = recorder_queue_bytes;
        self
    }
}

impl StreamService for Cv610StreamService {
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
        if self.preview_capacity == 0
            || self.decoder_capacity == 0
            || self.recorder_queue_bytes == 0
        {
            return Err(StreamServiceError::InvalidRequest(
                "all stream queues must have positive bounded capacity".to_owned(),
            ));
        }
        let latest_frame = Arc::new(LatestDecodedFrameSlot::default());
        let worker_latest = Arc::clone(&latest_frame);
        let endpoint = self.endpoint.clone();
        let ffmpeg_path = self.ffmpeg_path.clone();
        let preview_capacity = self.preview_capacity;
        let decoder_capacity = self.decoder_capacity;
        let recorder_queue_bytes = self.recorder_queue_bytes;
        let worker_control = control.clone();
        let worker_session = session_id.clone();
        std::thread::Builder::new()
            .name(format!("cv610-{}", session_id.as_str()))
            .spawn(move || {
                let terminal = match run_session(
                    &endpoint,
                    &worker_session,
                    request,
                    &worker_control,
                    worker_latest,
                    ffmpeg_path.as_deref(),
                    preview_capacity,
                    decoder_capacity,
                    recorder_queue_bytes,
                ) {
                    Ok(terminal) => terminal,
                    Err(StreamServiceError::Cancelled { .. }) => StreamTerminal::Cancelled,
                    Err(error) => StreamTerminal::Failed(error),
                };
                worker_control.finish(terminal);
            })
            .map_err(|error| StreamServiceError::WorkerStart(error.to_string()))?;
        Ok(StreamSession::new(session_id, latest_frame, control))
    }
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    endpoint: &Cv610StreamEndpoint,
    _session_id: &StreamSessionId,
    request: StreamOpenRequest,
    control: &StreamOperationControl,
    latest_frame: Arc<LatestDecodedFrameSlot>,
    ffmpeg_path: Option<&Path>,
    preview_capacity: usize,
    decoder_capacity: usize,
    recorder_queue_bytes: usize,
) -> Result<StreamTerminal, StreamServiceError> {
    control.report(StreamServiceEvent::Stage(StreamStage::Connecting));
    let address = SocketAddr::new(endpoint.address, endpoint.port);
    let stream =
        TcpStream::connect_timeout(&address, control.timeouts.connect).map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                StreamServiceError::ConnectTimeout {
                    timeout_ms: duration_millis(control.timeouts.connect),
                }
            } else {
                StreamServiceError::Transport {
                    stage: StreamStage::Connecting,
                    reason: error.to_string(),
                }
            }
        })?;
    stream
        .set_read_timeout(Some(control.timeouts.idle))
        .map_err(|error| transport_error(StreamStage::Connecting, error))?;
    stream
        .set_write_timeout(Some(control.timeouts.idle))
        .map_err(|error| transport_error(StreamStage::Connecting, error))?;
    let interrupt = stream
        .try_clone()
        .map_err(|error| transport_error(StreamStage::Connecting, error))?;
    control.cancellation.register_interrupt(Arc::new(move || {
        let _ = interrupt.shutdown(Shutdown::Both);
    }));
    check_cancelled(control, StreamStage::Connecting)?;

    control.report(StreamServiceEvent::Stage(StreamStage::Negotiating));
    let request_bytes = MediaRequest {
        host: endpoint.address.to_string(),
        port: endpoint.port,
        channel: request.channel,
        media: request.media.clone(),
        cseq: request.cseq,
    }
    .to_bytes()
    .map_err(map_protocol)?;
    let mut writer = stream
        .try_clone()
        .map_err(|error| transport_error(StreamStage::Negotiating, error))?;
    writer
        .write_all(&request_bytes)
        .and_then(|()| writer.flush())
        .map_err(|error| map_io(StreamStage::Negotiating, control, error))?;

    let mut reader = BufReader::with_capacity(64 * 1024, stream);
    let raw_header = read_until_limit(
        &mut reader,
        b"\r\n\r\n",
        DEFAULT_MAX_HEADER_BYTES,
        StreamStage::Negotiating,
        control,
    )?;
    let response = parse_http_response(&raw_header).map_err(map_protocol)?;
    if response.status_code != 200 {
        return Err(StreamServiceError::Negotiation(format!(
            "HTTP-like status {} {}",
            response.status_code, response.reason
        )));
    }
    let raw_description = read_until_limit(
        &mut reader,
        b"\r\n\r\n",
        DEFAULT_MAX_MEDIA_DESCRIPTION_BYTES,
        StreamStage::Negotiating,
        control,
    )?;
    let description = parse_media_description(&raw_description).map_err(map_protocol)?;

    let metrics = Arc::new(MetricCounters::default());
    metrics.network_bytes.fetch_add(
        u64::try_from(raw_description.len()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    let mut transport_recorder =
        if let Some(destination) = request.recording.transport_destination.as_ref() {
            match RecorderProducer::start_transport(
                destination,
                request.recording.quota_bytes,
                recorder_queue_bytes,
                control.clone(),
                Arc::clone(&metrics),
            ) {
                Ok(recorder) => Some(recorder),
                Err(error) => {
                    control.report(StreamServiceEvent::Recording {
                        branch: RecordingBranch::TransportEvidence,
                        state: RecordingState::FailedIncomplete {
                            reason: error.to_string(),
                            bytes_written: 0,
                        },
                    });
                    None
                }
            }
        } else {
            None
        };
    let mut annexb_recorder = match (
        request.recording.annexb_destination.as_ref(),
        request.recording.timestamp_sidecar_destination.as_ref(),
    ) {
        (Some(destination), Some(sidecar)) => match RecorderProducer::start_annexb(
            destination,
            sidecar,
            request.recording.quota_bytes,
            recorder_queue_bytes,
            control.clone(),
            Arc::clone(&metrics),
        ) {
            Ok(recorder) => Some(recorder),
            Err(error) => {
                control.report(StreamServiceEvent::Recording {
                    branch: RecordingBranch::AnnexB,
                    state: RecordingState::FailedIncomplete {
                        reason: error.to_string(),
                        bytes_written: 0,
                    },
                });
                None
            }
        },
        (None, None) => None,
        _ => unreachable!("request validation pairs Annex-B and sidecar destinations"),
    };
    if let Some(recorder) = transport_recorder.as_mut()
        && let Err(error) = recorder.offer(RecordChunk::transport(raw_description))
    {
        recorder.fail(error);
        transport_recorder = None;
    }

    let decoder = match FfmpegDecoder::start(
        ffmpeg_path,
        description.codec,
        description.width,
        description.height,
        decoder_capacity,
        latest_frame,
        &control.cancellation,
    ) {
        Ok(decoder) => Some(decoder),
        Err(error) => {
            control.report(StreamServiceEvent::DecoderUnavailable {
                reason: error.to_string(),
            });
            None
        }
    };
    let resync_requested = Arc::new(AtomicBool::new(false));
    let (preview_sender, preview_depth) = decoder.clone().map_or((None, None), |decoder| {
        let (sender, receiver) = mpsc::sync_channel::<AccessUnit>(preview_capacity);
        let depth = Arc::new(AtomicUsize::new(0));
        let worker_depth = Arc::clone(&depth);
        let worker_metrics = Arc::clone(&metrics);
        let worker_resync = Arc::clone(&resync_requested);
        std::thread::Builder::new()
            .name("cv610-preview-au".to_owned())
            .spawn(move || {
                while let Ok(access_unit) = receiver.recv() {
                    worker_depth.fetch_sub(1, Ordering::AcqRel);
                    if decoder.offer(access_unit.annexb.into()) == Err(DecoderOfferError::Full) {
                        worker_metrics
                            .decoder_input_dropped
                            .fetch_add(1, Ordering::Relaxed);
                        worker_resync.store(true, Ordering::Release);
                    }
                }
            })
            .expect("validated bounded preview worker spawn");
        (Some(sender), Some(depth))
    });

    let session_result = (|| -> Result<StreamTerminal, StreamServiceError> {
        let mut validator = RtpValidator::new(
            DEFAULT_RTP_CONFIRMATION_PACKETS,
            description.payload_type,
            description.ssrc,
        )
        .map_err(map_protocol)?;
        let mut depacketizer = H26xDepacketizer::new(description.codec);
        let mut assembler = AccessUnitAssembler::new(description.codec);
        let mut preview_gate = PreviewResync::new(description.codec);
        let mut negotiated = false;
        let mut last_metrics = Instant::now();
        let mut access_unit_index = 0_u64;
        control.report(StreamServiceEvent::Stage(StreamStage::ConfirmingRtp));

        loop {
            check_cancelled(control, StreamStage::Playing)?;
            let record = match read_pq_record(
                &mut reader,
                description.rtp_channel,
                DEFAULT_MAX_RECORD_BYTES,
            ) {
                Ok(Some(record)) => record,
                Ok(None) => {
                    if let Some(access_unit) = assembler.flush() {
                        route_access_unit(
                            access_unit,
                            description.clock_rate,
                            &mut access_unit_index,
                            &mut annexb_recorder,
                            preview_sender.as_ref(),
                            preview_depth.as_ref(),
                            &mut preview_gate,
                            &metrics,
                        );
                    }
                    report_metrics(
                        control,
                        &metrics,
                        transport_recorder.as_ref(),
                        annexb_recorder.as_ref(),
                        preview_depth.as_ref(),
                        decoder.as_ref(),
                    );
                    return Ok(StreamTerminal::BoundaryClosed);
                }
                Err(PqStreamProtocolError::Io(error)) => {
                    return Err(map_io(StreamStage::Playing, control, error));
                }
                Err(error) => return Err(map_protocol(error)),
            };
            let record_bytes = record
                .raw_header
                .len()
                .checked_add(record.packet.len())
                .ok_or_else(|| {
                    StreamServiceError::ProtocolViolation("record byte count overflow".to_owned())
                })?;
            metrics.network_bytes.fetch_add(
                u64::try_from(record_bytes).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            if let Some(recorder) = transport_recorder.as_mut() {
                let mut bytes = Vec::with_capacity(record_bytes);
                bytes.extend_from_slice(&record.raw_header);
                bytes.extend_from_slice(&record.packet);
                if let Err(error) = recorder.offer(RecordChunk::transport(bytes)) {
                    recorder.fail(error);
                    transport_recorder = None;
                }
            }
            let packet = RtpPacket::parse(&record.packet).map_err(map_protocol)?;
            let packet_ssrc = packet.ssrc;
            metrics.rtp_packets.fetch_add(1, Ordering::Relaxed);
            let validated = validator.accept(packet).map_err(map_protocol)?;
            if validator.confirmed() && !negotiated {
                let ssrc = description.ssrc.unwrap_or(packet_ssrc);
                control.report(StreamServiceEvent::Negotiated(media_info(
                    &response,
                    &description,
                    ssrc,
                )));
                control.report(StreamServiceEvent::Stage(StreamStage::Playing));
                negotiated = true;
            }
            if validated.had_gap {
                metrics.rtp_gaps.fetch_add(
                    u64::from(validated.missing_packets.max(1)),
                    Ordering::Relaxed,
                );
                depacketizer.discard_incomplete_fragment();
                preview_gate.enter();
                metrics.decoder_resyncs.fetch_add(1, Ordering::Relaxed);
            }
            if resync_requested.swap(false, Ordering::AcqRel) {
                depacketizer.discard_incomplete_fragment();
                preview_gate.enter();
                metrics.decoder_resyncs.fetch_add(1, Ordering::Relaxed);
            }
            for packet in validated.packets {
                let units = depacketizer.push(&packet).map_err(map_protocol)?;
                for access_unit in assembler.push(&packet, units) {
                    route_access_unit(
                        access_unit,
                        description.clock_rate,
                        &mut access_unit_index,
                        &mut annexb_recorder,
                        preview_sender.as_ref(),
                        preview_depth.as_ref(),
                        &mut preview_gate,
                        &metrics,
                    );
                }
            }
            if last_metrics.elapsed() >= METRICS_INTERVAL {
                report_metrics(
                    control,
                    &metrics,
                    transport_recorder.as_ref(),
                    annexb_recorder.as_ref(),
                    preview_depth.as_ref(),
                    decoder.as_ref(),
                );
                last_metrics = Instant::now();
            }
        }
    })();

    drop(preview_sender);
    close_recorders(&mut transport_recorder, &mut annexb_recorder);
    if control.cancellation.is_cancelled() {
        control.report(StreamServiceEvent::Stage(StreamStage::Closing));
    }
    complete_decoder_shutdown(decoder.as_ref(), control.cancellation.is_cancelled());
    session_result
}

fn complete_decoder_shutdown(decoder: Option<&FfmpegDecoder>, cancellation_pending: bool) {
    let Some(decoder) = decoder else {
        return;
    };
    decoder.close_input();
    let local_deadline = Instant::now() + LOCAL_SIDECAR_EXIT_TIMEOUT;
    loop {
        if decoder.is_exited() {
            return;
        }
        if !cancellation_pending && Instant::now() >= local_deadline {
            decoder.kill();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[allow(clippy::too_many_arguments)]
fn route_access_unit(
    access_unit: AccessUnit,
    clock_rate: u32,
    index: &mut u64,
    recorder: &mut Option<RecorderProducer>,
    preview: Option<&SyncSender<AccessUnit>>,
    preview_depth: Option<&Arc<AtomicUsize>>,
    preview_gate: &mut PreviewResync,
    metrics: &MetricCounters,
) {
    let record_error = recorder.as_ref().and_then(|recorder| {
        let sidecar = timestamp_sidecar(*index, &access_unit, clock_rate);
        let chunk = RecordChunk::annexb(access_unit.annexb.clone(), sidecar);
        recorder.offer(chunk).err()
    });
    if let Some(error) = record_error
        && let Some(mut failed) = recorder.take()
    {
        failed.fail(error);
    }
    *index = index.saturating_add(1);
    let Some(preview) = preview else {
        return;
    };
    let Some(access_unit) = preview_gate.accept(access_unit) else {
        metrics.preview_dropped.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let depth = preview_depth.expect("preview sender and depth are paired");
    depth.fetch_add(1, Ordering::AcqRel);
    match preview.try_send(access_unit) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            depth.fetch_sub(1, Ordering::AcqRel);
            metrics.preview_dropped.fetch_add(1, Ordering::Relaxed);
            preview_gate.enter();
            metrics.decoder_resyncs.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {
            depth.fetch_sub(1, Ordering::AcqRel);
            metrics.preview_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn media_info(
    response: &super::pqstream_protocol::HttpLikeResponse,
    description: &MediaDescription,
    ssrc: u32,
) -> StreamMediaInfo {
    StreamMediaInfo {
        session: response.header("session").map(str::to_owned),
        codec: match description.codec {
            VideoCodec::H264 => StreamCodec::H264,
            VideoCodec::H265 => StreamCodec::H265,
        },
        payload_type: description.payload_type,
        ssrc,
        width: description.width,
        height: description.height,
        clock_rate: description.clock_rate,
        frame_rate: description.frame_rate,
        bitrate_value: description.bitrate_value,
        rtp_channel: description.rtp_channel,
    }
}

#[derive(Default)]
struct MetricCounters {
    network_bytes: AtomicU64,
    rtp_packets: AtomicU64,
    rtp_gaps: AtomicU64,
    preview_dropped: AtomicU64,
    decoder_input_dropped: AtomicU64,
    decoder_resyncs: AtomicU64,
    record_bytes: AtomicU64,
}

fn report_metrics(
    control: &StreamOperationControl,
    counters: &MetricCounters,
    transport: Option<&RecorderProducer>,
    annexb: Option<&RecorderProducer>,
    preview_depth: Option<&Arc<AtomicUsize>>,
    decoder: Option<&FfmpegDecoder>,
) {
    let decoder_stats = decoder.map(FfmpegDecoder::stats).unwrap_or_default();
    control.report(StreamServiceEvent::Metrics(StreamMetrics {
        network_bytes: counters.network_bytes.load(Ordering::Relaxed),
        rtp_packets: counters.rtp_packets.load(Ordering::Relaxed),
        rtp_gaps: counters.rtp_gaps.load(Ordering::Relaxed),
        preview_dropped: counters.preview_dropped.load(Ordering::Relaxed),
        decoder_input_dropped: counters.decoder_input_dropped.load(Ordering::Relaxed),
        decoder_resyncs: counters.decoder_resyncs.load(Ordering::Relaxed),
        decoded_frames: decoder_stats.decoded,
        presented_frames: 0,
        transport_queue_bytes: transport.map_or(0, RecorderProducer::queue_bytes),
        recorder_queue_bytes: annexb.map_or(0, RecorderProducer::queue_bytes),
        preview_queue_depth: preview_depth.map_or(0, |depth| depth.load(Ordering::Acquire)),
        decoder_queue_depth: decoder_stats.input_depth,
        record_bytes: counters.record_bytes.load(Ordering::Relaxed),
    }));
}

fn timestamp_sidecar(index: u64, unit: &AccessUnit, clock_rate: u32) -> Vec<u8> {
    let host_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{{\"access_unit\":{index},\"rtp_timestamp\":{},\"clock_rate\":{clock_rate},\"marker\":{},\"host_record_time_unix_ns\":{host_ns}}}\n",
        unit.rtp_timestamp, unit.marker
    )
    .into_bytes()
}

fn close_recorders(
    transport: &mut Option<RecorderProducer>,
    annexb: &mut Option<RecorderProducer>,
) {
    if let Some(recorder) = transport.take() {
        recorder.close();
    }
    if let Some(recorder) = annexb.take() {
        recorder.close();
    }
}

#[derive(Debug)]
struct RecordChunk {
    payload: Vec<u8>,
    sidecar: Option<Vec<u8>>,
}

impl RecordChunk {
    fn transport(payload: Vec<u8>) -> Self {
        Self {
            payload,
            sidecar: None,
        }
    }

    fn annexb(payload: Vec<u8>, sidecar: Vec<u8>) -> Self {
        Self {
            payload,
            sidecar: Some(sidecar),
        }
    }

    fn bytes(&self) -> usize {
        self.payload
            .len()
            .saturating_add(self.sidecar.as_ref().map_or(0, Vec::len))
    }
}

#[derive(Debug)]
enum RecorderOfferError {
    QueueFull,
    QuotaExceeded,
    Closed,
}

impl std::fmt::Display for RecorderOfferError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => formatter.write_str("bounded recorder queue is full"),
            Self::QuotaExceeded => formatter.write_str("recording quota exceeded"),
            Self::Closed => formatter.write_str("recorder writer is closed"),
        }
    }
}

struct RecorderQueueState {
    chunks: VecDeque<RecordChunk>,
    queued_bytes: usize,
    accepted_bytes: u64,
    closed: bool,
    failure_reason: Option<String>,
}

struct RecorderQueue {
    max_queue_bytes: usize,
    quota_bytes: u64,
    state: Mutex<RecorderQueueState>,
    ready: Condvar,
}

impl RecorderQueue {
    fn new(max_queue_bytes: usize, quota_bytes: u64) -> Self {
        Self {
            max_queue_bytes,
            quota_bytes,
            state: Mutex::new(RecorderQueueState {
                chunks: VecDeque::new(),
                queued_bytes: 0,
                accepted_bytes: 0,
                closed: false,
                failure_reason: None,
            }),
            ready: Condvar::new(),
        }
    }

    fn try_push(&self, chunk: RecordChunk) -> Result<(), RecorderOfferError> {
        let bytes = chunk.bytes();
        let bytes_u64 = u64::try_from(bytes).map_err(|_| RecorderOfferError::QuotaExceeded)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(RecorderOfferError::Closed);
        }
        if state.accepted_bytes.saturating_add(bytes_u64) > self.quota_bytes {
            return Err(RecorderOfferError::QuotaExceeded);
        }
        if state.queued_bytes.saturating_add(bytes) > self.max_queue_bytes {
            return Err(RecorderOfferError::QueueFull);
        }
        state.accepted_bytes += bytes_u64;
        state.queued_bytes += bytes;
        state.chunks.push_back(chunk);
        self.ready.notify_one();
        Ok(())
    }

    fn pop(&self) -> Option<RecordChunk> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(chunk) = state.chunks.pop_front() {
                state.queued_bytes = state.queued_bytes.saturating_sub(chunk.bytes());
                return Some(chunk);
            }
            if state.closed {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
        self.ready.notify_all();
    }

    fn fail(&self, reason: String) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.failure_reason.is_none() {
            state.failure_reason = Some(reason);
        }
        state.closed = true;
        self.ready.notify_all();
    }

    fn failure_reason(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure_reason
            .clone()
    }

    fn queue_bytes(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queued_bytes
    }
}

struct RecorderProducer {
    queue: Arc<RecorderQueue>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl RecorderProducer {
    fn start_transport(
        destination: &Path,
        quota_bytes: u64,
        queue_bytes: usize,
        control: StreamOperationControl,
        counters: Arc<MetricCounters>,
    ) -> Result<Self, StreamServiceError> {
        let file = File::create(destination).map_err(|error| StreamServiceError::Transport {
            stage: StreamStage::Negotiating,
            reason: format!(
                "cannot open explicit transport recording target {}: {error}",
                destination.display()
            ),
        })?;
        Self::start_writer(
            RecordingBranch::TransportEvidence,
            file,
            None,
            quota_bytes,
            queue_bytes,
            control,
            counters,
        )
    }

    fn start_annexb(
        destination: &Path,
        sidecar_destination: &Path,
        quota_bytes: u64,
        queue_bytes: usize,
        control: StreamOperationControl,
        counters: Arc<MetricCounters>,
    ) -> Result<Self, StreamServiceError> {
        let file = File::create(destination).map_err(|error| StreamServiceError::Transport {
            stage: StreamStage::Negotiating,
            reason: format!(
                "cannot open explicit Annex-B recording target {}: {error}",
                destination.display()
            ),
        })?;
        let sidecar =
            File::create(sidecar_destination).map_err(|error| StreamServiceError::Transport {
                stage: StreamStage::Negotiating,
                reason: format!(
                    "cannot open explicit timestamp sidecar target {}: {error}",
                    sidecar_destination.display()
                ),
            })?;
        Self::start_writer(
            RecordingBranch::AnnexB,
            file,
            Some(sidecar),
            quota_bytes,
            queue_bytes,
            control,
            counters,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_writer(
        branch: RecordingBranch,
        mut file: File,
        mut sidecar: Option<File>,
        quota_bytes: u64,
        queue_bytes: usize,
        control: StreamOperationControl,
        counters: Arc<MetricCounters>,
    ) -> Result<Self, StreamServiceError> {
        let queue = Arc::new(RecorderQueue::new(queue_bytes, quota_bytes));
        let worker_queue = Arc::clone(&queue);
        let worker_control = control.clone();
        let worker_counters = Arc::clone(&counters);
        let worker = std::thread::Builder::new()
            .name(format!("cv610-record-{branch:?}"))
            .spawn(move || {
                let mut written = 0_u64;
                while let Some(chunk) = worker_queue.pop() {
                    let write = file.write_all(&chunk.payload).and_then(|()| {
                        if let (Some(sidecar), Some(bytes)) =
                            (sidecar.as_mut(), chunk.sidecar.as_ref())
                        {
                            sidecar.write_all(bytes)?;
                        }
                        Ok(())
                    });
                    if let Err(error) = write {
                        worker_queue.fail(error.to_string());
                        break;
                    }
                    let chunk_bytes = u64::try_from(chunk.bytes()).unwrap_or(u64::MAX);
                    written = written.saturating_add(chunk_bytes);
                    worker_counters
                        .record_bytes
                        .fetch_add(chunk_bytes, Ordering::Relaxed);
                }
                let flush_error = file
                    .flush()
                    .and_then(|()| {
                        if let Some(sidecar) = sidecar.as_mut() {
                            sidecar.flush()?;
                        }
                        Ok(())
                    })
                    .err()
                    .map(|error| error.to_string());
                let failure = worker_queue.failure_reason().or(flush_error);
                let state = failure.map_or(
                    RecordingState::Completed {
                        bytes_written: written,
                    },
                    |reason| RecordingState::FailedIncomplete {
                        reason,
                        bytes_written: written,
                    },
                );
                worker_control.report(StreamServiceEvent::Recording { branch, state });
            })
            .map_err(|error| StreamServiceError::WorkerStart(error.to_string()))?;
        control.report(StreamServiceEvent::Recording {
            branch,
            state: RecordingState::Started,
        });
        Ok(Self {
            queue,
            worker: Some(worker),
        })
    }

    fn offer(&self, chunk: RecordChunk) -> Result<(), RecorderOfferError> {
        self.queue.try_push(chunk)
    }

    fn fail(&mut self, error: RecorderOfferError) {
        self.queue.fail(error.to_string());
    }

    fn queue_bytes(&self) -> usize {
        self.queue.queue_bytes()
    }

    fn close(mut self) {
        self.queue.close();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for RecorderProducer {
    fn drop(&mut self) {
        self.queue.close();
    }
}

fn read_until_limit(
    reader: &mut impl BufRead,
    marker: &[u8],
    limit: usize,
    stage: StreamStage,
    control: &StreamOperationControl,
) -> Result<Vec<u8>, StreamServiceError> {
    let mut bytes = Vec::new();
    loop {
        check_cancelled(control, stage)?;
        let available = reader
            .fill_buf()
            .map_err(|error| map_io(stage, control, error))?;
        if available.is_empty() {
            return Err(StreamServiceError::Negotiation(
                "peer closed before negotiation prelude completed".to_owned(),
            ));
        }
        bytes.push(available[0]);
        reader.consume(1);
        if bytes.ends_with(marker) {
            return Ok(bytes);
        }
        if bytes.len() >= limit {
            return Err(StreamServiceError::Negotiation(format!(
                "negotiation field exceeds {limit} bytes"
            )));
        }
    }
}

fn check_cancelled(
    control: &StreamOperationControl,
    stage: StreamStage,
) -> Result<(), StreamServiceError> {
    if control.cancellation.is_cancelled() {
        Err(StreamServiceError::Cancelled { stage })
    } else {
        Ok(())
    }
}

fn map_io(
    stage: StreamStage,
    control: &StreamOperationControl,
    error: std::io::Error,
) -> StreamServiceError {
    if control.cancellation.is_cancelled() {
        StreamServiceError::Cancelled { stage }
    } else if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        StreamServiceError::IdleTimeout {
            stage,
            timeout_ms: duration_millis(control.timeouts.idle),
        }
    } else {
        transport_error(stage, error)
    }
}

fn transport_error(stage: StreamStage, error: std::io::Error) -> StreamServiceError {
    StreamServiceError::Transport {
        stage,
        reason: error.to_string(),
    }
}

fn map_protocol(error: PqStreamProtocolError) -> StreamServiceError {
    match error {
        PqStreamProtocolError::TruncatedRecord {
            stage,
            expected,
            received,
        } => StreamServiceError::TruncatedRecord {
            stage: match stage {
                RecordTruncationStage::Header => "header",
                RecordTruncationStage::Payload => "payload",
            }
            .to_owned(),
            expected,
            received,
        },
        PqStreamProtocolError::UnsupportedFraming(reason) => {
            StreamServiceError::UnsupportedFraming(reason)
        }
        PqStreamProtocolError::Io(error) => StreamServiceError::Transport {
            stage: StreamStage::Playing,
            reason: error.to_string(),
        },
        other => StreamServiceError::ProtocolViolation(other.to_string()),
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{fs, net::TcpListener, sync::mpsc::RecvTimeoutError, time::Duration};

    use camera_toolbox_app::{StreamCancellation, StreamRecordingRequest, StreamTimeouts};

    use super::*;

    fn rtp(sequence: u16, timestamp: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x80, if marker { 0x80 | 98 } else { 98 }];
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&0x1234_5678_u32.to_be_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    fn pq_record(packet: &[u8]) -> Vec<u8> {
        let mut record = b"$\x00\x80\x00".to_vec();
        record.extend_from_slice(&u32::try_from(packet.len()).unwrap().to_be_bytes());
        record.extend_from_slice(packet);
        record
    }

    #[test]
    fn fake_server_preserves_exact_request_transport_and_timestamp_sidecar() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let expected_request = MediaRequest {
            host: "127.0.0.1".to_owned(),
            port: address.port(),
            channel: 0,
            media: "video_data".to_owned(),
            cseq: 1,
        }
        .to_bytes()
        .unwrap();
        let description = b"m=video 98 H265/90000/2/2/30/6144\r\nTransport: RTP/AVP/TCP;unicast;otinterleaved=0-1;interleaved=0-1;ssrc=12345678\r\n\r\n";
        let records = [
            pq_record(&rtp(1, 100, false, b"\x00\x00\x00\x01\x40\x01A")),
            pq_record(&rtp(2, 100, false, b"\x00\x00\x00\x01\x42\x01B")),
            pq_record(&rtp(3, 100, true, b"\x00\x00\x00\x01\x44\x01C")),
            pq_record(&rtp(4, 200, true, b"\x00\x00\x00\x01\x26\x01D")),
        ];
        let body = [description.as_slice(), &records.concat()].concat();
        let server_body = body.clone();
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut request = vec![0_u8; expected_request.len()];
            std::io::Read::read_exact(&mut connection, &mut request).unwrap();
            assert_eq!(request, expected_request);
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nSession: 42\r\nContent-Type: application/octet-stream\r\n\r\n")
                .unwrap();
            connection.write_all(&server_body).unwrap();
        });

        let root =
            std::env::temp_dir().join(format!("camera-toolbox-stream-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let transport = root.join("capture.raw");
        let annexb = root.join("capture.h265");
        let sidecar = root.join("capture.timestamps.jsonl");
        let service = Cv610StreamService::new(
            "test-stream",
            Cv610StreamEndpoint {
                address: address.ip(),
                port: address.port(),
            },
        )
        .unwrap()
        .with_ffmpeg_path(PathBuf::from("/missing/ffmpeg"))
        .with_queue_capacities(1, 1, 1024 * 1024);
        let (events_sender, events_receiver) = mpsc::sync_channel(64);
        let control = StreamOperationControl::new(
            StreamTimeouts {
                connect: Duration::from_secs(1),
                idle: Duration::from_secs(1),
            },
            StreamCancellation::default(),
            Arc::new(move |event| {
                let _ = events_sender.try_send(event);
            }),
        )
        .unwrap();
        let session = service
            .open(
                StreamSessionId::new("fixture").unwrap(),
                StreamOpenRequest {
                    channel: 0,
                    media: "video_data".to_owned(),
                    cseq: 1,
                    recording: StreamRecordingRequest {
                        transport_destination: Some(transport.clone()),
                        annexb_destination: Some(annexb.clone()),
                        timestamp_sidecar_destination: Some(sidecar.clone()),
                        quota_bytes: 1024 * 1024,
                    },
                },
                control,
            )
            .unwrap();
        let mut saw_boundary = false;
        loop {
            match events_receiver.recv_timeout(Duration::from_secs(3)) {
                Ok(StreamServiceEvent::Terminal(StreamTerminal::BoundaryClosed)) => {
                    saw_boundary = true;
                    break;
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        server.join().unwrap();
        assert!(saw_boundary);
        assert_eq!(fs::read(&transport).unwrap(), body);
        let recorded = fs::read(&annexb).unwrap();
        assert!(recorded.ends_with(b"\x00\x00\x00\x01\x26\x01D"));
        let timestamps = fs::read_to_string(&sidecar).unwrap();
        assert!(timestamps.contains("\"rtp_timestamp\":100"));
        assert!(timestamps.contains("\"rtp_timestamp\":200"));
        assert!(session.latest_frame.latest().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ignored_eof_decoder_retains_session_until_force_cleanup() {
        use std::{io::Read, os::unix::fs::PermissionsExt};

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let expected_request_len = MediaRequest {
            host: "127.0.0.1".to_owned(),
            port: address.port(),
            channel: 0,
            media: "video_data".to_owned(),
            cseq: 1,
        }
        .to_bytes()
        .unwrap()
        .len();
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut request = vec![0_u8; expected_request_len];
            connection.read_exact(&mut request).unwrap();
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nSession: 42\r\n\r\n")
                .unwrap();
            connection
                .write_all(b"m=video 98 H265/90000/2/2/30/6144\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=12345678\r\n\r\n")
                .unwrap();
            for (sequence, payload) in [
                (1, &b"\x40\x01A"[..]),
                (2, &b"\x42\x01B"[..]),
                (3, &b"\x44\x01C"[..]),
            ] {
                connection
                    .write_all(&pq_record(&rtp(sequence, 100, true, payload)))
                    .unwrap();
            }
            let mut drain = Vec::new();
            let _ = connection.read_to_end(&mut drain);
        });

        let root = std::env::temp_dir().join(format!(
            "camera-toolbox-stream-eof-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let pid_file = root.join("sidecar.pid");
        let script = root.join("ignore-eof-ffmpeg.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nwhile :; do sleep 1; done\n",
                pid_file.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).unwrap();

        let service = Cv610StreamService::new(
            "ignored-eof-session",
            Cv610StreamEndpoint {
                address: address.ip(),
                port: address.port(),
            },
        )
        .unwrap()
        .with_ffmpeg_path(script);
        let (events_sender, events_receiver) = mpsc::sync_channel(64);
        let control = StreamOperationControl::new(
            StreamTimeouts {
                connect: Duration::from_secs(1),
                idle: Duration::from_secs(5),
            },
            StreamCancellation::default(),
            Arc::new(move |event| {
                let _ = events_sender.try_send(event);
            }),
        )
        .unwrap();
        let session = service
            .open(
                StreamSessionId::new("ignored-eof").unwrap(),
                StreamOpenRequest {
                    channel: 0,
                    media: "video_data".to_owned(),
                    cseq: 1,
                    recording: StreamRecordingRequest::default(),
                },
                control,
            )
            .unwrap();
        let pid_deadline = Instant::now() + Duration::from_secs(1);
        while !pid_file.exists() && Instant::now() < pid_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid = fs::read_to_string(&pid_file).unwrap().trim().to_owned();

        session.request_close();
        let quiet_deadline = Instant::now() + Duration::from_millis(150);
        while Instant::now() < quiet_deadline {
            if let Ok(StreamServiceEvent::Terminal(terminal)) =
                events_receiver.recv_timeout(Duration::from_millis(10))
            {
                panic!("terminal escaped before sidecar shutdown: {terminal:?}");
            }
        }
        assert!(session.force_cleanup());
        let mut forced = false;
        let event_deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < event_deadline {
            if let Ok(StreamServiceEvent::Terminal(StreamTerminal::Forced {
                remote_state_unknown: true,
            })) = events_receiver.recv_timeout(Duration::from_millis(20))
            {
                forced = true;
                break;
            }
        }
        assert!(forced);
        let process_path = PathBuf::from(format!("/proc/{pid}"));
        let reap_deadline = Instant::now() + Duration::from_secs(1);
        while process_path.exists() && Instant::now() < reap_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_path.exists(),
            "forced sidecar process must be reaped"
        );
        server.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn byte_queue_overflow_is_bounded_and_marks_lossless_branch_failed() {
        let queue = RecorderQueue::new(4, 100);
        assert!(
            queue
                .try_push(RecordChunk::transport(vec![1, 2, 3, 4]))
                .is_ok()
        );
        assert!(matches!(
            queue.try_push(RecordChunk::transport(vec![5])),
            Err(RecorderOfferError::QueueFull)
        ));
        assert_eq!(queue.queue_bytes(), 4);
    }
}
