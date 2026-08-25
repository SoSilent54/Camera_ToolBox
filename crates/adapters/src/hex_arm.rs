//! Hex Arm WebSocket Binary/protobuf adapter.
//!
//! KCP is deliberately represented by the app configuration but rejected before a
//! connection is attempted. This adapter owns one WebSocket session at a time;
//! changing host or port closes that session before connecting to the new target.

use std::{
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use camera_toolbox_app::platform::{
    HexArmControlClient, HexArmJointPositionsRequest, HexArmTargetConfig, HexArmTransport,
};
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use serde_json::{Value, json};
use tokio::{net::TcpStream, runtime::Runtime, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

mod proto {
    include!(concat!(env!("OUT_DIR"), "/_.rs"));
}

const PROTOCOL_MAJOR_VERSION: u32 = 1;

type HexArmSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 可注入 app `EngineServices` 的 Hex Arm 控制客户端。
#[derive(Default)]
pub struct HexArmWebSocketClient {
    session: Mutex<Option<ConnectedSession>>,
}

struct ConnectedSession {
    target: SessionTarget,
    runtime: Runtime,
    socket: HexArmSocket,
    latest_status: proto::ApiUp,
}

#[derive(Debug, PartialEq, Eq)]
struct SessionTarget {
    host: String,
    port: u16,
}

/// APIDown 命令编码器；只包含首期允许的安全命令。
pub struct HexArmCommandBuilder;

impl HexArmCommandBuilder {
    /// 编码 API 控制初始化命令。
    pub fn initialize_api_control() -> Vec<u8> {
        encode_arm_exclusive(
            proto::arm_exclusive_command::ExclusiveCommand::ApiControlInitialize(true),
        )
    }

    /// 编码 API 控制释放命令，用于断开连接前归还会话。
    pub fn deinitialize_api_control() -> Vec<u8> {
        encode_arm_exclusive(
            proto::arm_exclusive_command::ExclusiveCommand::ApiControlInitialize(false),
        )
    }

    /// 编码标定命令。
    pub fn calibrate() -> Vec<u8> {
        encode_arm_exclusive(proto::arm_exclusive_command::ExclusiveCommand::Calibrate(
            true,
        ))
    }

    /// 编码远程清除停车停止命令。
    pub fn clear_parking_stop() -> Vec<u8> {
        let shared = proto::ArmSharedCommand {
            command: Some(proto::arm_shared_command::Command::ClearParkingStop(true)),
        };
        proto::ApiDown {
            down: Some(proto::api_down::Down::ArmCommand(proto::ArmCommand {
                command: Some(proto::arm_command::Command::ArmSharedCommand(shared)),
            })),
        }
        .encode_to_vec()
    }

    /// 编码零电流命令；不暴露 torque/MIT 控制。
    pub fn zero_current() -> Vec<u8> {
        let control = proto::ArmApiControlCommand {
            command: Some(
                proto::arm_api_control_command::Command::ArmApiZeroCurrentCommand(
                    proto::ArmApiZeroCurrentCommand {},
                ),
            ),
        };
        encode_api_control(control)
    }

    /// 编码关节位置命令。所有位置必须是有限弧度值。
    pub fn joint_positions(joint_positions_radians: &[f64]) -> Result<Vec<u8>, String> {
        validate_joint_positions(joint_positions_radians)?;
        let control = proto::ArmApiControlCommand {
            command: Some(
                proto::arm_api_control_command::Command::ArmApiJointPositionCommand(
                    proto::ArmApiJointPositionCommand {
                        joint_positions: joint_positions_radians.to_vec(),
                    },
                ),
            ),
        };
        Ok(encode_api_control(control))
    }
}

impl HexArmWebSocketClient {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn probe_inner(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        let session = connect_session(target)?;
        let status = project_status(session.latest_status.clone());
        close_session(session);
        Ok(status)
    }

    fn connect_inner(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        let guard = self.ensure_session(target)?;
        let status = project_status(
            guard
                .as_ref()
                .expect("session is initialized")
                .latest_status
                .clone(),
        );
        Ok(json!({ "connected": true, "transport": "websocket", "status": status }))
    }

    fn status_inner(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        validate_transport(target)?;
        let mut guard = lock_session(&self.session)?;
        if guard.is_none() {
            drop(guard);
            return self.probe_inner(target);
        }
        if guard
            .as_ref()
            .is_some_and(|session| session.target != SessionTarget::from(target))
        {
            return Err("Hex Arm target differs from the active connection".to_owned());
        }
        let result = receive_status(
            guard.as_mut().expect("session is initialized"),
            command_timeout(target),
        )
        .map(|status| {
            guard
                .as_mut()
                .expect("session is initialized")
                .latest_status = status.clone();
            project_status(status)
        });
        if result.is_err() {
            discard_session(&mut guard);
        }
        result
    }
    fn send_inner(
        &self,
        target: &HexArmTargetConfig,
        command: Vec<u8>,
        require_session_ownership: bool,
    ) -> Result<Value, String> {
        let mut guard = self.active_session(target)?;
        let result = (|| {
            let session = guard.as_mut().expect("session is initialized");
            if require_session_ownership {
                validate_session_ownership(&session.latest_status)?;
            }
            send_binary(session, command_timeout(target), command)?;
            let status = receive_status(session, command_timeout(target))?;
            session.latest_status = status.clone();
            Ok(project_status(status))
        })();
        if result.is_err() {
            discard_session(&mut guard);
        }
        result
    }

    fn initialize_api_control_inner(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        let mut guard = self.active_session(target)?;
        let result = (|| {
            let session = guard.as_mut().expect("session is initialized");
            validate_initialization_status(&session.latest_status)?;
            send_binary(
                session,
                command_timeout(target),
                HexArmCommandBuilder::initialize_api_control(),
            )?;
            let status = receive_status(session, command_timeout(target))?;
            validate_session_ownership(&status)?;
            session.latest_status = status.clone();
            Ok(project_status(status))
        })();
        if result.is_err() {
            discard_session(&mut guard);
        }
        result
    }

    fn disconnect_inner(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        validate_transport(target)?;
        let mut guard = lock_session(&self.session)?;
        if guard
            .as_ref()
            .is_some_and(|session| session.target != SessionTarget::from(target))
        {
            return Err("Hex Arm target differs from the active connection".to_owned());
        }
        if let Some(session) = guard.as_mut() {
            // 断开时不等待状态，先尽力卸除驱动再释放 API 会话。
            let _ = send_binary(
                session,
                command_timeout(target),
                HexArmCommandBuilder::zero_current(),
            );
            let _ = send_binary(
                session,
                command_timeout(target),
                HexArmCommandBuilder::deinitialize_api_control(),
            );
        }
        discard_session(&mut guard);
        Ok(json!({ "connected": false, "transport": "websocket" }))
    }

    fn ensure_session(
        &self,
        target: &HexArmTargetConfig,
    ) -> Result<MutexGuard<'_, Option<ConnectedSession>>, String> {
        validate_transport(target)?;
        let mut guard = lock_session(&self.session)?;
        let required_target = SessionTarget::from(target);
        if guard
            .as_ref()
            .is_some_and(|session| session.target != required_target)
        {
            return Err(
                "Hex Arm target differs from the active connection; disconnect it first".to_owned(),
            );
        }
        if guard.is_none() {
            *guard = Some(connect_session(target)?);
        }
        Ok(guard)
    }
    fn active_session(
        &self,
        target: &HexArmTargetConfig,
    ) -> Result<MutexGuard<'_, Option<ConnectedSession>>, String> {
        validate_transport(target)?;
        let guard = lock_session(&self.session)?;
        let Some(session) = guard.as_ref() else {
            return Err(
                "Hex Arm is not connected; call connect before sending commands".to_owned(),
            );
        };
        if session.target != SessionTarget::from(target) {
            return Err("Hex Arm target differs from the active connection".to_owned());
        }
        Ok(guard)
    }
}

