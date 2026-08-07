use camera_toolbox_adapters::{ImageRasterCodec, filesystem::LocalFileSystem};
use std::{path::PathBuf, sync::Arc, time::Duration};

use camera_toolbox_app::{
    DirectoryRef, ExportDestination, FileRef, FileSourceId, FsCancellation, ImageOpenMode,
    LocalRawAnalyzeReport, RasterImageCodec, SourcePath, SourceReadProgress,
};
use camera_toolbox_core::{
    AssetId, BayerPattern, CaptureMetadata, ChromaOrder, EphemeralAsset, IntegrityState,
    MediaFormat, NativeImage, OwnedMediaPayload, RawFrame, RawSpec, Rgba8Frame, Roi, RoiStats,
    Yuv420SpFrame, Yuv420SpSpec, YuvMatrix, YuvRange, analyze_raw_roi, analyze_roi,
};
use eframe::egui::{self, accesskit::Role};

#[cfg(all(target_os = "linux", feature = "platform-cv610"))]
use super::LIVE_STOP_TIMEOUT;
use super::{
    ActiveRawOpenJob, CameraToolboxApp, LoadedRaw, OpenedFileDocument, RawOpenJobEvent,
    WorkspaceFileOpenRequest, decode_strided_u16le_raw, decode_workspace_image_request,
    save_asset_source, save_asset_source_with, x5_233_channel_mapping, x5_233_live_source,
    x5_233_raw_bit_depth, x5_233_rtsp_timeouts, x5_233_rtsp_url, x5_233_yuv_snapshot_spec,
};
use crate::{
    analysis_panel::DesiredAnalysis,
    analysis_worker::{AnalysisData, AnalysisDomain, AnalysisKey, AnalysisPayload, AnalysisResult},
    color_worker::ColorRenderResult,
    histogram_link::{HistogramBinSelection, HistogramSeriesId, SpatialHighlightResult},
    image_save::{SaveFormat, SaveKey, SaveResult},
};

const TEST_VIEWPORT: egui::Vec2 = egui::vec2(640.0, 360.0);

#[allow(clippy::cast_possible_truncation)]
fn accesskit_rect_center(rect: egui::accesskit::Rect) -> egui::Pos2 {
    egui::pos2(
        ((rect.x0 + rect.x1) * 0.5) as f32,
        ((rect.y0 + rect.y1) * 0.5) as f32,
    )
}

fn run_app_frame(
    context: &egui::Context,
    app: &mut CameraToolboxApp,
    frame: &mut eframe::Frame,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let mut input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, TEST_VIEWPORT)),
        ..Default::default()
    };
    input.events = events;
    context.run_ui(input, |ui| eframe::App::ui(app, ui, frame))
}

fn run_app_frame_with_viewport(
    context: &egui::Context,
    app: &mut CameraToolboxApp,
    frame: &mut eframe::Frame,
    viewport: egui::Vec2,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let mut input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
        ..Default::default()
    };
    input.events = events;
    context.run_ui(input, |ui| eframe::App::ui(app, ui, frame))
}

fn settle_app_frame_with_viewport(
    context: &egui::Context,
    app: &mut CameraToolboxApp,
    frame: &mut eframe::Frame,
    viewport: egui::Vec2,
    time: f64,
) -> egui::FullOutput {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
        time: Some(time),
        ..Default::default()
    };
    context.run_ui(input, |ui| eframe::App::ui(app, ui, frame))
}

fn accessibility_text(output: &egui::FullOutput) -> String {
    output
        .platform_output
        .accesskit_update
        .as_ref()
        .expect("accessibility tree is enabled")
        .nodes
        .iter()
        .filter_map(|(_, node)| node.label().or_else(|| node.value()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn accessibility_exact_label_count(output: &egui::FullOutput, label: &str) -> usize {
    output
        .platform_output
        .accesskit_update
        .as_ref()
        .expect("accessibility tree is enabled")
        .nodes
        .iter()
        .filter(|(_, node)| node.label().or_else(|| node.value()) == Some(label))
        .count()
}

fn accesskit_bounds(output: &egui::FullOutput, label: &str) -> egui::accesskit::Rect {
    output
        .platform_output
        .accesskit_update
        .as_ref()
        .expect("accessibility tree is enabled")
        .nodes
        .iter()
        .find_map(|(_, node)| {
            (node.label() == Some(label) || node.value() == Some(label))
                .then(|| node.bounds())
                .flatten()
        })
        .unwrap_or_else(|| panic!("accessibility node {label:?} is visible"))
}

fn accesskit_bounds_all(output: &egui::FullOutput, label: &str) -> Vec<egui::accesskit::Rect> {
    output
        .platform_output
        .accesskit_update
        .as_ref()
        .expect("accessibility tree is enabled")
        .nodes
        .iter()
        .filter_map(|(_, node)| {
            (node.label() == Some(label) || node.value() == Some(label))
                .then(|| node.bounds())
                .flatten()
        })
        .collect()
}

fn test_export_destination() -> ExportDestination {
    let source_id = FileSourceId::new("gui-save-result-test").unwrap();
    let root = std::env::current_dir().unwrap();
    let file_system: Arc<dyn camera_toolbox_app::FileSystem> =
        Arc::new(LocalFileSystem::new(source_id.clone(), &root).unwrap());
    ExportDestination::new(DirectoryRef::root(source_id), file_system).unwrap()
}

fn test_live_source() -> crate::workspace::LiveStreamSource {
    crate::workspace::LiveStreamSource::Rtsp {
        label: "Test".to_owned(),
        channel: 0,
        transport: camera_toolbox_app::RtspTransport::Tcp,
        source_fingerprint: "test-rtsp-source".to_owned(),
        geometry_key: "test-rtsp-config".to_owned(),
        authoritative_capture: None,
    }
}

#[cfg(feature = "calibration-opencv")]
fn test_calibration_item_id() -> camera_toolbox_app::CalibrationItemId {
    let mut session = camera_toolbox_app::CalibrationSession::new(
        camera_toolbox_core::BoardSpec::new(2, 2, 1.0).unwrap(),
    );
    let outcome = session.add_or_refresh(
        FileRef::new(
            FileSourceId::new("live-viewer-presentation-test").unwrap(),
            SourcePath::new("dataset.png").unwrap(),
        ),
        camera_toolbox_app::FileVersion {
            size: 1,
            modified_millis: None,
        },
        "dataset.png".to_owned(),
    );
    let camera_toolbox_app::AddCalibrationItemOutcome::Added(id) = outcome else {
        panic!("expected added test item");
    };
    id
}

fn test_decoded_frame(
    session_id: &camera_toolbox_app::StreamSessionId,
    sequence: u64,
    value: u8,
) -> camera_toolbox_app::DecodedVideoFrame {
    camera_toolbox_app::DecodedVideoFrame {
        width: 1,
        height: 1,
        rgba: Arc::from(vec![value, value, value, 255]),
        identity: camera_toolbox_app::StreamFrameIdentity::unavailable(
            session_id.clone(),
            0,
            sequence,
            "test frame has no source PTS",
        ),
    }
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn live_viewer_texture_stays_live_when_dataset_overlay_exists() {
    let context = egui::Context::default();
    let session_id = camera_toolbox_app::StreamSessionId::new("viewer-texture-test").unwrap();
    let latest = Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default());
    latest.publish(test_decoded_frame(&session_id, 1, 17));
    let mut document = crate::workspace::LiveDocument::new(
        crate::workspace::DocumentId::from_raw(42),
        session_id.clone(),
        Arc::clone(&latest),
        test_live_source(),
    );
    document.install_latest_texture(&context);
    let live_texture_id = document.texture().unwrap().id();
    let presentation = crate::calibration_workspace::CalibrationViewerPresentation {
        item_id: Some(test_calibration_item_id()),
        overlay: crate::calibration_workspace::CalibrationViewerOverlay::default(),
    };

    assert_eq!(
        CameraToolboxApp::live_viewer_render_texture(&document, Some(&presentation))
            .unwrap()
            .id(),
        live_texture_id
    );

    latest.publish(test_decoded_frame(&session_id, 2, 34));
    document.install_latest_texture(&context);
    assert_eq!(
        document.displayed_frame().unwrap().identity.frame_sequence,
        2
    );
    assert_eq!(
        CameraToolboxApp::live_viewer_render_texture(&document, Some(&presentation))
            .unwrap()
            .id(),
        live_texture_id
    );
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn live_viewer_dataset_overlay_style_is_yellow() {
    assert_eq!(
        super::LIVE_VIEWER_DATASET_OVERLAY_COLOR,
        egui::Color32::from_rgb(255, 190, 64)
    );
}

#[test]
fn workspace_source_modes_render_rtsp_controls_exclusively() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.explorer_panel_expanded = true;
    let mut frame = eframe::Frame::_new_kittest();

    app.explorer.select_local_mode_for_test();
    let local = accessibility_text(&run_app_frame(&context, &mut app, &mut frame, Vec::new()));
    assert!(!local.contains("Connect RTSP"));
    assert!(!local.contains("RTSP Stream"));
    assert!(!local.contains("Prefer hardware acceleration"));

    #[cfg(feature = "platform-ssh")]
    {
        app.explorer.select_sftp_mode_for_test();
        let sftp = accessibility_text(&run_app_frame(&context, &mut app, &mut frame, Vec::new()));
        assert!(!sftp.contains("Connect RTSP"));
        assert!(!sftp.contains("RTSP Stream"));
        assert!(!sftp.contains("Prefer hardware acceleration"));
    }

    app.explorer.select_rtsp_mode_for_test();
    let rtsp = accessibility_text(&run_app_frame(&context, &mut app, &mut frame, Vec::new()));
    assert!(rtsp.contains("RTSP Stream"));
    assert!(rtsp.contains("Connect RTSP"));
    assert!(rtsp.contains("Prefer hardware acceleration"));
    assert!(!rtsp.contains("Name"));

    app.explorer.select_x5_233_driver_mode_for_test();
    let x5_driver = accessibility_text(&run_app_frame(&context, &mut app, &mut frame, Vec::new()));
    assert!(x5_driver.contains("X5_233 Driver"));
    assert!(x5_driver.contains("Device / Control"));
    assert!(x5_driver.contains("Host / IP"));
    assert!(x5_driver.contains("RTSP Preview"));
    assert!(x5_driver.contains("Read TCP Status"));
    assert!(x5_driver.contains("Stop selected RTSP"));
    assert!(x5_driver.contains("Connect selected RTSP"));
    assert!(x5_driver.contains("TCP Snapshot"));
    assert!(x5_driver.contains("Capture YUV"));
    assert!(x5_driver.contains("Capture RAW"));
    assert!(!x5_driver.contains("Connect RTSP"));
    assert!(!x5_driver.contains("Connect device"));
    assert!(!x5_driver.contains("SSH user"));
}

#[test]
fn color_workspace_renders_input_viewer_and_controls() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.explorer_panel_expanded = true;
    app.product_workspace = super::ProductWorkspace::Color;
    let mut frame = eframe::Frame::_new_kittest();

    let visible = accessibility_text(&run_app_frame_with_viewport(
        &context,
        &mut app,
        &mut frame,
        egui::vec2(1568.0, 882.0),
        Vec::new(),
    ));

    assert!(visible.contains("Workspace"));
    assert!(visible.contains("Color Check"));
    assert!(visible.contains("D65"));
    assert!(visible.contains("ColorChecker 24（Nov 2014+）"));
    assert!(visible.contains("Analyze current image"));
    assert!(visible.contains("Capture RTSP frame"));
    assert!(visible.contains("Export metrics JSON"));
    assert!(visible.contains("Export YAML Report"));
    assert!(!visible.contains("Intrinsic Calibration"));
}

#[test]
fn color_yaml_report_action_routes_to_workspace() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.product_workspace = super::ProductWorkspace::Color;
    let mut frame = eframe::Frame::_new_kittest();

    app.handle_color_inspection_action(&context, super::ColorInspectionAction::ExportYamlReport);
    let visible = accessibility_text(&run_app_frame_with_viewport(
        &context,
        &mut app,
        &mut frame,
        egui::vec2(1568.0, 882.0),
        Vec::new(),
    ));

    assert!(visible.contains("analyze an image before exporting color YAML report"));
}

#[test]
fn color_workspace_rejects_non_png_file_inputs_before_open_worker() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.product_workspace = super::ProductWorkspace::Color;
    let destination = test_export_destination();
    let file_system = Arc::clone(destination.file_system());
    let reference = FileRef::new(
        file_system.source_id().clone(),
        SourcePath::new("input.jpg").unwrap(),
    );

    app.handle_explorer_action(
        &context,
        crate::explorer::ExplorerAction::OpenAuto {
            display_path: PathBuf::from("input.jpg"),
            file_system,
            reference,
            remote: false,
        },
    );

    assert!(app.active_raw_open.is_none());
    assert!(app.pending_auto_open.is_empty());
    assert!(app.workspace.active_image().is_none());
}

