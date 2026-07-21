use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use camera_toolbox_app::{
    CapabilityKind, CapabilityResolutionKey, CapabilityVariant, CaptureStore, CaptureStoreLimits,
    CommandServiceError, CommandTerminal, DefaultCapabilityResolver, DumpCancellation, OperationId,
    PlatformBindings, PlatformConfig, PlatformProfile, PlatformProfileId, RemoteDiscoveryRequest,
    RemoteDiscoverySource, RemoteFileRequest, RemoteOpenDisposition, RemoteOperationControl,
    RemoteServiceError, RemoteTimeouts, RemoteWatchEvent, RemoteWatchRequest,
    ResolvedTargetBindings, SensorSelection, SshManagedConfig, TypedCommandRequest,
};
use camera_toolbox_core::{AssetId, MediaFormat, OwnedMediaPayload};
use sha2::{Digest, Sha256};

use super::{
    CommandRecipe, CommandRecipeRegistry, CredentialResolver, MemoryRemoteFile, MemorySshTransport,
    RecipeArg, SshManagedPlatformProvider, SshTransportError, SshTransportFactory,
    TransportCommandOutput,
};

const ROOT: &str = "/data/captures";
const HOST_KEY: &str = "memory-host-key";
const CREDENTIAL_REF: &str = "test-keychain:ssh-a";

fn profile(auto_open: bool, event_subsystem: Option<&str>) -> PlatformProfile {
    PlatformProfile {
        id: PlatformProfileId::new("ssh-a").unwrap(),
        display_name: "SSH A".to_owned(),
        config: PlatformConfig::SshManaged(SshManagedConfig {
            host: "192.0.2.10".to_owned(),
            port: 22,
            username: "camera".to_owned(),
            expected_host_key: HOST_KEY.to_owned(),
            credential_ref: CREDENTIAL_REF.to_owned(),
            command_subsystem: None,
            capture_recipe: "capture".to_owned(),
            remote_artifact_dir: ROOT.to_owned(),
            remote_artifact_glob: "*.raw".to_owned(),
            remote_event_subsystem: event_subsystem.map(str::to_owned),
            passive_watch_auto_open: auto_open,
            stable_samples: 2,
            stability_interval_ms: 100,
            max_fetch_bytes: 1024,
            command_output_bytes: 128,
        }),
    }
}

fn recipes() -> Arc<CommandRecipeRegistry> {
    let mut recipes = CommandRecipeRegistry::new();
    recipes
        .register(CommandRecipe {
            id: "capture".to_owned(),
            program: "/usr/libexec/camera-toolbox-capture".to_owned(),
            parameters: Vec::new(),
            argv: vec![RecipeArg::Literal("--once".to_owned())],
            artifact_path_from_stdout: true,
        })
        .unwrap();
    Arc::new(recipes)
}

fn provider(memory: &MemorySshTransport) -> SshManagedPlatformProvider {
    memory.allow_credential(CREDENTIAL_REF);
    let resolver: Arc<dyn CredentialResolver> = Arc::new(memory.clone());
    let transport: Arc<dyn SshTransportFactory> = Arc::new(memory.clone());
    SshManagedPlatformProvider::new(resolver, transport, recipes())
}

fn control(cancellation: DumpCancellation) -> RemoteOperationControl {
    RemoteOperationControl::new(
        RemoteTimeouts {
            connect: Duration::from_millis(100),
            idle: Duration::from_millis(100),
            overall: Duration::from_secs(3),
        },
        cancellation,
    )
    .unwrap()
}

fn stat(path: &str, size: u64, mtime: u64, done: bool) -> camera_toolbox_app::RemoteFileStat {
    camera_toolbox_app::RemoteFileStat {
        path: path.to_owned(),
        size,
        modified_seconds: mtime,
        producer_marker: done.then(|| "producer-done-1".to_owned()),
        sha256: None,
    }
}

fn request(path: &str, expected_sha256: Option<String>) -> RemoteFileRequest {
    RemoteFileRequest {
        asset_id: AssetId::new(format!("asset-{}", path.rsplit('/').next().unwrap())).unwrap(),
        remote_path: path.to_owned(),
        expected_sha256,
        format: MediaFormat::Binary,
        source_name: path.rsplit('/').next().unwrap().to_owned(),
    }
}

fn store(per_operation: usize, global: usize) -> CaptureStore {
    CaptureStore::new(CaptureStoreLimits::new(per_operation, global).unwrap())
}

