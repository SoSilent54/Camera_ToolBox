//! 多格式静态图像的原生帧、平面布局与像素语义。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{CfaSite, ChromaOrder, RawFrame};

/// Y′CbCr 与 R′G′B′ 之间使用的矩阵。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YuvMatrix {
    Bt601,
    Bt709,
}

/// 8-bit Y′CbCr code 的取值范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YuvRange {
    Limited,
    Full,
}

/// 带 stride 的不可变 8-bit plane；多个 plane 可共享同一底层 source。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePlane {
    storage: Arc<Vec<u8>>,
    offset: usize,
    stride: usize,
    rows: usize,
}

impl ImagePlane {
    /// 构造共享存储的 plane。
    ///
    /// # Errors
    ///
    /// stride/rows 为零、尺寸溢出或 plane 超出底层存储时返回错误。
    pub fn new(
        storage: Arc<Vec<u8>>,
        offset: usize,
        stride: usize,
        rows: usize,
    ) -> Result<Self, ImageFrameError> {
        if stride == 0 || rows == 0 {
            return Err(ImageFrameError::InvalidPlaneExtent { stride, rows });
        }
        let byte_len = stride
            .checked_mul(rows)
            .ok_or(ImageFrameError::SizeOverflow)?;
        let end = offset
            .checked_add(byte_len)
            .ok_or(ImageFrameError::SizeOverflow)?;
        if end > storage.len() {
            return Err(ImageFrameError::PlaneOutOfBounds {
                offset,
                byte_len,
                storage_len: storage.len(),
            });
        }
        Ok(Self {
            storage,
            offset,
            stride,
            rows,
        })
    }

    #[must_use]
    pub const fn stride(&self) -> usize {
        self.stride
    }

    #[must_use]
    pub const fn rows(&self) -> usize {
        self.rows
    }

    #[must_use]
    pub const fn byte_len(&self) -> usize {
        self.stride * self.rows
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.storage[self.offset..self.offset + self.byte_len()]
    }

    #[must_use]
    pub fn row(&self, row: usize) -> Option<&[u8]> {
        if row >= self.rows {
            return None;
        }
        let start = self.offset + row * self.stride;
        Some(&self.storage[start..start + self.stride])
    }
}

/// 带显式 stride 的 RGBA8 原生/显示帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba8Frame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pixels: Arc<[u8]>,
}

impl Rgba8Frame {
    /// 构造 RGBA8 帧；buffer 必须精确覆盖 `stride * height`。
    ///
    /// # Errors
    ///
    /// 尺寸无效、stride 不足、尺寸溢出或 buffer 长度不匹配时返回错误。
    pub fn new(
        width: u32,
        height: u32,
        stride: usize,
        pixels: Arc<[u8]>,
    ) -> Result<Self, ImageFrameError> {
        if width == 0 || height == 0 {
            return Err(ImageFrameError::InvalidDimensions { width, height });
        }
        let row_bytes = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(ImageFrameError::SizeOverflow)?;
        if stride < row_bytes {
            return Err(ImageFrameError::StrideTooSmall {
                plane: "rgba",
                stride,
                minimum: row_bytes,
            });
        }
        let expected = stride
            .checked_mul(height as usize)
            .ok_or(ImageFrameError::SizeOverflow)?;
        if pixels.len() != expected {
            return Err(ImageFrameError::BufferLengthMismatch {
                plane: "rgba",
                expected,
                actual: pixels.len(),
            });
        }
        Ok(Self {
            width,
            height,
            stride,
            pixels,
        })
    }

    /// 构造紧密排列的 RGBA8 帧。
    ///
    /// # Errors
    ///
    /// 规格或 buffer 长度无效时返回错误。
    pub fn tight(width: u32, height: u32, pixels: Arc<[u8]>) -> Result<Self, ImageFrameError> {
        let stride = (width as usize)
            .checked_mul(4)
            .ok_or(ImageFrameError::SizeOverflow)?;
        Self::new(width, height, stride, pixels)
    }

    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let start = y as usize * self.stride + x as usize * 4;
        Some([
            self.pixels[start],
            self.pixels[start + 1],
            self.pixels[start + 2],
            self.pixels[start + 3],
        ])
    }
}

/// 8-bit NV12/NV21 的完整布局与颜色解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Yuv420SpSpec {
    pub width: u32,
    pub height: u32,
    pub y_stride: usize,
    pub chroma_stride: usize,
    pub chroma_order: ChromaOrder,
    pub matrix: YuvMatrix,
    pub range: YuvRange,
}

