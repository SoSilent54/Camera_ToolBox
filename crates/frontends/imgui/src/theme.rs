//! ImGui 字体加载与状态/overlay 颜色。

use imgui::{FontConfig, FontGlyphRanges, FontSource};
use std::path::Path;

pub const ACCENT: [f32; 4] = [0.32, 0.70, 1.0, 1.0];
pub const MUTED: [f32; 4] = [0.66, 0.70, 0.78, 1.0];
pub const OK: [f32; 4] = [0.30, 0.88, 0.54, 1.0];
pub const WARN: [f32; 4] = [1.0, 0.76, 0.38, 1.0];
pub const ERR: [f32; 4] = [1.0, 0.48, 0.48, 1.0];

/// 加载中文字体；找不到 CJK 字体时保留默认字体并记录风险。
pub fn install_fonts(ctx: &mut imgui::Context) {
    let cjk_font = [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
    ]
    .into_iter()
    .find(|path| Path::new(path).exists());

    match cjk_font.and_then(|path| std::fs::read(path).ok()) {
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
