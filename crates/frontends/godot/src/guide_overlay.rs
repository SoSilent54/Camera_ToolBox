//! GuideOverlay：RTSP viewer 上的引导可视化层（自定义绘制）。
//!
//! worker 线程（guided 采集）把检测结果写入共享槽；主线程 `draw` 读取并绘制：
//! - 目标棋盘外框/网格：黄色表示未对齐，绿色表示进入 hold。
//! - 检测结果：只画检测棋盘外框和中心，不画内角点连线，避免遮挡棋盘。
//! - 姿态误差：对齐原版的 roll/pitch/yaw 圆弧误差环，不绘制背景文字面板。

use godot::classes::{Control, IControl};
use godot::prelude::*;
use std::sync::{Arc, Mutex};

/// 目标引导网格线（UV 坐标，0..1）。
#[derive(Clone, Default, Debug, PartialEq)]
pub struct OverlayGridLine {
    pub start_uv: [f32; 2],
    pub end_uv: [f32; 2],
}

/// guide hold 状态面板。
#[derive(Clone, Default, Debug, PartialEq)]
pub struct OverlayStatus {
    pub hold_frames: u8,
    pub hold_target: u8,
    pub detail_label: String,
    pub detail_value: f64,
    pub detail_limit: f64,
    pub matched: bool,
}

