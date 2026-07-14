//! 媒体 adapter。

pub mod ffmpeg_decoder;

pub use ffmpeg_decoder::{DecoderOfferError, DecoderStats, FfmpegDecoder, FfmpegDecoderError};
