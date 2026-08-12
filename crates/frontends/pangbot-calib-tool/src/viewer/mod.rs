//! RAW Viewer 显示、缩放、hover 与 minimap。

use std::sync::Arc;

use camera_toolbox_core::{NativeImage, Roi};
use eframe::egui;

use crate::{
    color_controls::DisplayMode,
    histogram_link::{HistogramBinSelection, SpatialHighlight},
};

mod hover;
mod interaction;
mod model;

pub(crate) use hover::{HoverNeighborhood, HoverViewSettings};
pub(crate) use model::{LoadedRaw, bayer_label, pixel_inspection_texture_options};

use hover::{render_hover_view, render_native_hover_view};
use interaction::{
    RoiGesture, draw_minimap, draw_roi_draft, draw_roi_overlay, hover_pixel, image_extent_f32,
    image_pixel_from_screen, update_roi_interaction,
};

#[cfg(test)]
use hover::{
    hover_view_position, neighborhood_has_out_of_range, neighborhood_window, roi_contains,
};
#[cfg(test)]
use interaction::{
    ImagePixel, finish_roi_gesture, roi_drag_pointer_position, roi_from_inclusive_pixels,
};
#[cfg(test)]
use model::preview_with_diagnostics;

const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 64.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CursorPixel {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) raw_value: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewerAction {
    CommitRoi(Roi),
    ResetRoi,
    ClickPixel { x: u32, y: u32 },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ViewerOutput {
    pub(crate) rect: egui::Rect,
    pub(crate) action: Option<ViewerAction>,
}

/// Viewer 只消费原生图像语义、当前显示纹理与 ROI；RAW 专属详情通过可选上下文保留。
pub(crate) struct ViewerImage<'a> {
    pub(crate) generation: u64,
    pub(crate) native: NativeImage,
    pub(crate) texture_id: egui::TextureId,
    pub(crate) roi: Roi,
    pub(crate) show_roi: bool,
    raw: Option<&'a LoadedRaw>,
}

impl<'a> ViewerImage<'a> {
    pub(crate) fn raw(loaded: &'a LoadedRaw, display_mode: DisplayMode) -> Self {
        Self {
            generation: loaded.generation,
            native: NativeImage::Raw(Arc::clone(&loaded.frame)),
            texture_id: loaded.active_texture_id(display_mode),
            roi: loaded.roi,
            show_roi: true,
            raw: Some(loaded),
        }
    }

    pub(crate) fn native(
        generation: u64,
        native: NativeImage,
        texture_id: egui::TextureId,
        roi: Roi,
    ) -> Self {
        Self {
            generation,
            native,
            texture_id,
            roi,
            show_roi: true,
            raw: None,
        }
    }

    pub(crate) fn native_without_roi(
        generation: u64,
        native: NativeImage,
        texture_id: egui::TextureId,
    ) -> Self {
        let dimensions = native.dimensions();
        Self {
            generation,
            native,
            texture_id,
            roi: Roi {
                x: 0,
                y: 0,
                width: dimensions[0],
                height: dimensions[1],
            },
            show_roi: false,
            raw: None,
        }
    }
}

#[derive(Clone, Copy)]
struct ViewerGeometry {
    viewer_rect: egui::Rect,
    image_rect: egui::Rect,
}

struct SpatialOverlayTexture {
    selection: HistogramBinSelection,
    texture: egui::TextureHandle,
}

pub(crate) struct ImageViewerState {
    pub(crate) zoom: f32,
    pan: egui::Vec2,
    pub(crate) fit_on_next_frame: bool,
    pub(crate) cursor: Option<CursorPixel>,
    pub(crate) horizontal_flip: bool,
    roi_gesture: Option<RoiGesture>,
    geometry: Option<ViewerGeometry>,
    spatial_overlay: Option<SpatialOverlayTexture>,
}

impl Default for ImageViewerState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            fit_on_next_frame: true,
            cursor: None,
            horizontal_flip: false,
            roi_gesture: None,
            geometry: None,
            spatial_overlay: None,
        }
    }
}

impl ImageViewerState {
    pub(crate) fn displayed_image_rect(&self) -> Option<egui::Rect> {
        self.geometry.map(|geometry| geometry.image_rect)
    }

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

