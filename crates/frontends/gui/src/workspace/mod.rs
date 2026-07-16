//! 多文档工作区的稳定标识、文档所有权与资源预算。

use std::{fmt, sync::Arc};

#[cfg(test)]
use eframe::egui;

use crate::viewer::LoadedRaw;

mod document;
mod image_document;
mod live_document;
mod resources;
mod tabs;

pub(crate) use document::RawDocument;
pub(crate) use image_document::ImageDocument;
pub(crate) use live_document::{LiveDocument, LiveDocumentLifecycle};
pub(crate) use tabs::{TabBarAction, render_tab_bar};

pub(crate) const DEFAULT_DERIVED_RESOURCE_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// GUI 会话内稳定且不复用的文档标识。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct DocumentId(u64);

impl DocumentId {
    #[cfg(test)]
    pub(crate) const fn from_raw(value: u64) -> Self {
        Self(value)
    }
}

impl fmt::Display for DocumentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DocumentIdentity {
    pub(crate) document_id: DocumentId,
    pub(crate) generation: u64,
}

/// 当前静态 RAW 文档集合。权威 RAW source 常驻，只有可重建派生资源参与 LRU。
pub(crate) struct WorkspaceState {
    documents: Vec<RawDocument>,
    image_documents: Vec<ImageDocument>,
    live_documents: Vec<LiveDocument>,
    active: Option<DocumentId>,
    next_document_id: u64,
    access_clock: u64,
    derived_budget_bytes: usize,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::with_derived_budget(DEFAULT_DERIVED_RESOURCE_BUDGET_BYTES)
    }
}

impl WorkspaceState {
    pub(crate) const fn with_derived_budget(derived_budget_bytes: usize) -> Self {
        Self {
            documents: Vec::new(),
            image_documents: Vec::new(),
            live_documents: Vec::new(),
            active: None,
            next_document_id: 1,
            access_clock: 0,
            derived_budget_bytes,
        }
    }

    pub(crate) fn open_local_raw(&mut self, loaded: LoadedRaw) -> DocumentId {
        let id = DocumentId(self.next_document_id);
        self.next_document_id = self.next_document_id.saturating_add(1);
        self.access_clock = self.access_clock.saturating_add(1);
        self.documents
            .push(RawDocument::new(id, loaded, self.access_clock));
        self.active = Some(id);
        id
    }

    pub(crate) fn open_file_raw(
        &mut self,
        loaded: LoadedRaw,
        source: camera_toolbox_app::ImageSourceHandle,
        interpretation: camera_toolbox_app::RawInterpretation,
        generation: u64,
        foreground: bool,
    ) -> DocumentId {
        let id = DocumentId(self.next_document_id);
        self.next_document_id = self.next_document_id.saturating_add(1);
        self.access_clock = self.access_clock.saturating_add(1);
        let mut document = RawDocument::new(id, loaded, self.access_clock);
        document.attach_file_source(source, interpretation, generation);
        self.documents.push(document);
        if foreground || self.active.is_none() {
            self.active = Some(id);
        }
        id
    }

    pub(crate) fn open_image(
        &mut self,
        generation: u64,
        path: std::path::PathBuf,
        result: camera_toolbox_app::ImageOpenResult,
        foreground: bool,
    ) -> Result<DocumentId, String> {
        let id = DocumentId(self.next_document_id);
        self.next_document_id = self.next_document_id.saturating_add(1);
        self.access_clock = self.access_clock.saturating_add(1);
        let document =
            ImageDocument::from_open_result(id, generation, path, result, self.access_clock)?;
        self.image_documents.push(document);
        if foreground || self.active.is_none() {
            self.active = Some(id);
        }
        Ok(id)
    }

    pub(crate) fn replace_file_raw(
        &mut self,
        id: DocumentId,
        loaded: LoadedRaw,
        source: camera_toolbox_app::ImageSourceHandle,
        interpretation: camera_toolbox_app::RawInterpretation,
        generation: u64,
    ) -> bool {
        let Some(document) = self.document_mut(id) else {
            return false;
        };
        document.replace_file_source(loaded, source, interpretation, generation);
        true
    }

    pub(crate) fn open_live(
        &mut self,
        session_id: camera_toolbox_app::StreamSessionId,
        latest_frame: std::sync::Arc<camera_toolbox_app::LatestDecodedFrameSlot>,
    ) -> DocumentId {
        let id = DocumentId(self.next_document_id);
        self.next_document_id = self.next_document_id.saturating_add(1);
        self.live_documents
            .push(LiveDocument::new(id, session_id, latest_frame));
        self.active = Some(id);
        id
    }

