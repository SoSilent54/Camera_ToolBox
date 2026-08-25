//! 纯数据流组合节点：fan-in 聚合、数据集累积、图像栅格覆盖与采集引导。
//!
//! 除 `OverlayComposer` 外，标定节点使用与端口 kind 对应的 `DataPacket` 变体，避免 JSON
//! 负载与声明端口脱节：DatasetCollector 仅以完整帧身份关联 image/detection/score 元数据，
//! 不做时间对齐且不把图像 bytes 内联；coverage 从 accepted/enabled detection 的角点中心计算
//! 实际占用栅格，`PoseGuide` 仅输出图像栅格目标，绝不冒充相机 6DoF 位姿。
//!
//! - `OverlayComposer`：多路 video/image/overlay 输入 → 聚合 `scene` 输出（fan-in pass-through）。
//! - `DatasetCollector`：维护可接受/启用的 rich sample list，`Trigger` 输出 `dataset`。
//! - `CoverageAnalyzer`：`dataset` → 仅统计 accepted/enabled 样本的 `coverage`（+ overlay）。
//! - `PoseGuide`：`coverage` → 下一个未覆盖的图像栅格 `target`（+ overlay）。

use std::{collections::BTreeSet, sync::Arc};

use camera_toolbox_core::ChessboardDetection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    engine::{
        CalibrationFrameScore, DataPacket, FrameProvenance, ImageFrame, ImageFrameIdentity,
        NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec,
        PortSpec,
    },
    platform::{SourcePts, SourcePtsProvenance},
};

/// Dataset JSON 的唯一版本标识。旧的 `samples: [ChessboardDetection]` 不再被消费者接受。
pub(crate) const CALIBRATION_DATASET_KIND: &str = "calib.dataset.v1";

/// 不持有图像字节的运行时图像引用。它只允许消费者定位或描述图像，不能把大图塞进 Dataset。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DatasetImageRef {
    #[serde(rename = "ref")]
    pub(crate) reference: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: Option<String>,
}

/// 样本质量分数及其同帧序号；关联仅依赖完整 `ImageFrameIdentity`，不是时间邻近匹配。
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DatasetSampleScore {
    pub(crate) score: f64,
    pub(crate) frame_sequence: u64,
}

/// 是否人工接受、是否启用。Coverage 与 Solver 必须同时要求二者为真。
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct DatasetSampleAcceptance {
    pub(crate) accepted: bool,
    pub(crate) enabled: bool,
}

impl Default for DatasetSampleAcceptance {
    fn default() -> Self {
        Self {
            accepted: true,
            enabled: true,
        }
    }
}

/// 可持久化的 Dataset 样本。`provenance` 记录来源身份的结构化元数据，不包含像素平面。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CalibrationDatasetSample {
    pub(crate) id: String,
    pub(crate) image_ref: DatasetImageRef,
    pub(crate) detection: ChessboardDetection,
    pub(crate) score: Option<DatasetSampleScore>,
    pub(crate) acceptance: DatasetSampleAcceptance,
    pub(crate) provenance: Value,
}

/// Dataset 的传输形状；数学消费者只从其中的 accepted/enabled detection 取角点。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CalibrationDataset {
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) board: Value,
    pub(crate) samples: Vec<CalibrationDatasetSample>,
    pub(crate) count: usize,
    /// 当前仅存在于节点运行态/输出快照的选中样本；绝不写入 WorkflowGraph。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) selected_sample_id: Option<String>,
}

impl CalibrationDataset {
    /// 返回能进入几何覆盖和标定求解的样本；reject 或 disable 都不能影响数学输入。
    pub(crate) fn accepted_enabled_samples(
        &self,
    ) -> impl Iterator<Item = &CalibrationDatasetSample> {
        self.samples
            .iter()
            .filter(|sample| sample.acceptance.accepted && sample.acceptance.enabled)
    }
}

/// DatasetCollector 内部状态保留精确帧身份，发出时才投影为不含 bytes 的 JSON 样本。
struct CollectedDatasetSample {
    id: String,
    image_ref: DatasetImageRef,
    detection: Arc<ChessboardDetection>,
    score: Option<DatasetSampleScore>,
    acceptance: DatasetSampleAcceptance,
    provenance: Value,
    identity: ImageFrameIdentity,
}

impl CollectedDatasetSample {
    fn wire(&self) -> CalibrationDatasetSample {
        CalibrationDatasetSample {
            id: self.id.clone(),
            image_ref: self.image_ref.clone(),
            detection: self.detection.as_ref().clone(),
            score: self.score,
            acceptance: self.acceptance,
            provenance: self.provenance.clone(),
        }
    }
}

