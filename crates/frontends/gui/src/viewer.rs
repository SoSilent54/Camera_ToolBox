//! RAW Viewer 显示、缩放、hover 与 minimap。

use camera_toolbox_app::LocalRawAnalyzeReport;
use camera_toolbox_core::{BayerPattern, RawFrame, Roi, RoiStats};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 64.0;
const MINIMAP_MAX_SIZE: egui::Vec2 = egui::vec2(180.0, 130.0);
const MINIMAP_MARGIN: f32 = 12.0;

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
}

pub(crate) struct LoadedRaw {
    pub(crate) path: std::path::PathBuf,
    pub(crate) frame: RawFrame,
    pub(crate) roi: Roi,
    pub(crate) stats: RoiStats,
    texture: TextureHandle,
    pub(crate) diagnostics: RawDiagnostics,
}

impl LoadedRaw {
    pub(crate) fn from_report(context: &egui::Context, report: LocalRawAnalyzeReport) -> Self {
        let (image, diagnostics) = preview_with_diagnostics(&report.frame);
        let texture = context.load_texture(
            format!("local-raw-preview:{}", report.path.display()),
            image,
            raw_texture_options(),
        );
        Self {
            path: report.path,
            frame: report.frame,
            roi: report.roi,
            stats: report.stats,
            texture,
            diagnostics,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CursorPixel {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) value: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct ImageViewerState {
    pub(crate) zoom: f32,
    pan: egui::Vec2,
    pub(crate) fit_on_next_frame: bool,
    pub(crate) cursor: Option<CursorPixel>,
}

impl Default for ImageViewerState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            fit_on_next_frame: true,
            cursor: None,
        }
    }
}

impl ImageViewerState {
    pub(crate) fn zoom_by(
        &mut self,
        factor: f32,
        anchor: Option<egui::Pos2>,
        viewer_rect: egui::Rect,
    ) {
        let old_zoom = self.zoom;
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let Some(anchor) = anchor else {
            return;
        };
        let scale = self.zoom / old_zoom;
        let local_anchor = anchor - viewer_rect.min;
        self.pan = local_anchor + (self.pan - local_anchor) * scale;
    }

    fn fit_to_rect(&mut self, rect: egui::Rect, image_width: f32, image_height: f32) {
        let zoom_x = rect.width() / image_width.max(1.0);
        let zoom_y = rect.height() / image_height.max(1.0);
        self.zoom = zoom_x.min(zoom_y).clamp(MIN_ZOOM, MAX_ZOOM);
        let image_size = egui::vec2(image_width * self.zoom, image_height * self.zoom);
        self.pan = rect.center().to_vec2() - image_size * 0.5 - rect.min.to_vec2();
        self.fit_on_next_frame = false;
    }

    fn image_rect(
        &self,
        viewer_rect: egui::Rect,
        image_width: f32,
        image_height: f32,
    ) -> egui::Rect {
        egui::Rect::from_min_size(
            viewer_rect.min + self.pan,
            egui::vec2(image_width * self.zoom, image_height * self.zoom),
        )
    }
}

pub(crate) fn render_viewer(
    ui: &mut egui::Ui,
    loaded: Option<&LoadedRaw>,
    viewer: &mut ImageViewerState,
) {
    let available = ui.available_size().max(egui::vec2(1.0, 1.0));
    let (viewer_rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
    let painter = ui.painter_at(viewer_rect);
    painter.rect_filled(viewer_rect, 0.0, egui::Color32::from_gray(22));

    let Some(loaded) = loaded else {
        painter.text(
            viewer_rect.center(),
            egui::Align2::CENTER_CENTER,
            "File -> Open Raw...",
            egui::TextStyle::Heading.resolve(ui.style()),
            egui::Color32::GRAY,
        );
        viewer.cursor = None;
        return;
    };

    if viewer.fit_on_next_frame {
        viewer.fit_to_rect(
            viewer_rect,
            loaded.frame.spec.width as f32,
            loaded.frame.spec.height as f32,
        );
    }

    if response.dragged_by(egui::PointerButton::Primary) {
        viewer.pan += response.drag_delta();
    }

    if response.hovered() {
        let scroll_y = ui.input(|input| input.smooth_scroll_delta().y);
        if scroll_y.abs() > f32::EPSILON {
            let factor = (scroll_y * 0.0015).exp();
            viewer.zoom_by(factor, response.hover_pos(), viewer_rect);
        }
    }

    let image_rect = viewer.image_rect(
        viewer_rect,
        loaded.frame.spec.width as f32,
        loaded.frame.spec.height as f32,
    );
    painter.image(
        loaded.texture.id(),
        image_rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
    draw_roi_overlay(&painter, image_rect, &loaded.frame, loaded.roi);
    draw_raw_warning_overlay(&painter, viewer_rect, loaded);

    viewer.cursor = response
        .hover_pos()
        .and_then(|position| hover_pixel(image_rect, position, &loaded.frame));
    if let Some(cursor) = viewer.cursor {
        response.on_hover_text(format!(
            "x={} y={} raw={}",
            cursor.x, cursor.y, cursor.value
        ));
    }

    draw_minimap(&painter, viewer_rect, image_rect, loaded);
}

/// 缩小时使用 mipmap 抑制混叠，放大时保持单像素边界。
const fn raw_texture_options() -> TextureOptions {
    TextureOptions {
        magnification: egui::TextureFilter::Nearest,
        minification: egui::TextureFilter::Linear,
        wrap_mode: egui::TextureWrapMode::ClampToEdge,
        mipmap_mode: Some(egui::TextureFilter::Linear),
    }
}

fn preview_with_diagnostics(frame: &RawFrame) -> (ColorImage, RawDiagnostics) {
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
                    x: x as u32,
                    y: y as u32,
                    value,
                });
                preview.push(egui::Color32::MAGENTA);
            } else {
                let gray = ((u32::from(value) * 255) / u32::from(max_code)) as u8;
                preview.push(egui::Color32::from_gray(gray));
            }
        }
    }

    (ColorImage::new([width, height], preview), diagnostics)
}

