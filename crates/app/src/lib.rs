//! Camera Toolbox 应用层：命令、事件、workflow 和外部能力端口。

pub mod asset;
pub mod calibration;
pub mod export;
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
    AUTO_CAPTURE_DETECTOR_FINGERPRINT, AUTO_CAPTURE_FEATURE_SCHEMA_VERSION,
    AddCalibrationItemOutcome, AutoAdmissionAssessment, AutoAdmissionDepthRange,
    AutoAdmissionItemContribution, AutoAdmissionItemVisualization, AutoAdmissionPnpState,
    AutoCandidateAdmission, AutoCandidateCommit, AutoCandidateId, AutoCandidateToken,
    AutoCaptureAcceptanceCriteria, AutoCaptureAcquisitionKey, AutoCaptureBaseline,
    CalibrationDatasetItem, CalibrationEncodedPng, CalibrationImageCrop,
    CalibrationImageOrientation, CalibrationInputError, CalibrationInputKey,
    CalibrationInputRevision, CalibrationItemId, CalibrationItemStatus, CalibrationJobToken,
    CalibrationSession, CalibrationSessionError, CalibrationSnapshot, InitialIntrinsicsBinding,
    InstalledCalibration, MIN_CALIBRATION_VIEWS, PixelCoordinateConvention, PnPObservation,
    StreamCaptureId, read_calibration_png,
};
pub use export::{ExportArtifact, ExportDestination, ExportReceipt, ExportService};
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
