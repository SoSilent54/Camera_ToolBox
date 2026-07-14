use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use camera_toolbox_adapters::{
    PlatformRegistry,
    platforms::{
        hisilicon_cv610::HisiliconCv610Provider,
        ssh_managed::{
            CommandParameterKind, CommandParameterSpec, CommandRecipe, CommandRecipeRegistry,
            CredentialResolver, ProductionCredentialResolver, RecipeArg,
            SshManagedPlatformProvider,
        },
    },
};
use camera_toolbox_app::{
    CapabilityKind, Cv610Config, Cv610DumpConfig, Cv610StreamConfig, DumpJobEvent, DumpJobState,
    DumpServiceError, EvidenceMaturity, EvidencedCapabilityDeclaration, LocalConfig, OperationId,
    PlatformBindings, PlatformConfig, PlatformProfile, PlatformProfileId, ProfileStore,
    RemoteJobEvent, RemoteJobState, ResolvedTargetBindings, SensorDescriptor, SensorId,
    SensorModeKey, SensorPlatformCapabilityCell, SensorSelection, SshManagedConfig,
    StreamServiceEvent, StreamSessionEvent, StreamSessionId, StreamStage,
};
use camera_toolbox_core::{
    AssetId, BayerPattern, CaptureMetadata, EphemeralAsset, IntegrityState, MediaFormat,
    OwnedMediaPayload, RawSpec,
    sensor::{SensorMode, SensorProfile},
};
use clap::Parser;
use ratatui::{Terminal, backend::TestBackend};

use crate::{
    Args, RemoteFormatArg,
    production::ProductionBindingBackend,
    state::{BindingBackend, ConsoleAction, ConsoleState, ControllerEvent},
    terminal::{TerminalOps, TerminalRestore},
    ui::{UiState, render},
    write_fatal_error,
};

struct CountingBackend {
    registry: PlatformRegistry,
    count: Arc<AtomicUsize>,
}

impl BindingBackend for CountingBackend {
    fn bind(&self, profile: &PlatformProfile) -> Result<PlatformBindings, String> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.registry
            .bind(profile)
            .map_err(|error| error.to_string())
    }
}

fn test_backend(count: Arc<AtomicUsize>) -> Box<dyn BindingBackend> {
    let credentials: Arc<dyn CredentialResolver> = Arc::new(ProductionCredentialResolver::new());
    let ssh =
        SshManagedPlatformProvider::production(credentials, Arc::new(CommandRecipeRegistry::new()));
    Box::new(CountingBackend {
        registry: PlatformRegistry::new(HisiliconCv610Provider::default(), ssh),
        count,
    })
}

fn local_profile(id: &str) -> PlatformProfile {
    PlatformProfile {
        id: PlatformProfileId::new(id).unwrap(),
        display_name: format!("Local {id}"),
        config: PlatformConfig::Local(LocalConfig::default()),
    }
}

fn cv610_profile(id: &str, preferred_raw_bits: u8) -> PlatformProfile {
    PlatformProfile {
        id: PlatformProfileId::new(id).unwrap(),
        display_name: format!("CV610 {id}"),
        config: PlatformConfig::HisiliconCv610(Cv610Config {
            host: "127.0.0.1".to_owned(),
            dump: Cv610DumpConfig {
                preferred_raw_bits,
                ..Cv610DumpConfig::default()
            },
            stream: Cv610StreamConfig::default(),
        }),
    }
}

fn sensor() -> SensorDescriptor {
    SensorDescriptor {
        id: SensorId::new("imx-test").unwrap(),
        display_name: "IMX Test".to_owned(),
        profile: SensorProfile {
            model: "imx-test".to_owned(),
            modes: vec![SensorMode {
                id: "1080p30".to_owned(),
                raw: RawSpec {
                    width: 1920,
                    height: 1080,
                    bit_depth: 12,
                    bayer: BayerPattern::Rggb,
                },
                is_wdr: false,
            }],
            registers: Vec::new(),
        },
    }
}

fn state_with(store: ProfileStore, args: Args) -> (ConsoleState, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let state = ConsoleState::new(
        args,
        store,
        test_backend(Arc::clone(&count)),
        Arc::new(CommandRecipeRegistry::new()),
        None,
    )
    .unwrap();
    (state, count)
}

fn profile_index(state: &ConsoleState, id: &str) -> usize {
    state
        .profiles
        .platforms()
        .position(|profile| profile.id.as_str() == id)
        .unwrap()
}

fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
}

