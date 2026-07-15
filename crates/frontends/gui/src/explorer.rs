//! 文件工作区 Explorer：本地/SFTP 挂载浏览，目录读取始终在后台线程完成。

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
    time::Duration,
};
#[cfg(feature = "platform-ssh")]
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    time::Instant,
};

use camera_toolbox_adapters::filesystem::LocalFileSystem;
#[cfg(feature = "platform-ssh")]
use camera_toolbox_adapters::{
    filesystem::SftpFileSystem,
    platforms::ssh_managed::{
        CredentialResolver, RusshTransportFactory, SshConnectionTarget, SshCredential,
        SshTransportFactory,
    },
};
#[cfg(feature = "platform-ssh")]
use camera_toolbox_app::{
    ConnectionSettings, DumpCancellation, RemoteAuthentication, RemoteConnectionConfig,
    RemoteConnectionId, RemoteOperationControl, RemoteTimeouts,
};
use camera_toolbox_app::{
    DirectoryRef, FileEntry, FileKind, FileRef, FileSourceId, FileSystem, FsControl,
    ListPageRequest, MountedSourceConfig, SourcePath, WorkspaceSettings,
};
use eframe::egui;
#[cfg(feature = "platform-ssh")]
use secrecy::{ExposeSecret, ExposeSecretMut, SecretBox, SecretString};

#[cfg(feature = "platform-ssh")]
const REMOTE_CONNECT_WATCHDOG: Duration = Duration::from_secs(30);

pub(crate) enum ExplorerAction {
    AddLocalMount(PathBuf),
    #[cfg(feature = "platform-ssh")]
    SaveRemoteConnection(RemoteConnectionCommit),
    #[cfg(feature = "platform-ssh")]
    AddSftpMount {
        connection_id: RemoteConnectionId,
        remote_root: String,
    },
    OpenAuto {
        display_path: PathBuf,
        file_system: Arc<dyn FileSystem>,
        reference: FileRef,
    },
}

#[cfg(feature = "platform-ssh")]
pub(crate) struct RemoteConnectionCommit {
    pub(crate) config: RemoteConnectionConfig,
    pub(crate) session_password: Option<SecretString>,
}

#[cfg(feature = "platform-ssh")]
struct RemoteConnectionDraft {
    original_id: Option<RemoteConnectionId>,
    existing_password_slot_id: Option<String>,
    host: String,
    port: u16,
    username: String,
    password: SecretBox<String>,
}

#[cfg(feature = "platform-ssh")]
impl RemoteConnectionDraft {
    fn new() -> Self {
        Self {
            original_id: None,
            existing_password_slot_id: None,
            host: String::new(),
            port: 22,
            username: String::new(),
            password: SecretBox::default(),
        }
    }

    fn from_config(connection: &RemoteConnectionConfig) -> Self {
        let existing_password_slot_id = match &connection.authentication {
            RemoteAuthentication::Password { slot_id } => Some(slot_id.clone()),
            RemoteAuthentication::PrivateKeyFile { .. } => None,
        };
        Self {
            original_id: Some(connection.id.clone()),
            existing_password_slot_id,
            host: connection.host.clone(),
            port: connection.port,
            username: connection.username.clone(),
            password: SecretBox::default(),
        }
    }

    fn connect_request(&self) -> Result<RemoteConnectRequest, String> {
        let host = self.host.trim();
        if host.is_empty() || host.contains(char::is_whitespace) || host.contains(['/', '\\', '\0'])
        {
            return Err("Enter a valid Host / IP".to_owned());
        }
        if self.port == 0 {
            return Err("SSH port must be between 1 and 65535".to_owned());
        }
        let username = self.username.trim();
        if username.is_empty() || username.contains('\0') {
            return Err("Username must not be empty".to_owned());
        }
        if self.password.expose_secret().is_empty() {
            return Err("Password must not be empty".to_owned());
        }

        let id = match &self.original_id {
            Some(id) => id.clone(),
            None => remote_connection_id(host, self.port, username)?,
        };
        let slot_id = self
            .existing_password_slot_id
            .clone()
            .unwrap_or_else(|| format!("credential-{}", id.as_str()));
        Ok(RemoteConnectRequest {
            id,
            slot_id,
            host: host.to_owned(),
            port: self.port,
            username: username.to_owned(),
            password: SecretString::from(self.password.expose_secret().clone()),
        })
    }
}

#[cfg(feature = "platform-ssh")]
struct RemoteConnectRequest {
    id: RemoteConnectionId,
    slot_id: String,
    host: String,
    port: u16,
    username: String,
    password: SecretString,
}

#[cfg(feature = "platform-ssh")]
struct RemoteConnectResult {
    generation: u64,
    result: Result<RemoteConnectionCommit, String>,
}

#[cfg(feature = "platform-ssh")]
struct ActiveRemoteConnect {
    generation: u64,
    started_at: Instant,
    cancellation: DumpCancellation,
    receiver: Receiver<RemoteConnectResult>,
}

#[cfg(feature = "platform-ssh")]
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
    let connection_target = SshConnectionTarget {
        host: request.host.clone(),
        port: request.port,
        username: request.username.clone(),
        expected_host_key: None,
        command_subsystem: None,
        remote_event_subsystem: None,
    };
    let mut session = transport
        .connect(
            &connection_target,
            SshCredential::Password(password_for_transport),
            &control,
        )
        .map_err(|error| error.to_string())?;
    session
        .canonicalize("/", &control)
        .map_err(|error| format!("SSH login succeeded, but SFTP is unavailable: {error}"))?;

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
        session_password: Some(password_for_session),
    })
}

#[cfg(feature = "platform-ssh")]
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

pub(crate) struct MountBinding {
    pub(crate) config: MountedSourceConfig,
    pub(crate) file_system: Arc<dyn FileSystem>,
}

