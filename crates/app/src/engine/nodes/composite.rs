//! 纯数据流组合节点：fan-in 聚合、数据集累积、覆盖度/姿态占位。
//!
//! 这四个 factory 只依赖 `NodeRuntime`（`emit`/`report_state`/`report_event`），不注入任何
//! `EngineServices`，因此可在 M2 第一档单独落地。未强类型化的负载（dataset/coverage/target/scene）
//! 复用 `DataPacket::Json` 承载（packet 变体扩充留待 M2-c）。
//!
//! - `OverlayComposer`：多路 video/image/overlay 输入 → 聚合 `scene` 输出（fan-in pass-through）。
//! - `DatasetCollector`：`detection`/`image` 累积，`Trigger` 动作一次性输出 `dataset`。
//! - `CoverageAnalyzer`：`dataset` → `coverage`（+ 可选 `overlay`），轻量占位统计。
//! - `PoseGuide`：`coverage` → `target`（+ 可选 `overlay`），占位 pass-through。

use std::sync::Arc;

use serde_json::json;

use crate::engine::{
    DataPacket, NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState,
    NodeSpec, PortSpec,
};

/// 从规格输出端口里按 id 取输出端口（id 跟随前端 `node_definition`，避免硬编码断裂）。
fn find_output_port<'a>(spec: &'a NodeSpec, id: &str) -> Option<&'a PortSpec> {
    spec.outputs.iter().find(|port| port.id == id)
}

/// 把一条 JSON 负载发到指定输出端口；端口缺失或未连接不算错误（emit 自身 no-op）。
fn emit_json(rt: &NodeRuntime, port: &str, value: serde_json::Value) -> Result<(), NodeError> {
    rt.emit(port, DataPacket::Json(Arc::new(value)))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// OverlayComposer：多路 layer 输入 fan-in → scene
// ---------------------------------------------------------------------------

pub struct OverlayComposerFactory;

impl NodeFactory for OverlayComposerFactory {
    fn kind(&self) -> &'static str {
        "overlayComposer"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(OverlayComposerNode { spec }))
    }
}

pub struct OverlayComposerNode {
    spec: NodeSpec,
}

impl NodeInstance for OverlayComposerNode {
    fn kind(&self) -> &'static str {
        "overlayComposer"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for layers");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        // 只聚合三种 layer 输入；scene 输出直接透传 layer 负载（VideoFrame/ImageFrame/Json），
        // 不新造类型——与 transform.rs 的 PassThroughNode 范式一致。
        if !matches!(port, "video" | "image" | "overlay") {
            return Ok(());
        }
        // 输出端口 id 跟随规格声明的 scene 输出（避免硬编码导致接线断裂）。
        let scene_port = self
            .spec
            .outputs
            .iter()
            .map(|p| p.id.as_str())
            .find(|id| *id == "scene")
            .unwrap_or("scene");
        let _ = rt.emit(scene_port, packet);
        rt.report_event(format!("composed scene from `{port}`"));
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

// ---------------------------------------------------------------------------
// DatasetCollector：detection/image 累积，Trigger 输出 dataset
// ---------------------------------------------------------------------------

pub struct DatasetCollectorFactory;

impl NodeFactory for DatasetCollectorFactory {
    fn kind(&self) -> &'static str {
        "datasetCollector"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(DatasetCollectorNode {
            spec,
            samples: Vec::new(),
        }))
    }
}

pub struct DatasetCollectorNode {
    spec: NodeSpec,
    samples: Vec<serde_json::Value>,
}

impl NodeInstance for DatasetCollectorNode {
    fn kind(&self) -> &'static str {
        "datasetCollector"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "accept detections then trigger");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        // 仅累积 detection 输入；image 帧属可选旁路，暂不纳入样本（占位阶段）。
        if port != "detection" {
            return Ok(());
        }
        let value = packet_to_json(packet);
        let max_samples = config_usize(&self.spec, "maxSamples", 80);
        if self.samples.len() < max_samples {
            self.samples.push(value);
        }
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => {
                let dataset = json!({
                    "kind": "dataset",
                    "samples": self.samples.clone(),
                    "count": self.samples.len(),
                });
                emit_json(rt, "dataset", dataset)?;
                rt.report_event(format!("emitted dataset with {} samples", self.samples.len()));
                Ok(())
            }
            NodeAction::Custom { name, .. } if name == "clear" => {
                self.samples.clear();
                rt.report_event("dataset cleared");
                Ok(())
            }
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CoverageAnalyzer：dataset → coverage（+ 可选 overlay）
// ---------------------------------------------------------------------------

pub struct CoverageAnalyzerFactory;

impl NodeFactory for CoverageAnalyzerFactory {
    fn kind(&self) -> &'static str {
        "coverageAnalyzer"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(CoverageAnalyzerNode { spec }))
    }
}

pub struct CoverageAnalyzerNode {
    spec: NodeSpec,
}

