//! 最小确定性采集逻辑节点：`Detection -> GainScore -> CaptureRequest`。
//!
//! 节点只在收到数据包时同步计算，不引入计时、后台循环或隐藏状态机。帧身份从检测
//! 结果复制到评分，再作为 `CaptureRequest::source_identity` 原样传递给采集适配器。

use std::sync::Arc;

use crate::{
    engine::{
        CaptureMode, CaptureRequest, CaptureTarget, DataPacket, FrameProvenance, GainScore,
        NodeAction, NodeError, NodeFactory, NodeInstance, NodeRuntime, NodeRuntimeState, NodeSpec,
    },
    platform::SourcePts,
};

/// 将棋盘角点完整度映射为归一化 gain。
pub struct GainScorerFactory;

impl NodeFactory for GainScorerFactory {
    fn kind(&self) -> &'static str {
        "gainScorer"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(GainScorerNode { spec }))
    }
}

pub struct GainScorerNode {
    spec: NodeSpec,
}

impl GainScorerNode {
    fn expected_corners(&self) -> Result<usize, NodeError> {
        let expected = config_u64(&self.spec, "expectedCorners", 88)?;
        usize::try_from(expected)
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                NodeError::Config("gainScorer.expectedCorners must be a positive usize".to_owned())
            })
    }
}

impl NodeInstance for GainScorerNode {
    fn kind(&self) -> &'static str {
        "gainScorer"
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
                "gainScorer.detection requires calib.detection".to_owned(),
            ));
        };
        let expected = self.expected_corners()?;
        let gain = (detection.detection.corners.len() as f64 / expected as f64).min(1.0);
        rt.emit(
            "score",
            DataPacket::Score(Arc::new(GainScore {
                gain,
                frame_identity: detection.frame_identity.clone(),
            })),
        )?;
        rt.report_event(format!("scored detection gain {gain:.3}"));
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

/// 对达到阈值的不同帧做固定帧数的稳定保持，并创建精确的抓帧请求。
pub struct CaptureGateFactory;

impl NodeFactory for CaptureGateFactory {
    fn kind(&self) -> &'static str {
        "captureGate"
    }

    fn instantiate(&self, spec: NodeSpec) -> Result<Box<dyn NodeInstance>, NodeError> {
        Ok(Box::new(CaptureGateNode {
            spec,
            stable_frames: 0,
            last_identity: None,
        }))
    }
}

pub struct CaptureGateNode {
    spec: NodeSpec,
    stable_frames: u8,
    last_identity: Option<crate::engine::ImageFrameIdentity>,
}

impl CaptureGateNode {
    fn config(&self) -> Result<CaptureGateConfig, NodeError> {
        let minimum_gain = config_f64(&self.spec, "minimumGain", 0.4)?;
        if !minimum_gain.is_finite() || !(0.0..=1.0).contains(&minimum_gain) {
            return Err(NodeError::Config(
                "captureGate.minimumGain must be finite and within [0, 1]".to_owned(),
            ));
        }
        let hold_frames = config_u64(&self.spec, "holdFrames", 3)?;
        let hold_frames = u8::try_from(hold_frames).map_err(|_| {
            NodeError::Config("captureGate.holdFrames must be within 1..=30".to_owned())
        })?;
        if !(1..=30).contains(&hold_frames) {
            return Err(NodeError::Config(
                "captureGate.holdFrames must be within 1..=30".to_owned(),
            ));
        }
        let channel = config_u16(&self.spec, "channel", 0)?;
        let camera = config_u16(&self.spec, "camera", 0)?;
        let target = match config_string(&self.spec, "target", "yuv").as_str() {
            "yuv" => CaptureTarget::Yuv { channel },
            "raw" => CaptureTarget::Raw { camera },
            value => {
                return Err(NodeError::Config(format!(
                    "captureGate.target `{value}` is unsupported; expected yuv or raw"
                )));
            }
        };
        Ok(CaptureGateConfig {
            minimum_gain,
            hold_frames,
            target,
            mode: config_string(&self.spec, "mode", "latest"),
            rtsp_pts_tolerance_90k: config_u64(&self.spec, "rtspPtsTolerance90k", 0)?,
        })
    }