struct ListJobResult {
    source_id: FileSourceId,
    directory: SourcePath,
    request_id: u64,
    result: Result<Vec<FileEntry>, String>,
}

struct MountView {
    config: MountedSourceConfig,
    display_name: String,
    display_root: String,
    file_system: Arc<dyn FileSystem>,
    current_directory: SourcePath,
    selected: Option<SourcePath>,
    entries: Vec<FileEntry>,
    loading: bool,
    loaded_once: bool,
    error: Option<String>,
    active_request_id: u64,
}

impl MountView {
    fn try_local(source_id: FileSourceId, root: PathBuf) -> Result<Self, String> {
        let file_system = Arc::new(
            LocalFileSystem::new(source_id.clone(), &root).map_err(|error| error.to_string())?,
        );
        let root = file_system.root().to_path_buf();
        let display_name = root
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map_or_else(|| root.display().to_string(), str::to_owned);
        Ok(Self {
            config: MountedSourceConfig::Local {
                source_id,
                root: root.clone(),
            },
            display_name,
            display_root: root.display().to_string(),
            file_system,
            current_directory: SourcePath::root(),
            selected: None,
            entries: Vec::new(),
            loading: false,
            loaded_once: false,
            error: None,
            active_request_id: 0,
        })
    }

    #[cfg(feature = "platform-ssh")]
    fn try_sftp(
        source_id: FileSourceId,
        connection_id: camera_toolbox_app::RemoteConnectionId,
        remote_root: String,
        connections: &ConnectionSettings,
        credentials: Arc<dyn CredentialResolver>,
        transport: Arc<dyn SshTransportFactory>,
    ) -> Result<Self, String> {
        let connection = connections
            .get(&connection_id)
            .ok_or_else(|| format!("missing remote connection {}", connection_id.as_str()))?;
        let credential_ref = credential_ref_from_authentication(&connection.authentication)?;
        let target = SshConnectionTarget {
            host: connection.host.clone(),
            port: connection.port,
            username: connection.username.clone(),
            expected_host_key: None,
            command_subsystem: None,
            remote_event_subsystem: None,
        };
        let file_system: Arc<dyn FileSystem> = Arc::new(
            SftpFileSystem::new(
                source_id.clone(),
                remote_root.clone(),
                target,
                credential_ref,
                credentials,
                transport,
            )
            .map_err(|error| error.to_string())?,
        );
        Ok(Self {
            config: MountedSourceConfig::Sftp {
                source_id,
                connection_id: connection_id.clone(),
                remote_root: remote_root.clone(),
            },
            display_name: format!("{} ({remote_root})", connection.display_name),
            display_root: format!(
                "ssh://{}@{}:{}{}",
                connection.username, connection.host, connection.port, remote_root
            ),
            file_system,
            current_directory: SourcePath::root(),
            selected: None,
            entries: Vec::new(),
            loading: false,
            loaded_once: false,
            error: None,
            active_request_id: 0,
        })
    }

    fn source_id(&self) -> &FileSourceId {
        self.config.source_id()
    }

    fn directory_ref(&self) -> DirectoryRef {
        DirectoryRef::new(self.source_id().clone(), self.current_directory.clone())
    }

    fn location_label(&self) -> String {
        if self.current_directory.is_root() {
            self.display_root.clone()
        } else {
            format!("{}/{}", self.display_root, self.current_directory.as_str())
        }
    }

    fn display_path(&self, path: &SourcePath) -> PathBuf {
        if path.is_root() {
            return PathBuf::from(&self.display_root);
        }
        PathBuf::from(format!("{}/{}", self.display_root, path.as_str()))
    }
}

pub(crate) struct ExplorerState {
    mounts: Vec<MountView>,
    active_source: Option<FileSourceId>,
    list_sender: Sender<ListJobResult>,
    list_receiver: Receiver<ListJobResult>,
    next_request_id: u64,
    global_error: Option<String>,
    pending_local_root: String,
    #[cfg(feature = "platform-ssh")]
    connections: ConnectionSettings,
    #[cfg(feature = "platform-ssh")]
    credentials: Arc<dyn CredentialResolver>,
    #[cfg(feature = "platform-ssh")]
    transport: Arc<dyn SshTransportFactory>,
    #[cfg(feature = "platform-ssh")]
    selected_connection: Option<RemoteConnectionId>,
    #[cfg(feature = "platform-ssh")]
    pending_remote_root: String,
    #[cfg(feature = "platform-ssh")]
    remote_editor_open: bool,
    #[cfg(feature = "platform-ssh")]
    remote_editor_error: Option<String>,
    #[cfg(feature = "platform-ssh")]
    remote_status: Option<String>,
    #[cfg(feature = "platform-ssh")]
    remote_draft: RemoteConnectionDraft,
    #[cfg(feature = "platform-ssh")]
    next_remote_connect_generation: u64,
    #[cfg(feature = "platform-ssh")]
    active_remote_connect: Option<ActiveRemoteConnect>,
}

