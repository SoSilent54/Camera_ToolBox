//! ROI 统计分析。

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::raw::RawFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoiStats {
    pub min: u16,
    pub max: u16,
    pub mean: f64,
    pub saturated_pixels: u64,
    pub total_pixels: u64,
}

impl RoiStats {
    #[must_use]
    pub fn saturation_ratio(self) -> f64 {
        self.saturated_pixels as f64 / self.total_pixels as f64
    }
}

pub fn analyze_roi(frame: &RawFrame, roi: Roi) -> Result<RoiStats, AnalysisError> {
    let roi = roi
        .clamped_to(frame.spec.width, frame.spec.height)
        .ok_or(AnalysisError::EmptyRoi)?;
    let stride = frame.spec.stride_pixels as usize;
    let saturation_code = frame.spec.max_code_value();
    let pixels = frame.pixels();

    let mut min = u16::MAX;
    let mut max = u16::MIN;
    let mut sum: u64 = 0;
    let mut saturated_pixels: u64 = 0;
    let mut total_pixels: u64 = 0;

    for row in roi.y..roi.y + roi.height {
        let row_start = row as usize * stride;
        for col in roi.x..roi.x + roi.width {
            let value = pixels[row_start + col as usize];
            min = min.min(value);
            max = max.max(value);
            sum += u64::from(value);
            saturated_pixels += u64::from(value >= saturation_code);
            total_pixels += 1;
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

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AnalysisError {
    #[error("roi is empty after clamping to image bounds")]
    EmptyRoi,
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
            stride_pixels: 2,
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
}
