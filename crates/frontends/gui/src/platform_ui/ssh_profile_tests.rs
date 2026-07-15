use std::path::Path;

use super::*;

const KEY_A: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
const KEY_B: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";

struct StaticProbe(ServerHostKey);

impl camera_toolbox_adapters::platforms::ssh_managed::ServerHostKeyProbe for StaticProbe {
    fn probe(
        &self,
        _target: &HostKeyTarget,
        _timeout: std::time::Duration,
    ) -> Result<ServerHostKey, camera_toolbox_adapters::platforms::ssh_managed::HostKeyError> {
        Ok(self.0.clone())
    }
}

fn worker_with_probe(path: &Path, key: ServerHostKey) -> HostKeyWorker {
    use std::sync::Arc;

    let manager = camera_toolbox_adapters::platforms::ssh_managed::HostKeyManager::with_probe(
        path,
        Arc::new(StaticProbe(key)),
    )
    .unwrap();
    HostKeyWorker::with_manager(manager).unwrap()
}

fn wait_for_host_result(state: &mut SshEditorState, config: &mut SshManagedConfig) {
    for _ in 0..100 {
        state.poll(config, true);
        if !matches!(
            state.host_status,
            HostIdentityStatus::Scanning | HostIdentityStatus::Trusting
        ) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("host-key worker did not finish deterministic request");
}

fn append_shape_text(shape: &egui::epaint::Shape, output: &mut String) {
    match shape {
        egui::epaint::Shape::Text(text) => {
            output.push_str(&text.galley.job.text);
            output.push('\n');
        }
        egui::epaint::Shape::Vec(shapes) => {
            for shape in shapes {
                append_shape_text(shape, output);
            }
        }
        _ => {}
    }
}

#[test]
fn new_ssh_profile_renders_password_first_basic_flow() {
    let context = egui::Context::default();
    let mut state = SshEditorState::for_test();
    let mut config = super::super::device_manager::incomplete_ssh_template();
    let output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(720.0, 900.0),
            )),
            ..Default::default()
        },
        |ui| state.render(ui, &mut config),
    );
    let mut rendered = String::new();
    for clipped in &output.shapes {
        append_shape_text(&clipped.shape, &mut rendered);
    }

    assert!(rendered.contains("SSH-managed"));
    assert!(rendered.contains("Host / IP"));
    assert!(rendered.contains("Client authentication"));
    assert!(rendered.contains("Password"));
    assert!(rendered.contains("Process memory only / never saved"));
    assert!(rendered.contains("测试 SSH 登录与远程路径"));
    assert!(rendered.contains("服务器身份"));
    assert!(rendered.contains(
        "测试验证服务器身份、SSH 登录、规范化路径和 SFTP 元数据可见性；不读取文件内容。"
    ));
    assert!(rendered.contains("Remote file"));
    assert!(rendered.contains("Advanced"));
    assert!(!rendered.contains("验证服务器身份（不测试账号密码）"));
}

#[test]
fn session_reference_requires_password_when_process_registry_is_empty() {
    let mut config = super::super::device_manager::incomplete_ssh_template();
    config.host = "camera.example".to_owned();
    config.username = "root".to_owned();
    config.expected_host_key = KEY_A.to_owned();
    config.credential_ref = "session:profile-camera".to_owned();
    config.remote_artifact_dir = "/data".to_owned();
    config.remote_artifact_glob = "frame.raw".to_owned();
    let profile = PlatformProfile {
        id: PlatformProfileId::new("camera").unwrap(),
        display_name: "Camera".to_owned(),
        config: PlatformConfig::SshManaged(config.clone()),
    };

    let mut missing = SshEditorState::from_profile(&profile, false);
    assert!(missing.password_required);
    let error = match missing.prepare_commit(&mut config.clone(), false) {
        Ok(_) => panic!("missing process password unexpectedly accepted"),
        Err(error) => error,
    };
    assert!(error.contains("password is required"));

    let mut ready = SshEditorState::from_profile(&profile, true);
    assert!(!ready.password_required);
    assert!(matches!(
        ready.prepare_commit(&mut config, false).unwrap(),
        SshCommitAuth::Password { password: None }
    ));
}

#[test]
fn password_is_default_and_cleared_on_auth_switch_and_consume() {
    let mut state = SshEditorState::new();
    assert_eq!(state.auth_mode, SshAuthMode::Password);
    state
        .password
        .insert_text("secret", egui::text::CharIndex::default());
    let previous = state.auth_mode;
    state.auth_mode = SshAuthMode::PrivateKeyFile;
    state.apply_auth_change(previous);
    assert!(state.password_is_empty());
    let previous = state.auth_mode;
    state.auth_mode = SshAuthMode::Password;
    state.apply_auth_change(previous);
    assert!(state.password_required);
    assert!(state.password_is_empty());
    state
        .password
        .insert_text("again", egui::text::CharIndex::default());
    let _secret = state.password.take().unwrap();
    assert!(state.password_is_empty());
}

