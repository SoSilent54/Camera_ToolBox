//! 8-bit NV12/NV21 与 RGBA8 的确定性定点转换。

use std::sync::Arc;

use thiserror::Error;

use crate::{
    ChromaOrder, ImageFrameError, Rgba8Frame, Yuv420SpFrame, Yuv420SpSpec, YuvMatrix, YuvRange,
};

/// 将完整 8-bit NV12/NV21 帧转换为紧密 RGBA8；取消检查以行为粒度。
///
/// # Errors
///
/// 帧布局无效、目标尺寸溢出、内存预留失败或取消时返回错误。
pub fn yuv420sp_to_rgba8_with_cancel<F>(
    frame: &Yuv420SpFrame,
    mut is_cancelled: F,
) -> Result<Rgba8Frame, ImageConversionError>
where
    F: FnMut() -> bool,
{
    frame.spec.validate()?;
    let pixel_count = (frame.spec.width as usize)
        .checked_mul(frame.spec.height as usize)
        .ok_or(ImageFrameError::SizeOverflow)?;
    let byte_len = pixel_count
        .checked_mul(4)
        .ok_or(ImageFrameError::SizeOverflow)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(byte_len)
        .map_err(|_| ImageConversionError::AllocationFailed { bytes: byte_len })?;

    for y in 0..frame.spec.height {
        if is_cancelled() {
            return Err(ImageConversionError::Cancelled);
        }
        for x in 0..frame.spec.width {
            let (luma, u, v) = frame
                .sample(x, y)
                .expect("validated dimensions keep samples in bounds");
            let [r, g, b] = yuv_to_rgb(luma, u, v, frame.spec.matrix, frame.spec.range);
            output.extend_from_slice(&[r, g, b, u8::MAX]);
        }
    }

    Ok(Rgba8Frame::tight(
        frame.spec.width,
        frame.spec.height,
        Arc::from(output),
    )?)
}

/// 将 RGBA8 帧转换为紧密 NV12/NV21；透明像素先合成到黑色，取消检查以行为粒度。
///
/// # Errors
///
/// 输入无效、4:2:0 尺寸为奇数、内存预留失败或取消时返回错误。
pub fn rgba8_to_yuv420sp_with_cancel<F>(
    frame: &Rgba8Frame,
    chroma_order: ChromaOrder,
    matrix: YuvMatrix,
    range: YuvRange,
    mut is_cancelled: F,
) -> Result<Yuv420SpFrame, ImageConversionError>
where
    F: FnMut() -> bool,
{
    let width = frame.width as usize;
    let height = frame.height as usize;
    let spec = Yuv420SpSpec {
        width: frame.width,
        height: frame.height,
        y_stride: width,
        chroma_stride: width,
        chroma_order,
        matrix,
        range,
    };
    spec.validate()?;
    let byte_len = spec.payload_len()?;
    let y_len = spec.y_len()?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(byte_len)
        .map_err(|_| ImageConversionError::AllocationFailed { bytes: byte_len })?;
    output.resize(byte_len, 0);

    for y in 0..height {
        if is_cancelled() {
            return Err(ImageConversionError::Cancelled);
        }
        for x in 0..width {
            let rgba = frame
                .pixel(x as u32, y as u32)
                .expect("validated dimensions keep pixels in bounds");
            let rgb = composite_over_black(rgba);
            output[y * width + x] = rgb_to_yuv(rgb, matrix, range)[0];
        }
    }

    for block_y in 0..height / 2 {
        if is_cancelled() {
            return Err(ImageConversionError::Cancelled);
        }
        for block_x in 0..width / 2 {
            let mut u_sum = 0_u16;
            let mut v_sum = 0_u16;
            for offset_y in 0..2 {
                for offset_x in 0..2 {
                    let x = block_x * 2 + offset_x;
                    let y = block_y * 2 + offset_y;
                    let rgba = frame
                        .pixel(x as u32, y as u32)
                        .expect("validated dimensions keep pixels in bounds");
                    let [_, u, v] = rgb_to_yuv(composite_over_black(rgba), matrix, range);
                    u_sum = u_sum.saturating_add(u16::from(u));
                    v_sum = v_sum.saturating_add(u16::from(v));
                }
            }
            let u = ((u_sum + 2) / 4) as u8;
            let v = ((v_sum + 2) / 4) as u8;
            let index = y_len + block_y * width + block_x * 2;
            match chroma_order {
                ChromaOrder::Uv => {
                    output[index] = u;
                    output[index + 1] = v;
                }
                ChromaOrder::Vu => {
                    output[index] = v;
                    output[index + 1] = u;
                }
            }
        }
    }

    Ok(Yuv420SpFrame::from_contiguous(spec, Arc::new(output))?)
}

#[must_use]
pub fn yuv_to_rgb(y: u8, u: u8, v: u8, matrix: YuvMatrix, range: YuvRange) -> [u8; 3] {
    let y = i32::from(y);
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let (c, y_scale, rv, gu, gv, bu) = match (matrix, range) {
        (YuvMatrix::Bt601, YuvRange::Limited) => ((y - 16).max(0), 298, 409, 100, 208, 516),
        (YuvMatrix::Bt709, YuvRange::Limited) => ((y - 16).max(0), 298, 459, 55, 136, 541),
        (YuvMatrix::Bt601, YuvRange::Full) => (y, 256, 359, 88, 183, 454),
        (YuvMatrix::Bt709, YuvRange::Full) => (y, 256, 403, 48, 120, 475),
    };
    [
        clamp_u8((y_scale * c + rv * e + 128) >> 8),
        clamp_u8((y_scale * c - gu * d - gv * e + 128) >> 8),
        clamp_u8((y_scale * c + bu * d + 128) >> 8),
    ]
}

