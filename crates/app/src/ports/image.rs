//! 静态 raster 图像编解码端口。

use std::io::Write;

use camera_toolbox_core::Rgba8Frame;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RasterFormat {
    Png,
    Jpeg,
}

impl RasterFormat {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
        }
    }
}

/// 由 adapter 实现的无状态 raster 编解码能力。
pub trait RasterImageCodec: Send + Sync {
    /// 解码为权威 RGBA8；实现必须在分配前执行 decoded-byte limit。
    ///
    /// # Errors
    ///
    /// signature/格式不匹配、数据损坏、尺寸溢出或超出预算时返回错误。
    fn decode_rgba8(
        &self,
        format: RasterFormat,
        encoded: &[u8],
        decoded_byte_limit: usize,
    ) -> Result<Rgba8Frame, RasterCodecError>;

    /// 将 RGBA8 编码为 PNG 并写入调用方提供的 sink。
    ///
    /// # Errors
    ///
    /// 输入布局无效或编码/写入失败时返回错误。
    fn encode_png(
        &self,
        frame: &Rgba8Frame,
        writer: &mut dyn Write,
    ) -> Result<(), RasterCodecError>;
}

#[derive(Debug, Error)]
pub enum RasterCodecError {
    #[error("input is not valid {expected} data")]
    SignatureMismatch { expected: &'static str },
    #[error("{format} dimensions overflow host address space")]
    SizeOverflow { format: &'static str },
    #[error("decoded {format} requires {required} bytes, limit is {limit} bytes")]
    DecodedImageTooLarge {
        format: &'static str,
        required: usize,
        limit: usize,
    },
    #[error("{format} codec error: {message}")]
    Codec {
        format: &'static str,
        message: String,
    },
    #[error("PNG encoding requires tightly packed RGBA8; stride={stride}, expected={expected}")]
    UnsupportedRgbaStride { stride: usize, expected: usize },
    #[error("PNG output failed: {0}")]
    Output(#[source] std::io::Error),
}
