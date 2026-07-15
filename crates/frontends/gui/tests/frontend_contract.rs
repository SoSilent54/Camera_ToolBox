use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use camera_toolbox_app::ProfileStore;

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
}

impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn local_unbound_platform_probe_uses_the_shipping_binary() {
    let fixture = FixtureDirectory::create();
    let store_path = fixture.path().join("profiles.json");
    ProfileStore::with_builtin_local()
        .expect("create builtin Local fixture")
        .save_to_path(&store_path)
        .expect("save fixture store");

    let output = Command::new(env!("CARGO_BIN_EXE_camera-toolbox"))
        .args([
            "platform",
            "probe",
            "--profile-store",
            store_path.to_str().expect("UTF-8 store path"),
            "--platform",
            "local",
        ])
        .output()
        .expect("run shipping CLI process");
    assert!(
        output.status.success(),
        "platform probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8(output.stdout).expect("CLI JSON is UTF-8");

    assert!(report.contains("\"platform_id\":\"local\""));
    assert!(report.contains("\"display_name\":\"Local files\""));
    assert!(report.contains("\"provider\":\"local\""));
    assert!(report.contains("\"selection\":{\"selection\":\"unbound\"}"));
    assert!(report.contains("\"status\":\"success\""));
    assert!(report.contains("\"capability\":\"local_open\""));
    assert!(report.contains("\"evidence\":\"protocol_verified\""));
    assert!(report.contains("\"driver\":\"local-file-v1\""));
    assert!(report.contains("\"supported_variants\":[\"local_raw\"]"));
}