/// 把 Dataset JSON 解析为唯一的 richer sample schema，并在进入数学消费者前校验结构不变量。
pub(crate) fn parse_calibration_dataset(dataset: &Value) -> Result<CalibrationDataset, NodeError> {
    let dataset: CalibrationDataset = serde_json::from_value(dataset.clone()).map_err(|error| {
        NodeError::Precondition(format!("invalid calibration dataset: {error}"))
    })?;
    if dataset.kind != CALIBRATION_DATASET_KIND {
        return Err(NodeError::Precondition(format!(
            "dataset payload kind must be {CALIBRATION_DATASET_KIND}"
        )));
    }
    if dataset.count != dataset.samples.len() {
        return Err(NodeError::Precondition(format!(
            "dataset count {} does not match {} samples",
            dataset.count,
            dataset.samples.len()
        )));
    }
    for (index, sample) in dataset.samples.iter().enumerate() {
        if sample.id.trim().is_empty() {
            return Err(NodeError::Precondition(format!(
                "dataset sample {index} has an empty id"
            )));
        }
        if sample.image_ref.reference.trim().is_empty()
            || sample.image_ref.width == 0
            || sample.image_ref.height == 0
        {
            return Err(NodeError::Precondition(format!(
                "dataset sample {index} has an invalid imageRef"
            )));
        }
        if sample.image_ref.width != sample.detection.image_size.width
            || sample.image_ref.height != sample.detection.image_size.height
        {
            return Err(NodeError::Precondition(format!(
                "dataset sample {index} imageRef size does not match detection image size"
            )));
        }
        let provenance_frame_sequence = sample
            .provenance
            .get("frameIdentity")
            .and_then(Value::as_object)
            .and_then(|identity| identity.get("frameSequence"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                NodeError::Precondition(format!(
                    "dataset sample {index} provenance must contain frameIdentity.frameSequence"
                ))
            })?;
        if let Some(score) = sample.score {
            if !score.score.is_finite() || !(0.0..=1.0).contains(&score.score) {
                return Err(NodeError::Precondition(format!(
                    "dataset sample {index} has an invalid score"
                )));
            }
            if score.frame_sequence != provenance_frame_sequence {
                return Err(NodeError::Precondition(format!(
                    "dataset sample {index} score frameSequence does not match provenance"
                )));
            }
        }
    }
    Ok(dataset)
}

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
// DatasetCollector：以同帧身份聚合元数据，Trigger 输出 sample-list dataset
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
            pending_images: Vec::new(),
            pending_scores: Vec::new(),
            cached_images: Vec::new(),
            selected_sample_id: None,
            next_sample_id: 1,
        }))
    }
}

pub struct DatasetCollectorNode {
    spec: NodeSpec,
    samples: Vec<CollectedDatasetSample>,
    /// image/score 可先到达；只按完整 identity 相等关联，绝不按时间戳寻找“最近帧”。
    pending_images: Vec<(ImageFrameIdentity, DatasetImageRef)>,
    pending_scores: Vec<(ImageFrameIdentity, DatasetSampleScore)>,
    /// 有界 runtime 图像缓存；键只用完整 `ImageFrameIdentity`，供已选样本的 preview 输出复用。
    cached_images: Vec<(ImageFrameIdentity, Arc<ImageFrame>)>,
    selected_sample_id: Option<String>,
    next_sample_id: u64,
}

