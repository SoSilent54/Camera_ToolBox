//! Step 1 连接设备：设备 IP / TCP 端口 / 启动驱动 / 读取状态。

use godot::classes::{Button, Control, HBoxContainer, Label, LineEdit, VBoxContainer};
use godot::prelude::*;

/// Step 1 的控件句柄；由向导持有并驱动状态刷新。
pub struct ConnectStep {
    pub panel: Gd<Control>,
    pub device_ip: Gd<LineEdit>,
    pub status: Gd<Label>,
    pub probe_button: Gd<Button>,
    pub bootstrap_button: Gd<Button>,
}

impl ConnectStep {
    /// 构建 Step 1 面板（纯代码，不依赖编辑器）。
    pub fn build() -> Self {
        let mut v = VBoxContainer::new_alloc();
        v.add_theme_constant_override("separation", 8);

        // 输入行：设备 IP + 固定 TCP 端口 + 动作按钮。
        let mut row = HBoxContainer::new_alloc();
        row.add_theme_constant_override("separation", 8);

        let mut ip_label = Label::new_alloc();
        ip_label.set_text("设备 IP");
        ip_label.add_theme_font_size_override("font_size", 15);

        let mut ip = LineEdit::new_alloc();
        ip.set_placeholder("192.168.1.100");
        ip.set_custom_minimum_size(Vector2::new(240.0, 0.0));

        let mut port_label = Label::new_alloc();
        port_label.set_text("TCP 9073");
        port_label.add_theme_font_size_override("font_size", 15);

        let mut bootstrap = Button::new_alloc();
        bootstrap.set_text("启动驱动");

        let mut probe = Button::new_alloc();
        probe.set_text("读取状态");

        row.add_child(&ip_label);
        row.add_child(&ip);
        row.add_child(&port_label);
        row.add_child(&bootstrap);
        row.add_child(&probe);
        v.add_child(&row);

        // 状态行：probe / bootstrap 结果就地呈现。
        let mut status = Label::new_alloc();
        status.set_text("未连接");
        status.set_modulate(crate::ui::theme::MUTED);
        status.add_theme_font_size_override("font_size", 14);
        v.add_child(&status);

        let panel: Gd<Control> = v.upcast();
        Self {
            panel,
            device_ip: ip,
            status,
            probe_button: probe,
            bootstrap_button: bootstrap,
        }
    }

    /// 更新状态文本与颜色（就地错误提示，不弹窗）。
    pub fn set_status(&mut self, text: &str, color: godot::builtin::Color) {
        self.status.set_text(text);
        self.status.set_modulate(color);
    }
}
