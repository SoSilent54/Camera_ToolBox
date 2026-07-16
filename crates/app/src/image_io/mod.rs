//! 传输无关的静态图像格式分流与打开流水线。

use std::sync::Arc;

use camera_toolbox_core::{
    ChromaOrder, ImageConversionError, ImageFrameError, NativeImage, RawProbeReport, Rgba8Frame,
    Yuv420SpFrame, Yuv420SpSpec, yuv420sp_to_rgba8_with_cancel,
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
            (ImageFileKind::Yuv420Sp { .. }, ImageOpenMode::Auto) => {
                Err(ImageOpenError::YuvParametersRequired)
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
        spec.validate()?;
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
        let source = self.acquire(file_system, reference, control, progress)?;
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
    #[error("headerless YUV requires confirmed width, height, strides, layout, matrix, and range")]
    YuvParametersRequired,
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
    fn bare_yuv_auto_mode_never_decodes_without_confirmation() {
        let spec = Yuv420SpSpec {
            width: 2,
            height: 2,
            y_stride: 2,
            chroma_stride: 2,
            chroma_order: ChromaOrder::Uv,
            matrix: YuvMatrix::Bt601,
            range: YuvRange::Limited,
        };
        assert!(spec.validate().is_ok());
        assert_eq!(mode_name(&ImageOpenMode::Auto), "auto");
        assert_eq!(mode_name(&ImageOpenMode::Yuv420Sp(spec)), "yuv420sp");
    }
}
