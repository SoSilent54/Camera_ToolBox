//! 平台 UI 模块。

mod device_manager;
mod host_key_worker;
mod live_runtime;
mod profile_commit;
mod ssh_profile;
mod ssh_runtime;
mod stream_panel;

pub(crate) use device_manager::{DeviceManagerAction, DeviceManagerState};
pub(crate) use live_runtime::{LiveRuntime, PlatformEffect, PlatformUiAction};
pub(crate) use stream_panel::{StreamPanelAction, StreamPanelState, render_stream_panel};