#[test]
fn test_backend_render_contains_all_acceptance_panes() {
    let mut store = ProfileStore::new();
    store.insert_platform(local_profile("local")).unwrap();
    store.insert_sensor(sensor()).unwrap();
    let (state, _) = state_with(store, Args::default());
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| render(frame, &state, &UiState::default()))
        .unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("Platform profiles"));
    assert!(text.contains("Sensor / Mode"));
    assert!(text.contains("Resolved capabilities / evidence"));
    assert!(text.contains("Jobs"));
    assert!(text.contains("Assets"));
    assert!(text.contains("Typed controller event log"));
    assert!(text.contains("? Help"));
}

#[test]
fn unbound_selection_resolves_platform_only_capabilities() {
    let mut store = ProfileStore::new();
    store.insert_platform(local_profile("local")).unwrap();
    let (state, count) = state_with(store, Args::default());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let snapshot = state.snapshot.as_ref().unwrap();
    assert!(matches!(snapshot.key.sensor, SensorSelection::Unbound));
    assert!(
        snapshot
            .bindings
            .capability(CapabilityKind::LocalOpen)
            .is_some()
    );
}

#[test]
fn sensor_switch_reuses_bound_candidate_without_provider_rebind() {
    let mut store = ProfileStore::new();
    store.insert_platform(local_profile("local")).unwrap();
    store.insert_sensor(sensor()).unwrap();
    let (mut state, count) = state_with(store, Args::default());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    state.select_sensor_index(1);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(state.bind_count, 1);
    assert!(matches!(
        state.snapshot.as_ref().unwrap().key.sensor,
        SensorSelection::Mode(_)
    ));
}

