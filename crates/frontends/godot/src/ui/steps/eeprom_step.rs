//! Step 4 EEPROM 写入：固定 I²C bus 映射 + 读取状态 + 写入标定结果（强确认）。

use godot::classes::{Button, Control, HBoxContainer, Label, VBoxContainer};
use godot::prelude::*;

use crate::ui::theme;

/// Step 4 的控件句柄。
pub struct EepromStep {
    pub panel: Gd<Control>,
    pub inspect_button: Gd<Button>,
    pub write_button: Gd<Button>,
    pub status: Gd<Label>,
}

impl EepromStep {
    /// 构建 Step 4 面板。
    pub fn build() -> Self {
        let mut v = VBoxContainer::new_alloc();
        v.add_theme_constant_override("separation", 10);

        // 固定映射说明（只读，防误配）。
        let mut mapping = Label::new_alloc();
        mapping.set_text("写入目标：CH0 → i2c-4（左路内参） · CH3 → i2c-6（右路内参，待接入）");
        mapping.add_theme_font_size_override("font_size", 14);
        mapping.add_theme_color_override("font_color", theme::MUTED);
        v.add_child(&mapping);

        // 动作行。
        let mut row = HBoxContainer::new_alloc();
        row.add_theme_constant_override("separation", 8);
        let mut inspect_button = Button::new_alloc();
        inspect_button.set_text("读取当前状态");
        let mut write_button = Button::new_alloc();
        write_button.set_text("写入标定结果");
        write_button.set_disabled(true);
        row.add_child(&inspect_button);
        row.add_child(&write_button);
        v.add_child(&row);

        // 状态区。
        let mut status = Label::new_alloc();
        status.set_text("未读取");
        status.add_theme_font_size_override("font_size", 13);
        status.add_theme_color_override("font_color", theme::MUTED);
        status.set_autowrap_mode(godot::classes::text_server::AutowrapMode::WORD_SMART);
        v.add_child(&status);

        let panel: Gd<Control> = v.upcast();
        Self {
            panel,
            inspect_button,
            write_button,
            status,
        }
    }

    /// 写状态文本；成功绿、失败红。
    pub fn set_status(&mut self, text: &str, ok: bool) {
        self.status.set_text(text);
        self.status
            .add_theme_color_override("font_color", if ok { theme::OK } else { theme::ERR });
    }
}
