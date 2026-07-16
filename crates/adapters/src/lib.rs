//! Camera Toolbox starter adapters.

pub mod filesystem;
pub mod image_codec;
pub mod local_raw;
#[cfg(feature = "platform-cv610")]
pub mod media;
pub mod platform_registry;
pub mod platforms {
    #[cfg(feature = "platform-cv610")]
    pub mod hisilicon_cv610;
    #[cfg(feature = "platform-ssh")]
    pub mod ssh_managed;
}

pub use image_codec::ImageRasterCodec;
pub use local_raw::LocalRawLoader;
pub use platform_registry::{PlatformRegistry, PlatformRegistryError};
pub mod synthetic;

pub use synthetic::{MemoryArtifactStore, SyntheticCaptureAdapter};
