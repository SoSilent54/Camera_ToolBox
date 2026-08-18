//! 纯数据流组合节点：fan-in 聚合、数据集累积、图像栅格覆盖与采集引导。
//!
//! 除 `OverlayComposer` 外，标定节点使用与端口 kind 对应的 `DataPacket` 变体，避免 JSON
//! 负载与声明端口脱节：检测被累积为 dataset，coverage 从角点中心计算实际占用栅格，
//! `PoseGuide` 仅输出图像栅格目标，绝不冒充相机 6DoF 位姿。
//!
//! - `OverlayComposer`：多路 video/image/overlay 输入 → 聚合 `scene` 输出（fan-in pass-through）。
//! - `DatasetCollector`：累积 `detection`，`Trigger` 输出 `dataset`，`clear` 清空内存样本。
//! - `CoverageAnalyzer`：`dataset` → 以棋盘角点中心统计的 `coverage`（+ overlay）。
//! - `PoseGuide`：`coverage` → 下一个未覆盖的图像栅格 `target`（+ overlay）。

use std::{collections::BTreeSet, sync::Arc};

use camera_toolbox_core::ChessboardDetection;
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
// DatasetCollector：detection 累积，Trigger 输出 dataset
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
    samples: Vec<Arc<ChessboardDetection>>,
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
        if port != "detection" {
            return Ok(());
        }
        let DataPacket::Detection(detection) = packet else {
            return Err(NodeError::Precondition(
                "datasetCollector.detection requires calib.detection".to_owned(),
            ));
        };
        let max_samples = config_usize(&self.spec, "maxSamples", 80);
        if self.samples.len() < max_samples {
            self.samples.push(detection);
        }
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => {
                let samples: Vec<&ChessboardDetection> =
                    self.samples.iter().map(Arc::as_ref).collect();
                let dataset = json!({
                    "kind": "calib.dataset.v1",
                    "samples": samples,
                    "count": self.samples.len(),
                });
                rt.emit("dataset", DataPacket::Dataset(Arc::new(dataset)))?;
                rt.report_event(format!(
                    "emitted dataset with {} samples",
                    self.samples.len()
                ));
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
        let DataPacket::Dataset(dataset) = packet else {
            return Err(NodeError::Precondition(
                "coverageAnalyzer.dataset requires calib.dataset".to_owned(),
            ));
        };
        let samples = dataset_samples(&dataset)?;
        let (grid_cols, grid_rows) = self.grid()?;
        let coverage = grid_coverage(&samples, grid_cols, grid_rows)?;
        rt.emit("coverage", DataPacket::Coverage(Arc::new(coverage.clone())))?;
        if find_output_port(&self.spec, "overlay").is_some() {
            emit_json(
                rt,
                "overlay",
                json!({"kind": "overlay", "coverage": coverage}),
            )?;
        }
        rt.report_event(format!("coverage: {} samples", samples.len()));
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
    fn grid(&self) -> Result<(u64, u64), NodeError> {
        let cols = config_u64(&self.spec, "gridCols", 6);
        let rows = config_u64(&self.spec, "gridRows", 4);
        if cols == 0 || rows == 0 {
            return Err(NodeError::Config(
                "gridCols and gridRows must be positive".to_owned(),
            ));
        }
        cols.checked_mul(rows)
            .ok_or_else(|| NodeError::Config("coverage grid is too large".to_owned()))?;
        Ok((cols, rows))
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
        if !config_bool(&self.spec, "enabled", true) {
            rt.report_event("pose guide disabled");
            return Ok(());
        }
        let DataPacket::Coverage(coverage) = packet else {
            return Err(NodeError::Precondition(
                "poseGuide.coverage requires calib.coverage".to_owned(),
            ));
        };
        let Some((col, row)) = first_missing_cell(&coverage) else {
            rt.report_event("coverage complete; no image-grid target");
            return Ok(());
        };
        let target = json!({
            "kind": "capture.target.v1",
            "gridCol": col,
            "gridRow": row,
            "reason": "uncovered-image-grid-cell",
        });
        rt.emit("target", DataPacket::Target(Arc::new(target.clone())))?;
        if find_output_port(&self.spec, "overlay").is_some() {
            emit_json(
                rt,
                "overlay",
                json!({"kind": "overlay", "nextTarget": target}),
            )?;
        }
        rt.report_event(format!("guided next image-grid cell ({col}, {row})"));
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

/// 解码 DatasetCollector 唯一产生的 payload，拒绝手工拼接的弱类型数据。
fn dataset_samples(dataset: &serde_json::Value) -> Result<Vec<ChessboardDetection>, NodeError> {
    if dataset.get("kind").and_then(serde_json::Value::as_str) != Some("calib.dataset.v1") {
        return Err(NodeError::Precondition(
            "dataset payload kind must be calib.dataset.v1".to_owned(),
        ));
    }
    serde_json::from_value(dataset.get("samples").cloned().unwrap_or_default())
        .map_err(|error| NodeError::Precondition(format!("invalid dataset samples: {error}")))
}

/// 以每帧棋盘格角点的图像中心，计算其在图像二维栅格中的实际占用位置。
fn grid_coverage(
    samples: &[ChessboardDetection],
    grid_cols: u64,
    grid_rows: u64,
) -> Result<serde_json::Value, NodeError> {
    let mut occupied = BTreeSet::new();
    for (index, detection) in samples.iter().enumerate() {
        if detection.image_size.width == 0
            || detection.image_size.height == 0
            || detection.corners.is_empty()
        {
            return Err(NodeError::Precondition(format!(
                "dataset sample {index} has no usable image geometry"
            )));
        }
        let (sum_x, sum_y) =
            detection
                .corners
                .iter()
                .try_fold((0.0_f64, 0.0_f64), |(x, y), corner| {
                    if !corner.is_finite() {
                        return Err(NodeError::Precondition(format!(
                            "dataset sample {index} contains non-finite corner"
                        )));
                    }
                    Ok((x + f64::from(corner.x), y + f64::from(corner.y)))
                })?;
        let count = detection.corners.len() as f64;
        let normalized_x = (sum_x / count / f64::from(detection.image_size.width)).clamp(0.0, 1.0);
        let normalized_y = (sum_y / count / f64::from(detection.image_size.height)).clamp(0.0, 1.0);
        let col = (normalized_x * grid_cols as f64).floor() as u64;
        let row = (normalized_y * grid_rows as f64).floor() as u64;
        occupied.insert((col.min(grid_cols - 1), row.min(grid_rows - 1)));
    }
    let total_cells = grid_cols * grid_rows;
    let missing_cells = (0..grid_rows)
        .flat_map(|row| (0..grid_cols).map(move |col| (col, row)))
        .filter(|cell| !occupied.contains(cell))
        .map(|(col, row)| json!({"col": col, "row": row}))
        .collect::<Vec<_>>();
    Ok(json!({
        "kind": "calib.coverage.v1",
        "sampleCount": samples.len(),
        "occupiedCells": occupied.len(),
        "totalCells": total_cells,
        "coverageRatio": occupied.len() as f64 / total_cells as f64,
        "gridCols": grid_cols,
        "gridRows": grid_rows,
        "missingCells": missing_cells,
    }))
}

fn first_missing_cell(coverage: &serde_json::Value) -> Option<(u64, u64)> {
    let cell = coverage
        .get("missingCells")?
        .as_array()?
        .first()?
        .as_object()?;
    Some((cell.get("col")?.as_u64()?, cell.get("row")?.as_u64()?))
}

fn config_bool(spec: &NodeSpec, key: &str, fallback: bool) -> bool {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(fallback)
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
    use camera_toolbox_core::{CalibrationImageSize, CalibrationPoint};

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
        assert_eq!(node.grid().expect("default grid"), (6, 4));
    }

    #[test]
    fn coverage_is_based_on_distinct_detected_grid_cells() {
        let sample = |x, y| ChessboardDetection {
            image_size: CalibrationImageSize::new(100, 100).expect("image size"),
            corners: vec![CalibrationPoint::new(x, y)],
        };
        let coverage =
            grid_coverage(&[sample(10.0, 10.0), sample(90.0, 10.0)], 2, 2).expect("coverage");
        assert_eq!(coverage["sampleCount"], 2);
        assert_eq!(coverage["occupiedCells"], 2);
        assert_eq!(coverage["totalCells"], 4);
        assert_eq!(coverage["coverageRatio"], 0.5);
        assert_eq!(first_missing_cell(&coverage), Some((0, 1)));
    }

    #[test]
    fn dataset_payload_requires_declared_kind() {
        assert!(dataset_samples(&serde_json::json!({"samples": []})).is_err());
    }
}
