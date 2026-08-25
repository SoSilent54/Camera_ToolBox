//! 边级流事件：只表达一次数据包成功沿某条边投递的脉冲。

use serde::Serialize;

/// 图执行器接线时固化的边端点信息，供运行时成功投递后还原到具体边。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeLink {
    pub edge_id: String,
    pub source_node_id: String,
    pub source_port_id: String,
    pub target_node_id: String,
    pub target_port_id: String,
}

/// 单次边级脉冲：后端成功把某个数据包送达某条边的下游端口时上报。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeFlowPulse {
    pub edge_id: String,
    pub source_node_id: String,
    pub source_port_id: String,
    pub target_node_id: String,
    pub target_port_id: String,
    pub packet_kind: String,
    pub sequence: Option<u64>,
    pub emitted_at_ns: u64,
}
