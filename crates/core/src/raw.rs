//! RAW 图像描述与像素缓冲。

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BayerPattern {
    Rggb,
    Grbg,
    Gbrg,
    Bggr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawSpec {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub stride_pixels: u32,
    pub bayer: BayerPattern,
}

impl RawSpec {
    pub fn validate(&self) -> Result<(), RawFrameError> {
        if self.width == 0 || self.height == 0 {
            return Err(RawFrameError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }
        if self.stride_pixels < self.width {
            return Err(RawFrameError::InvalidStride {
                width: self.width,
                stride_pixels: self.stride_pixels,
            });
        }
        if !(1..=16).contains(&self.bit_depth) {
            return Err(RawFrameError::InvalidBitDepth {
                bit_depth: self.bit_depth,
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn pixel_count(&self) -> usize {
        (self.stride_pixels as usize) * (self.height as usize)
    }

    #[must_use]
    pub const fn max_code_value(&self) -> u16 {
        if self.bit_depth >= 16 {
            u16::MAX
        } else {
            ((1u32 << self.bit_depth) - 1) as u16
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    pub spec: RawSpec,
    pixels: Vec<u16>,
}

impl RawFrame {
    /// 创建已解包 RAW 帧；长度必须等于 `stride_pixels * height`。
    pub fn new(spec: RawSpec, pixels: Vec<u16>) -> Result<Self, RawFrameError> {
        spec.validate()?;
        let expected = spec.pixel_count();
        let actual = pixels.len();
        if actual != expected {
            return Err(RawFrameError::PixelCountMismatch { expected, actual });
        }
        Ok(Self { spec, pixels })
    }

    #[must_use]
    pub fn pixels(&self) -> &[u16] {
        &self.pixels
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RawFrameError {
    #[error("raw pixel count mismatch: expected {expected}, got {actual}")]
    PixelCountMismatch { expected: usize, actual: usize },
    #[error("raw dimensions must be non-zero: width={width}, height={height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("raw stride must be >= width: width={width}, stride_pixels={stride_pixels}")]
    InvalidStride { width: u32, stride_pixels: u32 },
    #[error("raw bit depth must be in 1..=16, got {bit_depth}")]
    InvalidBitDepth { bit_depth: u8 },
}
