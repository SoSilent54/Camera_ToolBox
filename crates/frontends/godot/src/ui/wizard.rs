//! 向导：单列纵向布局 + 步骤面板状态机。
//!
//! 交互模型（已与用户确认）：
//! - 当前步骤展开，已完成步骤折叠为标题行，未完成步骤灰显；
//! - 前一步未完成时后一步锁定（内容隐藏、面板半透明）；
//! - 步骤状态由面板自身表达（无独立顶部指示器，避免重复）；
//! - Step 2 双路预览与采集为同一阶段（引导可视化叠加在 viewer 上）。

use godot::classes::{
    control::{LayoutPreset, SizeFlags},
    Control, HBoxContainer, HSeparator, Label, MarginContainer, PanelContainer, ScrollContainer,
    VBoxContainer,
};
use godot::prelude::*;

use crate::ui::steps::connect::ConnectStep;
use crate::ui::steps::preview::PreviewStep;
use crate::ui::steps::{StepId, STEP_TITLES};
use crate::ui::theme;
use crate::ui::steps::solve_step::SolveStep;

/// 向导全部 UI 句柄与流程状态（纯 Rust，非 Godot 类）。
pub struct UiState {
    panels: Vec<Gd<PanelContainer>>,
    step_headers: Vec<Gd<Label>>,
    step_summaries: Vec<Gd<Label>>,
    step_bodies: Vec<Gd<Control>>,
    summaries: Vec<String>,
    completed: [bool; 4],
    active: StepId,
    pub connect: ConnectStep,
    pub preview: PreviewStep,
    pub solve: SolveStep,
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
            ("margin_left", 20),
            ("margin_right", 20),
            ("margin_top", 14),
            ("margin_bottom", 12),
        ] {
            margin.add_theme_constant_override(side, px);
        }
        root.add_child(&margin);

        let mut outer = VBoxContainer::new_alloc();
        outer.add_theme_constant_override("separation", 12);
        margin.add_child(&outer);

        // 标题。
        let mut title = Label::new_alloc();
        title.set_text("pongbot-calib-tool · X5_233 标定");
        title.add_theme_font_size_override("font_size", 22);
        title.add_theme_color_override("font_color", theme::TEXT);
        outer.add_child(&title);

        let separator = HSeparator::new_alloc();
        outer.add_child(&separator);

        // 步骤区：纵向滚动容器。
        let mut scroll = ScrollContainer::new_alloc();
        scroll.set_h_size_flags(SizeFlags::EXPAND_FILL);
        scroll.set_v_size_flags(SizeFlags::EXPAND_FILL);
        outer.add_child(&scroll);

        let mut body = VBoxContainer::new_alloc();
        body.set_h_size_flags(SizeFlags::EXPAND_FILL);
        body.add_theme_constant_override("separation", 12);
        scroll.add_child(&body);

        let mut panels = Vec::new();
        let mut step_headers = Vec::new();
        let mut step_summaries = Vec::new();
        let mut step_bodies = Vec::new();
        let mut connect: Option<ConnectStep> = None;
        let mut preview: Option<PreviewStep> = None;
        let mut solve: Option<SolveStep> = None;
        for index in 0..4 {
            let mut panel = PanelContainer::new_alloc();
            panel.add_theme_stylebox_override("panel", &theme::panel_style(None));
            let mut panel_v = VBoxContainer::new_alloc();
            panel_v.add_theme_constant_override("separation", 8);

            let mut header_row = HBoxContainer::new_alloc();
            header_row.add_theme_constant_override("separation", 10);
            let mut header = Label::new_alloc();
            header.set_text(format!("Step {} · {}", index + 1, STEP_TITLES[index]).as_str());
            header.add_theme_font_size_override("font_size", 15);
            header.add_theme_color_override("font_color", theme::MUTED);
            let mut summary = Label::new_alloc();
            summary.add_theme_font_size_override("font_size", 12);
            summary.add_theme_color_override("font_color", theme::MUTED);
            header_row.add_child(&header);
            header_row.add_child(&summary);
            panel_v.add_child(&header_row);
            step_headers.push(header);
            step_summaries.push(summary);

            // 各步骤正文：Step 1 连接面板、Step 2 双预览，其余占位。
            if index == 0 {
                let step = ConnectStep::build();
                panel_v.add_child(&step.panel);
                step_bodies.push(step.panel.clone());
                connect = Some(step);
            } else if index == 1 {
                let step = PreviewStep::build();
                panel_v.add_child(&step.panel);
                step_bodies.push(step.panel.clone());
                preview = Some(step);
            } else if index == 2 {
                let step = SolveStep::build();
                panel_v.add_child(&step.panel);
                step_bodies.push(step.panel.clone());
                solve = Some(step);
            } else {
                let mut placeholder = Label::new_alloc();
                placeholder.set_text("（待实现）");
                placeholder.add_theme_font_size_override("font_size", 14);
                placeholder.add_theme_color_override("font_color", theme::MUTED);
                panel_v.add_child(&placeholder);
                step_bodies.push(placeholder.upcast());
            }

            panel.add_child(&panel_v);
            body.add_child(&panel);
            panels.push(panel);
        }
        let connect = connect.expect("Step 1 构建失败");
        let preview = preview.expect("Step 2 构建失败");
        let solve = solve.expect("Step 3 构建失败");

        // 底部状态栏。
        let mut status_bar = Label::new_alloc();
        status_bar.set_text("就绪");
        status_bar.add_theme_font_size_override("font_size", 12);
        status_bar.add_theme_color_override("font_color", theme::MUTED);
        outer.add_child(&status_bar);

        let mut state = Self {
            panels,
            step_headers,
            step_summaries,
            step_bodies,
            summaries: vec![String::new(); 4],
            completed: [false; 4],
            active: StepId::Connect,
            connect,
            preview,
            solve,
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
        for candidate in 0..4 {
            if !self.completed[candidate] {
                next = Some(StepId::from_index(candidate));
                break;
            }
        }
        self.active = next.unwrap_or(id);
        self.refresh();
    }

    /// 按当前状态刷新面板：当前步高亮、完成步绿、锁定步灰显半透明。
    pub fn refresh(&mut self) {
        for index in 0..4 {
            let is_active = index == self.active as usize;
            let is_done = self.completed[index];
            let header_color = if is_done {
                theme::OK
            } else if is_active {
                theme::ACCENT
            } else {
                theme::MUTED
            };
            let prefix = if is_done {
                "✓"
            } else if is_active {
                "▸"
            } else {
                "·"
            };
            self.step_headers[index].set_text(
                format!("{prefix} Step {} · {}", index + 1, STEP_TITLES[index]).as_str(),
            );
            self.step_headers[index].add_theme_color_override("font_color", header_color);
            self.step_summaries[index].set_text(self.summaries[index].as_str());
            self.step_bodies[index].set_visible(is_active);

            // 面板边框：当前步强调色；内容半透明：锁定步。
            if is_active {
                self.panels[index]
                    .add_theme_stylebox_override("panel", &theme::panel_style(Some(theme::ACCENT)));
            } else {
                self.panels[index]
                    .add_theme_stylebox_override("panel", &theme::panel_style(None));
            }
            self.panels[index].set_modulate(if is_active || is_done {
                Color::from_rgba(1.0, 1.0, 1.0, 1.0)
            } else {
                Color::from_rgba(1.0, 1.0, 1.0, 0.55)
            });
        }
    }
}
