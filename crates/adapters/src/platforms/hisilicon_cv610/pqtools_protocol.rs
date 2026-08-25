//! PQTools Still Dump 的纯请求编码与响应解析；本模块不创建 socket。

use std::io::{self, Read};

use camera_toolbox_app::{
    DumpEnvelope, DumpSourceDescriptor, DumpTruncationStage, VerifiedDumpKind,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const REQUEST_SIZE: usize = 128;
pub const RAW_METADATA_SIZE: usize = 152;
pub const JPEG_METADATA_SIZE: usize = 48;
pub const YUV_METADATA_SIZE: usize = 32;
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;
const REQUEST_CHECKSUM_OFFSET: usize = 124;
const MAX_PROGRESS_TOKENS: u8 = 64;
const MAX_ERROR_BYTES: usize = 1024;
const IO_CHUNK_BYTES: usize = 64 * 1024;

/// 编码四种且仅四种已验证的 128-byte 请求。
#[must_use]
pub fn encode_request(kind: VerifiedDumpKind) -> [u8; REQUEST_SIZE] {
    let mut packet = [0_u8; REQUEST_SIZE];
    packet[..4].copy_from_slice(&120_u32.to_le_bytes());
    packet[8..28].copy_from_slice(b"HIPQHi3516CV610\0\0\0\0\0");
    packet[28..44].copy_from_slice(b"1.0.0.1\0\0\0\0\0\0\0\0\0");
    put_word(&mut packet, 44, command_code(kind));
    put_word(&mut packet, 48, 1);
    match kind {
        VerifiedDumpKind::Raw10 | VerifiedDumpKind::Raw12 => {
            put_word(&mut packet, 52, 0xff09_3000);
            put_word(&mut packet, 60, 1);
            put_word(
                &mut packet,
                64,
                if kind == VerifiedDumpKind::Raw10 {
                    1
                } else {
                    2
                },
            );
            put_word(&mut packet, 80, 1);
        }
        VerifiedDumpKind::Jpeg => {
            put_word(&mut packet, 52, 0xff09_3302);
            put_word(&mut packet, 60, 1);
        }
        VerifiedDumpKind::Nv21 => {
            put_word(&mut packet, 52, 0xff09_1000);
            put_word(&mut packet, 60, 1);
            put_word(&mut packet, 72, 1);
        }
    }
    let checksum = packet[..REQUEST_CHECKSUM_OFFSET]
        .iter()
        .fold(0_u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
    put_word(&mut packet, REQUEST_CHECKSUM_OFFSET, checksum);
    packet
}

#[must_use]
pub const fn command_code(kind: VerifiedDumpKind) -> u32 {
    match kind {
        VerifiedDumpKind::Raw10 | VerifiedDumpKind::Raw12 => 0x66,
        VerifiedDumpKind::Jpeg => 0x85,
        VerifiedDumpKind::Nv21 => 0x65,
    }
}

fn put_word(packet: &mut [u8; REQUEST_SIZE], offset: usize, value: u32) {
    packet[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// metadata 已完成闭环，但尚未读取或分配 payload。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsePrefix {
    pub envelope: DumpEnvelope,
    pub descriptor: DumpSourceDescriptor,
    pub payload_length: usize,
    metadata_checksum: u32,
}

/// final payload buffer 原地填充和验证后的摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseVerification {
    pub payload_sha256: String,
    pub response_checksum: u32,
}

/// 先消费 `C0* -> D0 -> envelope -> type metadata`，在任何 payload 分配前完成长度闭环。
///
/// # Errors
///
/// marker、frame count、声明上限、metadata 或 type-specific 长度无效时返回 typed error。
pub fn read_response_prefix<R: Read>(
    reader: &mut R,
    kind: VerifiedDumpKind,
    max_response_bytes: usize,
) -> Result<ResponsePrefix, PqtoolsProtocolError> {
    let mut progress_tokens = 0_u8;
    loop {
        let marker = read_array::<1, _>(reader, DumpTruncationStage::Marker)?[0];
        match marker {
            0xc0 => {
                progress_tokens = progress_tokens.checked_add(1).ok_or(
                    PqtoolsProtocolError::TooManyProgressTokens {
                        limit: MAX_PROGRESS_TOKENS,
                    },
                )?;
                if progress_tokens > MAX_PROGRESS_TOKENS {
                    return Err(PqtoolsProtocolError::TooManyProgressTokens {
                        limit: MAX_PROGRESS_TOKENS,
                    });
                }
            }
            0xd0 => break,
            0xee => {
                return Err(PqtoolsProtocolError::ServerRejected(read_error_text(
                    reader,
                )?));
            }
            unknown => return Err(PqtoolsProtocolError::UnknownMarker(unknown)),
        }
    }

    let header = read_array::<8, _>(reader, DumpTruncationStage::Envelope)?;
    let frame_count = u32::from_le_bytes(header[..4].try_into().expect("four-byte frame count"));
    if frame_count != 1 {
        return Err(PqtoolsProtocolError::UnsupportedFrameCount(frame_count));
    }
    let block_u32 = u32::from_le_bytes(header[4..].try_into().expect("four-byte block length"));
    let block_length = usize::try_from(block_u32)
        .map_err(|_| PqtoolsProtocolError::LengthOverflow("block length"))?;
    if block_length < 4 {
        return Err(PqtoolsProtocolError::BlockLengthTooSmall(block_length));
    }
    if block_length > max_response_bytes {
        return Err(PqtoolsProtocolError::ResponseTooLarge {
            declared: block_length,
            limit: max_response_bytes,
        });
    }

    let metadata_size = metadata_size(kind);
    let minimum_block =
        metadata_size
            .checked_add(4)
            .ok_or(PqtoolsProtocolError::LengthOverflow(
                "metadata plus checksum",
            ))?;
    if block_length < minimum_block {
        return Err(PqtoolsProtocolError::MetadataDoesNotFit {
            block_length,
            metadata_size,
        });
    }

    let (descriptor, metadata_checksum) = match kind {
        VerifiedDumpKind::Raw10 | VerifiedDumpKind::Raw12 => {
            let metadata =
                read_array::<RAW_METADATA_SIZE, _>(reader, DumpTruncationStage::Metadata)?;
            (parse_raw_metadata(&metadata, kind)?, byte_sum(&metadata))
        }
        VerifiedDumpKind::Jpeg => {
            let metadata =
                read_array::<JPEG_METADATA_SIZE, _>(reader, DumpTruncationStage::Metadata)?;
            (parse_jpeg_metadata(&metadata)?, byte_sum(&metadata))
        }
        VerifiedDumpKind::Nv21 => {
            let metadata =
                read_array::<YUV_METADATA_SIZE, _>(reader, DumpTruncationStage::Metadata)?;
            (parse_yuv_metadata(&metadata)?, byte_sum(&metadata))
        }
    };
    let payload_length = descriptor
        .checked_payload_len()
        .map_err(|error| PqtoolsProtocolError::InvalidMetadata(error.to_string()))?;
    let expected_block = metadata_size
        .checked_add(payload_length)
        .and_then(|length| length.checked_add(4))
        .ok_or(PqtoolsProtocolError::LengthOverflow(
            "metadata-derived block length",
        ))?;
    if expected_block != block_length {
        return Err(PqtoolsProtocolError::BlockLengthMismatch {
            declared: block_length,
            expected: expected_block,
        });
    }

    Ok(ResponsePrefix {
        envelope: DumpEnvelope {
            progress_tokens,
            frame_count,
            block_length,
        },
        descriptor,
        payload_length,
        metadata_checksum,
    })
}

/// 将 payload 直接读入调用方提供的最终 buffer，并增量计算 checksum/SHA-256。
///
/// # Errors
///
/// buffer 长度不等于已闭环 payload、payload/checksum 截断、checksum 或 JPEG 边界无效时返回 typed error。
pub fn read_payload_and_checksum<R: Read>(
    reader: &mut R,
    prefix: &ResponsePrefix,
    payload: &mut [u8],
) -> Result<ResponseVerification, PqtoolsProtocolError> {
    if payload.len() != prefix.payload_length {
        return Err(PqtoolsProtocolError::FinalBufferLengthMismatch {
            expected: prefix.payload_length,
            actual: payload.len(),
        });
    }

    let mut checksum = prefix.metadata_checksum;
    let mut sha256 = Sha256::new();
    let mut received = 0_usize;
    while received < payload.len() {
        let end = received
            .checked_add(IO_CHUNK_BYTES.min(payload.len() - received))
            .ok_or(PqtoolsProtocolError::LengthOverflow("payload read offset"))?;
        match reader.read(&mut payload[received..end]) {
            Ok(0) => {
                return Err(PqtoolsProtocolError::Truncated {
                    stage: DumpTruncationStage::Payload,
                    expected: payload.len(),
                    received,
                });
            }
            Ok(count) => {
                let chunk = &payload[received..received + count];
                checksum = checksum.wrapping_add(byte_sum(chunk));
                sha256.update(chunk);
                received += count;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(PqtoolsProtocolError::Io {
                    stage: DumpTruncationStage::Payload,
                    source,
                });
            }
        }
    }

    let checksum_bytes = read_checksum(reader)?;
    let response_checksum = u32::from_le_bytes(checksum_bytes);
    if response_checksum != checksum {
        return Err(PqtoolsProtocolError::ChecksumMismatch {
            received: response_checksum,
            calculated: checksum,
        });
    }
    if let DumpSourceDescriptor::Jpeg { .. } = prefix.descriptor {
        if payload.get(..2) != Some(&[0xff, 0xd8]) {
            return Err(PqtoolsProtocolError::InvalidJpeg("missing SOI"));
        }
        if payload.get(payload.len().saturating_sub(2)..) != Some(&[0xff, 0xd9]) {
            return Err(PqtoolsProtocolError::InvalidJpeg("missing EOI"));
        }
    }

    Ok(ResponseVerification {
        payload_sha256: hex_digest(sha256.finalize().as_slice()),
        response_checksum,
    })
}

const fn metadata_size(kind: VerifiedDumpKind) -> usize {
    match kind {
        VerifiedDumpKind::Raw10 | VerifiedDumpKind::Raw12 => RAW_METADATA_SIZE,
        VerifiedDumpKind::Jpeg => JPEG_METADATA_SIZE,
        VerifiedDumpKind::Nv21 => YUV_METADATA_SIZE,
    }
}

fn parse_raw_metadata(
    metadata: &[u8; RAW_METADATA_SIZE],
    requested: VerifiedDumpKind,
) -> Result<DumpSourceDescriptor, PqtoolsProtocolError> {
    let words = words::<38>(metadata);
    let [width, height, stride_word, bit_enum, ..] = words;
    let bit_depth = match bit_enum {
        1 => 10,
        2 => 12,
        value => return Err(PqtoolsProtocolError::UnknownRawBitEnum(value)),
    };
    let requested_bits = requested
        .raw_bit_depth()
        .expect("RAW request variants always have a bit depth");
    if bit_depth != requested_bits {
        return Err(PqtoolsProtocolError::RawBitDepthMismatch {
            requested: requested_bits,
            received: bit_depth,
        });
    }
    if width == 0 || height == 0 {
        return Err(PqtoolsProtocolError::InvalidMetadata(format!(
            "RAW dimensions must be non-zero: {width}x{height}"
        )));
    }
    let width_host =
        usize::try_from(width).map_err(|_| PqtoolsProtocolError::LengthOverflow("RAW width"))?;
    let row_bits = width_host
        .checked_mul(usize::from(bit_depth))
        .ok_or(PqtoolsProtocolError::LengthOverflow("RAW packed row bits"))?;
    let minimum_stride = row_bits
        .checked_add(7)
        .ok_or(PqtoolsProtocolError::LengthOverflow(
            "RAW packed row rounding",
        ))?
        / 8;
    let stride = usize::try_from(stride_word)
        .map_err(|_| PqtoolsProtocolError::LengthOverflow("RAW stride"))?;
    if stride < minimum_stride {
        return Err(PqtoolsProtocolError::InvalidMetadata(format!(
            "RAW stride {stride} is smaller than packed row {minimum_stride}"
        )));
    }
    Ok(DumpSourceDescriptor::Raw {
        width,
        height,
        stride,
        bit_depth,
        metadata_words: words,
    })
}

fn parse_jpeg_metadata(
    metadata: &[u8; JPEG_METADATA_SIZE],
) -> Result<DumpSourceDescriptor, PqtoolsProtocolError> {
    if &metadata[..8] != b"OTSI_JPG" {
        return Err(PqtoolsProtocolError::InvalidMetadata(
            "JPEG metadata magic is not OTSI_JPG".to_owned(),
        ));
    }
    let width = u16::from_le_bytes(metadata[8..10].try_into().expect("JPEG width"));
    let height = u16::from_le_bytes(metadata[10..12].try_into().expect("JPEG height"));
    let payload_u32 = u32::from_le_bytes(metadata[12..16].try_into().expect("JPEG length"));
    if width == 0 || height == 0 {
        return Err(PqtoolsProtocolError::InvalidMetadata(format!(
            "JPEG dimensions must be non-zero: {width}x{height}"
        )));
    }
    if payload_u32 < 4 {
        return Err(PqtoolsProtocolError::InvalidMetadata(format!(
            "JPEG payload length is too small: {payload_u32}"
        )));
    }
    let payload_len = usize::try_from(payload_u32)
        .map_err(|_| PqtoolsProtocolError::LengthOverflow("JPEG payload length"))?;
    let mut reserved = [0_u8; 32];
    reserved.copy_from_slice(&metadata[16..]);
    Ok(DumpSourceDescriptor::Jpeg {
        width,
        height,
        payload_len,
        reserved,
    })
}

fn parse_yuv_metadata(
    metadata: &[u8; YUV_METADATA_SIZE],
) -> Result<DumpSourceDescriptor, PqtoolsProtocolError> {
    let words = words::<8>(metadata);
    let [
        width,
        height,
        y_stride_word,
        chroma_stride_word,
        _,
        _,
        format,
        _,
    ] = words;
    if width == 0 || height == 0 || height % 2 != 0 {
        return Err(PqtoolsProtocolError::InvalidMetadata(format!(
            "YUV420 dimensions must be non-zero with even height: {width}x{height}"
        )));
    }
    if format != 3 {
        return Err(PqtoolsProtocolError::UnknownYuvFormat(format));
    }
    let width_host =
        usize::try_from(width).map_err(|_| PqtoolsProtocolError::LengthOverflow("YUV width"))?;
    let y_stride = usize::try_from(y_stride_word)
        .map_err(|_| PqtoolsProtocolError::LengthOverflow("Y stride"))?;
    let chroma_stride = usize::try_from(chroma_stride_word)
        .map_err(|_| PqtoolsProtocolError::LengthOverflow("chroma stride"))?;
    if y_stride < width_host || chroma_stride < width_host {
        return Err(PqtoolsProtocolError::InvalidMetadata(format!(
            "YUV stride is smaller than width: width={width_host}, y={y_stride}, chroma={chroma_stride}"
        )));
    }
    Ok(DumpSourceDescriptor::Nv21 {
        width,
        height,
        y_stride,
        chroma_stride,
        metadata_words: words,
    })
}

fn words<const N: usize>(bytes: &[u8]) -> [u32; N] {
    std::array::from_fn(|index| {
        let offset = index * 4;
        u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("metadata contains exact word count"),
        )
    })
}

fn read_error_text<R: Read>(reader: &mut R) -> Result<String, PqtoolsProtocolError> {
    let mut message = Vec::with_capacity(64);
    for _ in 0..MAX_ERROR_BYTES {
        let byte = read_array::<1, _>(reader, DumpTruncationStage::ErrorText)?[0];
        if byte == 0 {
            return Ok(String::from_utf8_lossy(&message).into_owned());
        }
        message.push(byte);
    }
    Err(PqtoolsProtocolError::ErrorTextTooLong {
        limit: MAX_ERROR_BYTES,
    })
}

fn read_checksum<R: Read>(reader: &mut R) -> Result<[u8; 4], PqtoolsProtocolError> {
    let mut output = [0_u8; 4];
    let mut received = 0_usize;
    while received < output.len() {
        match reader.read(&mut output[received..]) {
            Ok(0) => return Err(PqtoolsProtocolError::PeerClosedBeforeChecksum { received }),
            Ok(count) => received += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(PqtoolsProtocolError::Io {
                    stage: DumpTruncationStage::Checksum,
                    source,
                });
            }
        }
    }
    Ok(output)
}

