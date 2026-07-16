//! Explorer SFTP 会话登录；端点、密码和连接只存在于当前进程。

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use camera_toolbox_adapters::platforms::ssh_managed::{
    SshConnectionTarget, SshCredential, SshTransportFactory,
};
use camera_toolbox_app::{
    DumpCancellation, RemoteAuthentication, RemoteConnectionConfig, RemoteConnectionId,
    RemoteOperationControl, RemoteTimeouts,
};
use eframe::egui;
use secrecy::{ExposeSecret, ExposeSecretMut, SecretBox, SecretString};

const REMOTE_CONNECT_WATCHDOG: Duration = Duration::from_secs(30);

pub(super) const SFTP_NAMESPACE_ROOT: &str = "/";

pub(crate) struct RemoteConnectionCommit {
    pub(crate) config: RemoteConnectionConfig,
    pub(crate) session_password: SecretString,
}

#[derive(Default)]
struct RemoteConnectionDraft {
    endpoint: String,
    password: SecretBox<String>,
}

impl RemoteConnectionDraft {
    fn connect_request(&self) -> Result<RemoteConnectRequest, String> {
        let endpoint = parse_endpoint(&self.endpoint)?;
        if self.password.expose_secret().is_empty() {
            return Err("Password must not be empty".to_owned());
        }
        let id = remote_connection_id(&endpoint.host, endpoint.port, &endpoint.username)?;
        let slot_id = format!("credential-{}", id.as_str());
        Ok(RemoteConnectRequest {
            id,
            slot_id,
            host: endpoint.host,
            port: endpoint.port,
            username: endpoint.username,
            password: SecretString::from(self.password.expose_secret().clone()),
        })
    }

    fn clear_password(&mut self) {
        self.password.expose_secret_mut().clear();
    }
}

struct ParsedEndpoint {
    username: String,
    host: String,
    port: u16,
}

struct RemoteConnectRequest {
    id: RemoteConnectionId,
    slot_id: String,
    host: String,
    port: u16,
    username: String,
    password: SecretString,
}

struct RemoteConnectResult {
    generation: u64,
    result: Result<RemoteConnectionCommit, String>,
}

struct ActiveRemoteConnect {
    generation: u64,
    started_at: Instant,
    cancellation: DumpCancellation,
    receiver: Receiver<RemoteConnectResult>,
}

pub(super) struct RemoteConnector {
    draft: RemoteConnectionDraft,
    transport: Arc<dyn SshTransportFactory>,
    status: Option<String>,
    error: Option<String>,
    next_generation: u64,
    active: Option<ActiveRemoteConnect>,
}

impl RemoteConnector {
    pub(super) fn new(transport: Arc<dyn SshTransportFactory>) -> Self {
        Self {
            draft: RemoteConnectionDraft::default(),
            transport,
            status: None,
            error: None,
            next_generation: 1,
            active: None,
        }
    }

