#[cfg(feature = "platform-ssh")]
use std::collections::VecDeque;
use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "platform-ssh")]
use camera_toolbox_adapters::platforms::ssh_managed::{
    CredentialResolver, MemoryRemoteFile, MemorySshTransport, ProductionCredentialResolver,
    RusshTransportFactory, SshTransportFactory,
};
use camera_toolbox_app::{EntryName, FileEntry, FileKind, FileRef, FileVersion, SourcePath};
#[cfg(feature = "platform-ssh")]
use camera_toolbox_app::{
    RawDecodeParams, RawOpenMode, RawOpenPipeline, RemoteAuthentication, RemoteConnectionConfig,
    RemoteConnectionId, RemoteFileStat, SourceCache,
};
#[cfg(feature = "platform-ssh")]
use camera_toolbox_core::{BayerPattern, RawEncoding, RawSpec};
use eframe::egui::{self, accesskit::Role};

use super::*;

fn explorer_state() -> ExplorerState {
    #[cfg(feature = "platform-ssh")]
    {
        ExplorerState::new(
            Arc::new(ProductionCredentialResolver::new()),
            Arc::new(RusshTransportFactory),
        )
    }
    #[cfg(not(feature = "platform-ssh"))]
    {
        ExplorerState::new()
    }
}

fn explorer_with_file(directory: &PathBuf, name: &str) -> (ExplorerState, FileRef) {
    let mut explorer = explorer_state();
    let mut view = SourceView::try_local(directory.clone()).unwrap();
    let entry_name = EntryName::new(name).unwrap();
    let reference = FileRef::new(
        view.source_id().clone(),
        view.current_directory.join(&entry_name),
    );
    view.entries = vec![FileEntry {
        reference: reference.clone(),
        name: entry_name,
        kind: FileKind::File,
        version: FileVersion {
            size: fs::metadata(directory.join(name)).unwrap().len(),
            modified_millis: Some(1),
        },
    }];
    view.loaded_once = true;
    explorer.local_view = Some(view);
    (explorer, reference)
}

fn pointer_button(position: egui::Pos2, button: egui::PointerButton, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos: position,
        button,
        pressed,
        modifiers: egui::Modifiers::default(),
    }
}

fn render_frame(
    context: &egui::Context,
    explorer: &mut ExplorerState,
    time: f64,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, Option<ExplorerAction>) {
    render_frame_for_workspace(context, explorer, time, events, false)
}

fn render_frame_for_workspace(
    context: &egui::Context,
    explorer: &mut ExplorerState,
    time: f64,
    events: Vec<egui::Event>,
    calibration: bool,
) -> (egui::FullOutput, Option<ExplorerAction>) {
    let mut input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(440.0, 720.0),
        )),
        time: Some(time),
        ..Default::default()
    };
    input.events = events;
    let mut action = None;
    let output = context.run_ui(input, |ui| {
        action = explorer.render(context, ui, calibration);
    });
    (output, action)
}

fn visible_text(output: egui::FullOutput) -> String {
    output
        .platform_output
        .accesskit_update
        .expect("accessibility tree is enabled")
        .nodes
        .into_iter()
        .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
        .collect::<Vec<_>>()
        .join("\n")
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

fn temp_directory() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "camera-toolbox-explorer-test-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&directory).unwrap();
    directory
}

#[test]
fn local_workspace_replaces_old_mount_ui_labels() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut explorer = explorer_state();

    let (output, _) = render_frame(&context, &mut explorer, 0.0, Vec::new());
    let text = visible_text(output);

    for expected in ["Local", "Path", "Name", "Size", "[D] ..."] {
        assert!(text.contains(expected), "missing {expected:?}");
    }
    for removed in [
        "Root",
        "Open Folder",
        "Sources / Connections",
        "File Explorer",
        "Mount Local Path",
    ] {
        assert!(!text.contains(removed), "unexpected {removed:?}");
    }
}

#[cfg(feature = "platform-ssh")]
#[test]
fn sftp_mode_exposes_ephemeral_login_fields() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut explorer = explorer_state();
    explorer.mode = WorkspaceMode::Sftp;

    let (output, _) = render_frame(&context, &mut explorer, 0.0, Vec::new());
    let text = visible_text(output);

    for expected in ["SFTP", "Endpoint", "Password", "Connect"] {
        assert!(text.contains(expected), "missing {expected:?}");
    }
    for removed in ["Root", "Expected host key", "Password slot", "Saved remote"] {
        assert!(!text.contains(removed), "unexpected {removed:?}");
    }
}

