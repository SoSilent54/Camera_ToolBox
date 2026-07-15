//! SSH server host-key 阻塞操作的单请求后台 worker。

use std::{
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread::{self, JoinHandle},
    time::Duration,
};

use camera_toolbox_adapters::platforms::ssh_managed::{
    HostKeyAssessment, HostKeyManager, HostKeyTarget, HostKeyTrustOutcome, ServerHostKey,
};

const HOST_KEY_SCAN_TIMEOUT: Duration = Duration::from_secs(5);

enum HostKeyWorkerRequest {
    Scan {
        generation: u64,
        target: HostKeyTarget,
    },
    Trust {
        generation: u64,
        target: HostKeyTarget,
        key: ServerHostKey,
    },
}

pub(super) enum HostKeyWorkerResult {
    Scan {
        generation: u64,
        target: HostKeyTarget,
        result: Result<HostKeyAssessment, String>,
    },
    Trust {
        generation: u64,
        target: HostKeyTarget,
        key: ServerHostKey,
        result: Result<HostKeyTrustOutcome, String>,
    },
}

pub(super) struct HostKeyWorker {
    request_tx: Option<Sender<HostKeyWorkerRequest>>,
    result_rx: Receiver<HostKeyWorkerResult>,
    in_flight: bool,
    thread: Option<JoinHandle<()>>,
}

impl HostKeyWorker {
    #[cfg_attr(test, allow(dead_code))]
    pub(super) fn production() -> Result<Self, String> {
        let manager = HostKeyManager::production().map_err(|error| error.to_string())?;
        Self::start(manager)
    }

    #[cfg(test)]
    pub(super) fn with_manager(manager: HostKeyManager) -> Result<Self, String> {
        Self::start(manager)
    }

    fn start(manager: HostKeyManager) -> Result<Self, String> {
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("ssh-host-key-worker".to_owned())
            .spawn(move || run_worker(manager, request_rx, result_tx))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            request_tx: Some(request_tx),
            result_rx,
            in_flight: false,
            thread: Some(thread),
        })
    }

    pub(super) fn request_scan(
        &mut self,
        generation: u64,
        target: HostKeyTarget,
    ) -> Result<(), String> {
        self.submit(HostKeyWorkerRequest::Scan { generation, target })
    }

    pub(super) fn request_trust(
        &mut self,
        generation: u64,
        target: HostKeyTarget,
        key: ServerHostKey,
    ) -> Result<(), String> {
        self.submit(HostKeyWorkerRequest::Trust {
            generation,
            target,
            key,
        })
    }

    pub(super) fn try_recv(&mut self) -> Option<HostKeyWorkerResult> {
        match self.result_rx.try_recv() {
            Ok(result) => {
                self.in_flight = false;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.request_tx.take();
                self.in_flight = false;
                None
            }
        }
    }

    fn submit(&mut self, request: HostKeyWorkerRequest) -> Result<(), String> {
        if self.in_flight {
            return Err("a host-key request is already in flight".to_owned());
        }
        let Some(request_tx) = self.request_tx.as_ref() else {
            return Err("host-key worker is unavailable".to_owned());
        };
        request_tx.send(request).map_err(|_| {
            self.in_flight = false;
            "host-key worker is unavailable".to_owned()
        })?;
        self.in_flight = true;
        Ok(())
    }
}