impl ExplorerState {
    #[cfg(not(feature = "platform-ssh"))]
    pub(crate) fn from_settings(settings: &WorkspaceSettings) -> Self {
        let (list_sender, list_receiver) = mpsc::channel();
        let mut mounts = Vec::new();
        let mut global_error = None;
        for mount in settings.mounts() {
            let MountedSourceConfig::Local { source_id, root } = mount else {
                continue;
            };
            match MountView::try_local(source_id.clone(), root.clone()) {
                Ok(view) => mounts.push(view),
                Err(error) => global_error = Some(error),
            }
        }
        Self {
            active_source: mounts.first().map(|mount| mount.source_id().clone()),
            mounts,
            list_sender,
            list_receiver,
            next_request_id: 1,
            global_error,
            pending_local_root: default_local_mount_input(),
        }
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn from_settings(
        settings: &WorkspaceSettings,
        connections: &ConnectionSettings,
        credentials: Arc<dyn CredentialResolver>,
        transport: Arc<RusshTransportFactory>,
    ) -> Self {
        let (list_sender, list_receiver) = mpsc::channel();
        let mut mounts = Vec::new();
        let mut errors = Vec::new();
        let transport: Arc<dyn SshTransportFactory> = transport;
        for mount in settings.mounts() {
            let view = match mount {
                MountedSourceConfig::Local { source_id, root } => {
                    MountView::try_local(source_id.clone(), root.clone())
                }
                MountedSourceConfig::Sftp {
                    source_id,
                    connection_id,
                    remote_root,
                } => MountView::try_sftp(
                    source_id.clone(),
                    connection_id.clone(),
                    remote_root.clone(),
                    connections,
                    Arc::clone(&credentials),
                    Arc::clone(&transport),
                ),
            };
            match view {
                Ok(view) => mounts.push(view),
                Err(error) => errors.push(error),
            }
        }
        let selected_connection = connections
            .iter()
            .next()
            .map(|connection| connection.id.clone());
        let remote_draft = selected_connection
            .as_ref()
            .and_then(|id| connections.get(id))
            .map(RemoteConnectionDraft::from_config)
            .unwrap_or_else(RemoteConnectionDraft::new);
        Self {
            active_source: mounts.first().map(|mount| mount.source_id().clone()),
            mounts,
            list_sender,
            list_receiver,
            next_request_id: 1,
            global_error: (!errors.is_empty()).then(|| errors.join("; ")),
            pending_local_root: default_local_mount_input(),
            connections: connections.clone(),
            credentials,
            transport,
            selected_connection,
            pending_remote_root: "/opt".to_owned(),
            remote_editor_open: connections.iter().next().is_none(),
            remote_editor_error: None,
            remote_status: None,
            remote_draft,
            next_remote_connect_generation: 1,
            active_remote_connect: None,
        }
    }

    pub(crate) fn ensure_default_local_mount(&mut self, context: &egui::Context) {
        if self.mounts.is_empty() {
            if let Ok(root) = std::env::current_dir() {
                let _ = self.add_local_mount(root, context);
            }
            return;
        }
        if self
            .active_mount()
            .is_some_and(|mount| !mount.loaded_once && !mount.loading)
        {
            self.refresh_active(context);
        }
    }

    pub(crate) fn add_local_mount(
        &mut self,
        root: PathBuf,
        context: &egui::Context,
    ) -> Result<MountedSourceConfig, String> {
        self.global_error = None;
        let source_id = local_source_id_for_root(&root)?;
        if let Some(config) = self
            .mounts
            .iter()
            .find(|mount| mount.source_id() == &source_id)
            .map(|mount| mount.config.clone())
        {
            self.active_source = Some(source_id);
            self.refresh_active(context);
            return Ok(config);
        }
        let view = MountView::try_local(source_id.clone(), root)?;
        let config = view.config.clone();
        self.active_source = Some(source_id);
        self.mounts.push(view);
        self.refresh_active(context);
        Ok(config)
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn add_sftp_mount(
        &mut self,
        connection_id: RemoteConnectionId,
        remote_root: String,
        context: &egui::Context,
    ) -> Result<MountedSourceConfig, String> {
        self.global_error = None;
        let source_id = sftp_source_id_for_mount(&connection_id, &remote_root)?;
        if let Some(config) = self
            .mounts
            .iter()
            .find(|mount| mount.source_id() == &source_id)
            .map(|mount| mount.config.clone())
        {
            self.active_source = Some(source_id);
            self.refresh_active(context);
            return Ok(config);
        }
        let view = MountView::try_sftp(
            source_id.clone(),
            connection_id,
            remote_root,
            &self.connections,
            Arc::clone(&self.credentials),
            Arc::clone(&self.transport),
        )?;
        let config = view.config.clone();
        self.active_source = Some(source_id);
        self.mounts.push(view);
        self.refresh_active(context);
        Ok(config)
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn finish_remote_connection_save(
        &mut self,
        connection: RemoteConnectionConfig,
        context: &egui::Context,
    ) -> Result<(), String> {
        self.connections
            .upsert(connection.clone())
            .map_err(|error| error.to_string())?;
        self.selected_connection = Some(connection.id.clone());
        self.remote_editor_open = false;
        self.remote_editor_error = None;
        self.global_error = None;
        self.remote_status = Some(format!(
            "Connected to {}@{}:{}",
            connection.username, connection.host, connection.port
        ));
        self.cancel_remote_connect();
        self.remote_draft = RemoteConnectionDraft::from_config(&connection);
        let connections = self.connections.clone();
        let credentials = Arc::clone(&self.credentials);
        let transport = Arc::clone(&self.transport);
        let mut refresh_active = false;
        for mount in &mut self.mounts {
            let MountedSourceConfig::Sftp {
                source_id,
                connection_id,
                remote_root,
            } = &mount.config
            else {
                continue;
            };
            if connection_id != &connection.id {
                continue;
            }
            let was_active = self.active_source.as_ref() == Some(source_id);
            let current_directory = mount.current_directory.clone();
            let mut rebuilt = MountView::try_sftp(
                source_id.clone(),
                connection.id.clone(),
                remote_root.clone(),
                &connections,
                Arc::clone(&credentials),
                Arc::clone(&transport),
            )?;
            rebuilt.current_directory = current_directory;
            rebuilt.selected = None;
            *mount = rebuilt;
            refresh_active |= was_active;
        }
        if refresh_active {
            self.refresh_active(context);
        }
        Ok(())
    }

    #[cfg(feature = "platform-ssh")]
    fn begin_new_remote_connection(&mut self) {
        self.remote_editor_open = true;
        self.remote_editor_error = None;
        self.remote_status = None;
        self.remote_draft = RemoteConnectionDraft::new();
    }

    #[cfg(feature = "platform-ssh")]
    fn begin_edit_selected_remote_connection(&mut self) {
        let Some(connection) = self
            .selected_connection
            .as_ref()
            .and_then(|id| self.connections.get(id))
        else {
            self.remote_editor_error = Some("Select a saved remote connection first".to_owned());
            self.remote_editor_open = true;
            return;
        };
        self.remote_editor_open = true;
        self.remote_editor_error = None;
        self.remote_status = None;
        self.remote_draft = RemoteConnectionDraft::from_config(connection);
    }

    #[cfg(feature = "platform-ssh")]
    fn remote_connect_in_flight(&self) -> bool {
        self.active_remote_connect.is_some()
    }

    #[cfg(feature = "platform-ssh")]
    fn cancel_remote_connect(&mut self) {
        if let Some(active) = self.active_remote_connect.take() {
            active.cancellation.cancel();
        }
    }

    #[cfg(feature = "platform-ssh")]
    fn start_remote_connect(&mut self, context: &egui::Context) {
        if self.remote_connect_in_flight() {
            return;
        }
        let generation = self.next_remote_connect_generation;
        self.next_remote_connect_generation = self.next_remote_connect_generation.saturating_add(1);
        let request = match self.remote_draft.connect_request() {
            Ok(request) => request,
            Err(error) => {
                self.remote_editor_error = Some(error);
                return;
            }
        };
        let cancellation = DumpCancellation::default();
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = mpsc::channel();
        self.remote_editor_error = None;
        self.global_error = None;
        self.remote_status = Some("Connecting to the remote device…".to_owned());
        self.active_remote_connect = Some(ActiveRemoteConnect {
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

    #[cfg(feature = "platform-ssh")]
    fn poll_remote_connect_result(&mut self, context: &egui::Context) -> Option<ExplorerAction> {
        let outcome = {
            let active = self.active_remote_connect.as_ref()?;
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
            .active_remote_connect
            .take()
            .expect("active connection exists while polling its result");
        match outcome {
            Ok(result) if result.generation == active.generation => match result.result {
                Ok(commit) => {
                    self.remote_status = Some("Remote device connected. Saving…".to_owned());
                    Some(ExplorerAction::SaveRemoteConnection(commit))
                }
                Err(error) => {
                    self.remote_status = None;
                    self.remote_editor_error = Some(error);
                    None
                }
            },
            Ok(_) => {
                active.cancellation.cancel();
                self.remote_status = None;
                self.remote_editor_error =
                    Some("Ignored an out-of-date remote connection result".to_owned());
                None
            }
            Err(error) => {
                active.cancellation.cancel();
                self.remote_status = None;
                self.remote_editor_error = Some(error);
                None
            }
        }
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn set_remote_connection_error(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.cancel_remote_connect();
        self.remote_status = None;
        self.remote_editor_open = true;
        self.remote_editor_error = Some(error.clone());
        self.global_error = Some(error);
    }

    pub(crate) fn set_global_error(&mut self, error: impl Into<String>) {
        self.global_error = Some(error.into());
    }

    pub(crate) fn poll(&mut self) {
        loop {
            let result = match self.list_receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            let Some(mount) = self
                .mounts
                .iter_mut()
                .find(|mount| mount.source_id() == &result.source_id)
            else {
                continue;
            };
            if mount.active_request_id != result.request_id
                || mount.current_directory != result.directory
            {
                continue;
            }
            mount.loading = false;
            mount.loaded_once = true;
            match result.result {
                Ok(entries) => {
                    mount.entries = entries;
                    mount.error = None;
                }
                Err(error) => {
                    mount.error = Some(error);
                    mount.entries.clear();
                }
            }
        }
    }

    pub(crate) fn render(
        &mut self,
        context: &egui::Context,
        ui: &mut egui::Ui,
    ) -> Option<ExplorerAction> {
        self.poll();
        let mut action = None;
        #[cfg(feature = "platform-ssh")]
        if let Some(remote_action) = self.poll_remote_connect_result(context) {
            action = Some(remote_action);
        }

        egui::CollapsingHeader::new("Sources / Connections")
            .id_salt("workspace_sources_connections")
            .default_open(true)
            .show(ui, |ui| {
                self.render_local_mount_controls(ui, &mut action);
                #[cfg(feature = "platform-ssh")]
                self.render_remote_connection_controls(ui, &mut action);
            });

        if let Some(error) = &self.global_error {
            ui.colored_label(egui::Color32::RED, error);
        }
        ui.separator();
        ui.heading("File Explorer");

        if self.mounts.is_empty() {
            ui.label("No mounted source.");
            return action;
        }

        let mut selected_source = self.active_source.clone();
        egui::ComboBox::from_id_salt("explorer_mounts")
            .width(ui.available_width())
            .selected_text(self.active_mount().map_or_else(
                || "Select mount".to_owned(),
                |mount| mount.display_name.clone(),
            ))
            .show_ui(ui, |ui| {
                for mount in &self.mounts {
                    ui.selectable_value(
                        &mut selected_source,
                        Some(mount.source_id().clone()),
                        &mount.display_name,
                    );
                }
            });
        if selected_source != self.active_source {
            self.active_source = selected_source;
            self.refresh_active(context);
        }

        let mut navigate_up = false;
        let mut refresh = false;
        let mut navigate_to = None;
        let mut open_reference = None;
        {
            let Some(mount) = self.active_mount_mut() else {
                return action;
            };
            ui.horizontal(|ui| {
                let up_enabled = !mount.current_directory.is_root();
                if ui
                    .add_enabled(up_enabled, egui::Button::new("Up"))
                    .clicked()
                {
                    navigate_up = true;
                }
                if ui.button("Refresh").clicked() {
                    refresh = true;
                }
            });
            ui.small(mount.location_label());
            if mount.loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading directory…");
                });
            }
            if let Some(error) = &mount.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for entry in &mount.entries {
                    let icon = match entry.kind {
                        FileKind::Directory => "[D]",
                        FileKind::File => "[F]",
                        FileKind::Symlink => "[L]",
                        FileKind::Other => "[?]",
                    };
                    let path = entry.reference.path.clone();
                    let selected = mount.selected.as_ref() == Some(&path);
                    let response = ui.selectable_label(selected, format!("{icon} {}", entry.name));
                    if response.clicked() {
                        mount.selected = Some(path.clone());
                    }
                    if response.double_clicked() {
                        match entry.kind {
                            FileKind::Directory => navigate_to = Some(path),
                            FileKind::File => open_reference = Some(entry.reference.clone()),
                            FileKind::Symlink | FileKind::Other => {}
                        }
                    }
                }
            });
        }

        if navigate_up {
            self.navigate_up(context);
        }
        if let Some(path) = navigate_to {
            self.navigate_to(path, context);
        }
        if refresh {
            self.refresh_active(context);
        }
        if let Some(reference) = open_reference
            && let Some(mount) = self.active_mount()
        {
            action = Some(ExplorerAction::OpenAuto {
                display_path: mount.display_path(&reference.path),
                file_system: Arc::clone(&mount.file_system),
                reference,
            });
        }
        action
    }

    fn render_local_mount_controls(
        &mut self,
        ui: &mut egui::Ui,
        action: &mut Option<ExplorerAction>,
    ) {
        ui.horizontal(|ui| {
            ui.strong("Local");
            if ui.button("Add Folder…").clicked()
                && let Some(root) = rfd::FileDialog::new()
                    .set_title("Mount Folder")
                    .pick_folder()
                && action.is_none()
            {
                *action = Some(ExplorerAction::AddLocalMount(root));
            }
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.pending_local_root)
                .hint_text("/path/to/local/folder")
                .desired_width(ui.available_width()),
        );
        if ui.button("Mount Local Path").clicked() && action.is_none() {
            let root = self.pending_local_root.trim();
            if root.is_empty() {
                self.global_error = Some("Local path must not be empty".to_owned());
            } else {
                *action = Some(ExplorerAction::AddLocalMount(PathBuf::from(root)));
            }
        }
    }

    #[cfg(feature = "platform-ssh")]
    fn render_remote_connection_controls(
        &mut self,
        ui: &mut egui::Ui,
        action: &mut Option<ExplorerAction>,
    ) {
        ui.separator();
        ui.strong("Remote");
        if self.connections.iter().next().is_some() {
            egui::ComboBox::from_id_salt("explorer_remote_connections")
                .width(ui.available_width())
                .selected_text(
                    self.selected_connection
                        .as_ref()
                        .and_then(|id| self.connections.get(id))
                        .map_or("Select remote", |connection| {
                            connection.display_name.as_str()
                        }),
                )
                .show_ui(ui, |ui| {
                    for connection in self.connections.iter() {
                        ui.selectable_value(
                            &mut self.selected_connection,
                            Some(connection.id.clone()),
                            &connection.display_name,
                        );
                    }
                });
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.remote_connect_in_flight(),
                    egui::Button::new("New Remote"),
                )
                .clicked()
            {
                self.begin_new_remote_connection();
            }
            if ui
                .add_enabled(
                    !self.remote_connect_in_flight() && self.selected_connection.is_some(),
                    egui::Button::new("Edit Selected"),
                )
                .clicked()
            {
                self.begin_edit_selected_remote_connection();
            }
        });
        if let Some(status) = &self.remote_status {
            ui.horizontal(|ui| {
                if self.remote_connect_in_flight() {
                    ui.spinner();
                }
                ui.small(status);
            });
        }
        if self.remote_editor_open {
            self.render_remote_connection_editor(ui);
        }
        self.render_remote_mount_controls(ui, action);
    }

    #[cfg(feature = "platform-ssh")]
    fn render_remote_connection_editor(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.strong(if self.remote_draft.original_id.is_some() {
                    "Edit Remote"
                } else {
                    "Connect to Remote"
                });
                if ui.button("Cancel").clicked() {
                    self.cancel_remote_connect();
                    self.remote_status = None;
                    self.remote_editor_open = false;
                    self.remote_editor_error = None;
                    self.remote_draft = RemoteConnectionDraft::new();
                }
            });
            if let Some(error) = &self.remote_editor_error {
                ui.colored_label(egui::Color32::RED, error);
            }
            ui.add_enabled_ui(!self.remote_connect_in_flight(), |ui| {
                ui.label("Host");
                ui.add(
                    egui::TextEdit::singleline(&mut self.remote_draft.host)
                        .hint_text("camera.local or 192.168.1.100")
                        .desired_width(ui.available_width()),
                );
                ui.label("Port");
                ui.add(egui::DragValue::new(&mut self.remote_draft.port).range(1..=u16::MAX));
                ui.label("Username");
                ui.add(
                    egui::TextEdit::singleline(&mut self.remote_draft.username)
                        .hint_text("root")
                        .desired_width(ui.available_width()),
                );
                ui.label("Password");
                ui.add(
                    egui::TextEdit::singleline(self.remote_draft.password.expose_secret_mut())
                        .password(true)
                        .desired_width(ui.available_width()),
                );
            });
            ui.small("Host identity is not verified or saved. Use this only on a trusted network.");
            if ui
                .add_enabled(
                    !self.remote_connect_in_flight(),
                    egui::Button::new("Connect"),
                )
                .clicked()
            {
                self.start_remote_connect(ui.ctx());
            }
        });
    }

    #[cfg(feature = "platform-ssh")]
    fn render_remote_mount_controls(
        &mut self,
        ui: &mut egui::Ui,
        action: &mut Option<ExplorerAction>,
    ) {
        if self.connections.iter().next().is_none() {
            ui.weak("Connect a remote source above to browse its files.");
            return;
        }
        ui.label("Remote location");
        egui::ComboBox::from_id_salt("file_explorer_remote_connection")
            .width(ui.available_width())
            .selected_text(
                self.selected_connection
                    .as_ref()
                    .and_then(|id| self.connections.get(id))
                    .map_or("Select remote", |connection| {
                        connection.display_name.as_str()
                    }),
            )
            .show_ui(ui, |ui| {
                for connection in self.connections.iter() {
                    ui.selectable_value(
                        &mut self.selected_connection,
                        Some(connection.id.clone()),
                        &connection.display_name,
                    );
                }
            });
        ui.add(
            egui::TextEdit::singleline(&mut self.pending_remote_root)
                .hint_text("/opt")
                .desired_width(ui.available_width()),
        );
        if ui.button("Mount Remote Folder").clicked()
            && action.is_none()
            && let Some(connection_id) = self.selected_connection.clone()
        {
            let remote_root = self.pending_remote_root.trim();
            *action = Some(ExplorerAction::AddSftpMount {
                connection_id,
                remote_root: if remote_root.is_empty() {
                    "/opt".to_owned()
                } else {
                    remote_root.to_owned()
                },
            });
        }
    }

    fn navigate_up(&mut self, context: &egui::Context) {
        let Some(mount) = self.active_mount_mut() else {
            return;
        };
        let Some(parent) = mount.current_directory.parent() else {
            return;
        };
        mount.current_directory = parent;
        mount.selected = None;
        self.refresh_active(context);
    }

    fn navigate_to(&mut self, path: SourcePath, context: &egui::Context) {
        let Some(mount) = self.active_mount_mut() else {
            return;
        };
        mount.current_directory = path;
        mount.selected = None;
        self.refresh_active(context);
    }

    fn refresh_active(&mut self, context: &egui::Context) {
        let Some(active) = self.active_source.as_ref() else {
            return;
        };
        let Some(index) = self
            .mounts
            .iter()
            .position(|mount| mount.source_id() == active)
        else {
            return;
        };
        self.request_refresh(index, context);
    }

    fn request_refresh(&mut self, index: usize, context: &egui::Context) {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let sender = self.list_sender.clone();
        let context = context.clone();
        let mount = &mut self.mounts[index];
        mount.loading = true;
        mount.error = None;
        mount.active_request_id = request_id;
        let source_id = mount.source_id().clone();
        let directory = mount.current_directory.clone();
        let file_system = Arc::clone(&mount.file_system);
        thread::spawn(move || {
            let control = FsControl::with_timeout(Duration::from_secs(5));
            let directory_ref = DirectoryRef::new(source_id.clone(), directory.clone());
            let mut continuation = None;
            let mut entries = Vec::new();
            let result = loop {
                let page = match file_system.list(
                    &directory_ref,
                    ListPageRequest {
                        continuation,
                        limit: 256,
                    },
                    &control,
                ) {
                    Ok(page) => page,
                    Err(error) => break Err(error.to_string()),
                };
                entries.extend(page.entries);
                let Some(next) = page.next else {
                    break Ok(entries);
                };
                continuation = Some(next);
            };
            let _ = sender.send(ListJobResult {
                source_id,
                directory,
                request_id,
                result,
            });
            context.request_repaint();
        });
    }

    pub(crate) fn mount_binding(&self, source_id: &FileSourceId) -> Option<MountBinding> {
        self.mounts
            .iter()
            .find(|mount| mount.source_id() == source_id)
            .map(|mount| MountBinding {
                config: mount.config.clone(),
                file_system: Arc::clone(&mount.file_system),
            })
    }

    pub(crate) fn open_action_for(&self, reference: &FileRef) -> Option<ExplorerAction> {
        let mount = self
            .mounts
            .iter()
            .find(|mount| mount.source_id() == &reference.source_id)?;
        Some(ExplorerAction::OpenAuto {
            display_path: mount.display_path(&reference.path),
            file_system: Arc::clone(&mount.file_system),
            reference: reference.clone(),
        })
    }

    fn active_mount(&self) -> Option<&MountView> {
        let active = self.active_source.as_ref()?;
        self.mounts.iter().find(|mount| mount.source_id() == active)
    }

    fn active_mount_mut(&mut self) -> Option<&mut MountView> {
        let active = self.active_source.as_ref()?;
        self.mounts
            .iter_mut()
            .find(|mount| mount.source_id() == active)
    }
}

