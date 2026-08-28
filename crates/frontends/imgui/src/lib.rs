//! pongbot-calib-tool：X5 双目标定工具的领域层模块集合。
//!
//! Godot 前端迁移后的 UI 无关业务模块（预览/求解/EEPROM/控制器）全部放在 lib target，
//! `main.rs`（Dear ImGui 前端）与集成测试共用；二进制入口只做窗口/渲染/交互。

pub mod controller;
pub mod eeprom;
pub mod eeprom_history;
pub mod guide_overlay;
pub mod observability;
pub mod preview;
pub mod solve;
pub mod theme;
pub mod x5;
