use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use camera_toolbox_app::ProfileStore;
use camera_toolbox_tui::{Args, snapshot_to_string};

struct FixtureDirectory {
    path: PathBuf,
}

impl FixtureDirectory {
    fn create() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "camera-toolbox-frontend-contract-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create fixture directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(self) {
        fs::remove_dir_all(&self.path).expect("remove fixture directory");
        std::mem::forget(self);
    }
}

impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// Typed terminal-event propagation stays covered by the real CLI integration test
// `typed_dump_terminal_failure_reaches_a_failing_process_exit_status` and the TUI state test
// `typed_terminal_failure_is_presented_in_status`; this cross-package test intentionally owns
// their shared resolver, snapshot, and capability semantics instead of adding test-only APIs.
#[test]
fn acceptance_12_real_cli_and_tui_share_local_unbound_resolution_contract() {
    let fixture = FixtureDirectory::create();
    let store_path = fixture.path().join("profiles.json");
    ProfileStore::with_builtin_local()
        .expect("create builtin Local fixture")
        .save_to_path(&store_path)
        .expect("save fixture store");

    let cli_output = Command::new(env!("CARGO_BIN_EXE_camera-toolbox-cli"))
        .args([
            "platform",
            "probe",
            "--profile-store",
            store_path.to_str().expect("UTF-8 store path"),
            "--platform",
            "local",
        ])
        .output()
        .expect("run real CLI platform probe");
    assert!(
        cli_output.status.success(),
        "platform probe failed: {}",
        String::from_utf8_lossy(&cli_output.stderr)
    );
    let cli = String::from_utf8(cli_output.stdout).expect("CLI JSON is UTF-8");

    let tui = snapshot_to_string(Args {
        profile_store: Some(store_path.clone()),
        snapshot: true,
        ..Args::default()
    })
    .expect("render real TUI production snapshot");

    assert!(cli.contains("\"platform_id\":\"local\""));
    assert!(cli.contains("\"display_name\":\"Local files\""));
    assert!(cli.contains("\"provider\":\"local\""));
    assert!(tui.contains("platform: local | Local files | local\n"));

    assert!(cli.contains("\"selection\":{\"selection\":\"unbound\"}"));
    assert!(tui.contains("sensor: Unbound\n"));

    assert!(cli.contains("\"status\":\"success\""));
    assert!(tui.contains("resolution: resolved\n"));

    assert!(cli.contains("\"capability\":\"local_open\""));
    assert!(cli.contains("\"evidence\":\"protocol_verified\""));
    assert!(cli.contains("\"driver\":\"local-file-v1\""));
    assert!(cli.contains("\"supported_variants\":[\"local_raw\"]"));
    assert!(tui.contains("  - LocalOpen | ProtocolVerified | local-file-v1 | [LocalRaw]\n"));

    fixture.cleanup();
}
