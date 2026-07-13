//! Bayer RAW 彩色预览的纯计算链路。

use thiserror::Error;

use crate::raw::{BayerPattern, RawFrame, RawFrameError, RawSpec};

const MAX_CHANNEL_GAIN: f32 = 16.0;
const CROSS: [(i32, i32); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
const DIAGONAL: [(i32, i32); 4] = [(-1, -1), (1, -1), (-1, 1), (1, 1)];
const HORIZONTAL: [(i32, i32); 2] = [(-1, 0), (1, 0)];
const VERTICAL: [(i32, i32); 2] = [(0, -1), (0, 1)];
/// 默认显示 Gamma；用于将线性 RGB 编码为预览纹理的主观亮度。
pub const DEFAULT_DISPLAY_GAMMA: f32 = 2.2;

/// Bayer 2×2 单元中的四个物理位点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfaSite {
    R,
    Gr,
    Gb,
    B,
}

/// 与 R/Gr/Gb/B 位点一一对应的四通道参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CfaQuad<T> {
    pub r: T,
    pub gr: T,
    pub gb: T,
    pub b: T,
}

impl<T: Copy> CfaQuad<T> {
    #[must_use]
    pub const fn splat(value: T) -> Self {
        Self {
            r: value,
            gr: value,
            gb: value,
            b: value,
        }
    }

    #[must_use]
    pub const fn value(self, site: CfaSite) -> T {
        match site {
            CfaSite::R => self.r,
            CfaSite::Gr => self.gr,
            CfaSite::Gb => self.gb,
            CfaSite::B => self.b,
        }
    }
}

/// 单张 RAW 彩色预览使用的参数；不写回原始 `RawFrame`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorPipelineParams {
    pub bayer: BayerPattern,
    pub black_level: CfaQuad<u16>,
    pub gain: CfaQuad<f32>,
    /// `Some` 时将 clamp 后线性 RGB 做 `c^(1/gamma)`；`None` 直接线性量化。
    pub display_gamma: Option<f32>,
}

impl ColorPipelineParams {
    #[must_use]
    pub const fn for_spec(spec: &RawSpec) -> Self {
        Self {
            bayer: spec.bayer,
            black_level: CfaQuad::splat(0),
            gain: CfaQuad::splat(1.0),
            display_gamma: Some(DEFAULT_DISPLAY_GAMMA),
        }
    }

    /// 校验 black level、通道增益和显示 Gamma。
    ///
    /// # Errors
    ///
    /// 最大码值为零、black level 不小于最大码值、gain 非有限值/超出
    /// `[0, 16]`，或启用的 Gamma 非有限/非正时返回错误。
    pub fn validate(self, max_code: u16) -> Result<(), ColorRenderError> {
        if max_code == 0 {
            return Err(ColorRenderError::InvalidMaxCode);
        }
        for site in [CfaSite::R, CfaSite::Gr, CfaSite::Gb, CfaSite::B] {
            let black_level = self.black_level.value(site);
            if black_level >= max_code {
                return Err(ColorRenderError::BlackLevelOutOfRange {
                    site,
                    value: black_level,
                    max_code,
                });
            }
            let gain = self.gain.value(site);
            if !gain.is_finite() || !(0.0..=MAX_CHANNEL_GAIN).contains(&gain) {
                return Err(ColorRenderError::InvalidGain { site, value: gain });
            }
        }
        DisplayTransform::new(self.display_gamma)?;
        Ok(())
    }
}

/// Demosaic 后、显示变换前的线性 RGB。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearRgb {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// 可直接写入 8-bit GUI 纹理的显示 RGB。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}
/// 由已校验 Gamma 构造的显示变换；保证编码阶段不会接收无效指数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayTransform {
    display_gamma: Option<f32>,
}

impl DisplayTransform {
    /// 构造显示变换。
    ///
    /// # Errors
    ///
    /// 启用的 Gamma 非有限或非正时返回错误。
    pub fn new(display_gamma: Option<f32>) -> Result<Self, ColorRenderError> {
        if let Some(gamma) = display_gamma
            && (!gamma.is_finite() || gamma <= 0.0)
        {
            return Err(ColorRenderError::InvalidDisplayGamma { value: gamma });
        }
        Ok(Self { display_gamma })
    }

