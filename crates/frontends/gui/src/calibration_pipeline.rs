//! 标定 PNG 的有界两阶段后台流水线；GUI 线程只做非阻塞派发与结果安装。

use std::{
    io,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use camera_toolbox_adapters::{ImageRasterCodec, OpenCvCalibrationBackend};
use camera_toolbox_app::{
    AutoCandidateToken, CalibrationBackend, CalibrationBackendError, CalibrationCancellation,
    CalibrationEncodedPng, CalibrationInputError, CalibrationInputRevision, CalibrationJobToken,
    FileRef, FileSourceId, FileSystem, FileSystemError, FsCancellation, FsControl, RasterFormat,
    RasterImageCodec, read_calibration_png,
};
use camera_toolbox_core::{BoardSpec, ChessboardDetectionOutcome, Rgba8Frame};
use eframe::egui;

pub(crate) const IO_WORKERS: usize = 8;
pub(crate) const DETECTION_WORKERS: usize = 2;
pub(crate) const MAX_INFLIGHT_ENCODED_BYTES: u64 = 3 * MAX_ENCODED_PNG_BYTES;

const READ_INPUT_CAPACITY: usize = IO_WORKERS;
const READ_EVENT_CAPACITY: usize = 2 * IO_WORKERS;
const DETECTION_INPUT_CAPACITY: usize = DETECTION_WORKERS;
const DETECTION_EVENT_CAPACITY: usize = 2 * DETECTION_WORKERS;
pub(crate) const MAX_ENCODED_PNG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DECODED_PREVIEW_BYTES: usize = 256 * 1024 * 1024;
const FILE_READ_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ReadSource {
    pub source_id: FileSourceId,
    pub remote: bool,
}

pub(crate) struct ReadJob {
    pub batch_id: u64,
    pub token: CalibrationJobToken,
    pub source: ReadSource,
    pub file_system: Arc<dyn FileSystem>,
    pub reference: FileRef,
    pub file_cancellation: FsCancellation,
    pub calibration_cancellation: CalibrationCancellation,
    pub reserved_bytes: u64,
    queued_at: Instant,
}

impl ReadJob {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        batch_id: u64,
        token: CalibrationJobToken,
        source: ReadSource,
        file_system: Arc<dyn FileSystem>,
        reference: FileRef,
        file_cancellation: FsCancellation,
        calibration_cancellation: CalibrationCancellation,
    ) -> Self {
        let reserved_bytes = token.source_revision().encoded_bytes();
        Self {
            batch_id,
            token,
            source,
            file_system,
            reference,
            file_cancellation,
            calibration_cancellation,
            reserved_bytes,
            queued_at: Instant::now(),
        }
    }
}

/// 同一条 encoded-PNG worker 通道的两类请求。
#[derive(Clone, Debug)]
pub(crate) enum EncodedDetectionRequest {
    Dataset(CalibrationJobToken),
    Candidate(AutoCandidateToken),
}

impl EncodedDetectionRequest {
    #[must_use]
    pub(crate) const fn board(&self) -> BoardSpec {
        match self {
            Self::Dataset(token) => token.board(),
            Self::Candidate(token) => token.board(),
        }
    }

    #[must_use]
    pub(crate) fn source_revision(&self) -> &CalibrationInputRevision {
        match self {
            Self::Dataset(token) => token.source_revision(),
            Self::Candidate(token) => token.source_revision(),
        }
    }

    #[must_use]
    pub(crate) const fn kind(&self) -> &'static str {
        match self {
            Self::Dataset(_) => "dataset",
            Self::Candidate(_) => "candidate",
        }
    }

    #[must_use]
    pub(crate) const fn id(&self) -> u64 {
        match self {
            Self::Dataset(token) => token.item_id.get(),
            Self::Candidate(token) => token.id().get(),
        }
    }
}

pub(crate) struct LoadedDetectionJob {
    pub batch_id: u64,
    pub request: EncodedDetectionRequest,
    pub encoded: CalibrationEncodedPng,
    pub cancellation: CalibrationCancellation,
    pub reserved_bytes: u64,
    queued_at: Instant,
}

impl LoadedDetectionJob {
    pub(crate) fn from_encoded(
        batch_id: u64,
        request: EncodedDetectionRequest,
        encoded: CalibrationEncodedPng,
        cancellation: CalibrationCancellation,
    ) -> Self {
        Self {
            batch_id,
            request,
            encoded,
            cancellation,
            reserved_bytes: 0,
            queued_at: Instant::now(),
        }
    }
}