#[must_use]
pub fn rgb_to_yuv(rgb: [u8; 3], matrix: YuvMatrix, range: YuvRange) -> [u8; 3] {
    let [r, g, b] = rgb.map(i32::from);
    let [y_offset, y_r, y_g, y_b, u_r, u_g, u_b, v_r, v_g, v_b] = match (matrix, range) {
        (YuvMatrix::Bt601, YuvRange::Limited) => [16, 66, 129, 25, -38, -74, 112, 112, -94, -18],
        (YuvMatrix::Bt709, YuvRange::Limited) => [16, 47, 157, 16, -26, -87, 112, 112, -102, -10],
        (YuvMatrix::Bt601, YuvRange::Full) => [0, 77, 150, 29, -43, -85, 128, 128, -107, -21],
        (YuvMatrix::Bt709, YuvRange::Full) => [0, 54, 183, 19, -29, -99, 128, 128, -116, -12],
    };
    [
        clamp_u8(y_offset + ((y_r * r + y_g * g + y_b * b + 128) >> 8)),
        clamp_u8(128 + ((u_r * r + u_g * g + u_b * b + 128) >> 8)),
        clamp_u8(128 + ((v_r * r + v_g * g + v_b * b + 128) >> 8)),
    ]
}

#[must_use]
fn composite_over_black([r, g, b, a]: [u8; 4]) -> [u8; 3] {
    let alpha = u16::from(a);
    [r, g, b].map(|channel| ((u16::from(channel) * alpha + 127) / 255) as u8)
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ImageConversionError {
    #[error(transparent)]
    InvalidFrame(#[from] ImageFrameError),
    #[error("image conversion allocation failed for {bytes} bytes")]
    AllocationFailed { bytes: usize },
    #[error("image conversion cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_2x2(pixels: [[u8; 4]; 4]) -> Rgba8Frame {
        let bytes: Vec<u8> = pixels.into_iter().flatten().collect();
        Rgba8Frame::tight(2, 2, Arc::from(bytes)).unwrap()
    }

    #[test]
    fn limited_bt601_golden_vectors_match_black_white_and_red() {
        assert_eq!(
            yuv_to_rgb(16, 128, 128, YuvMatrix::Bt601, YuvRange::Limited),
            [0, 0, 0]
        );
        assert_eq!(
            yuv_to_rgb(235, 128, 128, YuvMatrix::Bt601, YuvRange::Limited),
            [255, 255, 255]
        );
        assert_eq!(
            rgb_to_yuv([255, 0, 0], YuvMatrix::Bt601, YuvRange::Limited),
            [82, 90, 240]
        );
    }

    #[test]
    fn full_bt709_round_trip_is_bounded() {
        let source = [32, 128, 224];
        let yuv = rgb_to_yuv(source, YuvMatrix::Bt709, YuvRange::Full);
        let restored = yuv_to_rgb(yuv[0], yuv[1], yuv[2], YuvMatrix::Bt709, YuvRange::Full);
        for (actual, expected) in restored.into_iter().zip(source) {
            assert!(actual.abs_diff(expected) <= 2, "{restored:?} != {source:?}");
        }
    }

    #[test]
    fn nv12_and_nv21_only_change_chroma_byte_order() {
        let source = frame_2x2([[255, 0, 0, 255]; 4]);
        let nv12 = rgba8_to_yuv420sp_with_cancel(
            &source,
            ChromaOrder::Uv,
            YuvMatrix::Bt601,
            YuvRange::Limited,
            || false,
        )
        .unwrap();
        let nv21 = rgba8_to_yuv420sp_with_cancel(
            &source,
            ChromaOrder::Vu,
            YuvMatrix::Bt601,
            YuvRange::Limited,
            || false,
        )
        .unwrap();
        assert_eq!(nv12.y_plane().bytes(), nv21.y_plane().bytes());
        assert_eq!(nv12.chroma_plane().bytes(), &[90, 240]);
        assert_eq!(nv21.chroma_plane().bytes(), &[240, 90]);
    }

    #[test]
    fn rgba_alpha_is_composited_over_black_before_yuv() {
        let opaque = frame_2x2([[100, 200, 50, 255]; 4]);
        let transparent = frame_2x2([[100, 200, 50, 0]; 4]);
        let opaque = rgba8_to_yuv420sp_with_cancel(
            &opaque,
            ChromaOrder::Uv,
            YuvMatrix::Bt601,
            YuvRange::Full,
            || false,
        )
        .unwrap();
        let transparent = rgba8_to_yuv420sp_with_cancel(
            &transparent,
            ChromaOrder::Uv,
            YuvMatrix::Bt601,
            YuvRange::Full,
            || false,
        )
        .unwrap();
        assert_ne!(opaque.y_plane().bytes(), transparent.y_plane().bytes());
        assert_eq!(transparent.y_plane().bytes(), &[0, 0, 0, 0]);
        assert_eq!(transparent.chroma_plane().bytes(), &[128, 128]);
    }

    #[test]
    fn conversion_observes_cancellation_before_output_installation() {
        let source = frame_2x2([[0, 0, 0, 255]; 4]);
        let error = rgba8_to_yuv420sp_with_cancel(
            &source,
            ChromaOrder::Uv,
            YuvMatrix::Bt601,
            YuvRange::Limited,
            || true,
        )
        .unwrap_err();
        assert_eq!(error, ImageConversionError::Cancelled);
    }
}