#[test]
fn strict_host_key_mismatch_fails_before_command_dispatch() {
    let memory = MemorySshTransport::new("different-host-key");
    let bindings = provider(&memory).bind(&profile(false, None)).unwrap();
    let error = bindings
        .command
        .unwrap()
        .service
        .execute(
            TypedCommandRequest::new("capture").unwrap(),
            control(DumpCancellation::default()),
        )
        .unwrap_err();
    assert_eq!(error, CommandServiceError::HostKeyMismatch);
    assert!(memory.captured_argv().is_empty());
}

#[test]
fn empty_recipe_binds_and_resolves_only_remote_file_then_fetches_in_memory() {
    let path = format!("{ROOT}/sparse.raw");
    let bytes = b"sparse-remote-file".to_vec();
    let memory = MemorySshTransport::new(HOST_KEY);
    memory.insert_file(
        &path,
        MemoryRemoteFile {
            bytes: bytes.clone(),
            stats: VecDeque::from([stat(&path, bytes.len() as u64, 1, true)]),
        },
    );
    let mut sparse_profile = profile(false, None);
    let PlatformConfig::SshManaged(config) = &mut sparse_profile.config else {
        panic!()
    };
    config.capture_recipe.clear();
    memory.allow_credential(CREDENTIAL_REF);
    let resolver: Arc<dyn CredentialResolver> = Arc::new(memory.clone());
    let transport: Arc<dyn SshTransportFactory> = Arc::new(memory.clone());
    let sparse_provider = SshManagedPlatformProvider::new(
        resolver,
        transport,
        Arc::new(CommandRecipeRegistry::new()),
    );
    let bindings = sparse_provider.bind(&sparse_profile).unwrap();
    assert!(bindings.command.is_none());
    assert!(bindings.remote_file.is_some());

    let resolved = DefaultCapabilityResolver
        .resolve(
            &CapabilityResolutionKey {
                platform_id: sparse_profile.id.clone(),
                sensor: SensorSelection::Unbound,
            },
            &PlatformBindings::SshManaged(Arc::new(bindings.clone())),
            None,
            None,
        )
        .unwrap();
    assert!(
        resolved
            .bindings
            .capability(CapabilityKind::Command)
            .is_none()
    );
    assert!(
        resolved
            .bindings
            .capability(CapabilityKind::RemoteFile)
            .is_some()
    );
    let ResolvedTargetBindings::SshManaged(resolved_bindings) = resolved.bindings.as_ref() else {
        panic!("expected SSH-managed bindings")
    };
    let result = resolved_bindings
        .remote_file
        .as_ref()
        .unwrap()
        .service
        .fetch(
            OperationId::new("sparse-fetch").unwrap(),
            request(&path, None),
            control(DumpCancellation::default()),
            &store(64, 64),
        )
        .unwrap();
    let OwnedMediaPayload::Bytes(published) = &result.asset.source else {
        panic!("expected byte payload")
    };
    assert_eq!(published.as_ref(), bytes.as_slice());
}

#[test]
fn nonempty_unknown_recipe_is_a_typed_bind_error() {
    let memory = MemorySshTransport::new(HOST_KEY);
    let mut unknown = profile(false, None);
    let PlatformConfig::SshManaged(config) = &mut unknown.config else {
        panic!()
    };
    config.capture_recipe = "missing".to_owned();
    assert!(matches!(
        provider(&memory).bind(&unknown),
        Err(super::SshManagedProviderError::CaptureRecipeNotRegistered(recipe))
            if recipe == "missing"
    ));
}

#[test]
fn profile_recipe_rejects_other_globally_registered_recipe() {
    let memory = MemorySshTransport::new(HOST_KEY);
    memory.allow_credential(CREDENTIAL_REF);
    let mut registry = CommandRecipeRegistry::new();
    for id in ["capture", "administrative-reset"] {
        registry
            .register(CommandRecipe {
                id: id.to_owned(),
                program: "/usr/libexec/camera-toolbox-capture".to_owned(),
                parameters: Vec::new(),
                argv: vec![RecipeArg::Literal(format!("--{id}"))],
                artifact_path_from_stdout: false,
            })
            .unwrap();
    }
    let resolver: Arc<dyn CredentialResolver> = Arc::new(memory.clone());
    let transport: Arc<dyn SshTransportFactory> = Arc::new(memory.clone());
    let bindings = SshManagedPlatformProvider::new(resolver, transport, Arc::new(registry))
        .bind(&profile(false, None))
        .unwrap();
    let error = bindings
        .command
        .unwrap()
        .service
        .execute(
            TypedCommandRequest::new("administrative-reset").unwrap(),
            control(DumpCancellation::default()),
        )
        .unwrap_err();
    assert_eq!(
        error,
        CommandServiceError::RecipeNotAllowed {
            recipe_id: "administrative-reset".to_owned(),
        }
    );
    assert!(memory.captured_argv().is_empty());
}