    /// 在面板布局前按上一帧稳定几何刷新 cursor，避免曲线 marker 滞后一帧。
    pub(crate) fn refresh_cursor(&mut self, context: &egui::Context, image: &NativeImage) {
        let position = context.pointer_latest_pos();
        self.cursor = self.geometry.and_then(|geometry| {
            position
                .filter(|position| geometry.viewer_rect.contains(*position))
                .and_then(|position| {
                    hover_pixel(geometry.image_rect, position, image, self.horizontal_flip)
                })
        });
    }

    /// 释放可重建 overlay texture，不改变 zoom/pan/ROI 交互状态。
    pub(crate) fn evict_derived_resources(&mut self) {
        self.spatial_overlay = None;
    }

    fn sync_spatial_overlay(
        &mut self,
        context: &egui::Context,
        generation: u64,
        dimensions: [u32; 2],
        highlight: Option<&SpatialHighlight>,
    ) -> Option<egui::TextureId> {
        let Some(highlight) = highlight.filter(|highlight| {
            highlight.selection.key.generation == generation
                && highlight.mask.width == dimensions[0]
                && highlight.mask.height == dimensions[1]
        }) else {
            self.spatial_overlay = None;
            return None;
        };
        let Some(overlay_image) = highlight.overlay_image.as_ref() else {
            self.spatial_overlay = None;
            return None;
        };
        let current = self
            .spatial_overlay
            .as_ref()
            .is_some_and(|overlay| overlay.selection == highlight.selection);
        if !current {
            let texture = context.load_texture(
                "histogram-spatial-highlight",
                Arc::clone(overlay_image),
                egui::TextureOptions::NEAREST,
            );
            self.spatial_overlay = Some(SpatialOverlayTexture {
                selection: highlight.selection,
                texture,
            });
        }
        self.spatial_overlay
            .as_ref()
            .map(|overlay| overlay.texture.id())
    }
}

pub(crate) fn viewer_texture_uv(horizontal_flip: bool) -> egui::Rect {
    if horizontal_flip {
        egui::Rect::from_min_max(egui::pos2(1.0, 0.0), egui::pos2(0.0, 1.0))
    } else {
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0))
    }
}

fn render_viewer_toolbar(ui: &mut egui::Ui, viewer: &mut ImageViewerState, image_available: bool) {
    ui.horizontal_wrapped(|ui| {
        ui.monospace(format!("{:.0}%", viewer.zoom * 100.0));
        if ui
            .add_enabled(image_available, egui::Button::new("Fit"))
            .on_hover_text("Fit image to the Viewer window.")
            .clicked()
        {
            viewer.fit_on_next_frame = true;
        }
        ui.checkbox(&mut viewer.horizontal_flip, "Flip X")
            .on_hover_text("Mirror the displayed image and all Viewer overlays horizontally.");
    });
}

