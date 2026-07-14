//! Camera Toolbox 应用层：命令、事件、workflow 和外部能力端口。

pub mod asset;
pub mod platform;

pub mod ports;
pub mod workflow;

pub use asset::{
    AssetReservation, CaptureStore, CaptureStoreError, CaptureStoreLimits, CaptureStoreStats,
};
pub use platform::*;

pub use ports::{
    ArtifactError, ArtifactStore, CaptureBackend, ExposureControl, RawFrameLoadError,
    RawFrameLoader, ReadableCaptureBackend, RegisterRead, RegisterWrite, SensorIdentity,
};
pub use workflow::{
    AnalysisReport, AppError, CaptureAndAnalyzeRequest, CommandEnvelope, LocalRawAnalyzeReport,
    LocalRawAnalyzeRequest, Workflow, WorkflowEvent,
};