impl HexArmControlClient for HexArmWebSocketClient {
    fn probe(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        self.probe_inner(target)
    }

    fn status(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        self.status_inner(target)
    }

    fn connect(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        self.connect_inner(target)
    }

    fn initialize_api_control(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        self.initialize_api_control_inner(target)
    }

    fn calibrate(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        require_control_enabled(target, "calibrate")?;
        self.send_inner(target, HexArmCommandBuilder::calibrate(), true)
    }

    fn clear_parking_stop(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        self.send_inner(target, HexArmCommandBuilder::clear_parking_stop(), true)
    }

    fn zero_current(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        self.send_inner(target, HexArmCommandBuilder::zero_current(), true)
    }

    fn send_joint_positions(
        &self,
        target: &HexArmTargetConfig,
        request: &HexArmJointPositionsRequest,
    ) -> Result<Value, String> {
        require_control_enabled(target, "send_joint_positions")?;
        self.send_inner(
            target,
            HexArmCommandBuilder::joint_positions(&request.joint_positions_radians)?,
            true,
        )
    }

    fn disconnect(&self, target: &HexArmTargetConfig) -> Result<Value, String> {
        self.disconnect_inner(target)
    }
}

impl From<&HexArmTargetConfig> for SessionTarget {
    fn from(target: &HexArmTargetConfig) -> Self {
        Self {
            host: target.host.clone(),
            port: target.port,
        }
    }
}

