//! 引擎节点规格：节点/端口的纯数据描述。
//!
//! kind 使用字符串标识（与前端 `NodeKind` 的 camelCase 序列化一致），
//! 由 [`crate::engine::NodeRegistry`] 做运行时动态分发；新增节点无需改枚举。

use std::fmt;

use serde::Serialize;

/// 节点实例标识（引擎内唯一，对应前端 `WorkflowNode.id`）。
pub type NodeId = String;
/// 节点类型标识（`rtspSource`、`viewer`、`calibrationSolver`…）。
pub type NodeKindId = String;
/// 端口标识（节点内唯一）。
pub type PortId = String;
/// 端口数据类型标识（`stream.video-frame`、`image.frame`…）。
pub type PortKindId = String;

/// 端口基数约束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortCardinality {
    One,
    Many,
}

/// 端口规格：节点实例化时由引擎从图定义投影而来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSpec {
    pub id: PortId,
    pub label: String,
    pub kind: PortKindId,
    pub cardinality: PortCardinality,
    /// 前置输入是否必需；必需输入未连接时节点进入 `Disabled`。
    pub required: bool,
}

/// 节点规格：一个节点实例的完整静态描述。
#[derive(Debug, Clone, PartialEq)]
pub struct NodeSpec {
    pub id: NodeId,
    pub kind: NodeKindId,
    pub title: String,
    pub inputs: Vec<PortSpec>,
    pub outputs: Vec<PortSpec>,
    pub config: serde_json::Value,
}

/// 节点运行时状态，由引擎根据「职责 + 前置输入连接 + 实例报告」推导。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRuntimeState {
    /// 前置输入未满足（缺失连接或上游不可用）。
    Disabled,
    /// 前置满足但未激活。
    Idle,
    /// 可执行（源节点已连接、按钮节点可触发）。
    Ready,
    /// 正在执行/产生数据。
    Running,
    /// 执行失败。
    Error,
    /// 非致命告警（与前端 `NodeRuntimeState::Warning` 对齐）。
    Warning,
}

impl fmt::Display for NodeRuntimeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Disabled => "disabled",
            Self::Idle => "idle",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Error => "error",
            Self::Warning => "warning",
        };
        f.write_str(label)
    }
}

/// 节点状态快照，经引擎上报给前端。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatusReport {
    pub node_id: NodeId,
    pub state: NodeRuntimeState,
    pub diagnostic: String,
}

/// 节点级事件（日志/诊断），经引擎上报给前端 Console。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeEvent {
    pub node_id: NodeId,
    pub message: String,
}
