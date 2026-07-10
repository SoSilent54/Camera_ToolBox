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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawEncoding {
    /// 已解包的 16-bit little-endian 灰度 RAW；RAW10/12 packed 暂不支持。
    U16Le,
}

impl RawEncoding {
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::U16Le => 2,
        }
    }
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

    /// 从本地 RAW 字节创建帧；当前只支持已解包 `u16le` 存储。
    pub fn from_bytes(
        spec: RawSpec,
        encoding: RawEncoding,
        bytes: &[u8],
    ) -> Result<Self, RawFrameError> {
        spec.validate()?;
        let expected = spec.pixel_count() * encoding.bytes_per_pixel();
        let actual = bytes.len();
        if actual != expected {
            return Err(RawFrameError::ByteCountMismatch { expected, actual });
        }

        let max_code_value = spec.max_code_value();
        let mut pixels = Vec::with_capacity(spec.pixel_count());
        match encoding {
            RawEncoding::U16Le => {
                for chunk in bytes.chunks_exact(2) {
                    let value = u16::from_le_bytes([chunk[0], chunk[1]]);
                    if value > max_code_value {
                        return Err(RawFrameError::PixelValueOutOfRange {
                            value,
                            max: max_code_value,
                        });
                    }
                    pixels.push(value);
                }
            }
        }
        Self::new(spec, pixels)
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
    #[error("raw byte count mismatch: expected {expected}, got {actual}")]
    ByteCountMismatch { expected: usize, actual: usize },
    #[error("raw pixel value {value} exceeds max code {max}")]
    PixelValueOutOfRange { value: u16, max: u16 },
    #[error("raw dimensions must be non-zero: width={width}, height={height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("raw stride must be >= width: width={width}, stride_pixels={stride_pixels}")]
    InvalidStride { width: u32, stride_pixels: u32 },
    #[error("raw bit depth must be in 1..=16, got {bit_depth}")]
    InvalidBitDepth { bit_depth: u8 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec() -> RawSpec {
        RawSpec {
            width: 2,
            height: 2,
            bit_depth: 10,
            stride_pixels: 2,
            bayer: BayerPattern::Rggb,
        }
    }

    #[test]
    fn decodes_unpacked_u16le_bytes() {
        let bytes = [0, 0, 1, 0, 0xff, 0x03, 0, 0x02];
        let frame = RawFrame::from_bytes(test_spec(), RawEncoding::U16Le, &bytes).unwrap();

        assert_eq!(frame.pixels(), &[0, 1, 1023, 512]);
    }

    #[test]
    fn rejects_byte_count_mismatch() {
        let err = RawFrame::from_bytes(test_spec(), RawEncoding::U16Le, &[0, 0]).unwrap_err();

        assert_eq!(
            err,
            RawFrameError::ByteCountMismatch {
                expected: 8,
                actual: 2,
            }
        );
    }

    #[test]
    fn rejects_values_above_bit_depth() {
        let bytes = [0, 0, 0, 4, 0, 0, 0, 0];
        let err = RawFrame::from_bytes(test_spec(), RawEncoding::U16Le, &bytes).unwrap_err();

        assert_eq!(
            err,
            RawFrameError::PixelValueOutOfRange {
                value: 1024,
                max: 1023,
            }
        );
    }
}
