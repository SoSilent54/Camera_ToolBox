//! 传输无关的 RAW 源缓存、Preset 排序与统一打开流水线。

mod pipeline;
mod preset;
mod source_cache;

pub use pipeline::{RawOpenError, RawOpenMode, RawOpenPipeline, RawOpenResult, RawOpenSession};
pub use preset::{
    RawDecodeParams, RawDecodePreset, RawInterpretation, RawPresetError, RawPresetEvidence,
};
pub use source_cache::{RawSourceHandle, SourceCache, SourceCacheError, SourceReadProgress};

pub(crate) use preset::{explicit, select_automatic};
