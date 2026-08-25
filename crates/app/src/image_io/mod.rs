//! 传输无关的静态图像格式分流与打开流水线。

use std::sync::Arc;

use camera_toolbox_core::{
    ChromaOrder, ImageConversionError, ImageFrameError, NativeImage, RawProbeReport, Rgba8Frame,
    Yuv420SpFrame, Yuv420SpSpec, YuvMatrix, YuvRange, yuv420sp_to_rgba8_with_cancel,
};
use thiserror::Error;

use crate::{
    FileRef, FileSystem, FsControl, ImageSourceHandle, RasterCodecError, RasterFormat,
    RasterImageCodec, RawInterpretation, RawOpenError, RawOpenMode, RawOpenPipeline, SourceCache,
    SourceCacheError, SourceReadProgress,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFileKind {
    Raw,
    Png,
    Jpeg,
    Yuv420Sp {
        chroma_order_hint: Option<ChromaOrder>,
    },
}

#[derive(Debug, Clone)]
pub enum ImageOpenMode {
    Auto,
    Raw(RawOpenMode),
    Yuv420Sp(Yuv420SpSpec),
}

#[derive(Debug)]
pub struct RawImageOpenMetadata {
    pub report: RawProbeReport,
    pub interpretation: RawInterpretation,
    pub generation: Option<u64>,
}

#[derive(Debug)]
pub struct ImageOpenResult {
    pub source: ImageSourceHandle,
    pub native: NativeImage,
    /// RAW 的显示面由 GUI 现有 RAW/Color pipeline 生成；其余格式在打开阶段生成。
    pub display: Option<Arc<Rgba8Frame>>,
    pub kind: ImageFileKind,
    pub raw: Option<RawImageOpenMetadata>,
}

#[derive(Clone)]
pub struct ImageOpenPipeline {
    raw: RawOpenPipeline,
    cache: SourceCache,
    codec: Arc<dyn RasterImageCodec>,
    max_source_bytes: u64,
    max_decoded_bytes: usize,
}

impl std::fmt::Debug for ImageOpenPipeline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageOpenPipeline")
            .field("raw", &self.raw)
            .field("cache", &self.cache)
            .field("max_source_bytes", &self.max_source_bytes)
            .field("max_decoded_bytes", &self.max_decoded_bytes)
            .finish_non_exhaustive()
    }
}

impl ImageOpenPipeline {
    #[must_use]
    pub fn new(
        raw: RawOpenPipeline,
        codec: Arc<dyn RasterImageCodec>,
        max_source_bytes: u64,
        max_decoded_bytes: usize,
    ) -> Self {
        Self {
            cache: raw.cache().clone(),
            raw,
            codec,
            max_source_bytes,
            max_decoded_bytes,
        }
    }

    /// 根据 suffix 选择 RAW、PNG、JPEG 或 YUV 参数工作流。
    ///
    /// # Errors
    ///
    /// 文件没有受支持 suffix 时返回错误。
    pub fn classify(reference: &FileRef) -> Result<ImageFileKind, ImageOpenError> {
        classify_image_file(reference)
    }

