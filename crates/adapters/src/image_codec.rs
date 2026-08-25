//! `image` crate backed PNG/JPEG static raster codec。

use std::io::{Cursor, Write};
use std::sync::Arc;

use camera_toolbox_app::{RasterCodecError, RasterFormat, RasterImageCodec};
use camera_toolbox_core::{ImageFrameError, Rgba8Frame};
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, ImageFormat, ImageReader, Limits};

#[derive(Debug, Default)]
pub struct ImageRasterCodec;

impl RasterImageCodec for ImageRasterCodec {
    fn decode_rgba8(
        &self,
        format: RasterFormat,
        encoded: &[u8],
        decoded_byte_limit: usize,
    ) -> Result<Rgba8Frame, RasterCodecError> {
        validate_signature(format, encoded)?;
        let image_format = image_format(format);
        let dimensions = ImageReader::with_format(Cursor::new(encoded), image_format)
            .into_dimensions()
            .map_err(|error| codec_error(format, error))?;
        let required = decoded_rgba_len(dimensions, format)?;
        if required > decoded_byte_limit {
            return Err(RasterCodecError::DecodedImageTooLarge {
                format: format.name(),
                required,
                limit: decoded_byte_limit,
            });
        }

        let mut reader = ImageReader::with_format(Cursor::new(encoded), image_format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(dimensions.0);
        limits.max_image_height = Some(dimensions.1);
        limits.max_alloc = Some(decoded_byte_limit as u64);
        reader.limits(limits);
        let decoded = reader
            .decode()
            .map_err(|error| codec_error(format, error))?
            .into_rgba8();
        if decoded.dimensions() != dimensions {
            return Err(RasterCodecError::Codec {
                format: format.name(),
                message: "decoder dimensions changed between metadata and pixel decode".to_owned(),
            });
        }
        Rgba8Frame::tight(dimensions.0, dimensions.1, Arc::from(decoded.into_raw()))
            .map_err(|error| frame_error(format, error))
    }

    fn encode_png(
        &self,
        frame: &Rgba8Frame,
        writer: &mut dyn Write,
    ) -> Result<(), RasterCodecError> {
        let expected_stride = (frame.width as usize)
            .checked_mul(4)
            .ok_or(RasterCodecError::SizeOverflow { format: "PNG" })?;
        if frame.stride != expected_stride {
            return Err(RasterCodecError::UnsupportedRgbaStride {
                stride: frame.stride,
                expected: expected_stride,
            });
        }
        PngEncoder::new(writer)
            .write_image(
                frame.pixels(),
                frame.width,
                frame.height,
                ExtendedColorType::Rgba8,
            )
            .map_err(|error| RasterCodecError::Codec {
                format: "PNG",
                message: error.to_string(),
            })
    }
}

fn validate_signature(format: RasterFormat, encoded: &[u8]) -> Result<(), RasterCodecError> {
    let valid = match format {
        RasterFormat::Png => encoded.starts_with(b"\x89PNG\r\n\x1a\n"),
        RasterFormat::Jpeg => encoded.starts_with(&[0xff, 0xd8, 0xff]),
    };
    if valid {
        Ok(())
    } else {
        Err(RasterCodecError::SignatureMismatch {
            expected: format.name(),
        })
    }
}

const fn image_format(format: RasterFormat) -> ImageFormat {
    match format {
        RasterFormat::Png => ImageFormat::Png,
        RasterFormat::Jpeg => ImageFormat::Jpeg,
    }
}

fn decoded_rgba_len(
    (width, height): (u32, u32),
    format: RasterFormat,
) -> Result<usize, RasterCodecError> {
    if width == 0 || height == 0 {
        return Err(RasterCodecError::Codec {
            format: format.name(),
            message: format!("invalid zero dimensions {width}x{height}"),
        });
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(RasterCodecError::SizeOverflow {
            format: format.name(),
        })
}

fn codec_error(format: RasterFormat, error: image::ImageError) -> RasterCodecError {
    RasterCodecError::Codec {
        format: format.name(),
        message: error.to_string(),
    }
}

fn frame_error(format: RasterFormat, error: ImageFrameError) -> RasterCodecError {
    RasterCodecError::Codec {
        format: format.name(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame() -> Rgba8Frame {
        Rgba8Frame::tight(2, 1, Arc::from([255_u8, 0, 0, 255, 0, 255, 0, 128])).unwrap()
    }

    #[test]
    fn png_round_trip_preserves_rgba_pixels() {
        let codec = ImageRasterCodec;
        let source = sample_frame();
        let mut encoded = Vec::new();
        codec.encode_png(&source, &mut encoded).unwrap();
        let decoded = codec
            .decode_rgba8(RasterFormat::Png, &encoded, 1024)
            .unwrap();
        assert_eq!(decoded.width, 2);
        assert_eq!(decoded.height, 1);
        assert_eq!(decoded.pixels(), source.pixels());
    }

    #[test]
    fn suffix_selected_codec_rejects_signature_mismatch() {
        let codec = ImageRasterCodec;
        let error = codec
            .decode_rgba8(RasterFormat::Png, b"not a png", 1024)
            .unwrap_err();
        assert!(matches!(
            error,
            RasterCodecError::SignatureMismatch { expected: "PNG" }
        ));
    }

    #[test]
    fn decoded_byte_limit_is_checked_before_pixel_decode() {
        let codec = ImageRasterCodec;
        let source = sample_frame();
        let mut encoded = Vec::new();
        codec.encode_png(&source, &mut encoded).unwrap();
        let error = codec
            .decode_rgba8(RasterFormat::Png, &encoded, 7)
            .unwrap_err();
        assert!(matches!(
            error,
            RasterCodecError::DecodedImageTooLarge {
                required: 8,
                limit: 7,
                ..
            }
        ));
    }
}
