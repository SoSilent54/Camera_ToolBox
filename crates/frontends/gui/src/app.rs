//! GUI 顶层编排；文档状态由 workspace 模块独立持有。

use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(feature = "platform-ssh")]
use crate::explorer::RemoteConnectionCommit;
use crate::{
    analysis_panel::{DesiredAnalysis, render_analysis_panel},
    analysis_worker::{
        AnalysisDomain, AnalysisKey, AnalysisRequest, AnalysisResult, AnalysisWorker,
    },
    auto_open::{AutoOpenCandidate, AutoOpenCoordinator},
    color_controls::{DisplayMode, render_color_controls},
    color_worker::{ColorRenderRequest, ColorRenderResult, ColorRenderWorker},
    explorer::{ExplorerAction, ExplorerState},
    histogram_link::{
        HistogramBinSelection, HistogramPixelSample, ImageHistogramHover, SpatialHighlight,
        SpatialHighlightRequest, SpatialHighlightResult, SpatialHighlightWorker,
        display_histogram_sample,
    },
    notification::{NotificationCenter, NotificationKey, NotificationScope, UiNotification},
    platform_ui::{LiveRuntime, PlatformEffect, PlatformUiAction, StreamPanelAction},
    raw_dialog::{RawOpenDialogState, local_file_source},
    raw_inspector::render_raw_inspector,
    viewer::{
        HoverNeighborhood, HoverViewSettings, ImageViewerState, LoadedRaw, ViewerAction,
        ViewerOutput, bayer_label, render_viewer,
    },
    workspace::{
        DocumentId, DocumentIdentity, LiveDocument, LiveDocumentLifecycle, TabBarAction,
        WorkspaceState, render_tab_bar,
    },
};
#[cfg(feature = "platform-ssh")]
use camera_toolbox_adapters::platforms::ssh_managed::RusshTransportFactory;
use camera_toolbox_app::{
    AutoOpenActivation, FileRef, FileSystem, FsCancellation, FsControl, LocalRawAnalyzeReport,
    LocalRawAnalyzeRequest, RawDecodeParams, RawInterpretation, RawOpenMode, RawOpenPipeline,
    RawSourceHandle, SourceCache, WorkspaceSettings, WorkspaceSettingsError,
    workspace_settings_path,
};
#[cfg(feature = "platform-ssh")]
use camera_toolbox_app::{
    ConnectionSettings, WorkspaceSettingsError as FileWorkspaceSettingsError,
    connection_settings_path,
};
use camera_toolbox_core::{
    MediaFormat, OwnedMediaPayload, PackedRawSpec, Roi, analyze_roi, decode_le_continuous_raw,
};
use eframe::egui;
const LIVE_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const AUTO_OPEN_QUEUE_LIMIT: usize = 16;
const AUTO_OPEN_BACKGROUND_TAB_LIMIT: usize = 8;

struct OpenedRawDocument {
    report: LocalRawAnalyzeReport,
    source: RawSourceHandle,
    interpretation: RawInterpretation,
}

struct WorkspaceFileOpenRequest {
    display_path: PathBuf,
    file_system: Arc<dyn FileSystem>,
    reference: FileRef,
}

struct RawOpenJobResult {
    attempt: u64,
    path: PathBuf,
    result: Result<OpenedRawDocument, String>,
}

struct ActiveRawOpenJob {
    attempt: u64,
    cancellation: FsCancellation,
}

struct PendingAutoOpenRequest {
    candidate: AutoOpenCandidate,
    request: WorkspaceFileOpenRequest,
}

struct AutoOpenJobResult {
    candidate: AutoOpenCandidate,
    path: PathBuf,
    result: Result<OpenedRawDocument, String>,
}

struct ActiveAutoOpenJob {
    candidate: AutoOpenCandidate,
    cancellation: FsCancellation,
}

struct ReinterpretJobResult {
    document_id: DocumentId,
    decode_generation: u64,
    result: Result<OpenedRawDocument, String>,
}

pub(crate) struct CameraToolboxApp {
    workspace: WorkspaceState,
    #[cfg(feature = "platform-ssh")]
    connection_settings: ConnectionSettings,
    workspace_settings: WorkspaceSettings,
    auto_open: AutoOpenCoordinator,
    explorer: ExplorerState,
    explorer_panel_expanded: bool,
    empty_viewer: ImageViewerState,
    raw_dialog: RawOpenDialogState,
    raw_pipeline: RawOpenPipeline,
    color_worker: ColorRenderWorker,
    analysis_worker: AnalysisWorker,
    spatial_worker: SpatialHighlightWorker,
    notifications: NotificationCenter,
    next_generation: u64,
    live_runtime: LiveRuntime,
    next_load_attempt: u64,
    pending_ephemeral_close: Option<DocumentId>,
    raw_open_sender: Sender<RawOpenJobResult>,
    raw_open_receiver: Receiver<RawOpenJobResult>,
    active_raw_open: Option<ActiveRawOpenJob>,
    pending_auto_open: VecDeque<PendingAutoOpenRequest>,
    auto_open_sender: Sender<AutoOpenJobResult>,
    auto_open_receiver: Receiver<AutoOpenJobResult>,
    active_auto_open: Option<ActiveAutoOpenJob>,
    auto_open_documents: BTreeMap<String, DocumentId>,
    auto_open_background_tabs: VecDeque<DocumentId>,
    reinterpret_sender: Sender<ReinterpretJobResult>,
    reinterpret_receiver: Receiver<ReinterpretJobResult>,
}

impl CameraToolboxApp {
    pub(crate) fn new(context: &egui::Context) -> std::io::Result<Self> {
        let cache = SourceCache::new(source_cache_directory(), 2 * 1024 * 1024 * 1024, 64)
            .map_err(std::io::Error::other)?;
        let workspace_settings = load_workspace_settings();
        #[cfg(feature = "platform-ssh")]
        let connection_settings = load_connection_settings();
        let (raw_open_sender, raw_open_receiver) = mpsc::channel();
        let (reinterpret_sender, reinterpret_receiver) = mpsc::channel();
        let (auto_open_sender, auto_open_receiver) = mpsc::channel();
        let live_runtime = LiveRuntime::new().map_err(std::io::Error::other)?;
        #[cfg(feature = "platform-ssh")]
        let explorer = ExplorerState::from_settings(
            &workspace_settings,
            &connection_settings,
            live_runtime.ssh_resolver(),
            Arc::new(RusshTransportFactory),
        );
        #[cfg(not(feature = "platform-ssh"))]
        let explorer = ExplorerState::from_settings(&workspace_settings);
        let auto_open = AutoOpenCoordinator::from_settings(&workspace_settings, &explorer);
        Ok(Self {
            workspace: WorkspaceState::default(),
            workspace_settings,
            #[cfg(feature = "platform-ssh")]
            connection_settings,
            explorer,
            auto_open,
            explorer_panel_expanded: false,
            empty_viewer: ImageViewerState::default(),
            raw_dialog: RawOpenDialogState::default(),
            raw_pipeline: RawOpenPipeline::new(cache, Vec::new(), 256 * 1024 * 1024),
            color_worker: ColorRenderWorker::new(context)?,
            analysis_worker: AnalysisWorker::new(context)?,
            spatial_worker: SpatialHighlightWorker::new(context)?,
            notifications: NotificationCenter::default(),
            next_generation: 1,
            live_runtime,
            next_load_attempt: 1,
            pending_ephemeral_close: None,
            raw_open_sender,
            raw_open_receiver,
            active_raw_open: None,
            pending_auto_open: VecDeque::new(),
            auto_open_sender,
            auto_open_receiver,
            active_auto_open: None,
            auto_open_documents: BTreeMap::new(),
            auto_open_background_tabs: VecDeque::new(),
            reinterpret_sender,
            reinterpret_receiver,
        })
    }
}

