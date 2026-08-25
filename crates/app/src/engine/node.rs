//! 节点抽象：工厂 + 实例。
//!
//! 同步 trait，延续 `StreamService` 的「同步接口 + 内部线程」模式；
//! 外部 IO 一律经 [`NodeRuntime::services`] 注入，不在节点内构造具体适配器。

use thiserror::Error;

use super::{packet::DataPacket, runtime::NodeRuntime, spec::NodeSpec};

/// 节点级错误。
#[derive(Debug, Error)]
pub enum NodeError {
    #[error("node config error: {0}")]
    Config(String),
    #[error("node precondition unmet: {0}")]
    Precondition(String),
    #[error("node execution failed: {0}")]
    Execution(String),
    #[error("node output channel full: {0}")]
    ChannelFull(String),
    #[error("node action unsupported: {0}")]
    UnsupportedAction(String),
}

impl From<super::channel::ChannelFull> for NodeError {
    fn from(_: super::channel::ChannelFull) -> Self {
        Self::ChannelFull("output channel full".to_owned())
    }
}
/// 节点级动作：连接/断开、一次触发、自动采集 arm/disarm，或自定义动作。
#[derive(Debug, Clone)]
pub enum NodeAction {
    Connect,
    Disconnect,
    Trigger,
    Arm,
    Disarm,
    Custom {
        name: String,
        payload: serde_json::Value,
    },
}

impl NodeAction {
    /// 动作名称（用于日志与前端按钮映射）。
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Connect => "connect",
            Self::Disconnect => "disconnect",
            Self::Trigger => "trigger",
            Self::Arm => "arm",
            Self::Disarm => "disarm",
            Self::Custom { name, .. } => name.as_str(),
        }
    }
}

/// 节点工厂：由 kind 创建实例。每种节点实现一个，注册进 [`crate::engine::NodeRegistry`]。
pub trait NodeFactory: Send + Sync {
    /// 节点类型标识（`rtspSource`、`viewer`…）。
    fn kind(&self) -> &'static str;

    /// 用规格创建一个运行态实例；此时不建立任何外部连接。
    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError>;
}

/// 节点运行态实例：由引擎在独立线程的 actor 循环里驱动。
///
/// 实现约定：
/// - `on_start`：建立内部状态；源节点在此不阻塞，真正的连接由 `on_action(Connect)` 触发。
/// - `on_input`：处理一个上游数据包，同步快速返回；耗时变换应经 `NodeRuntime::spawn` 后台执行。
/// - `on_action`：响应连接/断开/触发；副作用（连接 RTSP、执行 I²C）在此或后台任务完成。
/// - `on_stop`：取消后台任务、释放外部连接。
pub trait NodeInstance: Send {
    fn kind(&self) -> &'static str;

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError>;

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError>;

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError>;

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError>;

    /// 在 actor 线程应用增量配置；需要释放外部会话的节点可覆盖此钩子。
    fn on_config_update(
        &mut self,
        _config: serde_json::Value,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        Ok(())
    }
}

/// 节点 kind 常量，与前端 `NodeKind` camelCase 序列化一致。
pub mod kinds {
    pub const RTSP_SOURCE: &str = "rtspSource";
    pub const RTSP_DECODER: &str = "rtspDecoder";
    pub const VIEWER: &str = "viewer";
    pub const IMAGE_FILE_SOURCE: &str = "imageFileSource";
    pub const LOCAL_WORKSPACE: &str = "localWorkspace";
    pub const SFTP_WORKSPACE: &str = "sftpWorkspace";
    pub const FILE_BROWSER: &str = "fileBrowser";
    pub const SSH_SESSION: &str = "sshSession";
}
