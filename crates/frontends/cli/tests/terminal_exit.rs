use std::{
    net::TcpListener,
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use camera_toolbox_app::{
    Cv610Config, Cv610DumpConfig, Cv610StreamConfig, PlatformConfig, PlatformProfile,
    PlatformProfileId, ProfileStore,
};

const RECIPE_ENV_KEYS: [&str; 8] = [
    "CAMERA_TOOLBOX_SSH_RECIPE_ID",
    "CAMERA_TOOLBOX_SSH_RECIPE_PROGRAM",
    "CAMERA_TOOLBOX_SSH_RECIPE_OUTPUT_ROOT",
    "CAMERA_TOOLBOX_SSH_RECIPE_FORMATS",
    "CAMERA_TOOLBOX_SSH_RECIPE_OUTPUT_DIR_FLAG",
    "CAMERA_TOOLBOX_SSH_RECIPE_FORMAT_FLAG",
    "CAMERA_TOOLBOX_SSH_RECIPE_ONCE_FLAG",
    "CAMERA_TOOLBOX_SSH_RECIPE_PATH_STDOUT",
];

fn unique_directory() -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("camera-toolbox-cli-exit-{suffix}"))
}

#[test]
fn typed_dump_terminal_failure_reaches_a_failing_process_exit_status() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture listener");
    let port = listener.local_addr().expect("fixture address").port();
    listener
        .set_nonblocking(true)
        .expect("set fixture nonblocking");
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((socket, _)) => {
                    drop(socket);
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            }
        }
        panic!("CLI never connected to the fixture Dump endpoint");
    });

    let directory = unique_directory();
    let store_path = directory.join("profiles.json");
    let mut store = ProfileStore::new();
    store
        .insert_platform(PlatformProfile {
            id: PlatformProfileId::new("cv610-exit-test").expect("platform id"),
            display_name: "CV610 exit test".to_owned(),
            config: PlatformConfig::HisiliconCv610(Cv610Config {
                host: "127.0.0.1".to_owned(),
                dump: Cv610DumpConfig {
                    port,
                    ..Cv610DumpConfig::default()
                },
                stream: Cv610StreamConfig::default(),
            }),
        })
        .expect("insert profile");
    store.save_to_path(&store_path).expect("save fixture store");

    let mut command = Command::new(env!("CARGO_BIN_EXE_camera-toolbox-cli"));
    command.args([
        "cv610",
        "dump",
        "--profile-store",
        store_path.to_str().expect("UTF-8 store path"),
        "--platform",
        "cv610-exit-test",
        "--kind",
        "raw12",
    ]);
    for key in RECIPE_ENV_KEYS {
        command.env_remove(key);
    }
    let output = command.output().expect("run CLI process");
    server.join().expect("join fixture server");

    assert!(
        !output.status.success(),
        "typed Dump failure unexpectedly returned success: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report = String::from_utf8(output.stdout).expect("JSON stdout is UTF-8");
    assert!(report.starts_with('{') && report.ends_with("}\n"));
    assert!(report.contains("\"command\":\"cv610_dump\""));
    assert!(report.contains("\"status\":\"failed\""));
    assert!(report.contains("\"error\":\"") && !report.contains("\"error\":\"\""));

    std::fs::remove_dir_all(directory).expect("remove fixture directory");
}