#[cfg(unix)]
#[test]
fn discovered_key_selection_validates_real_file_without_raw_reference() {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("camera-toolbox-gui-key-{suffix}"));
    fs::create_dir_all(&directory).unwrap();
    let key = directory.join("id_ed25519");
    fs::write(
        &key,
        "-----BEGIN OPENSSH PRIVATE KEY-----\ntest\n-----END OPENSSH PRIVATE KEY-----\n",
    )
    .unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let mut state = SshEditorState::for_test();
    state.select_private_key(key.clone());
    let canonical = fs::canonicalize(&key).unwrap();
    assert_eq!(
        state.selected_private_key.as_deref(),
        Some(canonical.as_path())
    );
    assert!(canonical.is_absolute());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn exact_remote_path_round_trips_and_wildcard_stays_advanced() {
    let (root, basename) = split_exact_remote_file("/data/capture/frame.raw").unwrap();
    assert_eq!(root, "/data/capture");
    assert_eq!(basename, "frame.raw");
    assert_eq!(
        combine_remote_file(&root, &basename),
        "/data/capture/frame.raw"
    );
    assert!(split_exact_remote_file("/data/*.raw").is_err());
    assert!(!is_literal_remote_glob("*.raw"));
    assert!(is_literal_remote_glob("frame.raw"));
}

#[test]
fn password_connection_request_keeps_visible_password_and_normalizes_basic_path() {
    let mut state = SshEditorState::for_test();
    let mut config = super::super::device_manager::incomplete_ssh_template();
    state.set_password_for_test("test-password");
    state.exact_remote_file = "/data/capture/frame.raw".to_owned();
    config.remote_artifact_dir = "/unused".to_owned();
    config.remote_artifact_glob = "old.raw".to_owned();

    let request = state.prepare_connection_test_request(&config, 17).unwrap();

    assert!(!state.password_is_empty());
    assert_eq!(request.generation, 17);
    assert_eq!(request.remote_path, "/data/capture/frame.raw");
    assert_eq!(request.config.remote_artifact_dir, "/data/capture");
    assert_eq!(request.config.remote_artifact_glob, "frame.raw");
    assert!(matches!(request.auth, SshConnectionTestAuth::Password(_)));
}

#[test]
fn private_key_connection_request_uses_the_selected_path_without_fallback() {
    let mut state = SshEditorState::for_test();
    let config = super::super::device_manager::incomplete_ssh_template();
    let selected = PathBuf::from("/tmp/selected-client-key");
    state.auth_mode = SshAuthMode::PrivateKeyFile;
    state.selected_private_key = Some(selected.clone());
    state.exact_remote_file = "/data/frame.raw".to_owned();

    let request = state.prepare_connection_test_request(&config, 18).unwrap();

    assert!(matches!(
        request.auth,
        SshConnectionTestAuth::PrivateKeyFile(path) if path == selected
    ));
}

#[test]
fn advanced_connection_request_requires_and_uses_exact_test_path() {
    let mut state = SshEditorState::for_test();
    let mut config = super::super::device_manager::incomplete_ssh_template();
    state.set_password_for_test("test-password");
    state.advanced_remote_path = true;
    config.remote_artifact_dir = "/data/captures".to_owned();
    config.remote_artifact_glob = "*.raw".to_owned();

    assert!(state.prepare_connection_test_request(&config, 19).is_err());

    state.exact_remote_file = "/data/captures/current.raw".to_owned();
    let request = state.prepare_connection_test_request(&config, 20).unwrap();
    assert_eq!(request.remote_path, "/data/captures/current.raw");
    assert_eq!(request.config.remote_artifact_dir, "/data/captures");
    assert_eq!(request.config.remote_artifact_glob, "*.raw");
}

