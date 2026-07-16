//! 单一会话 Local/SFTP Workspace；目录监控和文件操作始终在后台执行。

mod browser;
#[cfg(feature = "platform-ssh")]
mod remote;

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use browser::{BrowserCommand, BrowserState, MutationRequest};
use camera_toolbox_adapters::filesystem::{
    DirectoryMonitorConfig, DirectoryMonitorEvent, DirectoryMonitorHandle, DirectoryMonitorState,
    LocalDirectoryMonitor, LocalFileSystem,
};
#[cfg(feature = "platform-ssh")]
use camera_toolbox_adapters::{
    filesystem::{RemoteDirectoryMonitor, SftpFileSystem},
    platforms::ssh_managed::{CredentialResolver, SshConnectionTarget, SshTransportFactory},
};
use camera_toolbox_app::{
    DirectoryChange, DirectoryRef, FileEntry, FileKind, FileRef, FileSourceId, FileSystem,
    FsCancellation, FsControl, MountedSourceConfig, SourcePath,
};
#[cfg(feature = "platform-ssh")]
use camera_toolbox_app::{RemoteAuthentication, RemoteConnectionConfig};
use eframe::egui;
#[cfg(feature = "platform-ssh")]
pub(crate) use remote::RemoteConnectionCommit;

const LOCAL_PATH_DEBOUNCE: Duration = Duration::from_millis(300);
const MUTATION_TIMEOUT: Duration = Duration::from_secs(30);
const MONITOR_REPAINT_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceMode {
    Local,
    #[cfg(feature = "platform-ssh")]
    Sftp,
}

impl WorkspaceMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Local => "Local",
            #[cfg(feature = "platform-ssh")]
            Self::Sftp => "SFTP",
        }
    }
}

pub(crate) enum ExplorerAction {
    #[cfg(feature = "platform-ssh")]
    ActivateSftp(RemoteConnectionCommit),
    OpenAuto {
        display_path: PathBuf,
        file_system: Arc<dyn FileSystem>,
        reference: FileRef,
        remote: bool,
    },
}

pub(crate) struct MountBinding {
    pub(crate) config: MountedSourceConfig,
    pub(crate) file_system: Arc<dyn FileSystem>,
}

struct SourceView {
    config: MountedSourceConfig,
    display_root: String,
    file_system: Arc<dyn FileSystem>,
    current_directory: SourcePath,
    selected: Option<SourcePath>,
    entries: Vec<FileEntry>,
    loading: bool,
    loaded_once: bool,
    stale: bool,
    error: Option<String>,
}

impl SourceView {
    fn try_local(root: PathBuf) -> Result<Self, String> {
        let source_id = local_source_id_for_root(&root)?;
        let file_system = Arc::new(
            LocalFileSystem::new(source_id.clone(), &root).map_err(|error| error.to_string())?,
        );
        let root = file_system.root().to_path_buf();
        Ok(Self {
            config: MountedSourceConfig::Local {
                source_id,
                root: root.clone(),
            },
            display_root: root.display().to_string(),
            file_system,
            current_directory: SourcePath::root(),
            selected: None,
            entries: Vec::new(),
            loading: false,
            loaded_once: false,
            stale: false,
            error: None,
        })
    }

