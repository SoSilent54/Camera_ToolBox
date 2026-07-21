//! 平台 UI 模块。

mod device_manager;
#[cfg(feature = "platform-ssh")]
mod host_key_worker;
mod live_runtime;
mod profile_commit;
#[cfg(feature = "platform-ssh")]
mod ssh_profile;
#[cfg(feature = "platform-ssh")]
mod ssh_runtime;
mod stream_panel;

pub(crate) use device_manager::{DeviceManagerAction, DeviceManagerState};
#[cfg(feature = "platform-ssh")]
pub(crate) use live_runtime::EepromProvisioningTarget;
pub(crate) use live_runtime::{LiveRuntime, PlatformEffect, PlatformUiAction};
#[cfg(feature = "platform-cv610")]
pub(crate) use stream_panel::render_stream_panel;
pub(crate) use stream_panel::{StreamPanelAction, StreamPanelState};
