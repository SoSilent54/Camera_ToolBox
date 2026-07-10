//! 本地 RAW 帧加载端口。
//!
//! app 只定义端口和错误语义；具体文件系统、远程传输或缓存实现由 adapters 提供。

use std::{io, path::Path};

use camera_toolbox_core::{RawEncoding, RawFrame, RawFrameError, RawSpec};
use thiserror::Error;

pub trait RawFrameLoader {
    /// 按显式规格加载已解包 RAW 帧。
    fn load_raw_frame(
        &self,
        path: &Path,
        spec: RawSpec,
        encoding: RawEncoding,
    ) -> Result<RawFrame, RawFrameLoadError>;
}

#[derive(Debug, Error)]
pub enum RawFrameLoadError {
    #[error("failed to read raw frame: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Raw(#[from] RawFrameError),
}
