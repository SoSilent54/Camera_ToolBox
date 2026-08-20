//! 自动标定采集逻辑节点：检测评分、阈值判断、连续保持和请求构造。
//!
//! 每个节点只承担一个确定性职责；帧身份从检测结果一直传到最终的
//! [`CaptureRequest::source_identity`]，不创建时间对齐或重建来源身份。

use std::sync::Arc;

use crate::engine::{
    CalibrationFrameScore, CaptureMode, CaptureRequest, CaptureSignal, CaptureTarget,
    CaptureTrigger, DataPacket, FrameProvenance, ImageFrameIdentity, NodeAction, NodeError,
    NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec,
};

/// 将棋盘角点完整度映射为归一化标定帧评分。
pub struct CalibrationFrameScorerFactory;

impl NodeFactory for CalibrationFrameScorerFactory {
    fn kind(&self) -> &'static str {
        "calibrationFrameScorer"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(CalibrationFrameScorerNode { spec }))
    }
}

pub struct CalibrationFrameScorerNode {
    spec: NodeSpec,
}

impl CalibrationFrameScorerNode {
    /// 返回棋盘规格要求的内角点数，避免零除或无意义的评分。
    fn expected_corners(&self) -> Result<usize, NodeError> {
        let expected = config_u64(&self.spec, "expectedCorners", 88)?;
        usize::try_from(expected)
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                NodeError::Config(
                    "calibrationFrameScorer.expectedCorners must be a positive usize".to_owned(),
                )
            })
    }
}

impl NodeInstance for CalibrationFrameScorerNode {
    fn kind(&self) -> &'static str {
        "calibrationFrameScorer"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for detections");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if port != "detection" {
            return Ok(());
        }
        let DataPacket::Detection(detection) = packet else {
            return Err(NodeError::Precondition(
                "calibrationFrameScorer.detection requires calib.detection".to_owned(),
            ));
        };
        let expected = self.expected_corners()?;
        let score = (detection.detection.corners.len() as f64 / expected as f64).min(1.0);
        rt.emit(
            "score",
            DataPacket::Score(Arc::new(CalibrationFrameScore {
                score,
                frame_identity: detection.frame_identity.clone(),
            })),
        )?;
        rt.report_event(format!("calibration frame scored {score:.3}"));
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
        probe.expected_corners()?;
        self.spec = next;
        Ok(())
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 将评分转换为带明确接受状态的通用阈值信号。
pub struct ScoreThresholdGateFactory;

impl NodeFactory for ScoreThresholdGateFactory {
    fn kind(&self) -> &'static str {
        "scoreThresholdGate"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(ScoreThresholdGateNode { spec }))
    }
}

pub struct ScoreThresholdGateNode {
    spec: NodeSpec,
}

impl ScoreThresholdGateNode {
    fn threshold(&self) -> Result<f64, NodeError> {
        let threshold = config_f64(&self.spec, "threshold", 0.4)?;
        if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
            return Err(NodeError::Config(
                "scoreThresholdGate.threshold must be finite and within [0, 1]".to_owned(),
            ));
        }
        Ok(threshold)
    }
}

impl NodeInstance for ScoreThresholdGateNode {
    fn kind(&self) -> &'static str {
        "scoreThresholdGate"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for frame scores");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if port != "score" {
            return Ok(());
        }
        let DataPacket::Score(score) = packet else {
            return Err(NodeError::Precondition(
                "scoreThresholdGate.score requires capture.score".to_owned(),
            ));
        };
        if !score.score.is_finite() || !(0.0..=1.0).contains(&score.score) {
            return Err(NodeError::Precondition(
                "scoreThresholdGate received a score outside [0, 1]".to_owned(),
            ));
        }
        let threshold = self.threshold()?;
        let accepted = score.score >= threshold;
        rt.emit(
            "signal",
            DataPacket::CaptureSignal(Arc::new(CaptureSignal {
                accepted,
                frame_identity: score.frame_identity.clone(),
            })),
        )?;
        rt.report_event(format!(
            "frame score {:.3} {} threshold {:.3}",
            score.score,
            if accepted { "meets" } else { "below" },
            threshold
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
        probe.threshold()?;
        self.spec = next;
        Ok(())
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 对不同来源帧的已接受信号做连续保持，达到数量后产生一次抓帧触发。
pub struct ConsecutiveHoldGateFactory;

impl NodeFactory for ConsecutiveHoldGateFactory {
    fn kind(&self) -> &'static str {
        "consecutiveHoldGate"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(ConsecutiveHoldGateNode {
            spec,
            consecutive_count: 0,
            last_identity: None,
        }))
    }
}

