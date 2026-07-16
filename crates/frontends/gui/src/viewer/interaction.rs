//! Viewer 坐标映射、ROI 手势、ROI overlay 与 minimap。

use camera_toolbox_core::{NativeImage, NativePixelSample, Roi};
use eframe::egui::{self, TextureId};

use super::{CursorPixel, ImageViewerState, ViewerAction};

const MINIMAP_MAX_SIZE: egui::Vec2 = egui::vec2(180.0, 130.0);
const MINIMAP_MARGIN: f32 = 12.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ImagePixel {
    pub(super) x: u32,
    pub(super) y: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RoiGesture {
    pub(super) anchor: ImagePixel,
    pub(super) current: ImagePixel,
    pub(super) drag_started: bool,
}

pub(super) fn update_roi_interaction(
    ui: &egui::Ui,
    response: &egui::Response,
    image_rect: egui::Rect,
    viewer: &mut ImageViewerState,
    dimensions: [u32; 2],
) -> Option<ViewerAction> {
    let [width, height] = dimensions;
    let (secondary_pressed, secondary_released, pointer_position, cancel) = ui.input(|input| {
        (
            input.pointer.button_pressed(egui::PointerButton::Secondary),
            input
                .pointer
                .button_released(egui::PointerButton::Secondary),
            roi_drag_pointer_position(input.pointer.interact_pos(), input.pointer.latest_pos()),
            input.key_pressed(egui::Key::Escape),
        )
    });

    if cancel {
        viewer.roi_gesture = None;
        return None;
    }

    if secondary_pressed
        && response.hovered()
        && let Some(position) = pointer_position
        && let Some(pixel) = image_pixel_from_screen(image_rect, position, width, height, false)
    {
        viewer.roi_gesture = Some(RoiGesture {
            anchor: pixel,
            current: pixel,
            drag_started: false,
        });
    }

    if response.drag_started_by(egui::PointerButton::Secondary)
        && let Some(gesture) = &mut viewer.roi_gesture
    {
        gesture.drag_started = true;
    }
    if let (Some(gesture), Some(position)) = (&mut viewer.roi_gesture, pointer_position)
        && let Some(pixel) = image_pixel_from_screen(image_rect, position, width, height, true)
    {
        gesture.current = pixel;
    }

    if secondary_released {
        return viewer.roi_gesture.take().map(finish_roi_gesture);
    }
    None
}

pub(super) const fn finish_roi_gesture(gesture: RoiGesture) -> ViewerAction {
    if gesture.drag_started {
        ViewerAction::CommitRoi(roi_from_inclusive_pixels(gesture.anchor, gesture.current))
    } else {
        ViewerAction::ResetRoi
    }
}

/// 拖拽离开原生窗口时，`interact_pos` 可能不可用；保留最新位置以便在图像边缘提交。
pub(super) const fn roi_drag_pointer_position(
    interact_position: Option<egui::Pos2>,
    latest_position: Option<egui::Pos2>,
) -> Option<egui::Pos2> {
    match interact_position {
        Some(position) => Some(position),
        None => latest_position,
    }
}

/// `egui` 几何统一使用 `f32`；图像维度经过 `RawSpec` 验证后再进入显示空间。
#[allow(clippy::cast_precision_loss)]
pub(super) fn image_extent_f32(value: u32) -> f32 {
    value as f32
}

/// 归一化坐标已限制到 [0, 1]，转换后再夹到有效像素闭区间。
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn normalized_to_pixel(normalized: f32, extent: u32) -> u32 {
    ((normalized * image_extent_f32(extent)).floor() as u32).min(extent - 1)
}

pub(super) fn image_pixel_from_screen(
    image_rect: egui::Rect,
    position: egui::Pos2,
    image_width: u32,
    image_height: u32,
    clamp: bool,
) -> Option<ImagePixel> {
    if image_width == 0
        || image_height == 0
        || image_rect.width() <= 0.0
        || image_rect.height() <= 0.0
        || (!clamp && !image_rect.contains(position))
    {
        return None;
    }
    let normalized_x = ((position.x - image_rect.left()) / image_rect.width()).clamp(0.0, 1.0);
    let normalized_y = ((position.y - image_rect.top()) / image_rect.height()).clamp(0.0, 1.0);
    let x = normalized_to_pixel(normalized_x, image_width);
    let y = normalized_to_pixel(normalized_y, image_height);
    Some(ImagePixel { x, y })
}

pub(super) const fn roi_from_inclusive_pixels(anchor: ImagePixel, current: ImagePixel) -> Roi {
    let x0 = if anchor.x < current.x {
        anchor.x
    } else {
        current.x
    };
    let y0 = if anchor.y < current.y {
        anchor.y
    } else {
        current.y
    };
    let x1 = if anchor.x > current.x {
        anchor.x
    } else {
        current.x
    };
    let y1 = if anchor.y > current.y {
        anchor.y
    } else {
        current.y
    };
    Roi {
        x: x0,
        y: y0,
        width: x1 - x0 + 1,
        height: y1 - y0 + 1,
    }
}

pub(super) fn hover_pixel(
    image_rect: egui::Rect,
    position: egui::Pos2,
    image: &NativeImage,
) -> Option<CursorPixel> {
    let [width, height] = image.dimensions();
    let pixel = image_pixel_from_screen(image_rect, position, width, height, false)?;
    let raw_value = match image.sample_at(pixel.x, pixel.y)? {
        NativePixelSample::Raw { value, .. } => Some(value),
        NativePixelSample::Rgba { .. } | NativePixelSample::Yuv { .. } => None,
    };
    Some(CursorPixel {
        x: pixel.x,
        y: pixel.y,
        raw_value,
    })
}

fn roi_screen_rect(image_rect: egui::Rect, dimensions: [u32; 2], roi: Roi) -> Option<egui::Rect> {
    let [width, height] = dimensions;
    let roi = roi.clamped_to(width, height)?;
    let scale_x = image_rect.width() / image_extent_f32(width);
    let scale_y = image_rect.height() / image_extent_f32(height);
    Some(egui::Rect::from_min_max(
        egui::pos2(
            image_rect.left() + image_extent_f32(roi.x) * scale_x,
            image_rect.top() + image_extent_f32(roi.y) * scale_y,
        ),
        egui::pos2(
            image_rect.left() + image_extent_f32(roi.x.saturating_add(roi.width)) * scale_x,
            image_rect.top() + image_extent_f32(roi.y.saturating_add(roi.height)) * scale_y,
        ),
    ))
}

pub(super) fn draw_roi_overlay(
    painter: &egui::Painter,
    image_rect: egui::Rect,
    dimensions: [u32; 2],
    roi: Roi,
    color: egui::Color32,
) {
    let Some(rect) = roi_screen_rect(image_rect, dimensions, roi) else {
        return;
    };
    painter.rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(1.5, color),
        egui::StrokeKind::Outside,
    );
}

pub(super) fn draw_roi_draft(
    painter: &egui::Painter,
    image_rect: egui::Rect,
    dimensions: [u32; 2],
    gesture: RoiGesture,
) {
    let roi = roi_from_inclusive_pixels(gesture.anchor, gesture.current);
    let Some(rect) = roi_screen_rect(image_rect, dimensions, roi) else {
        return;
    };
    painter.rect_filled(
        rect,
        0.0,
        egui::Color32::from_rgba_unmultiplied(255, 210, 0, 28),
    );
    painter.rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(2.0, egui::Color32::YELLOW),
        egui::StrokeKind::Outside,
    );
    painter.text(
        rect.left_top() + egui::vec2(4.0, 4.0),
        egui::Align2::LEFT_TOP,
        format!("{},{} · {}×{}", roi.x, roi.y, roi.width, roi.height),
        egui::FontId::monospace(12.0),
        egui::Color32::YELLOW,
    );
}

pub(super) fn draw_minimap(
    painter: &egui::Painter,
    viewer_rect: egui::Rect,
    image_rect: egui::Rect,
    dimensions: [u32; 2],
    texture_id: TextureId,
) {
    let image_width = image_extent_f32(dimensions[0]);
    let image_height = image_extent_f32(dimensions[1]);
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
