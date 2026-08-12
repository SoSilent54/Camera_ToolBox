//! SSH server host-key 阻塞操作的单请求后台 worker。

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use camera_toolbox_adapters::platforms::ssh_managed::{
    CredentialResolver, HostKeyAssessment, HostKeyManager, HostKeyTarget, HostKeyTrustOutcome,
    ProductionCredentialResolver, RusshTransportFactory, ServerHostKey, SshConnectionTarget,
    SshCredential, SshRemoteFileService, SshTransportFactory,
};
use camera_toolbox_app::{
    DumpCancellation, RemoteFileService, RemoteFileStat, RemoteOperationControl, RemoteTimeouts,
    SshManagedConfig,
};
use secrecy::SecretString;

const HOST_KEY_SCAN_TIMEOUT: Duration = Duration::from_secs(5);

const CONNECTION_TEST_CREDENTIAL_REF: &str = "session:gui-connection-test";

/// Secret-bearing connection-test authentication; intentionally not `Clone` or `Debug`.
#[allow(dead_code)]
pub(super) enum SshConnectionTestAuth {
    Password(SecretString),
    PrivateKeyFile(PathBuf),
}

/// Operation-owned connection-test input; intentionally not `Clone` or `Debug`.
#[allow(dead_code)]
pub(super) struct SshConnectionTestRequest {
    pub(super) generation: u64,
    pub(super) config: SshManagedConfig,
    pub(super) remote_path: String,
    pub(super) auth: SshConnectionTestAuth,
}

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
    ConnectionTest {
        request: SshConnectionTestRequest,
        endpoint: HostKeyTarget,
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

pub(super) struct SshConnectionTestResult {
    pub(super) generation: u64,
    pub(super) endpoint: HostKeyTarget,
    pub(super) result: Result<RemoteFileStat, String>,
}

pub(super) struct HostKeyWorker {
    request_tx: Option<Sender<HostKeyWorkerRequest>>,
    result_rx: Receiver<HostKeyWorkerResult>,
    connection_test_result_rx: Receiver<SshConnectionTestResult>,
    in_flight: bool,
    thread: Option<JoinHandle<()>>,
}

impl HostKeyWorker {
    #[cfg_attr(test, allow(dead_code))]
    pub(super) fn production() -> Result<Self, String> {
        let manager = HostKeyManager::production().map_err(|error| error.to_string())?;
        Self::start(manager, Arc::new(RusshTransportFactory))
    }

    #[cfg(test)]
    pub(super) fn with_manager(manager: HostKeyManager) -> Result<Self, String> {
        Self::start(manager, Arc::new(RusshTransportFactory))
    }

    #[cfg(test)]
    pub(super) fn with_manager_and_transport(
        manager: HostKeyManager,
        transport: Arc<dyn SshTransportFactory>,
    ) -> Result<Self, String> {
        Self::start(manager, transport)
    }

    fn start(
        manager: HostKeyManager,
        transport: Arc<dyn SshTransportFactory>,
    ) -> Result<Self, String> {
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let (connection_test_result_tx, connection_test_result_rx) = mpsc::channel();
        let worker_transport = Arc::clone(&transport);
        let thread = thread::Builder::new()
            .name("ssh-host-key-worker".to_owned())
            .spawn(move || {
                run_worker(
                    manager,
                    worker_transport,
                    request_rx,
                    result_tx,
                    connection_test_result_tx,
                );
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            request_tx: Some(request_tx),
            result_rx,
            connection_test_result_rx,
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

    pub(super) fn request_connection_test(
        &mut self,
        request: SshConnectionTestRequest,
    ) -> Result<(), String> {
        let endpoint = HostKeyTarget::new(request.config.host.clone(), request.config.port)
            .map_err(|error| error.to_string())?;
        self.submit(HostKeyWorkerRequest::ConnectionTest { request, endpoint })
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

    pub(super) fn try_recv_connection_test(&mut self) -> Option<SshConnectionTestResult> {
        match self.connection_test_result_rx.try_recv() {
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

    pub(super) fn is_in_flight(&self) -> bool {
        self.in_flight
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
    transport: Arc<dyn SshTransportFactory>,
    request_rx: Receiver<HostKeyWorkerRequest>,
    result_tx: Sender<HostKeyWorkerResult>,
    connection_test_result_tx: Sender<SshConnectionTestResult>,
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
            HostKeyWorkerRequest::ConnectionTest { request, endpoint } => {
                let generation = request.generation;
                let result = run_connection_test(request, Arc::clone(&transport));
                if connection_test_result_tx
                    .send(SshConnectionTestResult {
                        generation,
                        endpoint,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
                continue;
            }
        };
        if result_tx.send(result).is_err() {
            break;
        }
    }
}

fn run_connection_test(
    request: SshConnectionTestRequest,
    transport: Arc<dyn SshTransportFactory>,
) -> Result<RemoteFileStat, String> {
    let (credential_ref, resolver) = connection_test_resolver(request.auth)?;
    let config = request.config;
    let service = SshRemoteFileService::new(
        "gui-ssh-connection-test".to_owned(),
        SshConnectionTarget {
            host: config.host,
            port: config.port,
            username: config.username,
            expected_host_key: Some(config.expected_host_key),
            command_subsystem: config.command_subsystem,
            remote_event_subsystem: config.remote_event_subsystem,
        },
        credential_ref,
        resolver,
        transport,
        config.remote_artifact_dir,
        config.remote_artifact_glob,
        config.stable_samples,
        Duration::from_millis(config.stability_interval_ms),
        config.passive_watch_auto_open,
        config.max_fetch_bytes,
    );
    let control =
        RemoteOperationControl::new(RemoteTimeouts::default(), DumpCancellation::default())
            .map_err(|error| error.to_string())?;
    service
        .stat(&request.remote_path, control)
        .map_err(|error| error.to_string())
}

fn connection_test_resolver(
    auth: SshConnectionTestAuth,
) -> Result<(String, Arc<dyn CredentialResolver>), String> {
    match auth {
        SshConnectionTestAuth::Password(password) => Ok((
            CONNECTION_TEST_CREDENTIAL_REF.to_owned(),
            Arc::new(OneShotCredentialResolver::new(SshCredential::Password(
                password,
            ))),
        )),
        SshConnectionTestAuth::PrivateKeyFile(path) => {
            let path = path
                .to_str()
                .ok_or_else(|| "private-key file path is not valid UTF-8".to_owned())?;
            Ok((
                format!("key-file:{path}"),
                Arc::new(ProductionCredentialResolver::new()),
            ))
        }
    }
}

struct OneShotCredentialResolver {
    credential: Mutex<Option<SshCredential>>,
}

impl OneShotCredentialResolver {
    fn new(credential: SshCredential) -> Self {
        Self {
            credential: Mutex::new(Some(credential)),
        }
    }
}

impl CredentialResolver for OneShotCredentialResolver {
    fn resolve(&self, credential_ref: &str) -> Result<SshCredential, String> {
        if credential_ref != CONNECTION_TEST_CREDENTIAL_REF {
            return Err("connection-test credential reference is invalid".to_owned());
        }
        self.credential
            .lock()
            .map_err(|_| "connection-test credential is unavailable".to_owned())?
            .take()
            .ok_or_else(|| "connection-test credential was already consumed".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU8, AtomicU64, Ordering},
            mpsc::{self, Receiver, SyncSender},
        },
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use camera_toolbox_adapters::platforms::ssh_managed::{
        HostKeyError, MemoryRemoteFile, MemorySshTransport, ServerHostKeyProbe, SshTransportError,
        SshTransportSession,
    };

    use super::*;

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";
    const PASSWORD_AUTH: u8 = 1;
    const PRIVATE_KEY_AUTH: u8 = 2;
    const REMOTE_PATH: &str = "/data/frame.raw";

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

    #[derive(Clone)]
    struct ObservingTransport {
        memory: MemorySshTransport,
        observed_auth: Arc<AtomicU8>,
    }

    impl SshTransportFactory for ObservingTransport {
        fn connect(
            &self,
            target: &SshConnectionTarget,
            credential: SshCredential,
            control: &RemoteOperationControl,
        ) -> Result<Box<dyn SshTransportSession>, SshTransportError> {
            let kind = match &credential {
                SshCredential::Password(_) => PASSWORD_AUTH,
                SshCredential::PrivateKeyOpenSsh { .. } => PRIVATE_KEY_AUTH,
            };
            self.observed_auth.store(kind, Ordering::Release);
            self.memory.connect(target, credential, control)
        }
    }

    fn connection_test_config(expected_host_key: &str) -> SshManagedConfig {
        SshManagedConfig {
            host: "camera.example".to_owned(),
            port: 2222,
            username: "root".to_owned(),
            expected_host_key: expected_host_key.to_owned(),
            credential_ref: "unused-by-connection-test".to_owned(),
            command_subsystem: None,
            capture_recipe: String::new(),
            remote_artifact_dir: "/data".to_owned(),
            remote_artifact_glob: "frame.raw".to_owned(),
            remote_event_subsystem: None,
            passive_watch_auto_open: false,
            stable_samples: 2,
            stability_interval_ms: 500,
            max_fetch_bytes: 1024,
            command_output_bytes: 1024,
            rtsp: None,
        }
    }

    fn connection_test_stat() -> RemoteFileStat {
        RemoteFileStat {
            path: REMOTE_PATH.to_owned(),
            size: 4096,
            modified_seconds: 1_234,
            producer_marker: None,
            sha256: None,
        }
    }

    fn connection_test_worker(
        label: &str,
        memory: MemorySshTransport,
        observed_auth: Arc<AtomicU8>,
    ) -> (HostKeyWorker, KnownHostsFixture) {
        let fixture = KnownHostsFixture::new(label);
        let manager = HostKeyManager::with_probe(&fixture.path, Arc::new(ErrorProbe)).unwrap();
        let transport: Arc<dyn SshTransportFactory> = Arc::new(ObservingTransport {
            memory,
            observed_auth,
        });
        (
            HostKeyWorker::with_manager_and_transport(manager, transport).unwrap(),
            fixture,
        )
    }

    #[test]
    fn connection_test_password_uses_selected_auth_and_returns_sftp_stat() {
        let memory = MemorySshTransport::new(KEY_A);
        let expected_stat = connection_test_stat();
        memory.insert_file(
            REMOTE_PATH,
            MemoryRemoteFile {
                bytes: Vec::new(),
                stats: VecDeque::from([expected_stat.clone()]),
            },
        );
        let observed_auth = Arc::new(AtomicU8::new(0));
        let (mut worker, fixture) =
            connection_test_worker("connection-password", memory, Arc::clone(&observed_auth));

        worker
            .request_connection_test(SshConnectionTestRequest {
                generation: 51,
                config: connection_test_config(KEY_A),
                remote_path: REMOTE_PATH.to_owned(),
                auth: SshConnectionTestAuth::Password(SecretString::from(
                    "deterministic-test-password",
                )),
            })
            .unwrap();

        let SshConnectionTestResult {
            generation,
            endpoint,
            result: Ok(stat),
        } = recv_connection_test_result(&mut worker)
        else {
            panic!("unexpected connection-test result")
        };
        assert_eq!(generation, 51);
        assert_eq!(
            endpoint,
            HostKeyTarget::new("camera.example", 2222).unwrap()
        );
        assert_eq!(stat, expected_stat);
        assert_eq!(observed_auth.load(Ordering::Acquire), PASSWORD_AUTH);
        drop(worker);
        drop(fixture);
    }

    #[test]
    fn connection_test_preserves_strict_expected_host_key_pin() {
        let memory = MemorySshTransport::new(KEY_A);
        let observed_auth = Arc::new(AtomicU8::new(0));
        let (mut worker, fixture) =
            connection_test_worker("connection-host-pin", memory, observed_auth);

        worker
            .request_connection_test(SshConnectionTestRequest {
                generation: 52,
                config: connection_test_config(KEY_B),
                remote_path: REMOTE_PATH.to_owned(),
                auth: SshConnectionTestAuth::Password(SecretString::from("another-test-password")),
            })
            .unwrap();

        let SshConnectionTestResult {
            generation,
            endpoint,
            result: Err(error),
        } = recv_connection_test_result(&mut worker)
        else {
            panic!("unexpected connection-test host-key result")
        };
        assert_eq!(generation, 52);
        assert_eq!(
            endpoint,
            HostKeyTarget::new("camera.example", 2222).unwrap()
        );
        assert_eq!(error, "SSH host key mismatch");
        drop(worker);
        drop(fixture);
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

    fn recv_connection_test_result(worker: &mut HostKeyWorker) -> SshConnectionTestResult {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = worker.try_recv_connection_test() {
                return result;
            }
            assert!(
                Instant::now() < deadline,
                "connection-test result timed out"
            );
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
