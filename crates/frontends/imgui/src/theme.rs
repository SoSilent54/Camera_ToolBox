//! ImGui 字体加载与状态/overlay 颜色。

use imgui::{FontConfig, FontGlyphRanges, FontSource};
use std::path::PathBuf;

pub const ACCENT: [f32; 4] = [0.32, 0.70, 1.0, 1.0];
pub const MUTED: [f32; 4] = [0.66, 0.70, 0.78, 1.0];
pub const OK: [f32; 4] = [0.30, 0.88, 0.54, 1.0];
pub const WARN: [f32; 4] = [1.0, 0.76, 0.38, 1.0];
pub const ERR: [f32; 4] = [1.0, 0.48, 0.48, 1.0];

const MATH_GLYPH_RANGES: &[u32] = &[
    0x00b1, 0x00b1, // ±
    0x00d7, 0x00d7, // ×
    0x00f7, 0x00f7, // ÷
    0x0391, 0x03c9, // Greek capitals/lowercase: Δ, Σ, σ ...
    0x2070, 0x209f, // superscripts/subscripts: ᵀ ...
    0x2190, 0x22ff, // arrows + mathematical operators: ≤, ≥, ≈, ∞ ...
    0x25a0, 0x25ff, // geometric blocks used by compact charts.
    0x2700, 0x27bf, // dingbats: ✓ ...
    0,
];

fn math_font_candidates() -> [PathBuf; 8] {
    [
        PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
        PathBuf::from("/usr/share/fonts/truetype/ttf-dejavu/DejaVuSans.ttf"),
        PathBuf::from("/usr/share/fonts/truetype/freefont/FreeSans.ttf"),
        PathBuf::from("/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf"),
        PathBuf::from("/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf"),
        PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansMath-Regular.ttf"),
        PathBuf::from("/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf"),
        PathBuf::from("/usr/share/fonts/truetype/ancient-scripts/Symbola.ttf"),
    ]
}

fn read_first_font(candidates: impl IntoIterator<Item = PathBuf>) -> Option<(PathBuf, Vec<u8>)> {
    candidates.into_iter().find_map(|path| {
        path.is_file()
            .then(|| std::fs::read(&path).ok().map(|bytes| (path, bytes)))
            .flatten()
    })
}

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

    match read_first_font(candidates) {
        Some((cjk_path, cjk_bytes)) => {
            if let Some((math_path, math_bytes)) = read_first_font(math_font_candidates()) {
                ctx.fonts().add_font(&[
                    FontSource::TtfData {
                        data: &cjk_bytes,
                        size_pixels: 18.0,
                        config: Some(FontConfig {
                            glyph_ranges: FontGlyphRanges::chinese_full(),
                            ..FontConfig::default()
                        }),
                    },
                    FontSource::TtfData {
                        data: &math_bytes,
                        size_pixels: 18.0,
                        config: Some(FontConfig {
                            glyph_ranges: FontGlyphRanges::from_slice(MATH_GLYPH_RANGES),
                            ..FontConfig::default()
                        }),
                    },
                ]);
                tracing::info!(
                    cjk = %cjk_path.display(),
                    math = %math_path.display(),
                    "已加载 CJK 字体并合并数学符号字形"
                );
            } else {
                ctx.fonts().add_font(&[FontSource::TtfData {
                    data: &cjk_bytes,
                    size_pixels: 18.0,
                    config: Some(FontConfig {
                        glyph_ranges: FontGlyphRanges::chinese_full(),
                        ..FontConfig::default()
                    }),
                }]);
                tracing::warn!(
                    cjk = %cjk_path.display(),
                    "未找到数学符号字体，σ/Δ/≤ 等符号可能显示为方块"
                );
            }
        }
        None => {
            tracing::warn!("未找到 CJK 字体，中文可能显示为方块");
            ctx.fonts()
                .add_font(&[FontSource::DefaultFontData { config: None }]);
        }
    }
}