#[test]
fn existing_profile_pin_tests_login_directly_without_host_key_scan() {
    use std::{collections::VecDeque, fs, sync::Arc, time::Duration};

    use camera_toolbox_adapters::platforms::ssh_managed::{
        HostKeyManager, MemoryRemoteFile, MemorySshTransport,
    };
    use camera_toolbox_app::RemoteFileStat;

    let directory = std::env::temp_dir().join(format!(
        "camera-toolbox-direct-login-test-{}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    let manager = HostKeyManager::with_probe(
        directory.join("known_hosts"),
        Arc::new(StaticProbe(ServerHostKey::from_openssh(KEY_B).unwrap())),
    )
    .unwrap();
    let transport = MemorySshTransport::new(KEY_A);
    transport.insert_file(
        "/data/frame.raw",
        MemoryRemoteFile {
            bytes: Vec::new(),
            stats: VecDeque::from([RemoteFileStat {
                path: "/data/frame.raw".to_owned(),
                size: 4096,
                modified_seconds: 123,
                producer_marker: None,
                sha256: None,
            }]),
        },
    );
    let worker = HostKeyWorker::with_manager_and_transport(manager, Arc::new(transport)).unwrap();
    let mut state = SshEditorState::base(Some(worker), status_from_existing_pin(KEY_A));
    let mut config = super::super::device_manager::incomplete_ssh_template();
    config.host = "camera.example".to_owned();
    config.username = "root".to_owned();
    config.expected_host_key = KEY_A.to_owned();
    state.set_password_for_test("process-only-password");
    state.exact_remote_file = "/data/frame.raw".to_owned();

    state.start_connection_test(&config);
    assert!(matches!(
        state.connection_status,
        ConnectionTestStatus::Testing
    ));
    for _ in 0..100 {
        state.poll(&mut config, true);
        if !matches!(state.connection_status, ConnectionTestStatus::Testing) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(matches!(
        state.connection_status,
        ConnectionTestStatus::Success {
            size: 4096,
            modified_seconds: 123,
            ..
        }
    ));
    assert!(matches!(
        state.host_status,
        HostIdentityStatus::Pinned { .. }
    ));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn known_unknown_changed_and_explicit_trust_transitions_use_injected_worker() {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("camera-toolbox-host-key-{suffix}"));
    fs::create_dir_all(&directory).unwrap();
    let known_hosts = directory.join("known_hosts");
    let key_a = ServerHostKey::from_openssh(KEY_A).unwrap();
    let key_b = ServerHostKey::from_openssh(KEY_B).unwrap();
    let mut config = super::super::device_manager::incomplete_ssh_template();
    config.host = "camera.example".to_owned();
    let mut state = SshEditorState::base(
        Some(worker_with_probe(&known_hosts, key_a.clone())),
        HostIdentityStatus::Unverified,
    );

    state.request_scan(&config);
    wait_for_host_result(&mut state, &mut config);
    let key_to_trust = match &mut state.host_status {
        HostIdentityStatus::Unknown {
            key,
            verified_out_of_band,
        } => {
            assert!(!unknown_trust_allowed(*verified_out_of_band));
            *verified_out_of_band = true;
            assert!(unknown_trust_allowed(*verified_out_of_band));
            key.clone()
        }
        _ => panic!("empty known_hosts must yield Unknown"),
    };
    assert!(config.expected_host_key.is_empty());

    state.request_trust(&config, key_to_trust);
    wait_for_host_result(&mut state, &mut config);
    assert!(matches!(
        state.host_status,
        HostIdentityStatus::Trusted { .. }
    ));
    assert_eq!(config.expected_host_key, key_a.openssh());

    state.host_worker = Some(worker_with_probe(&known_hosts, key_a.clone()));
    state.request_scan(&config);
    wait_for_host_result(&mut state, &mut config);
    assert!(matches!(
        state.host_status,
        HostIdentityStatus::Trusted { .. }
    ));

    state.host_worker = Some(worker_with_probe(&known_hosts, key_b));
    state.request_scan(&config);
    wait_for_host_result(&mut state, &mut config);
    assert!(matches!(
        state.host_status,
        HostIdentityStatus::Changed { .. }
    ));
    assert_eq!(config.expected_host_key, key_a.openssh());

    fs::write(&known_hosts, format!("camera.example {KEY_B}\n")).unwrap();
    state.host_worker = Some(worker_with_probe(&known_hosts, key_a));
    state.request_scan(&config);
    wait_for_host_result(&mut state, &mut config);
    assert!(matches!(
        state.host_status,
        HostIdentityStatus::Trusted { .. }
    ));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn stale_or_wrong_target_scan_cannot_change_pin() {
    let key = ServerHostKey::from_openssh(KEY_A).unwrap();
    let mut config = super::super::device_manager::incomplete_ssh_template();
    config.host = "current.example".to_owned();
    let mut state = SshEditorState::new();
    state.host_generation = 10;
    state.apply_worker_result(
        &mut config,
        HostKeyWorkerResult::Scan {
            generation: 9,
            target: HostKeyTarget::new("current.example", 22).unwrap(),
            result: Ok(HostKeyAssessment::Trusted(key.clone())),
        },
    );
    state.apply_worker_result(
        &mut config,
        HostKeyWorkerResult::Scan {
            generation: 10,
            target: HostKeyTarget::new("old.example", 22).unwrap(),
            result: Ok(HostKeyAssessment::Trusted(key)),
        },
    );
    assert!(config.expected_host_key.is_empty());
}
