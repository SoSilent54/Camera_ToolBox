//! Artifact 访问端口。

use camera_toolbox_core::{RawFrame, sensor::CaptureArtifact};
use thiserror::Error;

pub trait ArtifactStore {
    fn load_raw(&self, artifact: &CaptureArtifact) -> Result<RawFrame, ArtifactError>;
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("artifact not found: {0}")]
    NotFound(String),
    #[error("artifact decode failed: {0}")]
    Decode(String),
    #[error("artifact store error: {0}")]
    Store(String),
}