impl Drop for HostKeyWorker {
    fn drop(&mut self) {
        // 对话框/应用关闭时不得等待最长 5 秒的 handshake；运行中的线程自然脱离。
        self.request_tx.take();
        if !self.in_flight
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

fn run_worker(
    manager: HostKeyManager,
    request_rx: Receiver<HostKeyWorkerRequest>,
    result_tx: Sender<HostKeyWorkerResult>,
) {
    while let Ok(request) = request_rx.recv() {
        let result = match request {
            HostKeyWorkerRequest::Scan { generation, target } => {
                let result = manager
                    .scan_and_assess(&target, HOST_KEY_SCAN_TIMEOUT)
                    .map_err(|error| error.to_string());
                HostKeyWorkerResult::Scan {
                    generation,
                    target,
                    result,
                }
            }
            HostKeyWorkerRequest::Trust {
                generation,
                target,
                key,
            } => {
                let result = manager
                    .trust(&target, &key)
                    .map_err(|error| error.to_string());
                HostKeyWorkerResult::Trust {
                    generation,
                    target,
                    key,
                    result,
                }
            }
        };
        if result_tx.send(result).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
            mpsc::{self, Receiver, SyncSender},
        },
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use camera_toolbox_adapters::platforms::ssh_managed::{HostKeyError, ServerHostKeyProbe};

    use super::*;

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";

    struct ControlledProbe {
        key: ServerHostKey,
        calls: SyncSender<(HostKeyTarget, Duration)>,
        release: Mutex<Receiver<()>>,
    }

    impl ServerHostKeyProbe for ControlledProbe {
        fn probe(
            &self,
            target: &HostKeyTarget,
            timeout: Duration,
        ) -> Result<ServerHostKey, HostKeyError> {
            self.calls.send((target.clone(), timeout)).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            Ok(self.key.clone())
        }
    }

    struct ErrorProbe;

    impl ServerHostKeyProbe for ErrorProbe {
        fn probe(
            &self,
            _target: &HostKeyTarget,
            _timeout: Duration,
        ) -> Result<ServerHostKey, HostKeyError> {
            Err(HostKeyError::Probe("deterministic failure".to_owned()))
        }
    }

    struct PanickingProbe {
        entered: SyncSender<()>,
    }

    impl ServerHostKeyProbe for PanickingProbe {
        fn probe(
            &self,
            _target: &HostKeyTarget,
            _timeout: Duration,
        ) -> Result<ServerHostKey, HostKeyError> {
            self.entered.send(()).unwrap();
            panic!("deterministic worker disconnect");
        }
    }

    #[test]
    fn scan_submission_is_nonblocking_bounded_and_single_flight() {
        let fixture = KnownHostsFixture::new("scan");
        let target = HostKeyTarget::new("camera.example", 2222).unwrap();
        let key = ServerHostKey::from_openssh(KEY_A).unwrap();
        let (call_tx, call_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let manager = HostKeyManager::with_probe(
            &fixture.path,
            Arc::new(ControlledProbe {
                key,
                calls: call_tx,
                release: Mutex::new(release_rx),
            }),
        )
        .unwrap();
        let mut worker = HostKeyWorker::with_manager(manager).unwrap();
        let caller_target = target.clone();
        let (submission_tx, submission_rx) = mpsc::sync_channel(1);
        let caller = std::thread::spawn(move || {
            let submitted = worker.request_scan(17, caller_target);
            submission_tx.send((worker, submitted)).unwrap();
        });
        let submission = submission_rx.recv_timeout(Duration::from_secs(1));
        let (mut worker, submitted) = match submission {
            Ok(submission) => submission,
            Err(error) => {
                let _ = release_tx.send(());
                let _ = caller.join();
                panic!("host-key request submission blocked: {error}");
            }
        };
        caller.join().unwrap();
        submitted.unwrap();
        assert_eq!(
            call_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (target.clone(), Duration::from_secs(5))
        );
        assert!(worker.request_scan(18, target.clone()).is_err());

        release_tx.send(()).unwrap();
        match recv_result(&mut worker) {
            HostKeyWorkerResult::Scan {
                generation,
                target: returned_target,
                result: Ok(HostKeyAssessment::Unknown(returned_key)),
            } => {
                assert_eq!(generation, 17);
                assert_eq!(returned_target, target);
                assert_eq!(returned_key.openssh(), KEY_A);
            }
            _ => panic!("unexpected scan result"),
        }
        drop(worker);
        drop(fixture);
    }

    #[test]
    fn trust_and_scan_errors_round_trip_without_network() {
        let fixture = KnownHostsFixture::new("results");
        let target = HostKeyTarget::new("camera.example", 22).unwrap();
        let key = ServerHostKey::from_openssh(KEY_A).unwrap();
        let manager = HostKeyManager::with_probe(&fixture.path, Arc::new(ErrorProbe)).unwrap();
        let mut worker = HostKeyWorker::with_manager(manager).unwrap();

        worker
            .request_trust(31, target.clone(), key.clone())
            .unwrap();
        assert!(
            worker
                .request_trust(32, target.clone(), key.clone())
                .is_err()
        );
        match recv_result(&mut worker) {
            HostKeyWorkerResult::Trust {
                generation,
                target: returned_target,
                key: returned_key,
                result,
            } => {
                assert_eq!(generation, 31);
                assert_eq!(returned_target, target);
                assert_eq!(returned_key, key);
                assert_eq!(result.unwrap(), HostKeyTrustOutcome::Added);
            }
            _ => panic!("unexpected trust result"),
        }

        worker.request_scan(33, target.clone()).unwrap();
        match recv_result(&mut worker) {
            HostKeyWorkerResult::Scan {
                generation,
                target: returned_target,
                result,
            } => {
                assert_eq!(generation, 33);
                assert_eq!(returned_target, target);
                assert!(result.unwrap_err().contains("deterministic failure"));
            }
            _ => panic!("unexpected scan error result"),
        }
        drop(worker);
        drop(fixture);
    }

    #[test]
    fn result_disconnect_clears_in_flight_state() {
        let fixture = KnownHostsFixture::new("disconnect");
        let target = HostKeyTarget::new("camera.example", 22).unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let manager = HostKeyManager::with_probe(
            &fixture.path,
            Arc::new(PanickingProbe {
                entered: entered_tx,
            }),
        )
        .unwrap();
        let mut worker = HostKeyWorker::with_manager(manager).unwrap();

        worker.request_scan(41, target.clone()).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let _ = worker.try_recv();
            match worker.request_scan(42, target.clone()) {
                Err(error) if error == "host-key worker is unavailable" => break,
                Err(error) if error == "a host-key request is already in flight" => {}
                other => panic!("unexpected request state after disconnect: {other:?}"),
            }
            assert!(
                Instant::now() < deadline,
                "worker channel did not disconnect"
            );
            std::thread::yield_now();
        }
        drop(worker);
        drop(fixture);
    }

    fn recv_result(worker: &mut HostKeyWorker) -> HostKeyWorkerResult {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = worker.try_recv() {
                return result;
            }
            assert!(Instant::now() < deadline, "worker result timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    struct KnownHostsFixture {
        directory: PathBuf,
        path: PathBuf,
    }

    impl KnownHostsFixture {
        fn new(label: &str) -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let unique = format!(
                "camera-toolbox-host-key-worker-{label}-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            );
            let directory = std::env::temp_dir().join(unique);
            fs::create_dir_all(&directory).unwrap();
            let directory = fs::canonicalize(directory).unwrap();
            let path = directory.join("known_hosts");
            Self { directory, path }
        }
    }

    impl Drop for KnownHostsFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}