#[test]
fn argv_command_smoke_preserves_unknown_partial_evidence_without_ssh() {
    let memory = MemorySshTransport::new(HOST_KEY);
    memory.set_command_output(TransportCommandOutput {
        stdout: b"partial remote output".to_vec(),
        stderr: b"transport vanished".to_vec(),
        exit_status: None,
        stdout_truncated: false,
        stderr_truncated: false,
    });
    let bindings = provider(&memory).bind(&profile(false, None)).unwrap();
    let result = bindings
        .command
        .unwrap()
        .service
        .execute(
            TypedCommandRequest::new("capture").unwrap(),
            control(DumpCancellation::default()),
        )
        .unwrap();
    assert_eq!(result.terminal, CommandTerminal::RemoteStateUnknown);
    assert_eq!(result.stdout, b"partial remote output");
    assert_eq!(
        memory.captured_argv()[0][0],
        "/usr/libexec/camera-toolbox-capture"
    );
}

#[test]
fn candidate_descriptor_distinguishes_standard_exec_from_optional_argv_subsystem() {
    let memory = MemorySshTransport::new(HOST_KEY);
    let standard = provider(&memory).bind(&profile(false, None)).unwrap();
    let standard = standard.command.unwrap().descriptor;
    assert_eq!(standard.driver, "russh-standard-exec-posix-argv-v1");
    assert_eq!(
        standard.supported_variants,
        std::collections::BTreeSet::from([CapabilityVariant::StandardExecCommand])
    );

    let mut subsystem_profile = profile(false, None);
    let PlatformConfig::SshManaged(config) = &mut subsystem_profile.config else {
        panic!()
    };
    config.command_subsystem = Some("camera-toolbox-argv-v1".to_owned());
    let subsystem = provider(&memory).bind(&subsystem_profile).unwrap();
    let subsystem = subsystem.command.unwrap().descriptor;
    assert_eq!(subsystem.driver, "russh-ctargv1-subsystem-v1");
    assert_eq!(
        subsystem.supported_variants,
        std::collections::BTreeSet::from([CapabilityVariant::ArgvSubsystemCommand])
    );
}

#[test]
fn stable_detection_accepts_growth_then_rejects_truncation() {
    let growing_path = format!("{ROOT}/grow.raw");
    let memory = MemorySshTransport::new(HOST_KEY);
    memory.insert_file(
        &growing_path,
        MemoryRemoteFile {
            bytes: vec![7; 8],
            stats: VecDeque::from([
                stat(&growing_path, 4, 1, false),
                stat(&growing_path, 8, 2, false),
                stat(&growing_path, 8, 2, false),
                stat(&growing_path, 8, 2, false),
            ]),
        },
    );
    let service = provider(&memory)
        .bind(&profile(false, None))
        .unwrap()
        .remote_file
        .unwrap()
        .service;
    let result = service
        .fetch(
            OperationId::new("grow").unwrap(),
            request(&growing_path, None),
            control(DumpCancellation::default()),
            &store(64, 64),
        )
        .unwrap();
    assert_eq!(result.remote_stat.size, 8);

    let truncate_path = format!("{ROOT}/truncate.raw");
    memory.insert_file(
        &truncate_path,
        MemoryRemoteFile {
            bytes: vec![3; 4],
            stats: VecDeque::from([
                stat(&truncate_path, 8, 1, false),
                stat(&truncate_path, 4, 2, false),
            ]),
        },
    );
    let error = service
        .fetch(
            OperationId::new("truncate").unwrap(),
            request(&truncate_path, None),
            control(DumpCancellation::default()),
            &store(64, 64),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        RemoteServiceError::FileTruncatedBeforeStable {
            previous: 8,
            current: 4,
            ..
        }
    ));
}

