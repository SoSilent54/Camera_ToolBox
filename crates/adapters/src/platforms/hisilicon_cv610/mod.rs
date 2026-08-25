//! HiSilicon CV610 的已验证 PQTools Still Dump provider。

pub mod dump_service;
pub mod pqstream_protocol;
pub mod pqtools_protocol;
pub mod provider;
pub mod stream_service;

pub use dump_service::{
    Cv610DumpEndpoint, Cv610DumpService, ValidatedDumpInitializer, ValidatedInitializerRegistry,
};
pub use pqstream_protocol::{
    AccessUnit, AccessUnitAssembler, DEFAULT_MAX_HEADER_BYTES, DEFAULT_MAX_MEDIA_DESCRIPTION_BYTES,
    DEFAULT_MAX_RECORD_BYTES, DEFAULT_RTP_CONFIRMATION_PACKETS, H26xDepacketizer, HttpLikeResponse,
    MediaDescription, MediaRequest, PqRecord, PqStreamProtocolError, PreviewResync,
    RecordTruncationStage, RtpPacket, RtpValidator, VideoCodec, parse_http_response,
    parse_media_description, read_pq_record,
};
pub use pqtools_protocol::{
    DEFAULT_MAX_RESPONSE_BYTES, JPEG_METADATA_SIZE, PqtoolsProtocolError, RAW_METADATA_SIZE,
    REQUEST_SIZE, ResponsePrefix, ResponseVerification, YUV_METADATA_SIZE, command_code,
    encode_request, read_payload_and_checksum, read_response_prefix,
};
pub use provider::{Cv610ProviderError, HisiliconCv610Provider};
pub use stream_service::{
    Cv610StreamEndpoint, Cv610StreamService, DEFAULT_DECODER_INPUT_CAPACITY,
    DEFAULT_PREVIEW_AU_CAPACITY, DEFAULT_RECORDER_QUEUE_BYTES,
};
