//! CV610/PQTools 使用的小端连续位流 RAW10/RAW12 解包。

use crate::{BayerPattern, RawFrame, RawFrameError, RawSpec};
use thiserror::Error;

/// 带逐行 stride 的 packed RAW 描述。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackedRawSpec {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub bit_depth: u8,
}

impl PackedRawSpec {
    /// 计算每行有效 packed 字节数，不包含 stride padding。
    ///
    /// # Errors
    ///
    /// 仅接受已验证的 10/12-bit；尺寸、算术或 stride 无效时返回错误。
    pub fn packed_row_bytes(self) -> Result<usize, RawPackingError> {
        if !matches!(self.bit_depth, 10 | 12) {
            return Err(RawPackingError::UnsupportedBitDepth(self.bit_depth));
        }
        if self.width == 0 || self.height == 0 {
            return Err(RawPackingError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }
        let width = usize::try_from(self.width).map_err(|_| RawPackingError::SizeOverflow)?;
        let bits = width
            .checked_mul(usize::from(self.bit_depth))
            .ok_or(RawPackingError::SizeOverflow)?;
        let packed = bits.checked_add(7).ok_or(RawPackingError::SizeOverflow)? / 8;
        if self.stride < packed {
            return Err(RawPackingError::StrideTooSmall {
                stride: self.stride,
                minimum: packed,
            });
        }
        Ok(packed)
    }

    /// 计算包含逐行 padding 的完整 source payload 长度。
    ///
    /// # Errors
    ///
    /// 描述无效或 `stride * height` 溢出时返回错误。
    pub fn payload_len(self) -> Result<usize, RawPackingError> {
        self.packed_row_bytes()?;
        let height = usize::try_from(self.height).map_err(|_| RawPackingError::SizeOverflow)?;
        self.stride
            .checked_mul(height)
            .ok_or(RawPackingError::SizeOverflow)
    }
}

/// 逐行跳过 padding，将小端连续位流解包为紧密 `u16` RAW。
///
/// 位深值保持在原始 10/12-bit 数值域，不做左移或归一化。
///
/// # Errors
///
/// 描述无效、payload 长度不闭合或输出尺寸溢出时返回错误。
pub fn decode_le_continuous_raw(
    packed: PackedRawSpec,
    bayer: BayerPattern,
    payload: &[u8],
) -> Result<RawFrame, RawPackingError> {
    let packed_row_bytes = packed.packed_row_bytes()?;
    let expected = packed.payload_len()?;
    if payload.len() != expected {
        return Err(RawPackingError::PayloadLengthMismatch {
            expected,
            actual: payload.len(),
        });
    }

    let width = usize::try_from(packed.width).map_err(|_| RawPackingError::SizeOverflow)?;
    let height = usize::try_from(packed.height).map_err(|_| RawPackingError::SizeOverflow)?;
    let pixel_count = width
        .checked_mul(height)
        .ok_or(RawPackingError::SizeOverflow)?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(pixel_count)
        .map_err(|_| RawPackingError::AllocationFailed { bytes: pixel_count })?;

    let mask = (1_u32 << packed.bit_depth) - 1;
    for row in payload.chunks_exact(packed.stride) {
        let source = &row[..packed_row_bytes];
        let mut accumulator = 0_u32;
        let mut available_bits = 0_u8;
        let mut byte_index = 0_usize;
        for _ in 0..width {
            while available_bits < packed.bit_depth {
                let byte = source
                    .get(byte_index)
                    .copied()
                    .ok_or(RawPackingError::PackedRowTruncated)?;
                accumulator |= u32::from(byte) << available_bits;
                available_bits += 8;
                byte_index += 1;
            }
            pixels.push((accumulator & mask) as u16);
            accumulator >>= packed.bit_depth;
            available_bits -= packed.bit_depth;
        }
        if byte_index != packed_row_bytes {
            return Err(RawPackingError::PackedRowNotFullyConsumed {
                consumed: byte_index,
                expected: packed_row_bytes,
            });
        }
    }

    RawFrame::new(
        RawSpec {
            width: packed.width,
            height: packed.height,
            bit_depth: packed.bit_depth,
            bayer,
        },
        pixels,
    )
    .map_err(RawPackingError::RawFrame)
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RawPackingError {
    #[error("only little-endian continuous RAW10/RAW12 is supported, got {0}-bit")]
    UnsupportedBitDepth(u8),
    #[error("packed RAW dimensions must be non-zero: width={width}, height={height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("packed RAW size overflows host address space")]
    SizeOverflow,
    #[error("packed RAW stride is too small: stride={stride}, minimum={minimum}")]
    StrideTooSmall { stride: usize, minimum: usize },
    #[error("packed RAW payload length mismatch: expected {expected}, got {actual}")]
    PayloadLengthMismatch { expected: usize, actual: usize },
    #[error("packed RAW row ended before all pixels were decoded")]
    PackedRowTruncated,
    #[error("packed RAW row was not fully consumed: expected {expected}, got {consumed}")]
    PackedRowNotFullyConsumed { consumed: usize, expected: usize },
    #[error("failed to allocate {bytes} RAW pixels")]
    AllocationFailed { bytes: usize },
    #[error(transparent)]
    RawFrame(#[from] RawFrameError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(values: &[u16], bit_depth: u8) -> Vec<u8> {
        let mut output = Vec::new();
        let mut accumulator = 0_u32;
        let mut bits = 0_u8;
        for &value in values {
            accumulator |= u32::from(value) << bits;
            bits += bit_depth;
            while bits >= 8 {
                output.push(accumulator as u8);
                accumulator >>= 8;
                bits -= 8;
            }
        }
        if bits != 0 {
            output.push(accumulator as u8);
        }
        output
    }

    #[test]
    fn decodes_non_byte_aligned_rows_and_skips_padding() {
        for (bit_depth, rows, stride) in [
            (10, vec![vec![1, 0x155, 0x3ff], vec![0x2aa, 7, 0]], 6),
            (12, vec![vec![1, 0xabc, 0xfff], vec![0x123, 9, 0]], 7),
        ] {
            let mut payload = Vec::new();
            for (row_index, values) in rows.iter().enumerate() {
                let encoded = pack(values, bit_depth);
                payload.extend_from_slice(&encoded);
                payload.resize(
                    payload.len() + stride - encoded.len(),
                    0xa0 + row_index as u8,
                );
            }
            let frame = decode_le_continuous_raw(
                PackedRawSpec {
                    width: 3,
                    height: 2,
                    stride,
                    bit_depth,
                },
                BayerPattern::Bggr,
                &payload,
            )
            .unwrap();
            let expected: Vec<u16> = rows.into_iter().flatten().collect();
            assert_eq!(frame.pixels(), expected);
        }
    }

    #[test]
    fn rejects_short_stride_and_payload() {
        let spec = PackedRawSpec {
            width: 3,
            height: 2,
            stride: 3,
            bit_depth: 10,
        };
        assert_eq!(
            spec.packed_row_bytes().unwrap_err(),
            RawPackingError::StrideTooSmall {
                stride: 3,
                minimum: 4
            }
        );

        let spec = PackedRawSpec { stride: 4, ..spec };
        assert_eq!(
            decode_le_continuous_raw(spec, BayerPattern::Rggb, &[0; 7]).unwrap_err(),
            RawPackingError::PayloadLengthMismatch {
                expected: 8,
                actual: 7
            }
        );
    }
}
