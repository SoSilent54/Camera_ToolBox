//! 本地 native hint + reconciliation 与远端增量轮询目录监控。

use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use camera_toolbox_app::{
    DirectoryDelta, DirectoryDeltaCause, DirectoryRef, DirectorySnapshot, FileSystem,
    FileSystemError, FsCancellation, FsControl, ListContinuation, ListPageRequest,
};
use notify::{RecursiveMode, Watcher};

const DEFAULT_PAGE_ENTRIES: usize = 512;
const DEFAULT_MAX_SNAPSHOT_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryMonitorState {
    Connected,
    Stale,
    Stopped,
}

#[derive(Debug, Clone)]
pub enum DirectoryMonitorEvent {
    Baseline(DirectorySnapshot),
    Delta(DirectoryDelta),
    State(DirectoryMonitorState),
    Error(FileSystemError),
}

#[derive(Debug, Clone)]
pub struct DirectoryMonitorConfig {
    pub poll_interval: Duration,
    pub reconciliation_interval: Duration,
    pub reconnect_backoff_min: Duration,
    pub reconnect_backoff_max: Duration,
    pub operation_timeout: Duration,
    pub page_entries: usize,
    pub max_snapshot_entries: usize,
}

impl Default for DirectoryMonitorConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
            reconciliation_interval: Duration::from_secs(5),
            reconnect_backoff_min: Duration::from_secs(2),
            reconnect_backoff_max: Duration::from_secs(30),
            operation_timeout: Duration::from_secs(30),
            page_entries: DEFAULT_PAGE_ENTRIES,
            max_snapshot_entries: DEFAULT_MAX_SNAPSHOT_ENTRIES,
        }
    }
}

pub struct DirectoryMonitorHandle {
    cancellation: FsCancellation,
    events: Receiver<DirectoryMonitorEvent>,
    worker: Option<thread::JoinHandle<()>>,
}

