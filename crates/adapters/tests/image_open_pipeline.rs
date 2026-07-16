use std::{
    fs,
    sync::Arc,
    time::{Duration, SystemTime},
};

use camera_toolbox_adapters::{ImageRasterCodec, filesystem::LocalFileSystem};
use camera_toolbox_app::{
    FileRef, FileSourceId, FsControl, ImageFileKind, ImageOpenError, ImageOpenMode,
    ImageOpenPipeline, RasterImageCodec, RawOpenPipeline, SourceCache, SourcePath,
};
use camera_toolbox_core::{
    ChromaOrder, NativeImage, Rgba8Frame, Yuv420SpSpec, YuvMatrix, YuvRange,
};

const LIMIT: usize = 256 * 1024 * 1024;

#[test]
fn opens_png_and_yuv_through_transport_neutral_pipeline() {
    let temp = TempDirectory::new();
    let codec = Arc::new(ImageRasterCodec);
    let rgba = Rgba8Frame::tight(
        2,
        2,
        Arc::from([
            255_u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128,
        ]),
    )
    .unwrap();
    let mut png = Vec::new();
    codec.encode_png(&rgba, &mut png).unwrap();
    fs::write(temp.path().join("sample.png"), png).unwrap();
    fs::write(
        temp.path().join("sample.nv12"),
        [16_u8, 32, 64, 128, 90, 240],
    )
    .unwrap();

    let source_id = FileSourceId::new("image-open-test").unwrap();
    let filesystem = LocalFileSystem::new(source_id.clone(), temp.path()).unwrap();
    let raw = RawOpenPipeline::new(
        SourceCache::new(LIMIT as u64, 4).unwrap(),
        Vec::new(),
        LIMIT as u64,
    );
    let pipeline = ImageOpenPipeline::new(raw, codec, LIMIT as u64, LIMIT);
    let control = FsControl::with_timeout(Duration::from_secs(5));

    let png_result = pipeline
        .open_with_progress(
            &filesystem,
            &file(&source_id, "sample.png"),
            ImageOpenMode::Auto,
            &control,
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(png_result.kind, ImageFileKind::Png);
    assert!(matches!(png_result.native, NativeImage::Rgba8(_)));
    assert_eq!(png_result.display.unwrap().pixels(), rgba.pixels());

    let spec = Yuv420SpSpec {
        width: 2,
        height: 2,
        y_stride: 2,
        chroma_stride: 2,
        chroma_order: ChromaOrder::Uv,
        matrix: YuvMatrix::Bt601,
        range: YuvRange::Limited,
    };
    let yuv_result = pipeline
        .open_with_progress(
            &filesystem,
            &file(&source_id, "sample.nv12"),
            ImageOpenMode::Yuv420Sp(spec),
            &control,
            &mut |_| {},
        )
        .unwrap();
    let NativeImage::Yuv420Sp(frame) = yuv_result.native else {
        panic!("expected native YUV frame");
    };
    assert_eq!(frame.sample(1, 1), Some((128, 90, 240)));
    assert!(yuv_result.display.is_some());
}

#[test]
fn yuv_confirmation_and_suffix_order_are_enforced_before_decode() {
    let temp = TempDirectory::new();
    fs::write(temp.path().join("sample.nv21"), [16_u8; 6]).unwrap();
    fs::write(temp.path().join("sample.yuv"), [16_u8; 6]).unwrap();
    let source_id = FileSourceId::new("image-open-yuv-test").unwrap();
    let filesystem = LocalFileSystem::new(source_id.clone(), temp.path()).unwrap();
    let codec = Arc::new(ImageRasterCodec);
    let raw = RawOpenPipeline::new(SourceCache::new(1024, 4).unwrap(), Vec::new(), 1024);
    let pipeline = ImageOpenPipeline::new(raw, codec, 1024, 1024);
    let control = FsControl::with_timeout(Duration::from_secs(5));

    let bare_error = pipeline
        .open_with_progress(
            &filesystem,
            &file(&source_id, "sample.yuv"),
            ImageOpenMode::Auto,
            &control,
            &mut |_| {},
        )
        .unwrap_err();
    assert!(matches!(bare_error, ImageOpenError::YuvParametersRequired));

    let wrong_order = Yuv420SpSpec {
        width: 2,
        height: 2,
        y_stride: 2,
        chroma_stride: 2,
        chroma_order: ChromaOrder::Uv,
        matrix: YuvMatrix::Bt601,
        range: YuvRange::Limited,
    };
    let order_error = pipeline
        .open_with_progress(
            &filesystem,
            &file(&source_id, "sample.nv21"),
            ImageOpenMode::Yuv420Sp(wrong_order),
            &control,
            &mut |_| {},
        )
        .unwrap_err();
    assert!(matches!(
        order_error,
        ImageOpenError::YuvChromaOrderMismatch {
            expected: ChromaOrder::Vu,
            actual: ChromaOrder::Uv,
        }
    ));
}

fn file(source_id: &FileSourceId, name: &str) -> FileRef {
    FileRef::new(source_id.clone(), SourcePath::new(name).unwrap())
}

struct TempDirectory(std::path::PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "camera-toolbox-image-open-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