pub struct ConsecutiveHoldGateNode {
    spec: NodeSpec,
    consecutive_count: u8,
    last_identity: Option<ImageFrameIdentity>,
}

impl ConsecutiveHoldGateNode {
    fn hold_count(&self) -> Result<u8, NodeError> {
        let hold_count = config_u64(&self.spec, "holdCount", 3)?;
        let hold_count = u8::try_from(hold_count).map_err(|_| {
            NodeError::Config("consecutiveHoldGate.holdCount must be within 1..=30".to_owned())
        })?;
        if !(1..=30).contains(&hold_count) {
            return Err(NodeError::Config(
                "consecutiveHoldGate.holdCount must be within 1..=30".to_owned(),
            ));
        }
        Ok(hold_count)
    }
}

impl NodeInstance for ConsecutiveHoldGateNode {
    fn kind(&self) -> &'static str {
        "consecutiveHoldGate"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(
            NodeRuntimeState::Ready,
            "waiting for accepted frame signals",
        );
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if port != "signal" {
            return Ok(());
        }
        let DataPacket::CaptureSignal(signal) = packet else {
            return Err(NodeError::Precondition(
                "consecutiveHoldGate.signal requires capture.signal".to_owned(),
            ));
        };
        if self.last_identity.as_ref() == Some(&signal.frame_identity) {
            rt.report_event("ignored duplicate signal for the same source frame");
            return Ok(());
        }
        self.last_identity = Some(signal.frame_identity.clone());
        if !signal.accepted {
            self.consecutive_count = 0;
            rt.report_event("rejected signal reset consecutive hold");
            return Ok(());
        }

        let hold_count = self.hold_count()?;
        self.consecutive_count = self.consecutive_count.saturating_add(1);
        if self.consecutive_count < hold_count {
            rt.report_event(format!(
                "consecutive hold {}/{}",
                self.consecutive_count, hold_count
            ));
            return Ok(());
        }

        rt.emit(
            "trigger",
            DataPacket::CaptureTrigger(Arc::new(CaptureTrigger {
                frame_identity: signal.frame_identity.clone(),
            })),
        )?;
        self.consecutive_count = 0;
        rt.report_event("capture trigger emitted after consecutive hold");
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
        let probe = Self {
            spec: next.clone(),
            consecutive_count: 0,
            last_identity: None,
        };
        probe.hold_count()?;
        self.spec = next;
        self.consecutive_count = 0;
        self.last_identity = None;
        Ok(())
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.consecutive_count = 0;
        self.last_identity = None;
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

/// 基于触发帧身份和设备配置构造精确的 `CaptureRequest`。
pub struct CaptureRequestBuilderFactory;

impl NodeFactory for CaptureRequestBuilderFactory {
    fn kind(&self) -> &'static str {
        "captureRequestBuilder"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(CaptureRequestBuilderNode { spec }))
    }
}

pub struct CaptureRequestBuilderNode {
    spec: NodeSpec,
}

impl CaptureRequestBuilderNode {
    fn capture_target(&self) -> Result<CaptureTarget, NodeError> {
        match config_string(&self.spec, "target", "yuv").as_str() {
            "yuv" => {
                config_u16(&self.spec, "channel", 0).map(|channel| CaptureTarget::Yuv { channel })
            }
            "raw" => {
                config_u16(&self.spec, "camera", 0).map(|camera| CaptureTarget::Raw { camera })
            }
            value => Err(NodeError::Config(format!(
                "captureRequestBuilder.target `{value}` is unsupported; expected yuv or raw"
            ))),
        }
    }

