//! 静态图像异步 Save、不可变快照与 YUV 输出参数确认。

use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use camera_toolbox_app::{
    EntryName, ExportDestination, ExportReceipt, ExportService, FileSystemError, FsControl,
    RasterImageCodec,
};
use camera_toolbox_core::{
    ChromaOrder, RawFrame, Rgba8Frame, YuvMatrix, YuvRange, rgba8_to_yuv420sp_with_cancel,
};
use eframe::egui;

use crate::workspace::DocumentId;

const RAW_WRITE_PIXELS: usize = 32 * 1024;
const SAVE_TIMEOUT: Duration = Duration::from_secs(60);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SaveKey {
    pub(crate) document_id: DocumentId,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveFormat {
    RawU16Le,
    Png,
    Yuv420Sp {
        chroma_order: ChromaOrder,
        matrix: YuvMatrix,
        range: YuvRange,
    },
}

pub(crate) enum SavePayload {
    Raw(Arc<RawFrame>),
    Display(Arc<Rgba8Frame>),
}

pub(crate) struct SaveRequest {
    pub(crate) key: SaveKey,
    pub(crate) destination: ExportDestination,
    pub(crate) target_label: String,
    pub(crate) file_name: EntryName,
    pub(crate) format: SaveFormat,
    pub(crate) payload: SavePayload,
}

pub(crate) struct SaveResult {
    pub(crate) key: SaveKey,
    pub(crate) destination: ExportDestination,
    pub(crate) target_label: String,
    pub(crate) format: SaveFormat,
    pub(crate) result: Result<u64, String>,
}

enum WorkerCommand {
    Save(SaveRequest),
    Shutdown,
}

pub(crate) struct ImageSaveWorker {
    commands: Sender<WorkerCommand>,
    results: Receiver<SaveResult>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ImageSaveWorker {
    pub(crate) fn new(
        context: &egui::Context,
        codec: Arc<dyn RasterImageCodec>,
    ) -> std::io::Result<Self> {
        let (command_sender, command_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let context = context.clone();
        let thread = thread::Builder::new()
            .name("image-save".to_owned())
            .spawn(move || {
                while let Ok(command) = command_receiver.recv() {
                    let WorkerCommand::Save(request) = command else {
                        break;
                    };
                    let is_cancelled = || worker_shutdown.load(Ordering::Acquire);
                    let key = request.key;
                    let destination = request.destination.clone();
                    let target_label = request.target_label.clone();
                    let format = request.format;
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        save_request(&*codec, request, &is_cancelled)
                    }))
                    .unwrap_or_else(|_| Err("save worker panicked".to_owned()))
                    .map(|receipt| receipt.bytes_written());
                    let _ = result_sender.send(SaveResult {
                        key,
                        destination,
                        target_label,
                        format,
                        result,
                    });
                    context.request_repaint_of(egui::ViewportId::ROOT);
                }
            })?;
        Ok(Self {
            commands: command_sender,
            results: result_receiver,
            shutdown,
            thread: Some(thread),
        })
    }

    pub(crate) fn submit(&self, request: SaveRequest) {
        let _ = self.commands.send(WorkerCommand::Save(request));
    }

    pub(crate) fn take_ready(&self) -> Option<SaveResult> {
        self.results.try_recv().ok()
    }
}