pub(crate) struct ReadStageResult {
    pub batch_id: u64,
    pub token: CalibrationJobToken,
    pub source: ReadSource,
    pub reserved_bytes: u64,
    pub result: Result<LoadedDetectionJob, PipelineStageError>,
}

pub(crate) enum ReadStageEvent {
    Started {
        batch_id: u64,
        token: CalibrationJobToken,
    },
    Finished(ReadStageResult),
}

pub(crate) struct DetectionProduct {
    pub source_revision: CalibrationInputRevision,
    pub outcome: ChessboardDetectionOutcome,
    pub preview: Option<Arc<Rgba8Frame>>,
}

pub(crate) struct DetectionStageResult {
    pub batch_id: u64,
    pub request: EncodedDetectionRequest,
    pub reserved_bytes: u64,
    pub result: Result<DetectionProduct, PipelineStageError>,
}

pub(crate) enum DetectionStageEvent {
    Started {
        batch_id: u64,
        request: EncodedDetectionRequest,
    },
    Finished(DetectionStageResult),
}

#[derive(Debug)]
pub(crate) struct PipelineStageError {
    message: String,
    cancelled: bool,
}

impl PipelineStageError {
    #[must_use]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

impl std::fmt::Display for PipelineStageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// 固定线程数、有界 channel 的读取/检测流水线。
pub(crate) struct CalibrationDetectionPipeline {
    read_sender: Option<SyncSender<ReadJob>>,
    read_events: Option<Receiver<ReadStageEvent>>,
    detection_sender: Option<SyncSender<LoadedDetectionJob>>,
    detection_events: Option<Receiver<DetectionStageEvent>>,
    workers: Vec<JoinHandle<()>>,
}

impl CalibrationDetectionPipeline {
    pub(crate) fn new(context: &egui::Context) -> io::Result<Self> {
        Self::new_with_components(
            context,
            Arc::new(OpenCvCalibrationBackend),
            Arc::new(ImageRasterCodec),
        )
    }

    fn new_with_components(
        context: &egui::Context,
        backend: Arc<dyn CalibrationBackend>,
        preview_codec: Arc<dyn RasterImageCodec>,
    ) -> io::Result<Self> {
        let (read_sender, read_receiver) = mpsc::sync_channel(READ_INPUT_CAPACITY);
        let (read_event_sender, read_events) = mpsc::sync_channel(READ_EVENT_CAPACITY);
        let (detection_sender, detection_receiver) = mpsc::sync_channel(DETECTION_INPUT_CAPACITY);
        let (detection_event_sender, detection_events) =
            mpsc::sync_channel(DETECTION_EVENT_CAPACITY);
        let read_receiver = Arc::new(Mutex::new(read_receiver));
        let detection_receiver = Arc::new(Mutex::new(detection_receiver));
        let mut workers = Vec::with_capacity(IO_WORKERS + DETECTION_WORKERS);

        for index in 0..IO_WORKERS {
            workers.push(spawn_read_worker(
                index,
                Arc::clone(&read_receiver),
                read_event_sender.clone(),
                context.clone(),
            )?);
        }
        for index in 0..DETECTION_WORKERS {
            workers.push(spawn_detection_worker(
                index,
                Arc::clone(&detection_receiver),
                detection_event_sender.clone(),
                Arc::clone(&backend),
                Arc::clone(&preview_codec),
                context.clone(),
            )?);
        }

        Ok(Self {
            read_sender: Some(read_sender),
            read_events: Some(read_events),
            detection_sender: Some(detection_sender),
            detection_events: Some(detection_events),
            workers,
        })
    }

    pub(crate) fn try_submit_read(&self, job: ReadJob) -> Result<(), TrySendError<ReadJob>> {
        match &self.read_sender {
            Some(sender) => sender.try_send(job),
            None => Err(TrySendError::Disconnected(job)),
        }
    }

    pub(crate) fn try_read_event(&self) -> Result<ReadStageEvent, TryRecvError> {
        self.read_events
            .as_ref()
            .map_or(Err(TryRecvError::Disconnected), Receiver::try_recv)
    }

    pub(crate) fn try_submit_detection(
        &self,
        job: LoadedDetectionJob,
    ) -> Result<(), TrySendError<LoadedDetectionJob>> {
        match &self.detection_sender {
            Some(sender) => sender.try_send(job),
            None => Err(TrySendError::Disconnected(job)),
        }
    }