#[test]
fn direct_rtsp_defaults_to_tcp_stable_baseline() {
    let context = egui::Context::default();
    let app = CameraToolboxApp::new(&context).unwrap();

    assert_eq!(
        app.direct_rtsp.transport,
        camera_toolbox_app::RtspTransport::Tcp
    );
    assert_eq!(
        app.direct_rtsp.latency_mode,
        camera_toolbox_app::RtspLatencyMode::Stable
    );
}

#[test]
fn x5_233_driver_defaults_match_driver_contract() {
    let context = egui::Context::default();
    let app = CameraToolboxApp::new(&context).unwrap();

    assert!(app.x5_233_driver.device_ip.is_empty());
    assert_eq!(app.x5_233_driver.ssh_user, "root");
    assert_eq!(app.x5_233_driver.ssh_password, "root");
    assert_eq!(app.x5_233_driver.tcp_port, 9073);
    assert!(app.x5_233_driver.configure_before_connect);
    assert_eq!(app.x5_233_driver.width, 1920);
    assert_eq!(app.x5_233_driver.height, 1080);
    assert_eq!(app.x5_233_driver.fps, 60);
    assert_eq!(app.x5_233_driver.bitrate_kbps, 12_000);
    assert_eq!(app.selected_x5_233_channels(), vec![0, 3]);
    assert_eq!(app.x5_233_driver.raw_camera, 0);
}

#[test]
fn x5_233_rtsp_uses_slower_board_connect_timeout() {
    let timeouts = x5_233_rtsp_timeouts();

    assert_eq!(timeouts.connect, Duration::from_secs(8));
    assert_eq!(timeouts.idle, Duration::from_secs(10));
}

#[test]
fn x5_233_selected_rtsp_requires_explicit_device_ip() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();

    app.start_x5_233_selected_rtsp();

    assert_eq!(
        app.x5_233_driver.last_error.as_deref(),
        Some("Enter X5_233 device IP before opening RTSP.")
    );
    assert!(app.workspace.live_documents().is_empty());
}

#[test]
fn x5_233_rtsp_url_maps_driver_channels_to_driver_ports() {
    let expected = [
        (0, "rtsp://10.21.12.108:554/PRR"),
        (1, "rtsp://10.21.12.108:555/PRR"),
        (3, "rtsp://10.21.12.108:557/PRR"),
        (4, "rtsp://10.21.12.108:558/PRR"),
    ];

    for (driver_channel, url) in expected {
        assert_eq!(
            x5_233_channel_mapping(driver_channel).map(|mapping| mapping.driver_channel),
            Some(driver_channel)
        );
        assert_eq!(
            x5_233_rtsp_url("10.21.12.108", driver_channel).as_deref(),
            Some(url)
        );
    }
    assert!(x5_233_rtsp_url("10.21.12.108", 2).is_none());
    assert!(x5_233_rtsp_url("", 0).is_none());
    assert!(x5_233_rtsp_url("   ", 0).is_none());
}

#[test]
fn x5_233_rtsp_and_tcp_capture_share_acquisition_identity() {
    let rtsp = x5_233_live_source(
        "10.21.12.108",
        0,
        9073,
        camera_toolbox_app::RtspTransport::Tcp,
        1920,
        1080,
    );
    let tcp_snapshot = x5_233_live_source(
        "10.21.12.108",
        0,
        9073,
        camera_toolbox_app::RtspTransport::Tcp,
        1920,
        1080,
    );
    let other_channel = x5_233_live_source(
        "10.21.12.108",
        3,
        9073,
        camera_toolbox_app::RtspTransport::Tcp,
        1920,
        1080,
    );

    assert_eq!(rtsp, tcp_snapshot);
    assert_ne!(rtsp, other_channel);
}

#[test]
fn x5_233_snapshot_metadata_builds_nv12_spec() {
    let spec = x5_233_yuv_snapshot_spec(4, 2, 8, 4).unwrap();

    assert_eq!(spec.width, 4);
    assert_eq!(spec.height, 2);
    assert_eq!(spec.y_stride, 4);
    assert_eq!(spec.chroma_stride, 4);
    assert_eq!(spec.chroma_order, ChromaOrder::Uv);
    assert_eq!(spec.matrix, YuvMatrix::Bt601);
    assert_eq!(spec.range, YuvRange::Limited);
    assert!(x5_233_yuv_snapshot_spec(4, 2, 7, 4).is_err());
}

#[test]
fn x5_233_raw_metadata_decodes_strided_u16le() {
    assert_eq!(x5_233_raw_bit_depth(24).unwrap(), 10);
    assert_eq!(x5_233_raw_bit_depth(0x2b).unwrap(), 10);
    assert_eq!(x5_233_raw_bit_depth(0x2c).unwrap(), 12);
    assert_eq!(x5_233_raw_bit_depth(0x2d).unwrap(), 14);
    assert!(x5_233_raw_bit_depth(0).is_err());

    let payload = [1_u16, 2, 0, 3, 4, 0]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let frame = decode_strided_u16le_raw(2, 2, 6, 10, BayerPattern::Rggb, &payload).unwrap();

    assert_eq!(frame.spec.width, 2);
    assert_eq!(frame.spec.height, 2);
    assert_eq!(frame.spec.bit_depth, 10);
    assert_eq!(frame.pixels(), &[1, 2, 3, 4]);
}

#[test]
fn x5_233_yuv_snapshot_publishes_image_document() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.x5_233_driver.device_ip = "10.21.12.108".to_owned();

    let status = app
        .publish_x5_233_yuv_snapshot(
            &context,
            crate::x5_tcp_client::X5YuvSnapshot {
                channel: 0,
                width: 2,
                height: 2,
                y_len: 4,
                uv_len: 2,
                frame_id: 7,
                timestamp_ns: 123,
                rtsp_timestamp_us: 0,
                rtsp_pts_90k: 0,
                match_rtsp_pts_delta_90k: None,
                match_mode: Some("latest".to_owned()),
                payload: vec![16, 32, 48, 64, 128, 128],
            },
        )
        .unwrap();

    let document = app.workspace.active_image().unwrap();
    assert!(status.contains("Published YUV driver CH0"));
    assert_eq!(document.title, "x5-233-ch0-frame7.nv12");
    assert!(matches!(document.native, NativeImage::Yuv420Sp(_)));
    let asset = document.source.asset().unwrap();
    assert_eq!(asset.metadata.attributes.get("width").unwrap(), "2");
    assert_eq!(asset.metadata.attributes.get("height").unwrap(), "2");
    assert_eq!(asset.metadata.attributes.get("y_stride").unwrap(), "2");
    assert_eq!(asset.metadata.attributes.get("chroma_stride").unwrap(), "2");
    assert_eq!(
        asset.metadata.format,
        MediaFormat::Yuv420Sp {
            chroma_order: ChromaOrder::Uv
        }
    );
}

#[test]
fn x5_233_raw_snapshot_publishes_raw_document() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.x5_233_driver.device_ip = "10.21.12.108".to_owned();
    let payload = [1_u16, 2, 3, 4]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();

    let status = app
        .publish_x5_233_raw_snapshot(
            &context,
            crate::x5_tcp_client::X5RawSnapshot {
                camera: 0,
                width: 2,
                height: 2,
                stride: 4,
                format_code: 24,
                frame_id: 9,
                timestamp_ns: 456,
                payload,
            },
        )
        .unwrap();

    let document = app.workspace.active().unwrap();
    assert!(status.contains("Published RAW cam0"));
    assert_eq!(document.title, "x5-233-camera0-frame9.raw");
    assert_eq!(document.loaded.frame.spec.width, 2);
    assert_eq!(document.loaded.frame.spec.height, 2);
    assert_eq!(document.loaded.frame.spec.bit_depth, 10);
    assert_eq!(document.loaded.frame.pixels(), &[1, 2, 3, 4]);
    let asset = document.source_asset.as_ref().unwrap();
    assert_eq!(
        asset.metadata.format,
        MediaFormat::RawU16Le { bit_depth: 10 }
    );
    assert_eq!(asset.metadata.attributes.get("stride").unwrap(), "4");
}

#[cfg(feature = "platform-ssh")]
#[test]
fn x5_233_remote_control_fallback_builds_root_connection() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.x5_233_driver.device_ip = "10.21.12.108".to_owned();

    let config = app.remote_control_connection("missing").unwrap();

    assert_eq!(config.host, "10.21.12.108");
    assert_eq!(config.port, 22);
    assert_eq!(config.username, "root");
    assert_eq!(config.display_name, "X5_233 root@10.21.12.108:22");
    assert!(matches!(
        config.authentication,
        camera_toolbox_app::RemoteAuthentication::Password { .. }
    ));
}

#[test]
fn rtsp_metrics_show_ffmpeg_io_without_rtp_counters() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.explorer_panel_expanded = true;
    app.explorer.select_rtsp_mode_for_test();
    let latest = Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default());
    app.workspace.open_live(
        camera_toolbox_app::StreamSessionId::new("rtsp-io-metrics-test").unwrap(),
        latest,
        test_live_source(),
    );
    app.workspace
        .active_live_mut()
        .expect("live document is active")
        .metrics = camera_toolbox_app::StreamMetrics {
        network_bytes: 123_456,
        network_bytes_available: true,
        network_bytes_per_second: 1_048_576,
        ffmpeg_media_bytes: 65_536,
        ffmpeg_media_bytes_per_second: 524_288,
        rtp_packets: 999,
        rtp_gaps: 888,
        preview_dropped: 7,
        decoder_resyncs: 2,
        ..Default::default()
    };

    let mut frame = eframe::Frame::_new_kittest();
    let output = run_app_frame_with_viewport(
        &context,
        &mut app,
        &mut frame,
        egui::vec2(1568.0, 882.0),
        Vec::new(),
    );
    let visible = accessibility_text(&output);

    assert!(visible.contains("FFmpeg I/O 123456 B"));
    assert!(visible.contains("FFmpeg I/O rate 1.00 MiB/s"));
    assert!(visible.contains("FFmpeg media 65536 B"));
    assert!(visible.contains("FFmpeg media rate 0.50 MiB/s"));
    assert!(visible.contains("media 65536 B · preview dropped 7 · resync 2"));
    assert!(!visible.contains("RTP gaps"));
    assert!(!visible.contains("RTP 999"));
    assert!(!visible.contains("Gaps 888"));
}

#[test]
fn cv610_metrics_keep_rtp_counters_visible() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.explorer_panel_expanded = true;
    app.explorer.select_rtsp_mode_for_test();
    let latest = Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default());
    app.workspace.open_live(
        camera_toolbox_app::StreamSessionId::new("cv610-rtp-metrics-test").unwrap(),
        latest,
        crate::workspace::LiveStreamSource::Cv610 {
            profile_id: camera_toolbox_app::PlatformProfileId::new("cv610-test").unwrap(),
            profile_label: "CV610".to_owned(),
            channel: 0,
            source_fingerprint: "cv610-source".to_owned(),
            geometry_key: "cv610-geometry".to_owned(),
        },
    );
    app.workspace
        .active_live_mut()
        .expect("live document is active")
        .metrics = camera_toolbox_app::StreamMetrics {
        network_bytes: 65_536,
        network_bytes_available: true,
        rtp_packets: 99,
        rtp_gaps: 3,
        preview_dropped: 2,
        decoder_resyncs: 1,
        ..Default::default()
    };

    let mut frame = eframe::Frame::_new_kittest();
    let output = run_app_frame_with_viewport(
        &context,
        &mut app,
        &mut frame,
        egui::vec2(1568.0, 882.0),
        Vec::new(),
    );
    let visible = accessibility_text(&output);

    assert!(visible.contains("Network 65536 B"));
    assert!(visible.contains("RTP 99"));
    assert!(visible.contains("Gaps 3"));
    assert!(visible.contains("RTP 99 · gaps 3 · preview dropped 2 · resync 1"));
}