fn encode_arm_exclusive(command: proto::arm_exclusive_command::ExclusiveCommand) -> Vec<u8> {
    proto::ApiDown {
        down: Some(proto::api_down::Down::ArmCommand(proto::ArmCommand {
            command: Some(proto::arm_command::Command::ArmExclusiveCommand(
                proto::ArmExclusiveCommand {
                    exclusive_command: Some(command),
                },
            )),
        })),
    }
    .encode_to_vec()
}

fn encode_api_control(control: proto::ArmApiControlCommand) -> Vec<u8> {
    encode_arm_exclusive(
        proto::arm_exclusive_command::ExclusiveCommand::ArmApiControlCommand(control),
    )
}

fn validate_joint_positions(joint_positions_radians: &[f64]) -> Result<(), String> {
    if joint_positions_radians
        .iter()
        .all(|value| value.is_finite())
    {
        Ok(())
    } else {
        Err("Hex Arm joint positions must be finite radians".to_owned())
    }
}

fn require_control_enabled(target: &HexArmTargetConfig, command: &str) -> Result<(), String> {
    if target.control_enabled {
        Ok(())
    } else {
        Err(format!(
            "Hex Arm {command} is disabled; set control_enabled=true before sending motion commands"
        ))
    }
}

fn validate_transport(target: &HexArmTargetConfig) -> Result<(), String> {
    match target.transport {
        HexArmTransport::WebSocket => validate_target(target),
        HexArmTransport::Kcp => {
            Err("Hex Arm KCP transport is unsupported in the first implementation".to_owned())
        }
    }
}

fn validate_target(target: &HexArmTargetConfig) -> Result<(), String> {
    if target.host.trim().is_empty() {
        Err("Hex Arm host must not be empty".to_owned())
    } else if target.port == 0 {
        Err("Hex Arm port must not be zero".to_owned())
    } else {
        Ok(())
    }
}

fn validate_session_ownership(status: &proto::ApiUp) -> Result<(), String> {
    let Some(proto::api_up::Status::ArmStatus(arm)) = status.status.as_ref() else {
        return Err("Hex Arm APIUp does not contain arm_status".to_owned());
    };
    if status.session_id == 0 {
        return Err("Hex Arm APIUp session_id must not be zero".to_owned());
    }
    if arm.session_holder != status.session_id {
        return Err("Hex Arm API session is not held by this WebSocket connection".to_owned());
    }
    if !arm.api_control_initialized {
        return Err(
            "Hex Arm API control is not initialized for this WebSocket connection".to_owned(),
        );
    }
    Ok(())
}

fn validate_initialization_status(status: &proto::ApiUp) -> Result<(), String> {
    let Some(proto::api_up::Status::ArmStatus(arm)) = status.status.as_ref() else {
        return Err("Hex Arm APIUp does not contain arm_status".to_owned());
    };
    if status.session_id == 0 {
        return Err("Hex Arm APIUp session_id must not be zero".to_owned());
    }
    if arm.session_holder != 0 && arm.session_holder != status.session_id {
        return Err("Hex Arm API session is held by another WebSocket connection".to_owned());
    }
    Ok(())
}