    /// 同步打开静态图像；调用方负责后台调度与 generation 校验。
    ///
    /// # Errors
    ///
    /// 模式与 suffix 不匹配、YUV 参数缺失、获取/解码/转换失败或取消时返回错误。
    pub fn open_with_progress(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        mode: ImageOpenMode,
        control: &FsControl,
        progress: &mut dyn FnMut(SourceReadProgress),
    ) -> Result<ImageOpenResult, ImageOpenError> {
        let kind = classify_image_file(reference)?;
        match (kind, mode) {
            (ImageFileKind::Raw, ImageOpenMode::Auto) => {
                self.open_raw(file_system, reference, RawOpenMode::Auto, control, progress)
            }
            (ImageFileKind::Raw, ImageOpenMode::Raw(mode)) => {
                self.open_raw(file_system, reference, mode, control, progress)
            }
            (ImageFileKind::Png, ImageOpenMode::Auto) => self.open_raster(
                file_system,
                reference,
                ImageFileKind::Png,
                RasterFormat::Png,
                control,
                progress,
            ),
            (ImageFileKind::Jpeg, ImageOpenMode::Auto) => self.open_raster(
                file_system,
                reference,
                ImageFileKind::Jpeg,
                RasterFormat::Jpeg,
                control,
                progress,
            ),
            (kind @ ImageFileKind::Yuv420Sp { .. }, ImageOpenMode::Auto) => {
                self.open_auto_yuv(file_system, reference, kind, control, progress)
            }
            (
                kind @ ImageFileKind::Yuv420Sp { chroma_order_hint },
                ImageOpenMode::Yuv420Sp(spec),
            ) => {
                if chroma_order_hint.is_some_and(|expected| expected != spec.chroma_order) {
                    return Err(ImageOpenError::YuvChromaOrderMismatch {
                        expected: chroma_order_hint.expect("checked as Some"),
                        actual: spec.chroma_order,
                    });
                }
                self.open_yuv(file_system, reference, kind, spec, control, progress)
            }
            (kind, mode) => Err(ImageOpenError::ModeMismatch {
                kind,
                mode: mode_name(&mode),
            }),
        }
    }

    fn open_raw(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        mode: RawOpenMode,
        control: &FsControl,
        progress: &mut dyn FnMut(SourceReadProgress),
    ) -> Result<ImageOpenResult, ImageOpenError> {
        let opened =
            self.raw
                .begin_with_progress(file_system, reference, mode, control, progress)?;
        Ok(ImageOpenResult {
            source: opened.source,
            native: NativeImage::Raw(Arc::new(opened.frame)),
            display: None,
            kind: ImageFileKind::Raw,
            raw: Some(RawImageOpenMetadata {
                report: opened.report,
                interpretation: opened.interpretation,
                generation: opened.generation,
            }),
        })
    }

    fn open_raster(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        kind: ImageFileKind,
        format: RasterFormat,
        control: &FsControl,
        progress: &mut dyn FnMut(SourceReadProgress),
    ) -> Result<ImageOpenResult, ImageOpenError> {
        let source = self.acquire(file_system, reference, control, progress)?;
        control.checkpoint()?;
        let frame = Arc::new(self.codec.decode_rgba8(
            format,
            source.bytes(),
            self.max_decoded_bytes,
        )?);
        control.checkpoint()?;
        Ok(ImageOpenResult {
            source,
            native: NativeImage::Rgba8(Arc::clone(&frame)),
            display: Some(frame),
            kind,
            raw: None,
        })
    }

    fn open_yuv(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        kind: ImageFileKind,
        spec: Yuv420SpSpec,
        control: &FsControl,
        progress: &mut dyn FnMut(SourceReadProgress),
    ) -> Result<ImageOpenResult, ImageOpenError> {
        Self::validate_yuv_kind(kind, spec)?;
        let source = self.acquire(file_system, reference, control, progress)?;
        self.decode_yuv_source(source, kind, spec, control)
    }

    fn open_auto_yuv(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        kind: ImageFileKind,
        control: &FsControl,
        progress: &mut dyn FnMut(SourceReadProgress),
    ) -> Result<ImageOpenResult, ImageOpenError> {
        let source = self.acquire(file_system, reference, control, progress)?;
        let spec = infer_yuv420sp_spec(reference, kind, source.version().size)?;
        self.decode_yuv_source(source, kind, spec, control)
    }

    /// 使用已缓存的不可变 headerless YUV payload 重新解释；不会访问文件系统。
    ///
    /// # Errors
    ///
    /// 参数、suffix hint、缓存长度或颜色转换无效时返回错误。
    pub fn reinterpret_yuv(
        &self,
        source: ImageSourceHandle,
        kind: ImageFileKind,
        spec: Yuv420SpSpec,
        control: &FsControl,
    ) -> Result<ImageOpenResult, ImageOpenError> {
        Self::validate_yuv_kind(kind, spec)?;
        self.decode_yuv_source(source, kind, spec, control)
    }

