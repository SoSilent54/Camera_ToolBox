//! GuideOverlay：RTSP viewer 上的引导可视化层（自定义绘制）。
//!
//! worker 线程（guided 采集）把检测结果写入共享槽；主线程 `draw` 读取并绘制：
//! - 检测到棋盘：角点网格（绿色 polyline，图像 → 控件坐标映射）+ 中心标记
//! - 未检测到：不绘制（引导文本由上层 Label 承担）

use godot::classes::{Control, IControl};
use godot::prelude::*;
use std::sync::{Arc, Mutex};

/// 一帧检测的绘制数据（worker → 主线程）。
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
        if !data.found || data.corners.is_empty() {
            return;
        }
        let size = self.base().get_size();
        if size.x <= 0.0 || size.y <= 0.0 || data.image_width <= 0.0 || data.image_height <= 0.0 {
            return;
        }
        // 图像保持宽高比居中映射到控件。
        let scale = (size.x / data.image_width).min(size.y / data.image_height);
        let offset_x = (size.x - data.image_width * scale) / 2.0;
        let offset_y = (size.y - data.image_height * scale) / 2.0;

        let mut points = PackedVector2Array::new();
        for (x, y) in &data.corners {
            points.push(Vector2::new(offset_x + x * scale, offset_y + y * scale));
        }
        self.base_mut()
            .draw_polyline(&points, Color::from_rgba(0.26, 0.82, 0.48, 0.9));

        // 棋盘包围盒中心标记。
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
        let center = Vector2::new(
            offset_x + (min_x + max_x) / 2.0 * scale,
            offset_y + (min_y + max_y) / 2.0 * scale,
        );
        self.base_mut()
            .draw_circle(center, 4.0, Color::from_rgba(1.0, 0.71, 0.33, 1.0));
    }
}
