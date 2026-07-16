//! 已加载 RAW、显示纹理及预览诊断的数据所有权。

use std::sync::Arc;

use camera_toolbox_app::LocalRawAnalyzeReport;
use camera_toolbox_core::{
    BayerPattern, ColorPipelineParams, ColorPixel, ColorRenderDiagnostics, RawFrame, Rgba8Frame,
    Roi, RoiStats, render_pixel_at,
};
use eframe::egui::{self, ColorImage, TextureHandle, TextureId, TextureOptions};

use crate::color_controls::{ColorEditState, DisplayMode};

use super::CursorPixel;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RawDiagnostics {
    pub(crate) out_of_range_pixels: u64,
    pub(crate) observed_max: u16,
    first_out_of_range: Option<CursorPixel>,
}

impl RawDiagnostics {
    pub(crate) const fn has_out_of_range(self) -> bool {
        self.out_of_range_pixels != 0
    }

    pub(crate) const fn first_out_of_range(self) -> Option<CursorPixel> {
        self.first_out_of_range
    }
}

pub(crate) struct InstalledColorPreview {
    texture: TextureHandle,
    pub(crate) image: Arc<ColorImage>,
    pub(crate) frame: Arc<Rgba8Frame>,
    pub(crate) rendered_params: ColorPipelineParams,
    pub(crate) rendered_revision: u64,
    pub(crate) diagnostics: ColorRenderDiagnostics,
}

pub(crate) struct LoadedRaw {
    pub(crate) generation: u64,
    pub(crate) path: std::path::PathBuf,
    pub(crate) frame: Arc<RawFrame>,
    pub(crate) roi: Roi,
    pub(crate) stats: Option<RoiStats>,
    raw_texture: Option<TextureHandle>,
    pub(crate) installed_color: Option<InstalledColorPreview>,
    pub(crate) color_edit: ColorEditState,
    pub(crate) diagnostics: RawDiagnostics,
}

impl LoadedRaw {
    pub(crate) fn from_report(
        context: &egui::Context,
        report: LocalRawAnalyzeReport,
        generation: u64,
    ) -> Self {
        let (image, diagnostics) = preview_with_diagnostics(&report.frame);
        let raw_texture = context.load_texture(
            format!("local-raw-preview:{generation}:{}", report.path.display()),
            image,
            raw_texture_options(),
        );
        let color_edit = ColorEditState::new(&report.frame.spec);
        Self {
            generation,
            path: report.path,
            frame: Arc::new(report.frame),
            roi: report.roi,
            stats: Some(report.stats),
            raw_texture: Some(raw_texture),
            installed_color: None,
            color_edit,
            diagnostics,
        }
    }

    /// 将旧 RAW 的完整颜色草稿迁移到新 frame，并失效仅属于旧 frame 的派生预览。
    pub(crate) fn inherit_color_edit_from(&mut self, previous: &mut Self) {
        std::mem::swap(&mut self.color_edit, &mut previous.color_edit);
        self.color_edit.prepare_for_new_frame();
        self.installed_color = None;
    }

    pub(crate) fn install_color(
        &mut self,
        context: &egui::Context,
        revision: u64,
        params: ColorPipelineParams,
        image: Arc<ColorImage>,
        frame: Arc<Rgba8Frame>,
        diagnostics: ColorRenderDiagnostics,
    ) {
        let texture = if let Some(mut installed) = self.installed_color.take() {
            installed
                .texture
                .set(Arc::clone(&image), raw_texture_options());
            installed.texture
        } else {
            context.load_texture(
                format!("local-raw-color:{}", self.generation),
                Arc::clone(&image),
                raw_texture_options(),
            )
        };
        self.installed_color = Some(InstalledColorPreview {
            texture,
            image,
            frame,
            rendered_params: params,
            rendered_revision: revision,
            diagnostics,
        });
    }

    pub(crate) fn installed_revision(&self) -> Option<u64> {
        self.installed_color
            .as_ref()
            .map(|preview| preview.rendered_revision)
    }

    /// RAW GPU texture、Color CPU image 与 Color GPU texture 的保守驻留估算。
    pub(crate) fn derived_resource_bytes(&self) -> usize {
        let pixel_count = self.frame.pixels().len();
        let raw_texture_bytes = self.raw_texture.as_ref().map_or(0, |_| {
            pixel_count.saturating_mul(std::mem::size_of::<egui::Color32>())
        });
        let color_bytes = self.installed_color.as_ref().map_or(0, |preview| {
            preview
                .image
                .pixels
                .len()
                .saturating_mul(std::mem::size_of::<egui::Color32>().saturating_mul(2))
        });
        raw_texture_bytes.saturating_add(color_bytes)
    }

    pub(crate) const fn has_raw_texture(&self) -> bool {
        self.raw_texture.is_some()
    }

    /// GPU texture 必须在 egui 线程恢复；权威 RawFrame 始终保持不变。
    pub(crate) fn ensure_raw_texture(&mut self, context: &egui::Context) {
        if self.raw_texture.is_some() {
            return;
        }
        let (image, _) = preview_with_diagnostics(&self.frame);
        self.raw_texture = Some(context.load_texture(
            format!(
                "local-raw-preview:{}:{}",
                self.generation,
                self.path.display()
            ),
            image,
            raw_texture_options(),
        ));
    }

    /// 所有预览均可由权威 RAW 与参数重建；清除 submitted 允许再次调度同一 revision。
    pub(crate) fn evict_preview_resources(&mut self) {
        self.raw_texture = None;
        self.installed_color = None;
        self.color_edit.submitted_revision = None;
    }