    fn validate_yuv_kind(kind: ImageFileKind, spec: Yuv420SpSpec) -> Result<(), ImageOpenError> {
        let ImageFileKind::Yuv420Sp { chroma_order_hint } = kind else {
            return Err(ImageOpenError::ModeMismatch {
                kind,
                mode: "yuv420sp",
            });
        };
        if chroma_order_hint.is_some_and(|expected| expected != spec.chroma_order) {
            return Err(ImageOpenError::YuvChromaOrderMismatch {
                expected: chroma_order_hint.expect("checked as Some"),
                actual: spec.chroma_order,
            });
        }
        spec.validate()?;
        Ok(())
    }

    fn decode_yuv_source(
        &self,
        source: ImageSourceHandle,
        kind: ImageFileKind,
        spec: Yuv420SpSpec,
        control: &FsControl,
    ) -> Result<ImageOpenResult, ImageOpenError> {
        let decoded_bytes = (spec.width as usize)
            .checked_mul(spec.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(ImageFrameError::SizeOverflow)?;
        if decoded_bytes > self.max_decoded_bytes {
            return Err(ImageOpenError::DecodedImageTooLarge {
                required: decoded_bytes,
                limit: self.max_decoded_bytes,
            });
        }
        control.checkpoint()?;
        let native = Arc::new(Yuv420SpFrame::from_contiguous(spec, source.shared_bytes())?);
        let display = Arc::new(yuv420sp_to_rgba8_with_cancel(&native, || {
            control.cancellation.is_cancelled()
        })?);
        control.checkpoint()?;
        Ok(ImageOpenResult {
            source,
            native: NativeImage::Yuv420Sp(native),
            display: Some(display),
            kind,
            raw: None,
        })
    }

    fn acquire(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        control: &FsControl,
        progress: &mut dyn FnMut(SourceReadProgress),
    ) -> Result<ImageSourceHandle, ImageOpenError> {
        Ok(self.cache.acquire_with_progress(
            file_system,
            reference,
            self.max_source_bytes,
            control,
            progress,
        )?)
    }

    #[must_use]
    pub fn codec(&self) -> &Arc<dyn RasterImageCodec> {
        &self.codec
    }
}

fn infer_yuv420sp_spec(
    reference: &FileRef,
    kind: ImageFileKind,
    source_len: u64,
) -> Result<Yuv420SpSpec, ImageOpenError> {
    let ImageFileKind::Yuv420Sp { chroma_order_hint } = kind else {
        return Err(ImageOpenError::ModeMismatch {
            kind,
            mode: "yuv420sp",
        });
    };
    let file_name = reference
        .path
        .file_name()
        .ok_or(ImageOpenError::MissingFileName)?;
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem);
    let (width, height) = parse_yuv_geometry(stem)
        .ok_or_else(|| ImageOpenError::YuvGeometryMissing(file_name.to_owned()))??;
    let spec = Yuv420SpSpec {
        width,
        height,
        y_stride: width as usize,
        chroma_stride: width as usize,
        chroma_order: chroma_order_hint.unwrap_or(ChromaOrder::Uv),
        matrix: YuvMatrix::Bt601,
        range: YuvRange::Limited,
    };
    spec.validate()?;
    let expected = spec.payload_len()?;
    let actual =
        usize::try_from(source_len).map_err(|_| ImageOpenError::YuvSourceTooLarge(source_len))?;
    if expected != actual {
        return Err(ImageOpenError::YuvPayloadLengthMismatch { expected, actual });
    }
    Ok(spec)
}

