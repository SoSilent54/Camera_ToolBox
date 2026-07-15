//! LiveRuntime focused tests kept separate from the production implementation.

use std::sync::Arc;

use camera_toolbox_app::{
    CapabilityKind, LocalConfig, OperationId, PlatformConfig, PlatformProfile, PlatformProfileId,
    SensorDescriptor, SensorId, SensorModeKey, SensorSelection,
};
use camera_toolbox_core::{
    AssetId, BayerPattern, CaptureMetadata, EphemeralAsset, IntegrityState, MediaFormat,
    OwnedMediaPayload, RawSpec,
    sensor::{SensorMode, SensorProfile},
};

use super::{AssetOpenSpec, JobContext, LiveRuntime, OpenIntent, PlatformEffect};
#[cfg(feature = "platform-ssh")]
use crate::platform_ui::device_manager::incomplete_ssh_template;

fn local_profile(id: &str) -> PlatformProfile {
    PlatformProfile {
        id: PlatformProfileId::new(id).unwrap(),
        display_name: id.to_owned(),
        config: PlatformConfig::Local(LocalConfig::default()),
    }
}

#[cfg(feature = "platform-ssh")]
fn ssh_profile(id: &str, glob: &str) -> PlatformProfile {
    let mut config = incomplete_ssh_template();
    config.host = "camera.example".to_owned();
    config.username = "root".to_owned();
    config.expected_host_key =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ"
            .to_owned();
    config.credential_ref = format!("session:{id}");
    config.remote_artifact_dir = "/data/capture".to_owned();
    config.remote_artifact_glob = glob.to_owned();
    PlatformProfile {
        id: PlatformProfileId::new(id).unwrap(),
        display_name: id.to_owned(),
        config: PlatformConfig::SshManaged(config),
    }
}

fn sensor() -> SensorDescriptor {
    SensorDescriptor {
        id: SensorId::new("sensor-a").unwrap(),
        display_name: "Sensor A".to_owned(),
        profile: SensorProfile {
            model: "sensor-a".to_owned(),
            modes: vec![SensorMode {
                id: "mode-a".to_owned(),
                raw: RawSpec {
                    width: 4,
                    height: 2,
                    bit_depth: 12,
                    bayer: BayerPattern::Rggb,
                },
                is_wdr: false,
            }],
            registers: Vec::new(),
        },
    }
}

#[test]
fn platform_and_sensor_selectors_are_independent_and_unbound_remains_usable() {
    let mut runtime = LiveRuntime::new().unwrap();
    runtime.profiles = camera_toolbox_app::ProfileStore::new();
    runtime
        .profiles
        .insert_platform(local_profile("local-a"))
        .unwrap();
    runtime
        .profiles
        .insert_platform(local_profile("local-b"))
        .unwrap();
    runtime.profiles.insert_sensor(sensor()).unwrap();

    runtime.select_platform(PlatformProfileId::new("local-a").unwrap());
    let unbound = runtime.snapshot.as_ref().unwrap();
    assert_eq!(runtime.sensor_selection, SensorSelection::Unbound);
    assert!(
        unbound
            .bindings
            .capability(CapabilityKind::LocalOpen)
            .is_some()
    );

    let selection = SensorSelection::Mode(SensorModeKey {
        sensor_id: SensorId::new("sensor-a").unwrap(),
        mode_id: "mode-a".to_owned(),
    });
    runtime.select_sensor(selection.clone());
    let first_hash = runtime.snapshot.as_ref().unwrap().aggregate_hash;
    runtime.select_platform(PlatformProfileId::new("local-b").unwrap());

    assert_eq!(runtime.sensor_selection, selection);
    assert_ne!(
        runtime.snapshot.as_ref().unwrap().aggregate_hash,
        first_hash
    );
    assert!(
        runtime
            .snapshot
            .as_ref()
            .unwrap()
            .bindings
            .capability(CapabilityKind::LocalOpen)
            .is_some()
    );
}

#[cfg(feature = "platform-ssh")]
#[test]
fn profile_selection_seeds_literal_remote_path_but_preserves_wildcard_user_path() {
    let mut runtime = LiveRuntime::new().unwrap();
    runtime.profiles = camera_toolbox_app::ProfileStore::new();
    runtime
        .profiles
        .insert_platform(ssh_profile("literal", "frame.raw"))
        .unwrap();
    runtime
        .profiles
        .insert_platform(ssh_profile("wildcard", "*.raw"))
        .unwrap();

    runtime.select_platform(PlatformProfileId::new("literal").unwrap());
    assert_eq!(runtime.capture_panel.remote_path, "/data/capture/frame.raw");
    runtime.capture_panel.remote_path = "/manual/keep.raw".to_owned();
    runtime.select_platform(PlatformProfileId::new("wildcard").unwrap());
    assert_eq!(runtime.capture_panel.remote_path, "/manual/keep.raw");
}

#[test]
fn background_asset_is_retained_without_opening_while_foreground_emits_open_effect() {
    let mut runtime = LiveRuntime::new().unwrap();
    runtime.profiles = camera_toolbox_app::ProfileStore::new();
    runtime
        .profiles
        .insert_platform(local_profile("local-a"))
        .unwrap();
    runtime.select_platform(PlatformProfileId::new("local-a").unwrap());
    let snapshot = runtime.snapshot.clone().unwrap();

    for (suffix, intent, expected_effects) in [
        ("background", OpenIntent::None, 0),
        ("foreground", OpenIntent::Foreground, 1),
    ] {
        let operation = OperationId::new(format!("operation-{suffix}")).unwrap();
        let asset_id = AssetId::new(format!("asset-{suffix}")).unwrap();
        let bytes = Arc::<[u8]>::from(&b"abc"[..]);
        let reservation = runtime
            .capture_store
            .reserve(operation.clone(), bytes.len())
            .unwrap();
        runtime
            .capture_store
            .publish_validated(
                reservation,
                EphemeralAsset::new(
                    asset_id.clone(),
                    OwnedMediaPayload::from_bytes(bytes),
                    CaptureMetadata {
                        format: MediaFormat::Binary,
                        source_name: suffix.to_owned(),
                        attributes: Default::default(),
                    },
                    IntegrityState::Verified {
                        algorithm: "test".to_owned(),
                        digest: suffix.to_owned(),
                    },
                ),
            )
            .unwrap();
        runtime.submissions.insert(
            operation.clone(),
            JobContext {
                snapshot: Arc::clone(&snapshot),
                intent,
                format: MediaFormat::Binary,
                spec: AssetOpenSpec {
                    bayer: BayerPattern::Rggb,
                },
            },
        );

        let mut effects = Vec::new();
        runtime.finish_asset_operation(&operation, &asset_id, false, &mut effects);
        assert_eq!(effects.len(), expected_effects);
        assert!(runtime.assets.contains_key(&asset_id));
        if expected_effects == 1 {
            assert!(matches!(
                effects[0],
                PlatformEffect::OpenAsset {
                    foreground: true,
                    ..
                }
            ));
        }
    }
}
