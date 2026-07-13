//! RAW Viewer 显示、缩放、hover 与 minimap。

use std::sync::Arc;

use camera_toolbox_app::LocalRawAnalyzeReport;
use camera_toolbox_core::{
    BayerPattern, ColorPipelineParams, ColorPixel, ColorRenderDiagnostics, RawFrame, Roi, RoiStats,
    render_pixel_at,
};
use eframe::egui::{self, ColorImage, TextureHandle, TextureId, TextureOptions};

use crate::color_controls::{ColorEditState, DisplayMode};

const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 64.0;
const MINIMAP_MAX_SIZE: egui::Vec2 = egui::vec2(180.0, 130.0);
const MINIMAP_MARGIN: f32 = 12.0;

const HOVER_VIEW_SIZE: egui::Vec2 = egui::vec2(448.0, 184.0);
const HOVER_CONTENT_SIZE: egui::Vec2 = egui::vec2(432.0, 168.0);
const HOVER_NEIGHBORHOOD_SIZE: egui::Vec2 = egui::vec2(168.0, 168.0);
const HOVER_POINTER_GAP: f32 = 16.0;
const HOVER_VIEWPORT_MARGIN: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HoverNeighborhood {
    Three,
    Five,
    Seven,
}

impl HoverNeighborhood {
    pub(crate) const ALL: [Self; 3] = [Self::Three, Self::Five, Self::Seven];

    pub(crate) const fn size(self) -> u32 {
        match self {
            Self::Three => 3,
            Self::Five => 5,
            Self::Seven => 7,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Three => "3 × 3",
            Self::Five => "5 × 5",
            Self::Seven => "7 × 7",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HoverViewSettings {
    pub(crate) enabled: bool,
    pub(crate) neighborhood: HoverNeighborhood,
}

impl Default for HoverViewSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            neighborhood: HoverNeighborhood::Five,
        }
    }
}

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

pub(crate) struct InstalledColorPreview {
    texture: TextureHandle,
    pub(crate) rendered_params: ColorPipelineParams,
    pub(crate) rendered_revision: u64,
    pub(crate) diagnostics: ColorRenderDiagnostics,
}