    pub(super) fn render(
        &mut self,
        context: &egui::Context,
        ui: &mut egui::Ui,
        enabled: bool,
    ) -> Option<RemoteConnectionCommit> {
        let completed = self.poll(context);
        let connecting = self.active.is_some();

        ui.label("Endpoint");
        ui.add_enabled(
            enabled && !connecting,
            egui::TextEdit::singleline(&mut self.draft.endpoint)
                .hint_text("user@192.168.1.100:22")
                .desired_width(ui.available_width()),
        );
        ui.label("Password");
        ui.add_enabled(
            enabled && !connecting,
            egui::TextEdit::singleline(self.draft.password.expose_secret_mut())
                .password(true)
                .desired_width(ui.available_width()),
        );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(enabled && !connecting, egui::Button::new("Connect"))
                .clicked()
            {
                self.start(context);
            }
            if ui
                .add_enabled(connecting, egui::Button::new("Cancel"))
                .clicked()
            {
                self.cancel();
                self.status = None;
                self.error = None;
            }
        });
        ui.small("Host identity is not verified or saved. Use this only on a trusted network.");
        if let Some(status) = &self.status {
            ui.horizontal(|ui| {
                if connecting {
                    ui.spinner();
                }
                ui.small(status);
            });
        }
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::RED, error);
        }
        completed
    }

    pub(super) fn mark_connected(&mut self) {
        self.draft.clear_password();
        self.status = Some("Connected; browsing server filesystem".to_owned());
        self.error = None;
    }

    pub(super) fn set_error(&mut self, error: impl Into<String>) {
        self.cancel();
        self.status = None;
        self.error = Some(error.into());
    }

    fn start(&mut self, context: &egui::Context) {
        if self.active.is_some() {
            return;
        }
        let request = match self.draft.connect_request() {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let cancellation = DumpCancellation::default();
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = mpsc::channel();
        self.error = None;
        self.status = Some("Connecting to the remote device…".to_owned());
        self.active = Some(ActiveRemoteConnect {
            generation,
            started_at: Instant::now(),
            cancellation,
            receiver,
        });
        context.request_repaint_after(REMOTE_CONNECT_WATCHDOG);
        let transport = Arc::clone(&self.transport);
        let context = context.clone();
        thread::spawn(move || {
            let result =
                run_remote_connect_guarded(request, transport.as_ref(), worker_cancellation);
            let _ = sender.send(RemoteConnectResult { generation, result });
            context.request_repaint();
        });
    }

    fn cancel(&mut self) {
        if let Some(active) = self.active.take() {
            active.cancellation.cancel();
        }
    }

    fn poll(&mut self, context: &egui::Context) -> Option<RemoteConnectionCommit> {
        let outcome = {
            let active = self.active.as_ref()?;
            let elapsed = active.started_at.elapsed();
            match active.receiver.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) if elapsed >= REMOTE_CONNECT_WATCHDOG => {
                    Some(Err(format!(
                        "Remote connection timed out after {} seconds",
                        REMOTE_CONNECT_WATCHDOG.as_secs()
                    )))
                }
                Err(TryRecvError::Empty) => {
                    context.request_repaint_after(REMOTE_CONNECT_WATCHDOG.saturating_sub(elapsed));
                    None
                }
                Err(TryRecvError::Disconnected) => Some(Err(
                    "Remote connection worker stopped unexpectedly".to_owned(),
                )),
            }
        }?;
        let active = self
            .active
            .take()
            .expect("active connection exists while polling its result");
        match outcome {
            Ok(result) if result.generation == active.generation => match result.result {
                Ok(commit) => {
                    self.status = Some("Remote device connected; opening directory…".to_owned());
                    self.error = None;
                    Some(commit)
                }
                Err(error) => {
                    self.status = None;
                    self.error = Some(error);
                    None
                }
            },
            Ok(_) => {
                active.cancellation.cancel();
                self.status = None;
                self.error = Some("Ignored an out-of-date remote connection result".to_owned());
                None
            }
            Err(error) => {
                active.cancellation.cancel();
                self.status = None;
                self.error = Some(error);
                None
            }
        }
    }
}

fn run_remote_connect(
    request: RemoteConnectRequest,
    transport: &dyn SshTransportFactory,
    cancellation: DumpCancellation,
) -> Result<RemoteConnectionCommit, String> {
    let password_for_transport = SecretString::from(request.password.expose_secret());
    let password_for_session = SecretString::from(request.password.expose_secret());
    let control = RemoteOperationControl::new(
        RemoteTimeouts {
            connect: Duration::from_secs(10),
            idle: Duration::from_secs(10),
            overall: Duration::from_secs(20),
        },
        cancellation,
    )
    .map_err(|error| error.to_string())?;
    let target = SshConnectionTarget {
        host: request.host.clone(),
        port: request.port,
        username: request.username.clone(),
        expected_host_key: None,
        command_subsystem: None,
        remote_event_subsystem: None,
    };
    let mut session = transport
        .connect(
            &target,
            SshCredential::Password(password_for_transport),
            &control,
        )
        .map_err(|error| error.to_string())?;
    session
        .canonicalize(SFTP_NAMESPACE_ROOT, &control)
        .map_err(|error| {
            format!("SSH login succeeded, but the SFTP namespace is unavailable: {error}")
        })?;

    let config = RemoteConnectionConfig {
        id: request.id,
        display_name: format!("{}@{}:{}", request.username, request.host, request.port),
        host: request.host,
        port: request.port,
        username: request.username,
        expected_host_key: None,
        authentication: RemoteAuthentication::Password {
            slot_id: request.slot_id,
        },
    };
    config.validate().map_err(|error| error.to_string())?;
    Ok(RemoteConnectionCommit {
        config,
        session_password: password_for_session,
    })
}

fn run_remote_connect_guarded(
    request: RemoteConnectRequest,
    transport: &dyn SshTransportFactory,
    cancellation: DumpCancellation,
) -> Result<RemoteConnectionCommit, String> {
    catch_unwind(AssertUnwindSafe(|| {
        run_remote_connect(request, transport, cancellation)
    }))
    .unwrap_or_else(|_| Err("Remote connection worker crashed unexpectedly".to_owned()))
}

