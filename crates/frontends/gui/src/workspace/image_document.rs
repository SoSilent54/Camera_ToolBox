//! PNG/JPEG/YUV 静态图像文档；权威 native 与派生 display 分离。

use std::{path::PathBuf, sync::Arc};

use camera_toolbox_app::{
    ImageFileKind, ImageOpenResult, ImageSourceHandle, TargetResolutionSnapshot,
};
use camera_toolbox_core::{ChromaOrder, EphemeralAsset, NativeImage, Rgba8Frame, Roi};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

use crate::{
    analysis_panel::AnalysisPanelState,
    viewer::{HoverViewSettings, ImageViewerState},
};

use super::DocumentId;

pub(crate) enum ImageDocumentSource {
    WorkspaceFile {
        path: PathBuf,
        handle: ImageSourceHandle,
        kind: ImageFileKind,
    },
    Capture {
        asset: Arc<EphemeralAsset>,
        resolution: Arc<TargetResolutionSnapshot>,
    },
}

impl ImageDocumentSource {
    pub(crate) fn asset(&self) -> Option<&Arc<EphemeralAsset>> {
        match self {
            Self::WorkspaceFile { .. } => None,
            Self::Capture { asset, .. } => Some(asset),
        }
    }
}

/// Viewer 的当前不可变显示 revision；纹理可以驱逐，RGBA frame 保持可重建/可保存。
pub(crate) struct DisplaySurface {
    pub(crate) revision: u64,
    pub(crate) frame: Arc<Rgba8Frame>,
    texture: Option<TextureHandle>,
}

impl DisplaySurface {
    fn new(revision: u64, frame: Arc<Rgba8Frame>) -> Self {
        Self {
            revision,
            frame,
            texture: None,
        }
    }

    pub(crate) fn ensure_texture(
        &mut self,
        context: &egui::Context,
        document_id: DocumentId,
    ) -> Result<(), String> {
        if self.texture.is_some() {
            return Ok(());
        }
        let expected_stride = (self.frame.width as usize)
            .checked_mul(4)
            .ok_or_else(|| "display width overflow".to_owned())?;
        if self.frame.stride != expected_stride {
            return Err(format!(
                "display RGBA stride {} is not tightly packed ({expected_stride})",
                self.frame.stride
            ));
        }
        let size = [
            usize::try_from(self.frame.width)
                .map_err(|_| "display width does not fit host".to_owned())?,
            usize::try_from(self.frame.height)
                .map_err(|_| "display height does not fit host".to_owned())?,
        ];
        let image = ColorImage::from_rgba_unmultiplied(size, self.frame.pixels());
        self.texture = Some(context.load_texture(
            format!("image-document:{document_id}:{}", self.revision),
            image,
            TextureOptions {
                magnification: egui::TextureFilter::Nearest,
                minification: egui::TextureFilter::Linear,
                wrap_mode: egui::TextureWrapMode::ClampToEdge,
                mipmap_mode: Some(egui::TextureFilter::Linear),
            },
        ));
        Ok(())
    }

    pub(crate) fn texture_id(&self) -> Option<egui::TextureId> {
        self.texture.as_ref().map(TextureHandle::id)
    }

    pub(crate) fn evict_texture(&mut self) {
        self.texture = None;
    }

    pub(crate) fn derived_bytes(&self, native: &NativeImage) -> usize {
        let display_bytes = match native {
            NativeImage::Rgba8(frame) if Arc::ptr_eq(frame, &self.frame) => 0,
            NativeImage::Raw(_) | NativeImage::Rgba8(_) | NativeImage::Yuv420Sp(_) => {
                self.frame.pixels().len()
            }
        };
        let texture_bytes = self
            .texture
            .as_ref()
            .map_or(0, |_| self.frame.pixels().len());
        display_bytes.saturating_add(texture_bytes)
    }
}