#[test]
fn fetch_rejects_hash_mismatch_and_budget_before_any_read() {
    let path = format!("{ROOT}/hash.raw");
    let memory = MemorySshTransport::new(HOST_KEY);
    memory.insert_file(
        &path,
        MemoryRemoteFile {
            bytes: b"abcdefgh".to_vec(),
            stats: VecDeque::from([stat(&path, 8, 1, true)]),
        },
    );
    let service = provider(&memory)
        .bind(&profile(false, None))
        .unwrap()
        .remote_file
        .unwrap()
        .service;
    let error = service
        .fetch(
            OperationId::new("budget").unwrap(),
            request(&path, None),
            control(DumpCancellation::default()),
            &store(4, 4),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        RemoteServiceError::Store(
            camera_toolbox_app::CaptureStoreError::PerOperationBudgetExceeded { .. }
        )
    ));
    assert_eq!(memory.read_calls(), 0);

    let error = service
        .fetch(
            OperationId::new("hash").unwrap(),
            request(&path, Some("00".repeat(32))),
            control(DumpCancellation::default()),
            &store(64, 64),
        )
        .unwrap_err();
    assert!(matches!(error, RemoteServiceError::HashMismatch { .. }));

    let grow_path = format!("{ROOT}/grew-during-fetch.raw");
    memory.insert_file(
        &grow_path,
        MemoryRemoteFile {
            bytes: b"abcdefghi".to_vec(),
            stats: VecDeque::from([stat(&grow_path, 8, 1, true)]),
        },
    );
    let error = service
        .fetch(
            OperationId::new("grew-during-fetch").unwrap(),
            request(&grow_path, None),
            control(DumpCancellation::default()),
            &store(64, 64),
        )
        .unwrap_err();
    assert_eq!(
        error,
        RemoteServiceError::GrewDuringFetch {
            expected: 8,
            received: 9,
        }
    );
}

#[test]
fn cancellation_discards_reservation_and_never_publishes_partial_asset() {
    let path = format!("{ROOT}/cancel.raw");
    let memory = MemorySshTransport::new(HOST_KEY);
    memory.set_read_behavior(2, Duration::ZERO, Some(2));
    memory.insert_file(
        &path,
        MemoryRemoteFile {
            bytes: b"abcdefgh".to_vec(),
            stats: VecDeque::from([stat(&path, 8, 1, true)]),
        },
    );
    let service = provider(&memory)
        .bind(&profile(false, None))
        .unwrap()
        .remote_file
        .unwrap()
        .service;
    let store = store(64, 64);
    let error = service
        .fetch(
            OperationId::new("cancel").unwrap(),
            request(&path, None),
            control(DumpCancellation::default()),
            &store,
        )
        .unwrap_err();
    assert!(matches!(error, RemoteServiceError::Cancelled { .. }));
    assert_eq!(store.stats().unwrap().reservation_count, 0);
    assert_eq!(store.stats().unwrap().asset_count, 0);
}

#[test]
fn canonical_symlink_escape_is_rejected_before_read() {
    let path = format!("{ROOT}/link.raw");
    let memory = MemorySshTransport::new(HOST_KEY);
    memory.set_canonical_path(&path, "/etc/secret.raw");
    memory.insert_file(
        &path,
        MemoryRemoteFile {
            bytes: b"secret".to_vec(),
            stats: VecDeque::from([stat(&path, 6, 1, true)]),
        },
    );
    let service = provider(&memory)
        .bind(&profile(false, None))
        .unwrap()
        .remote_file
        .unwrap()
        .service;
    let error = service
        .fetch(
            OperationId::new("escape").unwrap(),
            request(&path, None),
            control(DumpCancellation::default()),
            &store(64, 64),
        )
        .unwrap_err();
    assert!(matches!(error, RemoteServiceError::PathNotAllowed { .. }));
    assert_eq!(memory.read_calls(), 0);
}

