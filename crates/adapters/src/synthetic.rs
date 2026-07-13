//! 无设备副作用的合成 adapter，用于验证 workflow 和前端组装。

use std::path::PathBuf;

use camera_toolbox_app::{
    ArtifactError, ArtifactStore, CaptureBackend, RegisterRead, RegisterWrite, SensorIdentity,
};
use camera_toolbox_core::{
    BayerPattern, RawFrame, RawSpec,
    sensor::{
        CaptureArtifact, CaptureRequest, RegisterAccess, RegisterAddress, RegisterMapEntry,
        RegisterValue, RegisterWriteRequest, SensorError, SensorProfile,
    },
};

#[derive(Debug, Clone)]
pub struct SyntheticCaptureAdapter {
    profile: SensorProfile,
}

impl Default for SyntheticCaptureAdapter {
    fn default() -> Self {
        Self {
            profile: SensorProfile {
                model: "synthetic".to_owned(),
                modes: vec![camera_toolbox_core::sensor::SensorMode {
                    id: "default".to_owned(),
                    raw: synthetic_raw_spec(),
                    is_wdr: false,
                }],
                registers: vec![RegisterMapEntry {
                    address: RegisterAddress(0x0001),
                    access: RegisterAccess::ReadWrite,
                    width_bits: 8,
                    mask: Some(0x0f),
                }],
            },
        }
    }
}

impl SensorIdentity for SyntheticCaptureAdapter {
    fn profile(&self) -> &SensorProfile {
        &self.profile
    }
}

impl CaptureBackend for SyntheticCaptureAdapter {
    fn capture(&mut self, request: &CaptureRequest) -> Result<CaptureArtifact, SensorError> {
        if request.frame_count == 0 {
            return Err(SensorError::CaptureFailed(
                "frame_count must be greater than zero".to_owned(),
            ));
        }
        Ok(CaptureArtifact {
            raw_path: PathBuf::from("synthetic.raw"),
            manifest_path: None,
            frame_id: Some("synthetic-frame-0".to_owned()),
        })
    }
}

impl RegisterRead for SyntheticCaptureAdapter {
    fn read_register(&mut self, address: RegisterAddress) -> Result<RegisterValue, SensorError> {
        Ok(RegisterValue {
            value: u32::from(address.0 & 0x00ff),
            width_bits: 8,
        })
    }
}

impl RegisterWrite for SyntheticCaptureAdapter {
    fn write_register(&mut self, write: RegisterWriteRequest) -> Result<(), SensorError> {
        self.profile.validate_write(&write)
    }
}

#[derive(Debug, Clone)]
pub struct MemoryArtifactStore {
    frame: RawFrame,
}

impl Default for MemoryArtifactStore {
    fn default() -> Self {
        let spec = synthetic_raw_spec();
        let pixels = vec![0, 256, 512, 1023];
        let frame = RawFrame::new(spec, pixels).expect("synthetic frame spec must be valid");
        Self { frame }
    }
}

impl ArtifactStore for MemoryArtifactStore {
    fn load_raw(&self, artifact: &CaptureArtifact) -> Result<RawFrame, ArtifactError> {
        if artifact.raw_path.as_os_str().is_empty() {
            return Err(ArtifactError::NotFound("empty raw path".to_owned()));
        }
        Ok(self.frame.clone())
    }
}

const fn synthetic_raw_spec() -> RawSpec {
    RawSpec {
        width: 2,
        height: 2,
        bit_depth: 10,
        bayer: BayerPattern::Rggb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_toolbox_app::{CaptureAndAnalyzeRequest, Workflow};
    use camera_toolbox_core::{Roi, sensor::CaptureRequest};

    #[test]
    fn synthetic_workflow_reports_roi_stats() {
        let mut capture = SyntheticCaptureAdapter::default();
        let store = MemoryArtifactStore::default();
        let report = Workflow::run_capture_and_analyze(
            &mut capture,
            &store,
            CaptureAndAnalyzeRequest {
                capture: CaptureRequest {
                    mode_id: "default".to_owned(),
                    frame_count: 1,
                },
                roi: Roi {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
            },
        )
        .unwrap();

        assert_eq!(report.stats.max, 1023);
        assert_eq!(report.stats.saturated_pixels, 1);
    }
}
