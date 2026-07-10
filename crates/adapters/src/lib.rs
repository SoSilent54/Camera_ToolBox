//! Camera Toolbox starter adapters.

pub mod local_raw;

pub use local_raw::LocalRawLoader;
pub mod synthetic;

pub use synthetic::{MemoryArtifactStore, SyntheticCaptureAdapter};
