//! 深色主题常量与中文字体安装。
//!
//! Godot 默认字体不含 CJK 字形；通过 `SystemFont` 按 family 加载系统
//! 字体并设为全局 fallback，跨平台无需硬编码字体文件路径。

use godot::classes::{SystemFont, ThemeDb};
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

/// 背景底色。
pub const BG: Color = Color::from_rgba(0.078, 0.086, 0.106, 1.0);
/// 面板底色。
pub const PANEL: Color = Color::from_rgba(0.114, 0.125, 0.157, 1.0);
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