impl Drop for ImageSaveWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.commands.send(WorkerCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn save_request(
    codec: &dyn RasterImageCodec,
    request: SaveRequest,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<ExportReceipt, String> {
    if is_cancelled() {
        return Err(FileSystemError::Cancelled.to_string());
    }
    let target_label = request.target_label.clone();
    let control = FsControl::with_timeout(SAVE_TIMEOUT);
    let mut producer = |writer: &mut dyn Write| {
        if is_cancelled() {
            return Err(FileSystemError::Cancelled);
        }
        match (request.format, &request.payload) {
            (SaveFormat::RawU16Le, SavePayload::Raw(frame)) => {
                write_raw_u16le(writer, frame, is_cancelled)
            }
            (SaveFormat::Png, SavePayload::Display(frame)) => codec
                .encode_png(frame, writer)
                .map_err(|error| FileSystemError::Io(error.to_string())),
            (
                SaveFormat::Yuv420Sp {
                    chroma_order,
                    matrix,
                    range,
                },
                SavePayload::Display(frame),
            ) => write_yuv420sp(writer, frame, chroma_order, matrix, range, is_cancelled),
            (SaveFormat::RawU16Le, SavePayload::Display(_)) => Err(FileSystemError::Io(
                "RAW save requires authoritative native RAW data".to_owned(),
            )),
            (SaveFormat::Png | SaveFormat::Yuv420Sp { .. }, SavePayload::Raw(_)) => {
                Err(FileSystemError::Io(
                    "PNG/YUV save requires an immutable display revision".to_owned(),
                ))
            }
        }
    };
    ExportService
        .save_new_with(
            &request.destination,
            &request.file_name,
            &control,
            &mut producer,
        )
        .map_err(|error| save_error(&target_label, error))
}

fn write_raw_u16le(
    writer: &mut dyn Write,
    frame: &RawFrame,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<(), FileSystemError> {
    let mut bytes = Vec::with_capacity(RAW_WRITE_PIXELS * 2);
    for pixels in frame.pixels().chunks(RAW_WRITE_PIXELS) {
        if is_cancelled() {
            return Err(FileSystemError::Cancelled);
        }
        bytes.clear();
        for value in pixels {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        writer.write_all(&bytes).map_err(FileSystemError::io)?;
    }
    Ok(())
}

fn write_yuv420sp(
    writer: &mut dyn Write,
    frame: &Rgba8Frame,
    chroma_order: ChromaOrder,
    matrix: YuvMatrix,
    range: YuvRange,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<(), FileSystemError> {
    let yuv = rgba8_to_yuv420sp_with_cancel(frame, chroma_order, matrix, range, is_cancelled)
        .map_err(|error| FileSystemError::Io(error.to_string()))?;
    for row in 0..yuv.y_plane().rows() {
        if is_cancelled() {
            return Err(FileSystemError::Cancelled);
        }
        writer
            .write_all(yuv.y_plane().row(row).expect("validated Y row"))
            .map_err(FileSystemError::io)?;
    }
    for row in 0..yuv.chroma_plane().rows() {
        if is_cancelled() {
            return Err(FileSystemError::Cancelled);
        }
        writer
            .write_all(yuv.chroma_plane().row(row).expect("validated chroma row"))
            .map_err(FileSystemError::io)?;
    }
    Ok(())
}

fn save_error(target_label: &str, error: FileSystemError) -> String {
    match error {
        FileSystemError::AlreadyExists(_) => {
            format!("destination already exists and was preserved: {target_label}")
        }
        other => other.to_string(),
    }
}

pub(crate) struct YuvSaveDialogState {
    open: bool,
    target_label: String,
    dimensions: [u32; 2],
    chroma_order: ChromaOrder,
    chroma_order_hint: Option<ChromaOrder>,
    matrix: YuvMatrix,
    range: YuvRange,
}

impl Default for YuvSaveDialogState {
    fn default() -> Self {
        Self {
            open: false,
            target_label: String::new(),
            dimensions: [0, 0],
            chroma_order: ChromaOrder::Uv,
            chroma_order_hint: None,
            matrix: YuvMatrix::Bt601,
            range: YuvRange::Limited,
        }
    }
}

impl YuvSaveDialogState {
    pub(crate) fn open(
        &mut self,
        target_label: String,
        dimensions: [u32; 2],
        chroma_order_hint: Option<ChromaOrder>,
        matrix: YuvMatrix,
        range: YuvRange,
    ) {
        self.target_label = target_label;
        self.dimensions = dimensions;
        self.chroma_order_hint = chroma_order_hint;
        self.chroma_order = chroma_order_hint.unwrap_or(ChromaOrder::Uv);
        self.matrix = matrix;
        self.range = range;
        self.open = true;
    }

    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn show(
        &mut self,
        context: &egui::Context,
    ) -> Option<(ChromaOrder, YuvMatrix, YuvRange)> {
        if !self.open {
            return None;
        }
        let mut window_open = true;
        let mut confirmed = None;
        let mut cancel = false;
        egui::Window::new("Save YUV420SP")
            .collapsible(false)
            .resizable(false)
            .open(&mut window_open)
            .show(context, |ui| {
                ui.label(&self.target_label);
                ui.label(format!("{}×{}", self.dimensions[0], self.dimensions[1]));
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "Confirm matrix, range, and chroma order before conversion.",
                );
                ui.horizontal(|ui| {
                    ui.label("Chroma order");
                    ui.add_enabled_ui(self.chroma_order_hint.is_none(), |ui| {
                        ui.selectable_value(&mut self.chroma_order, ChromaOrder::Uv, "UV / NV12");
                        ui.selectable_value(&mut self.chroma_order, ChromaOrder::Vu, "VU / NV21");
                    });
                });
                ui.horizontal(|ui| {
                    ui.label("Matrix");
                    ui.selectable_value(&mut self.matrix, YuvMatrix::Bt601, "BT.601");
                    ui.selectable_value(&mut self.matrix, YuvMatrix::Bt709, "BT.709");
                });
                ui.horizontal(|ui| {
                    ui.label("Range");
                    ui.selectable_value(&mut self.range, YuvRange::Limited, "Limited");
                    ui.selectable_value(&mut self.range, YuvRange::Full, "Full");
                });
                ui.weak("BT.601 Limited is only a prefill; saving confirms the selected metadata.");
                if self.dimensions[0] % 2 != 0 || self.dimensions[1] % 2 != 0 {
                    ui.colored_label(
                        egui::Color32::LIGHT_RED,
                        "YUV420SP save requires even width and height.",
                    );
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            self.dimensions[0] > 0
                                && self.dimensions[1] > 0
                                && self.dimensions[0] % 2 == 0
                                && self.dimensions[1] % 2 == 0,
                            egui::Button::new("Save with confirmed parameters"),
                        )
                        .clicked()
                    {
                        confirmed = Some((self.chroma_order, self.matrix, self.range));
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if confirmed.is_some() || cancel || !window_open {
            self.open = false;
        }
        confirmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_adapters::{ImageRasterCodec, filesystem::LocalFileSystem};
    use camera_toolbox_app::{DirectoryRef, FileSourceId};
    use camera_toolbox_core::{BayerPattern, RawSpec};

    fn test_destination(root: &std::path::Path) -> ExportDestination {
        let source_id = FileSourceId::new("image-save-test").unwrap();
        let file_system = Arc::new(LocalFileSystem::new(source_id.clone(), root).unwrap());
        ExportDestination::new(DirectoryRef::root(source_id), file_system).unwrap()
    }

    fn save_request_for(
        key: SaveKey,
        destination: ExportDestination,
        file_name: &str,
        format: SaveFormat,
        payload: SavePayload,
    ) -> SaveRequest {
        SaveRequest {
            key,
            target_label: file_name.to_owned(),
            destination,
            file_name: EntryName::new(file_name).unwrap(),
            format,
            payload,
        }
    }

    #[test]
    fn raw_png_and_yuv_save_stream_to_create_new_destination_files() {
        let root = std::env::temp_dir().join(format!(
            "camera-toolbox-image-save-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let destination = test_destination(&root);
        let key = SaveKey {
            document_id: DocumentId::from_raw(1),
            generation: 1,
            revision: 1,
        };
        let raw = Arc::new(
            RawFrame::new(
                RawSpec {
                    width: 2,
                    height: 2,
                    bit_depth: 10,
                    bayer: BayerPattern::Rggb,
                },
                vec![1, 0x0203, 0x0405, 0x03ff],
            )
            .unwrap(),
        );
        let raw_receipt = save_request(
            &ImageRasterCodec,
            save_request_for(
                key,
                destination.clone(),
                "frame.raw",
                SaveFormat::RawU16Le,
                SavePayload::Raw(raw),
            ),
            &|| false,
        )
        .unwrap();
        assert_eq!(raw_receipt.bytes_written(), 8);
        assert_eq!(
            std::fs::read(root.join("frame.raw")).unwrap(),
            [1, 0, 3, 2, 5, 4, 255, 3]
        );

        let display = Arc::new(
            Rgba8Frame::tight(
                2,
                2,
                Arc::<[u8]>::from(vec![
                    255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128,
                ]),
            )
            .unwrap(),
        );
        save_request(
            &ImageRasterCodec,
            save_request_for(
                key,
                destination.clone(),
                "frame.png",
                SaveFormat::Png,
                SavePayload::Display(Arc::clone(&display)),
            ),
            &|| false,
        )
        .unwrap();
        assert!(std::fs::metadata(root.join("frame.png")).unwrap().len() > 0);

        let yuv_receipt = save_request(
            &ImageRasterCodec,
            save_request_for(
                key,
                destination.clone(),
                "frame.nv21",
                SaveFormat::Yuv420Sp {
                    chroma_order: ChromaOrder::Vu,
                    matrix: YuvMatrix::Bt709,
                    range: YuvRange::Full,
                },
                SavePayload::Display(display),
            ),
            &|| false,
        )
        .unwrap();
        assert_eq!(yuv_receipt.bytes_written(), 6);
        assert_eq!(std::fs::metadata(root.join("frame.nv21")).unwrap().len(), 6);

        let overwrite = save_request(
            &ImageRasterCodec,
            save_request_for(
                key,
                destination,
                "frame.raw",
                SaveFormat::RawU16Le,
                SavePayload::Raw(Arc::new(
                    RawFrame::new(
                        RawSpec {
                            width: 2,
                            height: 2,
                            bit_depth: 10,
                            bayer: BayerPattern::Rggb,
                        },
                        vec![0; 4],
                    )
                    .unwrap(),
                )),
            ),
            &|| false,
        )
        .unwrap_err();
        assert!(overwrite.contains("already exists"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_serializes_distinct_streaming_save_requests() {
        let root = std::env::temp_dir().join(format!(
            "camera-toolbox-image-save-worker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let destination = test_destination(&root);
        let context = egui::Context::default();
        let worker = ImageSaveWorker::new(&context, Arc::new(ImageRasterCodec)).unwrap();
        let key = SaveKey {
            document_id: DocumentId::from_raw(2),
            generation: 2,
            revision: 1,
        };
        let raw = Arc::new(
            RawFrame::new(
                RawSpec {
                    width: 2,
                    height: 2,
                    bit_depth: 10,
                    bayer: BayerPattern::Rggb,
                },
                vec![1, 2, 3, 4],
            )
            .unwrap(),
        );
        let display = Arc::new(Rgba8Frame::tight(2, 2, Arc::<[u8]>::from(vec![255; 16])).unwrap());
        worker.submit(save_request_for(
            key,
            destination.clone(),
            "first.raw",
            SaveFormat::RawU16Le,
            SavePayload::Raw(raw),
        ));
        worker.submit(save_request_for(
            key,
            destination,
            "second.png",
            SaveFormat::Png,
            SavePayload::Display(display),
        ));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut results = Vec::new();
        while results.len() < 2 && std::time::Instant::now() < deadline {
            if let Some(result) = worker.take_ready() {
                results.push(result);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result.result.is_ok()));
        assert_eq!(
            std::fs::read(root.join("first.raw")).unwrap(),
            [1, 0, 2, 0, 3, 0, 4, 0]
        );
        assert!(std::fs::metadata(root.join("second.png")).unwrap().len() > 0);
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
}
