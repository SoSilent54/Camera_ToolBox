//! Sensor 领域数据。

pub mod exposure;
pub mod profile;
pub mod types;

pub use exposure::{ExposurePlan, ExposureSetpoint};
pub use profile::{RegisterAccess, RegisterMapEntry, SensorMode, SensorModeId, SensorProfile};
pub use types::{
    CaptureArtifact, CaptureRequest, RegisterAddress, RegisterValue, RegisterWriteRequest,
    SensorError,
};
