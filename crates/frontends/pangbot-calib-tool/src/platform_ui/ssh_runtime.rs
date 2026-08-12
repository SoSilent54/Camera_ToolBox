//! SSH runtime controls：按 resolved handle/capability 独立门控操作。

use std::sync::Arc;

use camera_toolbox_app::{
    CapabilityVariant, OperationId, RemoteJobState, RemoteWatchEvent, ResolvedTargetBindings,
    SshManagedConfig, TargetResolutionSnapshot,
};
use camera_toolbox_core::{EphemeralAsset, MediaFormat};
use eframe::egui;

use super::live_runtime::CapturePanelState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SshRuntimeAction {
    Capture,
    Fetch,
    StartWatch,
    StopWatch(OperationId),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SshControlAvailability {
    fetch: bool,
    watch: bool,
    capture: bool,
}

pub(super) fn render_ssh_controls(
    ui: &mut egui::Ui,
    config: &SshManagedConfig,
    panel: &mut CapturePanelState,
    snapshot: Option<&TargetResolutionSnapshot>,
) -> Option<SshRuntimeAction> {
    let available = availability(snapshot);
    let capture_available = capture_button_enabled(config, available);
    let mut action = None;

    ui.heading("Remote Files");
    ui.label(format!(
        "{}@{}:{}",
        config.username, config.host, config.port
    ));
    ui.label(format!("Remote root: {}", config.remote_artifact_dir));
    ui.text_edit_singleline(&mut panel.remote_path);
    remote_format_selector(ui, &mut panel.remote_format);
    render_remote_geometry(ui, panel);
    if ui
        .add_enabled(
            available.fetch && !panel.remote_path.is_empty(),
            egui::Button::new("Fetch and Open"),
        )
        .clicked()
    {
        action = Some(SshRuntimeAction::Fetch);
    }

    ui.separator();
    ui.heading("Watch");
    ui.label(match config.remote_event_subsystem.as_deref() {
        Some(subsystem) => format!("Discovery subsystem: {subsystem}"),
        None => "Discovery: directory polling".to_owned(),
    });
    ui.label(if config.passive_watch_auto_open {
        "Stable watched assets open only in background."
    } else {
        "Stable watched assets are collected but never auto-opened."
    });
    if let Some(operation) = panel.watcher_operation.clone() {
        if ui.button("Stop Watch").clicked() {
            action = Some(SshRuntimeAction::StopWatch(operation));
        }
    } else if ui
        .add_enabled(available.watch, egui::Button::new("Start Watch"))
        .clicked()
    {
        action = Some(SshRuntimeAction::StartWatch);
    }

    ui.separator();
    ui.heading("Capture automation");
    if config.capture_recipe.is_empty() {
        ui.label("Capture disabled until recipe configured");
    } else {
        ui.label(format!("Recipe: {}", config.capture_recipe));
        ui.label(match config.command_subsystem.as_deref() {
            Some(subsystem) => format!("Command subsystem: {subsystem}"),
            None => "Command transport: standard SSH exec".to_owned(),
        });
        if !capture_available {
            ui.colored_label(
                egui::Color32::YELLOW,
                "Capture unavailable for the resolved target",
            );
        }
        if ui
            .add_enabled(capture_available, egui::Button::new("Remote Capture"))
            .clicked()
        {
            action = Some(SshRuntimeAction::Capture);
        }
    }
    action
}

fn capture_button_enabled(config: &SshManagedConfig, available: SshControlAvailability) -> bool {
    !config.capture_recipe.is_empty() && available.capture
}

fn availability(snapshot: Option<&TargetResolutionSnapshot>) -> SshControlAvailability {
    let Some(ResolvedTargetBindings::SshManaged(bindings)) =
        snapshot.map(|snapshot| snapshot.bindings.as_ref())
    else {
        return SshControlAvailability::default();
    };
    let fetch = bindings.remote_file.as_ref().is_some_and(|handle| {
        handle
            .descriptor
            .supported_variants
            .contains(&CapabilityVariant::RemoteFetch)
    });
    let watch = bindings.remote_file.as_ref().is_some_and(|handle| {
        handle
            .descriptor
            .supported_variants
            .contains(&CapabilityVariant::RemoteWatch)
    });
    let capture = bindings.command.is_some();
    SshControlAvailability {
        fetch,
        watch,
        capture,
    }
}

fn render_remote_geometry(ui: &mut egui::Ui, panel: &mut CapturePanelState) {
    if !matches!(
        panel.remote_format,
        MediaFormat::RawPacked { .. } | MediaFormat::Yuv420Sp { .. }
    ) {
        return;
    }
    egui::Grid::new("remote_media_geometry")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("Width");
            ui.add(egui::DragValue::new(&mut panel.remote_width).range(1..=u32::MAX));
            ui.end_row();
            ui.label("Height");
            ui.add(egui::DragValue::new(&mut panel.remote_height).range(1..=u32::MAX));
            ui.end_row();
            ui.label("Stride bytes");
            ui.add(egui::DragValue::new(&mut panel.remote_stride).range(1..=usize::MAX));
            ui.end_row();
        });
}

