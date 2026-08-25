//! Step 2 双路预览与采集：CH0/CH3 并排 viewer + overlay 引导可视化层。
//!
//! 每路卡片 = 标题行（通道/RTSP/I²C 固定映射 + 采集按钮）+ 预览区
//! （TextureRect 显示 RTSP 帧，GuideOverlay 绘制检测框/姿态引导，
//! overlay Label 显示 hold 计数与引导文本）+ 状态行。

use godot::classes::control::{LayoutPreset, MouseFilter};
use godot::classes::texture_rect::{ExpandMode, StretchMode};
use godot::classes::{Button, Control, HBoxContainer, Label, TextureRect, VBoxContainer};
use godot::prelude::*;

use crate::guide_overlay::GuideOverlay;
use crate::ui::theme;
use std::sync::{Arc, Mutex};

/// 单路 viewer 卡片。
pub struct ViewerCard {
    /// 卡片根容器（VBox），挂到 Step 2 面板。
    pub root: Gd<Control>,
    /// RTSP 帧显示。
    pub texture_rect: Gd<TextureRect>,
    /// 引导绘制层（检测框/姿态）。
    pub guide_overlay: Gd<GuideOverlay>,
    /// overlay 内左上角状态文本（hold 计数/引导文本）。
    pub overlay_label: Gd<Label>,
    /// 卡片下方状态行（连接/解码状态）。
    pub status: Gd<Label>,
    /// 采集开关按钮（开始/停止自动采集）。
    pub capture_button: Gd<Button>,
    /// 预览容器（宽度铺满，高度按 16:9 由上层同步）。
    pub view: Gd<Control>,
}

impl ViewerCard {
    /// 构建单路卡片；`title` 形如 "CH0 · RTSP 554 · i2c-4"。
    fn build(title: &str, overlay_slot: Arc<Mutex<Option<crate::guide_overlay::OverlayData>>>) -> Self {
        let mut card = VBoxContainer::new_alloc();
        card.add_theme_constant_override("separation", 6);
        card.set_h_size_flags(godot::classes::control::SizeFlags::EXPAND_FILL);

        // 标题行：通道信息 + 采集按钮。
        let mut header_row = HBoxContainer::new_alloc();
        header_row.add_theme_constant_override("separation", 10);
        let mut header = Label::new_alloc();
        header.set_text(title);
        header.add_theme_font_size_override("font_size", 13);
        header.add_theme_color_override("font_color", theme::ACCENT);
        header.set_h_size_flags(godot::classes::control::SizeFlags::EXPAND_FILL);
        let mut capture_button = Button::new_alloc();
        capture_button.set_text("开始采集");
        capture_button.set_custom_minimum_size(Vector2::new(96.0, 0.0));
        header_row.add_child(&header);
        header_row.add_child(&capture_button);
        card.add_child(&header_row);

        // 预览区：定高容器，TextureRect 铺满 + overlay 叠上层。
        let mut view = Control::new_alloc();
        view.set_custom_minimum_size(Vector2::new(560.0, 315.0));
        view.set_h_size_flags(godot::classes::control::SizeFlags::EXPAND_FILL);

        let mut texture_rect = TextureRect::new_alloc();
        texture_rect.set_anchors_preset(LayoutPreset::FULL_RECT);
        texture_rect.set_stretch_mode(StretchMode::KEEP_ASPECT_CENTERED);
        texture_rect.set_expand_mode(ExpandMode::IGNORE_SIZE);
        view.add_child(&texture_rect);

        // overlay 层：GuideOverlay（绘制）+ Label（hold/引导文本）。
        let mut overlay = Control::new_alloc();
        overlay.set_anchors_preset(LayoutPreset::FULL_RECT);
        overlay.set_mouse_filter(MouseFilter::IGNORE);

        let mut guide_overlay = GuideOverlay::new_alloc();
        guide_overlay.set_anchors_preset(LayoutPreset::FULL_RECT);
        guide_overlay.set_mouse_filter(MouseFilter::IGNORE);
        guide_overlay.bind_mut().attach(overlay_slot);
        overlay.add_child(&guide_overlay);

        let mut overlay_label = Label::new_alloc();
        overlay_label.set_text("未连接");
        overlay_label.add_theme_font_size_override("font_size", 14);
        overlay_label.add_theme_color_override("font_color", theme::WARN);
        overlay_label.set_position(Vector2::new(8.0, 6.0));
        overlay.add_child(&overlay_label);
        view.add_child(&overlay);

        card.add_child(&view);

        let mut status = Label::new_alloc();
        status.set_text("未连接");
        status.add_theme_font_size_override("font_size", 12);
        status.add_theme_color_override("font_color", theme::MUTED);
        card.add_child(&status);

        Self {
            root: card.upcast(),
            texture_rect,
            guide_overlay,
            overlay_label,
            status,
            capture_button,
            view,
        }
    }

    /// 更新 overlay 状态文本（引导可视化主入口）。
    pub fn set_overlay(&mut self, text: &str, color: godot::builtin::Color) {
        self.overlay_label.set_text(text);
        self.overlay_label.add_theme_color_override("font_color", color);
    }

    /// 更新卡片状态行。
    pub fn set_status(&mut self, text: &str) {
        self.status.set_text(text);
    }
}

/// Step 2 的控件句柄。
pub struct PreviewStep {
    pub panel: Gd<Control>,
    pub ch0: ViewerCard,
    pub ch3: ViewerCard,
}

impl PreviewStep {
    /// 构建 Step 2 面板（双路并排）；`ch0_slot`/`ch3_slot` 为检测绘制数据槽。
    pub fn build(
        ch0_slot: Arc<Mutex<Option<crate::guide_overlay::OverlayData>>>,
        ch3_slot: Arc<Mutex<Option<crate::guide_overlay::OverlayData>>>,
    ) -> Self {
        let mut v = VBoxContainer::new_alloc();
        v.add_theme_constant_override("separation", 10);

        let mut hint = Label::new_alloc();
        hint.set_text("双路预览：引导信息叠加在画面上，hold 计数达标自动采集。");
        hint.add_theme_font_size_override("font_size", 13);
        hint.add_theme_color_override("font_color", theme::MUTED);
        v.add_child(&hint);

        let mut row = HBoxContainer::new_alloc();
        row.add_theme_constant_override("separation", 12);

        let ch0 = ViewerCard::build("CH0 · RTSP 554 · i2c-4", ch0_slot);
        let ch3 = ViewerCard::build("CH3 · RTSP 557 · i2c-6", ch3_slot);
        row.add_child(&ch0.root);
        row.add_child(&ch3.root);
        v.add_child(&row);

        let panel: Gd<Control> = v.upcast();
        Self { panel, ch0, ch3 }
    }
}
