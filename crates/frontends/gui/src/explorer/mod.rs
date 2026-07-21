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
    time::Duration,
};

use browser::{BrowserCommand, BrowserSelection, BrowserState, MutationRequest};
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
    DirectoryChange, DirectoryRef, ExportDestination, FileEntry, FileKind, FileRef, FileSourceId,
    FileSystem, FsCancellation, FsControl, MountedSourceConfig, SourcePath,
};
#[cfg(feature = "platform-ssh")]
use camera_toolbox_app::{RemoteAuthentication, RemoteConnectionConfig};
use eframe::egui;
#[cfg(feature = "platform-ssh")]
pub(crate) use remote::RemoteConnectionCommit;

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

pub(crate) struct CalibrationImportCandidate {
    pub(crate) display_path: PathBuf,
    pub(crate) file_system: Arc<dyn FileSystem>,
    pub(crate) entry: FileEntry,
    pub(crate) remote: bool,
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
    AddCalibration(Vec<CalibrationImportCandidate>),
    CalibrationImportRejected {
        display_path: PathBuf,
    },
}

pub(crate) struct MountBinding {
    pub(crate) config: MountedSourceConfig,
    pub(crate) file_system: Arc<dyn FileSystem>,
}

struct SourceView {
    config: MountedSourceConfig,
    display_base: String,
    file_system: Arc<dyn FileSystem>,
    current_directory: SourcePath,
    selection: BrowserSelection,
    entries: Vec<FileEntry>,
    loading: bool,
    loaded_once: bool,
    stale: bool,
    error: Option<String>,
}

impl SourceView {
    fn try_local(path: PathBuf) -> Result<Self, String> {
        let path = std::fs::canonicalize(path).map_err(|error| error.to_string())?;
        if !path.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
        let namespace_root = local_namespace_root(&path)?;
        let source_id = local_source_id_for_root(&namespace_root)?;
        let file_system = Arc::new(
            LocalFileSystem::new(source_id.clone(), &namespace_root)
                .map_err(|error| error.to_string())?,
        );
        let root = file_system.root().to_path_buf();
        let current_directory =
            source_path_from_native(path.strip_prefix(&root).map_err(|_| {
                format!(
                    "Path is outside its filesystem namespace: {}",
                    path.display()
                )
            })?)?;
        Ok(Self {
            config: MountedSourceConfig::Local {
                source_id,
                root: root.clone(),
            },
            display_base: root.display().to_string(),
            file_system,
            current_directory,
            selection: BrowserSelection::default(),
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
            expected_host_key: connection.expected_host_key.clone(),
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
            display_base: format!(
                "sftp://{}@{}:{}/",
                connection.username, connection.host, connection.port
            ),
            file_system,
            current_directory: SourcePath::root(),
            selection: BrowserSelection::default(),
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

    fn display_path(&self, path: &SourcePath) -> PathBuf {
        if path.is_root() {
            return PathBuf::from(&self.display_base);
        }
        if self.display_base.ends_with('/') {
            PathBuf::from(format!("{}{}", self.display_base, path.as_str()))
        } else {
            PathBuf::from(format!("{}/{}", self.display_base, path.as_str()))
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

fn local_path_from_native(value: &str) -> Result<PathBuf, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Local Path must not be empty".to_owned());
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err("Local Path must be an absolute path".to_owned());
    }
    for component in path.components() {
        match component {
            std::path::Component::Normal(component) => {
                let component = component
                    .to_str()
                    .ok_or_else(|| "Path contains non-UTF-8 components".to_owned())?;
                if component.chars().any(char::is_control) {
                    return Err("Path contains control characters".to_owned());
                }
            }
            std::path::Component::ParentDir => {
                return Err("Local Path must not contain parent traversal".to_owned());
            }
            std::path::Component::CurDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {}
        }
    }
    Ok(path)
}

fn local_namespace_root(path: &Path) -> Result<PathBuf, String> {
    path.ancestors()
        .last()
        .filter(|root| root.is_absolute())
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("Path has no filesystem namespace root: {}", path.display()))
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
        if let Err(error) = self.activate_local_path(default_local_path(), context) {
            self.global_error = Some(format!("Open local path failed: {error}"));
        }
    }

    pub(crate) fn render(
        &mut self,
        context: &egui::Context,

        ui: &mut egui::Ui,
        calibration_import_enabled: bool,
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
        #[cfg(feature = "platform-ssh")]
        if self.mode == WorkspaceMode::Sftp
            && let Some(commit) = self.remote.render(context, ui, controls_enabled)
        {
            action = Some(ExplorerAction::ActivateSftp(commit));
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
        if view.is_remote() {
            if view.file_system.capabilities().write_new {
                ui.colored_label(
                    egui::Color32::GREEN,
                    "Pinned SSH source: exports create new files in this directory.",
                );
            } else {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "Browse-only SSH source: pin and remount a server host key before exporting.",
                );
            }
        }
        if let Some(error) = &view.error {
            ui.colored_label(egui::Color32::RED, error);
        }

        let busy = self.active_mutation.is_some();
        let browser_command = self.render_browser(context, ui, busy);
        if let Some(command) = browser_command
            && let Some(open_action) = self.handle_browser_command_for_workspace(
                command,
                context,
                calibration_import_enabled,
            )
        {
            action = Some(open_action);
        }
        if calibration_import_enabled {
            ui.separator();
            let selected_count = self.calibration_candidates(true).len();
            let directory_count = self.calibration_candidates(false).len();
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        selected_count > 0,
                        egui::Button::new(format!("Add selected PNGs ({selected_count})")),
                    )
                    .clicked()
                {
                    action = Some(ExplorerAction::AddCalibration(
                        self.calibration_candidates(true),
                    ));
                }
                if ui
                    .add_enabled(
                        directory_count > 0,
                        egui::Button::new(format!("Add directory PNGs ({directory_count})")),
                    )
                    .clicked()
                {
                    action = Some(ExplorerAction::AddCalibration(
                        self.calibration_candidates(false),
                    ));
                }
            });
            ui.weak("Calibration accepts original PNG files only.");
        }