    pub(crate) fn try_detection_event(&self) -> Result<DetectionStageEvent, TryRecvError> {
        self.detection_events
            .as_ref()
            .map_or(Err(TryRecvError::Disconnected), Receiver::try_recv)
    }
}

impl Drop for CalibrationDetectionPipeline {
    fn drop(&mut self) {
        drop(self.read_sender.take());
        drop(self.detection_sender.take());
        drop(self.read_events.take());
        drop(self.detection_events.take());
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn spawn_read_worker(
    index: usize,
    receiver: Arc<Mutex<Receiver<ReadJob>>>,
    events: SyncSender<ReadStageEvent>,
    repaint: egui::Context,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("camera-toolbox-calibration-read-{index}"))
        .spawn(move || {
            loop {
                let job = receiver
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .recv();
                let Ok(job) = job else {
                    break;
                };
                if events
                    .send(ReadStageEvent::Started {
                        batch_id: job.batch_id,
                        token: job.token.clone(),
                    })
                    .is_err()
                {
                    break;
                }
                repaint.request_repaint();
                if events
                    .send(ReadStageEvent::Finished(run_read(job)))
                    .is_err()
                {
                    break;
                }
                repaint.request_repaint();
            }
        })
}

fn spawn_detection_worker(
    index: usize,
    receiver: Arc<Mutex<Receiver<LoadedDetectionJob>>>,
    events: SyncSender<DetectionStageEvent>,
    backend: Arc<dyn CalibrationBackend>,
    preview_codec: Arc<dyn RasterImageCodec>,
    repaint: egui::Context,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("camera-toolbox-calibration-detect-{index}"))
        .spawn(move || {
            loop {
                let job = receiver
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .recv();
                let Ok(job) = job else {
                    break;
                };
                if events
                    .send(DetectionStageEvent::Started {
                        batch_id: job.batch_id,
                        request: job.request.clone(),
                    })
                    .is_err()
                {
                    break;
                }
                repaint.request_repaint();
                if events
                    .send(DetectionStageEvent::Finished(run_detection(
                        backend.as_ref(),
                        preview_codec.as_ref(),
                        job,
                    )))
                    .is_err()
                {
                    break;
                }
                repaint.request_repaint();
            }
        })
}

fn run_read(job: ReadJob) -> ReadStageResult {
    let queue_wait = job.queued_at.elapsed();
    let started = Instant::now();
    let mut control = FsControl::with_timeout(FILE_READ_TIMEOUT);
    control.cancellation = job.file_cancellation;
    let result = match job.token.source_revision().file_version() {
        Some(expected_version) => read_calibration_png(
            job.file_system.as_ref(),
            &job.reference,
            expected_version,
            MAX_ENCODED_PNG_BYTES,
            &control,
        )
        .map(|encoded| LoadedDetectionJob {
            batch_id: job.batch_id,
            request: EncodedDetectionRequest::Dataset(job.token.clone()),
            encoded,
            cancellation: job.calibration_cancellation,
            reserved_bytes: job.reserved_bytes,
            queued_at: Instant::now(),
        })
        .map_err(input_error),
        None => Err(PipelineStageError {
            message: "file read job received a non-file calibration input".to_owned(),
            cancelled: false,
        }),
    };
    tracing::debug!(
        operation = "calibration_pipeline_read",
        batch_id = job.batch_id,
        item_id = job.token.item_id.get(),
        source = %job.source.source_id,
        remote = job.source.remote,
        queue_wait_ms = queue_wait.as_secs_f64() * 1000.0,
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        bytes = job.reserved_bytes,
        success = result.is_ok(),
        "calibration PNG read stage finished"
    );
    ReadStageResult {
        batch_id: job.batch_id,
        token: job.token,
        source: job.source,
        reserved_bytes: job.reserved_bytes,
        result,
    }
}

fn run_detection(
    backend: &dyn CalibrationBackend,
    preview_codec: &dyn RasterImageCodec,
    job: LoadedDetectionJob,
) -> DetectionStageResult {
    let queue_wait = job.queued_at.elapsed();
    let detection_started = Instant::now();
    let request_kind = job.request.kind();
    let request_id = job.request.id();
    let detection = if job.encoded.source_revision != *job.request.source_revision() {
        Err(PipelineStageError {
            message: "encoded calibration PNG revision does not match its detection request"
                .to_owned(),
            cancelled: false,
        })
    } else {
        backend
            .detect_png(
                &job.encoded.bytes,
                job.encoded.image_size,
                MAX_DECODED_PREVIEW_BYTES,
                job.request.board(),
                &job.cancellation,
            )
            .map_err(backend_error)
    };
    let detection_elapsed = detection_started.elapsed();
    let result = detection.map(|outcome| {
        let preview_started = Instant::now();
        let preview = preview_codec
            .decode_rgba8(
                RasterFormat::Png,
                &job.encoded.bytes,
                MAX_DECODED_PREVIEW_BYTES,
            )
            .ok()
            .map(Arc::new);
        let preview_elapsed = preview_started.elapsed();
        tracing::debug!(
            operation = "calibration_pipeline_detect",
            batch_id = job.batch_id,
            request_kind,
            request_id,
            queue_wait_ms = queue_wait.as_secs_f64() * 1000.0,
            detect_ms = detection_elapsed.as_secs_f64() * 1000.0,
            preview_ms = preview_elapsed.as_secs_f64() * 1000.0,
            preview_available = preview.is_some(),
            "calibration detection stage finished"
        );
        DetectionProduct {
            source_revision: job.encoded.source_revision,
            outcome,
            preview,
        }
    });
    if let Err(error) = &result {
        tracing::debug!(
            operation = "calibration_pipeline_detect",
            batch_id = job.batch_id,
            request_kind,
            request_id,
            queue_wait_ms = queue_wait.as_secs_f64() * 1000.0,
            detect_ms = detection_elapsed.as_secs_f64() * 1000.0,
            error = %error,
            "calibration detection stage failed"
        );
    }
    DetectionStageResult {
        batch_id: job.batch_id,
        request: job.request,
        reserved_bytes: job.reserved_bytes,
        result,
    }
}

fn input_error(error: CalibrationInputError) -> PipelineStageError {
    let cancelled = matches!(
        &error,
        CalibrationInputError::FileSystem(FileSystemError::Cancelled)
    );
    PipelineStageError {
        message: error.to_string(),
        cancelled,
    }
}

fn backend_error(error: CalibrationBackendError) -> PipelineStageError {
    PipelineStageError {
        cancelled: matches!(error, CalibrationBackendError::Cancelled),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use camera_toolbox_adapters::filesystem::LocalFileSystem;
    use camera_toolbox_app::{
        AddCalibrationItemOutcome, CalibrationSession, FileSourceId, FileSystem, FileVersion,
        RasterCodecError, SourcePath,
    };
    use camera_toolbox_core::{
        BoardSpec, CalibrationDataError, CalibrationImageSize, CalibrationRequest,
        CalibrationSolution,
    };

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    struct DetectionGate {
        entered: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        released: AtomicBool,
        mutex: Mutex<()>,
        changed: Condvar,
    }

    impl DetectionGate {
        fn new() -> Self {
            Self {
                entered: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                released: AtomicBool::new(false),
                mutex: Mutex::new(()),
                changed: Condvar::new(),
            }
        }

        fn wait_for_entered(&self, expected: usize) {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut guard = self
                .mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while self.entered.load(Ordering::Acquire) < expected {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "only {} detector(s) entered",
                    self.entered.load(Ordering::Acquire)
                );
                let (next, timeout) = self.changed.wait_timeout(guard, remaining).unwrap();
                guard = next;
                assert!(
                    !timeout.timed_out(),
                    "only {} detector(s) entered",
                    self.entered.load(Ordering::Acquire)
                );
            }
        }

        fn release(&self) {
            self.released.store(true, Ordering::Release);
            self.changed.notify_all();
        }
    }

    struct BlockingBackend {
        gate: Arc<DetectionGate>,
    }

    impl CalibrationBackend for BlockingBackend {
        fn build_information(&self) -> Result<String, CalibrationBackendError> {
            Ok("test backend".to_owned())
        }

        fn detect_png(
            &self,
            _encoded_png: &[u8],
            expected_size: CalibrationImageSize,
            _decoded_byte_limit: usize,
            _board: BoardSpec,
            cancellation: &CalibrationCancellation,
        ) -> Result<ChessboardDetectionOutcome, CalibrationBackendError> {
            if cancellation.is_cancelled() {
                return Err(CalibrationBackendError::Cancelled);
            }
            let active = self.gate.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.gate.max_active.fetch_max(active, Ordering::AcqRel);
            self.gate.entered.fetch_add(1, Ordering::AcqRel);
            self.gate.changed.notify_all();
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut guard = self
                .gate
                .mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !self.gate.released.load(Ordering::Acquire) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    self.gate.active.fetch_sub(1, Ordering::AcqRel);
                    return Err(CalibrationBackendError::OpenCv {
                        operation: "test gate",
                        message: "timed out waiting for concurrent detector".to_owned(),
                    });
                }
                let (next, _) = self.gate.changed.wait_timeout(guard, remaining).unwrap();
                guard = next;
            }
            self.gate.active.fetch_sub(1, Ordering::AcqRel);
            Ok(ChessboardDetectionOutcome::NotFound {
                image_size: expected_size,
            })
        }

