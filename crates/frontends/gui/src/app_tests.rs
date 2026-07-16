use std::{path::PathBuf, sync::Arc};

use camera_toolbox_app::{FsCancellation, LocalRawAnalyzeReport, SourceReadProgress};
use camera_toolbox_core::{
    AssetId, BayerPattern, CaptureMetadata, ChromaOrder, EphemeralAsset, IntegrityState,
    MediaFormat, OwnedMediaPayload, RawFrame, RawSpec, Roi, RoiStats, analyze_raw_roi, analyze_roi,
};
use eframe::egui::{self, accesskit::Role};

#[cfg(all(target_os = "linux", feature = "platform-cv610"))]
use super::LIVE_STOP_TIMEOUT;
use super::{
    ActiveRawOpenJob, CameraToolboxApp, LoadedRaw, RawOpenJobEvent, save_asset_source,
    save_asset_source_with,
};
use crate::{
    analysis_panel::DesiredAnalysis,
    analysis_worker::{AnalysisData, AnalysisDomain, AnalysisKey, AnalysisPayload, AnalysisResult},
    color_worker::ColorRenderResult,
    histogram_link::{HistogramBinSelection, HistogramSeriesId, SpatialHighlightResult},
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
    let id = app.workspace.open_asset(asset, snapshot, true).unwrap();

    app.close_document(&context, id);
    assert_eq!(app.pending_ephemeral_close, Some(id));
    assert!(app.workspace.asset(id).is_some());

    app.pending_ephemeral_close = None;
    app.workspace.asset_mut(id).unwrap().saved = true;
    app.close_document(&context, id);
    assert!(app.workspace.asset(id).is_none());
}

#[test]
fn closing_inactive_live_tab_activates_its_confirmation() {
    let context = egui::Context::default();
    let mut app = CameraToolboxApp::new(&context).unwrap();
    let first = app.workspace.open_live(
        camera_toolbox_app::StreamSessionId::new("inactive-first").unwrap(),
        Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default()),
    );
    let second = app.workspace.open_live(
        camera_toolbox_app::StreamSessionId::new("active-second").unwrap(),
        Arc::new(camera_toolbox_app::LatestDecodedFrameSlot::default()),
    );
    assert!(app.workspace.activate(second));

    app.close_document(&context, first);

    assert_eq!(app.workspace.active_live().unwrap().id, first);
    assert!(matches!(
        app.workspace.active_live().unwrap().lifecycle,
        crate::workspace::LiveDocumentLifecycle::CloseRequested
    ));
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
                recording: StreamRecordingRequest::default(),
            },
        )
        .unwrap();
    let document_id = app.workspace.open_live(session_id.clone(), latest);
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
    assert!(matches!(
        app.workspace.live_mut(document_id).unwrap().lifecycle,
        crate::workspace::LiveDocumentLifecycle::ForcedCleanup {
            terminal: camera_toolbox_app::StreamTerminal::Forced {
                remote_state_unknown: true
            }
        }
    ));

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