impl eframe::App for CameraToolboxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.poll_color_result(&context);
        self.poll_analysis_result();
        self.poll_spatial_highlight_result();
        self.poll_raw_open_result(&context);
        self.poll_auto_open_result(&context);
        self.enqueue_auto_open_candidates();
        self.dispatch_auto_open(&context);
        self.poll_reinterpret_result(&context);
        self.poll_stream_events();
        for effect in self.live_runtime.poll_platform_events() {
            self.handle_platform_effect(&context, effect);
        }
        self.advance_live_close_deadlines();
        if let Some(document) = self.workspace.active_live_mut() {
            document.install_latest_texture(&context);
        }
        self.ensure_active_resources(&context);
        if let Some(document) = self.workspace.active_asset_mut() {
            document.ensure_texture(&context);
        }
        self.ensure_analysis();
        if let Some(document) = self.workspace.active_mut() {
            document
                .viewer
                .refresh_cursor(&context, &document.loaded.frame);
        }
        self.explorer.ensure_default_local_mount(&context);

        egui::Panel::top("menu_bar").show(ui, |ui| self.render_menu_bar(ui));
        let tab_action = egui::Panel::top("document_tabs")
            .resizable(false)
            .show(ui, |ui| render_tab_bar(ui, &self.workspace))
            .inner;
        if let Some(action) = tab_action {
            self.handle_tab_action(&context, action);
        }
        let explorer_action = if self.explorer_panel_expanded {
            let mut collapse = false;
            let action = egui::Panel::left("workspace_explorer_panel")
                .resizable(true)
                .default_size(280.0)
                .min_size(220.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("Workspace");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("‹").on_hover_text("Collapse Workspace").clicked() {
                                collapse = true;
                            }
                        });
                    });
                    ui.separator();
                    self.explorer.render(&context, ui)
                })
                .inner;
            if collapse {
                self.explorer_panel_expanded = false;
            }
            action
        } else {
            let expand = egui::Panel::left("workspace_explorer_panel_rail")
                .resizable(false)
                .min_size(32.0)
                .max_size(32.0)
                .show(ui, |ui| {
                    ui.button("›").on_hover_text("Expand Workspace").clicked()
                })
                .inner;
            if expand {
                self.explorer_panel_expanded = true;
            }
            None
        };
        if let Some(action) = explorer_action {
            self.handle_explorer_action(&context, action);
        }
        egui::Panel::bottom("status_bar").show(ui, |ui| self.render_status_bar(ui));
        self.render_analysis_panel_ui(ui);
        self.render_color_panel(ui);

        let viewer_output = egui::CentralPanel::default()
            .show(ui, |ui| {
                if let Some(document) = self.workspace.active_live_mut() {
                    let rect = Self::render_live_viewer(ui, document, &self.live_runtime);
                    ViewerOutput { rect, action: None }
                } else if let Some(document) = self.workspace.active_asset_mut() {
                    let rect = Self::render_asset_viewer(ui, document);
                    ViewerOutput { rect, action: None }
                } else if let Some(document) = self.workspace.active_mut() {
                    render_viewer(
                        ui,
                        Some(&document.loaded),
                        &mut document.viewer,
                        document.display_mode,
                        document.hover_view,
                        document.spatial_highlight.as_ref(),
                    )
                } else {
                    render_viewer(
                        ui,
                        None,
                        &mut self.empty_viewer,
                        DisplayMode::RawMono,
                        HoverViewSettings::default(),
                        None,
                    )
                }
            })
            .inner;
        if let Some(action) = viewer_output.action {
            self.handle_viewer_action(action);
        }
        self.notifications.render(
            &context,
            viewer_output.rect,
            context.input(|input| input.time),
        );
        self.render_raw_open_dialog(&context);
        self.render_pending_ephemeral_close(&context);
        self.ensure_analysis();
        self.workspace.enforce_derived_budget();
        self.workspace.release_inactive_live_textures();
        if !self.workspace.live_documents().is_empty() {
            context.request_repaint_after(Duration::from_millis(33));
        } else {
            context.request_repaint_after(Duration::from_millis(100));
        }
    }
}