    #[cfg(feature = "platform-ssh")]
    fn try_sftp(
        connection: RemoteConnectionConfig,
        credentials: Arc<dyn CredentialResolver>,
        transport: Arc<dyn SshTransportFactory>,
    ) -> Result<Self, String> {
        let source_id = sftp_source_id_for_mount(&connection.id, remote::SFTP_NAMESPACE_ROOT)?;
        let credential_ref = match &connection.authentication {
            RemoteAuthentication::Password { slot_id } => format!("session:{slot_id}"),
            RemoteAuthentication::PrivateKeyFile { path } => {
                let canonical = std::fs::canonicalize(path).map_err(|error| {
                    format!(
                        "cannot normalize private-key path {}: {error}",
                        path.display()
                    )
                })?;
                format!("key-file:{}", canonical.display())
            }
        };
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
                remote::SFTP_NAMESPACE_ROOT,
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
                connection_id: connection.id,
                remote_root: remote::SFTP_NAMESPACE_ROOT.to_owned(),
            },
            display_root: format!(
                "sftp://{}@{}:{}/",
                connection.username, connection.host, connection.port
            ),
            file_system,
            current_directory: SourcePath::root(),
            selected: None,
            entries: Vec::new(),
            loading: false,
            loaded_once: false,
            stale: false,
            error: None,
        })
    }

    fn source_id(&self) -> &FileSourceId {
        self.config.source_id()
    }

    fn directory_ref(&self) -> DirectoryRef {
        DirectoryRef::new(self.source_id().clone(), self.current_directory.clone())
    }

    fn navigation_path(&self) -> String {
        navigation_path_for(&self.config, &self.current_directory)
    }

    fn parse_navigation_path(&self, value: &str) -> Result<SourcePath, String> {
        let value = value.trim();
        if value.is_empty() {
            return Err("Path must not be empty".to_owned());
        }
        match &self.config {
            MountedSourceConfig::Local { root, .. } => {
                let candidate = Path::new(value);
                let relative = if candidate.is_absolute() {
                    candidate.strip_prefix(root).map_err(|_| {
                        format!("Path must stay within Local root {}", root.display())
                    })?
                } else {
                    candidate
                };
                source_path_from_native(relative)
            }
            MountedSourceConfig::Sftp { .. } => source_path_from_remote(value),
        }
    }

    fn display_path(&self, path: &SourcePath) -> PathBuf {
        if path.is_root() {
            return PathBuf::from(&self.display_root);
        }
        if self.display_root.ends_with('/') {
            PathBuf::from(format!("{}{}", self.display_root, path.as_str()))
        } else {
            PathBuf::from(format!("{}/{}", self.display_root, path.as_str()))
        }
    }

    fn is_remote(&self) -> bool {
        matches!(self.config, MountedSourceConfig::Sftp { .. })
    }

    fn local_native_path(&self) -> Option<PathBuf> {
        let MountedSourceConfig::Local { root, .. } = &self.config else {
            return None;
        };
        Some(if self.current_directory.is_root() {
            root.clone()
        } else {
            root.join(self.current_directory.as_str())
        })
    }
}

fn navigation_path_for(config: &MountedSourceConfig, path: &SourcePath) -> String {
    match config {
        MountedSourceConfig::Local { root, .. } => {
            if path.is_root() {
                root.display().to_string()
            } else {
                root.join(path.as_str()).display().to_string()
            }
        }
        MountedSourceConfig::Sftp { remote_root, .. } => join_remote_path(remote_root, path),
    }
}

fn join_remote_path(remote_root: &str, path: &SourcePath) -> String {
    let root = if remote_root == "/" {
        "/"
    } else {
        remote_root.trim_end_matches('/')
    };
    if path.is_root() {
        return root.to_owned();
    }
    if root == "/" {
        format!("/{}", path.as_str())
    } else {
        format!("{root}/{}", path.as_str())
    }
}

fn source_path_from_native(path: &Path) -> Result<SourcePath, String> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(component) => {
                let component = component
                    .to_str()
                    .ok_or_else(|| "Path contains non-UTF-8 components".to_owned())?;
                components.push(component);
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err("Path must stay within the active workspace root".to_owned());
            }
        }
    }
    SourcePath::directory(components.join("/")).map_err(|error| error.to_string())
}

fn source_path_from_remote(value: &str) -> Result<SourcePath, String> {
    let value = value.trim();
    if !value.starts_with('/') {
        return Err("SFTP Path must be an absolute path".to_owned());
    }
    SourcePath::directory(value.trim_start_matches('/')).map_err(|error| error.to_string())
}

struct ActiveDirectoryMonitor {
    source_id: FileSourceId,
    directory: SourcePath,
    handle: DirectoryMonitorHandle,
}

struct MutationJobResult {
    generation: u64,
    request: MutationRequest,
    result: Result<(), String>,
}