#[test]
fn double_click_file_emits_one_open_action() {
    let directory = temp_directory();
    fs::write(directory.join("frame.raw"), [1_u8, 2, 3, 4]).unwrap();
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut explorer = explorer_state();
    let mut view = SourceView::try_local(directory.clone()).unwrap();
    let reference = FileRef::new(
        view.source_id().clone(),
        SourcePath::new("frame.raw").unwrap(),
    );
    view.entries = vec![FileEntry {
        reference: reference.clone(),
        name: EntryName::new("frame.raw").unwrap(),
        kind: FileKind::File,
        version: FileVersion {
            size: 4,
            modified_millis: Some(1),
        },
    }];
    view.loaded_once = true;
    explorer.local_view = Some(view);

    let (output, _) = render_frame(&context, &mut explorer, 0.0, Vec::new());
    let position = button_center(&output, "[F] frame.raw");
    let press = |time, explorer: &mut ExplorerState| {
        render_frame(
            &context,
            explorer,
            time,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        )
        .1
    };
    let release = |time, explorer: &mut ExplorerState| {
        render_frame(
            &context,
            explorer,
            time,
            vec![egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
        )
        .1
    };

    assert!(press(0.10, &mut explorer).is_none());
    assert!(release(0.11, &mut explorer).is_none());
    assert!(press(0.20, &mut explorer).is_none());
    let action = release(0.21, &mut explorer).expect("double click emits open");
    let ExplorerAction::OpenAuto {
        reference: opened,
        remote,
        ..
    } = action
    else {
        panic!("expected file open action");
    };
    assert_eq!(opened, reference);
    assert!(!remote);

    drop(explorer);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn double_click_png_routes_to_calibration_dataset() {
    let directory = temp_directory();
    fs::write(directory.join("board.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    let context = egui::Context::default();
    context.enable_accesskit();
    let (mut explorer, reference) = explorer_with_file(&directory, "board.png");

    let (output, _) = render_frame_for_workspace(&context, &mut explorer, 0.0, Vec::new(), true);
    let position = button_center(&output, "[F] board.png");
    for (time, pressed) in [(0.10, true), (0.11, false), (0.20, true)] {
        let (_, action) = render_frame_for_workspace(
            &context,
            &mut explorer,
            time,
            vec![
                egui::Event::PointerMoved(position),
                pointer_button(position, egui::PointerButton::Primary, pressed),
            ],
            true,
        );
        assert!(action.is_none());
    }
    let (_, action) = render_frame_for_workspace(
        &context,
        &mut explorer,
        0.21,
        vec![pointer_button(
            position,
            egui::PointerButton::Primary,
            false,
        )],
        true,
    );
    let ExplorerAction::AddCalibration(candidates) = action.expect("double click adds PNG") else {
        panic!("expected calibration dataset action");
    };
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].entry.reference, reference);
    assert_eq!(candidates[0].display_path, directory.join("board.png"));
    let stat = candidates[0]
        .file_system
        .stat(
            &candidates[0].entry.reference,
            &FsControl::with_timeout(Duration::from_secs(1)),
        )
        .expect("calibration candidate resolves to the real file");
    assert_eq!(stat.reference, reference);

    drop(explorer);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn open_command_in_calibration_routes_to_dataset() {
    let directory = temp_directory();
    fs::write(directory.join("board.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    let context = egui::Context::default();
    let (mut explorer, reference) = explorer_with_file(&directory, "board.png");

    let action = explorer
        .handle_browser_command_for_workspace(
            BrowserCommand::Open(reference.clone()),
            &context,
            true,
        )
        .expect("Open adds PNG");
    let ExplorerAction::AddCalibration(candidates) = action else {
        panic!("expected calibration dataset action");
    };
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].entry.reference, reference);

    drop(explorer);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn open_command_in_calibration_rejects_non_png() {
    let directory = temp_directory();
    fs::write(directory.join("frame.jpg"), [1_u8, 2, 3, 4]).unwrap();
    let context = egui::Context::default();
    let (mut explorer, reference) = explorer_with_file(&directory, "frame.jpg");

    let action = explorer
        .handle_browser_command_for_workspace(BrowserCommand::Open(reference), &context, true)
        .expect("unsupported file reports rejection");
    let ExplorerAction::CalibrationImportRejected { display_path } = action else {
        panic!("expected explicit calibration import rejection");
    };
    assert_eq!(display_path, directory.join("frame.jpg"));

    drop(explorer);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn file_context_menu_exposes_required_commands() {
    let directory = temp_directory();
    fs::write(directory.join("frame.raw"), [1_u8, 2, 3, 4]).unwrap();
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut explorer = explorer_state();
    let mut view = SourceView::try_local(directory.clone()).unwrap();
    let reference = FileRef::new(
        view.source_id().clone(),
        SourcePath::new("frame.raw").unwrap(),
    );
    view.entries = vec![FileEntry {
        reference,
        name: EntryName::new("frame.raw").unwrap(),
        kind: FileKind::File,
        version: FileVersion {
            size: 4,
            modified_millis: Some(1),
        },
    }];
    view.loaded_once = true;
    explorer.local_view = Some(view);

    let (output, _) = render_frame(&context, &mut explorer, 0.0, Vec::new());
    let position = button_center(&output, "[F] frame.raw");
    render_frame(
        &context,
        &mut explorer,
        0.1,
        vec![
            egui::Event::PointerMoved(position),
            egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Secondary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
        ],
    );
    let (output, action) = render_frame(
        &context,
        &mut explorer,
        0.11,
        vec![egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Secondary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        }],
    );

    assert!(action.is_none());
    let text = visible_text(output);
    for expected in ["Open", "Rename", "Delete", "New Folder"] {
        assert!(text.contains(expected), "missing {expected:?}");
    }

    drop(explorer);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn applying_local_path_populates_browser_from_monitor_baseline() {
    let directory = temp_directory();
    fs::write(directory.join("fresh.raw"), [1_u8, 2, 3, 4]).unwrap();
    let context = egui::Context::default();
    let mut explorer = explorer_state();

    explorer
        .activate_local_path(directory.clone(), &context)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !explorer
        .local_view
        .as_ref()
        .is_some_and(|view| view.loaded_once)
        && Instant::now() < deadline
    {
        explorer.poll_monitor(&context);
        std::thread::sleep(Duration::from_millis(10));
    }

    let view = explorer.local_view.as_ref().unwrap();
    assert_eq!(view.entries.len(), 1);
    assert_eq!(view.entries[0].name.as_str(), "fresh.raw");
    assert_eq!(view.navigation_path(), directory.display().to_string());

    drop(explorer);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn local_absolute_path_navigation_is_unrestricted_and_synchronized() {
    let directory = temp_directory();
    let sibling = temp_directory();
    fs::create_dir(directory.join("nested")).unwrap();
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut explorer = explorer_state();
    explorer.local_view = Some(SourceView::try_local(directory.clone()).unwrap());
    let source_id = explorer.local_view.as_ref().unwrap().source_id().clone();

    let sibling_path = sibling.display().to_string();
    assert!(
        explorer
            .handle_browser_command(BrowserCommand::NavigatePath(sibling_path.clone()), &context)
            .is_none()
    );
    let view = explorer.local_view.as_ref().unwrap();
    assert_eq!(view.source_id(), &source_id);
    assert_eq!(view.navigation_path(), sibling_path);

    let (output, _) = render_frame(&context, &mut explorer, 0.0, Vec::new());
    assert!(visible_text(output).contains(&sibling.display().to_string()));
    assert!(
        explorer
            .handle_browser_command(BrowserCommand::NavigateUp, &context)
            .is_none()
    );
    assert_eq!(
        explorer.local_view.as_ref().unwrap().navigation_path(),
        sibling.parent().unwrap().display().to_string()
    );

    let nested_path = directory.join("nested").display().to_string();
    assert!(
        explorer
            .handle_browser_command(BrowserCommand::NavigatePath(nested_path.clone()), &context)
            .is_none()
    );
    assert_eq!(
        explorer.local_view.as_ref().unwrap().navigation_path(),
        nested_path
    );
    let stable_path = explorer.local_view.as_ref().unwrap().navigation_path();
    assert!(
        explorer
            .handle_browser_command(
                BrowserCommand::NavigatePath("relative".to_owned()),
                &context
            )
            .is_none()
    );
    assert_eq!(
        explorer.local_view.as_ref().unwrap().navigation_path(),
        stable_path
    );
    assert!(
        explorer
            .handle_browser_command(
                BrowserCommand::NavigatePath("../escape".to_owned()),
                &context
            )
            .is_none()
    );
    assert_eq!(
        explorer.local_view.as_ref().unwrap().navigation_path(),
        stable_path
    );

    assert_eq!(source_path_from_remote("/").unwrap(), SourcePath::root());
    for (absolute, relative) in [
        ("/opt/archive/raw", "opt/archive/raw"),
        ("/home/root", "home/root"),
        ("/tmp", "tmp"),
    ] {
        assert_eq!(
            source_path_from_remote(absolute).unwrap(),
            SourcePath::directory(relative).unwrap()
        );
    }
    assert!(source_path_from_remote("opt/relative").is_err());
    assert!(source_path_from_remote("/opt/../etc").is_err());

    drop(explorer);
    fs::remove_dir_all(directory).unwrap();
    fs::remove_dir_all(sibling).unwrap();
}

#[test]
fn directory_changes_remain_sorted_with_directories_first() {
    let source = FileSourceId::new("source").unwrap();
    let file = |name: &str, kind| FileEntry {
        reference: FileRef::new(source.clone(), SourcePath::new(name).unwrap()),
        name: EntryName::new(name).unwrap(),
        kind,
        version: FileVersion {
            size: 1,
            modified_millis: Some(1),
        },
    };
    let mut entries = vec![file("z.raw", FileKind::File)];

    apply_directory_changes(
        &mut entries,
        vec![DirectoryChange::Created(file(
            "folder",
            FileKind::Directory,
        ))],
    );

    assert_eq!(entries[0].name.as_str(), "folder");
    assert_eq!(entries[1].name.as_str(), "z.raw");
}

#[cfg(feature = "platform-ssh")]
fn remote_file(path: &str, bytes: &[u8]) -> MemoryRemoteFile {
    MemoryRemoteFile {
        bytes: bytes.to_vec(),
        stats: VecDeque::from([RemoteFileStat {
            path: path.to_owned(),
            size: u64::try_from(bytes.len()).unwrap(),
            modified_seconds: 7,
            producer_marker: None,
            sha256: None,
        }]),
    }
}
#[cfg(feature = "platform-ssh")]
fn wait_for_mutation(explorer: &mut ExplorerState, context: &egui::Context) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while explorer.active_mutation.is_some() && Instant::now() < deadline {
        explorer.poll_mutation(context);
        std::thread::sleep(Duration::from_millis(5));
    }
    explorer.poll_mutation(context);
    assert!(
        explorer.active_mutation.is_none(),
        "SFTP mutation timed out"
    );
}
#[cfg(feature = "platform-ssh")]
fn wait_for_sftp_directory(
    explorer: &mut ExplorerState,
    context: &egui::Context,
    expected: &SourcePath,
) {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        explorer.poll_monitor(context);
        let ready = explorer.sftp_view.as_ref().is_some_and(|view| {
            &view.current_directory == expected && view.loaded_once && !view.loading
        });
        if ready || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let view = explorer.sftp_view.as_ref().unwrap();
    assert_eq!(&view.current_directory, expected);
    assert!(view.loaded_once, "SFTP directory did not load");
    assert!(!view.loading, "SFTP directory remained loading");
    assert!(
        view.error.is_none(),
        "SFTP directory error: {:?}",
        view.error
    );
}

#[cfg(feature = "platform-ssh")]
#[test]
fn sftp_workspace_loads_and_polls_session_source() {
    let memory = Arc::new(MemorySshTransport::new("memory-host-key"));
    memory.allow_credential("session:test");
    memory.insert_file(
        "/opt/first.raw",
        remote_file("/opt/first.raw", &[1, 2, 3, 4]),
    );
    memory.insert_file(
        "/tmp/second.raw",
        remote_file("/tmp/second.raw", &[5, 6, 7, 8]),
    );
    let credentials: Arc<dyn CredentialResolver> = memory.clone();
    let transport: Arc<dyn SshTransportFactory> = memory.clone();
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut explorer = ExplorerState::new(credentials, transport);
    explorer
        .finish_sftp_connection(
            RemoteConnectionConfig {
                id: RemoteConnectionId::new("memory").unwrap(),
                display_name: "memory".to_owned(),
                host: "camera.test".to_owned(),
                port: 22,
                username: "root".to_owned(),
                expected_host_key: None,
                authentication: RemoteAuthentication::Password {
                    slot_id: "test".to_owned(),
                },
            },
            &context,
        )
        .unwrap();

    assert_eq!(explorer.sftp_view.as_ref().unwrap().navigation_path(), "/");
    assert!(
        explorer
            .handle_browser_command(BrowserCommand::NavigatePath("/opt".to_owned()), &context)
            .is_none()
    );
    assert_eq!(
        explorer.sftp_view.as_ref().unwrap().current_directory,
        SourcePath::directory("opt").unwrap()
    );
    assert_eq!(
        explorer.sftp_view.as_ref().unwrap().navigation_path(),
        "/opt"
    );

    wait_for_sftp_directory(
        &mut explorer,
        &context,
        &SourcePath::directory("opt").unwrap(),
    );
    let (output, _) = render_frame(&context, &mut explorer, 0.0, Vec::new());
    let text = visible_text(output);
    for expected in ["Path", "Name", "Size", "[D] ..."] {
        assert!(text.contains(expected), "missing {expected:?}");
    }
    assert!(!text.contains("Root"));
    assert_eq!(
        explorer.sftp_view.as_ref().unwrap().navigation_path(),
        "/opt"
    );
    assert_eq!(
        explorer
            .sftp_view
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["first.raw"]
    );

    assert!(
        explorer
            .handle_browser_command(BrowserCommand::NavigatePath("/tmp".to_owned()), &context)
            .is_none()
    );
    wait_for_sftp_directory(
        &mut explorer,
        &context,
        &SourcePath::directory("tmp").unwrap(),
    );
    assert_eq!(
        explorer.sftp_view.as_ref().unwrap().navigation_path(),
        "/tmp"
    );
    assert_eq!(
        explorer
            .sftp_view
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["second.raw"]
    );

    assert!(
        explorer
            .handle_browser_command(BrowserCommand::NavigatePath("/opt".to_owned()), &context)
            .is_none()
    );
    wait_for_sftp_directory(
        &mut explorer,
        &context,
        &SourcePath::directory("opt").unwrap(),
    );

    let first = explorer.sftp_view.as_ref().unwrap().entries[0]
        .reference
        .clone();
    assert!(matches!(
        explorer.open_action_for(&first),
        Some(ExplorerAction::OpenAuto { remote: true, .. })
    ));

    let source = first.source_id.clone();
    let file_system = Arc::clone(&explorer.sftp_view.as_ref().unwrap().file_system);
    explorer.begin_mutation(
        MutationRequest::CreateDirectory {
            parent: DirectoryRef::new(source.clone(), SourcePath::directory("opt").unwrap()),
            name: EntryName::new("archive").unwrap(),
        },
        &context,
    );
    wait_for_mutation(&mut explorer, &context);
    assert!(memory.contains_path("/opt/archive"));

    explorer.begin_mutation(
        MutationRequest::Rename {
            reference: first,
            new_name: EntryName::new("renamed.raw").unwrap(),
        },
        &context,
    );
    wait_for_mutation(&mut explorer, &context);
    assert!(memory.contains_path("/opt/renamed.raw"));

    let renamed = FileRef::new(source.clone(), SourcePath::new("opt/renamed.raw").unwrap());
    let renamed_entry = file_system
        .stat(&renamed, &FsControl::with_timeout(Duration::from_secs(1)))
        .unwrap();
    explorer.begin_mutation(
        MutationRequest::Move {
            entry: renamed_entry,
            destination: DirectoryRef::new(
                source.clone(),
                SourcePath::directory("opt/archive").unwrap(),
            ),
        },
        &context,
    );
    wait_for_mutation(&mut explorer, &context);
    assert!(memory.contains_path("/opt/archive/renamed.raw"));

    let moved = FileRef::new(
        source.clone(),
        SourcePath::new("opt/archive/renamed.raw").unwrap(),
    );
    let moved_entry = file_system
        .stat(&moved, &FsControl::with_timeout(Duration::from_secs(1)))
        .unwrap();
    explorer.begin_mutation(MutationRequest::Delete { entry: moved_entry }, &context);
    wait_for_mutation(&mut explorer, &context);
    assert!(!memory.contains_path("/opt/archive/renamed.raw"));

    let archive = FileRef::new(source, SourcePath::new("opt/archive").unwrap());
    let archive_entry = file_system
        .stat(&archive, &FsControl::with_timeout(Duration::from_secs(1)))
        .unwrap();
    explorer.begin_mutation(
        MutationRequest::Delete {
            entry: archive_entry,
        },
        &context,
    );
    wait_for_mutation(&mut explorer, &context);
    assert!(!memory.contains_path("/opt/archive"));

    memory.insert_file(
        "/opt/first.raw",
        remote_file("/opt/first.raw", &[1, 0, 2, 0, 3, 0, 4, 0]),
    );
    memory.insert_file("/opt/later.raw", remote_file("/opt/later.raw", &[5, 6]));
    let poll_deadline = Instant::now() + Duration::from_secs(3);
    while explorer
        .sftp_view
        .as_ref()
        .is_none_or(|view| view.entries.len() < 2)
        && Instant::now() < poll_deadline
    {
        explorer.poll_monitor(&context);
        std::thread::sleep(Duration::from_millis(10));
    }
    let names = explorer
        .sftp_view
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["first.raw", "later.raw"]);
    let first_reference = explorer
        .sftp_view
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .find(|entry| entry.name.as_str() == "first.raw")
        .unwrap()
        .reference
        .clone();
    let pipeline = RawOpenPipeline::new(SourceCache::new(32, 2).unwrap(), Vec::new(), 32);
    let params = RawDecodeParams {
        spec: RawSpec {
            width: 2,
            height: 2,
            bit_depth: 12,
            bayer: BayerPattern::Rggb,
        },
        encoding: RawEncoding::U16Le,
    };
    let control = FsControl::with_timeout(Duration::from_secs(1));
    let mut progress = Vec::new();
    let opened = pipeline
        .begin_with_progress(
            &*file_system,
            &first_reference,
            RawOpenMode::WithOptions(params.clone()),
            &control,
            &mut |update| progress.push(update),
        )
        .unwrap();
    assert_eq!(opened.frame.pixels(), &[1, 2, 3, 4]);
    assert_eq!(progress.first().unwrap().bytes_read, 0);
    assert_eq!(progress.last().unwrap().bytes_read, 8);
    assert!(
        progress
            .windows(2)
            .all(|pair| pair[0].bytes_read <= pair[1].bytes_read)
    );
    assert_eq!(memory.read_calls(), 1);

    let source = opened.source;
    drop(explorer);
    drop(file_system);
    drop(memory);
    let reinterpreted = pipeline.reinterpret(source, params, 9, &control).unwrap();
    assert_eq!(reinterpreted.generation, Some(9));
    assert_eq!(reinterpreted.frame.pixels(), &[1, 2, 3, 4]);
}

#[test]
fn local_mutation_requests_execute_through_filesystem_port() {
    let directory = temp_directory();
    fs::write(directory.join("before.raw"), [1_u8, 2]).unwrap();
    let view = SourceView::try_local(directory.clone()).unwrap();
    let source = view.source_id().clone();
    let workspace = view.current_directory.clone();
    let file_system = Arc::clone(&view.file_system);

    let before_name = EntryName::new("before.raw").unwrap();
    let before = FileRef::new(source.clone(), workspace.join(&before_name));
    execute_mutation(
        &*file_system,
        &MutationRequest::Rename {
            reference: before,
            new_name: EntryName::new("renamed.raw").unwrap(),
        },
        FsCancellation::default(),
    )
    .unwrap();
    assert!(directory.join("renamed.raw").is_file());

    let destination_name = EntryName::new("destination").unwrap();
    let destination = workspace.join(&destination_name);
    execute_mutation(
        &*file_system,
        &MutationRequest::CreateDirectory {
            parent: DirectoryRef::new(source.clone(), workspace.clone()),
            name: destination_name,
        },
        FsCancellation::default(),
    )
    .unwrap();
    assert!(directory.join("destination").is_dir());

    let renamed_name = EntryName::new("renamed.raw").unwrap();
    let renamed = FileRef::new(source.clone(), workspace.join(&renamed_name));
    let entry = file_system
        .stat(&renamed, &FsControl::with_timeout(Duration::from_secs(1)))
        .unwrap();
    execute_mutation(
        &*file_system,
        &MutationRequest::Move {
            entry,
            destination: DirectoryRef::new(source.clone(), destination.clone()),
        },
        FsCancellation::default(),
    )
    .unwrap();
    assert!(directory.join("destination/renamed.raw").is_file());

    let moved = FileRef::new(source, destination.join(&renamed_name));
    let entry = file_system
        .stat(&moved, &FsControl::with_timeout(Duration::from_secs(1)))
        .unwrap();
    execute_mutation(
        &*file_system,
        &MutationRequest::Delete { entry },
        FsCancellation::default(),
    )
    .unwrap();
    assert!(!directory.join("destination/renamed.raw").exists());

    drop(view);
    fs::remove_dir_all(directory).unwrap();
}