impl Yuv420SpSpec {
    /// 校验 4:2:0SP 的偶数尺寸、stride 与总长度。
    ///
    /// # Errors
    ///
    /// 尺寸、stride 或长度计算无效时返回错误。
    pub fn validate(&self) -> Result<(), ImageFrameError> {
        if self.width == 0 || self.height == 0 {
            return Err(ImageFrameError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }
        if self.width % 2 != 0 || self.height % 2 != 0 {
            return Err(ImageFrameError::Yuv420RequiresEvenDimensions {
                width: self.width,
                height: self.height,
            });
        }
        let width = self.width as usize;
        if self.y_stride < width {
            return Err(ImageFrameError::StrideTooSmall {
                plane: "y",
                stride: self.y_stride,
                minimum: width,
            });
        }
        if self.chroma_stride < width {
            return Err(ImageFrameError::StrideTooSmall {
                plane: "chroma",
                stride: self.chroma_stride,
                minimum: width,
            });
        }
        if self.chroma_stride % 2 != 0 {
            return Err(ImageFrameError::ChromaStrideMustBeEven(self.chroma_stride));
        }
        self.y_len()?;
        self.chroma_len()?;
        Ok(())
    }

    /// 返回 Y plane 字节数。
    ///
    /// # Errors
    ///
    /// 尺寸溢出时返回错误。
    pub fn y_len(&self) -> Result<usize, ImageFrameError> {
        self.y_stride
            .checked_mul(self.height as usize)
            .ok_or(ImageFrameError::SizeOverflow)
    }

    /// 返回交错 chroma plane 字节数。
    ///
    /// # Errors
    ///
    /// 尺寸溢出时返回错误。
    pub fn chroma_len(&self) -> Result<usize, ImageFrameError> {
        self.chroma_stride
            .checked_mul(self.height as usize / 2)
            .ok_or(ImageFrameError::SizeOverflow)
    }

    /// 返回无 header 连续 payload 的精确长度。
    ///
    /// # Errors
    ///
    /// 尺寸溢出时返回错误。
    pub fn payload_len(&self) -> Result<usize, ImageFrameError> {
        self.y_len()?
            .checked_add(self.chroma_len()?)
            .ok_or(ImageFrameError::SizeOverflow)
    }
}

/// 权威 8-bit NV12/NV21 帧；Y 与 chroma plane 保留原始 code。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Yuv420SpFrame {
    pub spec: Yuv420SpSpec,
    y: ImagePlane,
    chroma: ImagePlane,
}

impl Yuv420SpFrame {
    /// 从连续 Y + UV/VU payload 构造零拷贝 plane 视图。
    ///
    /// # Errors
    ///
    /// spec 无效或 payload 长度不匹配时返回错误。
    pub fn from_contiguous(
        spec: Yuv420SpSpec,
        bytes: Arc<Vec<u8>>,
    ) -> Result<Self, ImageFrameError> {
        spec.validate()?;
        let expected = spec.payload_len()?;
        if bytes.len() != expected {
            return Err(ImageFrameError::BufferLengthMismatch {
                plane: "yuv420sp",
                expected,
                actual: bytes.len(),
            });
        }
        let y_len = spec.y_len()?;
        let y = ImagePlane::new(Arc::clone(&bytes), 0, spec.y_stride, spec.height as usize)?;
        let chroma = ImagePlane::new(bytes, y_len, spec.chroma_stride, spec.height as usize / 2)?;
        Ok(Self { spec, y, chroma })
    }

    #[must_use]
    pub const fn y_plane(&self) -> &ImagePlane {
        &self.y
    }

    #[must_use]
    pub const fn chroma_plane(&self) -> &ImagePlane {
        &self.chroma
    }

    #[must_use]
    pub fn sample(&self, x: u32, y: u32) -> Option<(u8, u8, u8)> {
        if x >= self.spec.width || y >= self.spec.height {
            return None;
        }
        let luma = self.y.row(y as usize)?[x as usize];
        let chroma_row = self.chroma.row(y as usize / 2)?;
        let pair = x as usize / 2 * 2;
        let first = chroma_row[pair];
        let second = chroma_row[pair + 1];
        let (u, v) = match self.spec.chroma_order {
            ChromaOrder::Uv => (first, second),
            ChromaOrder::Vu => (second, first),
        };
        Some((luma, u, v))
    }
}

/// Viewer/Analysis 使用的权威原生图像；各变体均保留通道语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeImage {
    Raw(Arc<RawFrame>),
    Rgba8(Arc<Rgba8Frame>),
    Yuv420Sp(Arc<Yuv420SpFrame>),
}