fn loaded_raw(context: &egui::Context, name: &str, generation: u64) -> LoadedRaw {
    let spec = RawSpec {
        width: 2,
        height: 2,
        bit_depth: 10,
        bayer: BayerPattern::Rggb,
    };
    let frame = RawFrame::new(spec, vec![64, 128, 256, 512]).unwrap();
    let roi = Roi {
        x: 0,
        y: 0,
        width: frame.spec.width,
        height: frame.spec.height,
    };
    LoadedRaw::from_report(
        context,
        LocalRawAnalyzeReport {
            path: PathBuf::from(name),
            stats: analyze_roi(&frame, roi).unwrap(),
            frame,
            roi,
        },
        generation,
    )
}

fn app_with_loaded_raw(context: &egui::Context) -> CameraToolboxApp {
    let mut app = CameraToolboxApp::new(context).unwrap();
    app.workspace
        .open_local_raw(loaded_raw(context, "fixture.raw", 1));
    app
}

#[test]
fn png_workspace_open_reaches_viewer_and_source_rgb_analysis() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "camera-toolbox-gui-png-open-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("sample.png");
    let source_frame = Rgba8Frame::tight(
        2,
        2,
        Arc::<[u8]>::from(vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128,
        ]),
    )
    .unwrap();
    let mut encoded = Vec::new();
    ImageRasterCodec
        .encode_png(&source_frame, &mut encoded)
        .unwrap();
    std::fs::write(&path, encoded).unwrap();

    let source_id = FileSourceId::new("gui-png-open-test").unwrap();
    let file_system: Arc<dyn camera_toolbox_app::FileSystem> =
        Arc::new(LocalFileSystem::new(source_id.clone(), &root).unwrap());
    let reference = FileRef::new(source_id, SourcePath::new("sample.png").unwrap());
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let mut ignore_progress = |_| {};
    let opened = decode_workspace_image_request(
        &app.image_pipeline,
        WorkspaceFileOpenRequest {
            display_path: path.clone(),
            file_system,
            reference,
            remote: false,
        },
        ImageOpenMode::Auto,
        FsCancellation::default(),
        &mut ignore_progress,
    )
    .unwrap();
    let OpenedFileDocument::Image(opened) = opened else {
        panic!("PNG must route to a static image document");
    };
    app.install_opened_image(&context, 1, path, opened);

    let document = app.workspace.active_image().unwrap();
    assert_eq!(document.native.dimensions(), [2, 2]);
    assert_eq!(document.analysis_panel.domain, AnalysisDomain::SourceRgb);

    let mut frame = eframe::Frame::_new_kittest();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while app
        .workspace
        .active_image()
        .unwrap()
        .analysis_panel
        .current_key()
        .is_none()
        && std::time::Instant::now() < deadline
    {
        run_app_frame(&context, &mut app, &mut frame, Vec::new());
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let key = app
        .workspace
        .active_image()
        .unwrap()
        .analysis_panel
        .current_key()
        .expect("source RGB analysis must install");
    assert_eq!(key.domain, AnalysisDomain::SourceRgb);

    app.workspace
        .active_image_mut()
        .unwrap()
        .analysis_panel
        .domain = AnalysisDomain::DisplayRgb;
    let display_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while app
        .workspace
        .active_image()
        .unwrap()
        .analysis_panel
        .current_key()
        .is_none_or(|key| key.domain != AnalysisDomain::DisplayRgb)
        && std::time::Instant::now() < display_deadline
    {
        app.ensure_analysis();
        app.poll_analysis_result();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let display_key = app
        .workspace
        .active_image()
        .unwrap()
        .analysis_panel
        .current_key()
        .filter(|key| key.domain == AnalysisDomain::DisplayRgb)
        .expect("static image display RGB analysis must install");
    let display_selection = HistogramBinSelection {
        key: display_key,
        series: HistogramSeriesId::DisplayR,
        bin_index: 255,
        lower_code: 255,
        upper_code: 255,
    };
    app.update_spatial_highlight(Some(display_selection), false);
    assert_eq!(
        app.workspace.active_image().unwrap().spatial_requested,
        Some(display_selection)
    );

    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn raw_decode_panel_applies_automatically_without_button() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = app_with_loaded_raw(&context);
    let mut frame = eframe::Frame::_new_kittest();

    let output = run_app_frame(&context, &mut app, &mut frame, Vec::new());
    let text = output
        .platform_output
        .accesskit_update
        .expect("accessibility tree is enabled")
        .nodes
        .into_iter()
        .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("RAW Decode"));
    assert!(!text.contains("Apply Decode"));
}

#[test]
fn color_panel_bottom_gain_remains_reachable_in_short_viewport() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = app_with_loaded_raw(&context);
    let mut frame = eframe::Frame::_new_kittest();
    let panel_position = egui::pos2(500.0, 100.0);

    run_app_frame(&context, &mut app, &mut frame, Vec::new());
    run_app_frame(
        &context,
        &mut app,
        &mut frame,
        vec![egui::Event::PointerMoved(panel_position)],
    );
    run_app_frame(
        &context,
        &mut app,
        &mut frame,
        vec![egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -1_000.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        }],
    );
    let output = run_app_frame(&context, &mut app, &mut frame, Vec::new());
    let target = output
        .platform_output
        .accesskit_update
        .expect("accessibility tree is enabled")
        .nodes
        .into_iter()
        .filter_map(|(_, node)| {
            (node.role() == Role::SpinButton)
                .then(|| node.bounds())
                .flatten()
        })
        .filter(|bounds| bounds.x0 >= 360.0 && bounds.y0 >= 0.0 && bounds.y1 <= 200.0)
        .max_by(|left, right| left.y1.total_cmp(&right.y1))
        .expect("scrolled Channel gain control is visible");
    let start = accesskit_rect_center(target);
    let end = start + egui::vec2(20.0, 0.0);
    let before = app
        .workspace
        .active()
        .unwrap()
        .loaded
        .color_edit
        .params
        .gain
        .b;

    run_app_frame(
        &context,
        &mut app,
        &mut frame,
        vec![
            egui::Event::PointerMoved(start),
            egui::Event::PointerButton {
                pos: start,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
        ],
    );
    run_app_frame(
        &context,
        &mut app,
        &mut frame,
        vec![egui::Event::PointerMoved(end)],
    );
    run_app_frame(
        &context,
        &mut app,
        &mut frame,
        vec![egui::Event::PointerButton {
            pos: end,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        }],
    );

    let loaded = &app.workspace.active().unwrap().loaded;
    assert!((loaded.color_edit.params.gain.b - before).abs() > f32::EPSILON);
    assert!(loaded.color_edit.revision > 0);
}

#[test]
fn local_reports_open_as_independent_tabs() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let first = app
        .workspace
        .open_local_raw(loaded_raw(&context, "first.raw", 1));
    let second = app
        .workspace
        .open_local_raw(loaded_raw(&context, "second.raw", 2));

    assert_eq!(app.workspace.documents().len(), 2);
    assert_eq!(app.workspace.active_id(), Some(second));
    assert_eq!(app.workspace.document(first).unwrap().title, "first.raw");
    assert_eq!(app.workspace.document(second).unwrap().title, "second.raw");
}

#[test]
fn duplicate_generation_color_results_require_document_and_revision() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let first = app
        .workspace
        .open_local_raw(loaded_raw(&context, "first.raw", 7));
    let second = app
        .workspace
        .open_local_raw(loaded_raw(&context, "second.raw", 7));
    let first_params = app
        .workspace
        .document(first)
        .unwrap()
        .loaded
        .color_edit
        .params;

    app.install_color_result(
        &context,
        ColorRenderResult {
            document_id: first,
            frame_generation: 7,
            revision: 1,
            params: first_params,
            rendered: Err("first-only".to_owned()),
        },
    );
    app.install_color_result(
        &context,
        ColorRenderResult {
            document_id: second,
            frame_generation: 7,
            revision: 0,
            params: first_params,
            rendered: Err("stale-second".to_owned()),
        },
    );

    assert_eq!(
        app.workspace
            .document(first)
            .unwrap()
            .loaded
            .color_edit
            .render_error
            .as_deref(),
        Some("first-only")
    );
    assert!(
        app.workspace
            .document(second)
            .unwrap()
            .loaded
            .color_edit
            .render_error
            .is_none()
    );
}

fn analysis_key(
    document_id: crate::workspace::DocumentId,
    generation: u64,
    revision: Option<u64>,
    domain: AnalysisDomain,
    roi: Roi,
) -> AnalysisKey {
    AnalysisKey {
        document_id,
        generation,
        source_revision: revision,
        roi,
        domain,
    }
}

fn analysis_result(
    key: AnalysisKey,
    frame: &RawFrame,
    mean: f64,
    include_chart: bool,
) -> AnalysisResult {
    let stats = RoiStats {
        min: 1,
        max: 2,
        mean,
        saturated_pixels: 0,
        total_pixels: u64::from(key.roi.width) * u64::from(key.roi.height),
    };
    AnalysisResult {
        key,
        result: Ok(AnalysisPayload {
            chart: include_chart
                .then(|| AnalysisData::Raw(analyze_raw_roi(frame, key.roi).unwrap())),
            active_stats: stats,
            active_roi: key.roi,
        }),
    }
}

#[test]
fn duplicate_generation_analysis_results_require_document_and_source_revision() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let first = app
        .workspace
        .open_local_raw(loaded_raw(&context, "first.raw", 9));
    let second = app
        .workspace
        .open_local_raw(loaded_raw(&context, "second.raw", 9));
    let roi = app.workspace.document(first).unwrap().loaded.roi;
    let first_key = analysis_key(first, 9, None, AnalysisDomain::RawBayer, roi);
    let second_desired = analysis_key(second, 9, Some(4), AnalysisDomain::DisplayRgb, roi);
    let second_stale = analysis_key(second, 9, Some(3), AnalysisDomain::DisplayRgb, roi);

    {
        let first_document = app.workspace.document_mut(first).unwrap();
        first_document.loaded.stats = None;
        assert_eq!(
            first_document.analysis_panel.set_desired(first_key),
            DesiredAnalysis::Submit
        );
    }
    {
        let second_document = app.workspace.document_mut(second).unwrap();
        second_document.loaded.stats = None;
        assert_eq!(
            second_document.analysis_panel.set_desired(second_desired),
            DesiredAnalysis::Submit
        );
    }
    let first_frame = Arc::clone(&app.workspace.document(first).unwrap().loaded.frame);
    let second_frame = Arc::clone(&app.workspace.document(second).unwrap().loaded.frame);
    app.install_analysis_result(analysis_result(first_key, &first_frame, 11.0, true));
    app.install_analysis_result(analysis_result(second_stale, &second_frame, 22.0, false));

    assert_eq!(
        app.workspace
            .document(first)
            .unwrap()
            .loaded
            .stats
            .unwrap()
            .mean,
        11.0
    );
    assert!(
        app.workspace
            .document(second)
            .unwrap()
            .loaded
            .stats
            .is_none()
    );
}