    fn capture_mode(&self, identity: &ImageFrameIdentity) -> Result<CaptureMode, NodeError> {
        let mode = config_string(&self.spec, "mode", "latest");
        match mode.as_str() {
            "latest" => Ok(CaptureMode::Latest),
            "frame_id" => Ok(CaptureMode::FrameId(identity.frame_sequence)),
            "timestamp_ns" => identity.device_timestamp_ns().map_or_else(
                || {
                    Err(NodeError::Precondition(
                        "captureRequestBuilder timestamp_ns mode requires device timestamp_ns metadata"
                            .to_owned(),
                    ))
                },
                |timestamp_ns| Ok(CaptureMode::TimestampNs(timestamp_ns)),
            ),
            value => Err(NodeError::Config(format!(
                "captureRequestBuilder.mode `{value}` is unsupported; expected latest, frame_id, or timestamp_ns"
            ))),
        }
    }

    fn validate_config(&self) -> Result<(), NodeError> {
        self.capture_target()?;
        match config_string(&self.spec, "mode", "latest").as_str() {
            "latest" | "frame_id" | "timestamp_ns" => Ok(()),
            value => Err(NodeError::Config(format!(
                "captureRequestBuilder.mode `{value}` is unsupported; expected latest, frame_id, or timestamp_ns"
            ))),
        }
    }

    fn validate_source(identity: &ImageFrameIdentity) -> Result<(), NodeError> {
        match &identity.provenance {
            FrameProvenance::Stream { .. } | FrameProvenance::Device { .. } => Ok(()),
            FrameProvenance::File { .. } => Err(NodeError::Precondition(
                "captureRequestBuilder cannot request a new capture from a file-backed frame"
                    .to_owned(),
            )),
            FrameProvenance::Unknown { .. } => Err(NodeError::Precondition(
                "captureRequestBuilder requires a source frame identity".to_owned(),
            )),
        }
    }
}

impl NodeInstance for CaptureRequestBuilderNode {
    fn kind(&self) -> &'static str {
        "captureRequestBuilder"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for capture triggers");
        Ok(())
    }

    fn on_input(
        &mut self,
        port: &str,
        packet: DataPacket,
        rt: &mut NodeRuntime,
    ) -> Result<(), NodeError> {
        if port != "trigger" {
            return Ok(());
        }
        let DataPacket::CaptureTrigger(trigger) = packet else {
            return Err(NodeError::Precondition(
                "captureRequestBuilder.trigger requires capture.trigger".to_owned(),
            ));
        };
        Self::validate_source(&trigger.frame_identity)?;
        let request = CaptureRequest {
            target: self.capture_target()?,
            mode: self.capture_mode(&trigger.frame_identity)?,
            source_identity: Some(trigger.frame_identity.clone()),
        };
        rt.emit("capture", DataPacket::CaptureRequest(Arc::new(request)))?;
        rt.report_event("capture request constructed from trigger identity");
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
        probe.validate_config()?;
        self.spec = next;
        Ok(())
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

fn config_string(spec: &NodeSpec, key: &str, fallback: &str) -> String {
    spec.config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or(fallback)
        .to_owned()
}

fn config_u64(spec: &NodeSpec, key: &str, fallback: u64) -> Result<u64, NodeError> {
    match spec.config.get(key) {
        None => Ok(fallback),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| NodeError::Config(format!("{key} must be an unsigned integer"))),
    }
}

fn config_u16(spec: &NodeSpec, key: &str, fallback: u16) -> Result<u16, NodeError> {
    let value = config_u64(spec, key, u64::from(fallback))?;
    u16::try_from(value).map_err(|_| NodeError::Config(format!("{key} must be a valid u16")))
}

