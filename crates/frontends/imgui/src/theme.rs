//! ImGui 字体加载与状态/overlay 颜色。

use imgui::{FontConfig, FontGlyphRanges, FontSource};
use std::path::{Path, PathBuf};

pub const ACCENT: [f32; 4] = [0.32, 0.70, 1.0, 1.0];
pub const MUTED: [f32; 4] = [0.66, 0.70, 0.78, 1.0];
pub const OK: [f32; 4] = [0.30, 0.88, 0.54, 1.0];
pub const WARN: [f32; 4] = [1.0, 0.76, 0.38, 1.0];
pub const ERR: [f32; 4] = [1.0, 0.48, 0.48, 1.0];

/// 加载中文字体；找不到 CJK 字体时保留默认字体并记录风险。
///
/// 搜索顺序：bundle 内 `fonts/`（随发布包分发）→ 各平台系统字体
/// （macOS PingFang / Windows 微软雅黑 / Linux Noto·文泉驿）。
pub fn install_fonts(ctx: &mut imgui::Context) {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // bundle 字体：可执行文件同目录 fonts/（发布流水线随包分发，OFL 可再分发）。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent().map(|dir| dir.join("fonts")) {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                let mut fonts: Vec<PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_file())
                    .collect();
                fonts.sort();
                candidates.extend(fonts);
            }
        }
    }
    candidates.extend([
        // macOS 系统字体（随系统分发，仅本机使用）。
        PathBuf::from("/System/Library/Fonts/PingFang.ttc"),
        PathBuf::from("/System/Library/Fonts/Hiragino Sans GB.ttc"),
        PathBuf::from("/System/Library/Fonts/STHeiti Light.ttc"),
        PathBuf::from("/System/Library/Fonts/Supplemental/Songti.ttc"),
        // Windows 系统字体。
        PathBuf::from("C:\\Windows\\Fonts\\msyh.ttc"),
        PathBuf::from("C:\\Windows\\Fonts\\msyh.ttf"),
        PathBuf::from("C:\\Windows\\Fonts\\simhei.ttf"),
        PathBuf::from("C:\\Windows\\Fonts\\simsun.ttc"),
        // Linux 系统字体。
        PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
        PathBuf::from("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc"),
        PathBuf::from("/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf"),
    ]);
    let cjk_font = candidates.into_iter().find(|path| path.is_file());

    match cjk_font.and_then(|path| std::fs::read(&path).ok()) {
        Some(bytes) => {
            ctx.fonts().add_font(&[FontSource::TtfData {
                data: Box::leak(bytes.into_boxed_slice()),
                size_pixels: 18.0,
                config: Some(FontConfig {
                    glyph_ranges: FontGlyphRanges::chinese_full(),
                    ..FontConfig::default()
                }),
            }]);
        }
        None => {
            tracing::warn!("未找到 CJK 字体，中文可能显示为方块");
            ctx.fonts()
                .add_font(&[FontSource::DefaultFontData { config: None }]);
        }
    }
}