#[test]
fn duplicate_generation_spatial_results_clear_only_exact_selection() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let first = app
        .workspace
        .open_local_raw(loaded_raw(&context, "first.raw", 13));
    let second = app
        .workspace
        .open_local_raw(loaded_raw(&context, "second.raw", 13));
    let roi = app.workspace.document(first).unwrap().loaded.roi;
    let first_key = analysis_key(first, 13, None, AnalysisDomain::RawBayer, roi);
    let second_key = analysis_key(second, 13, None, AnalysisDomain::RawBayer, roi);
    for (id, key) in [(first, first_key), (second, second_key)] {
        let frame = Arc::clone(&app.workspace.document(id).unwrap().loaded.frame);
        let document = app.workspace.document_mut(id).unwrap();
        assert_eq!(
            document.analysis_panel.set_desired(key),
            DesiredAnalysis::Submit
        );
        assert!(
            document
                .analysis_panel
                .accept_result(analysis_result(key, &frame, 1.0, true))
                .is_some()
        );
    }
    let first_selection = HistogramBinSelection {
        key: first_key,
        series: HistogramSeriesId::RawAll,
        bin_index: 1,
        lower_code: 1,
        upper_code: 1,
    };
    let second_selection = HistogramBinSelection {
        key: second_key,
        ..first_selection
    };
    app.workspace.document_mut(first).unwrap().spatial_requested = Some(first_selection);
    app.workspace
        .document_mut(second)
        .unwrap()
        .spatial_requested = Some(second_selection);

    app.install_spatial_highlight_result(SpatialHighlightResult {
        selection: first_selection,
        result: Err("first-only".to_owned()),
    });
    let stale_second = HistogramBinSelection {
        bin_index: 2,
        lower_code: 2,
        upper_code: 2,
        ..second_selection
    };
    app.install_spatial_highlight_result(SpatialHighlightResult {
        selection: stale_second,
        result: Err("stale-second".to_owned()),
    });

    assert!(
        app.workspace
            .document(first)
            .unwrap()
            .spatial_requested
            .is_none()
    );
    assert_eq!(
        app.workspace.document(second).unwrap().spatial_requested,
        Some(second_selection)
    );
}

#[test]
fn color_submission_resumes_after_a_b_a_switch() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let first = app
        .workspace
        .open_local_raw(loaded_raw(&context, "first.raw", 21));
    let second = app
        .workspace
        .open_local_raw(loaded_raw(&context, "second.raw", 22));

    assert!(app.workspace.activate(first));
    app.request_current_color();
    assert_eq!(
        app.workspace
            .document(first)
            .unwrap()
            .loaded
            .color_edit
            .submitted_revision,
        Some(1)
    );
    assert!(app.workspace.activate(second));
    app.request_current_color();
    assert_eq!(
        app.workspace
            .document(first)
            .unwrap()
            .loaded
            .color_edit
            .submitted_revision,
        None
    );
    assert!(app.workspace.activate(first));
    app.request_current_color();
    assert_eq!(
        app.workspace
            .document(first)
            .unwrap()
            .loaded
            .color_edit
            .submitted_revision,
        Some(1)
    );
}

#[test]
fn analysis_submission_resumes_after_a_b_a_switch() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let first = app
        .workspace
        .open_local_raw(loaded_raw(&context, "first.raw", 31));
    let second = app
        .workspace
        .open_local_raw(loaded_raw(&context, "second.raw", 32));
    let first_roi = app.workspace.document(first).unwrap().loaded.roi;
    let first_key = analysis_key(first, 31, None, AnalysisDomain::RawBayer, first_roi);

    assert!(app.workspace.activate(first));
    app.ensure_analysis();
    assert_eq!(
        app.workspace
            .document(first)
            .unwrap()
            .analysis_panel
            .pending_key(),
        Some(first_key)
    );
    assert!(app.workspace.activate(second));
    app.ensure_analysis();
    assert_eq!(
        app.workspace
            .document(first)
            .unwrap()
            .analysis_panel
            .pending_key(),
        None
    );
    assert!(app.workspace.activate(first));
    app.ensure_analysis();
    assert_eq!(
        app.workspace
            .document(first)
            .unwrap()
            .analysis_panel
            .pending_key(),
        Some(first_key)
    );
}

#[test]
fn spatial_submission_resumes_after_a_b_a_switch() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let first = app
        .workspace
        .open_local_raw(loaded_raw(&context, "first.raw", 41));
    let second = app
        .workspace
        .open_local_raw(loaded_raw(&context, "second.raw", 42));
    let roi = app.workspace.document(first).unwrap().loaded.roi;
    let first_key = analysis_key(first, 41, None, AnalysisDomain::RawBayer, roi);
    let second_key = analysis_key(second, 42, None, AnalysisDomain::RawBayer, roi);
    for (id, key) in [(first, first_key), (second, second_key)] {
        let frame = Arc::clone(&app.workspace.document(id).unwrap().loaded.frame);
        let document = app.workspace.document_mut(id).unwrap();
        document.analysis_panel.set_desired(key);
        assert!(
            document
                .analysis_panel
                .accept_result(analysis_result(key, &frame, 1.0, true))
                .is_some()
        );
    }
    let first_selection = HistogramBinSelection {
        key: first_key,
        series: HistogramSeriesId::RawAll,
        bin_index: 1,
        lower_code: 1,
        upper_code: 1,
    };
    let second_selection = HistogramBinSelection {
        key: second_key,
        ..first_selection
    };

    assert!(app.workspace.activate(first));
    app.update_spatial_highlight(Some(first_selection), false);
    assert_eq!(
        app.workspace.document(first).unwrap().spatial_requested,
        Some(first_selection)
    );
    assert!(app.workspace.activate(second));
    app.update_spatial_highlight(Some(second_selection), false);
    assert!(
        app.workspace
            .document(first)
            .unwrap()
            .spatial_requested
            .is_none()
    );
    assert!(app.workspace.activate(first));
    app.update_spatial_highlight(Some(first_selection), false);
    assert_eq!(
        app.workspace.document(first).unwrap().spatial_requested,
        Some(first_selection)
    );
}

#[test]
fn captured_source_export_writes_only_final_target_without_overwrite_or_staging() {
    use std::io::Write;

    let root = std::env::temp_dir().join(format!(
        "camera-toolbox-export-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let export_directory = root.join("exports");
    let working_directory = root.join("working");
    let config_directory = root.join("config");
    for directory in [&export_directory, &working_directory, &config_directory] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let asset = EphemeralAsset::new(
        AssetId::new("export-test").unwrap(),
        OwnedMediaPayload::from_bytes(Arc::<[u8]>::from(&b"new-source"[..])),
        CaptureMetadata {
            format: MediaFormat::Binary,
            source_name: "capture".to_owned(),
            attributes: Default::default(),
        },
        IntegrityState::Verified {
            algorithm: "test".to_owned(),
            digest: "test".to_owned(),
        },
    );

    let chosen_target = export_directory.join("capture.bin");
    save_asset_source(&chosen_target, &asset).unwrap();
    assert_eq!(std::fs::read(&chosen_target).unwrap(), b"new-source");
    let entries: Vec<_> = std::fs::read_dir(&export_directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, vec![chosen_target.file_name().unwrap()]);

    let existing_target = export_directory.join("existing.bin");
    std::fs::write(&existing_target, b"original-bytes").unwrap();
    let error = save_asset_source(&existing_target, &asset).unwrap_err();
    assert!(error.contains("already exists"));
    assert_eq!(std::fs::read(&existing_target).unwrap(), b"original-bytes");

    let failed_target = export_directory.join("failed.bin");
    let error = save_asset_source_with(&failed_target, &asset, |file, _asset| {
        file.write_all(b"partial")?;
        Err(std::io::Error::other("injected mid-write failure"))
    })
    .unwrap_err();
    assert!(error.contains("injected mid-write failure"));
    assert!(!failed_target.exists());

    for directory in [&export_directory, &working_directory, &config_directory] {
        assert!(std::fs::read_dir(directory).unwrap().all(|entry| {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.')
                && !name.contains(".part")
                && !name.contains("camera-toolbox-export")
        }));
    }
    assert_eq!(std::fs::read_dir(&working_directory).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&config_directory).unwrap().count(), 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn image_save_does_not_clear_captured_raw_source_prompt() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let snapshot = app.live_runtime.snapshot_for_test().unwrap();
    let asset = Arc::new(EphemeralAsset::new(
        AssetId::new("captured-raw-save-state").unwrap(),
        OwnedMediaPayload::from_bytes(Arc::<[u8]>::from(&[0, 1, 2, 3][..])),
        CaptureMetadata {
            format: MediaFormat::RawPacked { bit_depth: 10 },
            source_name: "captured-raw-save-state".to_owned(),
            attributes: std::collections::BTreeMap::new(),
        },
        IntegrityState::Verified {
            algorithm: "test".to_owned(),
            digest: "test".to_owned(),
        },
    ));
    let id = app.workspace.open_captured_raw(
        loaded_raw(&context, "captured.raw", 7),
        asset,
        snapshot,
        true,
    );
    assert!(app.workspace.document(id).unwrap().unsaved);

    let destination = test_export_destination();
    app.install_save_result(SaveResult {
        key: SaveKey {
            document_id: id,
            generation: 7,
            revision: 1,
        },
        destination: destination.clone(),
        target_label: "display.png".to_owned(),
        format: SaveFormat::Png,
        result: Ok(4),
    });
    assert!(app.workspace.document(id).unwrap().unsaved);

    app.install_save_result(SaveResult {
        key: SaveKey {
            document_id: id,
            generation: 7,
            revision: 1,
        },
        destination,
        target_label: "capture.raw".to_owned(),
        format: SaveFormat::RawU16Le,
        result: Ok(4),
    });
    assert!(app.workspace.document(id).unwrap().unsaved);
}

#[test]
fn unsaved_ephemeral_tab_is_retained_until_explicit_close_resolution() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let snapshot = app.live_runtime.snapshot_for_test().unwrap();
    let asset = Arc::new(EphemeralAsset::new(
        AssetId::new("pending-close").unwrap(),
        OwnedMediaPayload::from_bytes(Arc::<[u8]>::from(&[16, 16, 16, 16, 128, 128][..])),
        CaptureMetadata {
            format: MediaFormat::Yuv420Sp {
                chroma_order: ChromaOrder::Vu,
            },
            source_name: "pending-close".to_owned(),
            attributes: std::collections::BTreeMap::from([
                ("width".to_owned(), "2".to_owned()),
                ("height".to_owned(), "2".to_owned()),
                ("y_stride".to_owned(), "2".to_owned()),
                ("chroma_stride".to_owned(), "2".to_owned()),
            ]),
        },
        IntegrityState::Verified {
            algorithm: "test".to_owned(),
            digest: "test".to_owned(),
        },
    ));
    let spec = Yuv420SpSpec {
        width: 2,
        height: 2,
        y_stride: 2,
        chroma_stride: 2,
        chroma_order: ChromaOrder::Vu,
        matrix: YuvMatrix::Bt601,
        range: YuvRange::Limited,
    };
    let frame = Arc::new(
        Yuv420SpFrame::from_contiguous(spec, Arc::new(vec![16, 16, 16, 16, 128, 128])).unwrap(),
    );
    let display =
        Arc::new(Rgba8Frame::tight(2, 2, Arc::<[u8]>::from(vec![0, 0, 0, 255].repeat(4))).unwrap());
    let id = app
        .workspace
        .open_captured_image(
            9,
            asset,
            snapshot,
            NativeImage::Yuv420Sp(frame),
            display,
            true,
        )
        .unwrap();

    app.close_document(&context, id);
    assert_eq!(app.pending_ephemeral_close, Some(id));
    assert!(app.workspace.image(id).is_some());

    app.pending_ephemeral_close = None;
    app.workspace.image_mut(id).unwrap().unsaved = false;
    app.close_document(&context, id);
    assert!(app.workspace.image(id).is_none());
}

#[test]
fn closing_inactive_live_tab_requests_stop_and_removes_on_failure() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let first = app.workspace.open_live(
        camera_toolbox_app::StreamSessionId::new("live-close-a").unwrap(),
        Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default()),
        test_live_source(),
    );
    let second = app.workspace.open_live(
        camera_toolbox_app::StreamSessionId::new("live-close-b").unwrap(),
        Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default()),
        test_live_source(),
    );
    assert!(app.workspace.activate(second));

    app.close_document(&context, first);

    // request_close fails without a real RTSP connection → doc is removed
    assert!(app.workspace.live_documents().len() == 1);
    assert_eq!(app.workspace.live_documents()[0].id, second);
}

