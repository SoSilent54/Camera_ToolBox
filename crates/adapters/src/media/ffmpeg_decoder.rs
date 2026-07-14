//! 持久 FFmpeg sidecar：有界 Annex-B stdin，RGBA rawvideo stdout 与 capacity=1 latest slot。

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
};

use camera_toolbox_app::{LatestDecodedFrameSlot, StreamCancellation};
use thiserror::Error;

use crate::platforms::hisilicon_cv610::VideoCodec;

#[derive(Debug, Error)]
pub enum FfmpegDecoderError {
    #[error("FFmpeg executable is unavailable at {path}: {reason}")]
    Unavailable { path: String, reason: String },
    #[error("decoded frame dimensions overflow host memory")]
    FrameSizeOverflow,
    #[error("FFmpeg did not expose piped {0}")]
    MissingPipe(&'static str),
    #[error("failed to start FFmpeg decoder worker: {0}")]
    WorkerStart(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderOfferError {
    Full,
    Closed,
}

struct DecoderStatsInner {
    input_capacity: usize,
    input_depth: AtomicUsize,
    submitted: AtomicU64,
    dropped: AtomicU64,
    decoded: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecoderStats {
    pub input_depth: usize,
    pub submitted: u64,
    pub dropped: u64,
    pub decoded: u64,
}

/// Clone 只克隆 bounded sender 与 sidecar ownership；不会新建进程。
#[derive(Clone)]
pub struct FfmpegDecoder {
    input: Arc<Mutex<Option<SyncSender<Arc<[u8]>>>>>,
    stats: Arc<DecoderStatsInner>,
    child: Arc<Mutex<Child>>,
}

impl FfmpegDecoder {
    /// 自动路径优先 `CAMERA_TOOLBOX_FFMPEG`，随后使用 PATH 中的 `ffmpeg`。
    pub fn start(
        configured_path: Option<&Path>,
        codec: VideoCodec,
        width: u32,
        height: u32,
        input_capacity: usize,
        latest: Arc<LatestDecodedFrameSlot>,
        cancellation: &StreamCancellation,
    ) -> Result<Self, FfmpegDecoderError> {
        if input_capacity == 0 {
            return Err(FfmpegDecoderError::WorkerStart(
                "decoder input capacity must be positive".to_owned(),
            ));
        }
        let frame_bytes = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(FfmpegDecoderError::FrameSizeOverflow)?;
        let executable = configured_path.map_or_else(resolve_ffmpeg, PathBuf::from);
        let mut child = Command::new(&executable)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-fflags",
                "nobuffer",
                "-flags",
                "low_delay",
                "-f",
                codec.ffmpeg_input_format(),
                "-i",
                "pipe:0",
                "-an",
                "-sn",
                "-dn",
                "-pix_fmt",
                "rgba",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| FfmpegDecoderError::Unavailable {
                path: executable.display().to_string(),
                reason: error.to_string(),
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or(FfmpegDecoderError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(FfmpegDecoderError::MissingPipe("stdout"))?;
        let child = Arc::new(Mutex::new(child));
        let force_child = Arc::clone(&child);
        cancellation.register_force_cleanup(Arc::new(move || {
            let _ = force_child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .kill();
        }));

        let (input, receiver) = mpsc::sync_channel::<Arc<[u8]>>(input_capacity);
        let stats = Arc::new(DecoderStatsInner {
            input_capacity,
            input_depth: AtomicUsize::new(0),
            submitted: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            decoded: AtomicU64::new(0),
        });
        let input_stats = Arc::clone(&stats);
        std::thread::Builder::new()
            .name("cv610-ffmpeg-stdin".to_owned())
            .spawn(move || {
                let mut stdin = stdin;
                while let Ok(bytes) = receiver.recv() {
                    input_stats.input_depth.fetch_sub(1, Ordering::AcqRel);
                    if stdin.write_all(&bytes).is_err() || stdin.flush().is_err() {
                        break;
                    }
                }
                // Drop closes FFmpeg stdin; no guessed command or synchronous wait.
            })
            .map_err(|error| FfmpegDecoderError::WorkerStart(error.to_string()))?;

        let output_stats = Arc::clone(&stats);
        std::thread::Builder::new()
            .name("cv610-ffmpeg-stdout".to_owned())
            .spawn(move || {
                let mut stdout = stdout;
                loop {
                    let mut frame: Arc<[u8]> = vec![0_u8; frame_bytes].into();
                    let Some(buffer) = Arc::get_mut(&mut frame) else {
                        break;
                    };
                    if read_exact_or_eof(&mut stdout, buffer).is_err() {
                        break;
                    }
                    latest.publish(width, height, frame);
                    output_stats.decoded.fetch_add(1, Ordering::Relaxed);
                }
            })
            .map_err(|error| FfmpegDecoderError::WorkerStart(error.to_string()))?;

        Ok(Self {
            input: Arc::new(Mutex::new(Some(input))),
            stats,
            child,
        })
    }

    pub fn offer(&self, access_unit: Arc<[u8]>) -> Result<(), DecoderOfferError> {
        let mut depth = self.stats.input_depth.load(Ordering::Acquire);
        loop {
            if depth >= self.stats.input_capacity {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                return Err(DecoderOfferError::Full);
            }
            match self.stats.input_depth.compare_exchange_weak(
                depth,
                depth + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => depth = actual,
            }
        }
        let result = self
            .input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .ok_or(DecoderOfferError::Closed)
            .and_then(|input| match input.try_send(access_unit) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => Err(DecoderOfferError::Full),
                Err(TrySendError::Disconnected(_)) => Err(DecoderOfferError::Closed),
            });
        match result {
            Ok(()) => {
                self.stats.submitted.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(DecoderOfferError::Full) => {
                self.stats.input_depth.fetch_sub(1, Ordering::AcqRel);
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                Err(DecoderOfferError::Full)
            }
            Err(DecoderOfferError::Closed) => {
                self.stats.input_depth.fetch_sub(1, Ordering::AcqRel);
                Err(DecoderOfferError::Closed)
            }
        }
    }

    #[must_use]
    pub fn stats(&self) -> DecoderStats {
        DecoderStats {
            input_depth: self.stats.input_depth.load(Ordering::Acquire),
            submitted: self.stats.submitted.load(Ordering::Relaxed),
            dropped: self.stats.dropped.load(Ordering::Relaxed),
            decoded: self.stats.decoded.load(Ordering::Relaxed),
        }
    }

    /// 关闭 bounded stdin 队列；写线程排空后关闭子进程 stdin。
    pub fn close_input(&self) {
        self.input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// 非阻塞检查 sidecar 是否已经退出，供 session shutdown barrier 使用。
    #[must_use]
    pub fn is_exited(&self) -> bool {
        self.child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_wait()
            .is_ok_and(|status| status.is_some())
    }

    /// deadline 强制路径调用；普通 Stop 只通过关闭 socket 与 stdin 收尾。
    pub fn kill(&self) {
        let _ = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .kill();
    }
}

fn resolve_ffmpeg() -> PathBuf {
    std::env::var_os("CAMERA_TOOLBOX_FFMPEG")
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from("ffmpeg"), PathBuf::from)
}

fn read_exact_or_eof(reader: &mut impl Read, buffer: &mut [u8]) -> Result<(), ()> {
    let mut received = 0;
    while received < buffer.len() {
        match reader.read(&mut buffer[received..]) {
            Ok(0) => return Err(()),
            Ok(bytes) => received += bytes,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestTempDir(PathBuf);

    impl TestTempDir {
        fn new(label: &str) -> Self {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time must follow the Unix epoch")
                .as_nanos();
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "camera-toolbox-{label}-{}-{timestamp}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("unique test directory must be creatable");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_ffmpeg_is_a_typed_record_only_degradation() {
        let latest = Arc::new(LatestDecodedFrameSlot::default());
        let cancellation = StreamCancellation::default();
        let error = FfmpegDecoder::start(
            Some(Path::new("/definitely/not/a/camera-toolbox-ffmpeg")),
            VideoCodec::H265,
            2,
            2,
            1,
            latest,
            &cancellation,
        )
        .err()
        .expect("missing sidecar must fail");
        assert!(matches!(error, FfmpegDecoderError::Unavailable { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn slow_sidecar_keeps_decoder_input_bounded() {
        use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

        let root = TestTempDir::new("ffmpeg-test");
        let script = root.join("slow-ffmpeg.sh");
        fs::write(&script, "#!/bin/sh\nsleep 2\ncat >/dev/null\n").unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).unwrap();

        let latest = Arc::new(LatestDecodedFrameSlot::default());
        let cancellation = StreamCancellation::default();
        let decoder = FfmpegDecoder::start(
            Some(&script),
            VideoCodec::H265,
            2,
            2,
            1,
            latest,
            &cancellation,
        )
        .unwrap();
        let payload: Arc<[u8]> = vec![0_u8; 8 * 1024 * 1024].into();
        let mut observed_full = false;
        for _ in 0..64 {
            if decoder.offer(Arc::clone(&payload)) == Err(DecoderOfferError::Full) {
                observed_full = true;
                break;
            }
        }
        assert!(observed_full);
        assert!(decoder.stats().input_depth <= 1);
        let decoder = Arc::new(decoder);
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let decoder = Arc::clone(&decoder);
                std::thread::spawn(move || {
                    for _ in 0..1_000 {
                        let _ = decoder.offer(Arc::from(&b"x"[..]));
                        assert!(decoder.stats().input_depth <= 1);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(decoder.stats().input_depth <= 1);
        decoder.kill();
        std::thread::sleep(Duration::from_millis(10));
    }
    #[cfg(unix)]
    #[test]
    fn ignored_stdin_eof_requires_explicit_kill_before_shutdown_completes() {
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            time::{Duration, Instant},
        };

        let root = TestTempDir::new("ffmpeg-eof-test");
        let script = root.join("ignore-eof-ffmpeg.sh");
        fs::write(&script, "#!/bin/sh\nwhile :; do sleep 1; done\n").unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).unwrap();

        let decoder = FfmpegDecoder::start(
            Some(&script),
            VideoCodec::H264,
            2,
            2,
            1,
            Arc::new(LatestDecodedFrameSlot::default()),
            &StreamCancellation::default(),
        )
        .unwrap();
        decoder.close_input();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!decoder.is_exited(), "fixture must ignore stdin EOF");

        decoder.kill();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !decoder.is_exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(decoder.is_exited(), "kill must complete sidecar shutdown");
    }
}
