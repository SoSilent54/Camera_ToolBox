//! 向导步骤定义与步骤面板构建。
//!
//! 流程（用户确认）：Step 1 连通后自动打开双路 RTSP 预览并进入采集引导；
//! Step 2 双路预览与采集完成后自动进入 Step 3 求解与 EEPROM 写入。

pub mod connect;
pub mod eeprom_step;
pub mod preview;
pub mod solve_step;

/// 向导步骤标识；顺序即流程顺序。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StepId {
    Connect = 0,
    Preview = 1,
    Solve = 2,
}

impl StepId {
    /// 由索引构造；越界收敛到 Solve（最后一步）。
    pub fn from_index(index: usize) -> Self {
        match index {
            0 => Self::Connect,
            1 => Self::Preview,
            _ => Self::Solve,
        }
    }
}

/// 步骤标题（中文，面向操作员）。
pub const STEP_TITLES: [&str; 3] = ["连接设备", "双路预览与采集", "求解检查与 EEPROM 写入"];