impl NodeInstance for DatasetCollectorNode {
    fn kind(&self) -> &'static str {
        "datasetCollector"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(
            NodeRuntimeState::Ready,
            "accept calibration samples then trigger",
        );
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        match port {
            "frames" => {
                let DataPacket::VideoFrame(frame) = packet else {
                    return Err(NodeError::Precondition(
                        "datasetCollector.frames requires stream.video-frame".to_owned(),
                    ));
                };
                self.record_image(Arc::new(ImageFrame::from(frame.as_ref())))
            }

            "image" => {
                let DataPacket::ImageFrame(image) = packet else {
                    return Err(NodeError::Precondition(
                        "datasetCollector.image requires image.frame".to_owned(),
                    ));
                };
                self.record_image(image)
            }
            "detection" => {
                let DataPacket::Detection(detection) = packet else {
                    return Err(NodeError::Precondition(
                        "datasetCollector.detection requires calib.detection".to_owned(),
                    ));
                };
                self.record_detection(detection)?;
                self.emit_dataset(rt)
            }
            "score" => {
                let DataPacket::Score(score) = packet else {
                    return Err(NodeError::Precondition(
                        "datasetCollector.score requires capture.score".to_owned(),
                    ));
                };
                self.record_score(score.as_ref())
            }
            _ => Ok(()),
        }
    }

    fn on_action(&mut self, action: NodeAction, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        match action {
            NodeAction::Trigger => self.emit_dataset(rt),
            // `clear` 历史上没有 payload；继续接受 Null/任意 payload，并把空列表同步给下游。
            NodeAction::Custom { name, .. } if name == "clear" => {
                self.samples.clear();
                self.pending_images.clear();
                self.pending_scores.clear();
                self.cached_images.clear();
                self.selected_sample_id = None;
                self.emit_dataset(rt)?;
                rt.report_event("dataset cleared");
                Ok(())
            }
            NodeAction::Custom { name, payload } if name == "select" => {
                let sample_id = sample_id_from_action_payload(&payload)?.to_owned();
                let preview = self.select_sample(&sample_id)?;
                rt.emit("preview", DataPacket::ImageFrame(preview))?;
                // preview 必须先发，确保 dataset snapshot 仍是节点的 latest runtime output。
                self.emit_dataset(rt)?;
                rt.report_event(format!(
                    "selected dataset sample {sample_id} (preview emitted)"
                ));
                Ok(())
            }
            NodeAction::Custom { name, payload }
                if matches!(
                    name.as_str(),
                    "accept" | "reject" | "enable" | "disable" | "delete"
                ) =>
            {
                let sample_id = sample_id_from_action_payload(&payload)?.to_owned();
                self.apply_sample_action(&name, &sample_id)?;
                self.emit_dataset(rt)?;
                rt.report_event(format!("{name} dataset sample {sample_id}"));
                Ok(())
            }
            other => Err(NodeError::UnsupportedAction(other.name().to_owned())),
        }
    }

    fn on_config_update(
        &mut self,
        config: serde_json::Value,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let next = NodeSpec {
            config,
            ..self.spec.clone()
        };
        let max_samples = config_usize(&next, "maxSamples", 80);
        if max_samples == 0 {
            return Err(NodeError::Config(
                "datasetCollector.maxSamples must be positive".to_owned(),
            ));
        }
        self.spec = next;
        if self.samples.len() > max_samples {
            self.samples.truncate(max_samples);
        }
        while self.pending_images.len() > max_samples {
            self.pending_images.swap_remove(0);
        }
        while self.pending_scores.len() > max_samples {
            self.pending_scores.swap_remove(0);
        }
        while self.cached_images.len() > max_samples {
            self.cached_images.swap_remove(0);
        }
        self.clear_selection_if_missing();
        Ok(())
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

impl DatasetCollectorNode {
    fn max_samples(&self) -> Result<usize, NodeError> {
        let max_samples = config_usize(&self.spec, "maxSamples", 80);
        if max_samples == 0 {
            return Err(NodeError::Config(
                "datasetCollector.maxSamples must be positive".to_owned(),
            ));
        }
        Ok(max_samples)
    }

    /// 同时保存轻量引用与有界原帧缓存；缓存只按完整 identity 查找，绝不做时间邻近匹配。
    fn record_image(&mut self, image: Arc<ImageFrame>) -> Result<(), NodeError> {
        let identity = image.identity.clone();
        let image_ref = image_ref_from_image(image.as_ref());
        self.store_cached_image(image)?;
        if let Some(sample) = self
            .samples
            .iter_mut()
            .find(|sample| sample.identity == identity)
        {
            sample.image_ref = image_ref;
            return Ok(());
        }
        self.store_pending_image(identity, image_ref)
    }

    fn store_cached_image(&mut self, image: Arc<ImageFrame>) -> Result<(), NodeError> {
        let identity = image.identity.clone();
        if let Some((_, cached_image)) = self
            .cached_images
            .iter_mut()
            .find(|(cached_identity, _)| *cached_identity == identity)
        {
            *cached_image = image;
            return Ok(());
        }
        self.trim_cached_images()?;
        self.cached_images.push((identity, image));
        Ok(())
    }

    fn record_score(&mut self, score: &CalibrationFrameScore) -> Result<(), NodeError> {
        let identity = score.frame_identity.clone();
        let score = dataset_score(score)?;
        if let Some(sample) = self
            .samples
            .iter_mut()
            .find(|sample| sample.identity == identity)
        {
            sample.score = Some(score);
            return Ok(());
        }
        self.store_pending_score(identity, score)
    }

    fn record_detection(
        &mut self,
        detection: Arc<crate::engine::DetectionPacket>,
    ) -> Result<(), NodeError> {
        let identity = detection.frame_identity.clone();
        let pending_image = self.take_pending_image(&identity);
        let pending_score = self.take_pending_score(&identity);
        if let Some(sample) = self
            .samples
            .iter_mut()
            .find(|sample| sample.identity == identity)
        {
            sample.detection = Arc::clone(&detection.detection);
            if let Some(image_ref) = pending_image {
                sample.image_ref = image_ref;
            }
            if let Some(score) = pending_score {
                sample.score = Some(score);
            }
            return Ok(());
        }

        if self.samples.len() >= self.max_samples()? {
            return Ok(());
        }
        let image_ref = pending_image
            .unwrap_or_else(|| image_ref_from_detection(&identity, detection.detection.as_ref()));
        let id = self.next_sample_id()?;
        self.samples.push(CollectedDatasetSample {
            id,
            image_ref,
            detection: Arc::clone(&detection.detection),
            score: pending_score,
            acceptance: DatasetSampleAcceptance::default(),
            provenance: sample_provenance(&identity),
            identity,
        });
        Ok(())
    }

    fn emit_dataset(&self, rt: &NodeRuntime) -> Result<(), NodeError> {
        let dataset = CalibrationDataset {
            kind: CALIBRATION_DATASET_KIND.to_owned(),
            // Collector 不推测棋盘规格；若流程显式提供 board 元数据则原样透传，否则为 null。
            board: self
                .spec
                .config
                .get("board")
                .cloned()
                .unwrap_or(Value::Null),
            samples: self
                .samples
                .iter()
                .map(CollectedDatasetSample::wire)
                .collect(),
            count: self.samples.len(),
            selected_sample_id: self.selected_sample_id.clone(),
        };
        let dataset = serde_json::to_value(dataset).map_err(|error| {
            NodeError::Execution(format!("serialize calibration dataset: {error}"))
        })?;
        let eligible = self
            .samples
            .iter()
            .filter(|sample| sample.acceptance.accepted && sample.acceptance.enabled)
            .count();
        rt.emit("dataset", DataPacket::Dataset(Arc::new(dataset)))?;
        rt.report_event(format!(
            "emitted dataset with {} samples ({eligible} accepted/enabled)",
            self.samples.len()
        ));
        Ok(())
    }

    fn store_pending_image(
        &mut self,
        identity: ImageFrameIdentity,
        image_ref: DatasetImageRef,
    ) -> Result<(), NodeError> {
        if let Some((_, pending)) = self
            .pending_images
            .iter_mut()
            .find(|(pending_identity, _)| *pending_identity == identity)
        {
            *pending = image_ref;
            return Ok(());
        }
        self.trim_pending_images()?;
        self.pending_images.push((identity, image_ref));
        Ok(())
    }

    fn store_pending_score(
        &mut self,
        identity: ImageFrameIdentity,
        score: DatasetSampleScore,
    ) -> Result<(), NodeError> {
        if let Some((_, pending)) = self
            .pending_scores
            .iter_mut()
            .find(|(pending_identity, _)| *pending_identity == identity)
        {
            *pending = score;
            return Ok(());
        }
        self.trim_pending_scores()?;
        self.pending_scores.push((identity, score));
        Ok(())
    }

    fn trim_cached_images(&mut self) -> Result<(), NodeError> {
        if self.cached_images.len() >= self.max_samples()? {
            self.cached_images.swap_remove(0);
        }
        Ok(())
    }

    fn trim_pending_images(&mut self) -> Result<(), NodeError> {
        if self.pending_images.len() >= self.max_samples()? {
            self.pending_images.swap_remove(0);
        }
        Ok(())
    }

    fn trim_pending_scores(&mut self) -> Result<(), NodeError> {
        if self.pending_scores.len() >= self.max_samples()? {
            self.pending_scores.swap_remove(0);
        }
        Ok(())
    }

    fn take_pending_image(&mut self, identity: &ImageFrameIdentity) -> Option<DatasetImageRef> {
        self.pending_images
            .iter()
            .position(|(pending_identity, _)| pending_identity == identity)
            .map(|index| self.pending_images.swap_remove(index).1)
    }

    fn take_pending_score(&mut self, identity: &ImageFrameIdentity) -> Option<DatasetSampleScore> {
        self.pending_scores
            .iter()
            .position(|(pending_identity, _)| pending_identity == identity)
            .map(|index| self.pending_scores.swap_remove(index).1)
    }

    fn cached_image(&self, identity: &ImageFrameIdentity) -> Option<Arc<ImageFrame>> {
        self.cached_images
            .iter()
            .find(|(cached_identity, _)| cached_identity == identity)
            .map(|(_, image)| Arc::clone(image))
    }

    fn select_sample(&mut self, sample_id: &str) -> Result<Arc<ImageFrame>, NodeError> {
        let identity = self
            .samples
            .iter()
            .find(|sample| sample.id == sample_id)
            .map(|sample| sample.identity.clone())
            .ok_or_else(|| {
                NodeError::Precondition(format!("dataset sample `{sample_id}` does not exist"))
            })?;
        let preview = self.cached_image(&identity).ok_or_else(|| {
            NodeError::Precondition(format!(
                "dataset sample `{sample_id}` has no exact cached image preview"
            ))
        })?;
        self.selected_sample_id = Some(sample_id.to_owned());
        Ok(preview)
    }

    fn clear_selection_if_missing(&mut self) {
        let selected_missing = match self.selected_sample_id.as_deref() {
            Some(selected_sample_id) => !self
                .samples
                .iter()
                .any(|sample| sample.id == selected_sample_id),
            None => false,
        };
        if selected_missing {
            self.selected_sample_id = None;
        }
    }

    fn next_sample_id(&mut self) -> Result<String, NodeError> {
        let id = self.next_sample_id;
        self.next_sample_id = self.next_sample_id.checked_add(1).ok_or_else(|| {
            NodeError::Execution("dataset sample id counter overflowed".to_owned())
        })?;
        Ok(format!("sample-{id}"))
    }

    fn apply_sample_action(&mut self, action: &str, sample_id: &str) -> Result<(), NodeError> {
        let index = self
            .samples
            .iter()
            .position(|sample| sample.id == sample_id)
            .ok_or_else(|| {
                NodeError::Precondition(format!("dataset sample `{sample_id}` does not exist"))
            })?;
        match action {
            "delete" => {
                self.samples.remove(index);
                self.clear_selection_if_missing();
            }
            "accept" => self.samples[index].acceptance.accepted = true,
            "reject" => self.samples[index].acceptance.accepted = false,
            "enable" => self.samples[index].acceptance.enabled = true,
            "disable" => self.samples[index].acceptance.enabled = false,
            _ => return Err(NodeError::UnsupportedAction(action.to_owned())),
        }
        Ok(())
    }
}

fn sample_id_from_action_payload(payload: &Value) -> Result<&str, NodeError> {
    payload
        .get("sampleId")
        .and_then(Value::as_str)
        .filter(|sample_id| !sample_id.trim().is_empty())
        .ok_or_else(|| {
            NodeError::Precondition(
                "datasetCollector sample action requires non-empty payload.sampleId".to_owned(),
            )
        })
}

fn dataset_score(score: &CalibrationFrameScore) -> Result<DatasetSampleScore, NodeError> {
    if !score.score.is_finite() || !(0.0..=1.0).contains(&score.score) {
        return Err(NodeError::Precondition(
            "datasetCollector.score must be finite and within [0, 1]".to_owned(),
        ));
    }
    Ok(DatasetSampleScore {
        score: score.score,
        frame_sequence: score.frame_identity.frame_sequence,
    })
}

fn image_ref_from_image(image: &ImageFrame) -> DatasetImageRef {
    DatasetImageRef {
        reference: runtime_image_reference(&image.identity),
        width: image.width,
        height: image.height,
        format: Some(image.format.to_string()),
    }
}

fn image_ref_from_detection(
    identity: &ImageFrameIdentity,
    detection: &ChessboardDetection,
) -> DatasetImageRef {
    DatasetImageRef {
        reference: runtime_image_reference(identity),
        width: detection.image_size.width,
        height: detection.image_size.height,
        format: None,
    }
}

/// 当前没有稳定 artifact store 时使用不可解引用的 runtime ref；它只有身份文本，绝不包含像素。
fn runtime_image_reference(identity: &ImageFrameIdentity) -> String {
    format!("runtime://frame/{identity:?}")
}

fn sample_provenance(identity: &ImageFrameIdentity) -> Value {
    let source = match &identity.provenance {
        FrameProvenance::Stream { stream_id, channel } => {
            json!({"kind": "stream", "streamId": stream_id.as_str(), "channel": channel})
        }
        FrameProvenance::Device {
            driver,
            channel,
            camera,
            timestamp_ns,
        } => json!({
            "kind": "device",
            "driver": driver,
            "channel": channel,
            "camera": camera,
            "timestampNs": timestamp_ns,
        }),
        FrameProvenance::File { source } => json!({"kind": "file", "source": source}),
        FrameProvenance::Unknown { reason } => json!({"kind": "unknown", "reason": reason}),
    };
    json!({
        "source": source,
        "frameIdentity": {
            "frameSequence": identity.frame_sequence,
            "sourcePts": source_pts_metadata(&identity.source_pts),
            "hostMonotonicTimeNs": identity.host_monotonic_time_ns,
        },
    })
}

fn source_pts_metadata(source_pts: &SourcePts) -> Value {
    match source_pts {
        SourcePts::Known {
            ticks,
            time_base_numerator,
            time_base_denominator,
            provenance,
        } => json!({
            "kind": "known",
            "ticks": ticks,
            "timeBase": {"numerator": time_base_numerator, "denominator": time_base_denominator},
            "provenance": match provenance {
                SourcePtsProvenance::FfmpegDecodedFrame => "ffmpegDecodedFrame",
                SourcePtsProvenance::FfmpegShowinfo => "ffmpegShowinfo",
                SourcePtsProvenance::Unavailable => "unavailable",
            },
        }),
        SourcePts::Unavailable { reason } => json!({"kind": "unavailable", "reason": reason}),
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
        let dataset = parse_calibration_dataset(&dataset)?;
        // 先过滤人工拒绝和禁用项，再把只读 detection 借给覆盖算法；不复制角点或图像数据。
        let samples = dataset
            .accepted_enabled_samples()
            .map(|sample| &sample.detection)
            .collect::<Vec<_>>();
        let (grid_cols, grid_rows) = self.grid()?;
        let coverage = grid_coverage(samples.iter().copied(), grid_cols, grid_rows)?;
        rt.emit("coverage", DataPacket::Coverage(Arc::new(coverage.clone())))?;
        if find_output_port(&self.spec, "overlay").is_some() {
            emit_json(
                rt,
                "overlay",
                json!({"kind": "overlay", "coverage": coverage}),
            )?;
        }
        rt.report_event(format!(
            "coverage: {} accepted/enabled samples",
            samples.len()
        ));
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_config_update(
        &mut self,
        config: serde_json::Value,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        let next = NodeSpec {
            config,
            ..self.spec.clone()
        };
        let probe = Self { spec: next.clone() };
        probe.grid()?;
        self.spec = next;
        Ok(())
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

    fn on_config_update(
        &mut self,
        config: serde_json::Value,
        _rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        self.spec = NodeSpec {
            config,
            ..self.spec.clone()
        };
        Ok(())
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 以每帧棋盘格角点的图像中心，计算其在图像二维栅格中的实际占用位置。
///
/// 调用方必须先应用 Dataset 的 accepted/enabled 过滤；此函数只保留数学输入本身。
fn grid_coverage<'a>(
    samples: impl IntoIterator<Item = &'a ChessboardDetection>,
    grid_cols: u64,
    grid_rows: u64,
) -> Result<Value, NodeError> {
    let mut occupied = BTreeSet::new();
    let mut sample_count = 0_usize;
    for (index, detection) in samples.into_iter().enumerate() {
        sample_count = index + 1;
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
        "sampleCount": sample_count,
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
    use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};

    use crate::{
        engine::{EngineServices, NodeReporter, OutputRegistry, SpawnContext},
        platform::{DecodedVideoFrame, StreamSessionId},
    };
    use camera_toolbox_core::{CalibrationImageSize, CalibrationPoint};

    fn port(id: &str) -> PortSpec {
        PortSpec {
            id: id.to_owned(),
            label: id.to_owned(),
            kind: "status.metrics".to_owned(),
            cardinality: crate::engine::PortCardinality::One,
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
            config: json!({}),
        }
    }

    fn runtime_with_record(recorded: Arc<Mutex<Vec<DataPacket>>>) -> NodeRuntime {
        let (state_tx, _state_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("n".to_owned(), state_tx, event_tx);
        let mut outputs = OutputRegistry::default();
        outputs.set_record(Arc::new(move |packet| {
            recorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(packet);
        }));
        NodeRuntime::new(SpawnContext {
            outputs,
            reporter,
            services: Arc::new(EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        })
    }

    fn identity(sequence: u64) -> ImageFrameIdentity {
        ImageFrameIdentity {
            provenance: FrameProvenance::Stream {
                stream_id: StreamSessionId::new("test-stream").expect("stream id"),
                channel: 3,
            },
            frame_sequence: sequence,
            source_pts: SourcePts::Unavailable {
                reason: "test source PTS".to_owned(),
            },
            host_monotonic_time_ns: sequence * 1_000,
            device_timestamp_ns: None,
        }
    }

    fn detection(width: u32, height: u32, x: f32, y: f32) -> ChessboardDetection {
        ChessboardDetection {
            image_size: CalibrationImageSize::new(width, height).expect("image size"),
            corners: vec![CalibrationPoint::new(x, y)],
        }
    }

    fn rich_sample(
        id: &str,
        detection: ChessboardDetection,
        accepted: bool,
        enabled: bool,
    ) -> Value {
        json!({
            "id": id,
            "imageRef": {
                "ref": format!("runtime://frame/{id}"),
                "width": detection.image_size.width,
                "height": detection.image_size.height,
                "format": "GRAY8",
            },
            "detection": detection,
            "score": {"score": 0.5, "frameSequence": 1},
            "acceptance": {"accepted": accepted, "enabled": enabled},
            "provenance": {
                "source": {"kind": "test"},
                "frameIdentity": {"frameSequence": 1},
            },
        })
    }

    fn rich_dataset(samples: Vec<Value>) -> Value {
        json!({
            "kind": CALIBRATION_DATASET_KIND,
            "board": null,
            "samples": samples,
            "count": samples.len(),
        })
    }

    fn collector() -> DatasetCollectorNode {
        let mut spec = node_spec(
            "datasetCollector",
            vec!["frames", "image", "detection", "score"],
            vec!["dataset", "preview"],
        );
        spec.config = json!({"maxSamples": 4});
        DatasetCollectorNode {
            spec,
            samples: Vec::new(),
            pending_images: Vec::new(),
            pending_scores: Vec::new(),
            cached_images: Vec::new(),
            selected_sample_id: None,
            next_sample_id: 1,
        }
    }

    fn video_frame(frame_identity: ImageFrameIdentity) -> DataPacket {
        DataPacket::VideoFrame(Arc::new(DecodedVideoFrame {
            width: 2,
            height: 2,
            rgba: Arc::from([0_u8; 16]),
            identity: frame_identity
                .stream_identity()
                .expect("stream-backed frame identity"),
        }))
    }

    fn detection_packet(
        detection: ChessboardDetection,
        frame_identity: ImageFrameIdentity,
    ) -> DataPacket {
        DataPacket::Detection(Arc::new(crate::engine::DetectionPacket {
            detection: Arc::new(detection),
            frame_identity,
        }))
    }

    fn last_dataset(recorded: &Arc<Mutex<Vec<DataPacket>>>) -> Value {
        let recorded = recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match recorded.last().expect("emitted packet") {
            DataPacket::Dataset(dataset) => dataset.as_ref().clone(),
            packet => panic!("expected dataset, got {packet:?}"),
        }
    }

    fn last_coverage(recorded: &Arc<Mutex<Vec<DataPacket>>>) -> Value {
        let recorded = recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match recorded.last().expect("emitted packet") {
            DataPacket::Coverage(coverage) => coverage.as_ref().clone(),
            packet => panic!("expected coverage, got {packet:?}"),
        }
    }

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
        let coverage = grid_coverage(
            &[
                detection(100, 100, 10.0, 10.0),
                detection(100, 100, 90.0, 10.0),
            ],
            2,
            2,
        )
        .expect("coverage");
        assert_eq!(coverage["sampleCount"], 2);
        assert_eq!(coverage["occupiedCells"], 2);
        assert_eq!(coverage["totalCells"], 4);
        assert_eq!(coverage["coverageRatio"], 0.5);
        assert_eq!(first_missing_cell(&coverage), Some((0, 1)));
    }

    #[test]
    fn rich_dataset_parser_requires_complete_sample_metadata() {
        let payload = rich_dataset(vec![rich_sample(
            "sample-1",
            detection(100, 100, 10.0, 10.0),
            true,
            true,
        )]);
        let parsed = parse_calibration_dataset(&payload).expect("rich dataset parses");
        assert_eq!(parsed.count, 1);
        assert_eq!(
            parsed.samples[0].image_ref.reference,
            "runtime://frame/sample-1"
        );
        assert!(parsed.samples[0].acceptance.accepted);
        assert!(parsed.samples[0].acceptance.enabled);

        let legacy = json!({
            "kind": CALIBRATION_DATASET_KIND,
            "samples": [detection(100, 100, 10.0, 10.0)],
            "count": 1,
        });
        assert!(parse_calibration_dataset(&legacy).is_err());

        let mut mismatched_score = payload.clone();
        mismatched_score["samples"][0]["score"]["frameSequence"] = json!(2);
        assert!(parse_calibration_dataset(&mismatched_score).is_err());

        let mut wrong_count = payload;
        wrong_count["count"] = json!(2);
        assert!(parse_calibration_dataset(&wrong_count).is_err());
    }

    #[test]
    fn collector_emits_sample_list_with_metadata_score_and_acceptance() {
        let frame_identity = identity(7);
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime_with_record(Arc::clone(&recorded));
        let mut node = collector();
        let image = ImageFrame::rgba8(2, 2, Arc::from(vec![0_u8; 16]), frame_identity.clone())
            .expect("image");
        node.on_input("image", DataPacket::ImageFrame(Arc::new(image)), &mut rt)
            .expect("image input");
        node.on_input(
            "score",
            DataPacket::Score(Arc::new(CalibrationFrameScore {
                score: 0.75,
                frame_identity: frame_identity.clone(),
            })),
            &mut rt,
        )
        .expect("score input");
        node.on_input(
            "detection",
            detection_packet(detection(2, 2, 1.0, 1.0), frame_identity),
            &mut rt,
        )
        .expect("detection input");
        node.on_action(NodeAction::Trigger, &mut rt)
            .expect("emit dataset");

        let dataset = last_dataset(&recorded);
        let sample = &dataset["samples"][0];
        assert_eq!(dataset["kind"], CALIBRATION_DATASET_KIND);
        assert_eq!(dataset["count"], 1);
        assert_eq!(sample["id"], "sample-1");
        assert_eq!(sample["imageRef"]["width"], 2);
        assert_eq!(sample["imageRef"]["height"], 2);
        assert_eq!(sample["imageRef"]["format"], "RGBA8");
        assert!(sample["imageRef"].get("bytes").is_none());
        assert!(sample["imageRef"].get("planes").is_none());
        assert_eq!(sample["score"]["score"], 0.75);
        assert_eq!(
            sample["acceptance"],
            json!({"accepted": true, "enabled": true})
        );
        assert_eq!(sample["provenance"]["frameIdentity"]["frameSequence"], 7);
        assert!(parse_calibration_dataset(&dataset).is_ok());
    }

    #[test]
    fn collector_auto_emits_dataset_when_detection_arrives() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime_with_record(Arc::clone(&recorded));
        let mut node = collector();

        node.on_input(
            "detection",
            detection_packet(detection(2, 2, 1.0, 1.0), identity(7)),
            &mut rt,
        )
        .expect("detection input emits dataset");

        let dataset = last_dataset(&recorded);
        assert_eq!(dataset["kind"], CALIBRATION_DATASET_KIND);
        assert_eq!(dataset["count"], 1);
        assert_eq!(dataset["samples"][0]["id"], "sample-1");
        assert_eq!(dataset["samples"][0]["score"], Value::Null);
        assert_eq!(
            dataset["samples"][0]["provenance"]["frameIdentity"]["frameSequence"],
            7
        );
    }

    #[test]
    fn collector_never_joins_image_or_score_by_time() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime_with_record(Arc::clone(&recorded));
        let mut node = collector();
        let unrelated_identity = identity(8);
        let image = ImageFrame::rgba8(2, 2, Arc::from(vec![0_u8; 16]), unrelated_identity.clone())
            .expect("image");
        node.on_input("image", DataPacket::ImageFrame(Arc::new(image)), &mut rt)
            .expect("image input");
        node.on_input(
            "score",
            DataPacket::Score(Arc::new(CalibrationFrameScore {
                score: 0.75,
                frame_identity: unrelated_identity,
            })),
            &mut rt,
        )
        .expect("score input");
        node.on_input(
            "detection",
            detection_packet(detection(2, 2, 1.0, 1.0), identity(7)),
            &mut rt,
        )
        .expect("detection input");
        node.on_action(NodeAction::Trigger, &mut rt)
            .expect("emit dataset");

        let sample = &last_dataset(&recorded)["samples"][0];
        assert!(sample["score"].is_null());
        assert!(sample["imageRef"]["format"].is_null());
        assert_eq!(sample["provenance"]["frameIdentity"]["frameSequence"], 7);
    }

    #[test]
    fn collector_sample_actions_update_acceptance_and_reject_missing_payload() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime_with_record(Arc::clone(&recorded));
        let mut node = collector();
        node.on_input(
            "detection",
            detection_packet(detection(2, 2, 1.0, 1.0), identity(7)),
            &mut rt,
        )
        .expect("detection input");
        node.on_action(
            NodeAction::Custom {
                name: "reject".to_owned(),
                payload: json!({"sampleId": "sample-1"}),
            },
            &mut rt,
        )
        .expect("reject sample");
        let sample = &last_dataset(&recorded)["samples"][0];
        assert_eq!(
            sample["acceptance"],
            json!({"accepted": false, "enabled": true})
        );

        node.on_action(
            NodeAction::Custom {
                name: "disable".to_owned(),
                payload: json!({"sampleId": "sample-1"}),
            },
            &mut rt,
        )
        .expect("disable sample");
        let sample = &last_dataset(&recorded)["samples"][0];
        assert_eq!(
            sample["acceptance"],
            json!({"accepted": false, "enabled": false})
        );

        let error = node
            .on_action(
                NodeAction::Custom {
                    name: "accept".to_owned(),
                    payload: Value::Null,
                },
                &mut rt,
            )
            .expect_err("missing sample id must be rejected");
        assert!(matches!(error, NodeError::Precondition(_)));
    }

    #[test]
    fn collector_records_video_frame_for_exact_identity_preview() {
        let frame_identity = identity(7);
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime_with_record(Arc::clone(&recorded));
        let mut node = collector();

        node.on_input("frames", video_frame(frame_identity.clone()), &mut rt)
            .expect("video frame input");
        node.on_input(
            "detection",
            detection_packet(detection(2, 2, 1.0, 1.0), frame_identity),
            &mut rt,
        )
        .expect("detection input");
        recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        node.on_action(
            NodeAction::Custom {
                name: "select".to_owned(),
                payload: json!({"sampleId": "sample-1"}),
            },
            &mut rt,
        )
        .expect("select cached video frame");

        let recorded = recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(matches!(recorded.first(), Some(DataPacket::ImageFrame(_))));
    }

    #[test]
    fn collector_select_emits_exact_cached_image_before_dataset_snapshot() {
        let frame_identity = identity(7);
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime_with_record(Arc::clone(&recorded));
        let mut node = collector();
        let image = Arc::new(
            ImageFrame::rgba8(2, 2, Arc::from(vec![0_u8; 16]), frame_identity.clone())
                .expect("image"),
        );
        node.on_input("image", DataPacket::ImageFrame(Arc::clone(&image)), &mut rt)
            .expect("image input");
        node.on_input(
            "detection",
            detection_packet(detection(2, 2, 1.0, 1.0), frame_identity),
            &mut rt,
        )
        .expect("detection input");
        recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        node.on_action(
            NodeAction::Custom {
                name: "select".to_owned(),
                payload: json!({"sampleId": "sample-1"}),
            },
            &mut rt,
        )
        .expect("select sample");

        let recorded = recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(recorded.len(), 2);
        let DataPacket::ImageFrame(preview) = &recorded[0] else {
            panic!("select must emit cached image preview first");
        };
        assert_eq!(preview.as_ref(), image.as_ref());
        let DataPacket::Dataset(dataset) = &recorded[1] else {
            panic!("select must leave dataset as the latest runtime output");
        };
        assert_eq!(dataset["selectedSampleId"], "sample-1");
    }

    #[test]
    fn collector_select_rejects_nearby_but_nonidentical_image() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime_with_record(Arc::clone(&recorded));
        let mut node = collector();
        let image = ImageFrame::rgba8(2, 2, Arc::from(vec![0_u8; 16]), identity(8))
            .expect("unrelated image");
        node.on_input("image", DataPacket::ImageFrame(Arc::new(image)), &mut rt)
            .expect("image input");
        node.on_input(
            "detection",
            detection_packet(detection(2, 2, 1.0, 1.0), identity(7)),
            &mut rt,
        )
        .expect("detection input");
        recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        let error = node
            .on_action(
                NodeAction::Custom {
                    name: "select".to_owned(),
                    payload: json!({"sampleId": "sample-1"}),
                },
                &mut rt,
            )
            .expect_err("select must reject a sample without an exact cached image");

        assert!(matches!(
            error,
            NodeError::Precondition(message) if message.contains("no exact cached image preview")
        ));
        assert!(node.selected_sample_id.is_none());
        assert!(
            recorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn collector_bounds_preview_cache_by_sample_capacity() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime_with_record(recorded);
        let mut node = collector();
        node.on_config_update(json!({"maxSamples": 1}), &mut rt)
            .expect("reduce cache capacity");
        for sequence in 1..=2 {
            let image = ImageFrame::rgba8(2, 2, Arc::from(vec![0_u8; 16]), identity(sequence))
                .expect("image");
            node.on_input("image", DataPacket::ImageFrame(Arc::new(image)), &mut rt)
                .expect("image input");
        }
        assert_eq!(node.cached_images.len(), 1);
        assert_eq!(node.cached_images[0].0, identity(2));
    }

    #[test]
    fn coverage_only_uses_accepted_and_enabled_samples() {
        let dataset = rich_dataset(vec![
            rich_sample("active", detection(100, 100, 10.0, 10.0), true, true),
            rich_sample("rejected", detection(100, 100, 90.0, 10.0), false, true),
            rich_sample("disabled", detection(100, 100, 90.0, 90.0), true, false),
        ]);
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime_with_record(Arc::clone(&recorded));
        let mut node = CoverageAnalyzerNode {
            spec: node_spec("coverageAnalyzer", vec!["dataset"], vec!["coverage"]),
        };
        node.on_input("dataset", DataPacket::Dataset(Arc::new(dataset)), &mut rt)
            .expect("coverage input");

        let coverage = last_coverage(&recorded);
        assert_eq!(coverage["sampleCount"], 1);
        assert_eq!(coverage["occupiedCells"], 1);
    }

    #[test]
    fn config_update_applies_dataset_capacity_and_coverage_grid() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut rt = runtime_with_record(Arc::clone(&recorded));
        let mut collector = collector();
        for sequence in 1..=3 {
            collector
                .on_input(
                    "detection",
                    detection_packet(detection(2, 2, 1.0, 1.0), identity(sequence)),
                    &mut rt,
                )
                .expect("record sample");
        }
        collector
            .on_config_update(json!({"maxSamples": 2}), &mut rt)
            .expect("update collector config");
        collector
            .on_action(NodeAction::Trigger, &mut rt)
            .expect("emit trimmed dataset");
        assert_eq!(last_dataset(&recorded)["count"], 2);

        let mut coverage = CoverageAnalyzerNode {
            spec: node_spec("coverageAnalyzer", vec!["dataset"], vec!["coverage"]),
        };
        coverage
            .on_config_update(json!({"gridCols": 3, "gridRows": 2}), &mut rt)
            .expect("update coverage grid");
        coverage
            .on_input(
                "dataset",
                DataPacket::Dataset(Arc::new(rich_dataset(vec![rich_sample(
                    "active",
                    detection(100, 100, 10.0, 10.0),
                    true,
                    true,
                )]))),
                &mut rt,
            )
            .expect("coverage input");
        assert_eq!(last_coverage(&recorded)["totalCells"], 6);
    }
}
