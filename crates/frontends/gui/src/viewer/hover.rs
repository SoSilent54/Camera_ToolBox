//! RAW Viewer 的像素 Hover、邻域可视化与详情面板。

use camera_toolbox_core::{RawFrame, Roi};
use eframe::egui;

use super::{CursorPixel, LoadedRaw};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct NeighborhoodWindow {
    pub(super) source_min: [u32; 2],
    pub(super) source_max: [u32; 2],
    pub(super) destination_min: [u32; 2],
    pub(super) destination_max: [u32; 2],
}

pub(super) fn render_hover_view(
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

pub(super) fn hover_view_position(pointer: egui::Pos2, viewport: egui::Rect) -> egui::Pos2 {
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

pub(super) fn neighborhood_window(
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

pub(super) fn neighborhood_has_out_of_range(frame: &RawFrame, window: NeighborhoodWindow) -> bool {
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
    if let Some(stats) = loaded.stats {
        hover_detail_label(
            ui,
            format!(
                "ROI {} · {}–{}",
                if inside_roi { "in" } else { "out" },
                stats.min,
                stats.max
            ),
        );
        hover_detail_label(
            ui,
            format!(
                "μ={:.2} · at/above max {}/{}",
                stats.mean, stats.saturated_pixels, stats.total_pixels
            ),
        );
    } else {
        hover_detail_label(
            ui,
            format!(
                "ROI {} · analysis pending",
                if inside_roi { "in" } else { "out" }
            ),
        );
    }
    if has_out_of_range {
        ui.colored_label(egui::Color32::MAGENTA, "MAGENTA = RAW > max");
    }
}

fn hover_detail_label(ui: &mut egui::Ui, text: String) {
    ui.add(egui::Label::new(egui::RichText::new(text).monospace()).truncate());
}

pub(super) fn roi_contains(roi: Roi, cursor: CursorPixel) -> bool {
    cursor.x >= roi.x
        && cursor.y >= roi.y
        && cursor.x < roi.x.saturating_add(roi.width)
        && cursor.y < roi.y.saturating_add(roi.height)
}
