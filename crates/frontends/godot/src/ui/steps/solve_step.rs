//! Step 3 求解检查：棋盘参数 + 标定结果几何摘要 + 单图重投影误差柱状图。

use godot::classes::text_server::AutowrapMode;
use godot::classes::{Button, Control, HBoxContainer, IControl, Label, SpinBox, VBoxContainer};
use godot::prelude::*;

use crate::ui::theme;

/// 单路每张图重投影 RMSE 柱状图。
#[derive(GodotClass)]
#[class(init, base = Control)]
pub struct ReprojectionBarChart {
    base: Base<Control>,
    values: Vec<f64>,
    limit: f64,
}

impl ReprojectionBarChart {
    /// 更新柱状图数据；单位为 pixel RMSE。
    pub fn set_values(&mut self, values: Vec<f64>, limit: f64) {
        self.values = values;
        self.limit = limit.max(0.1);
        self.base_mut().queue_redraw();
    }

    pub fn clear_values(&mut self) {
        self.values.clear();
        self.base_mut().queue_redraw();
    }
}

#[godot_api]
impl IControl for ReprojectionBarChart {
    fn draw(&mut self) {
        let rect = self.base().get_rect();
        let size = rect.size;
        let origin = Vector2::new(8.0, size.y - 18.0);
        let chart_w = (size.x - 16.0).max(1.0);
        let chart_h = (size.y - 28.0).max(1.0);
        let axis = Color::from_rgba(1.0, 1.0, 1.0, 0.22);
        self.base_mut().draw_line(
            Vector2::new(8.0, origin.y),
            Vector2::new(size.x - 8.0, origin.y),
            axis,
        );
        self.base_mut()
            .draw_line(Vector2::new(8.0, origin.y), Vector2::new(8.0, 8.0), axis);

        if self.values.is_empty() {
            return;
        }
        let values = self.values.clone();
        let max_value = reprojection_axis_max(&values);
        let n = values.len() as f32;
        let slot = chart_w / n;
        let bar_w = (slot * 0.90).clamp(2.0, 24.0);
        for (index, value) in values.into_iter().enumerate() {
            let value = value.max(0.0);
            let ratio = (value / max_value).clamp(0.0, 1.0) as f32;
            let h = chart_h * ratio;
            let x = 8.0 + index as f32 * slot + (slot - bar_w) * 0.5;
            let y = origin.y - h;
            let color = if value <= self.limit {
                Color::from_rgba(0.22, 0.92, 0.48, 0.90)
            } else {
                Color::from_rgba(1.0, 0.46, 0.36, 0.95)
            };
            self.base_mut().draw_rect(
                Rect2 {
                    position: Vector2::new(x, y),
                    size: Vector2::new(bar_w, h.max(1.0)),
                },
                color,
            );
        }

        let limit_y = origin.y - chart_h * (self.limit / max_value).clamp(0.0, 1.0) as f32;
        self.base_mut().draw_line(
            Vector2::new(8.0, limit_y),
            Vector2::new(size.x - 8.0, limit_y),
            Color::from_rgba(1.0, 0.76, 0.18, 0.70),
        );
    }
}

fn reprojection_axis_max(values: &[f64]) -> f64 {
    values.iter().copied().fold(0.0_f64, f64::max).max(0.1)
}
/// Step 3 的控件句柄。
pub struct SolveStep {
    pub panel: Gd<Control>,
    pub board_cols: Gd<SpinBox>,
    pub board_rows: Gd<SpinBox>,
    pub square_mm: Gd<SpinBox>,
    pub solve_button: Gd<Button>,
    pub ch0_result: Gd<Label>,
    pub ch3_result: Gd<Label>,
    pub ch0_chart: Gd<ReprojectionBarChart>,
    pub ch3_chart: Gd<ReprojectionBarChart>,
}

