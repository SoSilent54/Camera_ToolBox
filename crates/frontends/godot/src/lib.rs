//! pongbot-calib-tool：Godot 桌面端 X5_233 标定工具（gdext 入口）。
//!
//! 全部 UI 用 Rust 代码构建（不依赖 Godot 编辑器可视化搭建）；
//! 运行：`godot --path crates/frontends/godot/godot`。

use godot::classes::Node;
use godot::prelude::*;

/// 应用根节点：后续在此挂接 5 步向导 UI 与领域层控制器。
#[derive(GodotClass)]
#[class(init, base = Node)]
pub struct CalibApp {
    base: Base<Node>,
}

#[godot_api]
impl INode for CalibApp {
    fn ready(&mut self) {
        godot_print!("pongbot-calib-tool: CalibApp ready");
    }
}

struct CalibExtension;

/// GDExtension 入口：gdext 宏自动注册所有 `#[derive(GodotClass)]` 类型。
#[gdextension]
unsafe impl ExtensionLibrary for CalibExtension {}