impl CameraToolboxApp {
    fn render_menu_bar(&mut self, ui: &mut egui::Ui) {
        let mut request_color = false;
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| self.render_file_menu(ui));
            ui.menu_button("View", |ui| {
                request_color |= self.render_view_menu(ui);
            });
            ui.menu_button("Tools", |ui| self.render_tools_menu(ui));
            ui.menu_button("Help", |ui| {
                ui.label("Camera Toolbox");
                ui.label("Local Bayer RAW color viewer");
                ui.separator();
                ui.label("Log directory");
                if let Some(directory) = camera_toolbox_logging::logging_directory() {
                    let path = directory.display().to_string();
                    ui.monospace(&path);
                    if ui.button("Copy log path").clicked() {
                        ui.ctx().copy_text(path);
                    }
                } else {
                    ui.label("Unavailable on this platform");
                }
            });
        });
        if request_color {
            self.request_current_color();
        }
    }

    fn render_file_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Open Raw...").clicked() {
            self.raw_dialog.open(ui.ctx());
            ui.close();
        }
        if ui
            .add_enabled(
                self.workspace.active_asset().is_some()
                    || self
                        .workspace
                        .active()
                        .is_some_and(|document| document.source_asset.is_some()),
                egui::Button::new("Save/Export Captured Source..."),
            )
            .clicked()
        {
            self.save_active_ephemeral_source();
            ui.close();
        }
        if ui
            .add_enabled(
                self.workspace.active_id().is_some(),
                egui::Button::new("Close Image"),
            )
            .clicked()
        {
            if let Some(id) = self.workspace.active_id() {
                self.close_document(ui.ctx(), id);
            }
            ui.close();
        }
        ui.separator();
        if ui.button("Quit").clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn render_view_menu(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(document) = self.workspace.active_mut() else {
            ui.add_enabled(false, egui::Button::new(DisplayMode::RawMono.label()));
            ui.add_enabled(false, egui::Button::new(DisplayMode::Color.label()));
            return false;
        };
        let mut request_color = false;
        if ui
            .add(egui::Button::selectable(
                document.display_mode == DisplayMode::RawMono,
                DisplayMode::RawMono.label(),
            ))
            .clicked()
        {
            document.display_mode = DisplayMode::RawMono;
            ui.close();
        }
        if ui
            .add(egui::Button::selectable(
                document.display_mode == DisplayMode::Color,
                DisplayMode::Color.label(),
            ))
            .clicked()
        {
            document.display_mode = DisplayMode::Color;
            request_color = true;
            ui.close();
        }
        ui.separator();
        Self::render_view_navigation(ui, document);
        request_color
    }

    fn render_tools_menu(&mut self, ui: &mut egui::Ui) {
        let Some(document) = self.workspace.active_mut() else {
            ui.add_enabled(false, egui::Checkbox::new(&mut false, "Hover View"));
            return;
        };
        ui.checkbox(&mut document.hover_view.enabled, "Hover View");
        if document.hover_view.enabled {
            ui.menu_button("Hover View Settings", |ui| {
                ui.label("Neighborhood");
                ui.separator();
                for neighborhood in HoverNeighborhood::ALL {
                    if ui
                        .selectable_value(
                            &mut document.hover_view.neighborhood,
                            neighborhood,
                            neighborhood.label(),
                        )
                        .clicked()
                    {
                        ui.close();
                    }
                }
            });
        }
        ui.checkbox(&mut document.analysis_panel.expanded, "Analysis Panel");
        if ui.button("Reset ROI to Full Frame").clicked() {
            let width = document.loaded.frame.spec.width;
            let height = document.loaded.frame.spec.height;
            Self::commit_roi_for_document(
                document,
                Roi {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
            );
            ui.close();
        }
        ui.weak("Right-drag image to select ROI");
    }

    fn render_view_navigation(ui: &mut egui::Ui, document: &mut crate::workspace::RawDocument) {
        if ui.button("Fit to Window").clicked() {
            document.viewer.fit_on_next_frame = true;
            ui.close();
        }
        if ui.button("Actual Size / 100%").clicked() {
            document.viewer.zoom = 1.0;
            document.viewer.fit_on_next_frame = false;
            ui.close();
        }
        if ui.button("Zoom In").clicked() {
            document.viewer.zoom_by(1.25, None, egui::Rect::NOTHING);
            ui.close();
        }
        if ui.button("Zoom Out").clicked() {
            document.viewer.zoom_by(0.8, None, egui::Rect::NOTHING);
            ui.close();
        }
        if ui.button("Reset View").clicked() {
            document.viewer = ImageViewerState::default();
            ui.close();
        }
    }

    fn render_color_panel(&mut self, ui: &mut egui::Ui) {
        let Some(expanded) = self
            .workspace
            .active()
            .map(|document| document.color_panel_expanded)
        else {
            return;
        };
        if !expanded {
            let mut expand = false;
            egui::Panel::right("color_processing_rail")
                .resizable(false)
                .min_size(32.0)
                .max_size(32.0)
                .show(ui, |ui| {
                    let response =
                        ui.add_sized(egui::vec2(28.0, 28.0), egui::Button::new("‹").frame(false));
                    expand = response.clicked();
                    response.on_hover_text("Expand Color Processing");
                });
            if expand && let Some(document) = self.workspace.active_mut() {
                document.color_panel_expanded = true;
            }
            return;
        }

        let mut should_submit = false;
        let mut collapse = false;
        let mut reinterpret_request = None;
        let workspace = &mut self.workspace;
        egui::Panel::right("color_processing")
            .resizable(true)
            .default_size(280.0)
            .min_size(240.0)
            .max_size(420.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Color Processing");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let response = ui.button("›");
                        collapse = response.clicked();
                        response.on_hover_text("Collapse Color Processing");
                    });
                });
                egui::ScrollArea::vertical()
                    .id_salt("color_processing_controls")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let document = workspace.active_mut().expect("active document exists");
                        let source_spec = document.loaded.frame.spec.clone();
                        let installed_revision = document.loaded.installed_revision();
                        let response = render_color_controls(
                            ui,
                            &mut document.loaded.color_edit,
                            &source_spec,
                            &mut document.display_mode,
                            installed_revision,
                        );
                        let inspector = render_raw_inspector(
                            ui,
                            &mut document.raw_inspector,
                            document.raw_source.is_some(),
                        );
                        if inspector.apply_requested {
                            let decode_generation = document.decode_generation.saturating_add(1);
                            let bayer = document.loaded.color_edit.params.bayer;
                            match document.raw_inspector.decode_params(bayer) {
                                Ok(params) => {
                                    let Some(source) = document.raw_source.clone() else {
                                        document.raw_inspector.mark_error(
                                            None,
                                            "当前文档没有可复用的 RAW source".to_owned(),
                                        );
                                        return;
                                    };
                                    document.decode_generation = decode_generation;
                                    document.raw_inspector.mark_submitted(decode_generation);
                                    reinterpret_request = Some((
                                        document.id,
                                        decode_generation,
                                        source,
                                        params,
                                        document.loaded.roi,
                                        document.loaded.path.clone(),
                                    ));
                                }
                                Err(error) => document.raw_inspector.mark_error(None, error),
                            }
                        }
                        should_submit = response.params_changed
                            || (response.mode_changed
                                && document.display_mode == DisplayMode::Color);
                    });
            });
        if let Some((document_id, decode_generation, source, params, roi, path)) =
            reinterpret_request
        {
            self.start_reinterpret(
                ui.ctx(),
                document_id,
                decode_generation,
                source,
                params,
                roi,
                path,
            );
        }
        if collapse && let Some(document) = self.workspace.active_mut() {
            document.color_panel_expanded = false;
        }
        if should_submit {
            self.request_current_color();
        }
    }

    fn render_analysis_panel_ui(&mut self, ui: &mut egui::Ui) {
        let Some(document) = self.workspace.active() else {
            return;
        };
        let image_hover = self.image_histogram_hover();
        let expanded = document.analysis_panel.expanded;
        let default_size = document.analysis_panel.panel_height();
        let min_size = document.analysis_panel.min_height();
        let max_size = if expanded {
            (ui.available_height() * 0.45).max(min_size)
        } else {
            min_size
        };
        let response = egui::Panel::bottom("analysis_panel")
            .resizable(expanded)
            .default_size(default_size)
            .min_size(min_size)
            .max_size(max_size)
            .show(ui, |ui| {
                let document = self.workspace.active_mut().expect("active document exists");
                render_analysis_panel(ui, &mut document.analysis_panel, image_hover)
            })
            .inner;
        if response.selection_changed
            && let Some(document) = self.workspace.active_mut()
        {
            document.analysis_pending_active = None;
        }
        self.update_spatial_highlight(
            response.hovered_bin,
            response.selection_changed || response.view_interacting,
        );
    }

    fn update_spatial_highlight(
        &mut self,
        selection: Option<HistogramBinSelection>,
        suppress: bool,
    ) {
        let Some(selection) = selection.filter(|_| !suppress) else {
            self.clear_active_spatial_highlight();
            return;
        };
        let request = {
            let Some(document) = self.workspace.active_mut() else {
                return;
            };
            if document.spatial_requested == Some(selection) {
                return;
            }
            if document.analysis_panel.current_key() != Some(selection.key)
                || document.id != selection.key.document_id
                || document.loaded.generation != selection.key.generation
            {
                document.spatial_requested = None;
                document.spatial_highlight = None;
                document.viewer.evict_derived_resources();
                return;
            }
            let display_image = match selection.key.domain {
                AnalysisDomain::RawBayer => None,
                AnalysisDomain::DisplayRgb => {
                    let Some(preview) = document.loaded.installed_color.as_ref() else {
                        document.spatial_requested = None;
                        document.spatial_highlight = None;
                        return;
                    };
                    if Some(preview.rendered_revision) != selection.key.source_revision {
                        document.spatial_requested = None;
                        document.spatial_highlight = None;
                        return;
                    }
                    Some(Arc::clone(&preview.image))
                }
            };
            document.spatial_highlight = None;
            document.viewer.evict_derived_resources();
            document.spatial_requested = Some(selection);
            SpatialHighlightRequest {
                selection,
                frame: Arc::clone(&document.loaded.frame),
                display_image,
            }
        };
        self.workspace
            .supersede_spatial_submissions_except(request.selection.key.document_id);
        self.spatial_worker.submit(request);
    }

    fn clear_active_spatial_highlight(&mut self) {
        let Some(document) = self.workspace.active_mut() else {
            return;
        };
        if document.spatial_requested.is_none() && document.spatial_highlight.is_none() {
            return;
        }
        document.spatial_requested = None;
        document.spatial_highlight = None;
        document.viewer.evict_derived_resources();
    }

    fn poll_spatial_highlight_result(&mut self) {
        let Some(result) = self.spatial_worker.take_ready() else {
            return;
        };
        self.install_spatial_highlight_result(result);
    }

    fn install_spatial_highlight_result(&mut self, result: SpatialHighlightResult) {
        let identity = DocumentIdentity {
            document_id: result.selection.key.document_id,
            generation: result.selection.key.generation,
        };
        let Some(document) = self.workspace.matching_document_mut(identity) else {
            return;
        };
        if document.spatial_requested != Some(result.selection)
            || document.analysis_panel.current_key() != Some(result.selection.key)
        {
            return;
        }
        match result.result {
            Ok(payload) => {
                document.spatial_highlight = Some(SpatialHighlight {
                    selection: result.selection,
                    mask: payload.mask,
                    overlay_image: payload.overlay_image,
                });
                document.mark_derived_loaded();
            }
            Err(error) => {
                tracing::warn!(
                    operation = "build_histogram_spatial_highlight",
                    document_id = %document.id,
                    generation = result.selection.key.generation,
                    error = %error,
                    "spatial highlight failed"
                );
                document.spatial_requested = None;
                document.spatial_highlight = None;
            }
        }
    }

    fn image_histogram_hover(&self) -> Option<ImageHistogramHover> {
        let document = self.workspace.active()?;
        let key = document.analysis_panel.current_key()?;
        let cursor = document.viewer.cursor?;
        if document.id != key.document_id
            || document.loaded.generation != key.generation
            || !key.roi.contains(cursor.x, cursor.y)
        {
            return None;
        }
        let row_width = usize::try_from(document.loaded.frame.spec.width).ok()?;
        let x = usize::try_from(cursor.x).ok()?;
        let y = usize::try_from(cursor.y).ok()?;
        let index = y.checked_mul(row_width)?.checked_add(x)?;
        let sample = match key.domain {
            AnalysisDomain::RawBayer => HistogramPixelSample::Raw {
                site: document.loaded.frame.spec.bayer.site_at(cursor.x, cursor.y),
                value: *document.loaded.frame.pixels().get(index)?,
            },
            AnalysisDomain::DisplayRgb => {
                let preview = document.loaded.installed_color.as_ref()?;
                if Some(preview.rendered_revision) != key.source_revision {
                    return None;
                }
                HistogramPixelSample::Display(display_histogram_sample(
                    *preview.image.pixels.get(index)?,
                ))
            }
        };
        Some(ImageHistogramHover {
            key,
            x: cursor.x,
            y: cursor.y,
            sample,
        })
    }

    fn handle_viewer_action(&mut self, action: ViewerAction) {
        let Some(document) = self.workspace.active_mut() else {
            return;
        };
        match action {
            ViewerAction::CommitRoi(roi) => Self::commit_roi_for_document(document, roi),
            ViewerAction::ResetRoi => {
                let spec = &document.loaded.frame.spec;
                Self::commit_roi_for_document(
                    document,
                    Roi {
                        x: 0,
                        y: 0,
                        width: spec.width,
                        height: spec.height,
                    },
                );
            }
        }
    }

    fn commit_roi_for_document(document: &mut crate::workspace::RawDocument, roi: Roi) {
        let Some(roi) = roi.clamped_to(
            document.loaded.frame.spec.width,
            document.loaded.frame.spec.height,
        ) else {
            return;
        };
        if document.loaded.roi == roi {
            return;
        }
        tracing::debug!(
            operation = "commit_roi",
            document_id = %document.id,
            generation = document.loaded.generation,
            roi_x = roi.x,
            roi_y = roi.y,
            roi_width = roi.width,
            roi_height = roi.height,
            "committed viewer ROI"
        );
        document.loaded.roi = roi;
        document.loaded.stats = None;
        document.analysis_pending_active = None;
    }

    fn ensure_analysis(&mut self) {
        let request = {
            let Some(document) = self.workspace.active_mut() else {
                return;
            };
            let loaded = &document.loaded;
            let chart_roi = document.analysis_panel.scope.resolve(
                loaded.roi,
                loaded.frame.spec.width,
                loaded.frame.spec.height,
            );
            let (source_revision, display_image) = match document.analysis_panel.domain {
                AnalysisDomain::RawBayer => (None, None),
                AnalysisDomain::DisplayRgb => {
                    let Some(preview) = &loaded.installed_color else {
                        document.analysis_panel.wait_for_source();
                        return;
                    };
                    (
                        Some(preview.rendered_revision),
                        Some(Arc::clone(&preview.image)),
                    )
                }
            };
            let key = AnalysisKey {
                document_id: document.id,
                generation: loaded.generation,
                source_revision,
                roi: chart_roi,
                domain: document.analysis_panel.domain,
            };
            let desired = document.analysis_panel.set_desired(key);
            let chart_ready = document.analysis_panel.has_current(key);
            let stats_ready = loaded.stats.is_some();
            let stats_pending =
                document.analysis_pending_active == Some((loaded.generation, loaded.roi));
            if desired != DesiredAnalysis::Submit && (stats_ready || stats_pending) {
                return;
            }
            let compute_chart = !chart_ready;
            document.analysis_pending_active = Some((loaded.generation, loaded.roi));
            AnalysisRequest {
                key,
                active_roi: loaded.roi,
                compute_chart,
                frame: Arc::clone(&loaded.frame),
                display_image,
            }
        };
        self.workspace
            .supersede_analysis_submissions_except(request.key.document_id);
        self.analysis_worker.submit(request);
    }

    fn poll_analysis_result(&mut self) {
        let Some(result) = self.analysis_worker.take_ready() else {
            return;
        };
        self.install_analysis_result(result);
    }

    fn install_analysis_result(&mut self, result: AnalysisResult) {
        let key = result.key;
        let identity = DocumentIdentity {
            document_id: key.document_id,
            generation: key.generation,
        };
        let Some(document) = self.workspace.matching_document_mut(identity) else {
            tracing::debug!(
                operation = "poll_histogram_analysis",
                document_id = %key.document_id,
                generation = key.generation,
                "dropped analysis result for closed or replaced document"
            );
            return;
        };
        let accepted = document.analysis_panel.accept_result(result);
        let Some((active_roi, stats)) = accepted else {
            tracing::debug!(
                operation = "poll_histogram_analysis",
                document_id = %key.document_id,
                generation = key.generation,
                revision = ?key.source_revision,
                domain = key.domain.label(),
                "dropped stale or failed histogram analysis"
            );
            return;
        };
        if document.loaded.roi == active_roi {
            document.loaded.stats = Some(stats);
            document.analysis_pending_active = None;
            document.mark_derived_loaded();
            tracing::debug!(
                operation = "install_histogram_analysis",
                document_id = %key.document_id,
                generation = key.generation,
                revision = ?key.source_revision,
                domain = key.domain.label(),
                "installed histogram analysis"
            );
        }
    }

    fn render_status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(document) = self.workspace.active_live() {
                let media = document.media.as_ref().map_or_else(
                    || "Negotiating".to_owned(),
                    |media| {
                        format!(
                            "{:?} {}×{} PT {} SSRC {:08x} {} fps",
                            media.codec,
                            media.width,
                            media.height,
                            media.payload_type,
                            media.ssrc,
                            media.frame_rate
                        )
                    },
                );
                ui.label(&document.title);
                ui.separator();
                ui.label(media);
                ui.separator();
                ui.label(format!(
                    "RTP gaps {} · preview dropped {} · resync {}",
                    document.metrics.rtp_gaps,
                    document.metrics.preview_dropped,
                    document.metrics.decoder_resyncs
                ));
                return;
            }
            let Some(document) = self.workspace.active() else {
                ui.label("Ready");
                return;
            };
            let loaded = &document.loaded;
            let displayed_bayer = loaded.displayed_bayer(document.display_mode);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!("{:.0}%", document.viewer.zoom * 100.0));
                ui.separator();
                ui.label(document.display_mode.label());
                Self::render_diagnostic_badges(ui, document);
                ui.separator();
                ui.label(format!("ROI: {}×{}", loaded.roi.width, loaded.roi.height));
                ui.separator();
                ui.allocate_ui_with_layout(
                    ui.available_size(),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add(egui::Label::new(&document.title).truncate());
                        ui.separator();
                        ui.label(format!(
                            "{}×{} · {}-bit · {}",
                            loaded.frame.spec.width,
                            loaded.frame.spec.height,
                            loaded.frame.spec.bit_depth,
                            bayer_label(displayed_bayer).to_uppercase()
                        ));
                    },
                );
            });
        });
    }

    fn render_diagnostic_badges(ui: &mut egui::Ui, document: &crate::workspace::RawDocument) {
        let loaded = &document.loaded;
        if loaded.diagnostics.has_out_of_range() {
            ui.separator();
            ui.colored_label(
                egui::Color32::YELLOW,
                format!("RAW range: {}", loaded.diagnostics.out_of_range_pixels),
            );
        }
        if document.display_mode != DisplayMode::Color {
            return;
        }
        let Some(preview) = &loaded.installed_color else {
            return;
        };
        if preview.diagnostics.display_clipped_channels > 0 {
            ui.separator();
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "RGB clipped: {}",
                    preview.diagnostics.display_clipped_channels
                ),
            );
        }
        if preview.diagnostics.missing_neighbor_channels > 0 {
            ui.separator();
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "Demosaic edge: {}",
                    preview.diagnostics.missing_neighbor_channels
                ),
            );
        }
    }

    fn handle_explorer_action(&mut self, context: &egui::Context, action: ExplorerAction) {
        match action {
            ExplorerAction::AddLocalMount(root) => {
                let config = match self.explorer.add_local_mount(root.clone(), context) {
                    Ok(config) => config,
                    Err(error) => {
                        tracing::error!(
                            operation = "explorer_add_local_mount",
                            root = %root.display(),
                            error = %error,
                            "failed to mount local directory"
                        );
                        self.explorer
                            .set_global_error(format!("Mount folder failed: {error}"));
                        return;
                    }
                };
                if self
                    .workspace_settings
                    .mounts()
                    .any(|existing| existing.source_id() == config.source_id())
                {
                    self.reload_auto_open();
                    return;
                }
                let mut updated = self.workspace_settings.clone();
                if let Err(error) = updated
                    .insert_mount(config)
                    .and_then(|_| save_workspace_settings(&updated))
                {
                    tracing::error!(
                        operation = "explorer_persist_local_mount",
                        root = %root.display(),
                        error = %error,
                        "failed to persist workspace mount"
                    );
                    self.explorer
                        .set_global_error(format!("Persist mount failed: {error}"));
                    return;
                }
                self.workspace_settings = updated;
                self.reload_auto_open();
            }
            #[cfg(feature = "platform-ssh")]
            ExplorerAction::SaveRemoteConnection(RemoteConnectionCommit {
                config,
                session_password,
            }) => {
                if let camera_toolbox_app::RemoteAuthentication::Password { slot_id } =
                    &config.authentication
                    && let Some(password) = session_password
                    && let Err(error) = self
                        .live_runtime
                        .ssh_credential_resolver()
                        .register_session_password(slot_id, password)
                {
                    tracing::error!(
                        operation = "explorer_register_remote_password",
                        connection_id = config.id.as_str(),
                        slot_id,
                        error = %error,
                        "failed to register remote session password"
                    );
                    self.explorer.set_remote_connection_error(format!(
                        "Register remote password failed: {error}"
                    ));
                    return;
                }
                let mut updated = self.connection_settings.clone();
                if let Err(error) = updated
                    .upsert(config.clone())
                    .and_then(|_| save_connection_settings(&updated))
                {
                    tracing::error!(
                        operation = "explorer_save_remote_connection",
                        connection_id = config.id.as_str(),
                        host = %config.host,
                        port = config.port,
                        error = %error,
                        "failed to persist remote connection"
                    );
                    self.explorer.set_remote_connection_error(format!(
                        "Save remote connection failed: {error}"
                    ));
                    return;
                }
                self.connection_settings = updated;
                if let Err(error) = self.explorer.finish_remote_connection_save(config, context) {
                    tracing::error!(
                        operation = "explorer_refresh_remote_connection",
                        error = %error,
                        "saved remote connection but Explorer refresh failed"
                    );
                    self.explorer.set_global_error(format!(
                        "Remote connection saved, but Explorer refresh failed: {error}"
                    ));
                    return;
                }
                self.reload_auto_open();
            }
            #[cfg(feature = "platform-ssh")]
            ExplorerAction::AddSftpMount {
                connection_id,
                remote_root,
            } => {
                let config = match self.explorer.add_sftp_mount(
                    connection_id.clone(),
                    remote_root.clone(),
                    context,
                ) {
                    Ok(config) => config,
                    Err(error) => {
                        tracing::error!(
                            operation = "explorer_add_sftp_mount",
                            connection_id = connection_id.as_str(),
                            remote_root,
                            error = %error,
                            "failed to mount remote directory"
                        );
                        self.explorer
                            .set_global_error(format!("Mount remote failed: {error}"));
                        return;
                    }
                };
                if self
                    .workspace_settings
                    .mounts()
                    .any(|existing| existing.source_id() == config.source_id())
                {
                    self.reload_auto_open();
                    return;
                }
                let mut updated = self.workspace_settings.clone();
                if let Err(error) = updated
                    .insert_mount(config)
                    .and_then(|_| save_workspace_settings(&updated))
                {
                    tracing::error!(
                        operation = "explorer_persist_sftp_mount",
                        connection_id = connection_id.as_str(),
                        remote_root,
                        error = %error,
                        "failed to persist remote mount"
                    );
                    self.explorer
                        .set_global_error(format!("Persist remote mount failed: {error}"));
                    return;
                }
                self.workspace_settings = updated;
                self.reload_auto_open();
            }
            ExplorerAction::OpenAuto {
                display_path,
                file_system,
                reference,
            } => {
                let attempt = self.begin_raw_open_attempt();
                self.start_load_workspace_file(
                    context,
                    attempt,
                    WorkspaceFileOpenRequest {
                        display_path,
                        file_system,
                        reference,
                    },
                );
            }
        }
    }

    fn reload_auto_open(&mut self) {
        self.auto_open =
            AutoOpenCoordinator::from_settings(&self.workspace_settings, &self.explorer);
    }

    fn cancel_active_auto_open(&mut self) {
        if let Some(active) = self.active_auto_open.take() {
            active.cancellation.cancel();
        }
    }

    fn enqueue_auto_open_candidates(&mut self) {
        for candidate in self.auto_open.poll() {
            let Some(ExplorerAction::OpenAuto {
                display_path,
                file_system,
                reference,
            }) = self.explorer.open_action_for(&candidate.reference)
            else {
                tracing::warn!(
                    operation = "auto_open_enqueue",
                    rule_id = candidate.rule_id.as_str(),
                    source_id = %candidate.reference.source_id,
                    path = %candidate.reference.path.as_str(),
                    "auto-open source is no longer mounted; dropping candidate"
                );
                continue;
            };
            if self.pending_auto_open.len() >= AUTO_OPEN_QUEUE_LIMIT {
                self.pending_auto_open.pop_front();
            }
            self.pending_auto_open.push_back(PendingAutoOpenRequest {
                candidate,
                request: WorkspaceFileOpenRequest {
                    display_path,
                    file_system,
                    reference,
                },
            });
        }
    }

    fn dispatch_auto_open(&mut self, context: &egui::Context) {
        if self.active_raw_open.is_some() || self.active_auto_open.is_some() {
            return;
        }
        let Some(pending) = self.pending_auto_open.pop_front() else {
            return;
        };
        let cancellation = FsCancellation::default();
        self.active_auto_open = Some(ActiveAutoOpenJob {
            candidate: pending.candidate.clone(),
            cancellation: cancellation.clone(),
        });
        let pipeline = self.raw_pipeline.clone();
        let sender = self.auto_open_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let path = pending.request.display_path.clone();
            let result = decode_workspace_file_request(&pipeline, pending.request, cancellation)
                .map_err(|error| error.to_string());
            let _ = sender.send(AutoOpenJobResult {
                candidate: pending.candidate,
                path,
                result,
            });
            context.request_repaint();
        });
    }

    fn begin_raw_open_attempt(&mut self) -> u64 {
        let attempt = self.next_load_attempt;
        self.next_load_attempt = self.next_load_attempt.saturating_add(1);
        if attempt > 1 {
            self.notifications
                .clear_scope(NotificationScope::LoadAttempt(attempt - 1));
        }
        attempt
    }

    fn render_raw_open_dialog(&mut self, context: &egui::Context) {
        let Some(request) = self.raw_dialog.show(context, &self.raw_pipeline) else {
            return;
        };
        let attempt = self.begin_raw_open_attempt();
        self.start_load_raw(context, attempt, request);
        self.raw_dialog.close(context);
    }

    fn start_load_raw(
        &mut self,
        context: &egui::Context,
        attempt: u64,
        request: LocalRawAnalyzeRequest,
    ) {
        if let Some(active) = self.active_raw_open.take() {
            active.cancellation.cancel();
        }
        self.cancel_active_auto_open();
        let cancellation = FsCancellation::default();
        self.active_raw_open = Some(ActiveRawOpenJob {
            attempt,
            cancellation: cancellation.clone(),
        });
        let pipeline = self.raw_pipeline.clone();
        let sender = self.raw_open_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let path = request.path.clone();
            let result = decode_raw_request(&pipeline, request, cancellation)
                .map_err(|error| error.to_string());
            let _ = sender.send(RawOpenJobResult {
                attempt,
                path,
                result,
            });
            context.request_repaint();
        });
    }

    fn start_load_workspace_file(
        &mut self,
        context: &egui::Context,
        attempt: u64,
        request: WorkspaceFileOpenRequest,
    ) {
        if let Some(active) = self.active_raw_open.take() {
            active.cancellation.cancel();
        }
        let cancellation = FsCancellation::default();
        self.cancel_active_auto_open();
        self.active_raw_open = Some(ActiveRawOpenJob {
            attempt,
            cancellation: cancellation.clone(),
        });
        let pipeline = self.raw_pipeline.clone();
        let sender = self.raw_open_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let path = request.display_path.clone();
            let result = decode_workspace_file_request(&pipeline, request, cancellation)
                .map_err(|error| error.to_string());
            let _ = sender.send(RawOpenJobResult {
                attempt,
                path,
                result,
            });
            context.request_repaint();
        });
    }

    fn poll_raw_open_result(&mut self, context: &egui::Context) {
        loop {
            let result = match self.raw_open_receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            if !self
                .active_raw_open
                .as_ref()
                .is_some_and(|active| active.attempt == result.attempt)
            {
                continue;
            }
            self.active_raw_open = None;
            match result.result {
                Ok(completed) => self.install_opened_raw(context, result.attempt, completed),
                Err(message) => {
                    tracing::error!(
                        operation = "load_raw",
                        attempt = result.attempt,
                        path = %result.path.display(),
                        error = %message,
                        "RAW loading failed"
                    );
                    self.notifications.push_once(UiNotification::error(
                        NotificationKey::RawLoadFailed {
                            attempt: result.attempt,
                        },
                        "RAW load failed",
                        &message,
                    ));
                    self.raw_dialog.set_error(message);
                }
            }
        }
    }

    fn poll_auto_open_result(&mut self, context: &egui::Context) {
        loop {
            let result = match self.auto_open_receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            let Some(active) = self.active_auto_open.as_ref() else {
                continue;
            };
            if active.candidate.rule_id != result.candidate.rule_id
                || active.candidate.reference != result.candidate.reference
            {
                continue;
            }
            self.active_auto_open = None;
            match result.result {
                Ok(completed) => self.install_auto_opened_raw(context, result.candidate, completed),
                Err(message) => {
                    tracing::error!(
                        operation = "auto_open_raw",
                        rule_id = result.candidate.rule_id.as_str(),
                        path = %result.path.display(),
                        error = %message,
                        "auto-open RAW loading failed"
                    );
                }
            }
        }
    }

    fn install_opened_raw(
        &mut self,
        context: &egui::Context,
        attempt: u64,
        completed: OpenedRawDocument,
    ) {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let loaded = LoadedRaw::from_report(context, completed.report, generation);
        let notification = loaded.diagnostics.first_out_of_range().map(|first| {
            UiNotification::raw_range(
                generation,
                loaded.frame.spec.bit_depth,
                loaded.frame.spec.max_code_value(),
                loaded.diagnostics.out_of_range_pixels,
                loaded.diagnostics.observed_max,
                (first.x, first.y, first.value),
                context.input(|input| input.time),
            )
        });
        tracing::debug!(
            operation = "load_raw",
            attempt,
            generation,
            path = %loaded.path.display(),
            width = loaded.frame.spec.width,
            height = loaded.frame.spec.height,
            bit_depth = loaded.frame.spec.bit_depth,
            "RAW loaded"
        );
        if loaded.diagnostics.has_out_of_range() {
            tracing::warn!(
                attempt,
                operation = "load_raw",
                generation,
                path = %loaded.path.display(),
                bit_depth = loaded.frame.spec.bit_depth,
                out_of_range_pixels = loaded.diagnostics.out_of_range_pixels,
                observed_max = loaded.diagnostics.observed_max,
                "RAW samples exceed declared bit-depth range"
            );
        }
        let document_id = self.workspace.open_file_raw(
            loaded,
            completed.source,
            completed.interpretation,
            generation,
            true,
        );
        if let Some(notification) = notification {
            self.notifications.push_once(notification);
        }
        tracing::debug!(
            operation = "open_document",
            document_id = %document_id,
            generation,
            "opened local RAW in new tab"
        );
        self.request_current_color();
        self.workspace.enforce_derived_budget();
    }

    fn install_auto_opened_raw(
        &mut self,
        context: &egui::Context,
        candidate: AutoOpenCandidate,
        completed: OpenedRawDocument,
    ) {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let loaded = LoadedRaw::from_report(context, completed.report, generation);
        let notification = loaded.diagnostics.first_out_of_range().map(|first| {
            UiNotification::raw_range(
                generation,
                loaded.frame.spec.bit_depth,
                loaded.frame.spec.max_code_value(),
                loaded.diagnostics.out_of_range_pixels,
                loaded.diagnostics.observed_max,
                (first.x, first.y, first.value),
                context.input(|input| input.time),
            )
        });
        tracing::debug!(
            operation = "auto_open_raw",
            rule_id = candidate.rule_id.as_str(),
            generation,
            path = %loaded.path.display(),
            width = loaded.frame.spec.width,
            height = loaded.frame.spec.height,
            bit_depth = loaded.frame.spec.bit_depth,
            "auto-open RAW loaded"
        );
        if loaded.diagnostics.has_out_of_range() {
            tracing::warn!(
                operation = "auto_open_raw",
                rule_id = candidate.rule_id.as_str(),
                generation,
                path = %loaded.path.display(),
                bit_depth = loaded.frame.spec.bit_depth,
                out_of_range_pixels = loaded.diagnostics.out_of_range_pixels,
                observed_max = loaded.diagnostics.observed_max,
                "auto-open RAW samples exceed declared bit-depth range"
            );
        }
        if candidate.activation == AutoOpenActivation::FollowLatest {
            let existing_id = self
                .auto_open_documents
                .get(candidate.rule_id.as_str())
                .copied();
            if let Some(document_id) = existing_id {
                let previous_generation = self
                    .workspace
                    .document(document_id)
                    .map(|document| document.loaded.generation);
                if let Some(previous_generation) = previous_generation {
                    let was_active = self.workspace.active_id() == Some(document_id);
                    let replaced = self.workspace.replace_file_raw(
                        document_id,
                        loaded,
                        completed.source,
                        completed.interpretation,
                        generation,
                    );
                    debug_assert!(
                        replaced,
                        "existing follow-latest document must still be replaceable"
                    );
                    self.notifications
                        .clear_scope(NotificationScope::ImageGeneration(previous_generation));
                    if let Some(notification) = notification {
                        self.notifications.push_once(notification);
                    }
                    if was_active {
                        self.request_current_color();
                    }
                    self.workspace.enforce_derived_budget();
                    return;
                }
                self.auto_open_documents.remove(candidate.rule_id.as_str());
            }
        }
        let document_id = self.workspace.open_file_raw(
            loaded,
            completed.source,
            completed.interpretation,
            generation,
            false,
        );
        match candidate.activation {
            AutoOpenActivation::FollowLatest => {
                self.auto_open_documents
                    .insert(candidate.rule_id.as_str().to_owned(), document_id);
            }
            AutoOpenActivation::NewBackgroundTab => {
                self.track_auto_open_background_tab(context, document_id);
            }
        }
        if let Some(notification) = notification {
            self.notifications.push_once(notification);
        }
        if self.workspace.active_id() == Some(document_id) {
            self.request_current_color();
        }
        self.workspace.enforce_derived_budget();
    }

    fn start_reinterpret(
        &mut self,
        context: &egui::Context,
        document_id: DocumentId,
        decode_generation: u64,
        source: RawSourceHandle,
        params: RawDecodeParams,
        roi: Roi,
        path: PathBuf,
    ) {
        let pipeline = self.raw_pipeline.clone();
        let sender = self.reinterpret_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let result =
                decode_raw_reinterpret(&pipeline, source, params, decode_generation, roi, path)
                    .map_err(|error| error.to_string());
            let _ = sender.send(ReinterpretJobResult {
                document_id,
                decode_generation,
                result,
            });
            context.request_repaint();
        });
    }

    fn poll_reinterpret_result(&mut self, context: &egui::Context) {
        loop {
            let result = match self.reinterpret_receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            match result.result {
                Ok(completed) => self.install_reinterpreted_raw(
                    context,
                    result.document_id,
                    result.decode_generation,
                    completed,
                ),
                Err(message) => {
                    if let Some(document) = self.workspace.document_mut(result.document_id) {
                        document
                            .raw_inspector
                            .mark_error(Some(result.decode_generation), message);
                    }
                }
            }
        }
    }

    fn install_reinterpreted_raw(
        &mut self,
        context: &egui::Context,
        document_id: DocumentId,
        decode_generation: u64,
        completed: OpenedRawDocument,
    ) {
        let Some(previous_generation) = self.workspace.document(document_id).and_then(|document| {
            (document.decode_generation == decode_generation).then_some(document.loaded.generation)
        }) else {
            return;
        };
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let loaded = LoadedRaw::from_report(context, completed.report, generation);
        let notification = loaded.diagnostics.first_out_of_range().map(|first| {
            UiNotification::raw_range(
                generation,
                loaded.frame.spec.bit_depth,
                loaded.frame.spec.max_code_value(),
                loaded.diagnostics.out_of_range_pixels,
                loaded.diagnostics.observed_max,
                (first.x, first.y, first.value),
                context.input(|input| input.time),
            )
        });
        tracing::debug!(
            operation = "reinterpret_raw",
            document_id = %document_id,
            generation,
            decode_generation,
            path = %loaded.path.display(),
            width = loaded.frame.spec.width,
            height = loaded.frame.spec.height,
            bit_depth = loaded.frame.spec.bit_depth,
            "installed reinterpreted RAW"
        );
        if loaded.diagnostics.has_out_of_range() {
            tracing::warn!(
                operation = "reinterpret_raw",
                document_id = %document_id,
                generation,
                decode_generation,
                path = %loaded.path.display(),
                bit_depth = loaded.frame.spec.bit_depth,
                out_of_range_pixels = loaded.diagnostics.out_of_range_pixels,
                observed_max = loaded.diagnostics.observed_max,
                "RAW samples exceed declared bit-depth range"
            );
        }
        let Some(document) = self.workspace.document_mut(document_id) else {
            return;
        };
        if document.decode_generation != decode_generation {
            return;
        }
        document.install_reinterpreted(
            loaded,
            completed.source,
            completed.interpretation,
            decode_generation,
        );
        self.notifications
            .clear_scope(NotificationScope::ImageGeneration(previous_generation));
        if let Some(notification) = notification {
            self.notifications.push_once(notification);
        }
        if self.workspace.active_id() == Some(document_id) {
            self.request_current_color();
        }
        self.workspace.enforce_derived_budget();
    }

    fn request_current_color(&mut self) {
        let request = {
            let Some(document) = self.workspace.active_mut() else {
                return;
            };
            let loaded = &mut document.loaded;
            let revision = loaded.color_edit.revision;
            if loaded.color_edit.submitted_revision == Some(revision) {
                return;
            }
            if let Err(error) = loaded
                .color_edit
                .params
                .validate(loaded.frame.spec.max_code_value())
            {
                tracing::warn!(
                    operation = "validate_color_params",
                    document_id = %document.id,
                    generation = loaded.generation,
                    revision,
                    error = %error,
                    "color parameter validation rejected"
                );
                loaded.color_edit.mark_error(error.to_string());
                return;
            }
            let request = ColorRenderRequest {
                document_id: document.id,
                frame_generation: loaded.generation,
                revision,
                frame: Arc::clone(&loaded.frame),
                params: loaded.color_edit.params,
            };
            loaded.color_edit.mark_submitted();
            request
        };
        tracing::debug!(
            operation = "submit_color_render",
            document_id = %request.document_id,
            generation = request.frame_generation,
            revision = request.revision,
            "submitted color render"
        );
        self.workspace
            .supersede_color_submissions_except(request.document_id);
        self.color_worker.submit(request);
    }

    fn poll_color_result(&mut self, context: &egui::Context) {
        let Some(result) = self.color_worker.take_ready() else {
            return;
        };
        self.install_color_result(context, result);
    }

    fn install_color_result(&mut self, context: &egui::Context, result: ColorRenderResult) {
        let identity = DocumentIdentity {
            document_id: result.document_id,
            generation: result.frame_generation,
        };
        let Some(document) = self.workspace.matching_document_mut(identity) else {
            tracing::debug!(
                operation = "poll_color_result",
                document_id = %result.document_id,
                generation = result.frame_generation,
                revision = result.revision,
                "dropped color result for closed or replaced document"
            );
            return;
        };
        if document.loaded.color_edit.revision != result.revision {
            tracing::debug!(
                operation = "poll_color_result",
                document_id = %result.document_id,
                generation = result.frame_generation,
                revision = result.revision,
                "dropped stale color result"
            );
            return;
        }
        match result.rendered {
            Ok(rendered) => {
                document.loaded.install_color(
                    context,
                    result.revision,
                    result.params,
                    rendered.image,
                    rendered.diagnostics,
                );
                document.loaded.color_edit.render_error = None;
                document.mark_derived_loaded();
                tracing::debug!(
                    operation = "install_color_render",
                    document_id = %document.id,
                    generation = document.loaded.generation,
                    revision = result.revision,
                    "installed color render"
                );
            }
            Err(error) => {
                document.loaded.color_edit.mark_error(error.clone());
                tracing::error!(
                    operation = "render_color",
                    document_id = %document.id,
                    generation = document.loaded.generation,
                    revision = result.revision,
                    error = %error,
                    "accepted color render failed"
                );

                self.notifications.push_once(UiNotification::error(
                    NotificationKey::ColorRenderFailed {
                        generation: document.loaded.generation,
                        revision: result.revision,
                    },
                    "Color preview failed",
                    error,
                ));
            }
        }
    }

    fn ensure_active_resources(&mut self, context: &egui::Context) {
        let should_request_color = if let Some(document) = self.workspace.active_mut() {
            document.loaded.ensure_raw_texture(context);
            document.display_mode == DisplayMode::Color
                && document.loaded.installed_revision() != Some(document.loaded.color_edit.revision)
                && document.loaded.color_edit.submitted_revision
                    != Some(document.loaded.color_edit.revision)
        } else {
            false
        };
        if should_request_color {
            self.request_current_color();
        }
    }

    fn handle_platform_ui_action(&mut self, context: &egui::Context, action: PlatformUiAction) {
        match action {
            PlatformUiAction::OpenRaw => self.raw_dialog.open(context),
            PlatformUiAction::Stream(action) => self.handle_stream_panel_action(action),
        }
    }

    fn handle_platform_effect(&mut self, context: &egui::Context, effect: PlatformEffect) {
        let PlatformEffect::OpenAsset {
            asset,
            snapshot,
            foreground,
            spec,
        } = effect;
        let result = match asset.metadata.format {
            MediaFormat::RawPacked { bit_depth } => self
                .open_packed_raw_asset(context, asset, snapshot, spec.bayer, bit_depth, foreground),
            MediaFormat::Jpeg | MediaFormat::Yuv420Sp { .. } => self
                .workspace
                .open_asset(asset, snapshot, foreground)
                .map(|_| ()),
            ref format => Err(format!(
                "captured media format {format:?} cannot be opened as an image"
            )),
        };
        if let Err(error) = result {
            self.live_runtime.panel.last_error = Some(error);
        }
    }

    fn open_packed_raw_asset(
        &mut self,
        context: &egui::Context,
        asset: Arc<camera_toolbox_core::EphemeralAsset>,
        snapshot: Arc<camera_toolbox_app::TargetResolutionSnapshot>,
        bayer: camera_toolbox_core::BayerPattern,
        bit_depth: u8,
        foreground: bool,
    ) -> Result<(), String> {
        let attribute = |name: &str| -> Result<usize, String> {
            asset
                .metadata
                .attributes
                .get(name)
                .ok_or_else(|| format!("captured RAW metadata is missing {name}"))?
                .parse::<usize>()
                .map_err(|error| format!("captured RAW metadata {name} is invalid: {error}"))
        };
        let width = u32::try_from(attribute("width")?)
            .map_err(|_| "captured RAW width does not fit u32".to_owned())?;
        let height = u32::try_from(attribute("height")?)
            .map_err(|_| "captured RAW height does not fit u32".to_owned())?;
        let stride = attribute("stride")?;
        let bytes = match &asset.source {
            OwnedMediaPayload::Bytes(bytes) => bytes.as_ref(),
            OwnedMediaPayload::Planes(_) => {
                return Err("packed RAW source must be one contiguous payload".to_owned());
            }
        };
        let frame = decode_le_continuous_raw(
            PackedRawSpec {
                width,
                height,
                stride,
                bit_depth,
            },
            bayer,
            bytes,
        )
        .map_err(|error| error.to_string())?;
        let roi = Roi {
            x: 0,
            y: 0,
            width,
            height,
        };
        let stats = analyze_roi(&frame, roi).map_err(|error| error.to_string())?;
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let loaded = LoadedRaw::from_report(
            context,
            LocalRawAnalyzeReport {
                path: std::path::PathBuf::from(&asset.metadata.source_name),
                frame,
                roi,
                stats,
            },
            generation,
        );
        self.workspace
            .open_captured_raw(loaded, asset, snapshot, foreground);
        Ok(())
    }
    fn save_active_ephemeral_source(&mut self) -> bool {
        let asset = self
            .workspace
            .active_asset()
            .map(|document| Arc::clone(&document.asset))
            .or_else(|| {
                self.workspace
                    .active()
                    .and_then(|document| document.source_asset.as_ref().map(Arc::clone))
            });
        let Some(asset) = asset else { return false };
        let extension = asset_extension(&asset.metadata.format);
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Captured source", &[extension])
            .set_file_name(format!("{}.{}", asset.metadata.source_name, extension))
            .save_file()
        else {
            return false;
        };
        match save_asset_source(&path, &asset) {
            Ok(()) => {
                if let Some(document) = self.workspace.active_asset_mut() {
                    document.saved = true;
                }
                if let Some(document) = self.workspace.active_mut() {
                    document.unsaved = false;
                }
                true
            }
            Err(error) => {
                self.live_runtime.panel.last_error = Some(error);
                false
            }
        }
    }

    fn render_pending_ephemeral_close(&mut self, context: &egui::Context) {
        let Some(id) = self.pending_ephemeral_close else {
            return;
        };
        let mut cancel = false;
        let mut save = false;
        let mut discard = false;
        egui::Window::new("Unsaved captured source")
            .collapsible(false)
            .resizable(false)
            .show(context, |ui| {
                ui.label("Save writes the source from memory to the chosen destination.");
                ui.horizontal(|ui| {
                    cancel = ui.button("Cancel").clicked();
                    save = ui.button("Save...").clicked();
                    discard = ui.button("Discard tab").clicked();
                });
            });
        if cancel {
            self.pending_ephemeral_close = None;
        } else if save {
            self.workspace.activate(id);
            if self.save_active_ephemeral_source() {
                self.pending_ephemeral_close = None;
                self.close_document(context, id);
            }
        } else if discard {
            if let Some(document) = self.workspace.asset_mut(id) {
                document.saved = true;
            }
            if let Some(document) = self.workspace.document_mut(id) {
                document.unsaved = false;
            }
            self.pending_ephemeral_close = None;
            self.close_document(context, id);
        }
    }

    fn render_asset_viewer(
        ui: &mut egui::Ui,
        document: &mut crate::workspace::AssetDocument,
    ) -> egui::Rect {
        let rect = ui.max_rect();
        ui.heading(&document.title);
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Format: {:?}", document.asset.metadata.format));
            ui.label(format!(
                "Source bytes: {}",
                document.asset.byte_len().unwrap_or(0)
            ));
            ui.label(format!("Integrity: {:?}", document.asset.integrity));
        });
        ui.collapsing("Source metadata", |ui| {
            for (key, value) in &document.asset.metadata.attributes {
                ui.label(format!("{key}: {value}"));
            }
            ui.monospace(format!("target={}", document.resolution.aggregate_hash));
        });
        ui.separator();
        if let Some(texture) = document.texture() {
            let available = ui.available_size();
            let source = texture.size_vec2();
            let scale = (available.x / source.x)
                .min(available.y / source.y)
                .min(1.0)
                .max(0.01);
            ui.centered_and_justified(|ui| {
                ui.add(egui::Image::new(texture).fit_to_exact_size(source * scale));
            });
        } else {
            ui.centered_and_justified(|ui| ui.spinner());
        }
        rect
    }

    fn handle_stream_panel_action(&mut self, action: StreamPanelAction) {
        match action {
            StreamPanelAction::Start => match self.live_runtime.start() {
                Ok((session_id, latest_frame)) => {
                    self.workspace.open_live(session_id, latest_frame);
                    self.live_runtime.panel.last_error = None;
                }
                Err(error) => self.live_runtime.panel.last_error = Some(error),
            },
            StreamPanelAction::RequestStop => {
                let id = self
                    .workspace
                    .active_live()
                    .map(|document| document.id)
                    .or_else(|| {
                        self.workspace
                            .live_documents()
                            .first()
                            .map(|document| document.id)
                    });
                if let Some(id) = id
                    && let Some(document) = self.workspace.live_mut(id)
                    && matches!(document.lifecycle, LiveDocumentLifecycle::Open)
                {
                    document.lifecycle = LiveDocumentLifecycle::CloseRequested;
                    self.workspace.activate(id);
                }
            }
        }
    }

    fn poll_stream_events(&mut self) {
        loop {
            let event = match self.live_runtime.try_recv() {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(error) => {
                    self.live_runtime.panel.last_error = Some(error);
                    break;
                }
            };
            if let Some(document) = self.workspace.live_by_session_mut(&event.session_id) {
                document.apply_event(event.event);
            }
        }
    }

    fn advance_live_close_deadlines(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .workspace
            .live_documents()
            .iter()
            .filter_map(|document| match document.lifecycle {
                LiveDocumentLifecycle::Closing { stop_deadline } if now >= stop_deadline => {
                    Some((document.id, document.session_id.clone()))
                }
                _ => None,
            })
            .collect();
        for (id, session_id) in expired {
            if self.live_runtime.force_cleanup(&session_id)
                && let Some(document) = self.workspace.live_mut(id)
            {
                document.lifecycle = LiveDocumentLifecycle::ForcedCleanup {
                    terminal: camera_toolbox_app::StreamTerminal::Forced {
                        remote_state_unknown: true,
                    },
                };
            }
        }
    }

    fn render_live_viewer(
        ui: &mut egui::Ui,
        document: &mut LiveDocument,
        runtime: &LiveRuntime,
    ) -> egui::Rect {
        let rect = ui.max_rect();
        ui.horizontal(|ui| {
            ui.heading(&document.title);
            if ui.button("Snapshot...").clicked()
                && let Some(frame) = document.latest_frame.latest()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("PNG image", &["png"])
                    .save_file()
            {
                document.last_snapshot = Some(match write_live_snapshot(&path, &frame) {
                    Ok(()) => format!("Saved {}", path.display()),
                    Err(error) => format!("Snapshot failed: {error}"),
                });
            }
        });
        if let Some(message) = document.last_snapshot.as_deref() {
            ui.label(message);
        }
        match &document.lifecycle {
            LiveDocumentLifecycle::CloseRequested => {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(
                        "Close this live session? The only stop action is closing its TCP socket.",
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            document.lifecycle = LiveDocumentLifecycle::Open;
                        }
                        if ui.button("Close Stream").clicked() {
                            if runtime.request_close(&document.session_id) {
                                document.lifecycle = LiveDocumentLifecycle::Closing {
                                    stop_deadline: Instant::now() + LIVE_STOP_TIMEOUT,
                                };
                            } else {
                                document.lifecycle = LiveDocumentLifecycle::ForcedCleanup {
                                    terminal: camera_toolbox_app::StreamTerminal::Forced {
                                        remote_state_unknown: true,
                                    },
                                };
                            }
                        }
                    });
                });
            }
            LiveDocumentLifecycle::Closing { .. } => {
                ui.colored_label(egui::Color32::YELLOW, "Closing asynchronously...");
            }
            LiveDocumentLifecycle::ForcedCleanup { terminal }
            | LiveDocumentLifecycle::Terminal { terminal } => {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("Stopped: {terminal:?}"));
            }
            LiveDocumentLifecycle::Open => {}
        }
        if let Some(reason) = document.decoder_unavailable.as_deref() {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!("Decoder unavailable; record-only mode: {reason}"),
            );
        }
        ui.separator();
        egui::Grid::new(format!("live_metrics_{}", document.id))
            .num_columns(4)
            .show(ui, |ui| {
                ui.label(format!("Network {} B", document.metrics.network_bytes));
                ui.label(format!("RTP {}", document.metrics.rtp_packets));
                ui.label(format!("Gaps {}", document.metrics.rtp_gaps));
                ui.label(format!("Dropped {}", document.metrics.preview_dropped));
                ui.end_row();
                ui.label(format!("Decoded {}", document.metrics.decoded_frames));
                ui.label(format!("Presented {}", document.presented_frames));
                ui.label(format!("Resync {}", document.metrics.decoder_resyncs));
                ui.label(format!("Record {} B", document.metrics.record_bytes));
                ui.end_row();

                ui.label(format!(
                    "Preview Q {}",
                    document.metrics.preview_queue_depth
                ));
                ui.label(format!(
                    "Decoder Q {}",
                    document.metrics.decoder_queue_depth
                ));
                ui.label(format!(
                    "Record Q {} B",
                    document.metrics.recorder_queue_bytes
                ));
                ui.label("source_rtp_pts=None");
                ui.end_row();
            });
        ui.separator();
        if let Some(texture) = document.texture() {
            let available = ui.available_size();
            let source = texture.size_vec2();
            let scale = (available.x / source.x)
                .min(available.y / source.y)
                .min(1.0)
                .max(0.01);
            ui.centered_and_justified(|ui| {
                ui.add(egui::Image::new(texture).fit_to_exact_size(source * scale));
            });
        } else {
            ui.centered_and_justified(|ui| {
                ui.spinner();
                ui.label("Waiting for decoded frame");
            });
        }
        rect
    }

    fn forget_auto_open_document(&mut self, id: DocumentId) {
        self.auto_open_background_tabs
            .retain(|existing| *existing != id);
        self.auto_open_documents
            .retain(|_, existing| *existing != id);
    }

    fn track_auto_open_background_tab(&mut self, context: &egui::Context, id: DocumentId) {
        self.auto_open_background_tabs
            .retain(|existing| *existing != id);
        self.auto_open_background_tabs.push_back(id);
        while self.auto_open_background_tabs.len() > AUTO_OPEN_BACKGROUND_TAB_LIMIT {
            let Some(oldest) = self.auto_open_background_tabs.pop_front() else {
                break;
            };
            if self.workspace.active_id() == Some(oldest) {
                self.auto_open_background_tabs.push_back(oldest);
                break;
            }
            self.close_document(context, oldest);
        }
    }

    fn handle_tab_action(&mut self, context: &egui::Context, action: TabBarAction) {
        match action {
            TabBarAction::Activate(id) => {
                if self.workspace.activate(id) {
                    self.ensure_active_resources(context);
                }
            }
            TabBarAction::Close(id) => self.close_document(context, id),
        }
    }

    fn close_document(&mut self, context: &egui::Context, id: DocumentId) {
        if self
            .workspace
            .asset(id)
            .is_some_and(|document| !document.saved)
            || self
                .workspace
                .document(id)
                .is_some_and(|document| document.unsaved && document.source_asset.is_some())
        {
            self.pending_ephemeral_close = Some(id);
            self.workspace.activate(id);
            return;
        }
        self.forget_auto_open_document(id);
        if self.workspace.asset(id).is_some() {
            self.workspace.remove_asset(id);
            return;
        }
        if let Some(document) = self.workspace.live_mut(id) {
            let (remove, activate_confirmation) = match document.lifecycle {
                LiveDocumentLifecycle::Open => {
                    document.lifecycle = LiveDocumentLifecycle::CloseRequested;
                    (false, true)
                }
                LiveDocumentLifecycle::Terminal { .. }
                | LiveDocumentLifecycle::ForcedCleanup { .. } => (true, false),
                LiveDocumentLifecycle::CloseRequested | LiveDocumentLifecycle::Closing { .. } => {
                    (false, false)
                }
            };
            if remove {
                self.workspace.remove_live(id);
            } else if activate_confirmation {
                self.workspace.activate(id);
            }
            return;
        }
        let Some(document) = self.workspace.close(id) else {
            return;
        };
        self.notifications
            .clear_scope(NotificationScope::ImageGeneration(
                document.loaded.generation,
            ));
        tracing::debug!(
            operation = "close_document",
            document_id = %document.id,
            generation = document.loaded.generation,
            "closed RAW document"
        );
        self.ensure_active_resources(context);
        self.workspace.enforce_derived_budget();
    }
}

