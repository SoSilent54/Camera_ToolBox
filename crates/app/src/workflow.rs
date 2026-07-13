//! P0 workflow 编排。

use std::path::{Path, PathBuf};

use camera_toolbox_core::{
    AnalysisError, RawEncoding, RawFrame, RawProbeReport, RawSpec, Roi, RoiStats, analyze_roi,
    probe_raw_candidates,
    sensor::{CaptureArtifact, CaptureRequest, SensorError},
};
use thiserror::Error;

use crate::ports::{
    ArtifactError, ArtifactStore, CaptureBackend, RawFrameLoadError, RawFrameLoader,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureAndAnalyzeRequest {
    pub capture: CaptureRequest,
    pub roi: Roi,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRawAnalyzeRequest {
    pub path: PathBuf,
    pub spec: RawSpec,
    pub encoding: RawEncoding,
    pub roi: Roi,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandEnvelope {
    CaptureAndAnalyze(CaptureAndAnalyzeRequest),
    LocalRawAnalyze(LocalRawAnalyzeRequest),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisReport {
    pub artifact: CaptureArtifact,
    pub roi: Roi,
    pub stats: RoiStats,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalRawAnalyzeReport {
    pub path: PathBuf,
    pub frame: RawFrame,
    pub roi: Roi,
    pub stats: RoiStats,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowEvent {
    CaptureStarted,
    CaptureCompleted(CaptureArtifact),
    AnalysisCompleted(AnalysisReport),
}

#[derive(Debug, Default)]
pub struct Workflow;

impl Workflow {
    pub fn run_capture_and_analyze<C, A>(
        capture_backend: &mut C,
        artifact_store: &A,
        request: CaptureAndAnalyzeRequest,
    ) -> Result<AnalysisReport, AppError>
    where
        C: CaptureBackend + ?Sized,
        A: ArtifactStore + ?Sized,
    {
        let artifact = capture_backend.capture(&request.capture)?;
        let frame = artifact_store.load_raw(&artifact)?;
        let stats = analyze_roi(&frame, request.roi)?;
        Ok(AnalysisReport {
            artifact,
            roi: request.roi,
            stats,
        })
    }

    /// 检查无头 RAW 并返回必须由用户确认的参数候选。
    ///
    /// # Errors
    ///
    /// 文件检查端口读取失败时返回错误。
    pub fn inspect_raw_parameters<L>(loader: &L, path: &Path) -> Result<RawProbeReport, AppError>
    where
        L: RawFrameLoader + ?Sized,
    {
        let input = loader.inspect_raw(path)?;
        Ok(probe_raw_candidates(&input))
    }

    /// 按用户确认的 RAW 参数加载完整帧并分析指定 ROI。
    ///
    /// # Errors
    ///
    /// 文件加载失败、RAW 参数无效或 ROI 超出图像范围时返回错误。
    pub fn load_raw_and_analyze<L>(
        loader: &L,
        request: LocalRawAnalyzeRequest,
    ) -> Result<LocalRawAnalyzeReport, AppError>
    where
        L: RawFrameLoader + ?Sized,
    {
        let frame = loader.load_raw_frame(&request.path, request.spec, request.encoding)?;
        let stats = analyze_roi(&frame, request.roi)?;
        Ok(LocalRawAnalyzeReport {
            path: request.path,
            frame,
            roi: request.roi,
            stats,
        })
    }
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Sensor(#[from] SensorError),
    #[error(transparent)]
    Analysis(#[from] AnalysisError),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error(transparent)]
    RawLoad(#[from] RawFrameLoadError),
}