    pub(crate) fn open_captured_raw(
        &mut self,
        loaded: LoadedRaw,
        asset: Arc<camera_toolbox_core::EphemeralAsset>,
        resolution: Arc<camera_toolbox_app::TargetResolutionSnapshot>,
        foreground: bool,
    ) -> DocumentId {
        if let Some(id) = self.document_for_asset(&asset.id) {
            if foreground {
                self.activate(id);
            }
            return id;
        }
        let id = DocumentId(self.next_document_id);
        self.next_document_id = self.next_document_id.saturating_add(1);
        self.access_clock = self.access_clock.saturating_add(1);
        let mut document = RawDocument::new(id, loaded, self.access_clock);
        document.attach_ephemeral_source(asset, resolution);
        self.documents.push(document);
        if foreground {
            self.active = Some(id);
        }
        id
    }

    pub(crate) fn open_captured_image(
        &mut self,
        generation: u64,
        asset: Arc<camera_toolbox_core::EphemeralAsset>,
        resolution: Arc<camera_toolbox_app::TargetResolutionSnapshot>,
        native: camera_toolbox_core::NativeImage,
        display: Arc<camera_toolbox_core::Rgba8Frame>,
        foreground: bool,
    ) -> Result<DocumentId, String> {
        if let Some(id) = self.document_for_asset(&asset.id) {
            if foreground {
                self.activate(id);
            }
            return Ok(id);
        }
        let id = DocumentId(self.next_document_id);
        self.next_document_id = self.next_document_id.saturating_add(1);
        self.access_clock = self.access_clock.saturating_add(1);
        let document = ImageDocument::from_capture(
            id,
            generation,
            asset,
            resolution,
            native,
            display,
            self.access_clock,
        );
        self.image_documents.push(document);
        if foreground {
            self.active = Some(id);
        }
        Ok(id)
    }

    pub(crate) const fn active_id(&self) -> Option<DocumentId> {
        self.active
    }

    pub(crate) fn active(&self) -> Option<&RawDocument> {
        self.document(self.active?)
    }

    pub(crate) fn active_mut(&mut self) -> Option<&mut RawDocument> {
        self.document_mut(self.active?)
    }

    pub(crate) fn active_live(&self) -> Option<&LiveDocument> {
        self.live(self.active?)
    }

    pub(crate) fn active_live_mut(&mut self) -> Option<&mut LiveDocument> {
        self.live_mut(self.active?)
    }

    pub(crate) fn active_image(&self) -> Option<&ImageDocument> {
        self.image(self.active?)
    }

    pub(crate) fn active_image_mut(&mut self) -> Option<&mut ImageDocument> {
        self.image_mut(self.active?)
    }

    pub(crate) fn document(&self, id: DocumentId) -> Option<&RawDocument> {
        self.documents.iter().find(|document| document.id == id)
    }

    pub(crate) fn document_mut(&mut self, id: DocumentId) -> Option<&mut RawDocument> {
        self.documents.iter_mut().find(|document| document.id == id)
    }

    pub(crate) fn documents(&self) -> &[RawDocument] {
        &self.documents
    }

    pub(crate) fn image(&self, id: DocumentId) -> Option<&ImageDocument> {
        self.image_documents
            .iter()
            .find(|document| document.id == id)
    }

    pub(crate) fn image_mut(&mut self, id: DocumentId) -> Option<&mut ImageDocument> {
        self.image_documents
            .iter_mut()
            .find(|document| document.id == id)
    }

    pub(crate) fn image_documents(&self) -> &[ImageDocument] {
        &self.image_documents
    }

    pub(crate) fn live(&self, id: DocumentId) -> Option<&LiveDocument> {
        self.live_documents
            .iter()
            .find(|document| document.id == id)
    }

    pub(crate) fn live_mut(&mut self, id: DocumentId) -> Option<&mut LiveDocument> {
        self.live_documents
            .iter_mut()
            .find(|document| document.id == id)
    }

    pub(crate) fn live_documents(&self) -> &[LiveDocument] {
        &self.live_documents
    }

    fn document_for_asset(&self, asset_id: &camera_toolbox_core::AssetId) -> Option<DocumentId> {
        self.documents
            .iter()
            .find(|document| {
                document
                    .source_asset
                    .as_ref()
                    .is_some_and(|asset| asset.id == *asset_id)
            })
            .map(|document| document.id)
            .or_else(|| {
                self.image_documents
                    .iter()
                    .find(|document| {
                        document
                            .source
                            .asset()
                            .is_some_and(|asset| asset.id == *asset_id)
                    })
                    .map(|document| document.id)
            })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.documents.is_empty()
            && self.image_documents.is_empty()
            && self.live_documents.is_empty()
    }

    pub(crate) fn supersede_color_submissions_except(&mut self, document_id: DocumentId) {
        for document in &mut self.documents {
            if document.id != document_id
                && document.loaded.installed_revision() != Some(document.loaded.color_edit.revision)
            {
                document.loaded.color_edit.submitted_revision = None;
            }
        }
    }