fn decode_raw_request(
    pipeline: &RawOpenPipeline,
    request: LocalRawAnalyzeRequest,
    cancellation: FsCancellation,
) -> anyhow::Result<OpenedRawDocument> {
    tracing::debug!(
        operation = "load_raw",
        path = %request.path.display(),
        "starting RAW load"
    );
    let (file_system, reference) = local_file_source(&request.path)?;
    let mut control = FsControl::with_timeout(Duration::from_secs(30));
    control.cancellation = cancellation;
    let result = pipeline.begin(
        &file_system,
        &reference,
        RawOpenMode::WithOptions(RawDecodeParams {
            spec: request.spec.clone(),
            encoding: request.encoding,
        }),
        &control,
    )?;
    let roi = request
        .roi
        .clamped_to(result.frame.spec.width, result.frame.spec.height)
        .unwrap_or(Roi {
            x: 0,
            y: 0,
            width: result.frame.spec.width,
            height: result.frame.spec.height,
        });
    let stats = analyze_roi(&result.frame, roi)?;
    Ok(OpenedRawDocument {
        report: LocalRawAnalyzeReport {
            path: request.path,
            frame: result.frame,
            roi,
            stats,
        },

        source: result.source,
        interpretation: result.interpretation,
    })
}

fn decode_workspace_file_request(
    pipeline: &RawOpenPipeline,
    request: WorkspaceFileOpenRequest,
    cancellation: FsCancellation,
) -> anyhow::Result<OpenedRawDocument> {
    tracing::debug!(
        operation = "load_raw",
        path = %request.display_path.display(),
        source_id = %request.reference.source_id,
        "starting workspace RAW auto-open"
    );
    let mut control = FsControl::with_timeout(Duration::from_secs(30));
    control.cancellation = cancellation;
    let result = pipeline.begin(
        &*request.file_system,
        &request.reference,
        RawOpenMode::Auto,
        &control,
    )?;
    let roi = Roi {
        x: 0,
        y: 0,
        width: result.frame.spec.width,
        height: result.frame.spec.height,
    };
    let stats = analyze_roi(&result.frame, roi)?;
    Ok(OpenedRawDocument {
        report: LocalRawAnalyzeReport {
            path: request.display_path,
            frame: result.frame,
            roi,
            stats,
        },
        source: result.source,
        interpretation: result.interpretation,
    })
}

