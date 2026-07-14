//! Camera Toolbox 核心领域类型。
//!
//! 本 crate 只包含纯数据结构和纯计算逻辑，不触碰 SSH、进程、GUI 或寄存器 IO。

pub mod analysis;
pub mod color;
pub mod raw;
pub mod raw_probe;
pub mod sensor;

pub use analysis::{
    AnalysisError, HistogramSeries, RawBayerHistogram, RawRoiAnalysis, Roi, RoiStats,
    analyze_raw_roi, analyze_raw_roi_with_cancel, analyze_roi, analyze_roi_with_cancel,
};
pub use color::{
    BayerPrepareDiagnostics, CfaQuad, CfaSite, ColorPipelineParams, ColorPixel,
    ColorRenderDiagnostics, ColorRenderError, DEFAULT_DISPLAY_GAMMA, DisplayTransform, LinearRgb,
    PreparedBayer, Rgb8, render_pixel_at,
};
pub use raw::{BayerPattern, RawEncoding, RawFrame, RawFrameError, RawSpec};
pub use raw_probe::{
    RawContainer, RawEndian, RawProbeCandidate, RawProbeInput, RawProbeReport, probe_raw_candidates,
};
