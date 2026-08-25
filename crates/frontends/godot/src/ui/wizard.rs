//! 向导：单列纵向布局 + 步骤指示器 + 步骤锁定状态机。
//!
//! 交互模型（已与用户确认）：
//! - 当前步骤展开，已完成步骤折叠为标题行，未完成步骤灰显；
//! - 前一步未完成时后一步锁定（面板隐藏）。

use godot::classes::{
    control::{LayoutPreset, SizeFlags},
    Control, HBoxContainer, HSeparator, Label, MarginContainer, PanelContainer, ScrollContainer,
    VBoxContainer,
};
use godot::prelude::*;

use crate::ui::steps::connect::ConnectStep;
use crate::ui::steps::{StepId, STEP_TITLES};
use crate::ui::theme;

/// 向导全部 UI 句柄与流程状态（纯 Rust，非 Godot 类）。
pub struct UiState {
    step_headers: Vec<Gd<Label>>,
    step_bodies: Vec<Gd<Control>>,
    step_summaries: Vec<Gd<Label>>,
    summaries: Vec<String>,
    completed: [bool; 5],
    active: StepId,
    pub connect: ConnectStep,
    pub status_bar: Gd<Label>,
}

impl UiState {
    /// 构建完整向导；返回状态与根控件（根控件需挂到场景树）。
    pub fn build() -> (Self, Gd<Control>) {
        let mut root = Control::new_alloc();
        root.set_anchors_preset(LayoutPreset::FULL_RECT);

        let mut margin = MarginContainer::new_alloc();
        margin.set_anchors_preset(LayoutPreset::FULL_RECT);
        for (side, px) in [
            ("margin_left", 16),
            ("margin_right", 16),
            ("margin_top", 12),
            ("margin_bottom", 12),
        ] {
            margin.add_theme_constant_override(side, px);
        }
        root.add_child(&margin);

        let mut outer = VBoxContainer::new_alloc();
        outer.add_theme_constant_override("separation", 10);
        margin.add_child(&outer);

        // 标题。
        let mut title = Label::new_alloc();
        title.set_text("pongbot-calib-tool · X5_233 标定");
        title.add_theme_font_size_override("font_size", 20);
        outer.add_child(&title);

        // 步骤指示器（5 个状态标签）。
        let mut indicator = HBoxContainer::new_alloc();
        indicator.add_theme_constant_override("separation", 18);
        let mut step_headers = Vec::new();
        for index in 0..5 {
            let mut label = Label::new_alloc();
            label.set_text(format!("{} {}", index + 1, STEP_TITLES[index]).as_str());
            label.add_theme_font_size_override("font_size", 15);
            indicator.add_child(&label);
            step_headers.push(label);
        }
        outer.add_child(&indicator);

        let separator = HSeparator::new_alloc();
        outer.add_child(&separator);

        // 步骤区：纵向滚动容器。
        let mut scroll = ScrollContainer::new_alloc();
        scroll.set_h_size_flags(SizeFlags::EXPAND_FILL);
        scroll.set_v_size_flags(SizeFlags::EXPAND_FILL);
        outer.add_child(&scroll);

        let mut body = VBoxContainer::new_alloc();
        body.set_h_size_flags(SizeFlags::EXPAND_FILL);
        body.add_theme_constant_override("separation", 10);
        scroll.add_child(&body);

        let mut step_bodies = Vec::new();
        let mut step_summaries = Vec::new();
        let mut connect: Option<ConnectStep> = None;
        for index in 0..5 {
            let mut panel = PanelContainer::new_alloc();
            let mut panel_v = VBoxContainer::new_alloc();
            panel_v.add_theme_constant_override("separation", 6);

            let mut header_row = HBoxContainer::new_alloc();
            header_row.add_theme_constant_override("separation", 10);
            let mut header = Label::new_alloc();
            header.set_text(format!("Step {} · {}", index + 1, STEP_TITLES[index]).as_str());
            header.add_theme_font_size_override("font_size", 16);
            let mut summary = Label::new_alloc();
            summary.add_theme_font_size_override("font_size", 13);
            summary.set_modulate(theme::MUTED);
            header_row.add_child(&header);
            header_row.add_child(&summary);
            panel_v.add_child(&header_row);
            step_summaries.push(summary);

            // 各步骤正文：Step 1 为连接面板，其余为占位说明。
            if index == 0 {
                let step = ConnectStep::build();
                panel_v.add_child(&step.panel);
                step_bodies.push(step.panel.clone());
                connect = Some(step);
            } else {
                let mut placeholder = Label::new_alloc();
                placeholder.set_text("（待实现）");
                placeholder.set_modulate(theme::MUTED);
                panel_v.add_child(&placeholder);
                step_bodies.push(placeholder.upcast());
            }

            panel.add_child(&panel_v);
            body.add_child(&panel);
        }
        let connect = connect.expect("Step 1 构建失败");

        // 底部状态栏。
        let mut status_bar = Label::new_alloc();
        status_bar.set_text("就绪");
        status_bar.set_modulate(theme::MUTED);
        status_bar.add_theme_font_size_override("font_size", 13);
        outer.add_child(&status_bar);

        let mut state = Self {
            step_headers,
            step_bodies,
            step_summaries,
            summaries: vec![String::new(); 5],
            completed: [false; 5],
            active: StepId::Connect,
            connect,
            status_bar,
        };
        state.refresh();

        (state, root)
    }

    /// 步骤进入完成态：写摘要、推进 active 到下一个未完成步骤。
    pub fn complete_step(&mut self, id: StepId, summary: impl Into<String>) {
        let index = id as usize;
        self.completed[index] = true;
        self.summaries[index] = summary.into();
        let mut next = None;
        for candidate in 0..5 {
            if !self.completed[candidate] {
                next = Some(StepId::from_index(candidate));
                break;
            }
        }
        self.active = next.unwrap_or(id);
        self.refresh();
    }

    /// 按当前状态刷新指示器、面板可见性与摘要。
    pub fn refresh(&mut self) {
        for index in 0..5 {
            let (text, color) = if self.completed[index] {
                (format!("{} {} ✓", index + 1, STEP_TITLES[index]), theme::OK)
            } else if index == self.active as usize {
                (
                    format!("▸ {} {}", index + 1, STEP_TITLES[index]),
                    theme::ACCENT,
                )
            } else {
                (
                    format!("{} {}", index + 1, STEP_TITLES[index]),
                    theme::MUTED,
                )
            };
            self.step_headers[index].set_text(text.as_str());
            self.step_headers[index].set_modulate(color);
            let is_active = index == self.active as usize;
            self.step_bodies[index].set_visible(is_active);
            self.step_summaries[index].set_text(self.summaries[index].as_str());
        }
    }
}