pub(crate) fn render_viewer(
    ui: &mut egui::Ui,
    image: Option<ViewerImage<'_>>,
    viewer: &mut ImageViewerState,
    hover_settings: HoverViewSettings,
    spatial_highlight: Option<&SpatialHighlight>,
) -> ViewerOutput {
    render_viewer_toolbar(ui, viewer, image.is_some());
    let available = ui.available_size().max(egui::vec2(1.0, 1.0));
    let (viewer_rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
    let painter = ui.painter_at(viewer_rect);
    painter.rect_filled(viewer_rect, 0.0, egui::Color32::from_gray(22));

    let Some(image) = image else {
        painter.text(
            viewer_rect.center(),
            egui::Align2::CENTER_CENTER,
            "File -> Open...",
            egui::TextStyle::Heading.resolve(ui.style()),
            egui::Color32::GRAY,
        );
        viewer.cursor = None;
        viewer.roi_gesture = None;
        viewer.geometry = None;
        viewer.spatial_overlay = None;
        return ViewerOutput {
            rect: viewer_rect,
            action: None,
        };
    };
    let dimensions = image.native.dimensions();

    if viewer.fit_on_next_frame {
        viewer.fit_to_rect(
            viewer_rect,
            image_extent_f32(dimensions[0]),
            image_extent_f32(dimensions[1]),
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
        image_extent_f32(dimensions[0]),
        image_extent_f32(dimensions[1]),
    );
    viewer.geometry = Some(ViewerGeometry {
        viewer_rect,
        image_rect,
    });
    let mut action = if image.show_roi {
        update_roi_interaction(ui, &response, image_rect, viewer, dimensions)
    } else {
        viewer.roi_gesture = None;
        None
    };
    if action.is_none()
        && response.clicked_by(egui::PointerButton::Primary)
        && let Some(position) = response.interact_pointer_pos()
        && let Some(pixel) = image_pixel_from_screen(
            image_rect,
            position,
            dimensions[0],
            dimensions[1],
            false,
            viewer.horizontal_flip,
        )
    {
        action = Some(ViewerAction::ClickPixel {
            x: pixel.x,
            y: pixel.y,
        });
    }
    let spatial_texture_id =
        viewer.sync_spatial_overlay(ui.ctx(), image.generation, dimensions, spatial_highlight);
    let texture_uv = viewer_texture_uv(viewer.horizontal_flip);
    painter.image(
        image.texture_id,
        image_rect,
        texture_uv,
        egui::Color32::WHITE,
    );
    if let Some(texture_id) = spatial_texture_id {
        painter.image(texture_id, image_rect, texture_uv, egui::Color32::WHITE);
    }
    if image.show_roi {
        draw_roi_overlay(
            &painter,
            image_rect,
            dimensions,
            image.roi,
            if viewer
                .roi_gesture
                .is_some_and(|gesture| gesture.drag_started)
            {
                egui::Color32::from_rgba_unmultiplied(0, 255, 0, 120)
            } else {
                egui::Color32::GREEN
            },
            viewer.horizontal_flip,
        );
    }
    if image.show_roi
        && let Some(gesture) = viewer.roi_gesture.filter(|gesture| gesture.drag_started)
    {
        draw_roi_draft(
            &painter,
            image_rect,
            dimensions,
            gesture,
            viewer.horizontal_flip,
        );
    }
    draw_minimap(
        &painter,
        viewer_rect,
        image_rect,
        dimensions,
        image.texture_id,
        viewer.horizontal_flip,
    );

    let hover_position = response.hover_pos();
    viewer.cursor = hover_position.and_then(|position| {
        hover_pixel(image_rect, position, &image.native, viewer.horizontal_flip)
    });
    if hover_settings.enabled
        && !viewer
            .roi_gesture
            .is_some_and(|gesture| gesture.drag_started)
        && let (Some(position), Some(cursor)) = (hover_position, viewer.cursor)
    {
        if let Some(loaded) = image.raw {
            render_hover_view(ui.ctx(), position, cursor, loaded, hover_settings);
        } else {
            render_native_hover_view(
                ui.ctx(),
                position,
                cursor,
                &image.native,
                image.roi,
                hover_settings,
            );
        }
    }
    ViewerOutput {
        rect: viewer_rect,
        action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_core::{BayerPattern, RawFrame, RawSpec};

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
                raw_value: None,
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
                raw_value: None,
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
    fn screen_pixel_mapping_handles_zoom_pan_edges_and_clamping() {
        let rect = egui::Rect::from_min_max(egui::pos2(100.0, 50.0), egui::pos2(500.0, 250.0));
        assert_eq!(
            image_pixel_from_screen(rect, egui::pos2(100.0, 50.0), 4, 2, false, false),
            Some(ImagePixel { x: 0, y: 0 })
        );
        assert_eq!(
            image_pixel_from_screen(rect, egui::pos2(499.9, 249.9), 4, 2, false, false),
            Some(ImagePixel { x: 3, y: 1 })
        );
        assert_eq!(
            image_pixel_from_screen(rect, egui::pos2(600.0, 300.0), 4, 2, false, false),
            None
        );
        assert_eq!(
            image_pixel_from_screen(rect, egui::pos2(600.0, 300.0), 4, 2, true, false),
            Some(ImagePixel { x: 3, y: 1 })
        );
        assert_eq!(
            image_pixel_from_screen(rect, egui::pos2(100.0, 50.0), 4, 2, false, true),
            Some(ImagePixel { x: 3, y: 0 })
        );
        assert_eq!(
            image_pixel_from_screen(rect, egui::pos2(499.9, 249.9), 4, 2, false, true),
            Some(ImagePixel { x: 0, y: 1 })
        );
        assert_eq!(
            viewer_texture_uv(false),
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0))
        );
        assert_eq!(
            viewer_texture_uv(true),
            egui::Rect::from_min_max(egui::pos2(1.0, 0.0), egui::pos2(0.0, 1.0))
        );
    }

    #[test]
    fn screen_pixel_mapping_uses_viewer_zoom_pan_and_flip_geometry() {
        let viewer_rect =
            egui::Rect::from_min_size(egui::pos2(20.0, 10.0), egui::vec2(400.0, 260.0));
        let mut viewer = ImageViewerState::default();
        viewer.fit_to_rect(viewer_rect, 100.0, 50.0);
        viewer.zoom_by(2.0, Some(viewer_rect.center()), viewer_rect);
        viewer.horizontal_flip = true;
        let image_rect = viewer.image_rect(viewer_rect, 100.0, 50.0);

        let native = image_pixel_from_screen(
            image_rect,
            image_rect.left_top()
                + egui::vec2(image_rect.width() * 0.25, image_rect.height() * 0.5),
            100,
            50,
            false,
            viewer.horizontal_flip,
        );

        assert_eq!(native, Some(ImagePixel { x: 75, y: 25 }));
    }

    #[test]
    fn roi_drag_uses_latest_pointer_position_when_interaction_position_is_lost() {
        let latest = egui::pos2(600.0, 300.0);
        let rect = egui::Rect::from_min_max(egui::pos2(100.0, 50.0), egui::pos2(500.0, 250.0));

        assert_eq!(
            roi_drag_pointer_position(None, Some(latest))
                .and_then(|position| image_pixel_from_screen(rect, position, 4, 2, true, false)),
            Some(ImagePixel { x: 3, y: 1 })
        );
    }

    #[test]
    fn right_click_resets_roi_while_drag_commits_selection() {
        let anchor = ImagePixel { x: 8, y: 5 };
        let current = ImagePixel { x: 3, y: 2 };

        assert_eq!(
            finish_roi_gesture(RoiGesture {
                anchor,
                current,
                drag_started: false,
            }),
            ViewerAction::ResetRoi
        );
        assert_eq!(
            finish_roi_gesture(RoiGesture {
                anchor,
                current,
                drag_started: true,
            }),
            ViewerAction::CommitRoi(Roi {
                x: 3,
                y: 2,
                width: 6,
                height: 4,
            })
        );
    }

    #[test]
    fn inclusive_roi_mapping_normalizes_reverse_and_single_pixel_drags() {
        assert_eq!(
            roi_from_inclusive_pixels(ImagePixel { x: 8, y: 5 }, ImagePixel { x: 3, y: 2 }),
            Roi {
                x: 3,
                y: 2,
                width: 6,
                height: 4,
            }
        );
        assert_eq!(
            roi_from_inclusive_pixels(ImagePixel { x: 7, y: 9 }, ImagePixel { x: 7, y: 9 }),
            Roi {
                x: 7,
                y: 9,
                width: 1,
                height: 1,
            }
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
                raw_value: None,
            }
        ));
        assert!(!roi_contains(
            roi,
            CursorPixel {
                x: 13,
                y: 21,
                raw_value: None,
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
                raw_value: Some(42),
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
        let image = NativeImage::Raw(Arc::new(test_frame()));
        let hovered = hover_pixel(rect, egui::pos2(15.0, 15.0), &image, false).unwrap();

        assert_eq!((hovered.x, hovered.y, hovered.raw_value), (1, 1, Some(256)));
        let flipped = hover_pixel(rect, egui::pos2(15.0, 15.0), &image, true).unwrap();
        assert_eq!((flipped.x, flipped.y, flipped.raw_value), (0, 1, Some(512)));
    }

    #[test]
    fn texture_filters_smooth_minification_and_preserve_pixel_magnification() {
        let options = pixel_inspection_texture_options();

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