#[cfg(all(target_os = "linux", feature = "platform-cv610"))]
#[test]
fn ignored_eof_sidecar_stays_closing_until_gui_deadline_then_is_forced() {
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        time::{Duration, Instant},
    };

    use camera_toolbox_adapters::platforms::hisilicon_cv610::{
        Cv610StreamEndpoint, Cv610StreamService, HisiliconCv610Provider, MediaRequest,
    };
    use camera_toolbox_app::{
        CapabilityResolutionKey, Cv610Bindings, Cv610Config, Cv610DumpConfig, Cv610StreamConfig,
        DefaultCapabilityResolver, PlatformBindings, PlatformCapabilityHandle, PlatformConfig,
        PlatformProfile, PlatformProfileId, SensorSelection, StreamOpenRequest,
        StreamRecordingRequest, StreamService,
    };

    fn rtp(sequence: u16, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x80, 0x80 | 98];
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(&100_u32.to_be_bytes());
        packet.extend_from_slice(&0x1234_5678_u32.to_be_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    fn pq_record(packet: &[u8]) -> Vec<u8> {
        let mut record = b"$\x00\x80\x00".to_vec();
        record.extend_from_slice(&u32::try_from(packet.len()).unwrap().to_be_bytes());
        record.extend_from_slice(packet);
        record
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let expected_request_len = MediaRequest {
        host: "127.0.0.1".to_owned(),
        port: address.port(),
        channel: 0,
        media: "video_data".to_owned(),
        cseq: 1,
    }
    .to_bytes()
    .unwrap()
    .len();
    let server = std::thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        let mut request = vec![0_u8; expected_request_len];
        connection.read_exact(&mut request).unwrap();
        connection
            .write_all(b"HTTP/1.1 200 OK\r\nSession: 42\r\n\r\n")
            .unwrap();
        connection
            .write_all(b"m=video 98 H265/90000/2/2/30/6144\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=12345678\r\n\r\n")
            .unwrap();
        for (sequence, payload) in [
            (1, &b"\x40\x01A"[..]),
            (2, &b"\x42\x01B"[..]),
            (3, &b"\x44\x01C"[..]),
        ] {
            connection
                .write_all(&pq_record(&rtp(sequence, payload)))
                .unwrap();
        }
        let mut drain = Vec::new();
        let _ = connection.read_to_end(&mut drain);
    });

    let root = std::env::temp_dir().join(format!(
        "camera-toolbox-stream-eof-test-{}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let pid_file = root.join("sidecar.pid");
    let script = root.join("ignore-eof-ffmpeg.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nwhile :; do sleep 1; done\n",
            pid_file.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&script, permissions).unwrap();

    let platform_id = PlatformProfileId::new("gui-close-deadline").unwrap();
    let profile = PlatformProfile {
        id: platform_id.clone(),
        display_name: "GUI close deadline".to_owned(),
        config: PlatformConfig::HisiliconCv610(Cv610Config {
            host: "127.0.0.1".to_owned(),
            dump: Cv610DumpConfig::default(),
            stream: Cv610StreamConfig {
                port: address.port(),
                channel: 0,
                media: "video_data".to_owned(),
                auto_reconnect: false,
            },
        }),
    };
    let mut candidate: Cv610Bindings = HisiliconCv610Provider::default().bind(&profile).unwrap();
    let descriptor = Arc::clone(&candidate.stream.as_ref().unwrap().descriptor);
    let stream_service: Arc<dyn StreamService> = Arc::new(
        Cv610StreamService::new(
            "gui-close-deadline",
            Cv610StreamEndpoint {
                address: address.ip(),
                port: address.port(),
            },
        )
        .unwrap()
        .with_ffmpeg_path(script),
    );
    candidate.stream = Some(PlatformCapabilityHandle {
        service: stream_service,
        descriptor,
    });
    let bindings = PlatformBindings::Cv610(Arc::new(candidate));
    let key = CapabilityResolutionKey {
        platform_id,
        sensor: SensorSelection::Unbound,
    };
    let snapshot = DefaultCapabilityResolver
        .resolve(&key, &bindings, None, None)
        .unwrap();

    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let (session_id, latest) = app
        .live_runtime
        .start_resolved_for_test(
            Arc::new(snapshot),
            StreamOpenRequest {
                channel: 0,
                media: "video_data".to_owned(),
                cseq: 1,
                prefer_hardware_acceleration: false,
                recording: StreamRecordingRequest::default(),
            },
        )
        .unwrap();
    let document_id = app
        .workspace
        .open_live(session_id.clone(), latest, test_live_source());
    let pid_deadline = Instant::now() + Duration::from_secs(1);
    while !pid_file.exists() && Instant::now() < pid_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = fs::read_to_string(&pid_file).unwrap().trim().to_owned();

    assert!(app.live_runtime.request_close(&session_id));
    let close_started = Instant::now();
    app.workspace.live_mut(document_id).unwrap().lifecycle =
        crate::workspace::LiveDocumentLifecycle::Closing {
            stop_deadline: close_started + LIVE_STOP_TIMEOUT,
        };
    let mut frame = eframe::Frame::_new_kittest();
    let ui_started = Instant::now();
    run_app_frame(&context, &mut app, &mut frame, Vec::new());
    assert!(ui_started.elapsed() < Duration::from_millis(250));
    assert!(matches!(
        app.workspace.live_mut(document_id).unwrap().lifecycle,
        crate::workspace::LiveDocumentLifecycle::Closing { stop_deadline }
            if stop_deadline.duration_since(close_started) == LIVE_STOP_TIMEOUT
    ));

    app.advance_live_close_deadlines();
    assert!(matches!(
        app.workspace.live_mut(document_id).unwrap().lifecycle,
        crate::workspace::LiveDocumentLifecycle::Closing { .. }
    ));
    if let crate::workspace::LiveDocumentLifecycle::Closing { stop_deadline } =
        &mut app.workspace.live_mut(document_id).unwrap().lifecycle
    {
        *stop_deadline = Instant::now() - Duration::from_millis(1);
    }
    app.advance_live_close_deadlines();
    // doc is auto-removed by force_cleanup, not left as ForcedCleanup
    assert!(app.workspace.live_mut(document_id).is_none());

    let process_path = PathBuf::from(format!("/proc/{pid}"));
    let reap_deadline = Instant::now() + Duration::from_secs(1);
    while process_path.exists() && Instant::now() < reap_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_path.exists(),
        "deadline must kill and reap FFmpeg sidecar"
    );
    server.join().unwrap();
    let _ = fs::remove_dir_all(root);
}