fn draw_raw_warning_overlay(painter: &egui::Painter, viewer_rect: egui::Rect, loaded: &LoadedRaw) {
    let diagnostics = loaded.diagnostics;
    let Some(first) = diagnostics.first_out_of_range else {
        return;
    };
    let text = format!(
        "RAW RANGE WARNING: {} pixels exceed {}-bit max {} (observed max {}; first x={} y={} raw={}). Magenta pixels preserve original values.",
        diagnostics.out_of_range_pixels,
        loaded.frame.spec.bit_depth,
        loaded.frame.spec.max_code_value(),
        diagnostics.observed_max,
        first.x,
        first.y,
        first.value
    );
    let banner = egui::Rect::from_min_size(
        viewer_rect.min + egui::vec2(12.0, 12.0),
        egui::vec2((viewer_rect.width() - 24.0).max(1.0), 32.0),
    );
    painter.rect_filled(
        banner,
        4.0,
        egui::Color32::from_rgba_premultiplied(80, 45, 0, 235),
    );
    painter.text(
        banner.left_center() + egui::vec2(8.0, 0.0),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(13.0),
        egui::Color32::YELLOW,
    );
}

fn hover_pixel(
    image_rect: egui::Rect,
    position: egui::Pos2,
    frame: &RawFrame,
) -> Option<CursorPixel> {
    if !image_rect.contains(position) || image_rect.width() <= 0.0 || image_rect.height() <= 0.0 {
        return None;
    }
    let width = frame.spec.width;
    let height = frame.spec.height;
    let x = (((position.x - image_rect.left()) / image_rect.width()) * width as f32).floor() as u32;
    let y =
        (((position.y - image_rect.top()) / image_rect.height()) * height as f32).floor() as u32;
    if x >= width || y >= height {
        return None;
    }
    let index = y as usize * width as usize + x as usize;
    Some(CursorPixel {
        x,
        y,
        value: frame.pixels()[index],
    })
}

fn draw_roi_overlay(painter: &egui::Painter, image_rect: egui::Rect, frame: &RawFrame, roi: Roi) {
    let Some(roi) = roi.clamped_to(frame.spec.width, frame.spec.height) else {
        return;
    };
    let scale_x = image_rect.width() / frame.spec.width as f32;
    let scale_y = image_rect.height() / frame.spec.height as f32;
    let min = egui::pos2(
        image_rect.left() + roi.x as f32 * scale_x,
        image_rect.top() + roi.y as f32 * scale_y,
    );
    let max = egui::pos2(
        image_rect.left() + (roi.x + roi.width) as f32 * scale_x,
        image_rect.top() + (roi.y + roi.height) as f32 * scale_y,
    );
    painter.rect_stroke(
        egui::Rect::from_min_max(min, max),
        0.0,
        egui::Stroke::new(1.0, egui::Color32::GREEN),
        egui::StrokeKind::Outside,
    );
}