impl DirectoryMonitorHandle {
    #[must_use]
    pub fn try_recv(&self) -> Option<DirectoryMonitorEvent> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<DirectoryMonitorEvent, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    pub fn stop(mut self) {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for DirectoryMonitorHandle {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub struct LocalDirectoryMonitor;

impl LocalDirectoryMonitor {
    /// 监听当前本地目录；native 事件只作提示，周期 reconciliation 负责最终一致性。
    #[must_use]
    pub fn spawn(
        file_system: Arc<dyn FileSystem>,
        directory: DirectoryRef,
        native_path: PathBuf,
        config: DirectoryMonitorConfig,
    ) -> DirectoryMonitorHandle {
        spawn_monitor(move |events, cancellation| {
            local_worker(
                file_system,
                directory,
                native_path,
                config,
                events,
                cancellation,
            );
        })
    }
}

pub struct RemoteDirectoryMonitor;

impl RemoteDirectoryMonitor {
    /// SFTP 无可移植 watch API；使用 snapshot diff、断线重连和指数退避。
    #[must_use]
    pub fn spawn(
        file_system: Arc<dyn FileSystem>,
        directory: DirectoryRef,
        config: DirectoryMonitorConfig,
    ) -> DirectoryMonitorHandle {
        spawn_monitor(move |events, cancellation| {
            remote_worker(file_system, directory, config, events, cancellation);
        })
    }
}

fn spawn_monitor(
    worker: impl FnOnce(SyncSender<DirectoryMonitorEvent>, FsCancellation) + Send + 'static,
) -> DirectoryMonitorHandle {
    let cancellation = FsCancellation::default();
    let worker_cancellation = cancellation.clone();
    let (sender, events) = mpsc::sync_channel(256);
    let handle = thread::Builder::new()
        .name("camera-toolbox-directory-monitor".to_owned())
        .spawn(move || worker(sender, worker_cancellation))
        .expect("directory monitor thread must start");
    DirectoryMonitorHandle {
        cancellation,
        events,
        worker: Some(handle),
    }
}

#[derive(Debug)]
enum NativeHint {
    Changed,
    Error(String),
}

fn local_worker(
    file_system: Arc<dyn FileSystem>,
    directory: DirectoryRef,
    native_path: PathBuf,
    config: DirectoryMonitorConfig,
    events: SyncSender<DirectoryMonitorEvent>,
    cancellation: FsCancellation,
) {
    let (hint_sender, hint_receiver) = mpsc::sync_channel(1);
    let mut watcher =
        match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let hint = event.map_or_else(
                |error| NativeHint::Error(error.to_string()),
                |_| NativeHint::Changed,
            );
            let _ = hint_sender.try_send(hint);
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                try_emit(
                    &events,
                    DirectoryMonitorEvent::Error(FileSystemError::Io(error.to_string())),
                );
                return;
            }
        };
    if let Err(error) = watcher.watch(&native_path, RecursiveMode::NonRecursive) {
        try_emit(
            &events,
            DirectoryMonitorEvent::Error(FileSystemError::Io(error.to_string())),
        );
        return;
    }

    let mut sequence = 0_u64;
    let mut pending_error = None;
    let mut snapshot: Option<DirectorySnapshot> =
        match read_snapshot(&*file_system, &directory, &config, &cancellation) {
            Ok(initial) => {
                let installed = try_emit(&events, DirectoryMonitorEvent::Baseline(initial.clone()));
                installed.then_some(initial)
            }
            Err(error) => {
                retain_latest_error(&events, &mut pending_error, error);
                None
            }
        };
    let mut reconciliation_at = Instant::now() + config.reconciliation_interval;
    while !cancellation.is_cancelled() {
        flush_pending_error(&events, &mut pending_error);
        let timeout = reconciliation_at.saturating_duration_since(Instant::now());
        let cause = match hint_receiver.recv_timeout(timeout) {
            Ok(NativeHint::Changed) => DirectoryDeltaCause::NativeHint,
            Ok(NativeHint::Error(error)) => {
                retain_latest_error(&events, &mut pending_error, FileSystemError::Io(error));
                DirectoryDeltaCause::Reconciliation
            }
            Err(RecvTimeoutError::Timeout) => DirectoryDeltaCause::Reconciliation,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if cancellation.is_cancelled() {
            break;
        }
        if cause == DirectoryDeltaCause::Reconciliation {
            reconciliation_at = Instant::now() + config.reconciliation_interval;
        }
        match read_snapshot(&*file_system, &directory, &config, &cancellation) {
            Ok(next) => {
                if let Some(previous) = &snapshot {
                    let changes = previous.diff(&next);
                    if changes.is_empty() {
                        snapshot = Some(next);
                    } else {
                        let next_sequence = sequence.wrapping_add(1);
                        if try_emit(
                            &events,
                            DirectoryMonitorEvent::Delta(DirectoryDelta {
                                directory: directory.clone(),
                                sequence: next_sequence,
                                cause,
                                changes,
                            }),
                        ) {
                            sequence = next_sequence;
                            snapshot = Some(next);
                        }
                    }
                } else if try_emit(&events, DirectoryMonitorEvent::Baseline(next.clone())) {
                    snapshot = Some(next);
                }
            }
            Err(FileSystemError::Cancelled) => break,
            Err(error) => {
                retain_latest_error(&events, &mut pending_error, error);
            }
        }
    }
    try_emit(
        &events,
        DirectoryMonitorEvent::State(DirectoryMonitorState::Stopped),
    );
}

fn remote_worker(
    file_system: Arc<dyn FileSystem>,
    directory: DirectoryRef,
    config: DirectoryMonitorConfig,
    events: SyncSender<DirectoryMonitorEvent>,
    cancellation: FsCancellation,
) {
    let mut sequence = 0_u64;
    let mut snapshot: Option<DirectorySnapshot> = None;
    let mut announced_state = None;
    let mut backoff = config.reconnect_backoff_min;
    let mut pending_error = None;
    while !cancellation.is_cancelled() {
        flush_pending_error(&events, &mut pending_error);
        match read_snapshot(&*file_system, &directory, &config, &cancellation) {
            Ok(next) => {
                backoff = config.reconnect_backoff_min;
                if snapshot.is_some() && announced_state != Some(DirectoryMonitorState::Connected) {
                    if !try_emit(
                        &events,
                        DirectoryMonitorEvent::State(DirectoryMonitorState::Connected),
                    ) {
                        if wait_cancelled(&cancellation, config.poll_interval) {
                            break;
                        }
                        continue;
                    }
                    announced_state = Some(DirectoryMonitorState::Connected);
                }
                if let Some(previous) = &snapshot {
                    let changes = previous.diff(&next);
                    if changes.is_empty() {
                        snapshot = Some(next);
                    } else {
                        let next_sequence = sequence.wrapping_add(1);
                        if try_emit(
                            &events,
                            DirectoryMonitorEvent::Delta(DirectoryDelta {
                                directory: directory.clone(),
                                sequence: next_sequence,
                                cause: DirectoryDeltaCause::RemotePoll,
                                changes,
                            }),
                        ) {
                            sequence = next_sequence;
                            snapshot = Some(next);
                        }
                    }
                } else if try_emit(&events, DirectoryMonitorEvent::Baseline(next.clone())) {
                    snapshot = Some(next);
                }
                if snapshot.is_some()
                    && announced_state != Some(DirectoryMonitorState::Connected)
                    && try_emit(
                        &events,
                        DirectoryMonitorEvent::State(DirectoryMonitorState::Connected),
                    )
                {
                    announced_state = Some(DirectoryMonitorState::Connected);
                }
                if wait_cancelled(&cancellation, config.poll_interval) {
                    break;
                }
            }
            Err(FileSystemError::Cancelled) => break,
            Err(error) => {
                if announced_state != Some(DirectoryMonitorState::Stale)
                    && try_emit(
                        &events,
                        DirectoryMonitorEvent::State(DirectoryMonitorState::Stale),
                    )
                {
                    announced_state = Some(DirectoryMonitorState::Stale);
                }
                retain_latest_error(&events, &mut pending_error, error);
                if wait_cancelled(&cancellation, backoff) {
                    break;
                }
                backoff = backoff.saturating_mul(2).min(config.reconnect_backoff_max);
            }
        }
    }
    try_emit(
        &events,
        DirectoryMonitorEvent::State(DirectoryMonitorState::Stopped),
    );
}

fn try_emit(events: &SyncSender<DirectoryMonitorEvent>, event: DirectoryMonitorEvent) -> bool {
    match events.try_send(event) {
        Ok(()) => true,
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
    }
}

fn retain_latest_error(
    events: &SyncSender<DirectoryMonitorEvent>,
    pending: &mut Option<FileSystemError>,
    error: FileSystemError,
) {
    *pending = Some(error);
    flush_pending_error(events, pending);
}

fn flush_pending_error(
    events: &SyncSender<DirectoryMonitorEvent>,
    pending: &mut Option<FileSystemError>,
) {
    if pending
        .as_ref()
        .is_some_and(|error| try_emit(events, DirectoryMonitorEvent::Error(error.clone())))
    {
        *pending = None;
    }
}

fn read_snapshot(
    file_system: &dyn FileSystem,
    directory: &DirectoryRef,
    config: &DirectoryMonitorConfig,
    cancellation: &FsCancellation,
) -> Result<DirectorySnapshot, FileSystemError> {
    let control = FsControl::with_timeout(config.operation_timeout);
    let bridge = control.cancellation.clone();
    cancellation.register_interrupt(Arc::new(move || bridge.cancel()));
    let _registration = CancellationRegistration(cancellation);
    let mut continuation: Option<ListContinuation> = None;
    let mut seen = BTreeSet::new();
    let mut entries = Vec::new();
    loop {
        let page = file_system.list(
            directory,
            ListPageRequest {
                continuation,
                limit: config.page_entries.max(1),
            },
            &control,
        )?;
        entries.extend(page.entries);
        if entries.len() > config.max_snapshot_entries {
            return Err(FileSystemError::DirectoryLimitExceeded {
                observed: entries.len(),
                limit: config.max_snapshot_entries,
            });
        }
        let Some(next) = page.next else {
            return Ok(DirectorySnapshot::new(entries));
        };
        if !seen.insert(next.as_str().to_owned()) {
            return Err(FileSystemError::InvalidContinuation);
        }
        continuation = Some(next);
    }
}

struct CancellationRegistration<'a>(&'a FsCancellation);

impl Drop for CancellationRegistration<'_> {
    fn drop(&mut self) {
        self.0.clear_interrupt();
    }
}