fn connect_session(target: &HexArmTargetConfig) -> Result<ConnectedSession, String> {
    validate_transport(target)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| format!("failed to create Hex Arm runtime: {error}"))?;
    let url = websocket_url(target);
    let socket = runtime.block_on(async {
        timeout(connect_timeout(target), connect_async(&url))
            .await
            .map_err(|_| {
                format!(
                    "Hex Arm WebSocket connect timed out after {} ms",
                    target.connect_timeout_ms
                )
            })?
            .map(|(socket, _response)| socket)
            .map_err(|error| format!("Hex Arm WebSocket connect failed: {error}"))
    })?;
    let mut session = ConnectedSession {
        target: SessionTarget::from(target),
        runtime,
        socket,
        latest_status: proto::ApiUp::default(),
    };
    match receive_status(&mut session, connect_timeout(target)) {
        Ok(status) => {
            session.latest_status = status;
            Ok(session)
        }
        Err(error) => {
            close_session(session);
            Err(error)
        }
    }
}

fn receive_status(
    session: &mut ConnectedSession,
    timeout_duration: Duration,
) -> Result<proto::ApiUp, String> {
    session.runtime.block_on(async {
        timeout(timeout_duration, async {
            loop {
                let message = session
                    .socket
                    .next()
                    .await
                    .ok_or_else(|| "Hex Arm WebSocket closed while waiting for status".to_owned())
                    .and_then(|message| {
                        message.map_err(|error| format!("Hex Arm WebSocket read failed: {error}"))
                    })?;
                match message {
                    Message::Binary(bytes) => return decode_status(&bytes),
                    Message::Ping(payload) => session
                        .socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|error| format!("Hex Arm WebSocket pong failed: {error}"))?,
                    Message::Close(_) => {
                        return Err("Hex Arm WebSocket closed while waiting for status".to_owned());
                    }
                    Message::Text(_) | Message::Frame(_) => {
                        return Err(
                            "unexpected Hex Arm WebSocket non-Binary frame; expected Binary protobuf"
                                .to_owned(),
                        );
                    }
                    Message::Pong(_) => {}
                }
            }
        })
        .await
        .map_err(|_| "Hex Arm status timed out waiting for WebSocket Binary frame".to_owned())?
    })
}

fn send_binary(
    session: &mut ConnectedSession,
    timeout_duration: Duration,
    command: Vec<u8>,
) -> Result<(), String> {
    session.runtime.block_on(async {
        timeout(
            timeout_duration,
            session.socket.send(Message::Binary(command.into())),
        )
        .await
        .map_err(|_| "Hex Arm command timed out while writing WebSocket frame".to_owned())?
        .map_err(|error| format!("Hex Arm WebSocket write failed: {error}"))
    })
}

fn decode_status(bytes: &[u8]) -> Result<proto::ApiUp, String> {
    let message = proto::ApiUp::decode(bytes)
        .map_err(|error| format!("invalid Hex Arm APIUp protobuf: {error}"))?;
    if message.protocol_major_version != PROTOCOL_MAJOR_VERSION {
        return Err(format!(
            "unsupported Hex Arm protocol major version {}; expected {PROTOCOL_MAJOR_VERSION}",
            message.protocol_major_version
        ));
    }
    Ok(message)
}

fn project_status(message: proto::ApiUp) -> Value {
    let arm = match message.status {
        Some(proto::api_up::Status::ArmStatus(arm)) => json!({
            "api_control_initialized": arm.api_control_initialized,
            "calibrated": arm.calibrated,
            "session_holder": arm.session_holder,
            "parking_stop": arm.parking_stop_detail.map(|stop| json!({
                "reason": stop.reason,
                "category": stop.category,
                "remotely_clearable": stop.is_remotely_clearable,
            })),
            "motor_status": arm.motor_status.into_iter().map(|motor| json!({
                "torque_nm": motor.torque,
                "speed_rad_per_second": motor.speed,
                "encoder_position": motor.position,
                "pulse_per_rotation": motor.pulse_per_rotation,
                "wheel_radius": motor.wheel_radius,
            })).collect::<Vec<_>>(),
        }),
        _ => Value::Null,
    };
    json!({
        "protocol_major_version": message.protocol_major_version,
        "protocol_minor_version": message.protocol_minor_version,
        "robot_type": message.robot_type,
        "session_id": message.session_id,
        "log": message.log,
        "arm": arm,
    })
}

fn close_session(mut session: ConnectedSession) {
    let _ = session.runtime.block_on(session.socket.close(None));
}