    fn capture_mode(
        mode: &str,
        identity: &crate::engine::ImageFrameIdentity,
        tolerance: u64,
    ) -> Result<CaptureMode, NodeError> {
        match mode {
            "latest" => Ok(CaptureMode::Latest),
            "frame_id" => Ok(CaptureMode::FrameId(identity.frame_sequence)),
            "timestamp_ns" => Ok(CaptureMode::TimestampNs(identity.host_monotonic_time_ns)),
            "rtsp_pts_90k" => match &identity.source_pts {
                SourcePts::Known {
                    ticks,
                    time_base_numerator: 1,
                    time_base_denominator: 90_000,
                    ..
                } if *ticks >= 0 => Ok(CaptureMode::RtspPts90k {
                    pts: *ticks as u64,
                    tolerance,
                }),
                _ => Err(NodeError::Precondition(
                    "captureGate rtsp_pts_90k mode requires a non-negative 1/90000 source PTS"
                        .to_owned(),
                )),
            },
            value => Err(NodeError::Config(format!(
                "captureGate.mode `{value}` is unsupported; expected latest, frame_id, timestamp_ns, or rtsp_pts_90k"
            ))),
        }
    }

    fn validate_source(identity: &crate::engine::ImageFrameIdentity) -> Result<(), NodeError> {
        match &identity.provenance {
            FrameProvenance::Stream { .. } | FrameProvenance::Device { .. } => Ok(()),
            FrameProvenance::File { .. } => Err(NodeError::Precondition(
                "captureGate cannot request a new capture from a file-backed frame".to_owned(),
            )),
            FrameProvenance::Unknown { .. } => Err(NodeError::Precondition(
                "captureGate requires a source frame identity".to_owned(),
            )),
        }
    }
}

impl NodeInstance for CaptureGateNode {
    fn kind(&self) -> &'static str {
        "captureGate"
    }

    fn on_start(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        rt.report_state(NodeRuntimeState::Ready, "waiting for stable gain scores");
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
                "captureGate.score requires capture.score".to_owned(),
            ));
        };
        let config = self.config()?;
        if !score.gain.is_finite() || !(0.0..=1.0).contains(&score.gain) {
            return Err(NodeError::Precondition(
                "captureGate received an invalid gain outside [0, 1]".to_owned(),
            ));
        }
        Self::validate_source(&score.frame_identity)?;
        if self.last_identity.as_ref() == Some(&score.frame_identity) {
            rt.report_event("ignored duplicate score for the same source frame");
            return Ok(());
        }
        self.last_identity = Some(score.frame_identity.clone());
        if score.gain < config.minimum_gain {
            self.stable_frames = 0;
            rt.report_event(format!(
                "gain {:.3} below minimum {:.3}; stable hold reset",
                score.gain, config.minimum_gain
            ));
            return Ok(());
        }
        self.stable_frames = self.stable_frames.saturating_add(1);
        if self.stable_frames < config.hold_frames {
            rt.report_event(format!(
                "stable hold {}/{}",
                self.stable_frames, config.hold_frames
            ));
            return Ok(());
        }

        let mode = Self::capture_mode(
            &config.mode,
            &score.frame_identity,
            config.rtsp_pts_tolerance_90k,
        )?;
        rt.emit(
            "capture",
            DataPacket::CaptureRequest(Arc::new(CaptureRequest {
                target: config.target,
                mode,
                source_identity: Some(score.frame_identity.clone()),
            })),
        )?;
        self.stable_frames = 0;
        rt.report_event("capture request emitted after stable hold");
        Ok(())
    }

    fn on_action(&mut self, action: NodeAction, _rt: &mut NodeRuntime) -> Result<(), NodeError> {
        Err(NodeError::UnsupportedAction(action.name().to_owned()))
    }

    fn on_stop(&mut self, rt: &mut NodeRuntime) -> Result<(), NodeError> {
        self.stable_frames = 0;
        self.last_identity = None;
        rt.report_state(NodeRuntimeState::Idle, "stopped");
        Ok(())
    }
}