/// 从 basename 内唯一的 `WIDTHxHEIGHT` token 取得 headerless YUV 的初始几何。
fn parse_yuv_geometry(stem: &str) -> Option<Result<(u32, u32), ImageOpenError>> {
    let bytes = stem.as_bytes();
    let mut candidate = None;
    let mut start = 0;
    while start < bytes.len() {
        if !bytes[start].is_ascii_digit() || (start > 0 && bytes[start - 1].is_ascii_alphanumeric())
        {
            start += 1;
            continue;
        }
        let mut separator = start;
        while separator < bytes.len() && bytes[separator].is_ascii_digit() {
            separator += 1;
        }
        if separator == bytes.len() || !matches!(bytes[separator], b'x' | b'X') {
            start = separator.saturating_add(1);
            continue;
        }
        let height_start = separator + 1;
        let mut end = height_start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == height_start || (end < bytes.len() && bytes[end].is_ascii_alphanumeric()) {
            start = end.saturating_add(1);
            continue;
        }
        let width = parse_ascii_u32(&bytes[start..separator])?;
        let height = parse_ascii_u32(&bytes[height_start..end])?;
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Some(Err(ImageOpenError::YuvInvalidGeometry { width, height }));
        }
        if candidate.replace((width, height)).is_some() {
            return Some(Err(ImageOpenError::YuvGeometryAmbiguous(stem.to_owned())));
        }
        start = end;
    }
    Some(Ok(candidate?))
}

fn parse_ascii_u32(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        value
            .checked_mul(10)?
            .checked_add(u32::from(byte.checked_sub(b'0')?))
    })
}
/// suffix 只负责选择 workflow；headerless RAW/YUV 的解释参数仍由后续步骤确认。
///
/// # Errors
///
/// 文件没有名称、suffix 或 suffix 不受支持时返回错误。
pub fn classify_image_file(reference: &FileRef) -> Result<ImageFileKind, ImageOpenError> {
    let file_name = reference
        .path
        .file_name()
        .ok_or(ImageOpenError::MissingFileName)?;
    let extension = file_name
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| !extension.is_empty())
        .ok_or(ImageOpenError::MissingExtension)?
        .to_ascii_lowercase();
    match extension.as_str() {
        "raw" | "bin" => Ok(ImageFileKind::Raw),
        "png" => Ok(ImageFileKind::Png),
        "jpg" | "jpeg" => Ok(ImageFileKind::Jpeg),
        "nv12" => Ok(ImageFileKind::Yuv420Sp {
            chroma_order_hint: Some(ChromaOrder::Uv),
        }),
        "nv21" => Ok(ImageFileKind::Yuv420Sp {
            chroma_order_hint: Some(ChromaOrder::Vu),
        }),
        "yuv" => Ok(ImageFileKind::Yuv420Sp {
            chroma_order_hint: None,
        }),
        _ => Err(ImageOpenError::UnsupportedExtension(extension)),
    }
}

const fn mode_name(mode: &ImageOpenMode) -> &'static str {
    match mode {
        ImageOpenMode::Auto => "auto",
        ImageOpenMode::Raw(_) => "raw",
        ImageOpenMode::Yuv420Sp(_) => "yuv420sp",
    }
}