#[cfg(feature = "platform-ssh")]
fn remote_connection_id(
    host: &str,
    port: u16,
    username: &str,
) -> Result<RemoteConnectionId, String> {
    // 稳定散列避免把用户名和主机名原文暴露为内部标识，同时保证同一端点可重复定位。
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

#[cfg(feature = "platform-ssh")]
fn credential_ref_from_authentication(
    authentication: &RemoteAuthentication,
) -> Result<String, String> {
    match authentication {
        RemoteAuthentication::Password { slot_id } => Ok(format!("session:{slot_id}")),
        RemoteAuthentication::PrivateKeyFile { path } => {
            let canonical = std::fs::canonicalize(path).map_err(|error| {
                format!(
                    "cannot normalize private-key path {}: {error}",
                    path.display()
                )
            })?;
            Ok(format!("key-file:{}", canonical.display()))
        }
    }
}

fn default_local_mount_input() -> String {
    std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn local_source_id_for_root(root: &Path) -> Result<FileSourceId, String> {
    let canonical = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    FileSourceId::new(format!("local-{:016x}", hasher.finish())).map_err(|error| error.to_string())
}

#[cfg(feature = "platform-ssh")]
fn sftp_source_id_for_mount(
    connection_id: &RemoteConnectionId,
    remote_root: &str,
) -> Result<FileSourceId, String> {
    let mut hasher = DefaultHasher::new();
    connection_id.as_str().hash(&mut hasher);
    remote_root.hash(&mut hasher);
    FileSourceId::new(format!("sftp-{:016x}", hasher.finish())).map_err(|error| error.to_string())
}

#[cfg(all(test, feature = "platform-ssh"))]
mod tests {
    use std::sync::Arc;

    use camera_toolbox_adapters::platforms::ssh_managed::{
        MemorySshTransport, ProductionCredentialResolver, SshTransportError, SshTransportSession,
    };
    use eframe::egui::accesskit::Role;

    use super::*;

    fn remote_draft(host: &str, username: &str, password: &str) -> RemoteConnectionDraft {
        let mut draft = RemoteConnectionDraft::new();
        draft.host = host.to_owned();
        draft.username = username.to_owned();
        *draft.password.expose_secret_mut() = password.to_owned();
        draft
    }

    fn explorer_state(connections: &ConnectionSettings) -> ExplorerState {
        ExplorerState::from_settings(
            &WorkspaceSettings::default(),
            connections,
            Arc::new(ProductionCredentialResolver::new()),
            Arc::new(RusshTransportFactory),
        )
    }

    fn render_explorer_frame(
        context: &egui::Context,
        explorer: &mut ExplorerState,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Option<ExplorerAction>) {
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(420.0, 720.0),
            )),
            ..Default::default()
        };
        input.events = events;
        let mut action = None;
        let output = context.run_ui(input, |ui| action = explorer.render(context, ui));
        (output, action)
    }

    fn button_center(output: &egui::FullOutput, label: &str) -> egui::Pos2 {
        let bounds = output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("accessibility tree is enabled")
            .nodes
            .iter()
            .find_map(|(_, node)| {
                (node.role() == Role::Button && node.label() == Some(label))
                    .then(|| node.bounds())
                    .flatten()
            })
            .unwrap_or_else(|| panic!("button {label:?} is visible"));
        egui::pos2(
            ((bounds.x0 + bounds.x1) * 0.5) as f32,
            ((bounds.y0 + bounds.y1) * 0.5) as f32,
        )
    }

    fn click_button(
        context: &egui::Context,
        explorer: &mut ExplorerState,
        label: &str,
    ) -> Option<ExplorerAction> {
        let (output, _) = render_explorer_frame(context, explorer, Vec::new());
        let position = button_center(&output, label);
        render_explorer_frame(
            context,
            explorer,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        );
        render_explorer_frame(
            context,
            explorer,
            vec![egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
        )
        .1
    }

    fn saved_connection() -> RemoteConnectionConfig {
        RemoteConnectionConfig {
            id: RemoteConnectionId::new("saved-camera").unwrap(),
            display_name: "root@camera.local:22".to_owned(),
            host: "camera.local".to_owned(),
            port: 22,
            username: "root".to_owned(),
            expected_host_key: None,
            authentication: RemoteAuthentication::Password {
                slot_id: "saved-slot".to_owned(),
            },
        }
    }

    #[test]
    fn remote_connect_uses_password_without_saving_host_key() {
        let transport = MemorySshTransport::new("unverified-server-key");
        let request = remote_draft("camera.local", "root", "process-only-password")
            .connect_request()
            .unwrap();

        let commit = run_remote_connect(request, &transport, DumpCancellation::default()).unwrap();

        assert_eq!(commit.config.host, "camera.local");
        assert_eq!(commit.config.port, 22);
        assert_eq!(commit.config.username, "root");
        assert_eq!(commit.config.expected_host_key, None);
        assert!(matches!(
            commit.config.authentication,
            RemoteAuthentication::Password { .. }
        ));
        assert_eq!(
            commit
                .session_password
                .as_ref()
                .expect("session password is returned")
                .expose_secret(),
            "process-only-password"
        );
    }

    #[test]
    fn sidebar_exposes_only_simple_remote_login_fields() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut explorer = explorer_state(&ConnectionSettings::default());

        let (output, _) = render_explorer_frame(&context, &mut explorer, Vec::new());
        let visible_text = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree is enabled")
            .nodes
            .into_iter()
            .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
            .collect::<Vec<_>>()
            .join("\n");

        for expected in [
            "Sources / Connections",
            "Local",
            "Remote",
            "Host",
            "Port",
            "Username",
            "Password",
            "Connect",
            "File Explorer",
        ] {
            assert!(visible_text.contains(expected), "missing {expected:?}");
        }
        for hidden in [
            "Expected host key",
            "Password slot",
            "Authentication",
            "Private key",
            "Connection ID",
        ] {
            assert!(!visible_text.contains(hidden), "unexpected {hidden:?}");
        }
    }

    #[test]
    fn typed_local_path_mount_emits_local_source_action() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut explorer = explorer_state(&ConnectionSettings::default());

        let action = click_button(&context, &mut explorer, "Mount Local Path")
            .expect("mount action is emitted");

        let ExplorerAction::AddLocalMount(root) = action else {
            panic!("expected local mount action");
        };
        assert_eq!(root, PathBuf::from(default_local_mount_input()));
    }

    #[test]
    fn saved_remote_mount_emits_sftp_source_action() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let connection = saved_connection();
        let mut connections = ConnectionSettings::default();
        connections.insert(connection.clone()).unwrap();
        let mut explorer = explorer_state(&connections);

        let action = click_button(&context, &mut explorer, "Mount Remote Folder")
            .expect("mount action is emitted");

        let ExplorerAction::AddSftpMount {
            connection_id,
            remote_root,
        } = action
        else {
            panic!("expected SFTP mount action");
        };
        assert_eq!(connection_id, connection.id);
        assert_eq!(remote_root, "/opt");
    }

    #[test]
    fn generated_connection_id_is_stable_and_endpoint_scoped() {
        let first = remote_connection_id("camera.local", 22, "root").unwrap();
        let same = remote_connection_id("camera.local", 22, "root").unwrap();
        let other_port = remote_connection_id("camera.local", 2222, "root").unwrap();
        let other_user = remote_connection_id("camera.local", 22, "operator").unwrap();

        assert_eq!(first, same);
        assert!(first.as_str().starts_with("remote-"));
        assert_ne!(first, other_port);
        assert_ne!(first, other_user);
        assert!(!first.as_str().contains("camera.local"));
        assert!(!first.as_str().contains("root"));
    }

    #[test]
    fn connect_request_trims_endpoint_and_keeps_password_secret() {
        let draft = remote_draft("  camera.local  ", " root ", "test-password");
        let request = draft.connect_request().unwrap();
        assert_eq!(request.host, "camera.local");
        assert_eq!(request.username, "root");
        assert_eq!(request.port, 22);
        assert_eq!(
            request.slot_id,
            format!("credential-{}", request.id.as_str())
        );
        assert_eq!(request.password.expose_secret(), "test-password");
        assert!(!format!("{:?}", request.password).contains("test-password"));
        assert!(!request.slot_id.contains("test-password"));
    }

    #[test]
    fn editing_saved_password_connection_preserves_internal_references() {
        let original_id = RemoteConnectionId::new("saved-camera").unwrap();
        let connection = RemoteConnectionConfig {
            id: original_id.clone(),
            display_name: "Camera".to_owned(),
            host: "old-camera.local".to_owned(),
            port: 22,
            username: "root".to_owned(),
            expected_host_key: Some("legacy-pinned-key".to_owned()),
            authentication: RemoteAuthentication::Password {
                slot_id: "existing-slot".to_owned(),
            },
        };
        let mut draft = RemoteConnectionDraft::from_config(&connection);
        draft.host = "new-camera.local".to_owned();
        *draft.password.expose_secret_mut() = "replacement".to_owned();

        let request = draft.connect_request().unwrap();

        assert_eq!(request.id, original_id);
        assert_eq!(request.slot_id, "existing-slot");
        assert_eq!(request.host, "new-camera.local");
    }

    #[test]
    fn connect_request_rejects_invalid_or_incomplete_login() {
        for (host, username, password) in [
            ("", "root", "secret"),
            ("bad host", "root", "secret"),
            ("camera.local/path", "root", "secret"),
            ("camera.local", "", "secret"),
            ("camera.local", "root", ""),
        ] {
            assert!(
                remote_draft(host, username, password)
                    .connect_request()
                    .is_err(),
                "unexpected valid login: host={host:?}, username={username:?}"
            );
        }
    }

    struct PanickingTransport;

    impl SshTransportFactory for PanickingTransport {
        fn connect(
            &self,
            _target: &SshConnectionTarget,
            _credential: SshCredential,
            _control: &RemoteOperationControl,
        ) -> Result<Box<dyn SshTransportSession>, SshTransportError> {
            panic!("simulated transport panic");
        }
    }

    #[test]
    fn worker_panic_is_reported_as_connection_error() {
        let request = remote_draft("camera.local", "root", "secret")
            .connect_request()
            .unwrap();

        let result =
            run_remote_connect_guarded(request, &PanickingTransport, DumpCancellation::default());
        let Err(error) = result else {
            panic!("panicking transport must return an error");
        };

        assert!(error.contains("crashed unexpectedly"));
    }

    #[test]
    fn disconnected_worker_clears_connecting_state() {
        let mut explorer = explorer_state(&ConnectionSettings::default());
        let context = egui::Context::default();
        let (sender, receiver) = mpsc::channel::<RemoteConnectResult>();
        explorer.active_remote_connect = Some(ActiveRemoteConnect {
            generation: 1,
            started_at: Instant::now(),
            cancellation: DumpCancellation::default(),
            receiver,
        });
        drop(sender);

        assert!(explorer.poll_remote_connect_result(&context).is_none());
        assert!(!explorer.remote_connect_in_flight());
        assert_eq!(
            explorer.remote_editor_error.as_deref(),
            Some("Remote connection worker stopped unexpectedly")
        );
    }

    #[test]
    fn watchdog_cancels_connection_that_exceeds_deadline() {
        let mut explorer = explorer_state(&ConnectionSettings::default());
        let context = egui::Context::default();
        let (_sender, receiver) = mpsc::channel::<RemoteConnectResult>();
        let cancellation = DumpCancellation::default();
        explorer.active_remote_connect = Some(ActiveRemoteConnect {
            generation: 1,
            started_at: Instant::now()
                .checked_sub(REMOTE_CONNECT_WATCHDOG + Duration::from_secs(1))
                .unwrap(),
            cancellation: cancellation.clone(),
            receiver,
        });

        assert!(explorer.poll_remote_connect_result(&context).is_none());
        assert!(!explorer.remote_connect_in_flight());
        assert!(cancellation.is_cancelled());
        assert!(
            explorer
                .remote_editor_error
                .as_deref()
                .is_some_and(|error| error.contains("timed out"))
        );
    }

    #[test]
    fn cancel_button_stops_in_flight_connection() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut explorer = explorer_state(&ConnectionSettings::default());
        explorer.remote_editor_open = true;
        let (_sender, receiver) = mpsc::channel::<RemoteConnectResult>();
        let cancellation = DumpCancellation::default();
        explorer.active_remote_connect = Some(ActiveRemoteConnect {
            generation: 1,
            started_at: Instant::now(),
            cancellation: cancellation.clone(),
            receiver,
        });

        assert!(click_button(&context, &mut explorer, "Cancel").is_none());
        assert!(cancellation.is_cancelled());
        assert!(!explorer.remote_connect_in_flight());
        assert!(!explorer.remote_editor_open);
    }
}