    pub(super) fn active_texture_id(&self, display_mode: DisplayMode) -> TextureId {
        if display_mode == DisplayMode::Color
            && let Some(preview) = &self.installed_color
        {
            return preview.texture.id();
        }
        self.raw_texture
            .as_ref()
            .expect("active document raw texture is resident")
            .id()
    }

    pub(crate) fn displayed_bayer(&self, display_mode: DisplayMode) -> BayerPattern {
        if display_mode == DisplayMode::Color {
            self.inspection_bayer()
        } else {
            self.frame.spec.bayer
        }
    }

    pub(super) fn inspection_bayer(&self) -> BayerPattern {
        self.installed_color
            .as_ref()
            .map_or(self.frame.spec.bayer, |preview| {
                preview.rendered_params.bayer
            })
    }

    pub(super) fn raw_texture_id(&self) -> TextureId {
        self.raw_texture
            .as_ref()
            .expect("active document raw texture is resident")
            .id()
    }

    pub(super) fn rendered_pixel(&self, cursor: CursorPixel) -> Option<ColorPixel> {
        let preview = self.installed_color.as_ref()?;
        render_pixel_at(&self.frame, &preview.rendered_params, cursor.x, cursor.y).ok()
    }
}

/// 缩小时使用 mipmap 抑制混叠，放大时保持单像素边界。
pub(super) const fn raw_texture_options() -> TextureOptions {
    TextureOptions {
        magnification: egui::TextureFilter::Nearest,
        minification: egui::TextureFilter::Linear,
        wrap_mode: egui::TextureWrapMode::ClampToEdge,
        mipmap_mode: Some(egui::TextureFilter::Linear),
    }
}

pub(super) fn preview_with_diagnostics(frame: &RawFrame) -> (ColorImage, RawDiagnostics) {
    let width = frame.spec.width as usize;
    let height = frame.spec.height as usize;
    let max_code = frame.spec.max_code_value();
    let pixels = frame.pixels();
    let mut preview = Vec::with_capacity(width * height);
    let mut diagnostics = RawDiagnostics::default();

    for y in 0..height {
        let row_start = y * width;
        for x in 0..width {
            let value = pixels[row_start + x];
            diagnostics.observed_max = diagnostics.observed_max.max(value);
            if value > max_code {
                diagnostics.out_of_range_pixels += 1;
                diagnostics.first_out_of_range.get_or_insert(CursorPixel {
                    x: u32::try_from(x).expect("x originates from u32 image width"),
                    y: u32::try_from(y).expect("y originates from u32 image height"),
                    raw_value: Some(value),
                });
                preview.push(egui::Color32::MAGENTA);
            } else {
                let gray = u8::try_from((u32::from(value) * 255) / u32::from(max_code))
                    .expect("normalized RAW code stays within u8");
                preview.push(egui::Color32::from_gray(gray));
            }
        }
    }

    (ColorImage::new([width, height], preview), diagnostics)
}

pub(crate) const fn bayer_label(pattern: BayerPattern) -> &'static str {
    match pattern {
        BayerPattern::Rggb => "rggb",
        BayerPattern::Grbg => "grbg",
        BayerPattern::Gbrg => "gbrg",
        BayerPattern::Bggr => "bggr",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_core::{CfaQuad, RawSpec};
    use std::path::PathBuf;

    fn loaded(generation: u64) -> LoadedRaw {
        let spec = RawSpec {
            width: 2,
            height: 2,
            bit_depth: 10,
            bayer: BayerPattern::Rggb,
        };
        LoadedRaw {
            generation,
            path: PathBuf::from("frame.raw"),
            frame: Arc::new(RawFrame::new(spec.clone(), vec![1, 2, 3, 4]).unwrap()),
            roi: Roi {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            stats: None,
            raw_texture: None,
            installed_color: None,
            color_edit: ColorEditState::new(&spec),
            diagnostics: RawDiagnostics::default(),
        }
    }

    #[test]
    fn inheriting_color_edit_preserves_params_and_invalidates_submission() {
        let mut previous = loaded(1);
        previous.color_edit.params.bayer = BayerPattern::Bggr;
        previous.color_edit.params.black_level = CfaQuad {
            r: 10,
            gr: 20,
            gb: 30,
            b: 40,
        };
        previous.color_edit.params.gain = CfaQuad {
            r: 1.1,
            gr: 1.2,
            gb: 1.3,
            b: 1.4,
        };
        previous.color_edit.params.display_gamma = Some(1.8);
        previous.color_edit.link_black = false;
        previous.color_edit.link_green = false;
        previous.color_edit.touch();
        previous.color_edit.mark_submitted();
        let revision = previous.color_edit.revision;

        let mut replacement = loaded(2);
        replacement.inherit_color_edit_from(&mut previous);

        assert_eq!(replacement.color_edit.params.bayer, BayerPattern::Bggr);
        assert_eq!(replacement.color_edit.params.black_level.r, 10);
        assert!((replacement.color_edit.params.gain.b - 1.4).abs() < f32::EPSILON);
        assert_eq!(replacement.color_edit.params.display_gamma, Some(1.8));
        assert!(!replacement.color_edit.link_black);
        assert!(!replacement.color_edit.link_green);
        assert_eq!(replacement.color_edit.revision, revision);
        assert_eq!(replacement.color_edit.submitted_revision, None);
        assert!(replacement.installed_color.is_none());
    }
}
