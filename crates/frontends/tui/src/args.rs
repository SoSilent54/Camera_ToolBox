use std::{num::NonZeroU64, path::PathBuf};

use camera_toolbox_core::{ChromaOrder, MediaFormat};
use clap::{Parser, ValueEnum};

/// Camera Toolbox 平台控制台参数；所有会写文件的 stream 目标都必须显式传入。
#[derive(Debug, Clone, Parser)]
#[command(name = "camera-toolbox-tui")]
#[command(about = "Interactive Camera Toolbox platform console")]
pub struct Args {
    /// Versioned profile-store JSON; defaults to the per-user project path.
    #[arg(long)]
    pub profile_store: Option<PathBuf>,
    /// Render a deterministic text snapshot without touching the terminal.
    #[arg(long)]
    pub snapshot: bool,
    /// Finite stream duration. Recording stays disabled unless quota and destinations are explicit.
    #[arg(long, requires = "stream_quota_bytes")]
    pub stream_duration_secs: Option<NonZeroU64>,
    /// Aggregate stream recording quota across enabled branches.
    #[arg(long, requires = "stream_duration_secs")]
    pub stream_quota_bytes: Option<NonZeroU64>,
    /// Explicit transport-evidence recording destination.
    #[arg(long)]
    pub transport_destination: Option<PathBuf>,
    /// Explicit Annex-B recording destination; requires --timestamp-destination.
    #[arg(long, requires = "timestamp_destination")]
    pub annexb_destination: Option<PathBuf>,
    /// Explicit RTP timestamp sidecar; requires --annexb-destination.
    #[arg(long, requires = "annexb_destination")]
    pub timestamp_destination: Option<PathBuf>,
    /// Explicit allowlisted SSH capture format supplied to the profile recipe.
    #[arg(long)]
    pub capture_format: Option<String>,
    /// Exact remote artifact path used only by the typed fetch action.
    #[arg(long)]
    pub remote_path: Option<String>,
    /// Explicit media interpretation used by remote fetch/watch and capture discovery.
    #[arg(long, value_enum)]
    pub remote_format: Option<RemoteFormatArg>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            profile_store: None,
            snapshot: false,
            stream_duration_secs: None,
            stream_quota_bytes: None,
            transport_destination: None,
            annexb_destination: None,
            timestamp_destination: None,
            capture_format: None,
            remote_path: None,
            remote_format: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RemoteFormatArg {
    Raw10Packed,
    Raw12Packed,
    Raw10U16le,
    Raw12U16le,
    Jpeg,
    Nv12,
    Nv21,
    H264,
    H265,
    Binary,
}

impl From<RemoteFormatArg> for MediaFormat {
    fn from(value: RemoteFormatArg) -> Self {
        match value {
            RemoteFormatArg::Raw10Packed => Self::RawPacked { bit_depth: 10 },
            RemoteFormatArg::Raw12Packed => Self::RawPacked { bit_depth: 12 },
            RemoteFormatArg::Raw10U16le => Self::RawU16Le { bit_depth: 10 },
            RemoteFormatArg::Raw12U16le => Self::RawU16Le { bit_depth: 12 },
            RemoteFormatArg::Jpeg => Self::Jpeg,
            RemoteFormatArg::Nv12 => Self::Yuv420Sp {
                chroma_order: ChromaOrder::Uv,
            },
            RemoteFormatArg::Nv21 => Self::Yuv420Sp {
                chroma_order: ChromaOrder::Vu,
            },
            RemoteFormatArg::H264 => Self::H264AnnexB,
            RemoteFormatArg::H265 => Self::H265AnnexB,
            RemoteFormatArg::Binary => Self::Binary,
        }
    }
}
