//! Camera Toolbox 核心领域类型。
//!
//! 本 crate 只包含纯数据结构和纯计算逻辑，不触碰 SSH、进程、GUI 或寄存器 IO。

pub mod analysis;
pub mod raw;
pub mod sensor;

pub use analysis::{AnalysisError, Roi, RoiStats, analyze_roi};
pub use raw::{BayerPattern, RawEncoding, RawFrame, RawFrameError, RawSpec};
