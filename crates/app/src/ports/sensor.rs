//! Sensor 小能力端口。

use camera_toolbox_core::sensor::{
    CaptureArtifact, CaptureRequest, ExposureSetpoint, RegisterAddress, RegisterValue,
    RegisterWriteRequest, SensorError, SensorProfile,
};

pub trait SensorIdentity {
    fn profile(&self) -> &SensorProfile;

    fn model(&self) -> &str {
        self.profile().model.as_str()
    }
}

pub trait CaptureBackend: SensorIdentity {
    fn capture(&mut self, request: &CaptureRequest) -> Result<CaptureArtifact, SensorError>;
}

pub trait RegisterRead: SensorIdentity {
    fn read_register(&mut self, address: RegisterAddress) -> Result<RegisterValue, SensorError>;
}

pub trait RegisterWrite: RegisterRead {
    fn write_register(&mut self, write: RegisterWriteRequest) -> Result<(), SensorError>;
}

pub trait ExposureControl: RegisterWrite {
    fn apply_exposure(&mut self, setpoint: ExposureSetpoint) -> Result<(), SensorError>;
}

pub trait ReadableCaptureBackend: CaptureBackend + RegisterRead {}

impl<T> ReadableCaptureBackend for T where T: CaptureBackend + RegisterRead {}
