//! 本地 RAW 帧加载端口。
//!
//! app 只定义端口和错误语义；具体文件系统、远程传输或缓存实现由 adapters 提供。

use std::{io, path::Path};

use camera_toolbox_core::{RawEncoding, RawFrame, RawFrameError, RawProbeInput, RawSpec};
use thiserror::Error;

pub trait RawFrameLoader {
    /// 有界读取文件信息和分段样本，用于生成不具唯一性的参数候选。
    ///
    /// # Errors
    ///
    /// 文件不存在、不可读或读取失败时返回错误。
    fn inspect_raw(&self, path: &Path) -> Result<RawProbeInput, RawFrameLoadError>;

    /// 按显式规格加载已解包 RAW 帧。
    ///
    /// # Errors
    ///
    /// 文件读取失败或 RAW 规格与文件内容不匹配时返回错误。
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