        action
    }

    fn calibration_candidates(&self, selected_only: bool) -> Vec<CalibrationImportCandidate> {
        let Some(view) = self.active_view() else {
            return Vec::new();
        };
        view.entries
            .iter()
            .filter(|entry| entry.kind == FileKind::File && is_png_path(&entry.reference.path))
            .filter(|entry| !selected_only || view.selection.contains(&entry.reference.path))
            .map(|entry| CalibrationImportCandidate {
                display_path: view.display_path(&entry.reference.path),
                file_system: Arc::clone(&view.file_system),
                entry: entry.clone(),
                remote: view.is_remote(),
            })
            .collect()
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
                    selection,
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
                    selection,
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
                    selection,
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
                    selection,
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
        self.handle_browser_command_for_workspace(command, context, false)
    }

    fn handle_browser_command_for_workspace(
        &mut self,
        command: BrowserCommand,
        context: &egui::Context,
        calibration_import_enabled: bool,
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
                let result = match self.mode {
                    WorkspaceMode::Local => local_path_from_native(&value)
                        .and_then(|path| self.activate_local_path(path, context)),
                    #[cfg(feature = "platform-ssh")]
                    WorkspaceMode::Sftp => {
                        let parsed = self
                            .active_view()
                            .ok_or_else(|| "No active workspace source".to_owned())
                            .and_then(|_| source_path_from_remote(&value));
                        parsed.map(|path| self.navigate_to(path, context))
                    }
                };
                match result {
                    Ok(()) => self.browser.confirm_navigation(),
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
            BrowserCommand::Open(reference) => {
                if calibration_import_enabled {
                    self.calibration_open_action_for(&reference)
                } else {
                    self.open_action_for(&reference)
                }
            }
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

    fn activate_local_path(
        &mut self,
        path: PathBuf,
        context: &egui::Context,
    ) -> Result<(), String> {
        let view = SourceView::try_local(path)?;
        self.cancel_active_mutation();
        self.stop_active_monitor();
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
        view.selection.clear();
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
                        view.selection.clear();
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

    /// 返回当前 Explorer 目录的统一显式导出目标。
    ///
    /// SFTP source 只有在 host-key-pinned 文件系统声明 `write_new` capability 时可导出。
    pub(crate) fn active_save_destination(&self) -> Result<ExportDestination, String> {
        let view = self
            .active_view()
            .ok_or_else(|| "Select a local or pinned SSH directory before exporting.".to_owned())?;
        if !view.file_system.capabilities().write_new {
            let reason = if view.is_remote() {
                "SSH source is browse-only; pin and remount its host key before exporting."
            } else {
                "Active file source does not support creating export files."
            };
            return Err(reason.to_owned());
        }
        ExportDestination::new(view.directory_ref(), Arc::clone(&view.file_system))
            .map_err(|error| error.to_string())
    }

    #[must_use]
    pub(crate) fn active_save_target_label(&self) -> Option<String> {
        let view = self.active_view()?;
        Some(
            view.display_path(&view.current_directory)
                .display()
                .to_string(),
        )
    }

    /// 导出成功后仅在结果属于当前目录时重启该目录 monitor。
    pub(crate) fn refresh_save_destination(
        &mut self,
        destination: &ExportDestination,
        context: &egui::Context,
    ) {
        if self
            .active_view()
            .is_some_and(|view| &view.directory_ref() == destination.directory())
        {
            self.start_active_monitor(context);
        }
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

    fn calibration_open_action_for(&self, reference: &FileRef) -> Option<ExplorerAction> {
        let view = self
            .views()
            .find(|view| view.source_id() == &reference.source_id)?;
        let display_path = view.display_path(&reference.path);
        let entry = view
            .entries
            .iter()
            .find(|entry| entry.reference == *reference)?;
        if view.selection.contains(&reference.path) && view.selection.len() > 1 {
            let candidates = view
                .entries
                .iter()
                .filter(|entry| {
                    view.selection.contains(&entry.reference.path)
                        && entry.kind == FileKind::File
                        && is_png_path(&entry.reference.path)
                })
                .map(|entry| CalibrationImportCandidate {
                    display_path: view.display_path(&entry.reference.path),
                    file_system: Arc::clone(&view.file_system),
                    entry: entry.clone(),
                    remote: view.is_remote(),
                })
                .collect::<Vec<_>>();
            return (!candidates.is_empty()).then_some(ExplorerAction::AddCalibration(candidates));
        }

        if !is_png_path(&reference.path) {
            return Some(ExplorerAction::CalibrationImportRejected { display_path });
        }
        Some(ExplorerAction::AddCalibration(vec![
            CalibrationImportCandidate {
                display_path,
                file_system: Arc::clone(&view.file_system),
                entry: entry.clone(),
                remote: view.is_remote(),
            },
        ]))
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

fn is_png_path(path: &SourcePath) -> bool {
    path.file_name()
        .and_then(|name| Path::new(name).extension())
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
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

fn default_local_path() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
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