struct ActiveMutation {
    generation: u64,
    cancellation: FsCancellation,
}

pub(crate) struct ExplorerState {
    mode: WorkspaceMode,
    local_path: String,
    local_apply_at: Option<Instant>,
    local_view: Option<SourceView>,
    #[cfg(feature = "platform-ssh")]
    sftp_view: Option<SourceView>,
    active_monitor: Option<ActiveDirectoryMonitor>,
    browser: BrowserState,
    global_error: Option<String>,
    mutation_sender: Sender<MutationJobResult>,
    mutation_receiver: Receiver<MutationJobResult>,
    next_mutation_generation: u64,
    active_mutation: Option<ActiveMutation>,
    #[cfg(feature = "platform-ssh")]
    credentials: Arc<dyn CredentialResolver>,
    #[cfg(feature = "platform-ssh")]
    transport: Arc<dyn SshTransportFactory>,
    #[cfg(feature = "platform-ssh")]
    remote: remote::RemoteConnector,
}

impl ExplorerState {
    #[cfg(not(feature = "platform-ssh"))]
    pub(crate) fn new() -> Self {
        Self::base()
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn new(
        credentials: Arc<dyn CredentialResolver>,
        transport: Arc<dyn SshTransportFactory>,
    ) -> Self {
        let (mutation_sender, mutation_receiver) = mpsc::channel();
        Self {
            mode: WorkspaceMode::Local,
            local_path: default_local_path(),
            local_apply_at: None,
            local_view: None,
            sftp_view: None,
            active_monitor: None,
            browser: BrowserState::default(),
            global_error: None,
            mutation_sender,
            mutation_receiver,
            next_mutation_generation: 1,
            active_mutation: None,
            credentials,
            remote: remote::RemoteConnector::new(Arc::clone(&transport)),
            transport,
        }
    }

    #[cfg(not(feature = "platform-ssh"))]
    fn base() -> Self {
        let (mutation_sender, mutation_receiver) = mpsc::channel();
        Self {
            mode: WorkspaceMode::Local,
            local_path: default_local_path(),
            local_apply_at: None,
            local_view: None,
            active_monitor: None,
            browser: BrowserState::default(),
            global_error: None,
            mutation_sender,
            mutation_receiver,
            next_mutation_generation: 1,
            active_mutation: None,
        }
    }

    pub(crate) fn ensure_local_workspace(&mut self, context: &egui::Context) {
        if self.local_view.is_some() {
            return;
        }
        let path = PathBuf::from(self.local_path.trim());
        if path.as_os_str().is_empty() {
            return;
        }
        if let Err(error) = self.activate_local_root(path, context) {
            self.global_error = Some(format!("Open local path failed: {error}"));
        }
    }

    pub(crate) fn render(
        &mut self,
        context: &egui::Context,
        ui: &mut egui::Ui,
    ) -> Option<ExplorerAction> {
        self.ensure_local_workspace(context);
        self.poll_monitor(context);
        self.poll_mutation(context);

        let controls_enabled = self.active_mutation.is_none();
        let previous_mode = self.mode;
        ui.add_enabled_ui(controls_enabled, |ui| {
            egui::ComboBox::from_id_salt("workspace_mode")
                .width(ui.available_width())
                .selected_text(self.mode.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.mode, WorkspaceMode::Local, "Local");
                    #[cfg(feature = "platform-ssh")]
                    ui.selectable_value(&mut self.mode, WorkspaceMode::Sftp, "SFTP");
                });
        });
        if self.mode != previous_mode {
            self.switch_mode(context);
        }

        let mut action = None;
        match self.mode {
            WorkspaceMode::Local => {
                self.render_local_controls(context, ui, controls_enabled);
            }
            #[cfg(feature = "platform-ssh")]
            WorkspaceMode::Sftp => {
                if let Some(commit) = self.remote.render(context, ui, controls_enabled) {
                    action = Some(ExplorerAction::ActivateSftp(commit));
                }
            }
        }

