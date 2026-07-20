//! Camera Toolbox 应用层：命令、事件、workflow 和外部能力端口。

pub mod asset;
pub mod calibration;
pub mod filesystem;
pub mod image_io;
pub mod platform;
pub mod raw_open;

pub mod ports;
pub mod workflow;

pub use asset::{
    AssetReservation, CaptureStore, CaptureStoreError, CaptureStoreLimits, CaptureStoreStats,
};
pub use calibration::{
    AddCalibrationItemOutcome, CalibrationDatasetItem, CalibrationEncodedPng,
    CalibrationInputError, CalibrationItemId, CalibrationItemStatus, CalibrationJobToken,
    CalibrationSession, CalibrationSessionError, CalibrationSnapshot, InstalledCalibration,
    MIN_CALIBRATION_VIEWS, read_calibration_png,
};
pub use filesystem::*;
pub use image_io::*;
pub use platform::*;
pub use raw_open::*;

pub use ports::{
    ArtifactError, ArtifactStore, CalibrationBackend, CalibrationBackendError,
    CalibrationCancellation, CaptureBackend, ExposureControl, RasterCodecError, RasterFormat,
    RasterImageCodec, RawFrameLoadError, RawFrameLoader, ReadableCaptureBackend, RegisterRead,
    RegisterWrite, SensorIdentity,
};
pub use workflow::{
    AnalysisReport, AppError, CaptureAndAnalyzeRequest, CommandEnvelope, LocalRawAnalyzeReport,
    LocalRawAnalyzeRequest, Workflow, WorkflowEvent,
};
