//! 应用层端口定义。

pub mod artifact;
pub mod raw;
pub mod sensor;

pub use artifact::{ArtifactError, ArtifactStore};
pub use raw::{RawFrameLoadError, RawFrameLoader};
pub use sensor::{
    CaptureBackend, ExposureControl, ReadableCaptureBackend, RegisterRead, RegisterWrite,
    SensorIdentity,
};