#[test]
fn platform_change_rebinds_and_replaces_candidate_variant() {
    let mut store = ProfileStore::new();
    store.insert_platform(local_profile("local")).unwrap();
    store.insert_platform(cv610_profile("cv610", 10)).unwrap();
    let (mut state, count) = state_with(store, Args::default());
    let cv_index = profile_index(&state, "cv610");
    let local_index = profile_index(&state, "local");

    state.select_platform_index(cv_index);
    assert!(matches!(
        state.snapshot.as_ref().unwrap().bindings.as_ref(),
        ResolvedTargetBindings::Cv610(_)
    ));
    state.select_platform_index(local_index);
    assert!(matches!(
        state.snapshot.as_ref().unwrap().bindings.as_ref(),
        ResolvedTargetBindings::Local(_)
    ));
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[test]
fn typed_dump_stream_and_remote_events_route_to_independent_jobs() {
    let mut store = ProfileStore::new();
    store.insert_platform(local_profile("local")).unwrap();
    let (mut state, _) = state_with(store, Args::default());
    let operation_id = OperationId::new("dump-event").unwrap();
    let asset_id = AssetId::new("routed-asset").unwrap();
    let reservation = state
        .capture_store
        .reserve(operation_id.clone(), 3)
        .unwrap();
    state
        .capture_store
        .publish_validated(
            reservation,
            EphemeralAsset::new(
                asset_id.clone(),
                OwnedMediaPayload::from_bytes(Arc::<[u8]>::from(&b"raw"[..])),
                CaptureMetadata {
                    format: MediaFormat::Binary,
                    source_name: "fixture".to_owned(),
                    attributes: BTreeMap::new(),
                },
                IntegrityState::Verified {
                    algorithm: "fixture".to_owned(),
                    digest: "fixture".to_owned(),
                },
            ),
        )
        .unwrap();
    state.route_event(ControllerEvent::Dump(DumpJobEvent {
        operation_id,
        service_id: "dump-service".to_owned(),
        state: DumpJobState::Succeeded {
            asset_id: asset_id.clone(),
            payload_sha256: "fixture".to_owned(),
        },
    }));
    state.route_event(ControllerEvent::Stream(StreamSessionEvent {
        session_id: StreamSessionId::new("stream-event").unwrap(),
        service_id: "stream-service".to_owned(),
        event: StreamServiceEvent::Stage(StreamStage::Connecting),
    }));
    state.route_event(ControllerEvent::Remote(RemoteJobEvent {
        operation_id: OperationId::new("remote-event").unwrap(),
        service_id: "remote-service".to_owned(),
        state: RemoteJobState::Queued,
    }));
    assert_eq!(state.jobs.len(), 3);
    assert!(state.jobs.iter().any(|(id, _)| id == "dump-event"));
    assert!(state.jobs.iter().any(|(id, _)| id == "stream-event"));
    assert!(state.jobs.iter().any(|(id, _)| id == "remote-event"));
    assert!(state.assets.contains(&asset_id));
    assert!(state.capture_store.get(&asset_id).unwrap().is_some());
    assert_eq!(state.event_log.len(), 5); // bind, three typed events, retained asset
}

#[test]
fn typed_terminal_failure_is_presented_in_status() {
    let mut store = ProfileStore::new();
    store.insert_platform(local_profile("local")).unwrap();
    let (mut state, _) = state_with(store, Args::default());
    state.route_event(ControllerEvent::Dump(DumpJobEvent {
        operation_id: OperationId::new("dump-failure").unwrap(),
        service_id: "dump-service".to_owned(),
        state: DumpJobState::Failed(DumpServiceError::InvalidRequest(
            "actionable fixture".to_owned(),
        )),
    }));
    assert!(
        state
            .error
            .as_deref()
            .unwrap()
            .contains("actionable fixture")
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| render(frame, &state, &UiState::default()))
        .unwrap();
    assert!(rendered_text(&terminal).contains("actionable fixture"));
}

#[test]
fn actions_remain_disabled_without_matching_handles_or_explicit_files() {
    let mut local_store = ProfileStore::new();
    local_store.insert_platform(local_profile("local")).unwrap();
    let (mut local, _) = state_with(local_store, Args::default());
    assert!(local.action_status(ConsoleAction::Dump).is_err());
    local.perform(ConsoleAction::Dump);
    assert!(local.error.as_deref().unwrap().contains("disabled"));

    let mut cv_store = ProfileStore::new();
    cv_store
        .insert_platform(cv610_profile("cv610", 12))
        .unwrap();
    let (cv, _) = state_with(cv_store, Args::default());
    assert!(cv.action_status(ConsoleAction::Dump).is_ok());
    let stream_error = cv.action_status(ConsoleAction::StreamRecord).unwrap_err();
    assert!(stream_error.contains("--stream-duration-secs"));

    let path = std::env::temp_dir().join(format!(
        "camera-toolbox-tui-sparse-key-{}",
        std::process::id()
    ));
    fs::write(
        &path,
        "-----BEGIN OPENSSH PRIVATE KEY-----\nfixture\n-----END OPENSSH PRIVATE KEY-----\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let absolute = path.canonicalize().unwrap();
    let profile = ssh_profile("sparse", format!("key-file:{}", absolute.display()));
    let platform_id = profile.id.clone();
    let sparse_sensor = sensor();
    let sensor_mode = SensorModeKey {
        sensor_id: sparse_sensor.id.clone(),
        mode_id: sparse_sensor.profile.modes[0].id.clone(),
    };
    let mut ssh_store = ProfileStore::new();
    ssh_store.insert_platform(profile).unwrap();
    ssh_store.insert_sensor(sparse_sensor).unwrap();
    ssh_store
        .insert_capability_cell(SensorPlatformCapabilityCell {
            sensor_mode,
            platform_id,
            restrictions: Vec::new(),
            declarations: vec![EvidencedCapabilityDeclaration {
                capability: CapabilityKind::RemoteFile,
                supported: false,
                evidence: EvidenceMaturity::ProtocolVerified,
            }],
        })
        .unwrap();
    let recipes = recipe_registry();
    let mut ssh = ConsoleState::new(
        Args {
            capture_format: Some("jpeg".to_owned()),
            remote_format: Some(RemoteFormatArg::Jpeg),
            ..Args::default()
        },
        ssh_store,
        Box::new(ProductionBindingBackend::new(Arc::clone(&recipes))),
        recipes,
        None,
    )
    .unwrap();
    ssh.select_sensor_index(1);
    let ResolvedTargetBindings::SshManaged(bindings) =
        ssh.snapshot.as_ref().unwrap().bindings.as_ref()
    else {
        panic!("expected SSH-managed resolution")
    };
    assert!(bindings.command.is_some());
    assert!(bindings.remote_file.is_none());
    let capture_error = ssh.action_status(ConsoleAction::SshCapture).unwrap_err();
    assert!(capture_error.contains("RemoteFile handle"));
    let _ = fs::remove_file(path);
}

#[test]
fn snapshot_mode_is_deterministic_and_uses_no_terminal() {
    let args = Args::try_parse_from(["camera-toolbox-tui", "--snapshot"]).unwrap();
    assert!(args.snapshot);
    let mut store = ProfileStore::new();
    store.insert_platform(local_profile("local")).unwrap();
    let (state, _) = state_with(store, args);
    let first = state.snapshot_text();
    let second = state.snapshot_text();
    assert_eq!(first, second);
    let output = first;
    assert!(output.contains("Camera Toolbox Platform Console Snapshot"));
    assert!(output.contains("sensor: Unbound"));
    assert!(output.contains("resolution: resolved"));
    assert!(output.contains("LocalOpen"));
}

#[derive(Clone)]
struct MemoryTerminalOps {
    calls: Arc<Mutex<Vec<&'static str>>>,
    fail_enter: bool,
}

impl MemoryTerminalOps {
    fn call(&self, name: &'static str) {
        self.calls.lock().unwrap().push(name);
    }
}

impl TerminalOps for MemoryTerminalOps {
    fn enable_raw(&mut self) -> io::Result<()> {
        self.call("enable_raw");
        Ok(())
    }

    fn enter_alternate(&mut self) -> io::Result<()> {
        self.call("enter_alternate");
        if self.fail_enter {
            Err(io::Error::other("injected alternate-screen failure"))
        } else {
            Ok(())
        }
    }

    fn enable_mouse(&mut self) -> io::Result<()> {
        self.call("enable_mouse");
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.call("hide_cursor");
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.call("show_cursor");
        Ok(())
    }

    fn disable_mouse(&mut self) -> io::Result<()> {
        self.call("disable_mouse");
        Ok(())
    }

    fn leave_alternate(&mut self) -> io::Result<()> {
        self.call("leave_alternate");
        Ok(())
    }

    fn disable_raw(&mut self) -> io::Result<()> {
        self.call("disable_raw");
        Ok(())
    }
}

#[test]
fn terminal_enter_failure_restores_partial_transitions_and_is_presentable() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let result = TerminalRestore::enter_with(MemoryTerminalOps {
        calls: Arc::clone(&calls),
        fail_enter: true,
    });
    let error = match result {
        Ok(_) => panic!("injected enter failure unexpectedly succeeded"),
        Err(error) => error,
    };
    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            "enable_raw",
            "enter_alternate",
            "leave_alternate",
            "disable_raw"
        ]
    );
    let mut output = Vec::new();
    write_fatal_error(&mut output, error.root_cause()).unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "Camera Toolbox TUI error: injected alternate-screen failure\n"
    );
}