        fn calibrate(
            &self,
            _request: &CalibrationRequest,
            _cancellation: &CalibrationCancellation,
        ) -> Result<CalibrationSolution, CalibrationBackendError> {
            Err(CalibrationBackendError::InvalidData(
                CalibrationDataError::NoCalibrationViews,
            ))
        }
    }

    struct RejectingPreviewCodec;

    impl RasterImageCodec for RejectingPreviewCodec {
        fn decode_rgba8(
            &self,
            _format: RasterFormat,
            _encoded: &[u8],
            _decoded_byte_limit: usize,
        ) -> Result<Rgba8Frame, RasterCodecError> {
            Err(RasterCodecError::SignatureMismatch { expected: "test" })
        }

        fn encode_png(
            &self,
            _frame: &Rgba8Frame,
            _writer: &mut dyn std::io::Write,
        ) -> Result<(), RasterCodecError> {
            Err(RasterCodecError::SignatureMismatch { expected: "test" })
        }
    }

    #[test]
    fn read_stage_overlaps_two_detection_workers() {
        let test_id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "camera-toolbox-calibration-pipeline-{}-{test_id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let png = minimal_png();
        std::fs::write(root.join("b.png"), &png).unwrap();

        let source_id = FileSourceId::new(format!("pipeline-test-{test_id}")).unwrap();
        let file_system: Arc<dyn FileSystem> =
            Arc::new(LocalFileSystem::new(source_id.clone(), &root).unwrap());
        let b_reference = FileRef::new(source_id.clone(), SourcePath::new("b.png").unwrap());
        let b_entry = file_system
            .stat(
                &b_reference,
                &FsControl::with_timeout(Duration::from_secs(1)),
            )
            .unwrap();
        let a_reference = FileRef::new(source_id.clone(), SourcePath::new("a.png").unwrap());
        let a_version = FileVersion {
            size: png.len() as u64,
            modified_millis: Some(1),
        };
        let board = BoardSpec::new(2, 2, 1.0).unwrap();
        let mut session = CalibrationSession::new(board);
        let AddCalibrationItemOutcome::Added(a_id) =
            session.add_or_refresh(a_reference, a_version, "a.png".to_owned())
        else {
            panic!();
        };
        let AddCalibrationItemOutcome::Added(b_id) =
            session.add_or_refresh(b_reference.clone(), b_entry.version, "b.png".to_owned())
        else {
            panic!();
        };
        let a_token = session.begin_detection(a_id).unwrap();
        let b_token = session.begin_detection(b_id).unwrap();

        let gate = Arc::new(DetectionGate::new());
        let pipeline = CalibrationDetectionPipeline::new_with_components(
            &egui::Context::default(),
            Arc::new(BlockingBackend {
                gate: Arc::clone(&gate),
            }),
            Arc::new(RejectingPreviewCodec),
        )
        .unwrap();
        assert!(
            pipeline
                .try_submit_detection(LoadedDetectionJob {
                    batch_id: 1,
                    request: EncodedDetectionRequest::Dataset(a_token),
                    encoded: CalibrationEncodedPng {
                        bytes: png.clone().into(),
                        image_size: CalibrationImageSize::new(1, 1).unwrap(),
                        source_revision: a_version.clone().into(),
                    },
                    cancellation: CalibrationCancellation::default(),
                    reserved_bytes: a_version.size,
                    queued_at: Instant::now(),
                })
                .is_ok()
        );
        gate.wait_for_entered(1);

        assert!(
            pipeline
                .try_submit_read(ReadJob::new(
                    1,
                    b_token,
                    ReadSource {
                        source_id,
                        remote: false,
                    },
                    Arc::clone(&file_system),
                    b_reference,
                    FsCancellation::default(),
                    CalibrationCancellation::default(),
                ))
                .is_ok()
        );
        let read_result = wait_for_read_result(&pipeline);
        let loaded = read_result
            .result
            .expect("read must finish while A is detecting");
        assert!(pipeline.try_submit_detection(loaded).is_ok());
        gate.wait_for_entered(2);
        assert_eq!(gate.max_active.load(Ordering::Acquire), 2);

        gate.release();
        let results = wait_for_detection_results(&pipeline, 2);
        assert!(results.iter().all(|result| result.result.is_ok()));

        drop(pipeline);
        drop(file_system);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancelled_detection_job_reports_cancelled_without_entering_backend() {
        let test_id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let source_id = FileSourceId::new(format!("pipeline-cancel-test-{test_id}")).unwrap();
        let reference = FileRef::new(source_id, SourcePath::new("cancel.png").unwrap());
        let png = minimal_png();
        let version = FileVersion {
            size: png.len() as u64,
            modified_millis: Some(1),
        };
        let board = BoardSpec::new(2, 2, 1.0).unwrap();
        let mut session = CalibrationSession::new(board);
        let AddCalibrationItemOutcome::Added(item_id) =
            session.add_or_refresh(reference, version, "cancel.png".to_owned())
        else {
            panic!();
        };
        let token = session.begin_detection(item_id).unwrap();
        let gate = Arc::new(DetectionGate::new());
        let pipeline = CalibrationDetectionPipeline::new_with_components(
            &egui::Context::default(),
            Arc::new(BlockingBackend {
                gate: Arc::clone(&gate),
            }),
            Arc::new(RejectingPreviewCodec),
        )
        .unwrap();
        let cancellation = CalibrationCancellation::default();
        cancellation.cancel();
        assert!(
            pipeline
                .try_submit_detection(LoadedDetectionJob {
                    batch_id: 1,
                    request: EncodedDetectionRequest::Dataset(token),
                    encoded: CalibrationEncodedPng {
                        bytes: png.into(),
                        image_size: CalibrationImageSize::new(1, 1).unwrap(),
                        source_revision: version.clone().into(),
                    },
                    cancellation,
                    reserved_bytes: version.size,
                    queued_at: Instant::now(),
                })
                .is_ok()
        );

        let mut results = wait_for_detection_results(&pipeline, 1);
        let result = results.pop().expect("one detection result is expected");
        let Err(error) = result.result else {
            panic!("cancelled job unexpectedly succeeded");
        };
        assert!(error.is_cancelled());
        assert_eq!(gate.entered.load(Ordering::Acquire), 0);
    }

    fn wait_for_read_result(pipeline: &CalibrationDetectionPipeline) -> ReadStageResult {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut started = false;
        loop {
            match pipeline.try_read_event() {
                Ok(ReadStageEvent::Started { .. }) => {
                    assert!(!started, "read start event must be emitted once");
                    started = true;
                }
                Ok(ReadStageEvent::Finished(result)) => {
                    assert!(started, "read start event must precede completion");
                    return result;
                }
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("read event unavailable: {error:?}"),
            }
        }
    }

    fn wait_for_detection_results(
        pipeline: &CalibrationDetectionPipeline,
        expected_results: usize,
    ) -> Vec<DetectionStageResult> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut started = std::collections::HashSet::new();
        let mut results = Vec::with_capacity(expected_results);
        while results.len() < expected_results {
            match pipeline.try_detection_event() {
                Ok(DetectionStageEvent::Started {
                    request: EncodedDetectionRequest::Dataset(token),
                    ..
                }) => {
                    assert!(
                        started.insert(token.item_id),
                        "detection start event must be emitted once per item"
                    );
                }
                Ok(DetectionStageEvent::Started { .. }) => panic!("unexpected candidate request"),
                Ok(DetectionStageEvent::Finished(result)) => {
                    let EncodedDetectionRequest::Dataset(token) = &result.request else {
                        panic!("unexpected candidate request");
                    };
                    assert!(
                        started.contains(&token.item_id),
                        "detection start event must precede completion"
                    );
                    results.push(result);
                }
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("detection event unavailable: {error:?}"),
            }
        }
        results
    }

    fn minimal_png() -> Vec<u8> {
        let mut bytes = vec![0_u8; 24];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes[8..12].copy_from_slice(&13_u32.to_be_bytes());
        bytes[12..16].copy_from_slice(b"IHDR");
        bytes[16..20].copy_from_slice(&1_u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_be_bytes());
        bytes
    }
}