#[test]
fn gui_startup_does_not_create_implicit_configuration_files() {
    const PROBE: &str = "CAMERA_TOOLBOX_CONFIG_PROBE";
    const TEST_NAME: &str = "app::tests::gui_startup_does_not_create_implicit_configuration_files";
    let root = std::env::temp_dir().join(format!(
        "camera-toolbox-config-probe-{}",
        std::process::id()
    ));

    if std::env::var_os(PROBE).is_some() {
        let root = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap());
        let context = egui::Context::default();
        let mut app = CameraToolboxApp::new(&context).unwrap();
        let mut frame = eframe::Frame::_new_kittest();
        run_app_frame(&context, &mut app, &mut frame, Vec::new());
        drop(app);
        for file in [
            "workspace-settings.json",
            "connections.json",
            "platform-profiles.json",
        ] {
            assert!(!root.join("camera-toolbox").join(file).exists());
        }
        return;
    }

    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home")).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(PROBE, "1")
        .env("XDG_CONFIG_HOME", &root)
        .env("HOME", root.join("home"))
        .status()
        .unwrap();
    assert!(status.success());
    for file in [
        "workspace-settings.json",
        "connections.json",
        "platform-profiles.json",
    ] {
        assert!(!root.join("camera-toolbox").join(file).exists());
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn remote_raw_progress_is_generation_safe_and_visible() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.active_raw_open = Some(ActiveRawOpenJob {
        attempt: 2,
        path: PathBuf::from("sftp://camera/remote.raw"),
        remote: true,
        progress: None,
        cancellation: FsCancellation::default(),
    });

    app.raw_open_sender
        .send(RawOpenJobEvent::Progress {
            attempt: 1,
            progress: SourceReadProgress {
                bytes_read: 90,
                total_bytes: 100,
            },
        })
        .unwrap();
    app.poll_raw_open_result(&context);
    assert!(app.active_raw_open.as_ref().unwrap().progress.is_none());

    app.raw_open_sender
        .send(RawOpenJobEvent::Progress {
            attempt: 2,
            progress: SourceReadProgress {
                bytes_read: 50,
                total_bytes: 100,
            },
        })
        .unwrap();
    app.poll_raw_open_result(&context);
    assert_eq!(
        app.active_raw_open.as_ref().unwrap().progress,
        Some(SourceReadProgress {
            bytes_read: 50,
            total_bytes: 100,
        })
    );

    let output = context.run_ui(egui::RawInput::default(), |ui| app.render_status_bar(ui));
    let visible = output
        .platform_output
        .accesskit_update
        .expect("accessibility tree is enabled")
        .nodes
        .into_iter()
        .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(visible.contains("Transferring remote.raw"));
    assert!(visible.contains("50%"));
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn calibration_workspace_switch_preserves_viewer_documents() {
    let context = egui::Context::default();
    context.enable_accesskit();
    context.set_theme(egui::Theme::Light);
    let mut app = app_with_loaded_raw(&context);
    let viewer_document = app.workspace.active_id();
    let mut frame = eframe::Frame::_new_kittest();

    let _ = run_app_frame(&context, &mut app, &mut frame, Vec::new());
    let viewer_theme = context.theme();
    app.product_workspace = super::ProductWorkspace::Calibration;

    let output = run_app_frame(&context, &mut app, &mut frame, Vec::new());
    let visible = output
        .platform_output
        .accesskit_update
        .expect("accessibility tree is enabled")
        .nodes
        .into_iter()
        .filter_map(|(_, node)| node.label().or_else(|| node.value()).map(str::to_owned))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(app.is_calibration_workspace());
    assert_eq!(context.theme(), viewer_theme);
    assert_eq!(app.workspace.active_id(), viewer_document);
    assert!(visible.contains("Intrinsic Calibration"));
    assert!(visible.contains("Dataset (0)"));
    assert!(visible.contains("Dataset acceptance"));
    assert!(!visible.contains("Observe-only"));
}

#[test]
fn live_status_bar_reports_stream_stage_before_media_negotiation() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let latest = Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default());
    app.workspace.open_live(
        camera_toolbox_app::StreamSessionId::new("live-status-stage-test").unwrap(),
        latest,
        test_live_source(),
    );
    app.workspace
        .active_live_mut()
        .expect("live document is active")
        .stage = camera_toolbox_app::StreamStage::ConfirmingRtp;

    let mut frame = eframe::Frame::_new_kittest();
    let output = run_app_frame(&context, &mut app, &mut frame, Vec::new());
    let visible = accessibility_text(&output);
    assert!(visible.contains("Stream stage: ConfirmingRtp"));
    assert!(visible.contains("Stage: ConfirmingRtp"));
    assert!(!visible.contains("Negotiating"));
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn calibration_mode_open_live_document_creates_live_session() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.product_workspace = super::ProductWorkspace::Calibration;
    let source = x5_233_live_source(
        "10.21.12.108",
        0,
        9073,
        camera_toolbox_app::RtspTransport::Tcp,
        1920,
        1080,
    );
    let session_id = camera_toolbox_app::StreamSessionId::new("x5-calibration-open-test").unwrap();
    let latest = Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default());

    let live_id = app.open_live_workspace_document(session_id, latest, source.clone());

    assert_eq!(app.workspace.active_id(), Some(live_id));
    assert_eq!(app.calibration.workspace_count_for_test(), 2);
    assert_eq!(
        app.calibration.active_label_for_test(),
        "X5_233 10.21.12.108 CH0"
    );
    assert!(app.calibration.active_accepts_live_source(Some(&source)));
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn calibration_session_selection_reactivates_matching_live_document() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.product_workspace = super::ProductWorkspace::Calibration;
    let source_ch0 = x5_233_live_source(
        "10.21.12.108",
        0,
        9073,
        camera_toolbox_app::RtspTransport::Tcp,
        1920,
        1080,
    );
    let source_ch3 = x5_233_live_source(
        "10.21.12.108",
        3,
        9073,
        camera_toolbox_app::RtspTransport::Tcp,
        1920,
        1080,
    );
    let ch0_id = app.open_live_workspace_document(
        camera_toolbox_app::StreamSessionId::new("x5-calibration-ch0-test").unwrap(),
        Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default()),
        source_ch0.clone(),
    );
    let ch3_id = app.open_live_workspace_document(
        camera_toolbox_app::StreamSessionId::new("x5-calibration-ch3-test").unwrap(),
        Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default()),
        source_ch3.clone(),
    );
    assert_eq!(app.workspace.active_id(), Some(ch3_id));

    app.calibration.activate_live_source_session(&source_ch0);
    app.sync_active_calibration_live_document();

    assert_eq!(app.workspace.active_id(), Some(ch0_id));
    assert!(
        app.calibration.active_accepts_live_source(
            app.workspace.active_live().map(|document| &document.source)
        )
    );
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn calibration_workspace_embeds_live_viewer_in_primary_inspection() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.explorer_panel_expanded = true;
    app.explorer.select_rtsp_mode_for_test();
    let latest = Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default());
    latest.publish(camera_toolbox_app::DecodedVideoFrame {
        width: 2,
        height: 2,
        rgba: Arc::from(vec![
            255_u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ]),
        identity: camera_toolbox_app::StreamFrameIdentity::unavailable(
            camera_toolbox_app::StreamSessionId::new("calibration-live-viewer-test").unwrap(),
            0,
            1,
            "test frame has no source PTS",
        ),
    });
    app.workspace.open_live(
        camera_toolbox_app::StreamSessionId::new("calibration-live-viewer-test").unwrap(),
        latest,
        test_live_source(),
    );
    app.workspace
        .active_live_mut()
        .expect("live document is active")
        .show_calibration_detection = false;
    app.product_workspace = super::ProductWorkspace::Calibration;
    app.calibration
        .ensure_live_source_for_test(&test_live_source());
    let mut frame = eframe::Frame::_new_kittest();
    let viewport = egui::vec2(1568.0, 882.0);
    let mut output =
        run_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, Vec::new());
    let visible = accessibility_text(&output);
    assert!(visible.contains("RTSP · Test · CH0"));
    assert!(visible.contains("Stream stage: Connecting"));
    assert!(!visible.contains("Negotiating"));
    assert!(visible.contains("Intrinsic Calibration"));
    assert!(visible.contains("Dataset (0)"));
    assert!(visible.contains("EEPROM Provisioning"));
    assert!(visible.contains("Capture"));
    assert!(!visible.contains("Capture → Calibration dataset"));
    assert_eq!(accessibility_exact_label_count(&output, "Capture"), 1);
    assert!(!visible.contains("Preview and constraints"));
    assert!(!visible.contains("Dataset coverage"));
    assert!(visible.contains("Live Stream"));
    assert!(visible.contains("Dataset Image"));
    assert!(visible.contains("Calibration result"));
    let live_bounds = accesskit_bounds(&output, "Board detection");
    assert!(
        f64::from(live_bounds.y1) < f64::from(viewport.y) * 0.75,
        "live viewer {live_bounds:?} should be in top 75% of viewport {viewport:?}"
    );

    let dataset_img_toggle = accesskit_rect_center(accesskit_bounds(&output, "Dataset Image"));
    output = run_app_frame_with_viewport(
        &context,
        &mut app,
        &mut frame,
        viewport,
        vec![
            egui::Event::PointerMoved(dataset_img_toggle),
            egui::Event::PointerButton {
                pos: dataset_img_toggle,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: dataset_img_toggle,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ],
    );
    output = settle_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, 5.0);
    assert!(accessibility_text(&output).contains("Preview and constraints"));
    let dataset_bounds = accesskit_bounds(&output, "»");
    assert!(live_bounds.x1 < dataset_bounds.x0);
    // 使用 viewport width 估算 sidebar 应占右侧区域（min_size 300px），
    // dataset 收起按钮 `»` 的 x0 应 >= sidebar 的估算左边界。
    let viewport_width = f64::from(viewport.x);
    let sidebar_lx = viewport_width - 360.0; // default_size 360
    assert!(
        f64::from(dataset_bounds.x0) >= sidebar_lx - 10.0,
        "dataset control at {dataset_bounds:?} should be right of estimated sidebar left {sidebar_lx}"
    );

    let collapse = accesskit_rect_center(accesskit_bounds(&output, "»"));
    output = run_app_frame_with_viewport(
        &context,
        &mut app,
        &mut frame,
        viewport,
        vec![
            egui::Event::PointerMoved(collapse),
            egui::Event::PointerButton {
                pos: collapse,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: collapse,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ],
    );
    output = settle_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, 6.0);
    assert!(!accessibility_text(&output).contains("Dataset (0)"));
    let expand = accesskit_rect_center(accesskit_bounds(&output, "«"));
    output = run_app_frame_with_viewport(
        &context,
        &mut app,
        &mut frame,
        viewport,
        vec![
            egui::Event::PointerMoved(expand),
            egui::Event::PointerButton {
                pos: expand,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: expand,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ],
    );
    output = settle_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, 7.0);
    assert!(accessibility_text(&output).contains("Dataset (0)"));

    let eeprom = accesskit_rect_center(accesskit_bounds(&output, "EEPROM Provisioning"));
    output = run_app_frame_with_viewport(
        &context,
        &mut app,
        &mut frame,
        viewport,
        vec![
            egui::Event::PointerMoved(eeprom),
            egui::Event::PointerButton {
                pos: eeprom,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: eeprom,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ],
    );
    output = settle_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, 8.0);
    assert!(accessibility_text(&output).contains("YgStereo SNID"));
    let eeprom = accesskit_rect_center(accesskit_bounds(&output, "EEPROM Provisioning"));
    output = run_app_frame_with_viewport(
        &context,
        &mut app,
        &mut frame,
        viewport,
        vec![
            egui::Event::PointerMoved(eeprom),
            egui::Event::PointerButton {
                pos: eeprom,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: eeprom,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ],
    );
    output = settle_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, 9.0);
    assert!(!accessibility_text(&output).contains("YgStereo SNID"));
    let capture = accesskit_rect_center(accesskit_bounds(&output, "Capture"));
    output = run_app_frame_with_viewport(
        &context,
        &mut app,
        &mut frame,
        viewport,
        vec![
            egui::Event::PointerMoved(capture),
            egui::Event::PointerButton {
                pos: capture,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: capture,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ],
    );
    output = settle_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, 10.0);
    let visible = accessibility_text(&output);
    assert!(visible.contains("Dataset (1)"));
    assert_eq!(accessibility_exact_label_count(&output, "Capture"), 1);
    let document = app.workspace.active_live().unwrap();
    assert!(document.texture().is_some());
    assert_eq!(
        document.displayed_frame().unwrap().identity.frame_sequence,
        1
    );
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn viewer_rtsp_sidebar_capture_opens_viewer_document() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.explorer_panel_expanded = true;
    app.explorer.select_rtsp_mode_for_test();
    let session_id =
        camera_toolbox_app::StreamSessionId::new("viewer-sidebar-capture-test").unwrap();
    let latest = Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default());
    latest.publish(camera_toolbox_app::DecodedVideoFrame {
        width: 2,
        height: 2,
        rgba: Arc::from(vec![
            255_u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ]),
        identity: camera_toolbox_app::StreamFrameIdentity::unavailable(
            session_id.clone(),
            0,
            1,
            "viewer sidebar capture test",
        ),
    });
    let live_id = app
        .workspace
        .open_live(session_id, latest, test_live_source());
    app.workspace
        .active_live_mut()
        .expect("live document is active")
        .show_calibration_detection = false;

    let mut frame = eframe::Frame::_new_kittest();
    let viewport = egui::vec2(1568.0, 882.0);
    let output = run_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, Vec::new());
    let visible = accessibility_text(&output);
    assert!(visible.contains("Capture"));
    assert!(visible.contains("Capture route follows current workspace: Viewer"));
    assert_eq!(accessibility_exact_label_count(&output, "Capture"), 1);

    app.handle_workspace_stream_action(&context, super::WorkspaceStreamAction::Capture(live_id));
    let document = app
        .workspace
        .active_image()
        .expect("Viewer-mode stream capture opens an image document");
    assert!(document.title.contains("stream-ch0-frame1.png"));
    assert!(!app.is_calibration_workspace());
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn sidebar_capture_uses_displayed_frame_when_latest_slot_advances() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.product_workspace = super::ProductWorkspace::Calibration;
    app.explorer_panel_expanded = true;
    app.explorer.select_rtsp_mode_for_test();
    let session_id =
        camera_toolbox_app::StreamSessionId::new("displayed-frame-capture-test").unwrap();
    let latest = Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default());
    latest.publish(camera_toolbox_app::DecodedVideoFrame {
        width: 2,
        height: 2,
        rgba: Arc::from(vec![
            255_u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ]),
        identity: camera_toolbox_app::StreamFrameIdentity::unavailable(
            session_id.clone(),
            0,
            1,
            "displayed frame capture test",
        ),
    });
    let live_id =
        app.workspace
            .open_live(session_id.clone(), Arc::clone(&latest), test_live_source());
    app.workspace
        .active_live_mut()
        .expect("live document is active")
        .show_calibration_detection = false;

    let mut frame = eframe::Frame::_new_kittest();
    let viewport = egui::vec2(1568.0, 882.0);
    let _ = run_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, Vec::new());
    assert_eq!(
        app.workspace
            .live(live_id)
            .and_then(|document| document.displayed_frame())
            .map(|displayed| displayed.identity.frame_sequence),
        Some(1)
    );

    latest.publish(camera_toolbox_app::DecodedVideoFrame {
        width: 2,
        height: 2,
        rgba: Arc::from(vec![
            0_u8, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255,
        ]),
        identity: camera_toolbox_app::StreamFrameIdentity::unavailable(
            session_id,
            0,
            2,
            "newer frame must not replace displayed capture",
        ),
    });
    app.handle_workspace_stream_action(&context, super::WorkspaceStreamAction::Capture(live_id));

    let output = settle_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, 12.0);
    let visible = accessibility_text(&output);
    assert!(visible.contains("Dataset (1)"));
    assert!(visible.contains("RTSP ch0 frame 1"));
    assert!(!visible.contains("RTSP ch0 frame 2"));
}