fn discard_session(session: &mut MutexGuard<'_, Option<ConnectedSession>>) {
    if let Some(session) = session.take() {
        close_session(session);
    }
}

fn lock_session(
    session: &Mutex<Option<ConnectedSession>>,
) -> Result<MutexGuard<'_, Option<ConnectedSession>>, String> {
    session
        .lock()
        .map_err(|_| "Hex Arm session lock is poisoned".to_owned())
}

const fn connect_timeout(target: &HexArmTargetConfig) -> Duration {
    Duration::from_millis(target.connect_timeout_ms)
}

const fn command_timeout(target: &HexArmTargetConfig) -> Duration {
    Duration::from_millis(target.command_timeout_ms)
}

fn websocket_url(target: &HexArmTargetConfig) -> String {
    let host = target.host.trim();
    if host.starts_with('[') || !host.contains(':') {
        format!("ws://{host}:{}", target.port)
    } else {
        format!("ws://[{host}]:{}", target.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joint_position_command_encodes_protobuf() {
        let encoded = HexArmCommandBuilder::joint_positions(&[0.0, 1.5]).expect("finite joints");
        let message = proto::ApiDown::decode(encoded.as_slice()).expect("encoded APIDown");
        let Some(proto::api_down::Down::ArmCommand(arm)) = message.down else {
            panic!("expected arm command");
        };
        let Some(proto::arm_command::Command::ArmExclusiveCommand(exclusive)) = arm.command else {
            panic!("expected exclusive arm command");
        };
        let Some(proto::arm_exclusive_command::ExclusiveCommand::ArmApiControlCommand(control)) =
            exclusive.exclusive_command
        else {
            panic!("expected API control command");
        };
        let Some(proto::arm_api_control_command::Command::ArmApiJointPositionCommand(joints)) =
            control.command
        else {
            panic!("expected joint position command");
        };
        assert_eq!(joints.joint_positions, [0.0, 1.5]);
    }

    #[test]
    fn status_rejects_incompatible_protocol_major_version() {
        let encoded = proto::ApiUp {
            protocol_major_version: PROTOCOL_MAJOR_VERSION + 1,
            ..Default::default()
        }
        .encode_to_vec();
        let error = decode_status(&encoded).expect_err("incompatible major must fail");
        assert!(error.contains("unsupported Hex Arm protocol major version"));
    }

    #[test]
    fn status_projects_arm_state() {
        let encoded = proto::ApiUp {
            protocol_major_version: PROTOCOL_MAJOR_VERSION,
            protocol_minor_version: 2,
            session_id: 9,
            status: Some(proto::api_up::Status::ArmStatus(proto::ArmStatus {
                api_control_initialized: true,
                calibrated: true,
                session_holder: 9,
                ..Default::default()
            })),
            ..Default::default()
        }
        .encode_to_vec();
        let status = project_status(decode_status(&encoded).expect("compatible status"));
        assert_eq!(status["arm"]["api_control_initialized"], true);
        assert_eq!(status["arm"]["calibrated"], true);
        assert_eq!(status["session_id"], 9);
    }

    #[test]
    fn joint_positions_reject_non_finite_radians() {
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let error = HexArmCommandBuilder::joint_positions(&[invalid])
                .expect_err("non-finite joint angle must fail");
            assert!(error.contains("finite radians"));
        }
    }

    #[test]
    fn kcp_is_rejected_before_connection() {
        let target = HexArmTargetConfig {
            transport: HexArmTransport::Kcp,
            ..Default::default()
        };
        let error = HexArmWebSocketClient::default()
            .connect(&target)
            .expect_err("KCP is intentionally unsupported");
        assert!(error.contains("KCP transport is unsupported"));
    }

    #[test]
    fn exclusive_commands_require_initialized_session_ownership() {
        let status = proto::ApiUp {
            session_id: 7,
            status: Some(proto::api_up::Status::ArmStatus(proto::ArmStatus {
                api_control_initialized: true,
                session_holder: 7,
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(validate_session_ownership(&status).is_ok());

        let mut not_held = status;
        let Some(proto::api_up::Status::ArmStatus(arm)) = not_held.status.as_mut() else {
            panic!("expected arm status");
        };
        arm.session_holder = 0;
        assert!(validate_session_ownership(&not_held).is_err());
    }
}
