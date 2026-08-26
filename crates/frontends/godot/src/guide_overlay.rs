//! GuideOverlay：RTSP viewer 上的引导可视化层（自定义绘制）。
//!
//! worker 线程（guided 采集）把检测结果写入共享槽；主线程 `draw` 读取并绘制：
//! - 检测到棋盘：角点网格（绿色 polyline，图像 → 控件坐标映射）+ 中心标记
//! - 未检测到：不绘制（引导文本由上层 Label 承担）

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

/// 一帧检测 / 目标引导的绘制数据（worker → 主线程）。
#[derive(Clone, Default)]
pub struct OverlayData {
    pub found: bool,
    /// 图像像素坐标的角点（行优先，与 OpenCV 一致）。
    pub corners: Vec<(f32, f32)>,
    /// 图像尺寸（坐标映射用）。
    pub image_width: f32,
    pub image_height: f32,
    /// 姿态（rvec 转角度，度）：俯仰/偏航/翻滚近似。
    pub rotation_deg: (f32, f32, f32),
    /// 当前目标姿态的投影中心 / 外框 / 网格。
    pub target_center_uv: Option<[f32; 2]>,
    pub target_outline_uv: Option<[[f32; 2]; 4]>,
    pub target_grid_lines: Vec<OverlayGridLine>,
    pub target_matched: bool,
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

        // 先画原版 guide 目标框：未匹配黄色，匹配绿色。
        let target_color = if data.target_matched {
            Color::from_rgba(0.31, 0.90, 0.47, 0.95)
        } else {
            Color::from_rgba(1.0, 0.82, 0.31, 0.95)
        };
        let grid_color = if data.target_matched {
            Color::from_rgba(0.31, 0.90, 0.47, 0.45)
        } else {
            Color::from_rgba(1.0, 0.82, 0.31, 0.42)
        };
        for line in &data.target_grid_lines {
            self.base_mut().draw_line(uv_to_view(line.start_uv), uv_to_view(line.end_uv), grid_color);
        }
        if let Some(outline) = data.target_outline_uv {
            for i in 0..4 {
                self.base_mut().draw_line(uv_to_view(outline[i]), uv_to_view(outline[(i + 1) % 4]), target_color);
            }
        }
        if let Some(center) = data.target_center_uv {
            let c = uv_to_view(center);
            self.base_mut().draw_line(c + Vector2::new(-10.0, 0.0), c + Vector2::new(10.0, 0.0), target_color);
            self.base_mut().draw_line(c + Vector2::new(0.0, -10.0), c + Vector2::new(0.0, 10.0), target_color);
        }

        if !data.found || data.corners.is_empty() {
            return;
        }
        let mut points = PackedVector2Array::new();
        for (x, y) in &data.corners {
            points.push(px_to_view(*x, *y));
        }
        self.base_mut()
            .draw_polyline(&points, Color::from_rgba(0.26, 0.82, 0.48, 0.9));

        // 当前检测包围盒中心标记。
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;
        for (x, y) in &data.corners {
            min_x = min_x.min(*x);
            min_y = min_y.min(*y);
            max_x = max_x.max(*x);
            max_y = max_y.max(*y);
        }
        let center = px_to_view((min_x + max_x) * 0.5, (min_y + max_y) * 0.5);
        self.base_mut()
            .draw_circle(center, 4.0, Color::from_rgba(1.0, 0.71, 0.33, 1.0));
    }
}
