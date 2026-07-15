//! 统一 RAW acquire → probe → rank → decode 流水线。

use camera_toolbox_core::{
    RawFrame, RawFrameError, RawProbeInput, RawProbeReport, probe_raw_candidates,
};
use thiserror::Error;

use crate::{FileRef, FileSystem, FsControl};

use super::{
    RawDecodeParams, RawDecodePreset, RawInterpretation, RawPresetError, RawSourceHandle,
    SourceCache, SourceCacheError, SourceReadProgress, explicit, select_automatic,
};

const PROBE_WINDOW_BYTES: u64 = 64 * 1024;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawOpenMode {
    Auto,
    WithOptions(RawDecodeParams),
    Reinterpret {
        generation: u64,
        params: RawDecodeParams,
    },
}

#[derive(Debug)]
pub struct RawOpenSession {
    pub source: RawSourceHandle,
    pub report: RawProbeReport,
    pub recommended: Option<RawInterpretation>,
}

#[derive(Debug)]
pub struct RawOpenResult {
    pub source: RawSourceHandle,
    pub report: RawProbeReport,
    pub interpretation: RawInterpretation,
    pub frame: RawFrame,
    pub generation: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct RawOpenPipeline {
    cache: SourceCache,
    presets: Vec<RawDecodePreset>,
    max_source_bytes: u64,
}

impl RawOpenPipeline {
    #[must_use]
    pub const fn new(
        cache: SourceCache,
        presets: Vec<RawDecodePreset>,
        max_source_bytes: u64,
    ) -> Self {
        Self {
            cache,
            presets,
            max_source_bytes,
        }
    }

    /// 获取不可变源快照并生成统一候选报告。
    ///
    /// # Errors
    ///
    /// 源获取、缓存或 probe 分段读取失败时返回错误。
    pub fn inspect(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        control: &FsControl,
    ) -> Result<RawOpenSession, RawOpenError> {
        let source = self
            .cache
            .acquire(file_system, reference, self.max_source_bytes, control)?;
        let report = inspect_cached_source(&source)?;
        let recommended = select_automatic(reference, &report, &self.presets).ok();
        Ok(RawOpenSession {
            source,
            report,
            recommended,
        })
    }
    /// 获取不可变源并向调用方报告传输进度。
    ///
    /// # Errors
    ///
    /// 源获取、缓存或 probe 分段读取失败时返回错误。
    pub fn inspect_with_progress(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        control: &FsControl,
        progress: &mut dyn FnMut(SourceReadProgress),
    ) -> Result<RawOpenSession, RawOpenError> {
        let source = self.cache.acquire_with_progress(
            file_system,
            reference,
            self.max_source_bytes,
            control,
            progress,
        )?;
        let report = inspect_cached_source(&source)?;
        let recommended = select_automatic(reference, &report, &self.presets).ok();
        Ok(RawOpenSession {
            source,
            report,
            recommended,
        })
    }

    /// 所有入口共用的同步执行入口；调用方负责在线程池中调度并携带 generation。
    ///
    /// # Errors
    ///
    /// 无自动候选、显式参数无效或完整解码失败时返回错误。
    pub fn begin(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        mode: RawOpenMode,
        control: &FsControl,
    ) -> Result<RawOpenResult, RawOpenError> {
        let session = self.inspect(file_system, reference, control)?;
        self.decode(session, mode, control)
    }
    /// 与 `begin` 相同，但在源获取阶段报告传输进度。
    ///
    /// # Errors
    ///
    /// 无自动候选、显式参数无效、源获取或完整解码失败时返回错误。
    pub fn begin_with_progress(
        &self,
        file_system: &dyn FileSystem,
        reference: &FileRef,
        mode: RawOpenMode,
        control: &FsControl,
        progress: &mut dyn FnMut(SourceReadProgress),
    ) -> Result<RawOpenResult, RawOpenError> {
        let session = self.inspect_with_progress(file_system, reference, control, progress)?;
        self.decode(session, mode, control)
    }