    pub(crate) fn supersede_analysis_submissions_except(&mut self, document_id: DocumentId) {
        for document in &mut self.documents {
            if document.id != document_id {
                document.analysis_panel.mark_submission_superseded();
                document.analysis_pending_active = None;
            }
        }
        for document in &mut self.image_documents {
            if document.id != document_id {
                document.analysis_panel.mark_submission_superseded();
                document.analysis_pending_active = None;
            }
        }
    }

    pub(crate) fn supersede_spatial_submissions_except(&mut self, document_id: DocumentId) {
        for document in &mut self.documents {
            if document.id != document_id {
                document.spatial_requested = None;
            }
        }
        for document in &mut self.image_documents {
            if document.id != document_id {
                document.spatial_requested = None;
            }
        }
    }

    pub(crate) fn activate(&mut self, id: DocumentId) -> bool {
        if self.document(id).is_none() && self.image(id).is_none() && self.live(id).is_none() {
            return false;
        }
        self.access_clock = self.access_clock.saturating_add(1);
        let tick = self.access_clock;
        self.active = Some(id);
        if let Some(document) = self.document_mut(id) {
            document.last_access = tick;
        }
        if let Some(document) = self.image_mut(id) {
            document.last_access = tick;
        }
        true
    }

    pub(crate) fn close(&mut self, id: DocumentId) -> Option<RawDocument> {
        let index = self
            .documents
            .iter()
            .position(|document| document.id == id)?;
        let was_active = self.active == Some(id);
        let removed = self.documents.remove(index);
        if was_active {
            self.active = self
                .documents
                .get(index)
                .or_else(|| {
                    index
                        .checked_sub(1)
                        .and_then(|index| self.documents.get(index))
                })
                .map(|document| document.id)
                .or_else(|| self.image_documents.first().map(|document| document.id))
                .or_else(|| self.live_documents.first().map(|document| document.id));
            if let Some(active) = self.active {
                self.access_clock = self.access_clock.saturating_add(1);
                let tick = self.access_clock;
                if let Some(document) = self.document_mut(active) {
                    document.last_access = tick;
                }
            }
        }
        Some(removed)
    }

    pub(crate) fn remove_live(&mut self, id: DocumentId) -> Option<LiveDocument> {
        let index = self
            .live_documents
            .iter()
            .position(|document| document.id == id)?;
        let was_active = self.active == Some(id);
        let removed = self.live_documents.remove(index);
        if was_active {
            self.active = self
                .live_documents
                .get(index)
                .or_else(|| {
                    index
                        .checked_sub(1)
                        .and_then(|index| self.live_documents.get(index))
                })
                .map(|document| document.id)
                .or_else(|| self.documents.last().map(|document| document.id));
        }
        Some(removed)
    }

    pub(crate) fn remove_image(&mut self, id: DocumentId) -> Option<ImageDocument> {
        let index = self
            .image_documents
            .iter()
            .position(|document| document.id == id)?;
        let was_active = self.active == Some(id);
        let removed = self.image_documents.remove(index);
        if was_active {
            self.active = self
                .image_documents
                .get(index)
                .or_else(|| {
                    index
                        .checked_sub(1)
                        .and_then(|index| self.image_documents.get(index))
                })
                .map(|document| document.id)
                .or_else(|| self.documents.last().map(|document| document.id))
                .or_else(|| self.live_documents.last().map(|document| document.id));
        }
        Some(removed)
    }

    pub(crate) fn live_by_session_mut(
        &mut self,
        session_id: &camera_toolbox_app::StreamSessionId,
    ) -> Option<&mut LiveDocument> {
        self.live_documents
            .iter_mut()
            .find(|document| document.session_id == *session_id)
    }

    pub(crate) fn release_inactive_live_textures(&mut self) {
        let active = self.active;
        for document in &mut self.live_documents {
            if Some(document.id) != active {
                document.release_texture();
            }
        }
    }

    /// 用稳定文档标识和 source generation 双重路由后台结果。
    pub(crate) fn matching_document_mut(
        &mut self,
        identity: DocumentIdentity,
    ) -> Option<&mut RawDocument> {
        let document = self.document_mut(identity.document_id)?;
        (document.loaded.generation == identity.generation).then_some(document)
    }

    pub(crate) fn matching_image_mut(
        &mut self,
        identity: DocumentIdentity,
    ) -> Option<&mut ImageDocument> {
        let document = self.image_mut(identity.document_id)?;
        (document.generation == identity.generation).then_some(document)
    }

