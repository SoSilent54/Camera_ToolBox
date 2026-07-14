//! ROI 统计分析。

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    color::{CfaQuad, CfaSite},
    raw::RawFrame,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Roi {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Roi {
    /// 将 ROI 裁剪到图像范围内；裁剪后为空则返回 `None`。
    #[must_use]
    pub fn clamped_to(self, image_width: u32, image_height: u32) -> Option<Self> {
        let x0 = self.x.min(image_width);
        let y0 = self.y.min(image_height);
        let x1 = self.x.saturating_add(self.width).min(image_width);
        let y1 = self.y.saturating_add(self.height).min(image_height);
        let width = x1.saturating_sub(x0);
        let height = y1.saturating_sub(y0);
        (width > 0 && height > 0).then_some(Self {
            x: x0,
            y: y0,
            width,
            height,
        })
    }

    /// 判断像素坐标是否位于该半开区间 ROI 内。
    #[must_use]
    pub const fn contains(self, x: u32, y: u32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.width)
            && y < self.y.saturating_add(self.height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoiStats {
    pub min: u16,
    pub max: u16,
    pub mean: f64,
    /// 大于等于声明最大 code 的像素；包含 histogram 的 overflow 样本。
    pub saturated_pixels: u64,
    pub total_pixels: u64,
}

impl RoiStats {
    /// 统计比例仅用于显示；实际可分配图像的像素计数远低于 `f64` 精确整数上限。
    #[allow(clippy::cast_precision_loss)]
    #[must_use]
    pub fn saturation_ratio(self) -> f64 {
        self.saturated_pixels as f64 / self.total_pixels as f64
    }
}

const MAX_HISTOGRAM_BINS: usize = 4096;

/// 单条直方图序列；超出声明 bit-depth 的样本独立统计。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistogramSeries {
    bins: Vec<u64>,
    pub sample_count: u64,
    pub overflow_count: u64,
}

impl HistogramSeries {
    fn new(bin_count: usize) -> Self {
        Self {
            bins: vec![0; bin_count],
            sample_count: 0,
            overflow_count: 0,
        }
    }

    fn record(&mut self, value: u16, max_code: u16, bin_shift: u8) {
        self.sample_count = self.sample_count.saturating_add(1);
        if value > max_code {
            self.overflow_count = self.overflow_count.saturating_add(1);
            return;
        }
        let index = usize::from(value >> bin_shift);
        self.bins[index] = self.bins[index].saturating_add(1);
    }

    #[must_use]
    pub fn bins(&self) -> &[u64] {
        &self.bins
    }
}

/// RAW Bayer 四物理位点与全体样本的直方图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawBayerHistogram {
    pub max_code: u16,
    pub bin_shift: u8,
    pub channels: CfaQuad<HistogramSeries>,
    pub all: HistogramSeries,
}

impl RawBayerHistogram {
    fn new(bit_depth: u8, max_code: u16) -> Self {
        let exact_bits = bit_depth.min(12);
        let bin_count = (1usize << exact_bits).min(MAX_HISTOGRAM_BINS);
        let bin_shift = bit_depth.saturating_sub(12);
        Self {
            max_code,
            bin_shift,
            channels: CfaQuad {
                r: HistogramSeries::new(bin_count),
                gr: HistogramSeries::new(bin_count),
                gb: HistogramSeries::new(bin_count),
                b: HistogramSeries::new(bin_count),
            },
            all: HistogramSeries::new(bin_count),
        }
    }

    #[must_use]
    pub const fn bin_width(&self) -> u32 {
        1u32 << self.bin_shift
    }

    /// 返回正常 RAW code 所属 bin；超出声明最大 code 时返回 `None`。
    #[must_use]
    pub fn bin_index(&self, value: u16) -> Option<usize> {
        if value > self.max_code {
            return None;
        }
        let index = usize::from(value >> self.bin_shift);
        (index < self.all.bins.len()).then_some(index)
    }

    /// 返回 bin 覆盖的闭区间 RAW code，非法 bin 返回 `None`。
    #[must_use]
    pub fn bin_code_range(&self, index: usize) -> Option<(u16, u16)> {
        if index >= self.all.bins.len() {
            return None;
        }
        let width = self.bin_width();
        let lower = u32::try_from(index).ok()?.saturating_mul(width);
        let upper = lower
            .saturating_add(width.saturating_sub(1))
            .min(u32::from(self.max_code));
        Some((u16::try_from(lower).ok()?, u16::try_from(upper).ok()?))
    }

    #[must_use]
    pub fn total_overflow_count(&self) -> u64 {
        self.all.overflow_count
    }
}

/// 同一次 RAW ROI 遍历产生的标量统计与 Bayer 直方图。
#[derive(Debug, Clone, PartialEq)]
pub struct RawRoiAnalysis {
    pub roi: Roi,
    pub stats: RoiStats,
    pub histogram: RawBayerHistogram,
}

/// 分析经边界裁剪后的 ROI 标量统计。
///
/// # Errors
///
/// 当 ROI 裁剪后为空时返回 [`AnalysisError::EmptyRoi`]。
pub fn analyze_roi(frame: &RawFrame, roi: Roi) -> Result<RoiStats, AnalysisError> {
    analyze_roi_with_cancel(frame, roi, || false)
}

/// 可取消的 ROI 标量统计；取消检查以行为粒度。
///
/// # Errors
///
/// 当 ROI 裁剪后为空时返回 [`AnalysisError::EmptyRoi`]；取消回调返回 `true` 时返回
/// [`AnalysisError::Cancelled`]。
#[allow(clippy::cast_precision_loss)]
pub fn analyze_roi_with_cancel<F>(
    frame: &RawFrame,
    roi: Roi,
    mut is_cancelled: F,
) -> Result<RoiStats, AnalysisError>
where
    F: FnMut() -> bool,
{
    let roi = roi
        .clamped_to(frame.spec.width, frame.spec.height)
        .ok_or(AnalysisError::EmptyRoi)?;
    let width = frame.spec.width as usize;
    let saturation_code = frame.spec.max_code_value();
    let pixels = frame.pixels();

    let mut min = u16::MAX;
    let mut max = u16::MIN;
    let mut sum: u128 = 0;
    let mut saturated_pixels: u64 = 0;
    let mut total_pixels: u64 = 0;

    for row in roi.y..roi.y + roi.height {
        if is_cancelled() {
            return Err(AnalysisError::Cancelled);
        }
        let row_start = row as usize * width;
        for col in roi.x..roi.x + roi.width {
            let value = pixels[row_start + col as usize];
            min = min.min(value);
            max = max.max(value);
            sum = sum.saturating_add(u128::from(value));
            saturated_pixels = saturated_pixels.saturating_add(u64::from(value >= saturation_code));
            total_pixels = total_pixels.saturating_add(1);
        }
    }

    Ok(RoiStats {
        min,
        max,
        mean: sum as f64 / total_pixels as f64,
        saturated_pixels,
        total_pixels,
    })
}

/// 在一次 ROI 遍历中计算标量统计与 RAW Bayer 直方图。
///
/// # Errors
///
/// 当 ROI 裁剪后为空时返回 [`AnalysisError::EmptyRoi`]。
pub fn analyze_raw_roi(frame: &RawFrame, roi: Roi) -> Result<RawRoiAnalysis, AnalysisError> {
    analyze_raw_roi_with_cancel(frame, roi, || false)
}

/// 可取消的 RAW ROI 分析；取消检查以行为粒度，避免高分辨率 stale 请求长期占用 CPU。
///
/// # Errors
///
/// 当 ROI 裁剪后为空时返回 [`AnalysisError::EmptyRoi`]；取消回调返回 `true` 时返回
/// [`AnalysisError::Cancelled`]。
#[allow(clippy::cast_precision_loss)]
pub fn analyze_raw_roi_with_cancel<F>(
    frame: &RawFrame,
    roi: Roi,
    mut is_cancelled: F,
) -> Result<RawRoiAnalysis, AnalysisError>
where
    F: FnMut() -> bool,
{
    let roi = roi
        .clamped_to(frame.spec.width, frame.spec.height)
        .ok_or(AnalysisError::EmptyRoi)?;
    let frame_width = frame.spec.width as usize;
    let max_code = frame.spec.max_code_value();
    let mut histogram = RawBayerHistogram::new(frame.spec.bit_depth, max_code);
    let mut min = u16::MAX;
    let mut max = u16::MIN;
    let mut sum: u128 = 0;
    let mut saturated_pixels: u64 = 0;
    let mut total_pixels: u64 = 0;

    for y in roi.y..roi.y + roi.height {
        if is_cancelled() {
            return Err(AnalysisError::Cancelled);
        }
        let row_start = y as usize * frame_width;
        for x in roi.x..roi.x + roi.width {
            let value = frame.pixels()[row_start + x as usize];
            min = min.min(value);
            max = max.max(value);
            sum = sum.saturating_add(u128::from(value));
            saturated_pixels = saturated_pixels.saturating_add(u64::from(value >= max_code));
            total_pixels = total_pixels.saturating_add(1);
            histogram.all.record(value, max_code, histogram.bin_shift);
            let series = match frame.spec.bayer.site_at(x, y) {
                CfaSite::R => &mut histogram.channels.r,
                CfaSite::Gr => &mut histogram.channels.gr,
                CfaSite::Gb => &mut histogram.channels.gb,
                CfaSite::B => &mut histogram.channels.b,
            };
            series.record(value, max_code, histogram.bin_shift);
        }
    }

    Ok(RawRoiAnalysis {
        roi,
        stats: RoiStats {
            min,
            max,
            mean: sum as f64 / total_pixels as f64,
            saturated_pixels,
            total_pixels,
        },
        histogram,
    })
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AnalysisError {
    #[error("roi is empty after clamping to image bounds")]
    EmptyRoi,
    #[error("analysis cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::{BayerPattern, RawSpec};

    #[test]
    fn roi_analysis_clamps_and_counts_saturation() {
        let spec = RawSpec {
            width: 2,
            height: 2,
            bit_depth: 10,
            bayer: BayerPattern::Rggb,
        };
        let frame = RawFrame::new(spec, vec![0, 1023, 10, 20]).unwrap();
        let stats = analyze_roi(
            &frame,
            Roi {
                x: 1,
                y: 0,
                width: 4,
                height: 4,
            },
        )
        .unwrap();

        assert_eq!(stats.min, 20);
        assert_eq!(stats.max, 1023);
        assert_eq!(stats.total_pixels, 2);
        assert_eq!(stats.saturated_pixels, 1);
    }

    #[test]
    fn raw_histogram_uses_exact_ten_bit_bins_and_separate_overflow() {
        let frame = RawFrame::new(
            RawSpec {
                width: 2,
                height: 2,
                bit_depth: 10,
                bayer: BayerPattern::Rggb,
            },
            vec![0, 1023, 1024, 512],
        )
        .unwrap();
        let analysis = analyze_raw_roi(
            &frame,
            Roi {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
        )
        .unwrap();

        assert_eq!(analysis.histogram.all.bins().len(), 1024);
        assert_eq!(analysis.histogram.bin_width(), 1);
        assert_eq!(analysis.histogram.all.bins()[0], 1);
        assert_eq!(analysis.histogram.all.bins()[512], 1);
        assert_eq!(analysis.histogram.all.bins()[1023], 1);
        assert_eq!(analysis.histogram.total_overflow_count(), 1);
        assert_eq!(analysis.histogram.channels.gb.overflow_count, 1);
        assert_eq!(analysis.histogram.channels.r.sample_count, 1);
        assert_eq!(analysis.histogram.channels.gr.sample_count, 1);
        assert_eq!(analysis.histogram.channels.gb.sample_count, 1);
        assert_eq!(analysis.histogram.channels.b.sample_count, 1);
    }

    #[test]
    fn sixteen_bit_histogram_groups_sixteen_codes_per_bin() {
        let frame = RawFrame::new(
            RawSpec {
                width: 3,
                height: 1,
                bit_depth: 16,
                bayer: BayerPattern::Rggb,
            },
            vec![0, 15, 16],
        )
        .unwrap();
        let analysis = analyze_raw_roi(
            &frame,
            Roi {
                x: 0,
                y: 0,
                width: 3,
                height: 1,
            },
        )
        .unwrap();

        assert_eq!(analysis.histogram.all.bins().len(), 4096);
        assert_eq!(analysis.histogram.bin_width(), 16);
        assert_eq!(analysis.histogram.all.bins()[0], 2);
        assert_eq!(analysis.histogram.all.bins()[1], 1);
    }

    #[test]
    fn raw_histogram_respects_all_bayer_patterns() {
        for pattern in [
            BayerPattern::Rggb,
            BayerPattern::Grbg,
            BayerPattern::Gbrg,
            BayerPattern::Bggr,
        ] {
            let frame = RawFrame::new(
                RawSpec {
                    width: 2,
                    height: 2,
                    bit_depth: 8,
                    bayer: pattern,
                },
                vec![10, 20, 30, 40],
            )
            .unwrap();
            let analysis = analyze_raw_roi(
                &frame,
                Roi {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
            )
            .unwrap();

            for (x, y, value) in [(0, 0, 10), (1, 0, 20), (0, 1, 30), (1, 1, 40)] {
                let series = match pattern.site_at(x, y) {
                    CfaSite::R => &analysis.histogram.channels.r,
                    CfaSite::Gr => &analysis.histogram.channels.gr,
                    CfaSite::Gb => &analysis.histogram.channels.gb,
                    CfaSite::B => &analysis.histogram.channels.b,
                };
                assert_eq!(series.bins()[value], 1, "{pattern:?} at ({x}, {y})");
            }
        }
    }

    #[test]
    fn raw_histogram_cancellation_is_observable() {
        let frame = RawFrame::new(
            RawSpec {
                width: 1,
                height: 2,
                bit_depth: 8,
                bayer: BayerPattern::Rggb,
            },
            vec![1, 2],
        )
        .unwrap();
        let mut checks = 0;
        let error = analyze_raw_roi_with_cancel(
            &frame,
            Roi {
                x: 0,
                y: 0,
                width: 1,
                height: 2,
            },
            || {
                checks += 1;
                checks > 1
            },
        )
        .unwrap_err();

        assert_eq!(error, AnalysisError::Cancelled);
    }

    #[test]
    fn roi_membership_and_histogram_bin_lookup_share_half_open_rules() {
        let roi = Roi {
            x: 2,
            y: 3,
            width: 4,
            height: 2,
        };
        assert!(roi.contains(2, 3));
        assert!(roi.contains(5, 4));
        assert!(!roi.contains(6, 4));

        let histogram = RawBayerHistogram::new(16, u16::MAX);
        assert_eq!(histogram.bin_index(0), Some(0));
        assert_eq!(histogram.bin_index(15), Some(0));
        assert_eq!(histogram.bin_index(16), Some(1));
        assert_eq!(histogram.bin_code_range(0), Some((0, 15)));
        assert_eq!(histogram.bin_code_range(4095), Some((65_520, 65_535)));
        assert_eq!(histogram.bin_code_range(4096), None);
    }
}