impl NativeImage {
    #[must_use]
    pub fn dimensions(&self) -> [u32; 2] {
        match self {
            Self::Raw(frame) => [frame.spec.width, frame.spec.height],
            Self::Rgba8(frame) => [frame.width, frame.height],
            Self::Yuv420Sp(frame) => [frame.spec.width, frame.spec.height],
        }
    }

    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        match self {
            Self::Raw(frame) => frame
                .pixels()
                .len()
                .saturating_mul(std::mem::size_of::<u16>()),
            Self::Rgba8(frame) => frame.pixels().len(),
            Self::Yuv420Sp(frame) => frame
                .y_plane()
                .byte_len()
                .saturating_add(frame.chroma_plane().byte_len()),
        }
    }

    #[must_use]
    pub fn sample_at(&self, x: u32, y: u32) -> Option<NativePixelSample> {
        match self {
            Self::Raw(frame) => {
                if x >= frame.spec.width || y >= frame.spec.height {
                    return None;
                }
                let index = y as usize * frame.spec.width as usize + x as usize;
                Some(NativePixelSample::Raw {
                    site: frame.spec.bayer.site_at(x, y),
                    value: frame.pixels()[index],
                    max_code: frame.spec.max_code_value(),
                })
            }
            Self::Rgba8(frame) => {
                let [r, g, b, a] = frame.pixel(x, y)?;
                Some(NativePixelSample::Rgba { r, g, b, a })
            }
            Self::Yuv420Sp(frame) => {
                let (luma, u, v) = frame.sample(x, y)?;
                Some(NativePixelSample::Yuv {
                    y: luma,
                    u,
                    v,
                    chroma_x: x / 2,
                    chroma_y: y / 2,
                })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePixelSample {
    Raw {
        site: CfaSite,
        value: u16,
        max_code: u16,
    },
    Rgba {
        r: u8,
        g: u8,
        b: u8,
        a: u8,
    },
    Yuv {
        y: u8,
        u: u8,
        v: u8,
        chroma_x: u32,
        chroma_y: u32,
    },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ImageFrameError {
    #[error("image dimensions must be non-zero: width={width}, height={height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("image dimensions overflow host address space")]
    SizeOverflow,
    #[error("{plane} stride {stride} is smaller than required {minimum}")]
    StrideTooSmall {
        plane: &'static str,
        stride: usize,
        minimum: usize,
    },
    #[error("{plane} buffer length mismatch: expected {expected}, got {actual}")]
    BufferLengthMismatch {
        plane: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("invalid image plane extent: stride={stride}, rows={rows}")]
    InvalidPlaneExtent { stride: usize, rows: usize },
    #[error(
        "image plane exceeds storage: offset={offset}, length={byte_len}, storage={storage_len}"
    )]
    PlaneOutOfBounds {
        offset: usize,
        byte_len: usize,
        storage_len: usize,
    },
    #[error("YUV420SP requires even dimensions: width={width}, height={height}")]
    Yuv420RequiresEvenDimensions { width: u32, height: u32 },
    #[error("YUV420SP chroma stride must be even, got {0}")]
    ChromaStrideMustBeEven(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BayerPattern, RawSpec};

    fn yuv_spec() -> Yuv420SpSpec {
        Yuv420SpSpec {
            width: 4,
            height: 2,
            y_stride: 4,
            chroma_stride: 4,
            chroma_order: ChromaOrder::Uv,
            matrix: YuvMatrix::Bt601,
            range: YuvRange::Limited,
        }
    }

    #[test]
    fn rgba_requires_exact_stride_extent() {
        let error = Rgba8Frame::new(2, 2, 8, Arc::from([0_u8; 15])).unwrap_err();
        assert_eq!(
            error,
            ImageFrameError::BufferLengthMismatch {
                plane: "rgba",
                expected: 16,
                actual: 15,
            }
        );
    }

    #[test]
    fn yuv420_rejects_odd_dimensions() {
        let mut spec = yuv_spec();
        spec.width = 3;
        assert_eq!(
            spec.validate(),
            Err(ImageFrameError::Yuv420RequiresEvenDimensions {
                width: 3,
                height: 2,
            })
        );
    }

    #[test]
    fn contiguous_yuv_planes_share_layout_and_preserve_order() {
        let bytes = Arc::new(vec![16, 17, 18, 19, 20, 21, 22, 23, 90, 140, 100, 150]);
        let frame = Yuv420SpFrame::from_contiguous(yuv_spec(), bytes).unwrap();
        assert_eq!(frame.y_plane().bytes(), &[16, 17, 18, 19, 20, 21, 22, 23]);
        assert_eq!(frame.chroma_plane().bytes(), &[90, 140, 100, 150]);
        assert_eq!(frame.sample(3, 1), Some((23, 100, 150)));
    }

    #[test]
    fn native_sampling_retains_channel_semantics() {
        let raw = RawFrame::new(
            RawSpec {
                width: 2,
                height: 2,
                bit_depth: 10,
                bayer: BayerPattern::Rggb,
            },
            vec![1, 2, 3, 4],
        )
        .unwrap();
        let image = NativeImage::Raw(Arc::new(raw));
        assert_eq!(
            image.sample_at(1, 0),
            Some(NativePixelSample::Raw {
                site: CfaSite::Gr,
                value: 2,
                max_code: 1023,
            })
        );
    }
}