/// 单轴旋转误差弧：base 是参考环，arc 是当前误差扫角。
#[derive(Clone, Default, Debug, PartialEq)]
pub struct OverlayRotationArc {
    pub base_uv: Vec<[f32; 2]>,
    pub arc_uv: Vec<[f32; 2]>,
    pub tick_uv: [f32; 2],
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct OverlayRotationRings {
    pub center_uv: [f32; 2],
    pub roll: OverlayRotationArc,
    pub pitch: OverlayRotationArc,
    pub yaw: OverlayRotationArc,
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct OverlayPoseArrow {
    pub start_uv: [f32; 2],
    pub end_uv: [f32; 2],
}

/// 一帧检测 / 目标引导的绘制数据（worker → 主线程）。
#[derive(Clone, Default)]
pub struct OverlayData {
    /// 图像尺寸（坐标映射用）。
    pub image_width: f32,
    pub image_height: f32,
    /// 检测棋盘外框（图像像素坐标）：只画外框，不画内角点连线。
    pub detected_outline_px: Option<[[f32; 2]; 4]>,
    /// 当前目标姿态的投影中心 / 外框 / 网格。
    pub target_center_uv: Option<[f32; 2]>,
    pub target_outline_uv: Option<[[f32; 2]; 4]>,
    pub target_grid_lines: Vec<OverlayGridLine>,
    pub target_matched: bool,
    pub rotation_rings: Option<OverlayRotationRings>,
    pub pose_arrow: Option<OverlayPoseArrow>,
    pub status: Option<OverlayStatus>,
}

/// 覆盖在 TextureRect 上的透明绘制层。
#[derive(GodotClass)]
#[class(init, base = Control)]
pub struct GuideOverlay {
    base: Base<Control>,
    /// 检测数据共享槽（worker 写，draw 读）。
    data: Arc<Mutex<Option<OverlayData>>>,
}

impl GuideOverlay {
    /// 绑定数据槽（构建后由上层调用一次）。
    pub fn attach(&mut self, data: Arc<Mutex<Option<OverlayData>>>) {
        self.data = data;
    }

    /// 读取当前检测数据（主线程）。
    pub fn read(&self) -> Option<OverlayData> {
        self.data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[godot_api]
impl IControl for GuideOverlay {
    fn draw(&mut self) {
        let Some(data) = self.read() else {
            return;
        };
        let size = self.base().get_size();
        if size.x <= 0.0 || size.y <= 0.0 || data.image_width <= 0.0 || data.image_height <= 0.0 {
            return;
        }
        // 图像保持宽高比居中映射到控件。
        let scale = (size.x / data.image_width).min(size.y / data.image_height);
        let offset_x = (size.x - data.image_width * scale) / 2.0;
        let offset_y = (size.y - data.image_height * scale) / 2.0;
        let uv_to_view = |uv: [f32; 2]| -> Vector2 {
            Vector2::new(
                offset_x + uv[0] * data.image_width * scale,
                offset_y + uv[1] * data.image_height * scale,
            )
        };
        let px_to_view = |x: f32, y: f32| -> Vector2 {
            Vector2::new(offset_x + x * scale, offset_y + y * scale)
        };

        // 目标框：黄 = 继续对齐；绿 = 已匹配/hold。保留网格但降低透明度。
        let target_color = if data.target_matched {
            Color::from_rgba(0.22, 0.92, 0.48, 0.96)
        } else {
            Color::from_rgba(1.0, 0.76, 0.18, 0.96)
        };
        let target_grid_color = if data.target_matched {
            Color::from_rgba(0.22, 0.92, 0.48, 0.22)
        } else {
            Color::from_rgba(1.0, 0.76, 0.18, 0.20)
        };
        for line in &data.target_grid_lines {
            self.base_mut()
                .draw_line_ex(
                    uv_to_view(line.start_uv),
                    uv_to_view(line.end_uv),
                    target_grid_color,
                )
                .width(1.0)
                .antialiased(true)
                .done();
        }
        if let Some(outline) = data.target_outline_uv {
            for i in 0..4 {
                self.base_mut()
                    .draw_line_ex(
                        uv_to_view(outline[i]),
                        uv_to_view(outline[(i + 1) % 4]),
                        target_color,
                    )
                    .width(2.0)
                    .antialiased(true)
                    .done();
            }
        }

        // 检测结果只画外框和中心，不再画 88 个内角点的折线。
        if let Some(outline) = data.detected_outline_px {
            let detected_color = Color::from_rgba(0.18, 0.90, 0.46, 0.98);
            for i in 0..4 {
                self.base_mut()
                    .draw_line_ex(
                        px_to_view(outline[i][0], outline[i][1]),
                        px_to_view(outline[(i + 1) % 4][0], outline[(i + 1) % 4][1]),
                        detected_color,
                    )
                    .width(2.4)
                    .antialiased(true)
                    .done();
            }
            for corner in outline {
                self.base_mut()
                    .draw_circle(px_to_view(corner[0], corner[1]), 3.0, detected_color);
            }
        }

        if let Some(arrow) = data.pose_arrow {
            self.base_mut()
                .draw_line_ex(
                    uv_to_view(arrow.start_uv),
                    uv_to_view(arrow.end_uv),
                    Color::from_rgba(0.42, 0.68, 1.0, 0.86),
                )
                .width(2.0)
                .antialiased(true)
                .done();
        }

        if let Some(rings) = &data.rotation_rings {
            draw_rotation_arc(
                self,
                &rings.roll,
                Color::from_rgba(1.0, 0.58, 0.20, 0.95),
                &uv_to_view,
            );
            draw_rotation_arc(
                self,
                &rings.pitch,
                Color::from_rgba(0.30, 0.78, 1.0, 0.95),
                &uv_to_view,
            );
            draw_rotation_arc(
                self,
                &rings.yaw,
                Color::from_rgba(0.92, 0.42, 1.0, 0.95),
                &uv_to_view,
            );
            self.base_mut().draw_circle(
                uv_to_view(rings.center_uv),
                4.0,
                Color::from_rgba(1.0, 1.0, 1.0, 0.92),
            );
        }

        // hold 进度用 3 个小点表达，不加背景文字面板。
        if let (Some(status), Some(center)) = (&data.status, data.target_center_uv) {
            let center = uv_to_view(center);
            let total = status.hold_target.max(1);
            let filled = status.hold_frames.min(total);
            let start_x = center.x - (f32::from(total.saturating_sub(1)) * 12.0) * 0.5;
            for index in 0..total {
                let color = if index < filled {
                    Color::from_rgba(0.22, 0.92, 0.48, 0.95)
                } else if status.matched {
                    Color::from_rgba(1.0, 0.76, 0.18, 0.82)
                } else {
                    Color::from_rgba(1.0, 1.0, 1.0, 0.38)
                };
                self.base_mut().draw_circle(
                    Vector2::new(start_x + f32::from(index) * 12.0, center.y - 22.0),
                    4.0,
                    color,
                );
            }
        }
    }
}

fn draw_rotation_arc(
    overlay: &mut GuideOverlay,
    arc: &OverlayRotationArc,
    color: Color,
    uv_to_view: &impl Fn([f32; 2]) -> Vector2,
) {
    draw_uv_polyline(
        overlay,
        &arc.base_uv,
        Color::from_rgba(1.0, 1.0, 1.0, 0.28),
        1.2,
        uv_to_view,
    );
    draw_uv_polyline(overlay, &arc.arc_uv, color, 3.0, uv_to_view);
    overlay
        .base_mut()
        .draw_circle(uv_to_view(arc.tick_uv), 3.0, color);
}

fn draw_uv_polyline(
    overlay: &mut GuideOverlay,
    points: &[[f32; 2]],
    color: Color,
    width: f32,
    uv_to_view: &impl Fn([f32; 2]) -> Vector2,
) {
    if points.len() < 2 {
        return;
    }
    let mut packed = PackedVector2Array::new();
    for point in points {
        packed.push(uv_to_view(*point));
    }
    overlay
        .base_mut()
        .draw_polyline_ex(&packed, color)
        .width(width)
        .antialiased(true)
        .done();
}
