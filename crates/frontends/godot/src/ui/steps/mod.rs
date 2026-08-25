//! 5 步向导的步骤定义与步骤面板构建。

pub mod connect;

/// 向导步骤标识；顺序即流程顺序。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StepId {
    Connect = 0,
    Preview = 1,
    Capture = 2,
    Solve = 3,
    Eeprom = 4,
}

impl StepId {
    /// 由索引构造；越界收敛到 EEPROM（最后一步）。
    pub fn from_index(index: usize) -> Self {
        match index {
            0 => Self::Connect,
            1 => Self::Preview,
            2 => Self::Capture,
            3 => Self::Solve,
            _ => Self::Eeprom,
        }
    }
}

/// 步骤标题（中文，面向操作员）。
pub const STEP_TITLES: [&str; 5] = [
    "连接设备",
    "双路预览",
    "自动采集",
    "求解检查",
    "EEPROM 写入",
];