#[test]
fn color_stream_capture_creates_static_capture_image() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.product_workspace = super::ProductWorkspace::Color;

    app.explorer_panel_expanded = true;
    app.explorer.select_rtsp_mode_for_test();
    let session_id = camera_toolbox_app::StreamSessionId::new("color-capture-test").unwrap();
    let latest = Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default());
    latest.publish(test_decoded_frame(&session_id, 1, 128));
    let live_id = app
        .workspace
        .open_live(session_id, Arc::clone(&latest), test_live_source());
    let mut frame = eframe::Frame::_new_kittest();
    let viewport = egui::vec2(1568.0, 882.0);
    let _ = run_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, Vec::new());

    app.handle_workspace_stream_action(&context, super::WorkspaceStreamAction::Capture(live_id));

    let document = app
        .workspace
        .active_image()
        .expect("Color capture opens an image");
    assert!(document.is_color_capture());
    assert_eq!(document.native.dimensions(), [1, 1]);
    assert!(document.title.contains("color-rtsp-ch0-frame1.png"));
    assert!(app.is_color_workspace());
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn inactive_stream_capture_activates_clicked_row_and_uses_snapshot() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    app.product_workspace = super::ProductWorkspace::Calibration;
    app.explorer_panel_expanded = true;
    app.explorer.select_rtsp_mode_for_test();

    let first_session = camera_toolbox_app::StreamSessionId::new("inactive-capture-first").unwrap();
    let first_latest = Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default());
    first_latest.publish(camera_toolbox_app::DecodedVideoFrame {
        width: 2,
        height: 2,
        rgba: Arc::from(vec![
            255_u8, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
        ]),
        identity: camera_toolbox_app::StreamFrameIdentity::unavailable(
            first_session.clone(),
            0,
            1,
            "first displayed frame",
        ),
    });
    let first_id = app.workspace.open_live(
        first_session,
        first_latest,
        crate::workspace::LiveStreamSource::Rtsp {
            label: "First".to_owned(),
            channel: 0,
            transport: camera_toolbox_app::RtspTransport::Tcp,
            source_fingerprint: "test-rtsp-source-first".to_owned(),
            geometry_key: "test-rtsp-config-first".to_owned(),
            authoritative_capture: None,
        },
    );
    app.workspace
        .live_mut(first_id)
        .expect("first live document")
        .show_calibration_detection = false;

    let mut frame = eframe::Frame::_new_kittest();
    let viewport = egui::vec2(1568.0, 882.0);
    let _ = run_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, Vec::new());
    assert_eq!(
        app.workspace
            .live(first_id)
            .and_then(|document| document.displayed_frame())
            .map(|displayed| displayed.identity.frame_sequence),
        Some(1)
    );

    let second_session =
        camera_toolbox_app::StreamSessionId::new("inactive-capture-second").unwrap();
    let second_latest = Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default());
    second_latest.publish(camera_toolbox_app::DecodedVideoFrame {
        width: 2,
        height: 2,
        rgba: Arc::from(vec![
            0_u8, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255,
        ]),
        identity: camera_toolbox_app::StreamFrameIdentity::unavailable(
            second_session.clone(),
            1,
            7,
            "second displayed frame",
        ),
    });
    let second_id = app.workspace.open_live(
        second_session,
        second_latest,
        crate::workspace::LiveStreamSource::Rtsp {
            label: "Second".to_owned(),
            channel: 1,
            transport: camera_toolbox_app::RtspTransport::Tcp,
            source_fingerprint: "test-rtsp-source-second".to_owned(),
            geometry_key: "test-rtsp-config-second".to_owned(),
            authoritative_capture: None,
        },
    );
    app.workspace
        .live_mut(second_id)
        .expect("second live document")
        .show_calibration_detection = false;

    let output = settle_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, 13.0);
    assert_eq!(app.workspace.active_id(), Some(second_id));
    assert_eq!(
        app.workspace
            .live(first_id)
            .and_then(|document| document.displayed_frame())
            .map(|displayed| displayed.identity.frame_sequence),
        Some(1),
        "inactive row must retain its last displayed snapshot"
    );
    assert!(
        app.workspace
            .live(first_id)
            .expect("first live document")
            .texture()
            .is_none(),
        "inactive row should release only its GPU texture"
    );
    assert_eq!(
        app.workspace
            .live(second_id)
            .and_then(|document| document.displayed_frame())
            .map(|displayed| displayed.identity.frame_sequence),
        Some(7)
    );

    let mut capture_bounds = accesskit_bounds_all(&output, "Capture");
    capture_bounds.sort_by(|left, right| {
        left.y0
            .partial_cmp(&right.y0)
            .expect("accessibility row bounds are finite")
    });
    assert_eq!(
        capture_bounds.len(),
        2,
        "both stream rows must expose Capture"
    );
    let first_capture = accesskit_rect_center(capture_bounds[0]);
    _ = run_app_frame_with_viewport(
        &context,
        &mut app,
        &mut frame,
        viewport,
        vec![
            egui::Event::PointerMoved(first_capture),
            egui::Event::PointerButton {
                pos: first_capture,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: first_capture,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ],
    );
    let output = settle_app_frame_with_viewport(&context, &mut app, &mut frame, viewport, 14.0);
    let visible = accessibility_text(&output);
    assert_eq!(app.workspace.active_id(), Some(first_id));
    assert!(
        app.workspace
            .live(first_id)
            .expect("first live document")
            .texture()
            .is_some(),
        "activating a captured inactive row must reinstall its texture"
    );
    assert!(app.is_calibration_workspace());
    assert!(visible.contains("Dataset (1)"));
    assert!(visible.contains("RTSP ch0 frame 1"));
    assert!(!visible.contains("RTSP ch1 frame 7"));
}

#[cfg(all(feature = "calibration-opencv", feature = "platform-ssh"))]
mod eeprom_operation_tests {
    use super::super::*;
    use std::{fs, path::PathBuf, sync::Arc};

    use camera_toolbox_adapters::platforms::ssh_managed::{
        CredentialResolver, MemorySshTransport, SshTransportFactory,
    };
    use camera_toolbox_app::{
        EepromDeviceState, EepromHelperFailure, EepromProvisionService, EepromRollbackState,
        EepromSerialState, RemoteAuthentication, RemoteConnectionConfig, RemoteConnectionId,
        SnapshotHash,
    };
    use camera_toolbox_core::{
        EepromProvisionRequest, EepromProvisioningMode, EepromWriteSegment,
        YG_STEREO_P24C64G_IMAGE_BYTES, YG_STEREO_P24C64G_V1_MAP_ID,
    };

    #[derive(Clone)]
    struct FixedEepromService {
        result: Result<EepromHelperResult, EepromProvisionServiceError>,
    }

    impl EepromProvisionService for FixedEepromService {
        fn service_id(&self) -> &str {
            "fixed-test-eeprom"
        }

        fn execute(
            &self,
            _request: EepromProvisionOperation,
            _control: RemoteOperationControl,
        ) -> Result<EepromHelperResult, EepromProvisionServiceError> {
            self.result.clone()
        }
    }

    fn state(hash: char) -> EepromDeviceState {
        EepromDeviceState {
            image_sha256: hash.to_string().repeat(64),
            flag_valid: false,
            serial: EepromSerialState::Empty,
        }
    }

    fn request() -> EepromProvisionRequest {
        request_with_sn("2T02D2567K0042")
    }

    fn request_with_sn(serial_number: &str) -> EepromProvisionRequest {
        EepromProvisionRequest {
            map_id: YG_STEREO_P24C64G_V1_MAP_ID.to_owned(),
            mode: EepromProvisioningMode::UpdateCalibration,
            serial_number: serial_number.to_owned(),
            overwrite_existing_serial: false,
            segments: Vec::new(),
        }
    }

    fn calibration_segment(width: u32, height: u32, fx: f32, fy: f32, cx: f32, cy: f32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        for value in [fx, fy, cx, cy] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for index in 0..12_u32 {
            bytes.extend_from_slice(&(0.1_f32 + index as f32).to_le_bytes());
        }
        bytes
    }

    fn request_with_calibration_segment(serial_number: &str) -> EepromProvisionRequest {
        let mut request = request_with_sn(serial_number);
        request.segments = vec![EepromWriteSegment {
            offset: 0x0010,
            bytes: calibration_segment(1920, 1080, 1234.5, 1235.5, 960.25, 540.75),
        }];
        request
    }

    fn provision_intent(request: EepromProvisionRequest) -> CalibrationProvisionIntent {
        CalibrationProvisionIntent::Provision {
            request,
            expected_before_sha256: "a".repeat(64),
        }
    }

    fn target(
        result: Result<EepromHelperResult, EepromProvisionServiceError>,
    ) -> EepromProvisioningTarget {
        EepromProvisioningTarget {
            service: Arc::new(FixedEepromService { result }),
            snapshot_hash: SnapshotHash::digest_bytes(b"target"),
            label: "root@camera.local:22 / i2c-7 @test".to_owned(),
            i2c_bus: 7,
        }
    }

    fn history_path(serial_number: &str) -> PathBuf {
        eeprom_history_path(serial_number).unwrap()
    }

    fn legacy_history_path(serial_number: &str) -> PathBuf {
        std::path::Path::new("write_history").join(format!("{serial_number}.json"))
    }

    fn history_file_path(file_name: &str) -> PathBuf {
        std::path::Path::new("write_history").join(file_name)
    }

    fn write_history_with_recorded_snid(file_name: &str, serial_number: &str) -> PathBuf {
        fs::create_dir_all("write_history").unwrap();
        let path = history_file_path(file_name);
        let document = serde_json::json!({
            "schema_version": 2,
            "request": {
                "request": {
                    "serial_number": serial_number,
                },
            },
        });
        fs::write(&path, serde_yaml::to_string(&document).unwrap()).unwrap();
        path
    }

    fn remove_history_file(file_name: &str) {
        let _ = fs::remove_file(history_file_path(file_name));
    }

    fn remove_history(serial_number: &str) {
        let _ = fs::remove_file(history_path(serial_number));
        let _ = fs::remove_file(legacy_history_path(serial_number));
    }

    fn read_history(serial_number: &str) -> serde_json::Value {
        serde_yaml::from_slice(&fs::read(history_path(serial_number)).unwrap()).unwrap()
    }