pub(crate) struct LoadedRaw {
    pub(crate) generation: u64,
    pub(crate) path: std::path::PathBuf,
    pub(crate) frame: Arc<RawFrame>,
    pub(crate) roi: Roi,
    pub(crate) stats: RoiStats,
    raw_texture: TextureHandle,
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
            stats: report.stats,
            raw_texture,
            installed_color: None,
            color_edit,
            diagnostics,
        }
    }

    pub(crate) fn install_color(
        &mut self,
        context: &egui::Context,
        revision: u64,
        params: ColorPipelineParams,
        image: ColorImage,
        diagnostics: ColorRenderDiagnostics,
    ) {
        let texture = if let Some(mut installed) = self.installed_color.take() {
            installed.texture.set(image, raw_texture_options());
            installed.texture
        } else {
            context.load_texture(
                format!("local-raw-color:{}", self.generation),
                image,
                raw_texture_options(),
            )
        };
        self.installed_color = Some(InstalledColorPreview {
            texture,
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

    fn active_texture_id(&self, display_mode: DisplayMode) -> TextureId {
        if display_mode == DisplayMode::Color
            && let Some(preview) = &self.installed_color
        {
            return preview.texture.id();
        }
        self.raw_texture.id()
    }

    pub(crate) fn displayed_bayer(&self, display_mode: DisplayMode) -> BayerPattern {
        if display_mode == DisplayMode::Color {
            self.inspection_bayer()
        } else {
            self.frame.spec.bayer
        }
    }

    fn inspection_bayer(&self) -> BayerPattern {
        self.installed_color
            .as_ref()
            .map_or(self.frame.spec.bayer, |preview| {
                preview.rendered_params.bayer
            })
    }

    fn raw_texture_id(&self) -> TextureId {
        self.raw_texture.id()
    }

    fn rendered_pixel(&self, cursor: CursorPixel) -> Option<ColorPixel> {
        let preview = self.installed_color.as_ref()?;
        render_pixel_at(&self.frame, &preview.rendered_params, cursor.x, cursor.y).ok()
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
    display_mode: DisplayMode,
    hover_settings: HoverViewSettings,
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
    let active_texture_id = loaded.active_texture_id(display_mode);
    painter.image(
        active_texture_id,
        image_rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
    draw_roi_overlay(&painter, image_rect, &loaded.frame, loaded.roi);
    draw_raw_warning_overlay(&painter, viewer_rect, loaded);
    draw_minimap(&painter, viewer_rect, image_rect, loaded, active_texture_id);

    let hover_position = response.hover_pos();
    viewer.cursor =
        hover_position.and_then(|position| hover_pixel(image_rect, position, &loaded.frame));
    if hover_settings.enabled
        && let (Some(position), Some(cursor)) = (hover_position, viewer.cursor)
    {
        render_hover_view(ui.ctx(), position, cursor, loaded, hover_settings);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NeighborhoodWindow {
    source_min: [u32; 2],
    source_max: [u32; 2],
    destination_min: [u32; 2],
    destination_max: [u32; 2],
}

fn render_hover_view(
    context: &egui::Context,
    pointer: egui::Pos2,
    cursor: CursorPixel,
    loaded: &LoadedRaw,
    settings: HoverViewSettings,
) {
    let position = hover_view_position(pointer, context.content_rect());
    egui::Area::new(egui::Id::new("raw_hover_view"))
        .fixed_pos(position)
        .order(egui::Order::Foreground)
        .interactable(false)
        .show(context, |ui| {
            egui::Frame::popup(ui.style())
                .inner_margin(8)
                .show(ui, |ui| {
                    ui.set_min_size(HOVER_CONTENT_SIZE);
                    ui.set_max_size(HOVER_CONTENT_SIZE);
                    ui.horizontal(|ui| {
                        let (neighborhood_rect, _) =
                            ui.allocate_exact_size(HOVER_NEIGHBORHOOD_SIZE, egui::Sense::hover());
                        let has_out_of_range = draw_raw_neighborhood(
                            ui.painter(),
                            neighborhood_rect,
                            loaded,
                            cursor,
                            settings.neighborhood,
                        );
                        ui.separator();
                        ui.allocate_ui_with_layout(
                            egui::vec2(
                                HOVER_CONTENT_SIZE.x - HOVER_NEIGHBORHOOD_SIZE.x - 16.0,
                                HOVER_CONTENT_SIZE.y,
                            ),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                render_hover_details(ui, loaded, cursor, has_out_of_range);
                            },
                        );
                    });
                });
        });
}

fn hover_view_position(pointer: egui::Pos2, viewport: egui::Rect) -> egui::Pos2 {
    let minimum = viewport.min + egui::vec2(HOVER_VIEWPORT_MARGIN, HOVER_VIEWPORT_MARGIN);
    let maximum = egui::pos2(
        (viewport.right() - HOVER_VIEWPORT_MARGIN - HOVER_VIEW_SIZE.x).max(minimum.x),
        (viewport.bottom() - HOVER_VIEWPORT_MARGIN - HOVER_VIEW_SIZE.y).max(minimum.y),
    );
    let mut x = pointer.x + HOVER_POINTER_GAP;
    let mut y = pointer.y + HOVER_POINTER_GAP;
    if x + HOVER_VIEW_SIZE.x > viewport.right() - HOVER_VIEWPORT_MARGIN {
        x = pointer.x - HOVER_POINTER_GAP - HOVER_VIEW_SIZE.x;
    }
    if y + HOVER_VIEW_SIZE.y > viewport.bottom() - HOVER_VIEWPORT_MARGIN {
        y = pointer.y - HOVER_POINTER_GAP - HOVER_VIEW_SIZE.y;
    }
    egui::pos2(x.clamp(minimum.x, maximum.x), y.clamp(minimum.y, maximum.y))
}

fn neighborhood_window(
    cursor: CursorPixel,
    neighborhood: HoverNeighborhood,
    width: u32,
    height: u32,
) -> NeighborhoodWindow {
    let size = i64::from(neighborhood.size());
    let (source_left, source_right, destination_left, destination_right) =
        clipped_axis_window(cursor.x, size, width);
    let (source_top, source_bottom, destination_top, destination_bottom) =
        clipped_axis_window(cursor.y, size, height);

    NeighborhoodWindow {
        source_min: [source_left, source_top],
        source_max: [source_right, source_bottom],
        destination_min: [destination_left, destination_top],
        destination_max: [destination_right, destination_bottom],
    }
}

fn clipped_axis_window(cursor: u32, size: i64, limit: u32) -> (u32, u32, u32, u32) {
    let requested_start = i64::from(cursor) - size / 2;
    let source_start = requested_start.clamp(0, i64::from(limit));
    let source_end = (requested_start + size).clamp(0, i64::from(limit));
    let destination_start = source_start - requested_start;
    let destination_end = destination_start + source_end - source_start;

    (
        u32::try_from(source_start).expect("clamped source start"),
        u32::try_from(source_end).expect("clamped source end"),
        u32::try_from(destination_start).expect("non-negative destination start"),
        u32::try_from(destination_end).expect("bounded destination end"),
    )
}

#[allow(clippy::cast_precision_loss)]
fn draw_raw_neighborhood(
    painter: &egui::Painter,
    rect: egui::Rect,
    loaded: &LoadedRaw,
    cursor: CursorPixel,
    neighborhood: HoverNeighborhood,
) -> bool {
    let size = neighborhood.size();
    let window = neighborhood_window(
        cursor,
        neighborhood,
        loaded.frame.spec.width,
        loaded.frame.spec.height,
    );
    let cell_size = rect.width() / size as f32;
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(12));

    let source_uv = egui::Rect::from_min_max(
        egui::pos2(
            window.source_min[0] as f32 / loaded.frame.spec.width as f32,
            window.source_min[1] as f32 / loaded.frame.spec.height as f32,
        ),
        egui::pos2(
            window.source_max[0] as f32 / loaded.frame.spec.width as f32,
            window.source_max[1] as f32 / loaded.frame.spec.height as f32,
        ),
    );
    let destination = egui::Rect::from_min_max(
        rect.min
            + egui::vec2(
                window.destination_min[0] as f32 * cell_size,
                window.destination_min[1] as f32 * cell_size,
            ),
        rect.min
            + egui::vec2(
                window.destination_max[0] as f32 * cell_size,
                window.destination_max[1] as f32 * cell_size,
            ),
    );
    painter.image(
        loaded.raw_texture_id(),
        destination,
        source_uv,
        egui::Color32::WHITE,
    );

    let grid_stroke = egui::Stroke::new(1.0, egui::Color32::from_black_alpha(110));
    for index in 1..size {
        let offset = index as f32 * cell_size;
        painter.line_segment(
            [
                egui::pos2(rect.left() + offset, rect.top()),
                egui::pos2(rect.left() + offset, rect.bottom()),
            ],
            grid_stroke,
        );
        painter.line_segment(
            [
                egui::pos2(rect.left(), rect.top() + offset),
                egui::pos2(rect.right(), rect.top() + offset),
            ],
            grid_stroke,
        );
    }

    let center_offset = (size / 2) as f32 * cell_size;
    let center_rect = egui::Rect::from_min_size(
        rect.min + egui::vec2(center_offset, center_offset),
        egui::vec2(cell_size, cell_size),
    )
    .shrink(1.0);
    let center_stroke = egui::Stroke::new(2.0, egui::Color32::YELLOW);
    painter.rect_stroke(center_rect, 0.0, center_stroke, egui::StrokeKind::Inside);
    painter.line_segment(
        [
            egui::pos2(center_rect.left(), center_rect.center().y),
            egui::pos2(center_rect.right(), center_rect.center().y),
        ],
        center_stroke,
    );
    painter.line_segment(
        [
            egui::pos2(center_rect.center().x, center_rect.top()),
            egui::pos2(center_rect.center().x, center_rect.bottom()),
        ],
        center_stroke,
    );
    painter.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0, egui::Color32::GRAY),
        egui::StrokeKind::Inside,
    );

    neighborhood_has_out_of_range(&loaded.frame, window)
}

fn neighborhood_has_out_of_range(frame: &RawFrame, window: NeighborhoodWindow) -> bool {
    let width = usize::try_from(frame.spec.width).expect("RAW width fits usize");
    let max_code = frame.spec.max_code_value();
    for y in window.source_min[1]..window.source_max[1] {
        let row = usize::try_from(y).expect("RAW y fits usize") * width;
        for x in window.source_min[0]..window.source_max[0] {
            let index = row + usize::try_from(x).expect("RAW x fits usize");
            if frame.pixels()[index] > max_code {
                return true;
            }
        }
    }
    false
}

fn render_hover_details(
    ui: &mut egui::Ui,
    loaded: &LoadedRaw,
    cursor: CursorPixel,
    has_out_of_range: bool,
) {
    ui.spacing_mut().item_spacing.y = 1.0;
    let site = loaded.inspection_bayer().site_at(cursor.x, cursor.y);
    let max_code = loaded.frame.spec.max_code_value();
    hover_detail_label(ui, format!("Pos  x={} y={}", cursor.x, cursor.y));
    hover_detail_label(ui, format!("CFA  {site:?}"));
    hover_detail_label(ui, format!("RAW  {}/{}", cursor.value, max_code));

    let rendered_pixel = loaded.rendered_pixel(cursor);
    if let Some(color) = rendered_pixel {
        hover_detail_label(
            ui,
            format!(
                "RGBf {:.3} {:.3} {:.3}",
                color.linear.r, color.linear.g, color.linear.b
            ),
        );
        ui.horizontal(|ui| {
            hover_detail_label(
                ui,
                format!("RGB8 {} {} {}", color.rgb8.r, color.rgb8.g, color.rgb8.b),
            );
            let (swatch, _) = ui.allocate_exact_size(egui::vec2(24.0, 16.0), egui::Sense::hover());
            ui.painter().rect_filled(
                swatch,
                2.0,
                egui::Color32::from_rgb(color.rgb8.r, color.rgb8.g, color.rgb8.b),
            );
        });
        if color.display_clipped_channels > 0 || color.missing_neighbor_channels > 0 {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "pixel clipped={} missing={}",
                        color.display_clipped_channels, color.missing_neighbor_channels
                    ))
                    .color(egui::Color32::YELLOW)
                    .small(),
                )
                .truncate(),
            );
        }
    }

    if let Some(error) = &loaded.color_edit.render_error {
        let prefix = if rendered_pixel.is_some() {
            "RGB stale: "
        } else {
            "RGB unavailable: "
        };
        ui.add(
            egui::Label::new(
                egui::RichText::new(format!("{prefix}{error}")).color(egui::Color32::RED),
            )
            .truncate(),
        );
    } else if loaded.installed_revision() != Some(loaded.color_edit.revision) {
        hover_detail_label(
            ui,
            if rendered_pixel.is_some() {
                "RGB updating".to_owned()
            } else {
                "RGB rendering".to_owned()
            },
        );
    }

    let inside_roi = roi_contains(loaded.roi, cursor);
    hover_detail_label(
        ui,
        format!(
            "ROI {} · {}–{}",
            if inside_roi { "in" } else { "out" },
            loaded.stats.min,
            loaded.stats.max
        ),
    );
    hover_detail_label(
        ui,
        format!(
            "μ={:.2} · sat {}/{}",
            loaded.stats.mean, loaded.stats.saturated_pixels, loaded.stats.total_pixels
        ),
    );
    if has_out_of_range {
        ui.colored_label(egui::Color32::MAGENTA, "MAGENTA = RAW > max");
    }
}

