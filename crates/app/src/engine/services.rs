//! 引擎服务：节点可用外部能力的集合，由 web 启动时装配注入。
//!
//! 节点只依赖 trait（`StreamService` / `CalibrationBackend`），不依赖具体适配器。

use std::sync::Arc;

use super::node::NodeError;
use crate::platform::{
    EepromExecutor, HexArmControlClient, I2cExecutor, RtspStreamConfig, SftpFileReader,
    SshCommandExecutor, StreamService, X5ControlClient,
};
use crate::ports::{CalibrationBackend, RasterImageCodec, RawFrameLoader};

/// 流服务工厂：按 URL 配置创建独立流服务。
///
/// 源节点的 URL 各不相同，因此引擎不能持有单个 `StreamService`，而是持有工厂。
pub trait StreamServiceFactory: Send + Sync {
    fn create(&self, config: RtspStreamConfig) -> Arc<dyn StreamService>;
}

/// 引擎级服务集合。字段均为 `Option`，未配置时节点按前置条件失败而非 panic。
#[derive(Clone, Default)]
pub struct EngineServices {
    pub stream_factory: Option<Arc<dyn StreamServiceFactory>>,
    pub calibration: Option<Arc<dyn CalibrationBackend>>,
    pub raw_loader: Option<Arc<dyn RawFrameLoader>>,
    pub image_codec: Option<Arc<dyn RasterImageCodec>>,
    pub i2c_executor: Option<Arc<dyn I2cExecutor>>,
    pub eeprom_executor: Option<Arc<dyn EepromExecutor>>,
    pub x5_client: Option<Arc<dyn X5ControlClient>>,
    pub hex_arm_client: Option<Arc<dyn HexArmControlClient>>,
    pub sftp_reader: Option<Arc<dyn SftpFileReader>>,
    pub ssh_command_executor: Option<Arc<dyn SshCommandExecutor>>,
}

impl EngineServices {
    /// 获取流服务工厂（源节点连接 RTSP 时使用）。
    pub fn stream_factory(&self) -> Result<Arc<dyn StreamServiceFactory>, NodeError> {
        self.stream_factory.clone().ok_or_else(|| {
            NodeError::Precondition("stream service factory is not configured".to_owned())
        })
    }

    /// 获取标定后端（标定求解/检测节点使用）。
    pub fn calibration_backend(&self) -> Result<Arc<dyn CalibrationBackend>, NodeError> {
        self.calibration.clone().ok_or_else(|| {
            NodeError::Precondition("calibration backend is not configured".to_owned())
        })
    }

    /// 获取本地 RAW 帧加载器（LocalFileSource 等本地文件源节点使用）。
    pub fn raw_loader(&self) -> Result<Arc<dyn RawFrameLoader>, NodeError> {
        self.raw_loader
            .clone()
            .ok_or_else(|| NodeError::Precondition("raw frame loader is not configured".to_owned()))
    }

    /// 获取静态 raster 编解码器（图片加载/合成/检测转 PNG 时使用）。
    pub fn image_codec(&self) -> Result<Arc<dyn RasterImageCodec>, NodeError> {
        self.image_codec.clone().ok_or_else(|| {
            NodeError::Precondition("raster image codec is not configured".to_owned())
        })
    }

    /// 获取 I²C 执行器（I2cTransfer 节点执行读写时使用）。
    pub fn i2c_executor(&self) -> Result<Arc<dyn I2cExecutor>, NodeError> {
        self.i2c_executor
            .clone()
            .ok_or_else(|| NodeError::Precondition("i2c executor is not configured".to_owned()))
    }

    /// 获取 EEPROM 执行器（EepromProvision 节点 inspect/provision 时使用）。
    pub fn eeprom_executor(&self) -> Result<Arc<dyn EepromExecutor>, NodeError> {
        self.eeprom_executor
            .clone()
            .ok_or_else(|| NodeError::Precondition("eeprom executor is not configured".to_owned()))
    }

    /// 获取 X5_233 Driver 控制客户端（状态查询和 `command.capture.request.v1` 时使用）。
    pub fn x5_client(&self) -> Result<Arc<dyn X5ControlClient>, NodeError> {
        self.x5_client.clone().ok_or_else(|| {
            NodeError::Precondition("x5 control client is not configured".to_owned())
        })
    }

    /// 获取 Hex Arm 控制客户端（HexArmDevice 节点执行控制动作时使用）。
    pub fn hex_arm_client(&self) -> Result<Arc<dyn HexArmControlClient>, NodeError> {
        self.hex_arm_client.clone().ok_or_else(|| {
            NodeError::Precondition("hex arm control client is not configured".to_owned())
        })
    }

    /// 获取 SFTP 文件读取器（SftpFileSource 节点加载远程图片时使用）。
    pub fn sftp_reader(&self) -> Result<Arc<dyn SftpFileReader>, NodeError> {
        self.sftp_reader
            .clone()
            .ok_or_else(|| NodeError::Precondition("sftp file reader is not configured".to_owned()))
    }

    /// 获取 SSH 命令执行器（SshSession 节点执行远程命令时使用）。
    pub fn ssh_command_executor(&self) -> Result<Arc<dyn SshCommandExecutor>, NodeError> {
        self.ssh_command_executor.clone().ok_or_else(|| {
            NodeError::Precondition("ssh command executor is not configured".to_owned())
        })
    }
}
