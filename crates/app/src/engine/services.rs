//! 引擎服务：节点可用外部能力的集合，由 web 启动时装配注入。
//!
//! 节点只依赖 trait（`StreamService` / `CalibrationBackend`），不依赖具体适配器。

use std::sync::Arc;

use super::node::NodeError;
use crate::platform::{RtspStreamConfig, StreamService};
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
        self.raw_loader.clone().ok_or_else(|| {
            NodeError::Precondition("raw frame loader is not configured".to_owned())
        })
    }

    /// 获取静态 raster 编解码器（图片加载/合成/检测转 PNG 时使用）。
    pub fn image_codec(&self) -> Result<Arc<dyn RasterImageCodec>, NodeError> {
        self.image_codec.clone().ok_or_else(|| {
            NodeError::Precondition("raster image codec is not configured".to_owned())
        })
    }
}