    /// 使用已获取会话解码，避免 Open RAW 对话框确认后重复拉取远端源。
    ///
    /// # Errors
    ///
    /// 自动模式无候选、任务取消或 RAW 字节与规格不匹配时返回错误。
    pub fn decode(
        &self,
        session: RawOpenSession,
        mode: RawOpenMode,
        control: &FsControl,
    ) -> Result<RawOpenResult, RawOpenError> {
        control.checkpoint()?;
        let (interpretation, generation) = match mode {
            RawOpenMode::Auto => (
                session
                    .recommended
                    .clone()
                    .ok_or(RawPresetError::NoSupportedCandidate)?,
                None,
            ),
            RawOpenMode::WithOptions(params) => (explicit(params), None),
            RawOpenMode::Reinterpret { generation, params } => (explicit(params), Some(generation)),
        };
        Self::decode_cached_source(
            session.source,
            session.report,
            interpretation,
            generation,
            control,
        )
    }

    /// 基于已固定的不可变源重新解码；不会重新 stat/acquire 外部文件系统。
    ///
    /// # Errors
    ///
    /// 任务取消、缓存源读取失败或 RAW 字节与规格不匹配时返回错误。
    pub fn reinterpret(
        &self,
        source: RawSourceHandle,
        params: RawDecodeParams,
        generation: u64,
        control: &FsControl,
    ) -> Result<RawOpenResult, RawOpenError> {
        let report = inspect_cached_source(&source)?;
        Self::decode_cached_source(source, report, explicit(params), Some(generation), control)
    }

    fn decode_cached_source(
        source: RawSourceHandle,
        report: RawProbeReport,
        interpretation: RawInterpretation,
        generation: Option<u64>,
        control: &FsControl,
    ) -> Result<RawOpenResult, RawOpenError> {
        control.checkpoint()?;
        let frame = RawFrame::from_bytes(
            interpretation.params.spec.clone(),
            interpretation.params.encoding,
            source.bytes(),
        )?;
        Ok(RawOpenResult {
            source,
            report,
            interpretation,
            frame,
            generation,
        })
    }

    #[must_use]
    pub fn cache(&self) -> &SourceCache {
        &self.cache
    }
}

fn inspect_cached_source(source: &RawSourceHandle) -> Result<RawProbeReport, RawOpenError> {
    let file_len = source.version().size;
    let window_len = file_len.min(PROBE_WINDOW_BYTES);
    let mut offsets = vec![0];
    if file_len > window_len {
        offsets.push(((file_len - window_len) / 2) & !1);
        offsets.push((file_len - window_len) & !1);
    }
    offsets.sort_unstable();
    offsets.dedup();

    let bytes = source.bytes();
    let mut samples = Vec::with_capacity(offsets.len());
    for offset in offsets {
        let readable = (file_len - offset).min(window_len) & !1;
        let sample_len =
            usize::try_from(readable).map_err(|_| RawOpenError::ProbeWindowTooLarge(readable))?;
        let start =
            usize::try_from(offset).map_err(|_| RawOpenError::ProbeWindowTooLarge(offset))?;
        let end = start
            .checked_add(sample_len)
            .ok_or(RawOpenError::ProbeWindowTooLarge(readable))?;
        samples.push(bytes[start..end].to_vec());
    }
    Ok(probe_raw_candidates(&RawProbeInput {
        file_name: source.reference().path.file_name().map(str::to_owned),
        file_len,
        samples,
    }))
}

#[derive(Debug, Error)]
pub enum RawOpenError {
    #[error(transparent)]
    SourceCache(#[from] SourceCacheError),
    #[error(transparent)]
    FileSystem(#[from] crate::FileSystemError),
    #[error(transparent)]
    Preset(#[from] RawPresetError),
    #[error(transparent)]
    Raw(#[from] RawFrameError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("RAW probe window does not fit this platform address space: {0} bytes")]
    ProbeWindowTooLarge(u64),
}
