//! 深色主题：颜色常量、中文字体与控件样式工厂。
//!
//! Godot 默认字体不含 CJK 字形；通过 `SystemFont` 按 family 加载系统
//! 字体并设为全局 fallback，跨平台无需硬编码字体文件路径。

use godot::classes::{RenderingServer, StyleBoxFlat, SystemFont, ThemeDb};
use godot::prelude::*;

/// 加载系统中文字体为全局 fallback（Noto Sans CJK → 文泉驿 → 兜底 sans）。
pub fn install_cjk_font() {
    let mut font = SystemFont::new_gd();
    let mut names = PackedStringArray::new();
    for family in [
        "Noto Sans CJK SC",
        "Noto Sans CJK JP",
        "WenQuanYi Micro Hei",
        "sans-serif",
    ] {
        names.push(family);
    }
    font.set_font_names(&names);
    ThemeDb::singleton().set_fallback_font(&font);
}

/// 窗口清屏色与主题背景一致（避免默认灰色）。
pub fn install_window_background() {
    RenderingServer::singleton().set_default_clear_color(BG);
}

/// 面板样式：深色圆角，可选强调边框（用于当前步骤）。
pub fn panel_style(border: Option<Color>) -> Gd<StyleBoxFlat> {
    let mut sb = StyleBoxFlat::new_gd();
    sb.set_bg_color(PANEL);
    sb.set_corner_radius_all(8);
    sb.set_content_margin_all(14.0);
    match border {
        Some(color) => {
            sb.set_border_width_all(2);
            sb.set_border_color(color);
        }
        None => {
            sb.set_border_width_all(1);
            sb.set_border_color(BORDER);
        }
    }
    sb
}

/// 背景底色。
pub const BG: Color = Color::from_rgba(0.078, 0.086, 0.106, 1.0);
/// 面板底色。
pub const PANEL: Color = Color::from_rgba(0.122, 0.133, 0.165, 1.0);
/// 面板边框。
pub const BORDER: Color = Color::from_rgba(0.20, 0.22, 0.27, 1.0);
/// 强调色（当前步骤 / 主按钮）。
pub const ACCENT: Color = Color::from_rgba(0.29, 0.64, 1.0, 1.0);
/// 主文本。
pub const TEXT: Color = Color::from_rgba(0.91, 0.92, 0.94, 1.0);
/// 次要文本 / 锁定步骤。
pub const MUTED: Color = Color::from_rgba(0.54, 0.58, 0.65, 1.0);
/// 成功。
pub const OK: Color = Color::from_rgba(0.26, 0.82, 0.48, 1.0);
/// 警告。
pub const WARN: Color = Color::from_rgba(1.0, 0.71, 0.33, 1.0);
/// 错误。
pub const ERR: Color = Color::from_rgba(1.0, 0.42, 0.42, 1.0);