fn load_workspace_settings() -> WorkspaceSettings {
    let path = match workspace_settings_path() {
        Ok(path) => path,
        Err(error) => {
            tracing::error!(
                operation = "load_workspace_settings_path",
                error = %error,
                "failed to resolve workspace settings path"
            );
            return WorkspaceSettings::default();
        }
    };
    match WorkspaceSettings::load_from_path(&path) {
        Ok(settings) => settings,
        Err(WorkspaceSettingsError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            WorkspaceSettings::default()
        }
        Err(error) => {
            tracing::error!(
                operation = "load_workspace_settings",
                path = %path.display(),
                error = %error,
                "failed to load workspace settings; using defaults"
            );
            WorkspaceSettings::default()
        }
    }
}

fn save_workspace_settings(settings: &WorkspaceSettings) -> Result<(), WorkspaceSettingsError> {
    let path = workspace_settings_path()?;
    settings.save_to_path(&path)
}

#[cfg(feature = "platform-ssh")]
fn load_connection_settings() -> ConnectionSettings {
    let path = match connection_settings_path() {
        Ok(path) => path,
        Err(error) => {
            tracing::error!(
                operation = "load_connection_settings_path",
                error = %error,
                "failed to resolve connection settings path"
            );
            return ConnectionSettings::default();
        }
    };
    match ConnectionSettings::load_from_path(&path) {
        Ok(settings) => settings,
        Err(FileWorkspaceSettingsError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            ConnectionSettings::default()
        }
        Err(error) => {
            tracing::error!(
                operation = "load_connection_settings",
                path = %path.display(),
                error = %error,
                "failed to load connection settings; using defaults"
            );
            ConnectionSettings::default()
        }
    }
}