struct CaptureGateConfig {
    minimum_gain: f64,
    hold_frames: u8,
    target: CaptureTarget,
    mode: String,
    rtsp_pts_tolerance_90k: u64,
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
    use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};

    use camera_toolbox_core::{CalibrationImageSize, CalibrationPoint, ChessboardDetection};

    use super::*;
    use crate::{
        engine::{
            DetectionPacket, ImageFrameIdentity, NodeReporter, OutputRegistry, PortCardinality,
            PortSpec, SpawnContext,
        },
        platform::{SourcePtsProvenance, StreamFrameIdentity, StreamSessionId},
    };

    fn stream_identity(sequence: u64) -> ImageFrameIdentity {
        ImageFrameIdentity::from(&StreamFrameIdentity::known_at(
            StreamSessionId::new("logic-test").expect("valid stream id"),
            0,
            sequence,
            SourcePts::Known {
                ticks: sequence as i64 * 3_000,
                time_base_numerator: 1,
                time_base_denominator: 90_000,
                provenance: SourcePtsProvenance::FfmpegDecodedFrame,
            },
            sequence * 1_000,
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

    fn score(sequence: u64, gain: f64) -> DataPacket {
        DataPacket::Score(Arc::new(GainScore {
            gain,
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

    fn gain_spec() -> NodeSpec {
        NodeSpec {
            id: "gain".to_owned(),
            kind: "gainScorer".to_owned(),
            title: "Gain".to_owned(),
            inputs: vec![PortSpec {
                id: "detection".to_owned(),
                label: "Detection".to_owned(),
                kind: "calib.detection".to_owned(),
                cardinality: PortCardinality::One,
                required: true,
            }],
            outputs: vec![PortSpec {
                id: "score".to_owned(),
                label: "Score".to_owned(),
                kind: "capture.score".to_owned(),
                cardinality: PortCardinality::One,
                required: true,
            }],
            config: serde_json::json!({"expectedCorners": 4}),
        }
    }

    fn gate_spec(config: serde_json::Value) -> NodeSpec {
        NodeSpec {
            id: "gate".to_owned(),
            kind: "captureGate".to_owned(),
            title: "Gate".to_owned(),
            inputs: vec![PortSpec {
                id: "score".to_owned(),
                label: "Score".to_owned(),
                kind: "capture.score".to_owned(),
                cardinality: PortCardinality::One,
                required: true,
            }],
            outputs: vec![PortSpec {
                id: "capture".to_owned(),
                label: "Capture".to_owned(),
                kind: "command.capture".to_owned(),
                cardinality: PortCardinality::One,
                required: true,
            }],
            config,
        }
    }

    #[test]
    fn gain_scorer_preserves_detection_frame_identity() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&record));
        let mut node = GainScorerFactory
            .instantiate(gain_spec())
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
        assert_eq!(score.gain, 0.75);
        assert_eq!(score.frame_identity, stream_identity(7));
    }

    #[test]
    fn capture_gate_emits_typed_request_after_stable_hold() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&record));
        let mut node = CaptureGateFactory
            .instantiate(gate_spec(serde_json::json!({
                "minimumGain": 0.4,
                "holdFrames": 3,
                "mode": "latest",
                "channel": 3,
                "camera": 1,
                "target": "yuv"
            })))
            .expect("instantiate");

        for sequence in 1..=3 {
            node.on_input("score", score(sequence, 0.7), &mut runtime)
                .expect("stable score");
        }

        let outputs = record.lock().expect("record lock");
        assert_eq!(outputs.len(), 1);
        let DataPacket::CaptureRequest(request) = &outputs[0] else {
            panic!("expected capture request");
        };
        assert_eq!(request.target, CaptureTarget::Yuv { channel: 3 });
        assert_eq!(request.mode, CaptureMode::Latest);
        assert_eq!(request.source_identity.as_ref(), Some(&stream_identity(3)));
    }

    #[test]
    fn capture_gate_resets_unstable_hold_without_triggering() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&record));
        let mut node = CaptureGateFactory
            .instantiate(gate_spec(
                serde_json::json!({"minimumGain": 0.4, "holdFrames": 3}),
            ))
            .expect("instantiate");

        for (sequence, gain) in [(1, 0.8), (2, 0.1), (3, 0.8), (4, 0.8)] {
            node.on_input("score", score(sequence, gain), &mut runtime)
                .expect("score is valid");
        }
        assert!(record.lock().expect("record lock").is_empty());
    }

    #[test]
    fn capture_gate_rejects_invalid_threshold_unknown_identity_and_target() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = runtime(Arc::clone(&record));
        let mut invalid_threshold = CaptureGateFactory
            .instantiate(gate_spec(serde_json::json!({"minimumGain": 1.1})))
            .expect("instantiate");
        assert!(matches!(
            invalid_threshold.on_input("score", score(1, 0.8), &mut runtime),
            Err(NodeError::Config(_))
        ));

        let mut invalid_target = CaptureGateFactory
            .instantiate(gate_spec(serde_json::json!({"target": "rgb"})))
            .expect("instantiate");
        assert!(matches!(
            invalid_target.on_input("score", score(1, 0.8), &mut runtime),
            Err(NodeError::Config(_))
        ));

        let mut missing_identity = CaptureGateFactory
            .instantiate(gate_spec(serde_json::json!({"holdFrames": 1})))
            .expect("instantiate");
        let unknown = DataPacket::Score(Arc::new(GainScore {
            gain: 0.8,
            frame_identity: crate::engine::ImageFrameIdentity {
                provenance: FrameProvenance::Unknown {
                    reason: "test".to_owned(),
                },
                frame_sequence: 0,
                source_pts: SourcePts::Unavailable {
                    reason: "test".to_owned(),
                },
                host_monotonic_time_ns: 0,
            },
        }));
        assert!(matches!(
            missing_identity.on_input("score", unknown, &mut runtime),
            Err(NodeError::Precondition(_))
        ));
        assert!(record.lock().expect("record lock").is_empty());
    }
}
