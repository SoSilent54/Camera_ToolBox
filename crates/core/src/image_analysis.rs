//! RGBA8 与 YUV420SP 的原生通道 ROI 分析。

use crate::{AnalysisError, Rgba8Frame, Roi, Yuv420SpFrame};

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelAnalysis8 {
    pub min: u8,
    pub max: u8,
    pub mean: f64,
    pub total_samples: u64,
    pub histogram: Box<[u64; 256]>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RgbRoiAnalysis {
    pub roi: Roi,
    pub r: ChannelAnalysis8,
    pub g: ChannelAnalysis8,
    pub b: ChannelAnalysis8,
    pub a: ChannelAnalysis8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct YuvRoiAnalysis {
    pub roi: Roi,
    pub y: ChannelAnalysis8,
    pub u: ChannelAnalysis8,
    pub v: ChannelAnalysis8,
}

/// 分析 RGBA8 原生通道；取消检查以行为粒度。
///
/// # Errors
///
/// ROI 裁剪后为空或取消时返回错误。
pub fn analyze_rgba8_with_cancel<F>(
    frame: &Rgba8Frame,
    roi: Roi,
    mut is_cancelled: F,
) -> Result<RgbRoiAnalysis, AnalysisError>
where
    F: FnMut() -> bool,
{
    let roi = roi
        .clamped_to(frame.width, frame.height)
        .ok_or(AnalysisError::EmptyRoi)?;
    let mut channels = [
        ChannelAccumulator::default(),
        ChannelAccumulator::default(),
        ChannelAccumulator::default(),
        ChannelAccumulator::default(),
    ];

    for y in roi.y..roi.y + roi.height {
        if is_cancelled() {
            return Err(AnalysisError::Cancelled);
        }
        for x in roi.x..roi.x + roi.width {
            let pixel = frame
                .pixel(x, y)
                .expect("clamped ROI keeps pixels in bounds");
            for (accumulator, value) in channels.iter_mut().zip(pixel) {
                accumulator.record(value);
            }
        }
    }

    let [r, g, b, a] = channels.map(ChannelAccumulator::finish);
    Ok(RgbRoiAnalysis { roi, r, g, b, a })
}

/// 分析 YUV420SP 原生 code；U/V 以共享 chroma sample 为统计单位。
///
/// ROI 覆盖任意一个 2×2 luma 像素时，对应 U/V sample 只计一次。
///
/// # Errors
///
/// ROI 裁剪后为空或取消时返回错误。
pub fn analyze_yuv420sp_with_cancel<F>(
    frame: &Yuv420SpFrame,
    roi: Roi,
    mut is_cancelled: F,
) -> Result<YuvRoiAnalysis, AnalysisError>
where
    F: FnMut() -> bool,
{
    let roi = roi
        .clamped_to(frame.spec.width, frame.spec.height)
        .ok_or(AnalysisError::EmptyRoi)?;
    let mut y_accumulator = ChannelAccumulator::default();
    let mut u_accumulator = ChannelAccumulator::default();
    let mut v_accumulator = ChannelAccumulator::default();

    for y in roi.y..roi.y + roi.height {
        if is_cancelled() {
            return Err(AnalysisError::Cancelled);
        }
        let row = frame
            .y_plane()
            .row(y as usize)
            .expect("clamped ROI keeps luma row in bounds");
        for x in roi.x..roi.x + roi.width {
            y_accumulator.record(row[x as usize]);
        }
    }

    let chroma_x_start = roi.x / 2;
    let chroma_x_end = (roi.x + roi.width).div_ceil(2);
    let chroma_y_start = roi.y / 2;
    let chroma_y_end = (roi.y + roi.height).div_ceil(2);
    for chroma_y in chroma_y_start..chroma_y_end {
        if is_cancelled() {
            return Err(AnalysisError::Cancelled);
        }
        for chroma_x in chroma_x_start..chroma_x_end {
            let (_, u, v) = frame
                .sample(chroma_x * 2, chroma_y * 2)
                .expect("mapped chroma ROI stays in bounds");
            u_accumulator.record(u);
            v_accumulator.record(v);
        }
    }

    Ok(YuvRoiAnalysis {
        roi,
        y: y_accumulator.finish(),
        u: u_accumulator.finish(),
        v: v_accumulator.finish(),
    })
}

#[derive(Debug, Clone)]
struct ChannelAccumulator {
    min: u8,
    max: u8,
    sum: u128,
    total_samples: u64,
    histogram: Box<[u64; 256]>,
}

impl Default for ChannelAccumulator {
    fn default() -> Self {
        Self {
            min: u8::MAX,
            max: u8::MIN,
            sum: 0,
            total_samples: 0,
            histogram: Box::new([0; 256]),
        }
    }
}

impl ChannelAccumulator {
    fn record(&mut self, value: u8) {
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum = self.sum.saturating_add(u128::from(value));
        self.total_samples = self.total_samples.saturating_add(1);
        self.histogram[value as usize] = self.histogram[value as usize].saturating_add(1);
    }

    #[allow(clippy::cast_precision_loss)]
    fn finish(self) -> ChannelAnalysis8 {
        debug_assert!(self.total_samples > 0);
        ChannelAnalysis8 {
            min: self.min,
            max: self.max,
            mean: self.sum as f64 / self.total_samples as f64,
            total_samples: self.total_samples,
            histogram: self.histogram,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{ChromaOrder, Yuv420SpSpec, YuvMatrix, YuvRange};

    use super::*;

    #[test]
    fn rgba_analysis_preserves_alpha_and_histograms() {
        let frame =
            Rgba8Frame::tight(2, 1, Arc::from([10_u8, 20, 30, 40, 50, 60, 70, 80])).unwrap();
        let result = analyze_rgba8_with_cancel(
            &frame,
            Roi {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            || false,
        )
        .unwrap();
        assert_eq!(result.r.min, 10);
        assert_eq!(result.r.max, 50);
        assert_eq!(result.a.mean, 60.0);
        assert_eq!(result.r.histogram[10], 1);
        assert_eq!(result.r.histogram[50], 1);
    }

    #[test]
    fn yuv_partial_roi_counts_shared_chroma_once() {
        let spec = Yuv420SpSpec {
            width: 4,
            height: 4,
            y_stride: 4,
            chroma_stride: 4,
            chroma_order: ChromaOrder::Uv,
            matrix: YuvMatrix::Bt601,
            range: YuvRange::Limited,
        };
        let frame = Yuv420SpFrame::from_contiguous(
            spec,
            Arc::new(vec![
                1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 20, 30, 40, 50, 60, 70,
                80, 90,
            ]),
        )
        .unwrap();
        let roi = Roi {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let result = analyze_yuv420sp_with_cancel(&frame, roi, || false).unwrap();
        assert_eq!(result.y.total_samples, 4);
        assert_eq!(result.u.total_samples, 4);
        assert_eq!(result.v.total_samples, 4);
        assert_eq!(result.u.histogram[20], 1);
        assert_eq!(result.v.histogram[90], 1);
    }

    #[test]
    fn native_analysis_observes_cancellation() {
        let frame = Rgba8Frame::tight(1, 1, Arc::from([0_u8, 0, 0, 255])).unwrap();
        assert_eq!(
            analyze_rgba8_with_cancel(
                &frame,
                Roi {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                || true,
            ),
            Err(AnalysisError::Cancelled)
        );
    }
}
