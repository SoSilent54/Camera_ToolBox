//! Camera Toolbox 应用层：命令、事件、workflow 和外部能力端口。

pub mod ports;
pub mod workflow;

pub use ports::{
    ArtifactError, ArtifactStore, CaptureBackend, ExposureControl, ReadableCaptureBackend,
    RegisterRead, RegisterWrite, SensorIdentity,
};
pub use workflow::{
    AnalysisReport, AppError, CaptureAndAnalyzeRequest, CommandEnvelope, Workflow, WorkflowEvent,
};