        if let Some(error) = &self.global_error {
            ui.colored_label(egui::Color32::RED, error);
        }
        let Some(view) = self.active_view() else {
            if self.mode == WorkspaceMode::Local {
                ui.weak("Enter an existing local directory.");
            } else {
                ui.weak("Connect to an SFTP source to browse files.");
            }
            return action;
        };
        if view.loading || self.active_mutation.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if self.active_mutation.is_some() {
                    "Applying file operation…"
                } else {
                    "Loading directory…"
                });
            });
        }
        if view.stale {
            ui.colored_label(
                egui::Color32::YELLOW,
                "Directory connection is stale; retrying automatically…",
            );
        }
        if let Some(error) = &view.error {
            ui.colored_label(egui::Color32::RED, error);
        }

        let busy = self.active_mutation.is_some();
        let browser_command = self.render_browser(context, ui, busy);
        if let Some(command) = browser_command
            && let Some(open_action) = self.handle_browser_command(command, context)
        {
            action = Some(open_action);
        }
        action
    }

    fn render_local_controls(&mut self, context: &egui::Context, ui: &mut egui::Ui, enabled: bool) {
        ui.label("Root");
        let mut picked = None;
        let mut apply_now = false;
        ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.local_path)
                        .hint_text("/path/to/local/folder")
                        .desired_width((ui.available_width() - 92.0).max(80.0)),
                );
                if response.changed() {
                    self.local_apply_at = Some(Instant::now() + LOCAL_PATH_DEBOUNCE);
                    context.request_repaint_after(LOCAL_PATH_DEBOUNCE);
                }
                let enter = ui.input(|input| input.key_pressed(egui::Key::Enter));
                if (response.has_focus() || response.lost_focus()) && enter {
                    apply_now = true;
                }
                if ui.button("Open Folder").clicked() {
                    picked = rfd::FileDialog::new()
                        .set_title("Open Local Folder")
                        .pick_folder();
                }
            });
        });
        if !enabled {
            return;
        }

        if let Some(path) = picked {
            self.local_path = path.display().to_string();
            apply_now = true;
        }
        if self
            .local_apply_at
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            apply_now = true;
        }
        if !apply_now {
            return;
        }
        self.local_apply_at = None;
        let path = PathBuf::from(self.local_path.trim());
        if path.as_os_str().is_empty() {
            self.global_error = Some("Local path must not be empty".to_owned());
            return;
        }
        if let Err(error) = self.activate_local_root(path, context) {
            self.global_error = Some(format!("Open local path failed: {error}"));
        }
    }

    fn render_browser(
        &mut self,
        context: &egui::Context,
        ui: &mut egui::Ui,
        busy: bool,
    ) -> Option<BrowserCommand> {
        let browser = &mut self.browser;
        match self.mode {
            WorkspaceMode::Local => {
                let view = self.local_view.as_mut()?;
                let navigation_path = view.navigation_path();
                let SourceView {
                    config,
                    file_system,
                    current_directory,
                    selected,
                    entries,
                    ..
                } = view;
                browser.render(
                    context,
                    ui,
                    config.source_id(),
                    current_directory,
                    &navigation_path,
                    entries,
                    selected,
                    file_system.capabilities(),
                    busy,
                )
            }
            #[cfg(feature = "platform-ssh")]
            WorkspaceMode::Sftp => {
                let view = self.sftp_view.as_mut()?;
                let navigation_path = view.navigation_path();
                let SourceView {
                    config,
                    file_system,
                    current_directory,
                    selected,
                    entries,
                    ..
                } = view;
                browser.render(
                    context,
                    ui,
                    config.source_id(),
                    current_directory,
                    &navigation_path,
                    entries,
                    selected,
                    file_system.capabilities(),
                    busy,
                )
            }
        }
    }

    fn handle_browser_command(
        &mut self,
        command: BrowserCommand,
        context: &egui::Context,
    ) -> Option<ExplorerAction> {
        match command {
            BrowserCommand::NavigateUp => {
                if let Some(parent) = self
                    .active_view()
                    .and_then(|view| view.current_directory.parent())
                {
                    self.navigate_to(parent, context);
                }
                None
            }
            BrowserCommand::NavigatePath(value) => {
                let parsed = self
                    .active_view()
                    .ok_or_else(|| "No active workspace source".to_owned())
                    .and_then(|view| view.parse_navigation_path(&value));
                match parsed {
                    Ok(path) => {
                        self.browser.confirm_navigation();
                        self.navigate_to(path, context);
                    }
                    Err(error) => self.browser.set_navigation_error(error),
                }
                None
            }
            BrowserCommand::NavigateTo(path) => {
                self.navigate_to(path, context);
                None
            }
            BrowserCommand::Refresh => {
                self.start_active_monitor(context);
                None
            }
            BrowserCommand::Open(reference) => self.open_action_for(&reference),
            BrowserCommand::Mutate(request) => {
                self.begin_mutation(request, context);
                None
            }
        }
    }

    fn switch_mode(&mut self, context: &egui::Context) {
        self.cancel_active_mutation();
        self.stop_active_monitor();
        self.browser.clear_transient();
        self.global_error = None;
        self.start_active_monitor(context);
    }

    fn activate_local_root(
        &mut self,
        root: PathBuf,
        context: &egui::Context,
    ) -> Result<(), String> {
        let view = SourceView::try_local(root)?;
        self.cancel_active_mutation();
        self.stop_active_monitor();
        self.local_path = view.display_root.clone();
        self.local_view = Some(view);
        self.browser.clear_transient();
        self.global_error = None;
        if self.mode == WorkspaceMode::Local {
            self.start_active_monitor(context);
        }
        Ok(())
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn finish_sftp_connection(
        &mut self,
        connection: RemoteConnectionConfig,
        context: &egui::Context,
    ) -> Result<(), String> {
        let view = SourceView::try_sftp(
            connection,
            Arc::clone(&self.credentials),
            Arc::clone(&self.transport),
        )?;
        self.cancel_active_mutation();
        self.stop_active_monitor();
        self.sftp_view = Some(view);
        self.mode = WorkspaceMode::Sftp;
        self.browser.clear_transient();
        self.global_error = None;
        self.remote.mark_connected();
        self.start_active_monitor(context);
        Ok(())
    }

    #[cfg(feature = "platform-ssh")]
    pub(crate) fn set_remote_connection_error(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.remote.set_error(error.clone());
        self.global_error = Some(error);
    }

    fn navigate_to(&mut self, path: SourcePath, context: &egui::Context) {
        let Some(view) = self.active_view_mut() else {
            return;
        };
        view.current_directory = path;
        view.selected = None;
        self.browser.clear_transient();
        self.start_active_monitor(context);
    }

    fn start_active_monitor(&mut self, context: &egui::Context) {
        self.stop_active_monitor();
        let Some(view) = self.active_view_mut() else {
            return;
        };
        view.loading = true;
        view.stale = false;
        view.error = None;
        let source_id = view.source_id().clone();
        let directory = view.current_directory.clone();
        let directory_ref = view.directory_ref();
        let file_system = Arc::clone(&view.file_system);
        let native_path = view.local_native_path();
        let remote = view.is_remote();
        let config = DirectoryMonitorConfig::default();
        let handle = if remote {
            #[cfg(feature = "platform-ssh")]
            {
                RemoteDirectoryMonitor::spawn(file_system, directory_ref, config)
            }
            #[cfg(not(feature = "platform-ssh"))]
            {
                return;
            }
        } else {
            let Some(native_path) = native_path else {
                view.loading = false;
                view.error = Some("Local monitor path is unavailable".to_owned());
                return;
            };
            LocalDirectoryMonitor::spawn(file_system, directory_ref, native_path, config)
        };
        self.active_monitor = Some(ActiveDirectoryMonitor {
            source_id,
            directory,
            handle,
        });
        context.request_repaint_after(MONITOR_REPAINT_INTERVAL);
    }

    fn stop_active_monitor(&mut self) {
        let Some(monitor) = self.active_monitor.take() else {
            return;
        };
        let _ = thread::Builder::new()
            .name("camera-toolbox-monitor-cleanup".to_owned())
            .spawn(move || drop(monitor));
    }

    fn poll_monitor(&mut self, context: &egui::Context) {
        let Some(active) = self.active_monitor.as_ref() else {
            return;
        };
        context.request_repaint_after(MONITOR_REPAINT_INTERVAL);
        let source_id = active.source_id.clone();
        let directory = active.directory.clone();
        let mut events = Vec::new();
        while let Some(event) = active.handle.try_recv() {
            events.push(event);
        }
        if events.is_empty() {
            return;
        }
        let Some(view) = self.active_view_mut() else {
            return;
        };
        if view.source_id() != &source_id || view.current_directory != directory {
            return;
        }
        for event in events {
            match event {
                DirectoryMonitorEvent::Baseline(snapshot) => {
                    view.entries = snapshot.entries().cloned().collect();
                    sort_entries(&mut view.entries);
                    view.loading = false;
                    view.loaded_once = true;
                    view.stale = false;
                    view.error = None;
                }
                DirectoryMonitorEvent::Delta(delta) if delta.directory.path == directory => {
                    apply_directory_changes(&mut view.entries, delta.changes);
                    view.loading = false;
                    view.loaded_once = true;
                }
                DirectoryMonitorEvent::Delta(_) => {}
                DirectoryMonitorEvent::State(DirectoryMonitorState::Connected) => {
                    view.stale = false;
                    view.error = None;
                }
                DirectoryMonitorEvent::State(DirectoryMonitorState::Stale) => {
                    view.stale = true;
                }
                DirectoryMonitorEvent::State(DirectoryMonitorState::Stopped) => {
                    view.loading = false;
                    view.stale = true;
                    view.error = Some("Directory monitor stopped unexpectedly".to_owned());
                }
                DirectoryMonitorEvent::Error(error) => {
                    view.loading = false;
                    view.stale = true;
                    view.error = Some(error.to_string());
                }
            }
        }
    }

    fn begin_mutation(&mut self, request: MutationRequest, context: &egui::Context) {
        if self.active_mutation.is_some() {
            self.browser
                .set_message("Wait for the current file operation to finish");
            return;
        }
        let Some(view) = self.active_view() else {
            self.browser.set_message("No active file source");
            return;
        };
        let capabilities = view.file_system.capabilities();
        if !request.capability_enabled(capabilities) {
            self.browser
                .set_message("This file source does not support that operation");
            return;
        }
        let file_system = Arc::clone(&view.file_system);
        let generation = self.next_mutation_generation;
        self.next_mutation_generation = self.next_mutation_generation.saturating_add(1);
        let cancellation = FsCancellation::default();
        let worker_cancellation = cancellation.clone();
        let worker_request = request.clone();
        let sender = self.mutation_sender.clone();
        let context = context.clone();
        let monitor = self.active_monitor.take();
        self.active_mutation = Some(ActiveMutation {
            generation,
            cancellation,
        });
        thread::spawn(move || {
            // 在工作线程等待旧监控退出，既不阻塞 egui，也避免 SFTP 会话锁竞争。
            drop(monitor);
            let result = catch_unwind(AssertUnwindSafe(|| {
                execute_mutation(&*file_system, &worker_request, worker_cancellation)
            }))
            .unwrap_or_else(|_| Err("File operation worker crashed unexpectedly".to_owned()));
            let _ = sender.send(MutationJobResult {
                generation,
                request: worker_request,
                result,
            });
            context.request_repaint();
        });
    }

    fn poll_mutation(&mut self, context: &egui::Context) {
        loop {
            let result = match self.mutation_receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            if !self
                .active_mutation
                .as_ref()
                .is_some_and(|active| active.generation == result.generation)
            {
                continue;
            }
            self.active_mutation = None;
            match result.result {
                Ok(()) => {
                    self.browser.complete_mutation(&result.request);
                    if let Some(view) = self.active_view_mut() {
                        view.selected = None;
                    }
                }
                Err(error) => self.browser.fail_mutation(&result.request, error),
            }
            self.start_active_monitor(context);
        }
    }

    fn cancel_active_mutation(&mut self) {
        if let Some(active) = self.active_mutation.take() {
            active.cancellation.cancel();
        }
    }

    pub(crate) fn mount_binding(&self, source_id: &FileSourceId) -> Option<MountBinding> {
        self.views()
            .find(|view| view.source_id() == source_id)
            .map(|view| MountBinding {
                config: view.config.clone(),
                file_system: Arc::clone(&view.file_system),
            })
    }

    pub(crate) fn open_action_for(&self, reference: &FileRef) -> Option<ExplorerAction> {
        let view = self
            .views()
            .find(|view| view.source_id() == &reference.source_id)?;
        Some(ExplorerAction::OpenAuto {
            display_path: view.display_path(&reference.path),
            file_system: Arc::clone(&view.file_system),
            reference: reference.clone(),
            remote: view.is_remote(),
        })
    }

    fn active_view(&self) -> Option<&SourceView> {
        match self.mode {
            WorkspaceMode::Local => self.local_view.as_ref(),
            #[cfg(feature = "platform-ssh")]
            WorkspaceMode::Sftp => self.sftp_view.as_ref(),
        }
    }

    fn active_view_mut(&mut self) -> Option<&mut SourceView> {
        match self.mode {
            WorkspaceMode::Local => self.local_view.as_mut(),
            #[cfg(feature = "platform-ssh")]
            WorkspaceMode::Sftp => self.sftp_view.as_mut(),
        }
    }

    fn views(&self) -> impl Iterator<Item = &SourceView> {
        self.local_view.iter().chain({
            #[cfg(feature = "platform-ssh")]
            {
                self.sftp_view.iter()
            }
            #[cfg(not(feature = "platform-ssh"))]
            {
                std::iter::empty()
            }
        })
    }
}