fn parse_endpoint(value: &str) -> Result<ParsedEndpoint, String> {
    let value = value.trim();
    let (username, address) = value
        .split_once('@')
        .ok_or_else(|| "Endpoint must use user@host:port".to_owned())?;
    let username = username.trim();
    if username.is_empty() || username.contains(char::is_whitespace) || username.contains('\0') {
        return Err("Endpoint username must not be empty or contain whitespace".to_owned());
    }
    let address = address.trim();
    let (host, port) = if let Some(bracketed) = address.strip_prefix('[') {
        let closing = bracketed
            .find(']')
            .ok_or_else(|| "Bracketed IPv6 endpoint is missing ']'".to_owned())?;
        let host = &bracketed[..closing];
        let suffix = &bracketed[closing + 1..];
        let port = if suffix.is_empty() {
            22
        } else {
            let value = suffix
                .strip_prefix(':')
                .ok_or_else(|| "Unexpected text after bracketed IPv6 host".to_owned())?;
            parse_port(value)?
        };
        (host, port)
    } else {
        if address.matches(':').count() > 1 {
            return Err("IPv6 hosts must be enclosed in brackets".to_owned());
        }
        match address.rsplit_once(':') {
            Some((host, port)) => (host, parse_port(port)?),
            None => (address, 22),
        }
    };
    let host = host.trim();
    if host.is_empty() || host.contains(char::is_whitespace) || host.contains(['/', '\\', '\0']) {
        return Err("Endpoint host is invalid".to_owned());
    }
    Ok(ParsedEndpoint {
        username: username.to_owned(),
        host: host.to_owned(),
        port,
    })
}

fn parse_port(value: &str) -> Result<u16, String> {
    let port = value
        .parse::<u16>()
        .map_err(|_| "SSH port must be between 1 and 65535".to_owned())?;
    if port == 0 {
        Err("SSH port must be between 1 and 65535".to_owned())
    } else {
        Ok(port)
    }
}

fn remote_connection_id(
    host: &str,
    port: u16,
    username: &str,
) -> Result<RemoteConnectionId, String> {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in username
        .as_bytes()
        .iter()
        .copied()
        .chain([0])
        .chain(host.as_bytes().iter().copied())
        .chain([0])
        .chain(port.to_be_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    RemoteConnectionId::new(format!("remote-{hash:016x}")).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_adapters::platforms::ssh_managed::MemorySshTransport;

    #[test]
    fn parses_hostname_default_port_and_bracketed_ipv6() {
        let endpoint = parse_endpoint("root@camera.local").unwrap();
        assert_eq!(endpoint.username, "root");
        assert_eq!(endpoint.host, "camera.local");
        assert_eq!(endpoint.port, 22);

        let endpoint = parse_endpoint("operator@[2001:db8::1]:2222").unwrap();
        assert_eq!(endpoint.username, "operator");
        assert_eq!(endpoint.host, "2001:db8::1");
        assert_eq!(endpoint.port, 2222);
    }

    #[test]
    fn rejects_ambiguous_or_invalid_endpoints() {
        for endpoint in [
            "camera.local:22",
            "@camera.local:22",
            "root@bad host:22",
            "root@2001:db8::1",
            "root@camera.local:0",
        ] {
            assert!(parse_endpoint(endpoint).is_err(), "accepted {endpoint:?}");
        }
    }

    #[test]
    fn connector_uses_server_namespace_root_and_process_only_password() {
        let memory = Arc::new(MemorySshTransport::new("memory-host-key"));
        memory.insert_directory(SFTP_NAMESPACE_ROOT);
        let transport: Arc<dyn SshTransportFactory> = memory;
        let context = egui::Context::default();
        let mut connector = RemoteConnector::new(transport);
        connector.draft.endpoint = "root@camera.test:22".to_owned();
        connector
            .draft
            .password
            .expose_secret_mut()
            .push_str("memory-test-secret");

        connector.start(&context);
        let deadline = Instant::now() + Duration::from_secs(1);
        let commit = loop {
            if let Some(commit) = connector.poll(&context) {
                break commit;
            }
            assert!(Instant::now() < deadline, "memory SFTP connect timed out");
            thread::sleep(Duration::from_millis(5));
        };

        assert!(commit.config.expected_host_key.is_none());
        assert!(matches!(
            commit.config.authentication,
            RemoteAuthentication::Password { .. }
        ));
        assert!(!commit.session_password.expose_secret().is_empty());
    }
}