fn config_f64(spec: &NodeSpec, key: &str, fallback: f64) -> Result<f64, NodeError> {
    match spec.config.get(key) {
        None => Ok(fallback),
        Some(value) => value
            .as_f64()
            .ok_or_else(|| NodeError::Config(format!("{key} must be a number"))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{atomic::AtomicBool, mpsc, Arc, Mutex};

    use camera_toolbox_core::{CalibrationImageSize, CalibrationPoint, ChessboardDetection};

    use super::*;
    use crate::{
        engine::{
            DetectionPacket, NodeReporter, OutputRegistry, PortCardinality, PortSpec, SpawnContext,
        },
        platform::{SourcePts, SourcePtsProvenance, StreamFrameIdentity, StreamSessionId},
    };

    fn stream_identity(sequence: u64) -> ImageFrameIdentity {
        ImageFrameIdentity::from(&StreamFrameIdentity::known_at_with_device_timestamp(
            StreamSessionId::new("logic-test").expect("valid stream id"),
            0,
            sequence,
            SourcePts::Known {
                ticks: sequence as i64 * 3_000,
                time_base_numerator: 1,
                time_base_denominator: 90_000,
                provenance: SourcePtsProvenance::FfmpegDecodedFrame,
            },
            sequence * 10,
            Some(sequence * 1_000),
        ))
    }

    fn detection(sequence: u64, corners: usize) -> DataPacket {
        DataPacket::Detection(Arc::new(DetectionPacket {
            detection: Arc::new(ChessboardDetection {
                image_size: CalibrationImageSize::new(640, 480).expect("valid size"),
                corners: (0..corners)
                    .map(|index| CalibrationPoint::new(index as f32, 0.0))
                    .collect(),
            }),
            frame_identity: stream_identity(sequence),
        }))
    }

    fn score(sequence: u64, value: f64) -> DataPacket {
        DataPacket::Score(Arc::new(CalibrationFrameScore {
            score: value,
            frame_identity: stream_identity(sequence),
        }))
    }

    fn signal(sequence: u64, accepted: bool) -> DataPacket {
        DataPacket::CaptureSignal(Arc::new(CaptureSignal {
            accepted,
            frame_identity: stream_identity(sequence),
        }))
    }

    fn trigger(sequence: u64) -> DataPacket {
        DataPacket::CaptureTrigger(Arc::new(CaptureTrigger {
            frame_identity: stream_identity(sequence),
        }))
    }

    fn runtime(record: Arc<Mutex<Vec<DataPacket>>>) -> NodeRuntime {
        let (status_tx, _status_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::channel();
        let reporter = NodeReporter::new("logic-test".to_owned(), status_tx, event_tx);
        let mut outputs = OutputRegistry::default();
        outputs.set_record(Arc::new(move |packet| {
            record.lock().expect("record lock").push(packet)
        }));
        NodeRuntime::new(SpawnContext {
            outputs,
            reporter,
            services: Arc::new(crate::engine::EngineServices::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            viewer_slot: None,
        })
    }

    fn node_spec(
        kind: &str,
        input: (&str, &str),
        output: (&str, &str),
        config: serde_json::Value,
    ) -> NodeSpec {
        NodeSpec {
            id: kind.to_owned(),
            kind: kind.to_owned(),
            title: kind.to_owned(),
            inputs: vec![PortSpec {
                id: input.0.to_owned(),
                label: input.0.to_owned(),
                kind: input.1.to_owned(),
                cardinality: PortCardinality::One,
                required: true,
            }],
            outputs: vec![PortSpec {
                id: output.0.to_owned(),
                label: output.0.to_owned(),
                kind: output.1.to_owned(),
                cardinality: PortCardinality::One,
                required: true,
            }],
            config,
        }
    }

    fn scorer_spec() -> NodeSpec {
        node_spec(
            "calibrationFrameScorer",
            ("detection", "calib.detection"),
            ("score", "capture.score"),
            serde_json::json!({"expectedCorners": 4}),
        )
    }

    fn threshold_spec(config: serde_json::Value) -> NodeSpec {
        node_spec(
            "scoreThresholdGate",
            ("score", "capture.score"),
            ("signal", "capture.signal"),
            config,
        )
    }

    fn hold_spec(config: serde_json::Value) -> NodeSpec {
        node_spec(
            "consecutiveHoldGate",
            ("signal", "capture.signal"),
            ("trigger", "capture.trigger"),
            config,
        )
    }

    fn builder_spec(config: serde_json::Value) -> NodeSpec {
        node_spec(
            "captureRequestBuilder",
            ("trigger", "capture.trigger"),
            ("capture", "command.capture.request.v1"),
            config,
        )
    }

    #[test]
    fn calibration_frame_scorer_preserves_detection_frame_identity() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&record));
        let mut node = CalibrationFrameScorerFactory
            .instantiate(scorer_spec())
            .expect("instantiate");

        node.on_input("detection", detection(7, 3), &mut runtime)
            .expect("score detection");

        let output = record
            .lock()
            .expect("record lock")
            .pop()
            .expect("score output");
        let DataPacket::Score(score) = output else {
            panic!("expected score output");
        };
        assert_eq!(score.score, 0.75);
        assert_eq!(score.frame_identity, stream_identity(7));
    }

    #[test]
    fn score_threshold_gate_emits_accepted_and_rejected_signals() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&record));
        let mut node = ScoreThresholdGateFactory
            .instantiate(threshold_spec(serde_json::json!({"threshold": 0.5})))
            .expect("instantiate");

        node.on_input("score", score(1, 0.5), &mut runtime)
            .expect("accepted score");
        node.on_input("score", score(2, 0.49), &mut runtime)
            .expect("rejected score");

        let outputs = record.lock().expect("record lock");
        assert_eq!(outputs.len(), 2);
        let DataPacket::CaptureSignal(first) = &outputs[0] else {
            panic!("expected accepted signal");
        };
        let DataPacket::CaptureSignal(second) = &outputs[1] else {
            panic!("expected rejected signal");
        };
        assert_eq!(
            (first.accepted, &first.frame_identity),
            (true, &stream_identity(1))
        );
        assert_eq!(
            (second.accepted, &second.frame_identity),
            (false, &stream_identity(2))
        );
    }

    #[test]
    fn consecutive_hold_gate_requires_distinct_accepted_signals() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&record));
        let mut node = ConsecutiveHoldGateFactory
            .instantiate(hold_spec(serde_json::json!({"holdCount": 2})))
            .expect("instantiate");

        node.on_input("signal", signal(1, true), &mut runtime)
            .expect("first accepted signal");
        node.on_input("signal", signal(1, true), &mut runtime)
            .expect("duplicate signal");
        node.on_input("signal", signal(2, true), &mut runtime)
            .expect("second accepted signal");
        node.on_input("signal", signal(3, true), &mut runtime)
            .expect("next accepted signal");
        node.on_input("signal", signal(4, false), &mut runtime)
            .expect("rejected signal");
        node.on_input("signal", signal(5, true), &mut runtime)
            .expect("accepted signal after reset");

        let outputs = record.lock().expect("record lock");
        assert_eq!(outputs.len(), 1);
        let DataPacket::CaptureTrigger(trigger) = &outputs[0] else {
            panic!("expected capture trigger");
        };
        assert_eq!(trigger.frame_identity, stream_identity(2));
    }

    #[test]
    fn capture_request_builder_preserves_identity_and_validates_config() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&record));
        let mut node = CaptureRequestBuilderFactory
            .instantiate(builder_spec(serde_json::json!({
                "target": "yuv",
                "channel": 3,
                "mode": "timestamp_ns"
            })))
            .expect("instantiate");

        node.on_input("trigger", trigger(2), &mut runtime)
            .expect("build request");
        let outputs = record.lock().expect("record lock");
        assert_eq!(outputs.len(), 1);
        let DataPacket::CaptureRequest(request) = &outputs[0] else {
            panic!("expected capture request");
        };
        assert_eq!(request.target, CaptureTarget::Yuv { channel: 3 });
        assert_eq!(request.mode, CaptureMode::TimestampNs(2_000));
        assert_eq!(request.source_identity.as_ref(), Some(&stream_identity(2)));
        drop(outputs);

        let mut invalid_target = CaptureRequestBuilderFactory
            .instantiate(builder_spec(serde_json::json!({"target": "rgb"})))
            .expect("instantiate");
        assert!(matches!(
            invalid_target.on_input("trigger", trigger(1), &mut runtime),
            Err(NodeError::Config(_))
        ));

        let mut invalid_mode = CaptureRequestBuilderFactory
            .instantiate(builder_spec(serde_json::json!({"mode": "unknown"})))
            .expect("instantiate");
        assert!(matches!(
            invalid_mode.on_input("trigger", trigger(1), &mut runtime),
            Err(NodeError::Config(_))
        ));

        let unknown_identity = ImageFrameIdentity {
            provenance: FrameProvenance::Unknown {
                reason: "test".to_owned(),
            },
            frame_sequence: 0,
            source_pts: SourcePts::Unavailable {
                reason: "test".to_owned(),
            },
            host_monotonic_time_ns: 0,
            device_timestamp_ns: None,
        };
        let unknown_trigger = DataPacket::CaptureTrigger(Arc::new(CaptureTrigger {
            frame_identity: unknown_identity,
        }));
        assert!(matches!(
            node.on_input("trigger", unknown_trigger, &mut runtime),
            Err(NodeError::Precondition(_))
        ));
    }

    #[test]
    fn capture_request_builder_rejects_timestamp_mode_without_device_metadata() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(record);
        let mut node = CaptureRequestBuilderFactory
            .instantiate(builder_spec(serde_json::json!({"mode": "timestamp_ns"})))
            .expect("instantiate");
        let identity = ImageFrameIdentity::from(&StreamFrameIdentity::known_at(
            StreamSessionId::new("logic-test-without-device-timestamp").expect("valid stream id"),
            0,
            1,
            SourcePts::Unavailable {
                reason: "test".to_owned(),
            },
            123,
        ));
        let trigger = DataPacket::CaptureTrigger(Arc::new(CaptureTrigger {
            frame_identity: identity,
        }));

        assert!(matches!(
            node.on_input("trigger", trigger, &mut runtime),
            Err(NodeError::Precondition(_))
        ));
    }
    #[test]
    fn capture_request_builder_validates_only_the_selected_target_config() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&record));
        let mut node = CaptureRequestBuilderFactory
            .instantiate(builder_spec(serde_json::json!({
                "target": "raw",
                "camera": 1,
                "channel": 65_536
            })))
            .expect("instantiate");

        node.on_input("trigger", trigger(1), &mut runtime)
            .expect("build raw request");
        let output = record
            .lock()
            .expect("record lock")
            .pop()
            .expect("raw capture request");
        let DataPacket::CaptureRequest(request) = output else {
            panic!("expected capture request");
        };
        assert_eq!(request.target, CaptureTarget::Raw { camera: 1 });
        assert_eq!(request.mode, CaptureMode::Latest);
    }

    #[test]
    fn config_update_changes_spec_backed_runtime_behavior() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&record));

        let mut scorer = CalibrationFrameScorerFactory
            .instantiate(scorer_spec())
            .expect("instantiate scorer");
        scorer
            .on_config_update(serde_json::json!({"expectedCorners": 2}), &mut runtime)
            .expect("update scorer config");
        scorer
            .on_input("detection", detection(10, 2), &mut runtime)
            .expect("score after config update");
        let DataPacket::Score(updated_score) = record.lock().expect("record lock").pop().unwrap()
        else {
            panic!("expected score");
        };
        assert_eq!(updated_score.score, 1.0);

        let mut threshold = ScoreThresholdGateFactory
            .instantiate(threshold_spec(serde_json::json!({"threshold": 0.9})))
            .expect("instantiate threshold");
        threshold
            .on_config_update(serde_json::json!({"threshold": 0.4}), &mut runtime)
            .expect("update threshold config");
        threshold
            .on_input("score", score(11, 0.5), &mut runtime)
            .expect("threshold after config update");
        let DataPacket::CaptureSignal(updated_signal) =
            record.lock().expect("record lock").pop().unwrap()
        else {
            panic!("expected signal");
        };
        assert!(updated_signal.accepted);

        let mut hold = ConsecutiveHoldGateFactory
            .instantiate(hold_spec(serde_json::json!({"holdCount": 3})))
            .expect("instantiate hold");
        hold.on_input("signal", signal(12, true), &mut runtime)
            .expect("first signal");
        hold.on_config_update(serde_json::json!({"holdCount": 1}), &mut runtime)
            .expect("update hold config");
        hold.on_input("signal", signal(13, true), &mut runtime)
            .expect("signal after hold update");
        assert!(matches!(
            record.lock().expect("record lock").pop().unwrap(),
            DataPacket::CaptureTrigger(_)
        ));

        let mut builder = CaptureRequestBuilderFactory
            .instantiate(builder_spec(
                serde_json::json!({"target": "yuv", "channel": 0}),
            ))
            .expect("instantiate builder");
        builder
            .on_config_update(
                serde_json::json!({"target": "raw", "camera": 2}),
                &mut runtime,
            )
            .expect("update builder config");
        builder
            .on_input("trigger", trigger(14), &mut runtime)
            .expect("builder after config update");
        let DataPacket::CaptureRequest(request) =
            record.lock().expect("record lock").pop().unwrap()
        else {
            panic!("expected request");
        };
        assert_eq!(request.target, CaptureTarget::Raw { camera: 2 });
    }
}