fn execute_mutation(
    file_system: &dyn FileSystem,
    request: &MutationRequest,
    cancellation: FsCancellation,
) -> Result<(), String> {
    let mut control = FsControl::with_timeout(MUTATION_TIMEOUT);
    control.cancellation = cancellation;
    match request {
        MutationRequest::CreateDirectory { parent, name } => file_system
            .mkdir(parent, name, &control)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        MutationRequest::Rename {
            reference,
            new_name,
        } => file_system
            .rename(reference, new_name, &control)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        MutationRequest::Delete { entry } => file_system
            .delete(
                &entry.reference,
                entry.kind == FileKind::Directory,
                &control,
            )
            .map_err(|error| error.to_string()),
        MutationRequest::Move { entry, destination } => file_system
            .move_entry(&entry.reference, destination, &control)
            .map(|_| ())
            .map_err(|error| error.to_string()),
    }
}

fn apply_directory_changes(entries: &mut Vec<FileEntry>, changes: Vec<DirectoryChange>) {
    for change in changes {
        match change {
            DirectoryChange::Created(entry) => {
                entries.retain(|existing| existing.name != entry.name);
                entries.push(entry);
            }
            DirectoryChange::Modified { before, after } => {
                entries.retain(|existing| existing.name != before.name);
                entries.push(after);
            }
            DirectoryChange::Removed(entry) => {
                entries.retain(|existing| existing.name != entry.name);
            }
        }
    }
    sort_entries(entries);
}

fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|left, right| {
        let left_dir = left.kind == FileKind::Directory;
        let right_dir = right.kind == FileKind::Directory;
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.name.as_str().cmp(right.name.as_str()))
    });
}

fn default_local_path() -> String {
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
    connection_id: &camera_toolbox_app::RemoteConnectionId,
    remote_root: &str,
) -> Result<FileSourceId, String> {
    let mut hasher = DefaultHasher::new();
    connection_id.as_str().hash(&mut hasher);
    remote_root.hash(&mut hasher);
    FileSourceId::new(format!("sftp-{:016x}", hasher.finish())).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests;
