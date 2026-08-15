//! I²C / EEPROM 执行器端口：把「目标 + 凭据 + 动作 → 结果」抽象成 app 层可注入的 trait。
//!
//! 与 `I2cHelperService` / `EepromProvisionService` 不同，这两个 trait 把「连接目标 + 凭据」作为
//! **每次调用参数**传入（而非构造时固化），因为 web 工作流里每个控制节点按自身 config 决定目标；
//! 执行体（web 层实现）内部再据此构造具体的 SSH helper service。错误统一折叠为 `String`，
//! 是 app 层能表达的最简契约（service error 在实现侧转字符串）。

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

/// X5 设备控制客户端：对给定 host:port 执行 X5_233 TCP 控制操作。
///
/// 结果统一折叠为 `serde_json::Value`（X5 具体状态类型在 adapters，app 层只表达 JSON），
/// 错误统一为 `String`。这是 X5Device 节点接入真实 x5_tcp_client 能力的端口。
pub trait X5ControlClient: Send + Sync {
    /// 探测 X5 设备协议/通道/fps 摘要。
    fn probe(&self, host: &str, port: u16) -> Result<serde_json::Value, String>;

    /// 读取 X5 驱动运行状态（RTSP 通道、ring、fps/bitrate）。
    fn status(&self, host: &str, port: u16) -> Result<serde_json::Value, String>;

    /// 抓取指定通道的最新 YUV 快照（输出元数据，不含全像素）。
    fn capture_snapshot(
        &self,
        host: &str,
        port: u16,
        channel: u16,
    ) -> Result<serde_json::Value, String>;
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