pub(crate) struct ImageDocument {
    pub(crate) id: DocumentId,
    pub(crate) title: String,
    pub(crate) generation: u64,
    pub(crate) source: ImageDocumentSource,
    pub(crate) native: NativeImage,
    pub(crate) display: DisplaySurface,
    pub(crate) viewer: ImageViewerState,
    pub(crate) hover_view: HoverViewSettings,
    pub(crate) roi: Roi,
    pub(crate) analysis_panel: AnalysisPanelState,
    pub(crate) analysis_pending_active: Option<(u64, Roi)>,
    pub(crate) last_access: u64,
    pub(crate) unsaved: bool,
}

impl ImageDocument {
    pub(crate) fn from_open_result(
        id: DocumentId,
        generation: u64,
        path: PathBuf,
        result: ImageOpenResult,
        last_access: u64,
    ) -> Result<Self, String> {
        let display = result
            .display
            .ok_or_else(|| "RAW result must use RawDocument".to_owned())?;
        let [width, height] = result.native.dimensions();
        let title = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Untitled Image")
            .to_owned();
        let mut analysis_panel = AnalysisPanelState::default_for_native(&result.native);
        analysis_panel.open_for_first_image();
        Ok(Self {
            id,
            title,
            generation,
            source: ImageDocumentSource::WorkspaceFile {
                path,
                handle: result.source,
                kind: result.kind,
            },
            native: result.native,
            display: DisplaySurface::new(generation, display),
            viewer: ImageViewerState::default(),
            hover_view: HoverViewSettings::default(),
            roi: Roi {
                x: 0,
                y: 0,
                width,
                height,
            },
            analysis_panel,
            analysis_pending_active: None,
            last_access,
            unsaved: false,
        })
    }

    pub(crate) fn from_capture(
        id: DocumentId,
        generation: u64,
        asset: Arc<EphemeralAsset>,
        resolution: Arc<TargetResolutionSnapshot>,
        native: NativeImage,
        display: Arc<Rgba8Frame>,
        last_access: u64,
    ) -> Self {
        let [width, height] = native.dimensions();
        let mut analysis_panel = AnalysisPanelState::default_for_native(&native);
        analysis_panel.open_for_first_image();
        Self {
            id,
            title: asset.metadata.source_name.clone(),
            generation,
            source: ImageDocumentSource::Capture { asset, resolution },
            native,
            display: DisplaySurface::new(generation, display),
            viewer: ImageViewerState::default(),
            hover_view: HoverViewSettings::default(),
            roi: Roi {
                x: 0,
                y: 0,
                width,
                height,
            },
            analysis_panel,
            analysis_pending_active: None,
            last_access,
            unsaved: true,
        }
    }

    pub(crate) const fn status_label(&self) -> &'static str {
        if self.unsaved { "Unsaved" } else { "Ready" }
    }

    pub(crate) fn format_label(&self) -> &'static str {
        match &self.source {
            ImageDocumentSource::WorkspaceFile { kind, .. } => match kind {
                ImageFileKind::Raw => "RAW",
                ImageFileKind::Png => "PNG",
                ImageFileKind::Jpeg => "JPEG",
                ImageFileKind::Yuv420Sp {
                    chroma_order_hint: Some(ChromaOrder::Uv),
                } => "NV12",
                ImageFileKind::Yuv420Sp {
                    chroma_order_hint: Some(ChromaOrder::Vu),
                } => "NV21",
                ImageFileKind::Yuv420Sp {
                    chroma_order_hint: None,
                } => "YUV420SP",
            },
            ImageDocumentSource::Capture { .. } => "Capture",
        }
    }

    pub(crate) fn ensure_texture(&mut self, context: &egui::Context) -> Result<(), String> {
        self.display.ensure_texture(context, self.id)
    }

    pub(crate) fn derived_resource_bytes(&self) -> usize {
        self.display
            .derived_bytes(&self.native)
            .saturating_add(self.analysis_panel.derived_resource_bytes())
    }

    pub(crate) fn evict_derived_resources(&mut self) -> usize {
        let before = self.derived_resource_bytes();
        self.display.evict_texture();
        self.viewer.evict_derived_resources();
        self.analysis_panel.evict_derived();
        before.saturating_sub(self.derived_resource_bytes())
    }
}