#[derive(Debug, Error)]
pub enum ImageOpenError {
    #[error("image source has no file name")]
    MissingFileName,
    #[error("image source has no file extension")]
    MissingExtension,
    #[error("unsupported image extension .{0}")]
    UnsupportedExtension(String),
    #[error(
        "headerless YUV file name {0:?} needs one even WIDTHxHEIGHT token, for example frame_1920x1080.nv12"
    )]
    YuvGeometryMissing(String),
    #[error("headerless YUV file name {0:?} contains multiple geometry tokens")]
    YuvGeometryAmbiguous(String),
    #[error("headerless YUV geometry {width}x{height} must use non-zero even dimensions")]
    YuvInvalidGeometry { width: u32, height: u32 },
    #[error(
        "headerless YUV source length {actual} does not match tight-layout geometry ({expected} bytes expected)"
    )]
    YuvPayloadLengthMismatch { expected: usize, actual: usize },
    #[error("headerless YUV source length {0} cannot be represented on this host")]
    YuvSourceTooLarge(u64),
    #[error("image open mode {mode} does not match {kind:?}")]
    ModeMismatch {
        kind: ImageFileKind,
        mode: &'static str,
    },
    #[error("YUV suffix requires {expected:?} chroma order, got {actual:?}")]
    YuvChromaOrderMismatch {
        expected: ChromaOrder,
        actual: ChromaOrder,
    },
    #[error("decoded image requires {required} bytes, limit is {limit} bytes")]
    DecodedImageTooLarge { required: usize, limit: usize },
    #[error(transparent)]
    Raw(#[from] RawOpenError),
    #[error(transparent)]
    Source(#[from] SourceCacheError),
    #[error(transparent)]
    FileSystem(#[from] crate::FileSystemError),
    #[error(transparent)]
    Codec(#[from] RasterCodecError),
    #[error(transparent)]
    Frame(#[from] ImageFrameError),
    #[error(transparent)]
    Conversion(#[from] ImageConversionError),
}

#[cfg(test)]
mod tests {
    use camera_toolbox_core::{YuvMatrix, YuvRange};

    use super::*;
    use crate::{FileSourceId, SourcePath};

    fn reference(name: &str) -> FileRef {
        FileRef {
            source_id: FileSourceId::new("test").unwrap(),
            path: SourcePath::new(name).unwrap(),
        }
    }

    #[test]
    fn suffix_dispatch_is_case_insensitive_and_preserves_yuv_ambiguity() {
        assert_eq!(
            classify_image_file(&reference("frame.PNG")).unwrap(),
            ImageFileKind::Png
        );
        assert_eq!(
            classify_image_file(&reference("frame.nv21")).unwrap(),
            ImageFileKind::Yuv420Sp {
                chroma_order_hint: Some(ChromaOrder::Vu),
            }
        );
        assert_eq!(
            classify_image_file(&reference("frame.yuv")).unwrap(),
            ImageFileKind::Yuv420Sp {
                chroma_order_hint: None,
            }
        );
    }

    #[test]
    fn auto_yuv_infers_tight_geometry_and_suffix_order() {
        let spec = infer_yuv420sp_spec(
            &reference("capture_640x480.nv21"),
            ImageFileKind::Yuv420Sp {
                chroma_order_hint: Some(ChromaOrder::Vu),
            },
            640 * 480 * 3 / 2,
        )
        .unwrap();
        assert_eq!(spec.width, 640);
        assert_eq!(spec.height, 480);
        assert_eq!(spec.y_stride, 640);
        assert_eq!(spec.chroma_stride, 640);
        assert_eq!(spec.chroma_order, ChromaOrder::Vu);
        assert_eq!(spec.matrix, YuvMatrix::Bt601);
        assert_eq!(spec.range, YuvRange::Limited);
    }

    #[test]
    fn auto_bare_yuv_defaults_to_uv() {
        let spec = infer_yuv420sp_spec(
            &reference("capture_2x2.yuv"),
            ImageFileKind::Yuv420Sp {
                chroma_order_hint: None,
            },
            6,
        )
        .unwrap();
        assert_eq!(spec.chroma_order, ChromaOrder::Uv);
    }

    #[test]
    fn auto_yuv_rejects_missing_ambiguous_or_mismatched_geometry() {
        let kind = ImageFileKind::Yuv420Sp {
            chroma_order_hint: Some(ChromaOrder::Uv),
        };
        assert!(matches!(
            infer_yuv420sp_spec(&reference("capture.nv12"), kind, 6),
            Err(ImageOpenError::YuvGeometryMissing(_))
        ));
        assert!(matches!(
            infer_yuv420sp_spec(&reference("capture_2x2_4x4.nv12"), kind, 6),
            Err(ImageOpenError::YuvGeometryAmbiguous(_))
        ));
        assert!(matches!(
            infer_yuv420sp_spec(&reference("capture_2x2.nv12"), kind, 7),
            Err(ImageOpenError::YuvPayloadLengthMismatch {
                expected: 6,
                actual: 7,
            })
        ));
    }
}