fn hover_detail_label(ui: &mut egui::Ui, text: String) {
    ui.add(egui::Label::new(egui::RichText::new(text).monospace()).truncate());
}

fn roi_contains(roi: Roi, cursor: CursorPixel) -> bool {
    cursor.x >= roi.x
        && cursor.y >= roi.y
        && cursor.x < roi.x.saturating_add(roi.width)
        && cursor.y < roi.y.saturating_add(roi.height)
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
    texture_id: TextureId,
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
        texture_id,
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
    fn hover_view_defaults_to_enabled_five_by_five() {
        let settings = HoverViewSettings::default();

        assert!(settings.enabled);
        assert_eq!(settings.neighborhood, HoverNeighborhood::Five);
        assert_eq!(HoverNeighborhood::ALL.len(), 3);
        assert_eq!(settings.neighborhood.size(), 5);
    }

    #[test]
    fn neighborhood_window_keeps_out_of_image_slots_blank() {
        let top_left = neighborhood_window(
            CursorPixel {
                x: 0,
                y: 0,
                value: 0,
            },
            HoverNeighborhood::Five,
            4,
            3,
        );
        assert_eq!(top_left.source_min, [0, 0]);
        assert_eq!(top_left.source_max, [3, 3]);
        assert_eq!(top_left.destination_min, [2, 2]);
        assert_eq!(top_left.destination_max, [5, 5]);

        let bottom_right = neighborhood_window(
            CursorPixel {
                x: 3,
                y: 2,
                value: 0,
            },
            HoverNeighborhood::Five,
            4,
            3,
        );
        assert_eq!(bottom_right.source_min, [1, 0]);
        assert_eq!(bottom_right.source_max, [4, 3]);
        assert_eq!(bottom_right.destination_min, [0, 0]);
        assert_eq!(bottom_right.destination_max, [3, 3]);
    }

    #[test]
    fn hover_view_position_flips_and_clamps_inside_viewport() {
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        assert_eq!(
            hover_view_position(egui::pos2(100.0, 100.0), viewport),
            egui::pos2(116.0, 116.0)
        );
        assert_eq!(
            hover_view_position(egui::pos2(790.0, 100.0), viewport),
            egui::pos2(326.0, 116.0)
        );
        assert_eq!(
            hover_view_position(egui::pos2(100.0, 590.0), viewport),
            egui::pos2(116.0, 390.0)
        );
        assert_eq!(
            hover_view_position(egui::pos2(790.0, 590.0), viewport),
            egui::pos2(326.0, 390.0)
        );

        let narrow = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 150.0));
        assert_eq!(
            hover_view_position(egui::pos2(150.0, 75.0), narrow),
            egui::pos2(8.0, 8.0)
        );
    }

    #[test]
    fn roi_membership_uses_half_open_bounds() {
        let roi = Roi {
            x: 10,
            y: 20,
            width: 3,
            height: 2,
        };
        assert!(roi_contains(
            roi,
            CursorPixel {
                x: 12,
                y: 21,
                value: 0,
            }
        ));
        assert!(!roi_contains(
            roi,
            CursorPixel {
                x: 13,
                y: 21,
                value: 0,
            }
        ));
    }

    #[test]
    fn neighborhood_reports_magenta_diagnostic_samples() {
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
        let window = neighborhood_window(
            CursorPixel {
                x: 1,
                y: 0,
                value: 42,
            },
            HoverNeighborhood::Three,
            2,
            1,
        );

        assert!(neighborhood_has_out_of_range(&frame, window));
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