    /// 将线性 RGB clamp 后按此变换量化为 RGB8。
    #[must_use]
    pub fn encode(self, linear: LinearRgb) -> (Rgb8, u8) {
        let (r, r_clipped) = encode_display_channel(linear.r, self.display_gamma);
        let (g, g_clipped) = encode_display_channel(linear.g, self.display_gamma);
        let (b, b_clipped) = encode_display_channel(linear.b, self.display_gamma);
        (
            Rgb8 { r, g, b },
            u8::from(r_clipped) + u8::from(g_clipped) + u8::from(b_clipped),
        )
    }
}

/// black/gain 预处理阶段的输入诊断。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BayerPrepareDiagnostics {
    pub out_of_range_samples: u64,
    pub black_clipped_samples: u64,
}

/// 一次完整彩色纹理重建的诊断。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ColorRenderDiagnostics {
    pub prepare: BayerPrepareDiagnostics,
    pub display_clipped_channels: u64,
    pub missing_neighbor_channels: u64,
}

impl ColorRenderDiagnostics {
    #[must_use]
    pub const fn new(prepare: BayerPrepareDiagnostics) -> Self {
        Self {
            prepare,
            display_clipped_channels: 0,
            missing_neighbor_channels: 0,
        }
    }

    pub fn record_pixel(&mut self, missing_channels: u8, clipped_channels: u8) {
        self.missing_neighbor_channels = self
            .missing_neighbor_channels
            .saturating_add(u64::from(missing_channels));
        self.display_clipped_channels = self
            .display_clipped_channels
            .saturating_add(u64::from(clipped_channels));
    }
}

/// 单像素重建结果，供 hover 与整帧渲染共享同一算法。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorPixel {
    pub linear: LinearRgb,
    pub rgb8: Rgb8,
    pub missing_neighbor_channels: u8,
    pub display_clipped_channels: u8,
}

