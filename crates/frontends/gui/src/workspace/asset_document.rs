//! JPEG/NV21 ephemeral asset 文档；source 始终保留在内存，保存只由显式动作触发。

use std::sync::Arc;

use camera_toolbox_app::TargetResolutionSnapshot;
use camera_toolbox_core::{ChromaOrder, EphemeralAsset, MediaFormat, OwnedMediaPayload};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

use super::DocumentId;

pub(crate) struct AssetDocument {
    pub(crate) id: DocumentId,
    pub(crate) title: String,
    pub(crate) asset: Arc<EphemeralAsset>,
    pub(crate) resolution: Arc<TargetResolutionSnapshot>,
    image: ColorImage,
    texture: Option<TextureHandle>,
    pub(crate) saved: bool,
}

impl AssetDocument {
    pub(crate) fn new(
        id: DocumentId,
        asset: Arc<EphemeralAsset>,
        resolution: Arc<TargetResolutionSnapshot>,
    ) -> Result<Self, String> {
        let image = decode_preview(&asset)?;
        Ok(Self {
            id,
            title: asset.metadata.source_name.clone(),
            asset,
            resolution,
            image,
            texture: None,
            saved: false,
        })
    }

    pub(crate) fn ensure_texture(&mut self, context: &egui::Context) {
        if self.texture.is_none() {
            self.texture = Some(context.load_texture(
                format!("ephemeral-asset:{}", self.asset.id),
                self.image.clone(),
                TextureOptions::LINEAR,
            ));
        }
    }

    pub(crate) fn texture(&self) -> Option<&TextureHandle> {
        self.texture.as_ref()
    }
}

fn decode_preview(asset: &EphemeralAsset) -> Result<ColorImage, String> {
    match &asset.metadata.format {
        MediaFormat::Jpeg => decode_jpeg(payload_bytes(&asset.source)?),
        MediaFormat::Yuv420Sp { chroma_order } => decode_yuv420sp(asset, *chroma_order),
        format => Err(format!(
            "media format {format:?} has no raster asset preview"
        )),
    }
}

fn decode_jpeg(bytes: &[u8]) -> Result<ColorImage, String> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
        .map_err(|error| format!("JPEG decode failed: {error}"))?
        .to_rgba8();
    let size = [
        usize::try_from(image.width()).map_err(|_| "JPEG width does not fit host".to_owned())?,
        usize::try_from(image.height()).map_err(|_| "JPEG height does not fit host".to_owned())?,
    ];
    Ok(ColorImage::from_rgba_unmultiplied(size, image.as_raw()))
}

fn decode_yuv420sp(asset: &EphemeralAsset, order: ChromaOrder) -> Result<ColorImage, String> {
    let width = attribute_usize(asset, "width")?;
    let height = attribute_usize(asset, "height")?;
    let y_stride = attribute_usize(asset, "y_stride")?;
    let chroma_stride = attribute_usize(asset, "chroma_stride")?;
    if width == 0 || height < 2 || y_stride < width || chroma_stride < width {
        return Err("NV21 dimensions/strides are invalid".to_owned());
    }
    let bytes = payload_bytes(&asset.source)?;
    let y_len = y_stride
        .checked_mul(height)
        .ok_or_else(|| "NV21 Y plane size overflow".to_owned())?;
    let chroma_rows = height / 2;
    let chroma_len = chroma_stride
        .checked_mul(chroma_rows)
        .ok_or_else(|| "NV21 chroma plane size overflow".to_owned())?;
    if bytes.len() != y_len.saturating_add(chroma_len) {
        return Err(format!(
            "NV21 payload length mismatch: expected {}, got {}",
            y_len.saturating_add(chroma_len),
            bytes.len()
        ));
    }
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(
        width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| "NV21 preview size overflow".to_owned())?,
    )
    .map_err(|_| "NV21 preview allocation failed".to_owned())?;
    for y in 0..height {
        let chroma_row = (y / 2).min(chroma_rows - 1);
        for x in 0..width {
            let luma = bytes[y * y_stride + x];
            let pair = (x / 2) * 2;
            let first = bytes[y_len + chroma_row * chroma_stride + pair];
            let second = bytes[y_len + chroma_row * chroma_stride + pair + 1];
            let (u, v) = match order {
                ChromaOrder::Uv => (first, second),
                ChromaOrder::Vu => (second, first),
            };
            let [red, green, blue] = yuv_to_rgb(luma, u, v);
            rgba.extend_from_slice(&[red, green, blue, 255]);
        }
    }
    Ok(ColorImage::from_rgba_unmultiplied([width, height], &rgba))
}

fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = i32::from(y).saturating_sub(16).max(0);
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let clamp = |value: i32| u8::try_from(value.clamp(0, 255)).unwrap_or(0);
    [
        clamp((298 * c + 409 * e + 128) >> 8),
        clamp((298 * c - 100 * d - 208 * e + 128) >> 8),
        clamp((298 * c + 516 * d + 128) >> 8),
    ]
}

fn attribute_usize(asset: &EphemeralAsset, name: &str) -> Result<usize, String> {
    asset
        .metadata
        .attributes
        .get(name)
        .ok_or_else(|| format!("asset metadata is missing {name}"))?
        .parse::<usize>()
        .map_err(|error| format!("asset metadata {name} is invalid: {error}"))
}

pub(crate) fn payload_bytes(payload: &OwnedMediaPayload) -> Result<&[u8], String> {
    match payload {
        OwnedMediaPayload::Bytes(bytes) => Ok(bytes),
        OwnedMediaPayload::Planes(_) => {
            Err("multi-plane export requires a container format".to_owned())
        }
    }
}