    fn read_history_file(path: &std::path::Path) -> serde_json::Value {
        serde_yaml::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn cleanup_history(serial_number: &str) {
        fs::remove_file(history_path(serial_number)).unwrap();
    }

    #[test]
    fn provision_success_writes_yaml_with_bus_and_original_parameters() {
        let serial = "2T233268101900";
        remove_history(serial);
        let request = request_with_calibration_segment(serial);
        let helper = EepromHelperResult::Provision(EepromWriteResult {
            before: state('a'),
            after: state('c'),
            backup: vec![0x44; YG_STEREO_P24C64G_IMAGE_BYTES],
            page_plan: Vec::new(),
            bytewise_verified: true,
            rollback: EepromRollbackState::NotRequired,
        });

        let outcome = run_eeprom_operation(
            target(Ok(helper)),
            provision_intent(request),
            45,
            DumpCancellation::default(),
        )
        .unwrap();

        let EepromOperationOutcome::Provision { history_file, .. } = outcome else {
            panic!("expected provision outcome")
        };
        assert_eq!(history_file, history_path(serial).display().to_string());
        let audit = read_history(serial);
        assert_eq!(audit["schema_version"], 2);
        assert_eq!(audit["operation"], "eeprom_provision_success");
        assert_eq!(audit["target"]["i2c_bus"], 7);
        assert_eq!(audit["request"]["request"]["mode"], "update_calibration");
        assert_eq!(
            audit["request"]["request"]["calibration_parameters"]["image_size"]["width"],
            1920
        );
        assert_eq!(
            audit["request"]["request"]["calibration_parameters"]["image_size"]["height"],
            1080
        );
        assert_eq!(
            audit["request"]["request"]["calibration_parameters"]["camera_matrix"]["fx"],
            1234.5
        );
        assert_eq!(
            audit["request"]["request"]["write_segments"][0]["semantic_value"]["camera_matrix"]["cx"],
            960.25
        );
        assert_eq!(
            audit["result"]["backup_bytes"],
            YG_STEREO_P24C64G_IMAGE_BYTES
        );
        assert!(audit["result"].get("backup").is_none());
        cleanup_history(serial);
    }

    #[test]
    fn history_file_name_converts_snid_date_and_sequence_to_decimal() {
        assert_eq!(
            safe_eeprom_history_file_name("2T233268101a00").unwrap(),
            "2T233000_260801_73.yaml"
        );
        assert_eq!(
            safe_eeprom_history_file_name("2T23326CV0ZZ00").unwrap(),
            "2T233000_261231_3844.yaml"
        );
    }

    #[test]
    fn history_slot_allows_case_distinct_snids() {
        let existing = "2T233268101a00";
        let requested = "2T233268101A00";
        remove_history(existing);
        remove_history(requested);
        let existing_path = write_history_with_recorded_snid("2T233268101a00.yaml", existing);

        let result = ensure_eeprom_history_slot_available(requested);
        fs::remove_file(existing_path).unwrap();

        assert!(result.is_ok());
    }

    #[test]
    fn history_slot_rejects_exact_snid_across_case_distinct_filename() {
        let requested = "2T233268201A00";
        remove_history(requested);
        remove_history_file("2T233268201a00.yaml");
        let existing_path = write_history_with_recorded_snid("2T233268201a00.yaml", requested);

        let error = ensure_eeprom_history_slot_available(requested).unwrap_err();
        fs::remove_file(existing_path).unwrap();

        assert!(error.contains("already records SN 2T233268201A00"));
    }

    #[test]
    fn history_slot_checks_all_history_candidates_by_recorded_snid() {
        let requested = "2T233268301B00";
        remove_history(requested);
        remove_history_file("2T233268301b00.yaml");
        remove_history_file("legacy-case-candidate-03.json");
        let yaml_path = write_history_with_recorded_snid("2T233268301b00.yaml", "2T233268301b00");
        let json_path =
            write_history_with_recorded_snid("legacy-case-candidate-03.json", requested);

        let error = ensure_eeprom_history_slot_available(requested).unwrap_err();
        fs::remove_file(yaml_path).unwrap();
        fs::remove_file(json_path).unwrap();

        assert!(error.contains("already records SN 2T233268301B00"));
    }

    #[test]
    fn history_slot_rejects_occupied_decimal_filename_without_matching_snid() {
        let requested = "2T233268401C00";
        let occupant = "2T233268401c00";
        remove_history(requested);
        let occupied_name = safe_eeprom_history_file_name(requested).unwrap();
        remove_history_file(&occupied_name);
        let occupied_path = write_history_with_recorded_snid(&occupied_name, occupant);

        let error = ensure_eeprom_history_slot_available(requested).unwrap_err();
        fs::remove_file(occupied_path).unwrap();

        assert!(error.contains("filename for SN 2T233268401C00 is already occupied"));
    }

    #[test]
    fn persist_history_uses_decimal_snid_filename() {
        let requested = "2T233268501Z00";
        remove_history(requested);
        let expected_name = "2T233000_260805_124.yaml";
        remove_history_file(expected_name);
        let document = serde_json::json!({
            "request": {
                "request": {
                    "serial_number": requested,
                },
            },
        });

        let history_file = persist_eeprom_write_history_yaml(requested, 8, &document).unwrap();
        let saved_path = PathBuf::from(&history_file);
        let saved_audit = read_history_file(&saved_path);
        fs::remove_file(&saved_path).unwrap();

        assert_eq!(saved_path, history_file_path(expected_name));
        assert_eq!(
            saved_audit["request"]["request"]["serial_number"],
            requested
        );
    }

    #[test]
    fn provision_failure_saves_structured_rollback_audit() {
        let failure = EepromHelperFailure {
            code: "write_failed".to_owned(),
            message: "simulated page failure".to_owned(),
            before: Some(state('a')),
            backup: vec![0x5a; YG_STEREO_P24C64G_IMAGE_BYTES],
            rollback: EepromRollbackState::Restored,
            rollback_error: None,
        };
        let serial = "2T233268101d00";
        remove_history(serial);

        let error = run_eeprom_operation(
            target(Err(EepromProvisionServiceError::Helper(failure))),
            provision_intent(request_with_sn(serial)),
            43,
            DumpCancellation::default(),
        )
        .unwrap_err();

        assert!(error.message.contains("rollback=Restored"));
        assert!(!error.provision_state_unknown);
        let audit = read_history(serial);
        assert_eq!(audit["operation"], "eeprom_provision_failure");
        assert_eq!(audit["failure"]["code"], "write_failed");
        assert_eq!(audit["failure"]["rollback"], "restored");
        assert!(audit["failure"].get("backup").is_none());
        cleanup_history(serial);
    }

    #[test]
    fn provision_failed_rollback_marks_device_unknown() {
        let failure = EepromHelperFailure {
            code: "rollback_failed".to_owned(),
            message: "write and rollback both failed".to_owned(),
            before: Some(state('a')),
            backup: vec![0x5a; YG_STEREO_P24C64G_IMAGE_BYTES],
            rollback: EepromRollbackState::Failed,
            rollback_error: Some("read-back mismatch".to_owned()),
        };
        let serial = "2T233268101e00";
        remove_history(serial);
        let error = run_eeprom_operation(
            target(Err(EepromProvisionServiceError::Helper(failure))),
            provision_intent(request_with_sn(serial)),
            44,
            DumpCancellation::default(),
        )
        .unwrap_err();

        assert!(error.provision_state_unknown);
        assert!(error.message.contains("rollback=Failed"));
        cleanup_history(serial);
    }

    #[test]
    fn provision_transport_failure_marks_device_unknown() {
        let serial = "2T233268101f00";
        remove_history(serial);

        let error = run_eeprom_operation(
            target(Err(EepromProvisionServiceError::Transport(
                "SSH response was lost".to_owned(),
            ))),
            provision_intent(request_with_sn(serial)),
            44,
            DumpCancellation::default(),
        )
        .unwrap_err();

        assert!(error.provision_state_unknown);
        assert!(error.message.contains("SSH response was lost"));
        let audit = read_history(serial);
        assert_eq!(audit["device_state_unknown"], true);
        cleanup_history(serial);
    }
    #[test]
    fn failed_reconfiguration_drops_previous_eeprom_target() {
        let context = egui::Context::default();
        let mut app = CameraToolboxApp::new(&context).unwrap();
        app.eeprom_target = Some(target(Err(EepromProvisionServiceError::Transport(
            "unused fixture".to_owned(),
        ))));

        app.begin_eeprom_operation(
            &context,
            CalibrationProvisionIntent::ConfigureTarget(CalibrationEepromTargetRequest {
                i2c_bus: 7,
            }),
        );

        assert!(app.eeprom_target.is_none());
    }
    #[test]
    fn configures_eeprom_from_password_sftp_without_verified_host_identity() {
        let context = egui::Context::default();
        let mut app = CameraToolboxApp::new(&context).unwrap();
        let helper_path = CameraToolboxApp::local_eeprom_helper_candidates()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        fs::write(
            &helper_path,
            b"\x7fELF\x02\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xb7\x00",
        )
        .unwrap();
        let memory = Arc::new(MemorySshTransport::new("rotated-host-key"));
        memory.allow_credential("session:test");
        let credentials: Arc<dyn CredentialResolver> = memory.clone();
        let transport: Arc<dyn SshTransportFactory> = memory.clone();
        app.explorer = ExplorerState::new(credentials, transport);
        app.explorer
            .finish_sftp_connection(
                RemoteConnectionConfig {
                    id: RemoteConnectionId::new("memory-eeprom").unwrap(),
                    display_name: "root@camera.test:22".to_owned(),
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

        app.begin_eeprom_operation(
            &context,
            CalibrationProvisionIntent::ConfigureTarget(CalibrationEepromTargetRequest {
                i2c_bus: 7,
            }),
        );

        let target = app
            .eeprom_target
            .as_ref()
            .expect("EEPROM target configured");
        assert!(target.label.starts_with("root@camera.test:22 / i2c-7 @"));
        let _ = fs::remove_file(helper_path);
    }

    #[test]
    fn rejects_non_linux_aarch64_eeprom_helper_payload() {
        let path = PathBuf::from("wrong-helper");
        let error =
            CameraToolboxApp::validate_eeprom_helper_payload(b"not an ELF", &path).unwrap_err();
        assert!(error.contains("not a Linux AArch64 ELF"));
    }

    #[test]
    fn active_operation_rejects_target_reconfiguration_without_dropping_target() {
        let context = egui::Context::default();
        let mut app = CameraToolboxApp::new(&context).unwrap();
        app.eeprom_target = Some(target(Err(EepromProvisionServiceError::Transport(
            "unused fixture".to_owned(),
        ))));
        app.active_eeprom_cancellation = Some(DumpCancellation::default());

        app.begin_eeprom_operation(
            &context,
            CalibrationProvisionIntent::ConfigureTarget(CalibrationEepromTargetRequest {
                i2c_bus: 7,
            }),
        );

        assert!(app.eeprom_target.is_some());
        assert!(app.active_eeprom_cancellation.is_some());
    }
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn live_overlay_maps_pixel_centers_and_rejects_out_of_bounds_points() {
    let image_rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
    let image_size = camera_toolbox_core::CalibrationImageSize::new(2, 2).unwrap();

    assert_eq!(
        CameraToolboxApp::live_overlay_point(
            camera_toolbox_core::CalibrationPoint::new(0.0, 0.0),
            image_size,
            image_rect,
            false,
        ),
        Some(egui::pos2(60.0, 45.0))
    );
    assert_eq!(
        CameraToolboxApp::live_overlay_point(
            camera_toolbox_core::CalibrationPoint::new(1.0, 1.0),
            image_size,
            image_rect,
            false,
        ),
        Some(egui::pos2(160.0, 95.0))
    );
    assert_eq!(
        CameraToolboxApp::live_overlay_point(
            camera_toolbox_core::CalibrationPoint::new(-1.0, 0.0),
            image_size,
            image_rect,
            false,
        ),
        None
    );
    assert_eq!(
        CameraToolboxApp::live_overlay_point(
            camera_toolbox_core::CalibrationPoint::new(0.0, 0.0),
            image_size,
            image_rect,
            true,
        ),
        Some(egui::pos2(160.0, 45.0))
    );
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn guided_pose_arrow_depth_scales_make_near_endpoint_larger() {
    let (far_start, near_end) = CameraToolboxApp::guided_pose_arrow_depth_scales(1000.0, 600.0)
        .expect("valid positive depths produce perspective scales");
    assert!(near_end > far_start);

    let (near_start, far_end) = CameraToolboxApp::guided_pose_arrow_depth_scales(600.0, 1000.0)
        .expect("valid positive depths produce perspective scales");
    assert!(near_start > far_end);

    let (same_start, same_end) = CameraToolboxApp::guided_pose_arrow_depth_scales(800.0, 800.0)
        .expect("valid equal depths produce neutral scales");
    assert!((same_start - same_end).abs() <= f32::EPSILON);

    assert!(CameraToolboxApp::guided_pose_arrow_depth_scales(0.0, 800.0).is_none());
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn guided_pose_rotation_ring_sweep_uses_true_angle_degrees_and_preserves_direction() {
    let positive = CameraToolboxApp::guided_pose_rotation_ring_visual_sweep_degrees(15.0)
        .expect("finite angle produces a sweep");
    assert!((positive - 15.0).abs() <= f32::EPSILON);

    let negative = CameraToolboxApp::guided_pose_rotation_ring_visual_sweep_degrees(-5.0)
        .expect("finite angle produces a sweep");
    assert!((negative + 5.0).abs() <= f32::EPSILON);

    let large = CameraToolboxApp::guided_pose_rotation_ring_visual_sweep_degrees(179.5)
        .expect("finite angle produces a sweep");
    assert!((large - 179.5).abs() <= f32::EPSILON);

    assert!(CameraToolboxApp::guided_pose_rotation_ring_visual_sweep_degrees(f64::NAN).is_none());
}

#[cfg(feature = "calibration-opencv")]
#[test]
fn guided_pose_rotation_ring_geometry_only_translates_in_view_frame() {
    assert_eq!(
        CameraToolboxApp::GUIDED_POSE_RING_VIEW_ROTATION_RADIANS,
        0.0
    );

    let center_a = egui::pos2(120.0, 90.0);
    let center_b = egui::pos2(260.0, 170.0);
    let angle = 37.0_f32.to_radians();
    let offset_a = CameraToolboxApp::guided_pose_rotation_ellipse_point(
        center_a,
        24.0,
        72.0,
        CameraToolboxApp::GUIDED_POSE_RING_VIEW_ROTATION_RADIANS,
        angle,
    ) - center_a;
    let offset_b = CameraToolboxApp::guided_pose_rotation_ellipse_point(
        center_b,
        24.0,
        72.0,
        CameraToolboxApp::GUIDED_POSE_RING_VIEW_ROTATION_RADIANS,
        angle,
    ) - center_b;

    assert!((offset_a - offset_b).length() <= 1.0e-4);
}