    pub(crate) fn total_derived_bytes(&self) -> usize {
        let raw = self.documents.iter().fold(0usize, |total, document| {
            total.saturating_add(document.derived_resource_bytes())
        });
        self.image_documents.iter().fold(raw, |total, document| {
            total.saturating_add(document.derived_resource_bytes())
        })
    }

    /// Active 文档始终 pinned；按访问时钟驱逐 inactive 文档的可重建派生资源。
    pub(crate) fn enforce_derived_budget(&mut self) {
        resources::enforce_derived_budget(self);
    }

    #[cfg(test)]
    pub(crate) const fn derived_budget_bytes(&self) -> usize {
        self.derived_budget_bytes
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use camera_toolbox_app::LocalRawAnalyzeReport;
    use camera_toolbox_core::{BayerPattern, RawFrame, RawSpec, Roi, analyze_roi};

    use super::*;
    use crate::{analysis_worker::AnalysisDomain, color_controls::DisplayMode};

    fn loaded(context: &egui::Context, name: &str, generation: u64) -> LoadedRaw {
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
            width: 2,
            height: 2,
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

    #[test]
    fn two_tabs_keep_view_roi_color_and_analysis_isolated() {
        let context = egui::Context::default();
        let mut workspace = WorkspaceState::default();
        let first = workspace.open_local_raw(loaded(&context, "first.raw", 11));
        let second = workspace.open_local_raw(loaded(&context, "second.raw", 12));

        {
            let first = workspace.document_mut(first).unwrap();
            first.viewer.zoom = 3.0;
            first.viewer.fit_on_next_frame = false;
            first.display_mode = DisplayMode::RawMono;
            first.loaded.roi = Roi {
                x: 1,
                y: 0,
                width: 1,
                height: 2,
            };
            first.loaded.color_edit.touch();
            first.analysis_panel.domain = AnalysisDomain::DisplayRgb;
            first.analysis_panel.expanded = false;
        }

        let second_document = workspace.document(second).unwrap();
        assert_eq!(second_document.viewer.zoom, 1.0);
        assert!(second_document.viewer.fit_on_next_frame);
        assert_eq!(second_document.display_mode, DisplayMode::Color);
        assert_eq!(second_document.loaded.roi.width, 2);
        assert_eq!(second_document.loaded.color_edit.revision, 1);
        assert_eq!(
            second_document.analysis_panel.domain,
            AnalysisDomain::RawBayer
        );
        assert!(second_document.analysis_panel.expanded);
    }

    #[test]
    fn duplicate_generation_routes_only_by_document_identity() {
        let context = egui::Context::default();
        let mut workspace = WorkspaceState::default();
        let first = workspace.open_local_raw(loaded(&context, "first.raw", 7));
        let second = workspace.open_local_raw(loaded(&context, "second.raw", 7));

        let routed = workspace
            .matching_document_mut(DocumentIdentity {
                document_id: first,
                generation: 7,
            })
            .unwrap();
        routed.loaded.color_edit.mark_error("first only".to_owned());

        assert_eq!(
            workspace
                .document(first)
                .unwrap()
                .loaded
                .color_edit
                .render_error
                .as_deref(),
            Some("first only")
        );
        assert!(
            workspace
                .document(second)
                .unwrap()
                .loaded
                .color_edit
                .render_error
                .is_none()
        );
        assert!(
            workspace
                .matching_document_mut(DocumentIdentity {
                    document_id: second,
                    generation: 8,
                })
                .is_none()
        );
    }

    #[test]
    fn derived_budget_evicts_inactive_raw_texture_but_pins_active() {
        let context = egui::Context::default();
        let mut workspace = WorkspaceState::with_derived_budget(0);
        let first = workspace.open_local_raw(loaded(&context, "first.raw", 1));
        let second = workspace.open_local_raw(loaded(&context, "second.raw", 2));

        workspace.enforce_derived_budget();

        assert!(!workspace.document(first).unwrap().loaded.has_raw_texture());
        assert!(workspace.document(second).unwrap().loaded.has_raw_texture());
        assert!(workspace.total_derived_bytes() > workspace.derived_budget_bytes());
    }

    #[test]
    fn switching_and_close_choose_only_requested_document() {
        let context = egui::Context::default();
        let mut workspace = WorkspaceState::default();
        let first = workspace.open_local_raw(loaded(&context, "first.raw", 1));
        let second = workspace.open_local_raw(loaded(&context, "second.raw", 2));
        let third = workspace.open_local_raw(loaded(&context, "third.raw", 3));

        assert!(workspace.activate(second));
        assert_eq!(workspace.active_id(), Some(second));
        assert_eq!(workspace.close(second).unwrap().id, second);
        assert_eq!(workspace.active_id(), Some(third));
        assert!(workspace.document(first).is_some());
        assert!(workspace.document(second).is_none());
        assert!(workspace.document(third).is_some());
    }
}