#[test]
fn passive_watch_emits_no_auto_open_and_has_no_filesystem_side_effects() {
    const { assert!(!SshManagedConfig::DEFAULT_PASSIVE_WATCH_AUTO_OPEN) };
    let before = directory_snapshot();
    let path = format!("{ROOT}/watch.raw");
    let memory = MemorySshTransport::new(HOST_KEY);
    memory.insert_file(
        &path,
        MemoryRemoteFile {
            bytes: b"watch-data".to_vec(),
            stats: VecDeque::from([
                stat(&path, 10, 1, true),
                stat(&path, 10, 1, true),
                stat(&path, 10, 1, true),
            ]),
        },
    );
    let service = provider(&memory)
        .bind(&profile(false, None))
        .unwrap()
        .remote_file
        .unwrap()
        .service;
    let cancellation = DumpCancellation::default();
    let cancel_after_asset = cancellation.clone();
    let events = Arc::new(Mutex::new(Vec::new()));
    let reported = Arc::clone(&events);
    let terminal = service
        .watch(
            OperationId::new("watch").unwrap(),
            RemoteWatchRequest {
                asset_id_prefix: "watch-asset".to_owned(),
                format: MediaFormat::Binary,
            },
            control(cancellation),
            &store(64, 64),
            Arc::new(move |event| {
                if matches!(event, RemoteWatchEvent::AssetReady { .. }) {
                    cancel_after_asset.cancel();
                }
                reported
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(event);
            }),
        )
        .unwrap();
    assert_eq!(terminal, camera_toolbox_app::RemoteWatchTerminal::Cancelled);
    let events = events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(events.iter().any(|event| matches!(
        event,
        RemoteWatchEvent::AssetReady {
            open: RemoteOpenDisposition::None,
            ..
        }
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        RemoteWatchEvent::AssetReady {
            open: RemoteOpenDisposition::Background,
            ..
        }
    )));
    drop(events);
    assert_eq!(directory_snapshot(), before);
}

#[test]
fn invalid_event_helper_falls_back_to_polling_but_command_path_stays_first() {
    let path = format!("{ROOT}/fallback.raw");
    let memory = MemorySshTransport::new(HOST_KEY);
    memory.set_event_error(Some(SshTransportError::HelperProtocol(
        "bad helper response".to_owned(),
    )));
    memory.insert_file(
        &path,
        MemoryRemoteFile {
            bytes: b"fallback".to_vec(),
            stats: VecDeque::from([stat(&path, 8, 1, true)]),
        },
    );
    let service = provider(&memory)
        .bind(&profile(false, Some("camera-toolbox-events-v1")))
        .unwrap()
        .remote_file
        .unwrap()
        .service;
    let results = service
        .discover_and_fetch(
            OperationId::new("event-fallback").unwrap(),
            RemoteDiscoveryRequest {
                command_result_path: None,
                asset_id_prefix: "fallback".to_owned(),
                format: MediaFormat::Binary,
            },
            control(DumpCancellation::default()),
            &store(64, 64),
        )
        .unwrap();
    assert_eq!(
        results[0].discovery_source,
        RemoteDiscoverySource::DirectoryPolling
    );

    let results = service
        .discover_and_fetch(
            OperationId::new("command-first").unwrap(),
            RemoteDiscoveryRequest {
                command_result_path: Some(path),
                asset_id_prefix: "command".to_owned(),
                format: MediaFormat::Binary,
            },
            control(DumpCancellation::default()),
            &store(64, 64),
        )
        .unwrap();
    assert_eq!(
        results[0].discovery_source,
        RemoteDiscoverySource::CommandResultPath
    );
}

#[test]
fn complete_fetch_publishes_exact_in_memory_bytes_and_hash() {
    let path = format!("{ROOT}/smoke.raw");
    let bytes = b"local-state-machine-smoke".to_vec();
    let expected = format!("{:x}", Sha256::digest(&bytes));
    let memory = MemorySshTransport::new(HOST_KEY);
    memory.insert_file(
        &path,
        MemoryRemoteFile {
            bytes: bytes.clone(),
            stats: VecDeque::from([stat(&path, bytes.len() as u64, 7, true)]),
        },
    );
    let service = provider(&memory)
        .bind(&profile(false, None))
        .unwrap()
        .remote_file
        .unwrap()
        .service;
    let result = service
        .fetch(
            OperationId::new("smoke").unwrap(),
            request(&path, Some(expected.clone())),
            control(DumpCancellation::default()),
            &store(128, 128),
        )
        .unwrap();
    assert_eq!(result.payload_sha256, expected);
    let OwnedMediaPayload::Bytes(published) = &result.asset.source else {
        panic!("expected byte payload")
    };
    assert_eq!(published.as_ref(), bytes.as_slice());
    assert_eq!(result.asset.metadata.attributes["remote_path"], path);
}

fn directory_snapshot() -> BTreeMap<String, u64> {
    std::fs::read_dir(".")
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            let len = entry.metadata().map_or(0, |metadata| metadata.len());
            (name, len)
        })
        .collect()
}