fn read_array<const N: usize, R: Read>(
    reader: &mut R,
    stage: DumpTruncationStage,
) -> Result<[u8; N], PqtoolsProtocolError> {
    let mut output = [0_u8; N];
    let mut received = 0_usize;
    while received < N {
        match reader.read(&mut output[received..]) {
            Ok(0) => {
                return Err(PqtoolsProtocolError::Truncated {
                    stage,
                    expected: N,
                    received,
                });
            }
            Ok(count) => received += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(source) => return Err(PqtoolsProtocolError::Io { stage, source }),
        }
    }
    Ok(output)
}

fn byte_sum(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0_u32, |sum, byte| sum.wrapping_add(u32::from(*byte)))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[derive(Debug, Error)]
pub enum PqtoolsProtocolError {
    #[error("response truncated in {stage:?}: expected {expected}, received {received}")]
    Truncated {
        stage: DumpTruncationStage,
        expected: usize,
        received: usize,
    },
    #[error("peer closed before checksum: received {received} of 4 bytes")]
    PeerClosedBeforeChecksum { received: usize },
    #[error("I/O failed in {stage:?}: {source}")]
    Io {
        stage: DumpTruncationStage,
        #[source]
        source: io::Error,
    },
    #[error("unknown response marker 0x{0:02x}")]
    UnknownMarker(u8),
    #[error("0xC0 progress token count exceeds {limit}")]
    TooManyProgressTokens { limit: u8 },
    #[error("server rejected dump: {0}")]
    ServerRejected(String),
    #[error("server error text exceeds {limit} bytes")]
    ErrorTextTooLong { limit: usize },
    #[error("only frame_count=1 is verified, received {0}")]
    UnsupportedFrameCount(u32),
    #[error("response block length is too small: {0}")]
    BlockLengthTooSmall(usize),
    #[error("declared response length {declared} exceeds {limit}")]
    ResponseTooLarge { declared: usize, limit: usize },
    #[error("block {block_length} cannot contain metadata {metadata_size} plus checksum")]
    MetadataDoesNotFit {
        block_length: usize,
        metadata_size: usize,
    },
    #[error("invalid metadata: {0}")]
    InvalidMetadata(String),
    #[error("unverified RAW bit enum {0}")]
    UnknownRawBitEnum(u32),
    #[error("RAW bit depth mismatch: requested {requested}, received {received}")]
    RawBitDepthMismatch { requested: u8, received: u8 },
    #[error("unverified YUV format enum {0}")]
    UnknownYuvFormat(u32),
    #[error("block length does not close: declared {declared}, expected {expected}")]
    BlockLengthMismatch { declared: usize, expected: usize },
    #[error("final payload buffer length mismatch: expected {expected}, got {actual}")]
    FinalBufferLengthMismatch { expected: usize, actual: usize },
    #[error("checksum mismatch: received 0x{received:08x}, calculated 0x{calculated:08x}")]
    ChecksumMismatch { received: u32, calculated: u32 },
    #[error("invalid JPEG payload: {0}")]
    InvalidJpeg(&'static str),
    #[error("{0} overflows host address space")]
    LengthOverflow(&'static str),
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, path::Path};

    use super::*;

    const REQUESTS: [(VerifiedDumpKind, &str); 4] = [
        (
            VerifiedDumpKind::Raw12,
            "78000000000000004849505148693335313643563631300000000000312e302e302e310000000000000000006600000001000000003009ff000000000100000002000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000049070000",
        ),
        (
            VerifiedDumpKind::Raw10,
            "78000000000000004849505148693335313643563631300000000000312e302e302e310000000000000000006600000001000000003009ff000000000100000001000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000048070000",
        ),
        (
            VerifiedDumpKind::Jpeg,
            "78000000000000004849505148693335313643563631300000000000312e302e302e310000000000000000008500000001000000023309ff00000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006a070000",
        ),
        (
            VerifiedDumpKind::Nv21,
            "78000000000000004849505148693335313643563631300000000000312e302e302e310000000000000000006500000001000000001009ff000000000100000000000000000000000100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000026070000",
        ),
    ];

    struct FragmentedReader {
        bytes: Cursor<Vec<u8>>,
        fragments: Vec<usize>,
        index: usize,
    }

    impl Read for FragmentedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let limit = self.fragments[self.index % self.fragments.len()];
            self.index += 1;
            let length = limit.min(buffer.len());
            self.bytes.read(&mut buffer[..length])
        }
    }

    #[test]
    fn request_bytes_match_all_pcap_vectors() {
        for (kind, expected) in REQUESTS {
            assert_eq!(encode_request(kind).as_slice(), decode_hex(expected));
        }
    }

    #[test]
    fn fragmented_jpeg_round_trip_uses_final_buffer() {
        let payload = [0xff, 0xd8, 7, 9, 0xff, 0xd9];
        let mut metadata = [0_u8; JPEG_METADATA_SIZE];
        metadata[..8].copy_from_slice(b"OTSI_JPG");
        metadata[8..10].copy_from_slice(&6_u16.to_le_bytes());
        metadata[10..12].copy_from_slice(&4_u16.to_le_bytes());
        metadata[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let response = envelope(&metadata, &payload, 1);
        let mut reader = FragmentedReader {
            bytes: Cursor::new(response),
            fragments: vec![1, 2, 3, 5, 8, 13],
            index: 0,
        };
        let prefix = read_response_prefix(
            &mut reader,
            VerifiedDumpKind::Jpeg,
            DEFAULT_MAX_RESPONSE_BYTES,
        )
        .unwrap();
        let mut final_payload = vec![0; prefix.payload_length];
        let verified = read_payload_and_checksum(&mut reader, &prefix, &mut final_payload).unwrap();
        assert_eq!(final_payload, payload);
        assert_eq!(prefix.envelope.progress_tokens, 1);
        assert_eq!(verified.payload_sha256.len(), 64);
    }

    #[test]
    fn embedded_python_raw_and_yuv_fixtures_are_deterministic() {
        for (kind, bit_enum, bit_depth) in [
            (VerifiedDumpKind::Raw10, 1_u32, 10_u8),
            (VerifiedDumpKind::Raw12, 2_u32, 12_u8),
        ] {
            let mut metadata = [0_u8; RAW_METADATA_SIZE];
            for (index, word) in [4_u32, 2, 8, bit_enum].into_iter().enumerate() {
                metadata[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
            }
            let payload: Vec<u8> = (0..16).collect();
            let mut reader = Cursor::new(envelope(&metadata, &payload, 2));
            let prefix = read_response_prefix(&mut reader, kind, 1024).unwrap();
            assert!(matches!(
                &prefix.descriptor,
                DumpSourceDescriptor::Raw {
                    width: 4,
                    height: 2,
                    stride: 8,
                    bit_depth: received,
                    ..
                } if *received == bit_depth
            ));
            let mut final_payload = vec![0; prefix.payload_length];
            let verified =
                read_payload_and_checksum(&mut reader, &prefix, &mut final_payload).unwrap();
            assert_eq!(final_payload, payload);
            assert_eq!(
                verified.payload_sha256,
                "be45cb2605bf36bebde684841a28f0fd43c69850a3dce5fedba69928ee3a8991"
            );
        }

        let mut metadata = [0_u8; YUV_METADATA_SIZE];
        for (index, word) in [4_u32, 2, 4, 4, 0, 0, 3, 0].into_iter().enumerate() {
            metadata[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        let payload: Vec<u8> = (0..12).collect();
        let mut reader = Cursor::new(envelope(&metadata, &payload, 0));
        let prefix = read_response_prefix(&mut reader, VerifiedDumpKind::Nv21, 1024).unwrap();
        assert!(matches!(
            &prefix.descriptor,
            DumpSourceDescriptor::Nv21 {
                width: 4,
                height: 2,
                y_stride: 4,
                chroma_stride: 4,
                ..
            }
        ));
        let mut final_payload = vec![0; prefix.payload_length];
        let verified = read_payload_and_checksum(&mut reader, &prefix, &mut final_payload).unwrap();
        assert_eq!(final_payload, payload);
        assert_eq!(
            verified.payload_sha256,
            "fff3a9bcdd37363d703c1c4f9512533686157868f0d4f16a0f02d0f1da24f9a2"
        );
    }

    #[test]
    fn malformed_stages_and_limits_are_distinct() {
        assert!(matches!(
            read_response_prefix(
                &mut Cursor::new(Vec::<u8>::new()),
                VerifiedDumpKind::Jpeg,
                1024
            ),
            Err(PqtoolsProtocolError::Truncated {
                stage: DumpTruncationStage::Marker,
                ..
            })
        ));
        assert!(matches!(
            read_response_prefix(
                &mut Cursor::new(vec![0xd0, 1, 0]),
                VerifiedDumpKind::Jpeg,
                1024
            ),
            Err(PqtoolsProtocolError::Truncated {
                stage: DumpTruncationStage::Envelope,
                ..
            })
        ));
        let mut multiple_frames = vec![0xd0];
        multiple_frames.extend_from_slice(&2_u32.to_le_bytes());
        multiple_frames.extend_from_slice(&4_u32.to_le_bytes());
        assert!(matches!(
            read_response_prefix(
                &mut Cursor::new(multiple_frames),
                VerifiedDumpKind::Jpeg,
                1024
            ),
            Err(PqtoolsProtocolError::UnsupportedFrameCount(2))
        ));
        assert!(matches!(
            read_response_prefix(&mut Cursor::new(vec![0xab]), VerifiedDumpKind::Jpeg, 1024),
            Err(PqtoolsProtocolError::UnknownMarker(0xab))
        ));
        let rejection = b"\xeerc mode inconformity!\0";
        assert!(matches!(
            read_response_prefix(
                &mut Cursor::new(rejection.as_slice()),
                VerifiedDumpKind::Raw12,
                1024
            ),
            Err(PqtoolsProtocolError::ServerRejected(message))
                if message == "rc mode inconformity!"
        ));
        let mut unterminated = vec![0xee];
        unterminated.extend(std::iter::repeat_n(b'x', MAX_ERROR_BYTES));
        assert!(matches!(
            read_response_prefix(&mut Cursor::new(unterminated), VerifiedDumpKind::Jpeg, 2048),
            Err(PqtoolsProtocolError::ErrorTextTooLong {
                limit: MAX_ERROR_BYTES
            })
        ));
        let mut too_many = vec![0xc0; 65];
        too_many.push(0xd0);
        assert!(matches!(
            read_response_prefix(&mut Cursor::new(too_many), VerifiedDumpKind::Jpeg, 1024),
            Err(PqtoolsProtocolError::TooManyProgressTokens { limit: 64 })
        ));
        let mut oversized = vec![0xd0];
        oversized.extend_from_slice(&1_u32.to_le_bytes());
        oversized.extend_from_slice(&2048_u32.to_le_bytes());
        assert!(matches!(
            read_response_prefix(&mut Cursor::new(oversized), VerifiedDumpKind::Jpeg, 1024),
            Err(PqtoolsProtocolError::ResponseTooLarge {
                declared: 2048,
                limit: 1024
            })
        ));
    }

    #[test]
    fn metadata_payload_checksum_failures_are_separate() {
        let payload = [0xff, 0xd8, 0xff, 0xd9];
        let mut metadata = [0_u8; JPEG_METADATA_SIZE];
        metadata[..8].copy_from_slice(b"OTSI_JPG");
        metadata[8..10].copy_from_slice(&4_u16.to_le_bytes());
        metadata[10..12].copy_from_slice(&2_u16.to_le_bytes());
        metadata[12..16].copy_from_slice(&4_u32.to_le_bytes());

        let mut short_metadata = vec![0xd0];
        short_metadata.extend_from_slice(&1_u32.to_le_bytes());
        short_metadata.extend_from_slice(&56_u32.to_le_bytes());
        short_metadata.extend_from_slice(&metadata[..10]);
        assert!(matches!(
            read_response_prefix(
                &mut Cursor::new(short_metadata),
                VerifiedDumpKind::Jpeg,
                1024
            ),
            Err(PqtoolsProtocolError::Truncated {
                stage: DumpTruncationStage::Metadata,
                ..
            })
        ));

        let mut mismatch = vec![0xd0];
        mismatch.extend_from_slice(&1_u32.to_le_bytes());
        mismatch.extend_from_slice(&57_u32.to_le_bytes());
        mismatch.extend_from_slice(&metadata);
        assert!(matches!(
            read_response_prefix(&mut Cursor::new(mismatch), VerifiedDumpKind::Jpeg, 1024),
            Err(PqtoolsProtocolError::BlockLengthMismatch { .. })
        ));

        let response = envelope(&metadata, &payload, 0);
        let prefix_len = 1 + 8 + JPEG_METADATA_SIZE;
        let mut prefix_reader = Cursor::new(response[..prefix_len].to_vec());
        let prefix =
            read_response_prefix(&mut prefix_reader, VerifiedDumpKind::Jpeg, 1024).unwrap();
        let mut short_payload = Cursor::new(payload[..2].to_vec());
        let mut output = vec![0; payload.len()];
        assert!(matches!(
            read_payload_and_checksum(&mut short_payload, &prefix, &mut output),
            Err(PqtoolsProtocolError::Truncated {
                stage: DumpTruncationStage::Payload,
                expected: 4,
                received: 2
            })
        ));

        let mut checksum_short = payload.to_vec();
        checksum_short.extend_from_slice(&[1, 2]);
        assert!(matches!(
            read_payload_and_checksum(&mut Cursor::new(checksum_short), &prefix, &mut output),
            Err(PqtoolsProtocolError::PeerClosedBeforeChecksum { received: 2 })
        ));

        let mut bad_checksum = payload.to_vec();
        bad_checksum.extend_from_slice(&0_u32.to_le_bytes());
        assert!(matches!(
            read_payload_and_checksum(&mut Cursor::new(bad_checksum), &prefix, &mut output),
            Err(PqtoolsProtocolError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn capture_replay_fixtures_match_python_oracle_when_available() {
        let root = Path::new("/media/psf/Home/Desktop/PQStream_Alternative/captures/dump_replay");
        let cases = [
            (
                VerifiedDumpKind::Raw12,
                "raw12.wire.bin",
                "a136d0b32c1e96946dbe5e658a83141dcbc9610069bd6e50c83eefe0f87e066e",
                2_078_720,
            ),
            (
                VerifiedDumpKind::Raw10,
                "raw10.wire.bin",
                "8ce0050902b5470c9ce0155b425cdd6918a646eb391dcd906d2e7c46074af9e9",
                1_730_560,
            ),
            (
                VerifiedDumpKind::Jpeg,
                "jpg.wire.bin",
                "220267bef5206d6ea9d476fc2b476a81238cdd61da949d8142b097cb81248206",
                544_891,
            ),
            (
                VerifiedDumpKind::Nv21,
                "yuv.wire.bin",
                "9cb056afd79bd0f5dbbbe0c417c5dc8e1ccaae82016eb9ee641abd69b9ccb4cc",
                2_073_600,
            ),
        ];
        if !root.is_dir() {
            return;
        }
        for (kind, file, hash, length) in cases {
            let mut reader = Cursor::new(std::fs::read(root.join(file)).unwrap());
            let prefix =
                read_response_prefix(&mut reader, kind, DEFAULT_MAX_RESPONSE_BYTES).unwrap();
            assert_eq!(prefix.payload_length, length);
            let mut payload = vec![0; length];
            let verified = read_payload_and_checksum(&mut reader, &prefix, &mut payload).unwrap();
            assert_eq!(verified.payload_sha256, hash);
        }
    }

    fn envelope(metadata: &[u8], payload: &[u8], progress: usize) -> Vec<u8> {
        let mut output = vec![0xc0; progress];
        output.push(0xd0);
        output.extend_from_slice(&1_u32.to_le_bytes());
        output.extend_from_slice(&((metadata.len() + payload.len() + 4) as u32).to_le_bytes());
        output.extend_from_slice(metadata);
        output.extend_from_slice(payload);
        let checksum = byte_sum(metadata).wrapping_add(byte_sum(payload));
        output.extend_from_slice(&checksum.to_le_bytes());
        output
    }

    fn decode_hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(text, 16).unwrap()
            })
            .collect()
    }
}