fn recipe_registry() -> Arc<CommandRecipeRegistry> {
    let mut recipes = CommandRecipeRegistry::new();
    recipes
        .register(CommandRecipe {
            id: "capture".to_owned(),
            program: "/usr/bin/capture".to_owned(),
            parameters: vec![
                CommandParameterSpec {
                    name: "output_dir".to_owned(),
                    kind: CommandParameterKind::RemotePath {
                        root: "/tmp/captures".to_owned(),
                    },
                },
                CommandParameterSpec {
                    name: "format".to_owned(),
                    kind: CommandParameterKind::Choice(BTreeSet::from(["jpeg".to_owned()])),
                },
            ],
            argv: vec![
                RecipeArg::Parameter("output_dir".to_owned()),
                RecipeArg::Parameter("format".to_owned()),
            ],
            artifact_path_from_stdout: true,
        })
        .unwrap();
    Arc::new(recipes)
}

fn ssh_profile(id: &str, credential_ref: String) -> PlatformProfile {
    PlatformProfile {
        id: PlatformProfileId::new(id).unwrap(),
        display_name: format!("SSH {id}"),
        config: PlatformConfig::SshManaged(SshManagedConfig {
            host: "127.0.0.1".to_owned(),
            port: 22,
            username: "tester".to_owned(),
            expected_host_key: "ssh-ed25519 fixture".to_owned(),
            credential_ref,
            command_subsystem: None,
            capture_recipe: "capture".to_owned(),
            remote_artifact_dir: "/tmp/captures".to_owned(),
            remote_artifact_glob: "*.jpg".to_owned(),
            remote_event_subsystem: None,
            passive_watch_auto_open: false,
            stable_samples: 2,
            stability_interval_ms: 100,
            max_fetch_bytes: 1024,
            command_output_bytes: 1024,
        }),
    }
}

#[test]
fn production_credentials_reject_missing_session_actionably_and_accept_key_file() {
    let recipes = recipe_registry();
    let backend = ProductionBindingBackend::new(Arc::clone(&recipes));
    let session_error = match backend.bind(&ssh_profile("session", "session:missing".to_owned())) {
        Ok(_) => panic!("missing session credential unexpectedly bound"),
        Err(error) => error,
    };
    assert!(session_error.contains("no registered session secret"));
    assert!(session_error.contains("key-file:/absolute/path"));

    let path = std::env::temp_dir().join(format!("camera-toolbox-tui-key-{}", std::process::id()));
    fs::write(
        &path,
        "-----BEGIN OPENSSH PRIVATE KEY-----\nfixture\n-----END OPENSSH PRIVATE KEY-----\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let absolute = PathBuf::from(&path).canonicalize().unwrap();
    let bound = backend.bind(&ssh_profile(
        "key-file",
        format!("key-file:{}", absolute.display()),
    ));
    let _ = fs::remove_file(path);
    assert!(matches!(bound.unwrap(), PlatformBindings::SshManaged(_)));
}