#[derive(Debug, Error, PartialEq)]
pub enum ColorRenderError {
    #[error(transparent)]
    InvalidFrame(#[from] RawFrameError),
    #[error("color max code must be non-zero")]
    InvalidMaxCode,
    #[error("black level for {site:?} must be below {max_code}, got {value}")]
    BlackLevelOutOfRange {
        site: CfaSite,
        value: u16,
        max_code: u16,
    },
    #[error("gain for {site:?} must be finite and in 0..=16, got {value}")]
    InvalidGain { site: CfaSite, value: f32 },
    #[error("display gamma must be finite and positive, got {value}")]
    InvalidDisplayGamma { value: f32 },
    #[error("pixel ({x}, {y}) is outside {width}x{height}")]
    PixelOutOfBounds {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    #[error("color rendering cancelled")]
    Cancelled,
}

impl BayerPattern {
    /// 返回给定像素在 Bayer 2×2 周期中的物理位点。
    #[must_use]
    pub const fn site_at(self, x: u32, y: u32) -> CfaSite {
        match (self, x & 1, y & 1) {
            (Self::Rggb, 0, 0) | (Self::Grbg, 1, 0) | (Self::Gbrg, 0, 1) | (Self::Bggr, 1, 1) => {
                CfaSite::R
            }
            (Self::Rggb, 1, 0) | (Self::Grbg, 0, 0) | (Self::Gbrg, 1, 1) | (Self::Bggr, 0, 1) => {
                CfaSite::Gr
            }
            (Self::Rggb, 0, 1) | (Self::Grbg, 1, 1) | (Self::Gbrg, 0, 0) | (Self::Bggr, 1, 0) => {
                CfaSite::Gb
            }
            (Self::Rggb, 1, 1) | (Self::Grbg, 0, 1) | (Self::Gbrg, 1, 0) | (Self::Bggr, 0, 0) => {
                CfaSite::B
            }
            _ => unreachable!(),
        }
    }
}

/// 已完成 black subtraction、归一化和 CFA gain 的单通道线性 Bayer 图。
pub struct PreparedBayer {
    width: u32,
    height: u32,
    bayer: BayerPattern,
    samples: Vec<f32>,
    diagnostics: BayerPrepareDiagnostics,
}

impl PreparedBayer {
    /// 预处理完整 RAW，构造可重复读取的线性 Bayer 图。
    ///
    /// # Errors
    ///
    /// RAW 不变量或彩色参数无效时返回错误。
    pub fn new(frame: &RawFrame, params: &ColorPipelineParams) -> Result<Self, ColorRenderError> {
        Self::new_with_cancel(frame, params, || false)
    }

    /// 支持逐行协作取消的预处理入口。
    ///
    /// # Errors
    ///
    /// RAW/参数无效，或 `is_cancelled` 在任一行开始前返回 true 时返回错误。
    pub fn new_with_cancel<F>(
        frame: &RawFrame,
        params: &ColorPipelineParams,
        mut is_cancelled: F,
    ) -> Result<Self, ColorRenderError>
    where
        F: FnMut() -> bool,
    {
        let (pixel_count, max_code) = validate_frame(frame)?;
        params.validate(max_code)?;
        let width = frame.spec.width as usize;
        let mut samples = Vec::with_capacity(pixel_count);
        let mut diagnostics = BayerPrepareDiagnostics::default();

        for y in 0..frame.spec.height {
            if is_cancelled() {
                return Err(ColorRenderError::Cancelled);
            }
            let row_start = y as usize * width;
            for x in 0..frame.spec.width {
                let raw = frame.pixels()[row_start + x as usize];
                let site = params.bayer.site_at(x, y);
                let (sample, out_of_range, black_clipped) =
                    correct_sample(raw, max_code, site, params);
                diagnostics.out_of_range_samples = diagnostics
                    .out_of_range_samples
                    .saturating_add(u64::from(out_of_range));
                diagnostics.black_clipped_samples = diagnostics
                    .black_clipped_samples
                    .saturating_add(u64::from(black_clipped));
                samples.push(sample);
            }
        }

        Ok(Self {
            width: frame.spec.width,
            height: frame.spec.height,
            bayer: params.bayer,
            samples,
            diagnostics,
        })
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn diagnostics(&self) -> BayerPrepareDiagnostics {
        self.diagnostics
    }

    /// 对单像素执行双线性去马赛克，返回线性 RGB 和缺失通道数量。
    ///
    /// # Errors
    ///
    /// 坐标超出图像时返回错误。
    pub fn linear_rgb_at(&self, x: u32, y: u32) -> Result<(LinearRgb, u8), ColorRenderError> {
        validate_coordinate(x, y, self.width, self.height)?;
        Ok(demosaic_at(
            self.width,
            self.height,
            self.bayer,
            x,
            y,
            |sample_x, sample_y| {
                self.samples[sample_y as usize * self.width as usize + sample_x as usize]
            },
        ))
    }
}

/// 不分配整帧中间图，按当前参数重建一个 hover 像素。
///
/// # Errors
///
/// RAW/参数无效或坐标越界时返回错误。
pub fn render_pixel_at(
    frame: &RawFrame,
    params: &ColorPipelineParams,
    x: u32,
    y: u32,
) -> Result<ColorPixel, ColorRenderError> {
    let (_, max_code) = validate_frame(frame)?;
    params.validate(max_code)?;
    validate_coordinate(x, y, frame.spec.width, frame.spec.height)?;
    let (linear, missing_neighbor_channels) = demosaic_at(
        frame.spec.width,
        frame.spec.height,
        params.bayer,
        x,
        y,
        |sample_x, sample_y| {
            let index = sample_y as usize * frame.spec.width as usize + sample_x as usize;
            let raw = frame.pixels()[index];
            let site = params.bayer.site_at(sample_x, sample_y);
            correct_sample(raw, max_code, site, params).0
        },
    );
    let display_transform = DisplayTransform::new(params.display_gamma)?;
    let (rgb8, display_clipped_channels) = display_transform.encode(linear);
    Ok(ColorPixel {
        linear,
        rgb8,
        missing_neighbor_channels,
        display_clipped_channels,
    })
}

fn validate_frame(frame: &RawFrame) -> Result<(usize, u16), ColorRenderError> {
    frame.spec.validate()?;
    let expected = frame.spec.pixel_count()?;
    let actual = frame.pixels().len();
    if expected != actual {
        return Err(RawFrameError::PixelCountMismatch { expected, actual }.into());
    }
    let max_code = frame.spec.max_code_value();
    if max_code == 0 {
        return Err(ColorRenderError::InvalidMaxCode);
    }
    Ok((expected, max_code))
}

fn validate_coordinate(x: u32, y: u32, width: u32, height: u32) -> Result<(), ColorRenderError> {
    if x >= width || y >= height {
        return Err(ColorRenderError::PixelOutOfBounds {
            x,
            y,
            width,
            height,
        });
    }
    Ok(())
}

fn correct_sample(
    raw: u16,
    max_code: u16,
    site: CfaSite,
    params: &ColorPipelineParams,
) -> (f32, bool, bool) {
    let out_of_range = raw > max_code;
    let bounded = raw.min(max_code);
    let black_level = params.black_level.value(site);
    let black_clipped = bounded < black_level;
    let numerator = bounded.saturating_sub(black_level);
    let denominator = max_code - black_level;
    let normalized = f32::from(numerator) / f32::from(denominator);
    (
        normalized * params.gain.value(site),
        out_of_range,
        black_clipped,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RgbChannel {
    R,
    G,
    B,
}

const fn rgb_channel(site: CfaSite) -> RgbChannel {
    match site {
        CfaSite::R => RgbChannel::R,
        CfaSite::Gr | CfaSite::Gb => RgbChannel::G,
        CfaSite::B => RgbChannel::B,
    }
}

struct DemosaicContext<'a, F> {
    width: u32,
    height: u32,
    bayer: BayerPattern,
    sample: &'a mut F,
}

impl<F> DemosaicContext<'_, F>
where
    F: FnMut(u32, u32) -> f32,
{
    fn average(&mut self, x: u32, y: u32, offsets: &[(i32, i32)], target: RgbChannel) -> (f32, u8) {
        let mut sum = 0.0;
        let mut count = 0_u8;
        for &(dx, dy) in offsets {
            let sample_x = i64::from(x) + i64::from(dx);
            let sample_y = i64::from(y) + i64::from(dy);
            if sample_x < 0
                || sample_y < 0
                || sample_x >= i64::from(self.width)
                || sample_y >= i64::from(self.height)
            {
                continue;
            }
            let sample_x = u32::try_from(sample_x).expect("x bounds checked");
            let sample_y = u32::try_from(sample_y).expect("y bounds checked");
            if rgb_channel(self.bayer.site_at(sample_x, sample_y)) != target {
                continue;
            }
            sum += (self.sample)(sample_x, sample_y);
            count += 1;
        }
        if count == 0 {
            (0.0, 1)
        } else {
            (sum / f32::from(count), 0)
        }
    }
}

fn demosaic_at<F>(
    width: u32,
    height: u32,
    bayer: BayerPattern,
    x: u32,
    y: u32,
    mut sample: F,
) -> (LinearRgb, u8)
where
    F: FnMut(u32, u32) -> f32,
{
    let own = sample(x, y);
    let mut context = DemosaicContext {
        width,
        height,
        bayer,
        sample: &mut sample,
    };
    let (red, green, blue, missing) = match bayer.site_at(x, y) {
        CfaSite::R => {
            let (green, missing_green) = context.average(x, y, &CROSS, RgbChannel::G);
            let (blue, missing_blue) = context.average(x, y, &DIAGONAL, RgbChannel::B);
            (own, green, blue, missing_green + missing_blue)
        }
        CfaSite::B => {
            let (green, missing_green) = context.average(x, y, &CROSS, RgbChannel::G);
            let (red, missing_red) = context.average(x, y, &DIAGONAL, RgbChannel::R);
            (red, green, own, missing_green + missing_red)
        }
        CfaSite::Gr => {
            let (red, missing_red) = context.average(x, y, &HORIZONTAL, RgbChannel::R);
            let (blue, missing_blue) = context.average(x, y, &VERTICAL, RgbChannel::B);
            (red, own, blue, missing_red + missing_blue)
        }
        CfaSite::Gb => {
            let (red, missing_red) = context.average(x, y, &VERTICAL, RgbChannel::R);
            let (blue, missing_blue) = context.average(x, y, &HORIZONTAL, RgbChannel::B);
            (red, own, blue, missing_red + missing_blue)
        }
    };
    (
        LinearRgb {
            r: red,
            g: green,
            b: blue,
        },
        missing,
    )
}

// `encoded` 已由 clamp 与可选 Gamma 严格限制在 `[0, 1]`，量化后一定落在 `u8` 范围内。
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn encode_display_channel(value: f32, display_gamma: Option<f32>) -> (u8, bool) {
    let clipped = !value.is_finite() || !(0.0..=1.0).contains(&value);
    let bounded = if value.is_nan() || value == f32::NEG_INFINITY {
        0.0
    } else if value == f32::INFINITY {
        1.0
    } else {
        value.clamp(0.0, 1.0)
    };
    let encoded = display_gamma.map_or(bounded, |gamma| bounded.powf(1.0 / gamma));
    ((encoded * 255.0).round() as u8, clipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(pattern: BayerPattern, width: u32, height: u32, pixels: Vec<u16>) -> RawFrame {
        RawFrame::new(
            RawSpec {
                width,
                height,
                bit_depth: 10,
                bayer: pattern,
            },
            pixels,
        )
        .unwrap()
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1.0e-6, "{actual} != {expected}");
    }

    #[test]
    fn maps_all_bayer_patterns_and_periods() {
        let cases = [
            (
                BayerPattern::Rggb,
                [CfaSite::R, CfaSite::Gr, CfaSite::Gb, CfaSite::B],
            ),
            (
                BayerPattern::Grbg,
                [CfaSite::Gr, CfaSite::R, CfaSite::B, CfaSite::Gb],
            ),
            (
                BayerPattern::Gbrg,
                [CfaSite::Gb, CfaSite::B, CfaSite::R, CfaSite::Gr],
            ),
            (
                BayerPattern::Bggr,
                [CfaSite::B, CfaSite::Gb, CfaSite::Gr, CfaSite::R],
            ),
        ];
        for (pattern, expected) in cases {
            assert_eq!(
                [
                    pattern.site_at(0, 0),
                    pattern.site_at(1, 0),
                    pattern.site_at(0, 1),
                    pattern.site_at(1, 1),
                ],
                expected
            );
            assert_eq!(pattern.site_at(3, 2), pattern.site_at(1, 0));
        }
    }

    #[test]
    fn validates_black_and_gain_boundaries() {
        let spec = RawSpec {
            width: 2,
            height: 2,
            bit_depth: 10,
            bayer: BayerPattern::Rggb,
        };
        let mut params = ColorPipelineParams::for_spec(&spec);
        params.black_level.b = 1022;
        params.gain.r = 16.0;
        assert!(params.validate(1023).is_ok());

        params.black_level.b = 1023;
        assert!(matches!(
            params.validate(1023),
            Err(ColorRenderError::BlackLevelOutOfRange {
                site: CfaSite::B,
                ..
            })
        ));
        params.black_level.b = 0;
        for invalid in [-0.1, 16.1, f32::NAN, f32::INFINITY] {
            params.gain.gb = invalid;
            assert!(matches!(
                params.validate(1023),
                Err(ColorRenderError::InvalidGain {
                    site: CfaSite::Gb,
                    ..
                })
            ));
        }
        params.gain.gb = 1.0;
        assert_eq!(params.display_gamma, Some(DEFAULT_DISPLAY_GAMMA));
        for invalid in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            params.display_gamma = Some(invalid);
            assert!(matches!(
                params.validate(1023),
                Err(ColorRenderError::InvalidDisplayGamma { .. })
            ));
        }
    }

    #[test]
    fn prepares_channel_specific_black_and_gain_without_mutating_raw() {
        let raw = frame(BayerPattern::Rggb, 2, 2, vec![110, 220, 330, 440]);
        let original = raw.pixels().to_vec();
        let params = ColorPipelineParams {
            bayer: BayerPattern::Rggb,
            black_level: CfaQuad {
                r: 10,
                gr: 20,
                gb: 30,
                b: 40,
            },
            display_gamma: Some(DEFAULT_DISPLAY_GAMMA),
            gain: CfaQuad {
                r: 1.0,
                gr: 2.0,
                gb: 3.0,
                b: 4.0,
            },
        };
        let prepared = PreparedBayer::new(&raw, &params).unwrap();

        assert_close(prepared.samples[0], 100.0 / 1013.0);
        assert_close(prepared.samples[1], 200.0 / 1003.0 * 2.0);
        assert_close(prepared.samples[2], 300.0 / 993.0 * 3.0);
        assert_close(prepared.samples[3], 400.0 / 983.0 * 4.0);
        assert_eq!(raw.pixels(), original);
    }

    #[test]
    fn records_black_clipping_and_out_of_range() {
        let raw = frame(BayerPattern::Rggb, 3, 1, vec![9, 10, 1024]);
        let mut params = ColorPipelineParams::for_spec(&raw.spec);
        params.black_level = CfaQuad::splat(10);
        let prepared = PreparedBayer::new(&raw, &params).unwrap();

        assert_eq!(prepared.diagnostics().black_clipped_samples, 1);
        assert_eq!(prepared.diagnostics().out_of_range_samples, 1);
        assert_close(prepared.samples[0], 0.0);
        assert_close(prepared.samples[1], 0.0);
        assert_close(prepared.samples[2], 1.0);
        assert_eq!(raw.pixels()[2], 1024);
    }

    #[test]
    fn reconstructs_constant_planes_for_all_patterns() {
        for pattern in [
            BayerPattern::Rggb,
            BayerPattern::Grbg,
            BayerPattern::Gbrg,
            BayerPattern::Bggr,
        ] {
            let mut pixels = Vec::new();
            for y in 0..4 {
                for x in 0..4 {
                    pixels.push(match pattern.site_at(x, y) {
                        CfaSite::R => 100,
                        CfaSite::Gr | CfaSite::Gb => 200,
                        CfaSite::B => 300,
                    });
                }
            }
            let raw = frame(pattern, 4, 4, pixels);
            let params = ColorPipelineParams::for_spec(&raw.spec);
            let prepared = PreparedBayer::new(&raw, &params).unwrap();
            for y in 0..4 {
                for x in 0..4 {
                    let (rgb, missing) = prepared.linear_rgb_at(x, y).unwrap();
                    assert_eq!(missing, 0);
                    assert_close(rgb.r, 100.0 / 1023.0);
                    assert_close(rgb.g, 200.0 / 1023.0);
                    assert_close(rgb.b, 300.0 / 1023.0);
                }
            }
        }
    }

    #[test]
    fn uses_gr_horizontal_red_and_vertical_blue_neighbors() {
        let mut pixels = vec![0; 25];
        pixels[2 * 5 + 3] = 50;
        pixels[2 * 5 + 2] = 100;
        pixels[2 * 5 + 4] = 300;
        pixels[5 + 3] = 400;
        pixels[3 * 5 + 3] = 800;
        let raw = frame(BayerPattern::Rggb, 5, 5, pixels);
        let params = ColorPipelineParams::for_spec(&raw.spec);
        let prepared = PreparedBayer::new(&raw, &params).unwrap();
        let (rgb, missing) = prepared.linear_rgb_at(3, 2).unwrap();

        assert_eq!(missing, 0);
        assert_close(rgb.r, 200.0 / 1023.0);
        assert_close(rgb.g, 50.0 / 1023.0);
        assert_close(rgb.b, 600.0 / 1023.0);
    }

    #[test]
    fn uses_gb_vertical_red_and_horizontal_blue_neighbors() {
        let mut pixels = vec![0; 25];
        pixels[3 * 5 + 2] = 50;
        pixels[2 * 5 + 2] = 100;
        pixels[4 * 5 + 2] = 300;
        pixels[3 * 5 + 1] = 400;
        pixels[3 * 5 + 3] = 800;
        let raw = frame(BayerPattern::Rggb, 5, 5, pixels);
        let params = ColorPipelineParams::for_spec(&raw.spec);
        let prepared = PreparedBayer::new(&raw, &params).unwrap();
        let (rgb, missing) = prepared.linear_rgb_at(2, 3).unwrap();

        assert_eq!(missing, 0);
        assert_close(rgb.r, 200.0 / 1023.0);
        assert_close(rgb.g, 50.0 / 1023.0);
        assert_close(rgb.b, 600.0 / 1023.0);
    }

    #[test]
    fn edge_interpolation_uses_only_in_bounds_neighbors() {
        let raw = frame(
            BayerPattern::Rggb,
            3,
            3,
            vec![10, 100, 20, 300, 500, 0, 0, 0, 0],
        );
        let params = ColorPipelineParams::for_spec(&raw.spec);
        let prepared = PreparedBayer::new(&raw, &params).unwrap();
        let (rgb, missing) = prepared.linear_rgb_at(0, 0).unwrap();

        assert_eq!(missing, 0);
        assert_close(rgb.r, 10.0 / 1023.0);
        assert_close(rgb.g, 200.0 / 1023.0);
        assert_close(rgb.b, 500.0 / 1023.0);
    }

    #[test]
    fn degenerate_image_returns_zero_for_missing_channels() {
        let raw = frame(BayerPattern::Rggb, 1, 1, vec![512]);
        let params = ColorPipelineParams::for_spec(&raw.spec);
        let prepared = PreparedBayer::new(&raw, &params).unwrap();
        let (rgb, missing) = prepared.linear_rgb_at(0, 0).unwrap();

        assert_eq!(missing, 2);
        assert_close(rgb.r, 512.0 / 1023.0);
        assert_close(rgb.g, 0.0);
        assert_close(rgb.b, 0.0);
    }

    #[test]
    fn rejects_mutated_frame_invariants() {
        let mut raw = frame(BayerPattern::Rggb, 2, 2, vec![0; 4]);
        raw.spec.width = 3;
        let params = ColorPipelineParams::for_spec(&raw.spec);
        assert!(matches!(
            PreparedBayer::new(&raw, &params),
            Err(ColorRenderError::InvalidFrame(
                RawFrameError::PixelCountMismatch { .. }
            ))
        ));
    }

    #[test]
    fn encodes_linear_rgb_with_optional_gamma_and_clipping() {
        let gamma = DisplayTransform::new(Some(DEFAULT_DISPLAY_GAMMA)).unwrap();
        let (encoded, clipped) = gamma.encode(LinearRgb {
            r: 0.0,
            g: 0.25,
            b: 1.0,
        });
        assert_eq!(
            encoded,
            Rgb8 {
                r: 0,
                g: 136,
                b: 255
            }
        );
        assert_eq!(clipped, 0);

        let linear = DisplayTransform::new(None).unwrap();
        let (encoded, clipped) = linear.encode(LinearRgb {
            r: -0.1,
            g: 0.25,
            b: f32::INFINITY,
        });
        assert_eq!(
            encoded,
            Rgb8 {
                r: 0,
                g: 64,
                b: 255
            }
        );
        assert_eq!(clipped, 2);
    }

    #[test]
    fn hover_pixel_path_matches_prepared_render() {
        let raw = frame(
            BayerPattern::Rggb,
            3,
            3,
            vec![10, 100, 20, 300, 500, 400, 30, 600, 40],
        );
        let params = ColorPipelineParams::for_spec(&raw.spec);
        let prepared = PreparedBayer::new(&raw, &params).unwrap();
        let (linear, missing) = prepared.linear_rgb_at(1, 1).unwrap();
        let (rgb8, clipped) = DisplayTransform::new(params.display_gamma)
            .unwrap()
            .encode(linear);
        let direct = render_pixel_at(&raw, &params, 1, 1).unwrap();

        assert_eq!(direct.linear, linear);
        assert_eq!(direct.rgb8, rgb8);
        assert_eq!(direct.missing_neighbor_channels, missing);
        assert_eq!(direct.display_clipped_channels, clipped);
    }
}
