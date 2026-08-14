//! 帧变换节点：由数据输入触发的转换节点（范式样板）。
//!
//! 引擎语义下 `rtspSource` 已把「连接 + 解码」合并，因此 `rtspDecoder` 是 pass-through；
//! `frameSampler` 按时间降采样；`videoLayer`/`imageLayer` 是可见性标记的 pass-through。
//!
//! 这是「转换节点」的完整样板：`on_input` 收到上游帧 → 变换 → `emit` 到输出端口。

use crate::{
    engine::{DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec},
    platform::host_monotonic_time_ns,
};

/// RTSP 解码节点：解码已在 `rtspSource` 内完成，这里原样转发视频帧。
pub struct RtspDecoderFactory;

impl NodeFactory for RtspDecoderFactory {
    fn kind(&self) -> &'static str {
        "rtspDecoder"
    }

    fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(PassThroughNode {
            kind: "rtspDecoder",
            output_port: "frames",
        }))
    }
}

/// 视频图层节点：可见性标记 + 帧转发。
pub struct VideoLayerFactory;

impl NodeFactory for VideoLayerFactory {
    fn kind(&self) -> &'static str {
        "videoLayer"
    }

    fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(PassThroughNode {
            kind: "videoLayer",
            output_port: "layer",
        }))
    }
}

/// 图像图层节点：可见性标记 + 帧转发。
pub struct ImageLayerFactory;

impl NodeFactory for ImageLayerFactory {
    fn kind(&self) -> &'static str {
        "imageLayer"
    }

    fn instantiate(&self, _spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(PassThroughNode {
            kind: "imageLayer",
            output_port: "layer",
        }))
    }
}

/// pass-through 转换节点：原样转发视频/图像帧。
pub struct PassThroughNode {
    kind: &'static str,
    output_port: &'static str,
}

impl NodeInstance for PassThroughNode {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for frames");
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        match packet {
            DataPacket::VideoFrame(_) | DataPacket::ImageFrame(_) => {
                let _ = rt.emit(self.output_port, packet);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 帧降采样节点：按目标 fps 丢弃中间帧。
pub struct FrameSamplerFactory;

impl NodeFactory for FrameSamplerFactory {
    fn kind(&self) -> &'static str {
        "frameSampler"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(FrameSamplerNode {
            spec,
            last_emit_ns: None,
        }))
    }
}

pub struct FrameSamplerNode {
    spec: NodeSpec,
    last_emit_ns: Option<u64>,
}

impl NodeInstance for FrameSamplerNode {
    fn kind(&self) -> &'static str {
        "frameSampler"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        let fps_limit = config_f64(&self.spec, "fpsLimit", 30.0).max(1.0);
        rt.report_state(
            NodeRuntimeState::Ready,
            format!("downsampling to {fps_limit:.0} fps"),
        );
        Ok(())
    }

    fn on_input(
        &mut self,
        _port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let DataPacket::VideoFrame(frame) = packet else {
            return Ok(());
        };
        let now = host_monotonic_time_ns();
        let interval_ns = fps_interval_ns(&self.spec);
        if self
            .last_emit_ns
            .is_none_or(|last| now.saturating_sub(last) >= interval_ns)
        {
            self.last_emit_ns = Some(now);
            let _ = rt.emit("frames", DataPacket::VideoFrame(frame));
        }
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

fn fps_interval_ns(spec: &NodeSpec) -> u64 {
    let fps = config_f64(spec, "fpsLimit", 30.0).clamp(1.0, 240.0);
    (1_000_000_000.0 / fps) as u64
}

fn config_f64(spec: &NodeSpec, key: &str, fallback: f64) -> f64 {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(fallback)
}
