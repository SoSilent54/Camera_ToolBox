//! 文件工作区适配器。

mod local;
mod monitor;
#[cfg(feature = "platform-ssh")]
mod sftp;

pub use local::LocalFileSystem;
pub use monitor::{
    DirectoryMonitorConfig, DirectoryMonitorEvent, DirectoryMonitorHandle, DirectoryMonitorState,
    LocalDirectoryMonitor, RemoteDirectoryMonitor,
};
#[cfg(feature = "platform-ssh")]
pub use sftp::SftpFileSystem;