#[cfg(feature = "platform-ssh")]
fn save_connection_settings(settings: &ConnectionSettings) -> Result<(), WorkspaceSettingsError> {
    let path = connection_settings_path()?;
    settings.save_to_path(&path)
}

fn decode_raw_reinterpret(
    pipeline: &RawOpenPipeline,
    source: RawSourceHandle,
    params: RawDecodeParams,
    decode_generation: u64,
    roi: Roi,
    path: PathBuf,
) -> anyhow::Result<OpenedRawDocument> {
    tracing::debug!(
        operation = "reinterpret_raw",
        decode_generation,
        path = %path.display(),
        "starting RAW reinterpret"
    );
    let control = FsControl::with_timeout(Duration::from_secs(30));
    let result = pipeline.reinterpret(source, params, decode_generation, &control)?;
    let roi = roi
        .clamped_to(result.frame.spec.width, result.frame.spec.height)
        .unwrap_or(Roi {
            x: 0,
            y: 0,
            width: result.frame.spec.width,
            height: result.frame.spec.height,
        });
    let stats = analyze_roi(&result.frame, roi)?;
    Ok(OpenedRawDocument {
        report: LocalRawAnalyzeReport {
            path,
            frame: result.frame,
            roi,
            stats,
        },
        source: result.source,
        interpretation: result.interpretation,
    })
}
fn asset_extension(format: &MediaFormat) -> &'static str {
    match format {
        MediaFormat::RawPacked { .. } | MediaFormat::RawU16Le { .. } => "raw",
        MediaFormat::Jpeg => "jpg",
        MediaFormat::Yuv420Sp { .. } => "nv21",
        MediaFormat::H264AnnexB => "h264",
        MediaFormat::H265AnnexB => "h265",
        MediaFormat::Binary => "bin",
    }
}
fn save_asset_source(
    path: &Path,
    asset: &camera_toolbox_core::EphemeralAsset,
) -> Result<(), String> {
    save_asset_source_with(path, asset, |file, asset| {
        use std::io::Write;

        match &asset.source {
            OwnedMediaPayload::Bytes(bytes) => file.write_all(bytes)?,
            OwnedMediaPayload::Planes(planes) => {
                for plane in planes {
                    file.write_all(&plane.bytes)?;
                }
            }
        }
        Ok(())
    })
}

fn source_cache_directory() -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    std::env::temp_dir().join(format!(
        "camera-toolbox-source-cache-{}-{nonce}",
        std::process::id()
    ))
}

fn save_asset_source_with<F>(
    path: &Path,
    asset: &camera_toolbox_core::EphemeralAsset,
    write_payload: F,
) -> Result<(), String>
where
    F: FnOnce(&mut std::fs::File, &camera_toolbox_core::EphemeralAsset) -> std::io::Result<()>,
{
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "destination already exists and was preserved; choose a new path: {}",
                    path.display()
                )
            } else {
                error.to_string()
            }
        })?;
    let result = write_payload(&mut file, asset)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = result {
        let _ = std::fs::remove_file(path);
        return Err(format!("export incomplete: {error}"));
    }
    Ok(())
}

fn write_live_snapshot(
    path: &Path,
    frame: &camera_toolbox_app::DecodedVideoFrame,
) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
    writer
        .write_image_data(&frame.rgba)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
