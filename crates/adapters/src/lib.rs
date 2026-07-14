//! Camera Toolbox starter adapters.

pub mod local_raw;
pub mod media;
pub mod platform_registry;
pub mod platforms {
    pub mod hisilicon_cv610;
    pub mod ssh_managed;
}

pub use local_raw::LocalRawLoader;
pub use platform_registry::{PlatformRegistry, PlatformRegistryError};
pub mod synthetic;

pub use synthetic::{MemoryArtifactStore, SyntheticCaptureAdapter};
