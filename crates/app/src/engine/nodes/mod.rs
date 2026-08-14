//! 引擎内置节点实现。

pub mod rtsp_source;
pub mod viewer;

use crate::engine::NodeRegistry;

pub use rtsp_source::{RtspSourceFactory, RtspSourceNode};
pub use viewer::{ViewerFactory, ViewerNode};

/// 注册引擎内置节点到注册表。
pub fn register_builtin(registry: &mut NodeRegistry) {
    registry.register(Box::new(RtspSourceFactory));
    registry.register(Box::new(ViewerFactory));
}
