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
    /// 校验分辨率与有效位深。
    ///
    /// # Errors
    ///
    /// 分辨率为零或有效位深不在 `1..=16` 时返回错误。
    pub fn validate(&self) -> Result<(), RawFrameError> {
        if self.width == 0 || self.height == 0 {
            return Err(RawFrameError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }
        if !(1..=16).contains(&self.bit_depth) {
            return Err(RawFrameError::InvalidBitDepth {
                bit_depth: self.bit_depth,
            });
        }
        Ok(())
    }

    /// 计算紧密排列的像素数量。
    ///
    /// # Errors
    ///
    /// `width * height` 超出当前平台地址空间时返回错误。
    pub fn pixel_count(&self) -> Result<usize, RawFrameError> {
        (self.width as usize)
            .checked_mul(self.height as usize)
            .ok_or(RawFrameError::SizeOverflow)
    }

    #[must_use]
    pub const fn max_code_value(&self) -> u16 {
        if self.bit_depth >= 16 {
            u16::MAX
        } else {
            (1u16 << self.bit_depth) - 1
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    pub spec: RawSpec,
    pixels: Vec<u16>,
}

impl RawFrame {
    /// 创建紧密排列的已解包 RAW 帧；长度必须等于 `width * height`。
    ///
    /// # Errors
    ///
    /// RAW 规格无效、尺寸溢出或像素数量不匹配时返回错误。
    pub fn new(spec: RawSpec, pixels: Vec<u16>) -> Result<Self, RawFrameError> {
        spec.validate()?;
        let expected = spec.pixel_count()?;
        let actual = pixels.len();
        if actual != expected {
            return Err(RawFrameError::PixelCountMismatch { expected, actual });
        }
        Ok(Self { spec, pixels })
    }

    /// 从本地 RAW 字节创建帧；当前只支持已解包 `u16le` 存储。
    ///
    /// # Errors
    ///
    /// RAW 规格无效、尺寸溢出或字节数量不匹配时返回错误。
    pub fn from_bytes(
        spec: RawSpec,
        encoding: RawEncoding,
        bytes: &[u8],
    ) -> Result<Self, RawFrameError> {
        spec.validate()?;
        let pixel_count = spec.pixel_count()?;
        let expected = pixel_count
            .checked_mul(encoding.bytes_per_pixel())
            .ok_or(RawFrameError::SizeOverflow)?;
        let actual = bytes.len();
        if actual != expected {
            return Err(RawFrameError::ByteCountMismatch { expected, actual });
        }

        let mut pixels = Vec::with_capacity(pixel_count);
        match encoding {
            RawEncoding::U16Le => pixels.extend(
                bytes
                    .chunks_exact(2)
                    .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]])),
            ),
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
    #[error("raw dimensions must be non-zero: width={width}, height={height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("raw dimensions overflow host address space")]
    SizeOverflow,
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
    fn preserves_values_above_declared_bit_depth() {
        let bytes = [0, 0, 0, 4, 0, 0, 0, 0];
        let frame = RawFrame::from_bytes(test_spec(), RawEncoding::U16Le, &bytes).unwrap();

        assert_eq!(frame.pixels(), &[0, 1024, 0, 0]);
    }
}
