//! Step 1 连接设备：设备 IP / TCP 端口 / SSH 凭据 / 启动驱动 / 读取状态。

use godot::classes::{Button, Control, HBoxContainer, Label, LineEdit, VBoxContainer};
use godot::prelude::*;

/// Step 1 的控件句柄；由向导持有并驱动状态刷新。
pub struct ConnectStep {
    pub panel: Gd<Control>,
    pub device_ip: Gd<LineEdit>,
    pub ssh_user: Gd<LineEdit>,
    pub ssh_password: Gd<LineEdit>,
    pub status: Gd<Label>,
    pub probe_button: Gd<Button>,
    pub bootstrap_button: Gd<Button>,
}

impl ConnectStep {
    /// 构建 Step 1 面板（纯代码，不依赖编辑器）。
    pub fn build() -> Self {
        let mut v = VBoxContainer::new_alloc();
        v.add_theme_constant_override("separation", 10);

        // 输入行：设备 IP + 固定 TCP 端口 + 动作按钮。
        let mut row = HBoxContainer::new_alloc();
        row.add_theme_constant_override("separation", 8);

        let mut ip_label = Label::new_alloc();
        ip_label.set_text("设备 IP");
        ip_label.add_theme_font_size_override("font_size", 15);

        let mut ip = LineEdit::new_alloc();
        ip.set_placeholder("10.21.12.x");
        ip.set_text("10.21.12.");
        ip.set_custom_minimum_size(Vector2::new(220.0, 0.0));

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

        // SSH 凭据行：启动驱动使用（root/root 默认，板端常见出厂凭据）。
        let mut ssh_row = HBoxContainer::new_alloc();
        ssh_row.add_theme_constant_override("separation", 8);

        let mut user_label = Label::new_alloc();
        user_label.set_text("SSH 用户");
        user_label.add_theme_font_size_override("font_size", 15);

        let mut user = LineEdit::new_alloc();
        user.set_text("root");
        user.set_custom_minimum_size(Vector2::new(120.0, 0.0));

        let mut pass_label = Label::new_alloc();
        pass_label.set_text("SSH 密码");
        pass_label.add_theme_font_size_override("font_size", 15);

        let mut pass = LineEdit::new_alloc();
        pass.set_text("root");
        pass.set_secret(true);
        pass.set_custom_minimum_size(Vector2::new(160.0, 0.0));

        ssh_row.add_child(&user_label);
        ssh_row.add_child(&user);
        ssh_row.add_child(&pass_label);
        ssh_row.add_child(&pass);
        v.add_child(&ssh_row);

        // 状态行：probe / bootstrap 结果就地呈现。
        let mut status = Label::new_alloc();
        status.set_text("未连接");
        status.add_theme_font_size_override("font_size", 14);
        status.add_theme_color_override("font_color", crate::ui::theme::MUTED);
        v.add_child(&status);

        let panel: Gd<Control> = v.upcast();
        Self {
            panel,
            device_ip: ip,
            ssh_user: user,
            ssh_password: pass,
            status,
            probe_button: probe,
            bootstrap_button: bootstrap,
        }
    }

    /// 更新状态文本与颜色（就地错误提示，不弹窗）。
    pub fn set_status(&mut self, text: &str, color: godot::builtin::Color) {
        self.status.set_text(text);
        self.status.add_theme_color_override("font_color", color);
    }
}