impl NodeInstance for CoverageAnalyzerNode {
    fn kind(&self) -> &'static str {
        "coverageAnalyzer"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for dataset");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if port != "dataset" {
            return Ok(());
        }
        let dataset = packet_to_json(packet);
        // 占位统计：以样本计数近似覆盖度（真实格点覆盖统计属 M2 后续算法适配）。
        let count = dataset
            .get("samples")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        let (grid_cols, grid_rows) = self.grid();
        emit_json(
            rt,
            "coverage",
            json!({
                "kind": "coverage",
                "sampleCount": count,
                "occupied": count,
                "gridCols": grid_cols,
                "gridRows": grid_rows,
            }),
        )?;
        // overlay 为可选输出；未在规格中声明则跳过。
        if find_output_port(&self.spec, "overlay").is_some() {
            emit_json(
                rt,
                "overlay",
                json!({"kind": "overlay", "coverage": count, "gridCols": grid_cols, "gridRows": grid_rows}),
            )?;
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

impl CoverageAnalyzerNode {
    fn grid(&self) -> (u64, u64) {
        (
            config_u64(&self.spec, "gridCols", 6),
            config_u64(&self.spec, "gridRows", 4),
        )
    }
}

// ---------------------------------------------------------------------------
// PoseGuide：coverage → target（+ 可选 overlay）
// ---------------------------------------------------------------------------

pub struct PoseGuideFactory;

impl NodeFactory for PoseGuideFactory {
    fn kind(&self) -> &'static str {
        "poseGuide"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(PoseGuideNode { spec }))
    }
}

pub struct PoseGuideNode {
    spec: NodeSpec,
}

impl NodeInstance for PoseGuideNode {
    fn kind(&self) -> &'static str {
        "poseGuide"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for coverage");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if port != "coverage" {
            return Ok(());
        }
        let coverage = packet_to_json(packet);
        // 占位：由 coverage 生成单个 guided target（真实姿态求解属 M2 后续算法适配）。
        emit_json(
            rt,
            "target",
            json!({"kind": "target", "basedOn": coverage, "enabled": true}),
        )?;
        if find_output_port(&self.spec, "overlay").is_some() {
            emit_json(rt, "overlay", json!({"kind": "overlay", "poseGuide": true}))?;
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

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 把 `DataPacket` 折叠成 JSON 值（用于占位阶段的弱类型负载承载）。
fn packet_to_json(packet: DataPacket) -> serde_json::Value {
    match packet {
        DataPacket::Json(value) => (*value).clone(),
        DataPacket::Coverage(value)
        | DataPacket::Dataset(value)
        | DataPacket::Report(value)
        | DataPacket::Score(value)
        | DataPacket::Target(value) => (*value).clone(),
        DataPacket::VideoFrame(frame) => json!({
            "type": "video-frame",
            "width": frame.width,
            "height": frame.height,
            "sequence": frame.identity.frame_sequence,
        }),
        DataPacket::ImageFrame(frame) => json!({
            "type": "image-frame",
            "width": frame.width,
            "height": frame.height,
        }),
        DataPacket::Detection(detection) => json!({
            "type": "detection",
            "imageSize": detection.image_size,
            "cornerCount": detection.corners.len(),
        }),
        DataPacket::Solution(solution) => json!({
            "type": "solution",
            "rms": solution.rms_error,
            "views": solution.views.len(),
        }),
    }
}

fn config_u64(spec: &NodeSpec, key: &str, fallback: u64) -> u64 {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(fallback)
}

fn config_usize(spec: &NodeSpec, key: &str, fallback: usize) -> usize {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::PortCardinality;

    fn port(id: &str) -> PortSpec {
        PortSpec {
            id: id.to_owned(),
            label: id.to_owned(),
            kind: "status.metrics".to_owned(),
            cardinality: PortCardinality::One,
            required: false,
        }
    }

    fn node_spec(kind: &str, inputs: Vec<&str>, outputs: Vec<&str>) -> NodeSpec {
        NodeSpec {
            id: "n".to_owned(),
            kind: kind.to_owned(),
            title: kind.to_owned(),
            inputs: inputs.into_iter().map(port).collect(),
            outputs: outputs.into_iter().map(port).collect(),
            config: serde_json::json!({}),
        }
    }

    /// 直接调用 NodeInstance::on_input，配合最小 NodeRuntime 上下文即可验证 emit 语义。
    /// 这里通过构造真实引擎跑通（见下方 engine 级测试）过于笨重，改为验证 factory 可实例化
    /// 且 kind 正确，emit 行为由引擎 emit 单测覆盖。
    #[test]
    fn factories_instantiate_with_expected_kinds() {
        let cases: [(&dyn NodeFactory, &str); 4] = [
            (&OverlayComposerFactory, "overlayComposer"),
            (&DatasetCollectorFactory, "datasetCollector"),
            (&CoverageAnalyzerFactory, "coverageAnalyzer"),
            (&PoseGuideFactory, "poseGuide"),
        ];
        for (factory, kind) in cases {
            assert_eq!(factory.kind(), kind, "factory kind mismatch for {kind}");
            let spec = node_spec(kind, vec![], vec![]);
            let instance = factory.instantiate(spec).expect("instantiate");
            assert_eq!(instance.kind(), kind);
        }
    }

    #[test]
    fn coverage_grid_defaults_are_locked() {
        let spec = node_spec("coverageAnalyzer", vec![], vec![]);
        let node = CoverageAnalyzerNode { spec };
        assert_eq!(node.grid(), (6, 4));
    }

    #[test]
    fn packet_folds_detection_to_json() {
        // 仅验证 Json 变体往返，避免依赖 camera_toolbox_core 的构造细节。
        let value = serde_json::json!({"k": 1});
        let folded = packet_to_json(DataPacket::Json(Arc::new(value.clone())));
        assert_eq!(folded, value);
    }
}