fn draw_minimap(
    painter: &egui::Painter,
    viewer_rect: egui::Rect,
    image_rect: egui::Rect,
    loaded: &LoadedRaw,
) {
    let image_width = loaded.frame.spec.width as f32;
    let image_height = loaded.frame.spec.height as f32;
    if image_width <= 0.0 || image_height <= 0.0 {
        return;
    }

    let fit = (MINIMAP_MAX_SIZE.x / image_width)
        .min(MINIMAP_MAX_SIZE.y / image_height)
        .min(1.0);
    let minimap_size = egui::vec2(image_width * fit, image_height * fit).max(egui::vec2(1.0, 1.0));
    let minimap_rect = egui::Rect::from_min_size(
        egui::pos2(
            viewer_rect.right() - minimap_size.x - MINIMAP_MARGIN,
            viewer_rect.bottom() - minimap_size.y - MINIMAP_MARGIN,
        ),
        minimap_size,
    );

    painter.rect_filled(
        minimap_rect.expand(4.0),
        2.0,
        egui::Color32::from_black_alpha(180),
    );
    painter.image(
        loaded.texture.id(),
        minimap_rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );

    let visible_min = screen_to_image(viewer_rect.min, image_rect, image_width, image_height);
    let visible_max = screen_to_image(viewer_rect.max, image_rect, image_width, image_height);
    let min = egui::pos2(
        minimap_rect.left() + visible_min.x * fit,
        minimap_rect.top() + visible_min.y * fit,
    );
    let max = egui::pos2(
        minimap_rect.left() + visible_max.x * fit,
        minimap_rect.top() + visible_max.y * fit,
    );
    painter.rect_stroke(
        egui::Rect::from_min_max(min, max),
        0.0,
        egui::Stroke::new(1.0, egui::Color32::YELLOW),
        egui::StrokeKind::Outside,
    );
}

fn screen_to_image(
    position: egui::Pos2,
    image_rect: egui::Rect,
    image_width: f32,
    image_height: f32,
) -> egui::Pos2 {
    let x = (((position.x - image_rect.left()) / image_rect.width()) * image_width)
        .clamp(0.0, image_width);
    let y = (((position.y - image_rect.top()) / image_rect.height()) * image_height)
        .clamp(0.0, image_height);
    egui::pos2(x, y)
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
    use camera_toolbox_core::RawSpec;

    fn test_frame() -> RawFrame {
        RawFrame::new(
            RawSpec {
                width: 2,
                height: 2,
                bit_depth: 10,
                bayer: BayerPattern::Rggb,
            },
            vec![0, 1023, 512, 256],
        )
        .unwrap()
    }

    #[test]
    fn grayscale_preview_maps_tightly_packed_pixels() {
        let (image, diagnostics) = preview_with_diagnostics(&test_frame());

        assert_eq!(image.size, [2, 2]);
        assert_eq!(image.pixels[0], egui::Color32::from_gray(0));
        assert_eq!(image.pixels[1], egui::Color32::from_gray(255));
        assert_eq!(image.pixels[2], egui::Color32::from_gray(127));
        assert_eq!(image.pixels[3], egui::Color32::from_gray(63));
        assert_eq!(diagnostics.out_of_range_pixels, 0);
        assert_eq!(diagnostics.observed_max, 1023);
    }

    #[test]
    fn preview_marks_out_of_range_without_changing_raw_values() {
        let frame = RawFrame::new(
            RawSpec {
                width: 2,
                height: 1,
                bit_depth: 10,
                bayer: BayerPattern::Rggb,
            },
            vec![1024, 42],
        )
        .unwrap();

        let (image, diagnostics) = preview_with_diagnostics(&frame);

        assert_eq!(frame.pixels(), &[1024, 42]);
        assert_eq!(image.pixels[0], egui::Color32::MAGENTA);
        assert_eq!(diagnostics.out_of_range_pixels, 1);
    }

    #[test]
    fn hover_reads_original_tightly_packed_pixel() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(20.0, 20.0));
        let hovered = hover_pixel(rect, egui::pos2(15.0, 15.0), &test_frame()).unwrap();

        assert_eq!((hovered.x, hovered.y, hovered.value), (1, 1, 256));
    }

    #[test]
    fn texture_filters_smooth_minification_and_preserve_pixel_magnification() {
        let options = raw_texture_options();

        assert_eq!(options.magnification, egui::TextureFilter::Nearest);
        assert_eq!(options.minification, egui::TextureFilter::Linear);
        assert_eq!(options.mipmap_mode, Some(egui::TextureFilter::Linear));
    }

    #[test]
    fn fit_to_rect_uses_smaller_axis_and_centers_image() {
        let mut viewer = ImageViewerState::default();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));

        viewer.fit_to_rect(rect, 200.0, 100.0);
        let image_rect = viewer.image_rect(rect, 200.0, 100.0);

        assert!((viewer.zoom - 0.5).abs() < f32::EPSILON);
        assert_eq!(image_rect.size(), egui::vec2(100.0, 50.0));
        assert_eq!(image_rect.center(), rect.center());
    }
}