impl SolveStep {
    /// 构建 Step 3 面板。
    pub fn build() -> Self {
        let mut v = VBoxContainer::new_alloc();
        v.add_theme_constant_override("separation", 10);

        // 棋盘参数行。
        let mut row = HBoxContainer::new_alloc();
        row.add_theme_constant_override("separation", 8);

        let mut cols_label = Label::new_alloc();
        cols_label.set_text("棋盘内角点");
        cols_label.add_theme_font_size_override("font_size", 14);
        let mut cols = SpinBox::new_alloc();
        cols.set_value(11.0);
        cols.set_min(3.0);
        cols.set_max(64.0);
        cols.set_custom_minimum_size(Vector2::new(70.0, 0.0));

        let mut cross = Label::new_alloc();
        cross.set_text("×");
        cross.add_theme_font_size_override("font_size", 14);

        let mut rows = SpinBox::new_alloc();
        rows.set_value(8.0);
        rows.set_min(3.0);
        rows.set_max(64.0);
        rows.set_custom_minimum_size(Vector2::new(70.0, 0.0));

        let mut mm_label = Label::new_alloc();
        mm_label.set_text("格子边长 (mm)");
        mm_label.add_theme_font_size_override("font_size", 14);

        let mut square = SpinBox::new_alloc();
        square.set_value(40.0);
        square.set_min(0.5);
        square.set_max(100.0);
        square.set_custom_minimum_size(Vector2::new(80.0, 0.0));

        let mut solve_button = Button::new_alloc();
        solve_button.set_text("执行标定");

        row.add_child(&cols_label);
        row.add_child(&cols);
        row.add_child(&cross);
        row.add_child(&rows);
        row.add_child(&mm_label);
        row.add_child(&square);
        row.add_child(&solve_button);
        v.add_child(&row);

        // 结果区（双路）。
        let mut ch0_result = Label::new_alloc();
        ch0_result.set_text("CH0：待求解");
        ch0_result.add_theme_font_size_override("font_size", 14);
        ch0_result.add_theme_color_override("font_color", theme::MUTED);
        ch0_result.set_autowrap_mode(AutowrapMode::WORD_SMART);
        v.add_child(&ch0_result);
        let mut ch0_chart = ReprojectionBarChart::new_alloc();
        ch0_chart.set_custom_minimum_size(Vector2::new(0.0, 96.0));
        v.add_child(&ch0_chart);

        let mut ch3_result = Label::new_alloc();
        ch3_result.set_text("CH3：待求解");
        ch3_result.add_theme_font_size_override("font_size", 14);
        ch3_result.add_theme_color_override("font_color", theme::MUTED);
        ch3_result.set_autowrap_mode(AutowrapMode::WORD_SMART);
        v.add_child(&ch3_result);
        let mut ch3_chart = ReprojectionBarChart::new_alloc();
        ch3_chart.set_custom_minimum_size(Vector2::new(0.0, 96.0));
        v.add_child(&ch3_chart);

        let panel: Gd<Control> = v.upcast();
        Self {
            panel,
            board_cols: cols,
            board_rows: rows,
            square_mm: square,
            solve_button,
            ch0_result,
            ch3_result,
            ch0_chart,
            ch3_chart,
        }
    }

    /// 写单路结果；成功绿、失败红。
    pub fn set_result(&mut self, label: Gd<Label>, text: &str) {
        let ok = !text.contains("失败");
        let mut label = label;
        label.set_text(text);
        label.add_theme_color_override("font_color", if ok { theme::OK } else { theme::ERR });
    }

    /// 更新单路单图 RMSE 柱状图。
    pub fn set_chart(&mut self, mut chart: Gd<ReprojectionBarChart>, values: Vec<f64>, limit: f64) {
        chart.bind_mut().set_values(values, limit);
    }

    pub fn clear_charts(&mut self) {
        self.ch0_chart.bind_mut().clear_values();
        self.ch3_chart.bind_mut().clear_values();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reprojection_axis_uses_max_view_error_not_limit() {
        assert_eq!(reprojection_axis_max(&[0.12, 0.35, 0.21]), 0.35);
        assert_eq!(reprojection_axis_max(&[]), 0.1);
    }
}
