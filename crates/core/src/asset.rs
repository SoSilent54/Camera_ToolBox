//! 会话内媒体资产及其完整性元数据。

use std::{collections::BTreeMap, sync::Arc};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 会话内资产的稳定标识。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AssetId(String);

impl AssetId {
    /// 创建非空资产标识。
    ///
    /// # Errors
    ///
    /// 标识为空或只包含空白时返回错误。
    pub fn new(value: impl Into<String>) -> Result<Self, AssetError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(AssetError::EmptyAssetId);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AssetId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 资产中媒体载荷的解释类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaFormat {
    RawPacked { bit_depth: u8 },
    RawU16Le { bit_depth: u8 },
    Jpeg,
    Png,
    Yuv420Sp { chroma_order: ChromaOrder },
    H264AnnexB,
    H265AnnexB,
    Binary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChromaOrder {
    Uv,
    Vu,
}

/// 单个有 stride 的 owned plane。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedMediaPlane {
    pub bytes: Arc<[u8]>,
    pub stride: usize,
    pub rows: usize,
}

impl OwnedMediaPlane {
    /// 构造完整 plane；buffer 必须精确覆盖 `stride * rows`。
    ///
    /// # Errors
    ///
    /// 尺寸溢出或 buffer 长度不匹配时返回错误。
    pub fn new(bytes: Arc<[u8]>, stride: usize, rows: usize) -> Result<Self, AssetError> {
        let expected = stride
            .checked_mul(rows)
            .ok_or(AssetError::PayloadSizeOverflow)?;
        if bytes.len() != expected {
            return Err(AssetError::PlaneLengthMismatch {
                expected,
                actual: bytes.len(),
            });
        }
        Ok(Self {
            bytes,
            stride,
            rows,
        })
    }
}

/// 网络接收完成后转移所有权的权威媒体载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedMediaPayload {
    Bytes(Arc<[u8]>),
    Planes(Vec<OwnedMediaPlane>),
}

impl OwnedMediaPayload {
    #[must_use]
    pub fn from_bytes(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self::Bytes(bytes.into())
    }

    /// 返回所有 source buffer 的总字节数。
    ///
    /// # Errors
    ///
    /// 多 plane 总长度超出 `usize` 时返回错误。
    pub fn byte_len(&self) -> Result<usize, AssetError> {
        match self {
            Self::Bytes(bytes) => Ok(bytes.len()),
            Self::Planes(planes) => planes.iter().try_fold(0usize, |total, plane| {
                total
                    .checked_add(plane.bytes.len())
                    .ok_or(AssetError::PayloadSizeOverflow)
            }),
        }
    }
}

/// 采集时固化的轻量媒体元数据；完整 source bytes 只存在于 payload。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureMetadata {
    pub format: MediaFormat,
    pub source_name: String,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

/// 完整性终态。只有 `Verified` 资产可发布到 `CaptureStore`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum IntegrityState {
    Unverified,
    Verified { algorithm: String, digest: String },
    Failed { reason: String },
}

impl IntegrityState {
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }
}

/// 会话内存中的不可变权威 source；本类型没有任何落盘路径或 spill 语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EphemeralAsset {
    pub id: AssetId,
    pub source: OwnedMediaPayload,
    pub metadata: CaptureMetadata,
    pub integrity: IntegrityState,
}

impl EphemeralAsset {
    /// 构造已完成接收的资产；发布前仍由 `CaptureStore` 校验预算和完整性。
    #[must_use]
    pub const fn new(
        id: AssetId,
        source: OwnedMediaPayload,
        metadata: CaptureMetadata,
        integrity: IntegrityState,
    ) -> Self {
        Self {
            id,
            source,
            metadata,
            integrity,
        }
    }

    /// 返回资产 source 的总字节数。
    ///
    /// # Errors
    ///
    /// 多 plane 总长度溢出时返回错误。
    pub fn byte_len(&self) -> Result<usize, AssetError> {
        self.source.byte_len()
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AssetError {
    #[error("asset id must not be empty")]
    EmptyAssetId,
    #[error("media payload size overflows the host address space")]
    PayloadSizeOverflow,
    #[error("media plane length mismatch: expected {expected}, got {actual}")]
    PlaneLengthMismatch { expected: usize, actual: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plane_requires_exact_stride_extent() {
        let error = OwnedMediaPlane::new(Arc::from([0_u8; 7]), 4, 2).unwrap_err();
        assert_eq!(
            error,
            AssetError::PlaneLengthMismatch {
                expected: 8,
                actual: 7
            }
        );
    }

    #[test]
    fn payload_accounts_all_planes() {
        let payload = OwnedMediaPayload::Planes(vec![
            OwnedMediaPlane::new(Arc::from([0_u8; 8]), 4, 2).unwrap(),
            OwnedMediaPlane::new(Arc::from([0_u8; 4]), 4, 1).unwrap(),
        ]);
        assert_eq!(payload.byte_len(), Ok(12));
    }
}
