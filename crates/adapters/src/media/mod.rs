//! 媒体 adapter。

#[cfg(feature = "platform-cv610")]
pub mod ffmpeg_decoder;
pub mod ffmpeg_rtsp;
pub mod rtsp_stream_service;

#[cfg(feature = "platform-cv610")]
pub use ffmpeg_decoder::{DecoderOfferError, DecoderStats, FfmpegDecoder, FfmpegDecoderError};
pub use ffmpeg_rtsp::{FfmpegRtspDecoder, FfmpegRtspDecoderError, FfmpegRtspTransport};
pub use rtsp_stream_service::FfmpegRtspStreamService;