fn wait_cancelled(cancellation: &FsCancellation, duration: Duration) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if cancellation.is_cancelled() {
            return true;
        }
        let step = remaining.min(Duration::from_millis(50));
        thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
    cancellation.is_cancelled()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::LocalFileSystem;
    use camera_toolbox_app::{
        EntryName, FileEntry, FileRef, FileSourceId, FileSystemCapabilities, ListPage, ReadOutcome,
        ReadRequest,
    };
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FailOnceFileSystem {
        inner: LocalFileSystem,
        fail: AtomicBool,
    }

    impl FileSystem for FailOnceFileSystem {
        fn source_id(&self) -> &FileSourceId {
            self.inner.source_id()
        }
        fn capabilities(&self) -> FileSystemCapabilities {
            self.inner.capabilities()
        }
        fn list(
            &self,
            directory: &DirectoryRef,
            page: ListPageRequest,
            control: &FsControl,
        ) -> Result<ListPage, FileSystemError> {
            if self.fail.swap(false, Ordering::AcqRel) {
                Err(FileSystemError::Disconnected("injected".to_owned()))
            } else {
                self.inner.list(directory, page, control)
            }
        }
        fn stat(
            &self,
            reference: &FileRef,
            control: &FsControl,
        ) -> Result<FileEntry, FileSystemError> {
            self.inner.stat(reference, control)
        }
        fn read(
            &self,
            reference: &FileRef,
            request: ReadRequest,
            control: &FsControl,
            consume: &mut dyn FnMut(&[u8]) -> Result<(), FileSystemError>,
        ) -> Result<ReadOutcome, FileSystemError> {
            self.inner.read(reference, request, control, consume)
        }
        fn mkdir(
            &self,
            parent: &DirectoryRef,
            name: &EntryName,
            control: &FsControl,
        ) -> Result<DirectoryRef, FileSystemError> {
            self.inner.mkdir(parent, name, control)
        }
        fn rename(
            &self,
            reference: &FileRef,
            name: &EntryName,
            control: &FsControl,
        ) -> Result<FileRef, FileSystemError> {
            self.inner.rename(reference, name, control)
        }
        fn move_entry(
            &self,
            reference: &FileRef,
            destination: &DirectoryRef,
            control: &FsControl,
        ) -> Result<FileRef, FileSystemError> {
            self.inner.move_entry(reference, destination, control)
        }
        fn delete(
            &self,
            reference: &FileRef,
            recursive: bool,
            control: &FsControl,
        ) -> Result<(), FileSystemError> {
            self.inner.delete(reference, recursive, control)
        }
    }

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("camera-toolbox-monitor-{nonce}"));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fast_config() -> DirectoryMonitorConfig {
        DirectoryMonitorConfig {
            poll_interval: Duration::from_millis(20),
            reconciliation_interval: Duration::from_millis(20),
            reconnect_backoff_min: Duration::from_millis(20),
            reconnect_backoff_max: Duration::from_millis(40),
            operation_timeout: Duration::from_secs(2),
            page_entries: 1,
            max_snapshot_entries: 32,
        }
    }

    #[test]
    fn local_initial_failure_recovers_as_baseline_not_created_delta() {
        let root = TestRoot::new();
        fs::write(root.0.join("existing.raw"), [1_u8]).unwrap();
        let source_id = FileSourceId::new("local-monitor-test").unwrap();
        let file_system = Arc::new(FailOnceFileSystem {
            inner: LocalFileSystem::new(source_id.clone(), &root.0).unwrap(),
            fail: AtomicBool::new(true),
        });
        let monitor = LocalDirectoryMonitor::spawn(
            file_system,
            DirectoryRef::root(source_id),
            root.0.clone(),
            fast_config(),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut baseline_seen = false;
        while Instant::now() < deadline {
            match monitor.recv_timeout(Duration::from_millis(100)) {
                Ok(DirectoryMonitorEvent::Baseline(snapshot)) => {
                    assert_eq!(snapshot.entries().count(), 1);
                    baseline_seen = true;
                    break;
                }
                Ok(DirectoryMonitorEvent::Delta(_)) => {
                    panic!("existing entries must not become created deltas")
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(baseline_seen);
        monitor.stop();
    }

    #[test]
    fn remote_poll_preserves_baseline_and_emits_later_create() {
        let root = TestRoot::new();
        fs::write(root.0.join("existing.raw"), [1_u8]).unwrap();
        let source_id = camera_toolbox_app::FileSourceId::new("poll-test").unwrap();
        let file_system = Arc::new(LocalFileSystem::new(source_id.clone(), &root.0).unwrap());
        let monitor = RemoteDirectoryMonitor::spawn(
            file_system,
            DirectoryRef::root(source_id),
            fast_config(),
        );
        assert!(matches!(
            monitor.recv_timeout(Duration::from_secs(2)).unwrap(),
            DirectoryMonitorEvent::Baseline(_)
        ));
        let _ = monitor.recv_timeout(Duration::from_secs(2)).unwrap();
        fs::write(root.0.join("new.raw"), [2_u8]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut created = false;
        while Instant::now() < deadline {
            if let Ok(DirectoryMonitorEvent::Delta(delta)) =
                monitor.recv_timeout(Duration::from_millis(100))
            {
                created = delta.changes.iter().any(|change| {
                    matches!(change, camera_toolbox_app::DirectoryChange::Created(entry)
                        if entry.name.as_str() == "new.raw")
                });
                if created {
                    break;
                }
            }
        }
        assert!(created);
        monitor.stop();
    }
}
