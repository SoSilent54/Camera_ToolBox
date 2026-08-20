//! I²C / EEPROM 执行器端口：把「目标 + 凭据 + 动作 → 结果」抽象成 app 层可注入的 trait。
//!
//! 与 `I2cHelperService` / `EepromProvisionService` 不同，这两个 trait 把「连接目标 + 凭据」作为
//! **每次调用参数**传入（而非构造时固化），因为 web 工作流里每个控制节点按自身 config 决定目标；
//! 执行体（web 层实现）内部再据此构造具体的 SSH helper service。错误统一折叠为 `String`，
//! 是 app 层能表达的最简契约（service error 在实现侧转字符串）。

use std::sync::Arc;

use super::{
    CommandResult, RemoteFileStat, RemoteOperationControl, TypedCommandRequest,
    eeprom::{EepromHelperAction, EepromHelperResult},
    i2c::{I2cHelperAction, I2cHelperResult},
};

/// 控制目标：host/port/username + 可选的 pinned host key。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlTargetSpec {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub expected_host_key: Option<String>,
}

/// Hex Arm 可连接目标与安全控制配置。
///
/// app 层只保留适配器实现所需的传输与超时语义；默认关闭控制，避免工作流配置
/// 被动恢复时意外允许运动命令。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HexArmTargetConfig {
    pub host: String,
    pub port: u16,
    pub transport: HexArmTransport,
    pub control_enabled: bool,
    pub command_timeout_ms: u64,
    pub connect_timeout_ms: u64,
}

impl Default for HexArmTargetConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 8439,
            transport: HexArmTransport::WebSocket,
            control_enabled: false,
            command_timeout_ms: 200,
            connect_timeout_ms: 3_000,
        }
    }
}

/// Hex Arm 首期支持的控制传输。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HexArmTransport {
    /// WebSocket Binary 帧直接承载原始 protobuf。
    #[default]
    WebSocket,
    /// KCP 预留为显式不支持路径；首期适配器不得回退到 WebSocket。
    Kcp,
}

/// 一次关节位置命令，单位为弧度。
#[derive(Clone, Debug, PartialEq)]
pub struct HexArmJointPositionsRequest {
    pub joint_positions_radians: Vec<f64>,
}

/// Hex Arm 控制客户端：适配器负责 WebSocket/protobuf 协议，app 层只编排安全动作。
///
/// 所有调用返回 JSON，以避免 app 层耦合具体协议响应类型；错误由适配器折叠为字符串。
pub trait HexArmControlClient: Send + Sync {
    fn probe(&self, target: &HexArmTargetConfig) -> Result<serde_json::Value, String>;

    fn status(&self, target: &HexArmTargetConfig) -> Result<serde_json::Value, String>;

    fn connect(&self, target: &HexArmTargetConfig) -> Result<serde_json::Value, String>;

    fn initialize_api_control(
        &self,
        target: &HexArmTargetConfig,
    ) -> Result<serde_json::Value, String>;

    fn calibrate(&self, target: &HexArmTargetConfig) -> Result<serde_json::Value, String>;

    fn clear_parking_stop(&self, target: &HexArmTargetConfig) -> Result<serde_json::Value, String>;

    fn zero_current(&self, target: &HexArmTargetConfig) -> Result<serde_json::Value, String>;

    fn send_joint_positions(
        &self,
        target: &HexArmTargetConfig,
        request: &HexArmJointPositionsRequest,
    ) -> Result<serde_json::Value, String>;

    fn disconnect(&self, target: &HexArmTargetConfig) -> Result<serde_json::Value, String>;
}

/// I²C 执行器：对给定目标执行一次 I²C helper 动作。
pub trait I2cExecutor: Send + Sync {
    /// # Errors
    ///
    /// 目标/凭据无效、SSH 失败、helper 协议损坏或 helper 拒绝执行时返回错误字符串。
    fn execute(
        &self,
        target: &ControlTargetSpec,
        credential_ref: &str,
        action: I2cHelperAction,
        control: RemoteOperationControl,
    ) -> Result<I2cHelperResult, String>;
}

/// EEPROM 执行器：对给定目标执行一次 EEPROM helper 动作（Inspect / Provision）。
pub trait EepromExecutor: Send + Sync {
    /// # Errors
    ///
    /// 目标/凭据无效、SSH 失败、helper 协议损坏或 helper 拒绝写入时返回错误字符串。
    fn execute(
        &self,
        target: &ControlTargetSpec,
        credential_ref: &str,
        action: EepromHelperAction,
        control: RemoteOperationControl,
    ) -> Result<EepromHelperResult, String>;
}

/// X5_233 TCP 抓帧结果。字节在适配器完成协议校验后以不可变共享所有权交给引擎。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum X5233CapturePayload {
    Nv12 {
        channel: u16,
        width: u32,
        height: u32,
        y_len: usize,
        uv_len: usize,
        frame_id: u64,
        timestamp_ns: u64,
        payload: Arc<[u8]>,
    },
    BayerRaw {
        camera: u16,
        width: u32,
        height: u32,
        stride_bytes: u32,
        format_code: u32,
        frame_id: u64,
        timestamp_ns: u64,
        payload: Arc<[u8]>,
    },
}

/// X5_233 驱动控制客户端：只公开实际设备数据和状态，不产生 RTSP endpoint/url 负载。
pub trait X5ControlClient: Send + Sync {
    /// 探测 X5_233 驱动协议/通道/fps 摘要。
    fn probe(&self, host: &str, port: u16) -> Result<serde_json::Value, String>;

    /// 读取 X5_233 驱动运行状态（RTSP 通道、ring、fps/bitrate）。
    fn status(&self, host: &str, port: u16) -> Result<serde_json::Value, String>;

    /// 根据 `command.capture.request.v1` 获取已验证的 YUV 或 RAW source payload。
    fn capture(
        &self,
        host: &str,
        port: u16,
        request: &crate::engine::CaptureRequest,
    ) -> Result<X5233CapturePayload, String>;
}

/// SFTP 文件读取器：对给定目标读取远端文件的元数据或字节。
///
/// 与 `RemoteFileService` 的 fetch-to-CaptureStore 重链路不同，这个 trait 只提供
/// 「stat + 读字节」的轻量能力，供 `SftpFileSource` 节点加载并解码远程图片。
pub trait SftpFileReader: Send + Sync {
    /// 读取远端文件元数据（大小 + mtime）。
    fn stat(
        &self,
        target: &ControlTargetSpec,
        credential_ref: &str,
        remote_path: &str,
        control: RemoteOperationControl,
    ) -> Result<RemoteFileStat, String>;

    /// 读取远端文件字节（有界；超出 limit 返回错误）。
    fn read(
        &self,
        target: &ControlTargetSpec,
        credential_ref: &str,
        remote_path: &str,
        limit: usize,
        control: RemoteOperationControl,
    ) -> Result<Vec<u8>, String>;
}

/// SSH 命令执行器：对给定目标执行一次 allowlisted typed 命令。
///
/// 供 `SshSession` 节点执行远程命令（命令来自部署时注册的 typed allowlist recipe）。
pub trait SshCommandExecutor: Send + Sync {
    /// 执行一次 typed 命令。
    fn execute(
        &self,
        target: &ControlTargetSpec,
        credential_ref: &str,
        request: TypedCommandRequest,
        control: RemoteOperationControl,
    ) -> Result<CommandResult, String>;
}
