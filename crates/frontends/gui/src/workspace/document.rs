//! 单个 RAW 文档的权威状态与可驱逐派生状态。

use std::{path::Path, sync::Arc};

use crate::{
    analysis_panel::AnalysisPanelState,
    color_controls::DisplayMode,
    histogram_link::{HistogramBinSelection, SpatialHighlight},
    raw_inspector::RawInspectorState,
    viewer::{HoverViewSettings, ImageViewerState, LoadedRaw},
};
use camera_toolbox_app::{RawInterpretation, RawSourceHandle, TargetResolutionSnapshot};
use camera_toolbox_core::EphemeralAsset;

use super::DocumentId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DocumentResourceStatus {
    Unloaded,
    Loading,
    Ready,
    Unsaved,
}

impl DocumentResourceStatus {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Unloaded => "Unloaded",
            Self::Loading => "Loading",
            Self::Ready => "Ready",
            Self::Unsaved => "Unsaved",
        }
    }
}

pub(crate) struct RawDocument {
    pub(crate) id: DocumentId,
    pub(crate) title: String,
    pub(crate) loaded: LoadedRaw,
    pub(crate) viewer: ImageViewerState,
    pub(crate) display_mode: DisplayMode,
    pub(crate) color_panel_expanded: bool,
    pub(crate) hover_view: HoverViewSettings,
    pub(crate) analysis_panel: AnalysisPanelState,
    pub(crate) analysis_pending_active: Option<(u64, camera_toolbox_core::Roi)>,
    pub(crate) spatial_requested: Option<HistogramBinSelection>,
    pub(crate) spatial_highlight: Option<SpatialHighlight>,
    pub(crate) last_access: u64,
    pub(crate) unsaved: bool,
    pub(crate) source_asset: Option<Arc<EphemeralAsset>>,
    pub(crate) resolution: Option<Arc<TargetResolutionSnapshot>>,
    pub(crate) raw_source: Option<RawSourceHandle>,
    pub(crate) interpretation: Option<RawInterpretation>,
    pub(crate) decode_generation: u64,
    pub(crate) raw_inspector: RawInspectorState,
    derived_evicted: bool,
}

impl RawDocument {
    pub(crate) fn new(id: DocumentId, loaded: LoadedRaw, last_access: u64) -> Self {
        let title = document_title(&loaded.path);
        let mut analysis_panel = AnalysisPanelState::default();
        analysis_panel.open_for_first_image();
        let raw_inspector = RawInspectorState::new(&loaded.frame.spec);
        Self {
            id,
            title,
            loaded,
            viewer: ImageViewerState::default(),
            display_mode: DisplayMode::Color,
            color_panel_expanded: true,
            hover_view: HoverViewSettings::default(),
            analysis_panel,
            analysis_pending_active: None,
            spatial_requested: None,
            spatial_highlight: None,
            last_access,
            unsaved: false,
            source_asset: None,
            resolution: None,
            raw_source: None,
            interpretation: None,
            decode_generation: 0,
            raw_inspector,
            derived_evicted: false,
        }
    }

    pub(crate) fn attach_file_source(
        &mut self,
        source: RawSourceHandle,
        interpretation: RawInterpretation,
        generation: u64,
    ) {
        self.raw_source = Some(source);
        self.interpretation = Some(interpretation.clone());
        self.decode_generation = generation;
        self.raw_inspector
            .sync_from_spec(&interpretation.params.spec);
    }

    pub(crate) fn replace_file_source(
        &mut self,
        loaded: LoadedRaw,
        source: RawSourceHandle,
        interpretation: RawInterpretation,
        generation: u64,
    ) {
        self.title = document_title(&loaded.path);
        self.unsaved = false;
        self.source_asset = None;
        self.resolution = None;
        self.install_reinterpreted(loaded, source, interpretation, generation);
    }

    pub(crate) fn install_reinterpreted(
        &mut self,
        mut loaded: LoadedRaw,
        source: RawSourceHandle,
        interpretation: RawInterpretation,
        decode_generation: u64,
    ) {
        let dimensions_changed = self.loaded.frame.spec.width != loaded.frame.spec.width
            || self.loaded.frame.spec.height != loaded.frame.spec.height;
        if dimensions_changed {
            self.viewer = ImageViewerState::default();
        } else {
            self.viewer.evict_derived_resources();
        }
        loaded.inherit_color_edit_from(&mut self.loaded);
        self.loaded = loaded;
        self.raw_source = Some(source);
        self.interpretation = Some(interpretation.clone());
        self.decode_generation = decode_generation;
        self.raw_inspector
            .sync_from_spec(&interpretation.params.spec);
        self.analysis_panel.evict_derived();
        self.analysis_pending_active = None;
        self.spatial_requested = None;
        self.spatial_highlight = None;
        self.derived_evicted = false;
    }

    pub(crate) fn attach_ephemeral_source(
        &mut self,
        asset: Arc<EphemeralAsset>,
        resolution: Arc<TargetResolutionSnapshot>,
    ) {
        self.title = asset.metadata.source_name.clone();
        self.source_asset = Some(asset);
        self.resolution = Some(resolution);
        self.unsaved = true;
    }

    pub(crate) fn resource_status(&self) -> DocumentResourceStatus {
        if self.unsaved {
            return DocumentResourceStatus::Unsaved;
        }
        if self.loaded.has_raw_texture()
            && (self.display_mode != DisplayMode::Color
                || self.loaded.installed_revision() == Some(self.loaded.color_edit.revision))
        {
            return DocumentResourceStatus::Ready;
        }
        if self.loaded.color_edit.submitted_revision == Some(self.loaded.color_edit.revision) {
            DocumentResourceStatus::Loading
        } else if self.derived_evicted {
            DocumentResourceStatus::Unloaded
        } else {
            DocumentResourceStatus::Loading
        }
    }

    pub(crate) fn mark_derived_loaded(&mut self) {
        self.derived_evicted = false;
    }

    pub(crate) fn derived_resource_bytes(&self) -> usize {
        self.loaded
            .derived_resource_bytes()
            .saturating_add(self.analysis_panel.derived_resource_bytes())
            .saturating_add(
                self.spatial_highlight
                    .as_ref()
                    .map_or(0, SpatialHighlight::estimated_bytes),
            )
    }

    /// 保留 RAW source、ROI、viewer transform、颜色参数和 Analysis UI 选择，仅释放可重建结果。
    pub(crate) fn evict_derived_resources(&mut self) -> usize {
        let bytes = self.derived_resource_bytes();
        if bytes == 0 {
            return 0;
        }
        self.loaded.evict_preview_resources();
        self.analysis_panel.evict_derived();
        self.analysis_pending_active = None;
        self.spatial_requested = None;
        self.spatial_highlight = None;
        self.viewer.evict_derived_resources();
        self.derived_evicted = true;
        bytes
    }
}

fn document_title(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map_or_else(|| path.display().to_string(), str::to_owned)
}
