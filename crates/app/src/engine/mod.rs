//! 数据流引擎：节点抽象、数据通道、图执行器。
//!
//! 延续 `StreamService` 的「同步 trait + 内部线程」模式，纯 `std::sync`，不引入 tokio。
//! 节点按 kind 注册进 [`NodeRegistry`], 由 [`GraphEngine`] 实例化、接线并驱动。

pub mod channel;
pub mod flow;
pub mod graph;
pub mod node;
pub mod nodes;
pub mod packet;
pub mod registry;
pub mod runtime;
pub mod services;
pub mod spec;

pub use channel::{create_mailbox, ChannelFull, MailboxReceiver, MailboxSender, NodeMessage};
pub use flow::EdgeFlowPulse;
pub use graph::{EdgeSpec, GraphBuildError, GraphEngine, GraphSpec, PortEndpoint};
pub use node::{kinds, NodeAction, NodeError, NodeFactory, NodeInstance};
pub use nodes::register_builtin;
pub use packet::{
    BayerPattern, CalibrationBoardKind, CalibrationBoardParams, CalibrationBoardParamsKind,
    CalibrationFrameScore, CalibrationParameterError, CalibrationVector3, CameraModelKind,
    CameraModelParams, CameraModelParamsKind, CaptureMode, CaptureRequest, CaptureSignal,
    CaptureTarget, CaptureTrigger, ColorMetadata, ColorSpace, DataPacket, DetectionPacket,
    DetectionPose, DetectionPoseConvention, DetectionPoseKind, DistortionModelKind,
    DistortionModelParams, DistortionModelParamsKind, FrameProvenance, ImageFrame, ImageFrameError,
    ImageFrameFormat, ImageFrameIdentity, ImagePlane, RawMetadata,
};
pub use registry::NodeRegistry;
pub use runtime::{NodeReporter, NodeRuntime, OutputRegistry, SpawnContext};
pub use services::{EngineServices, StreamServiceFactory};
pub use spec::{
    NodeEvent, NodeId, NodeKindId, NodeRuntimeState, NodeSpec, NodeStatusReport, PortCardinality,
    PortId, PortKindId, PortSpec,
};