fn remote_format_selector(ui: &mut egui::Ui, format: &mut MediaFormat) {
    egui::ComboBox::from_id_salt("remote_media_format")
        .selected_text(remote_format_name(format))
        .show_ui(ui, |ui| {
            ui.selectable_value(format, MediaFormat::Jpeg, "JPEG");
            ui.selectable_value(format, MediaFormat::RawPacked { bit_depth: 10 }, "RAW10");
            ui.selectable_value(format, MediaFormat::RawPacked { bit_depth: 12 }, "RAW12");
            ui.selectable_value(
                format,
                MediaFormat::Yuv420Sp {
                    chroma_order: camera_toolbox_core::ChromaOrder::Vu,
                },
                "NV21",
            );
        });
}

pub(super) fn remote_format_name(format: &MediaFormat) -> &'static str {
    match format {
        MediaFormat::Jpeg => "jpeg",
        MediaFormat::RawPacked { bit_depth: 10 } => "raw10",
        MediaFormat::RawPacked { bit_depth: 12 } => "raw12",
        MediaFormat::Yuv420Sp {
            chroma_order: camera_toolbox_core::ChromaOrder::Vu,
        } => "nv21",
        _ => "binary",
    }
}

pub(super) fn decorate_remote_asset(
    asset: Arc<EphemeralAsset>,
    panel: &CapturePanelState,
) -> Arc<EphemeralAsset> {
    if matches!(asset.metadata.format, MediaFormat::Jpeg) {
        return asset;
    }
    let mut owned = (*asset).clone();
    owned
        .metadata
        .attributes
        .insert("width".to_owned(), panel.remote_width.to_string());
    owned
        .metadata
        .attributes
        .insert("height".to_owned(), panel.remote_height.to_string());
    match owned.metadata.format {
        MediaFormat::RawPacked { .. } => {
            owned
                .metadata
                .attributes
                .insert("stride".to_owned(), panel.remote_stride.to_string());
        }
        MediaFormat::Yuv420Sp { .. } => {
            owned
                .metadata
                .attributes
                .insert("y_stride".to_owned(), panel.remote_stride.to_string());
            owned
                .metadata
                .attributes
                .insert("chroma_stride".to_owned(), panel.remote_stride.to_string());
        }
        _ => {}
    }
    Arc::new(owned)
}

pub(super) fn remote_state_label(state: &RemoteJobState) -> String {
    match state {
        RemoteJobState::Queued => "Queued".to_owned(),
        RemoteJobState::Running(stage) => format!("Running: {stage:?}"),
        RemoteJobState::CommandCompleted(result) => format!("Command: {:?}", result.terminal),
        RemoteJobState::AssetReady { asset_id, .. } => format!("Asset ready: {asset_id}"),
        RemoteJobState::Watch(RemoteWatchEvent::AssetReady { result, open }) => {
            format!("Watched asset: {} ({open:?})", result.asset.id)
        }
        RemoteJobState::Watch(RemoteWatchEvent::CandidateFailed { path, error }) => {
            format!("Watch candidate {path}: {error}")
        }
        RemoteJobState::Watch(RemoteWatchEvent::Terminal(terminal)) => {
            format!("Watch stopped: {terminal:?}")
        }
        RemoteJobState::Failed(error) => format!("Failed: {error:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use camera_toolbox_adapters::platforms::ssh_managed::{
        CommandRecipeRegistry, CredentialResolver, MemorySshTransport, SshManagedPlatformProvider,
        SshTransportFactory,
    };
    use camera_toolbox_app::{
        PlatformBindings, PlatformConfig, PlatformProfile, PlatformProfileId, ProfileStore,
        SensorSelection,
    };

    use super::*;
    use crate::platform_ui::device_manager::incomplete_ssh_template;

    #[test]
    fn sparse_ssh_target_enables_fetch_and_watch_but_not_capture() {
        let key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
        let transport = Arc::new(MemorySshTransport::new(key));
        let resolver: Arc<dyn CredentialResolver> = transport.clone();
        let factory: Arc<dyn SshTransportFactory> = transport;
        let provider = SshManagedPlatformProvider::new(
            resolver,
            factory,
            Arc::new(CommandRecipeRegistry::new()),
        );
        let mut config = incomplete_ssh_template();
        config.host = "camera.example".to_owned();
        config.username = "root".to_owned();
        config.expected_host_key = key.to_owned();
        config.credential_ref = "session:test".to_owned();
        config.remote_artifact_dir = "/data".to_owned();
        config.remote_artifact_glob = "frame.raw".to_owned();
        assert!(!capture_button_enabled(
            &config,
            SshControlAvailability {
                fetch: true,
                watch: true,
                capture: true,
            },
        ));
        let profile = PlatformProfile {
            id: PlatformProfileId::new("camera").unwrap(),
            display_name: "camera".to_owned(),
            config: PlatformConfig::SshManaged(config),
        };
        let bindings = provider.bind(&profile).unwrap();
        let candidate = PlatformBindings::SshManaged(Arc::new(bindings));
        let mut store = ProfileStore::new();
        store.insert_platform(profile).unwrap();
        let snapshot = store
            .resolve_target(SensorSelection::Unbound, &candidate)
            .unwrap();

        assert_eq!(
            availability(Some(&snapshot)),
            SshControlAvailability {
                fetch: true,
                watch: true,
                capture: false,
            }
        );
    }
}
