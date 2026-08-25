//! CV610 PQStream 的无 I/O 协议层：精确请求、媒体描述、PQ record、RTP 与 H.26x 组包。

use std::io::{self, Read};

use thiserror::Error;

pub const START_CODE: &[u8; 4] = b"\x00\x00\x00\x01";
pub const DEFAULT_USER_AGENT: &str = "Streaming Media Client/1.0.0(Apr  2 2025)";
pub const DEFAULT_TRANSPORT: &str = "RTP/AVP/TCP;unicast;interleaved=0-1";
pub const DEFAULT_MAX_HEADER_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_MEDIA_DESCRIPTION_BYTES: usize = 4 * 1024;
pub const DEFAULT_MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_RTP_CONFIRMATION_PACKETS: usize = 3;

#[derive(Debug, Error)]
pub enum PqStreamProtocolError {
    #[error("invalid PQStream request: {0}")]
    InvalidRequest(String),
    #[error("HTTP-like response is invalid: {0}")]
    InvalidResponse(String),
    #[error("media description is invalid: {0}")]
    InvalidMediaDescription(String),
    #[error("unsupported PQStream framing: {0}")]
    UnsupportedFraming(String),
    #[error("PQStream record is truncated in {stage:?}: expected {expected}, received {received}")]
    TruncatedRecord {
        stage: RecordTruncationStage,
        expected: usize,
        received: usize,
    },
    #[error("PQStream record length {declared} is outside [{minimum}, {maximum}]")]
    InvalidRecordLength {
        declared: usize,
        minimum: usize,
        maximum: usize,
    },
    #[error("RTP packet is invalid: {0}")]
    InvalidRtp(String),
    #[error("H.26x payload is invalid: {0}")]
    InvalidH26x(String),
    #[error("PQStream I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordTruncationStage {
    Header,
    Payload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRequest {
    pub host: String,
    pub port: u16,
    pub channel: u16,
    pub media: String,
    pub cseq: u32,
}

impl MediaRequest {
    pub fn to_bytes(&self) -> Result<Vec<u8>, PqStreamProtocolError> {
        if self.host.trim().is_empty() || contains_line_break(&self.host) {
            return Err(PqStreamProtocolError::InvalidRequest(
                "host must be non-empty and contain no line breaks".to_owned(),
            ));
        }
        if self.port == 0 {
            return Err(PqStreamProtocolError::InvalidRequest(
                "port must be non-zero".to_owned(),
            ));
        }
        if self.channel > u8::MAX.into() {
            return Err(PqStreamProtocolError::InvalidRequest(
                "channel must fit one interleaved byte".to_owned(),
            ));
        }
        if self.media.trim().is_empty()
            || contains_line_break(&self.media)
            || !self
                .media
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(PqStreamProtocolError::InvalidRequest(
                "media must be a non-empty URI-safe token".to_owned(),
            ));
        }
        Ok(format!(
            "GET http://{}:{}/livestream/{}?action=play&media={} HTTP/1.1\r\n\
Cseq: {}\r\n\
User-Agent: {DEFAULT_USER_AGENT}\r\n\
Connection: Keep-Alive\r\n\
Cache-Control: no-cache\r\n\
Transport: {DEFAULT_TRANSPORT}\r\n\r\n",
            self.host, self.port, self.channel, self.media, self.cseq
        )
        .into_bytes())
    }
}

const fn contains_line_break(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if matches!(bytes[index], b'\r' | b'\n') {
            return true;
        }
        index += 1;
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpLikeResponse {
    pub status_code: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub raw_header: Vec<u8>,
}

impl HttpLikeResponse {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub fn parse_http_response(raw: &[u8]) -> Result<HttpLikeResponse, PqStreamProtocolError> {
    if raw.len() > DEFAULT_MAX_HEADER_BYTES {
        return Err(PqStreamProtocolError::InvalidResponse(format!(
            "header exceeds {} bytes",
            DEFAULT_MAX_HEADER_BYTES
        )));
    }
    if !raw.ends_with(b"\r\n\r\n") {
        return Err(PqStreamProtocolError::InvalidResponse(
            "header does not end with CRLF CRLF".to_owned(),
        ));
    }
    let text = std::str::from_utf8(raw).map_err(|_| {
        PqStreamProtocolError::InvalidResponse("header is not valid ASCII/UTF-8".to_owned())
    })?;
    if !text.is_ascii() {
        return Err(PqStreamProtocolError::InvalidResponse(
            "header is not ASCII".to_owned(),
        ));
    }
    let mut lines = text.split("\r\n");
    let status = lines.next().unwrap_or_default();
    let mut parts = status.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(PqStreamProtocolError::InvalidResponse(
            "invalid status line".to_owned(),
        ));
    }
    let status_code = parts
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| PqStreamProtocolError::InvalidResponse("invalid status code".to_owned()))?;
    let reason = parts.next().unwrap_or_default().to_owned();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or_else(|| {
            PqStreamProtocolError::InvalidResponse(format!("invalid header field {line:?}"))
        })?;
        let name = name.trim();
        if name.is_empty() {
            return Err(PqStreamProtocolError::InvalidResponse(
                "empty header name".to_owned(),
            ));
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok(HttpLikeResponse {
        status_code,
        reason,
        headers,
        raw_header: raw.to_vec(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
}

impl VideoCodec {
    #[must_use]
    pub const fn ffmpeg_input_format(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "hevc",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDescription {
    pub payload_type: u8,
    pub codec: VideoCodec,
    pub clock_rate: u32,
    pub width: u32,
    pub height: u32,
    pub frame_rate: u32,
    pub bitrate_value: u32,
    pub transport: String,
    pub rtp_channel: u8,
    pub control_channel: u8,
    pub ssrc: Option<u32>,
    pub raw: Vec<u8>,
}

fn parse_media_description_impl(raw: &[u8]) -> Result<MediaDescription, PqStreamProtocolError> {
    let text = std::str::from_utf8(raw).map_err(|_| {
        PqStreamProtocolError::InvalidMediaDescription("description is not UTF-8".to_owned())
    })?;
    let mut lines = text.split("\r\n").filter(|line| !line.is_empty());
    let media_line = lines.next().ok_or_else(|| {
        PqStreamProtocolError::InvalidMediaDescription("missing m=video line".to_owned())
    })?;
    let mut tokens = media_line.split_ascii_whitespace();
    if tokens.next() != Some("m=video") {
        return Err(PqStreamProtocolError::InvalidMediaDescription(
            "first field is not m=video".to_owned(),
        ));
    }
    let payload_type = parse_number::<u8>(tokens.next(), "payload type")?;
    if payload_type > 127 {
        return Err(PqStreamProtocolError::InvalidMediaDescription(
            "payload type exceeds 7 bits".to_owned(),
        ));
    }
    let format = tokens.next().ok_or_else(|| {
        PqStreamProtocolError::InvalidMediaDescription("missing media format".to_owned())
    })?;
    if tokens.next().is_some() {
        return Err(PqStreamProtocolError::InvalidMediaDescription(
            "m=video contains unexpected fields".to_owned(),
        ));
    }
    let fields: Vec<_> = format.split('/').collect();
    if fields.len() != 6 {
        return Err(PqStreamProtocolError::InvalidMediaDescription(
            "format must be codec/clock/width/height/fps/bitrate".to_owned(),
        ));
    }
    let codec = match fields[0].to_ascii_lowercase().as_str() {
        "h264" => VideoCodec::H264,
        "h265" => VideoCodec::H265,
        other => {
            return Err(PqStreamProtocolError::InvalidMediaDescription(format!(
                "unsupported codec {other}"
            )));
        }
    };
    let clock_rate = parse_positive(fields[1], "clock rate")?;
    let width = parse_positive(fields[2], "width")?;
    let height = parse_positive(fields[3], "height")?;
    let frame_rate = parse_positive(fields[4], "frame rate")?;
    let bitrate_value = fields[5].parse::<u32>().map_err(|_| {
        PqStreamProtocolError::InvalidMediaDescription("invalid bitrate-like field".to_owned())
    })?;
    let transport = lines
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("transport")
                    .then(|| value.trim().to_owned())
            })
        })
        .ok_or_else(|| {
            PqStreamProtocolError::InvalidMediaDescription("missing Transport field".to_owned())
        })?;
    let (rtp_channel, control_channel) = parse_interleaved(&transport)?;
    let ssrc = parse_ssrc(&transport)?;
    Ok(MediaDescription {
        payload_type,
        codec,
        clock_rate,
        width,
        height,
        frame_rate,
        bitrate_value,
        transport,
        rtp_channel,
        control_channel,
        ssrc,
        raw: raw.to_vec(),
    })
}

fn parse_number<T>(value: Option<&str>, name: &str) -> Result<T, PqStreamProtocolError>
where
    T: std::str::FromStr,
{
    value
        .and_then(|value| value.parse::<T>().ok())
        .ok_or_else(|| PqStreamProtocolError::InvalidMediaDescription(format!("invalid {name}")))
}

fn parse_positive(value: &str, name: &str) -> Result<u32, PqStreamProtocolError> {
    let value = value
        .parse::<u32>()
        .map_err(|_| PqStreamProtocolError::InvalidMediaDescription(format!("invalid {name}")))?;
    if value == 0 {
        return Err(PqStreamProtocolError::InvalidMediaDescription(format!(
            "{name} must be positive"
        )));
    }
    Ok(value)
}

fn parse_interleaved(transport: &str) -> Result<(u8, u8), PqStreamProtocolError> {
    let value = parameter(transport, "interleaved").ok_or_else(|| {
        PqStreamProtocolError::InvalidMediaDescription(
            "Transport lacks interleaved range".to_owned(),
        )
    })?;
    let (rtp, control) = value.split_once('-').ok_or_else(|| {
        PqStreamProtocolError::InvalidMediaDescription("invalid interleaved range".to_owned())
    })?;
    let rtp = rtp.trim().parse::<u8>().map_err(|_| {
        PqStreamProtocolError::InvalidMediaDescription("invalid RTP channel".to_owned())
    })?;
    let control = control.trim().parse::<u8>().map_err(|_| {
        PqStreamProtocolError::InvalidMediaDescription("invalid control channel".to_owned())
    })?;
    Ok((rtp, control))
}

fn parse_ssrc(transport: &str) -> Result<Option<u32>, PqStreamProtocolError> {
    let Some(value) = parameter(transport, "ssrc") else {
        return Ok(None);
    };
    let value = value.trim();
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(PqStreamProtocolError::InvalidMediaDescription(
            "SSRC must be exactly eight hexadecimal digits".to_owned(),
        ));
    }
    u32::from_str_radix(value, 16)
        .map(Some)
        .map_err(|_| PqStreamProtocolError::InvalidMediaDescription("invalid SSRC".to_owned()))
}

fn parameter<'a>(transport: &'a str, name: &str) -> Option<&'a str> {
    transport.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then_some(value.trim())
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PqRecord {
    pub channel: u8,
    pub raw_header: [u8; 8],
    pub packet: Vec<u8>,
}

pub fn read_pq_record<R: Read>(
    reader: &mut R,
    expected_channel: u8,
    max_record_bytes: usize,
) -> Result<Option<PqRecord>, PqStreamProtocolError> {
    let mut header = [0_u8; 8];
    let received = read_some_exact(reader, &mut header[..1])?;
    if received == 0 {
        return Ok(None);
    }
    let tail_received = read_some_exact(reader, &mut header[1..])?;
    if tail_received != 7 {
        return Err(PqStreamProtocolError::TruncatedRecord {
            stage: RecordTruncationStage::Header,
            expected: 8,
            received: 1 + tail_received,
        });
    }
    if header[0] != b'$' || header[2..4] != [0x80, 0x00] {
        return Err(PqStreamProtocolError::UnsupportedFraming(format!(
            "header={}",
            hex(&header)
        )));
    }
    if header[1] != expected_channel {
        return Err(PqStreamProtocolError::UnsupportedFraming(format!(
            "unexpected channel {}, expected {expected_channel}",
            header[1]
        )));
    }
    let declared = usize::try_from(u32::from_be_bytes(
        header[4..8].try_into().expect("fixed slice"),
    ))
    .map_err(|_| PqStreamProtocolError::InvalidRecordLength {
        declared: usize::MAX,
        minimum: 12,
        maximum: max_record_bytes,
    })?;
    if !(12..=max_record_bytes).contains(&declared) {
        return Err(PqStreamProtocolError::InvalidRecordLength {
            declared,
            minimum: 12,
            maximum: max_record_bytes,
        });
    }
    let mut packet = vec![0_u8; declared];
    let payload_received = read_some_exact(reader, &mut packet)?;
    if payload_received != declared {
        return Err(PqStreamProtocolError::TruncatedRecord {
            stage: RecordTruncationStage::Payload,
            expected: declared,
            received: payload_received,
        });
    }
    Ok(Some(PqRecord {
        channel: header[1],
        raw_header: header,
        packet,
    }))
}

fn read_some_exact<R: Read>(reader: &mut R, buffer: &mut [u8]) -> io::Result<usize> {
    let mut received = 0;
    while received < buffer.len() {
        match reader.read(&mut buffer[received..]) {
            Ok(0) => break,
            Ok(bytes) => received += bytes,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(received)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpPacket {
    pub sequence: u16,
    pub timestamp: u32,
    pub marker: bool,
    pub payload_type: u8,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

impl RtpPacket {
    pub fn parse(packet: &[u8]) -> Result<Self, PqStreamProtocolError> {
        if packet.len() < 12 {
            return Err(PqStreamProtocolError::InvalidRtp(
                "packet is shorter than the fixed header".to_owned(),
            ));
        }
        let first = packet[0];
        if first >> 6 != 2 {
            return Err(PqStreamProtocolError::InvalidRtp(format!(
                "version {} is not RTP v2",
                first >> 6
            )));
        }
        let csrc_count = usize::from(first & 0x0f);
        let mut offset = 12_usize
            .checked_add(csrc_count.checked_mul(4).ok_or_else(|| {
                PqStreamProtocolError::InvalidRtp("CSRC length overflow".to_owned())
            })?)
            .ok_or_else(|| PqStreamProtocolError::InvalidRtp("CSRC offset overflow".to_owned()))?;
        if offset > packet.len() {
            return Err(PqStreamProtocolError::InvalidRtp(
                "CSRC list is truncated".to_owned(),
            ));
        }
        if first & 0x10 != 0 {
            if offset + 4 > packet.len() {
                return Err(PqStreamProtocolError::InvalidRtp(
                    "extension header is truncated".to_owned(),
                ));
            }
            let words = usize::from(u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]));
            offset = offset
                .checked_add(4)
                .and_then(|value| value.checked_add(words.checked_mul(4)?))
                .ok_or_else(|| {
                    PqStreamProtocolError::InvalidRtp("extension length overflow".to_owned())
                })?;
            if offset > packet.len() {
                return Err(PqStreamProtocolError::InvalidRtp(
                    "extension data is truncated".to_owned(),
                ));
            }
        }
        let mut end = packet.len();
        if first & 0x20 != 0 {
            let padding = usize::from(*packet.last().expect("fixed header exists"));
            if padding == 0 || padding > end.saturating_sub(offset) {
                return Err(PqStreamProtocolError::InvalidRtp(
                    "padding length is invalid".to_owned(),
                ));
            }
            end -= padding;
        }
        if end <= offset {
            return Err(PqStreamProtocolError::InvalidRtp(
                "packet has no media payload".to_owned(),
            ));
        }
        Ok(Self {
            sequence: u16::from_be_bytes([packet[2], packet[3]]),
            timestamp: u32::from_be_bytes(packet[4..8].try_into().expect("fixed slice")),
            marker: packet[1] & 0x80 != 0,
            payload_type: packet[1] & 0x7f,
            ssrc: u32::from_be_bytes(packet[8..12].try_into().expect("fixed slice")),
            payload: packet[offset..end].to_vec(),
        })
    }
}

pub struct RtpValidator {
    confirmation_packets: usize,
    expected_payload_type: u8,
    ssrc: Option<u32>,
    candidate: Vec<RtpPacket>,
    confirmed: bool,
    last_sequence: Option<u16>,
    gaps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedRtp {
    pub packets: Vec<RtpPacket>,
    pub had_gap: bool,
    pub missing_packets: u16,
}

impl RtpValidator {
    pub fn new(
        confirmation_packets: usize,
        expected_payload_type: u8,
        expected_ssrc: Option<u32>,
    ) -> Result<Self, PqStreamProtocolError> {
        if !(2..=16).contains(&confirmation_packets) {
            return Err(PqStreamProtocolError::InvalidRtp(
                "confirmation count must be in [2, 16]".to_owned(),
            ));
        }
        if expected_payload_type > 127 {
            return Err(PqStreamProtocolError::InvalidRtp(
                "payload type exceeds 7 bits".to_owned(),
            ));
        }
        Ok(Self {
            confirmation_packets,
            expected_payload_type,
            ssrc: expected_ssrc,
            candidate: Vec::with_capacity(confirmation_packets),
            confirmed: false,
            last_sequence: None,
            gaps: 0,
        })
    }

    #[must_use]
    pub const fn confirmed(&self) -> bool {
        self.confirmed
    }

    #[must_use]
    pub const fn gaps(&self) -> u64 {
        self.gaps
    }

    pub fn accept(&mut self, packet: RtpPacket) -> Result<ValidatedRtp, PqStreamProtocolError> {
        if packet.payload_type != self.expected_payload_type {
            return Err(PqStreamProtocolError::InvalidRtp(format!(
                "payload type mismatch: expected {}, got {}",
                self.expected_payload_type, packet.payload_type
            )));
        }
        match self.ssrc {
            Some(ssrc) if ssrc != packet.ssrc => {
                return Err(PqStreamProtocolError::InvalidRtp(format!(
                    "SSRC changed: expected {ssrc:08x}, got {:08x}",
                    packet.ssrc
                )));
            }
            None => self.ssrc = Some(packet.ssrc),
            Some(_) => {}
        }
        if self.confirmed {
            let expected = self
                .last_sequence
                .expect("confirmed validator has a sequence")
                .wrapping_add(1);
            let had_gap = packet.sequence != expected;
            let missing_packets = packet.sequence.wrapping_sub(expected);
            if had_gap {
                self.gaps = self.gaps.saturating_add(u64::from(missing_packets.max(1)));
            }
            self.last_sequence = Some(packet.sequence);
            return Ok(ValidatedRtp {
                packets: vec![packet],
                had_gap,
                missing_packets,
            });
        }
        if self
            .candidate
            .last()
            .is_some_and(|last| packet.sequence != last.sequence.wrapping_add(1))
        {
            self.candidate.clear();
        }
        self.candidate.push(packet);
        if self.candidate.len() < self.confirmation_packets {
            return Ok(ValidatedRtp {
                packets: Vec::new(),
                had_gap: false,
                missing_packets: 0,
            });
        }
        self.confirmed = true;
        let packets = std::mem::take(&mut self.candidate);
        self.last_sequence = packets.last().map(|packet| packet.sequence);
        Ok(ValidatedRtp {
            packets,
            had_gap: false,
            missing_packets: 0,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NalUnit {
    pub annexb: Vec<u8>,
    pub nal_type: u8,
}

pub enum H26xDepacketizer {
    H264(H264Depacketizer),
    H265(H265Depacketizer),
}

impl H26xDepacketizer {
    #[must_use]
    pub fn new(codec: VideoCodec) -> Self {
        match codec {
            VideoCodec::H264 => Self::H264(H264Depacketizer::default()),
            VideoCodec::H265 => Self::H265(H265Depacketizer::default()),
        }
    }

    pub fn discard_incomplete_fragment(&mut self) {
        match self {
            Self::H264(value) => value.discard_incomplete_fragment(),
            Self::H265(value) => value.discard_incomplete_fragment(),
        }
    }

    pub fn push(&mut self, packet: &RtpPacket) -> Result<Vec<NalUnit>, PqStreamProtocolError> {
        if let Some((offset, payload)) = strip_annexb(&packet.payload) {
            let nal_type = match self {
                Self::H264(_) => payload.first().map(|byte| byte & 0x1f),
                Self::H265(_) => payload.first().map(|byte| (byte >> 1) & 0x3f),
            }
            .ok_or_else(|| PqStreamProtocolError::InvalidH26x("empty Annex-B NAL".to_owned()))?;
            let _ = offset;
            return Ok(vec![NalUnit {
                annexb: packet.payload.clone(),
                nal_type,
            }]);
        }
        match self {
            Self::H264(value) => value.push(packet),
            Self::H265(value) => value.push(packet),
        }
    }
}

fn strip_annexb(payload: &[u8]) -> Option<(usize, &[u8])> {
    if payload.starts_with(START_CODE) {
        Some((4, &payload[4..]))
    } else if payload.starts_with(b"\x00\x00\x01") {
        Some((3, &payload[3..]))
    } else {
        None
    }
}

#[derive(Default)]
pub struct H264Depacketizer {
    fragment: Option<Fragment>,
}

impl H264Depacketizer {
    pub fn discard_incomplete_fragment(&mut self) {
        self.fragment = None;
    }

    pub fn push(&mut self, packet: &RtpPacket) -> Result<Vec<NalUnit>, PqStreamProtocolError> {
        let payload = &packet.payload;
        let first = *payload.first().ok_or_else(|| {
            PqStreamProtocolError::InvalidH26x("empty H.264 RTP payload".to_owned())
        })?;
        let nal_type = first & 0x1f;
        if (1..=23).contains(&nal_type) {
            self.fragment = None;
            return Ok(vec![annexb_unit(payload, nal_type)]);
        }
        if nal_type == 24 {
            self.fragment = None;
            return parse_aggregation(&payload[1..], VideoCodec::H264);
        }
        if nal_type != 28 || payload.len() < 3 {
            return Err(PqStreamProtocolError::InvalidH26x(format!(
                "unsupported or truncated H.264 NAL type {nal_type}"
            )));
        }
        let header = payload[1];
        let start = header & 0x80 != 0;
        let end = header & 0x40 != 0;
        let original_type = header & 0x1f;
        let fragment = &payload[2..];
        if fragment.is_empty() || (start && end) {
            self.fragment = None;
            return Err(PqStreamProtocolError::InvalidH26x(
                "invalid H.264 FU-A flags or empty fragment".to_owned(),
            ));
        }
        if start {
            let mut bytes = Vec::with_capacity(1 + fragment.len());
            bytes.push((first & 0xe0) | original_type);
            bytes.extend_from_slice(fragment);
            self.fragment = Some(Fragment {
                timestamp: packet.timestamp,
                nal_type: original_type,
                bytes,
            });
            return Ok(Vec::new());
        }
        let Some(current) = self.fragment.as_mut() else {
            return Ok(Vec::new());
        };
        if current.timestamp != packet.timestamp || current.nal_type != original_type {
            self.fragment = None;
            return Ok(Vec::new());
        }
        current.bytes.extend_from_slice(fragment);
        if !end {
            return Ok(Vec::new());
        }
        let current = self.fragment.take().expect("fragment exists");
        Ok(vec![annexb_unit(&current.bytes, original_type)])
    }
}

#[derive(Default)]
pub struct H265Depacketizer {
    fragment: Option<Fragment>,
}

impl H265Depacketizer {
    pub fn discard_incomplete_fragment(&mut self) {
        self.fragment = None;
    }

    pub fn push(&mut self, packet: &RtpPacket) -> Result<Vec<NalUnit>, PqStreamProtocolError> {
        let payload = &packet.payload;
        if payload.len() < 2 {
            return Err(PqStreamProtocolError::InvalidH26x(
                "H.265 RTP payload is shorter than its NAL header".to_owned(),
            ));
        }
        let nal_type = (payload[0] >> 1) & 0x3f;
        if nal_type <= 47 {
            self.fragment = None;
            return Ok(vec![annexb_unit(payload, nal_type)]);
        }
        if nal_type == 48 {
            self.fragment = None;
            return parse_aggregation(&payload[2..], VideoCodec::H265);
        }
        if nal_type != 49 || payload.len() < 4 {
            return Err(PqStreamProtocolError::InvalidH26x(format!(
                "unsupported or truncated H.265 NAL type {nal_type}"
            )));
        }
        let header = payload[2];
        let start = header & 0x80 != 0;
        let end = header & 0x40 != 0;
        let original_type = header & 0x3f;
        let fragment = &payload[3..];
        if fragment.is_empty() || (start && end) {
            self.fragment = None;
            return Err(PqStreamProtocolError::InvalidH26x(
                "invalid H.265 FU flags or empty fragment".to_owned(),
            ));
        }
        if start {
            let mut bytes = Vec::with_capacity(2 + fragment.len());
            bytes.push((payload[0] & 0x81) | (original_type << 1));
            bytes.push(payload[1]);
            bytes.extend_from_slice(fragment);
            self.fragment = Some(Fragment {
                timestamp: packet.timestamp,
                nal_type: original_type,
                bytes,
            });
            return Ok(Vec::new());
        }
        let Some(current) = self.fragment.as_mut() else {
            return Ok(Vec::new());
        };
        if current.timestamp != packet.timestamp || current.nal_type != original_type {
            self.fragment = None;
            return Ok(Vec::new());
        }
        current.bytes.extend_from_slice(fragment);
        if !end {
            return Ok(Vec::new());
        }
        let current = self.fragment.take().expect("fragment exists");
        Ok(vec![annexb_unit(&current.bytes, original_type)])
    }
}

struct Fragment {
    timestamp: u32,
    nal_type: u8,
    bytes: Vec<u8>,
}

fn parse_aggregation(
    payload: &[u8],
    codec: VideoCodec,
) -> Result<Vec<NalUnit>, PqStreamProtocolError> {
    let mut offset = 0;
    let mut units = Vec::new();
    while offset < payload.len() {
        if offset + 2 > payload.len() {
            return Err(PqStreamProtocolError::InvalidH26x(
                "aggregation length is truncated".to_owned(),
            ));
        }
        let length = usize::from(u16::from_be_bytes([payload[offset], payload[offset + 1]]));
        offset += 2;
        if length == 0 || offset + length > payload.len() {
            return Err(PqStreamProtocolError::InvalidH26x(
                "aggregation NAL length is invalid".to_owned(),
            ));
        }
        let bytes = &payload[offset..offset + length];
        let nal_type = match codec {
            VideoCodec::H264 => bytes[0] & 0x1f,
            VideoCodec::H265 if bytes.len() >= 2 => (bytes[0] >> 1) & 0x3f,
            VideoCodec::H265 => {
                return Err(PqStreamProtocolError::InvalidH26x(
                    "aggregated H.265 NAL header is truncated".to_owned(),
                ));
            }
        };
        units.push(annexb_unit(bytes, nal_type));
        offset += length;
    }
    if units.is_empty() {
        return Err(PqStreamProtocolError::InvalidH26x(
            "aggregation packet is empty".to_owned(),
        ));
    }
    Ok(units)
}

fn annexb_unit(bytes: &[u8], nal_type: u8) -> NalUnit {
    let mut annexb = Vec::with_capacity(START_CODE.len() + bytes.len());
    annexb.extend_from_slice(START_CODE);
    annexb.extend_from_slice(bytes);
    NalUnit { annexb, nal_type }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub annexb: Vec<u8>,
    pub rtp_timestamp: u32,
    pub marker: bool,
    pub parameter_sets: u8,
    pub idr: bool,
}

pub struct AccessUnitAssembler {
    codec: VideoCodec,
    current: Option<AccessUnit>,
}

impl AccessUnitAssembler {
    #[must_use]
    pub const fn new(codec: VideoCodec) -> Self {
        Self {
            codec,
            current: None,
        }
    }

    pub fn push(&mut self, packet: &RtpPacket, units: Vec<NalUnit>) -> Vec<AccessUnit> {
        let mut completed = Vec::new();
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.rtp_timestamp != packet.timestamp)
            && let Some(current) = self.current.take()
        {
            completed.push(current);
        }
        if !units.is_empty() {
            let current = self.current.get_or_insert_with(|| AccessUnit {
                annexb: Vec::new(),
                rtp_timestamp: packet.timestamp,
                marker: false,
                parameter_sets: 0,
                idr: false,
            });
            for unit in units {
                current.parameter_sets |= parameter_set_bit(self.codec, unit.nal_type);
                current.idr |= is_idr(self.codec, unit.nal_type);
                current.annexb.extend_from_slice(&unit.annexb);
            }
            current.marker |= packet.marker;
        }
        if packet.marker
            && self
                .current
                .as_ref()
                .is_some_and(|current| current.rtp_timestamp == packet.timestamp)
            && let Some(current) = self.current.take()
        {
            completed.push(current);
        }
        completed
    }

    pub fn flush(&mut self) -> Option<AccessUnit> {
        self.current.take()
    }
}

#[derive(Debug)]
pub struct PreviewResync {
    codec: VideoCodec,
    waiting: bool,
    parameter_sets: [Option<Vec<u8>>; 3],
}

impl PreviewResync {
    #[must_use]
    pub const fn new(codec: VideoCodec) -> Self {
        Self {
            codec,
            waiting: false,
            parameter_sets: [None, None, None],
        }
    }

    pub fn enter(&mut self) {
        self.waiting = true;
    }

    #[must_use]
    pub const fn waiting(&self) -> bool {
        self.waiting
    }

    pub fn accept(&mut self, unit: AccessUnit) -> Option<AccessUnit> {
        self.capture_parameter_sets(&unit.annexb);
        if !self.waiting {
            return Some(unit);
        }
        if !unit.idr || !self.have_required_parameter_sets() {
            return None;
        }
        let mut annexb = Vec::new();
        for bytes in self.parameter_sets.iter().flatten() {
            annexb.extend_from_slice(bytes);
        }
        annexb.extend_from_slice(&unit.annexb);
        self.waiting = false;
        Some(AccessUnit { annexb, ..unit })
    }

    fn have_required_parameter_sets(&self) -> bool {
        match self.codec {
            VideoCodec::H264 => {
                self.parameter_sets[1].is_some() && self.parameter_sets[2].is_some()
            }
            VideoCodec::H265 => self.parameter_sets.iter().all(Option::is_some),
        }
    }

    fn capture_parameter_sets(&mut self, annexb: &[u8]) {
        for (nal_type, bytes) in split_annexb(self.codec, annexb) {
            let index = match (self.codec, nal_type) {
                (VideoCodec::H265, 32) => Some(0),
                (VideoCodec::H265, 33) | (VideoCodec::H264, 7) => Some(1),
                (VideoCodec::H265, 34) | (VideoCodec::H264, 8) => Some(2),
                _ => None,
            };
            if let Some(index) = index {
                self.parameter_sets[index] = Some(bytes.to_vec());
            }
        }
    }
}

fn parameter_set_bit(codec: VideoCodec, nal_type: u8) -> u8 {
    match (codec, nal_type) {
        (VideoCodec::H265, 32) => 1,
        (VideoCodec::H265, 33) | (VideoCodec::H264, 7) => 2,
        (VideoCodec::H265, 34) | (VideoCodec::H264, 8) => 4,
        _ => 0,
    }
}

const fn is_idr(codec: VideoCodec, nal_type: u8) -> bool {
    match codec {
        VideoCodec::H264 => nal_type == 5,
        VideoCodec::H265 => matches!(nal_type, 19 | 20),
    }
}

fn split_annexb(codec: VideoCodec, data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        let length = if data[index..].starts_with(START_CODE) {
            4
        } else if data[index..].starts_with(b"\x00\x00\x01") {
            3
        } else {
            index += 1;
            continue;
        };
        starts.push((index, length));
        index += length;
    }
    let mut result = Vec::new();
    for (position, (start, prefix)) in starts.iter().copied().enumerate() {
        let end = starts
            .get(position + 1)
            .map_or(data.len(), |(next, _)| *next);
        let payload = &data[start + prefix..end];
        if payload.is_empty() {
            continue;
        }
        let nal_type = match codec {
            VideoCodec::H264 => payload[0] & 0x1f,
            VideoCodec::H265 => (payload[0] >> 1) & 0x3f,
        };
        result.push((nal_type, &data[start..end]));
    }
    result
}

fn hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, path::Path};

    use super::*;

    fn media_description() -> &'static [u8] {
        b"m=video 98 H265/90000/1080/1280/30/6144\r\nTransport: RTP/AVP/TCP;unicast;otinterleaved=2-3;interleaved=2-3;ssrc=12345678\r\n\r\n"
    }

    fn rtp(sequence: u16, timestamp: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x80, if marker { 0x80 | 98 } else { 98 }];
        bytes.extend_from_slice(&sequence.to_be_bytes());
        bytes.extend_from_slice(&timestamp.to_be_bytes());
        bytes.extend_from_slice(&0x1234_5678_u32.to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn parsed_rtp(sequence: u16, timestamp: u32, marker: bool, payload: &[u8]) -> RtpPacket {
        RtpPacket::parse(&rtp(sequence, timestamp, marker, payload)).unwrap()
    }

    #[test]
    fn request_matches_official_successful_session_exactly() {
        let bytes = MediaRequest {
            host: "10.21.12.102".to_owned(),
            port: 80,
            channel: 0,
            media: "video_data".to_owned(),
            cseq: 7,
        }
        .to_bytes()
        .unwrap();
        assert_eq!(
            bytes,
            b"GET http://10.21.12.102:80/livestream/0?action=play&media=video_data HTTP/1.1\r\nCseq: 7\r\nUser-Agent: Streaming Media Client/1.0.0(Apr  2 2025)\r\nConnection: Keep-Alive\r\nCache-Control: no-cache\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        );
    }

    #[test]
    fn media_description_parses_dynamic_fields_and_bounds() {
        let parsed = parse_media_description(media_description()).unwrap();
        assert_eq!(parsed.payload_type, 98);
        assert_eq!(parsed.codec, VideoCodec::H265);
        assert_eq!((parsed.width, parsed.height), (1080, 1280));
        assert_eq!((parsed.rtp_channel, parsed.control_channel), (2, 3));
        assert_eq!(parsed.ssrc, Some(0x1234_5678));
        let oversized = vec![b'a'; DEFAULT_MAX_MEDIA_DESCRIPTION_BYTES + 1];
        assert!(parse_media_description(&oversized).is_err());
    }

    #[test]
    fn record_uses_uint32_length_and_distinguishes_boundary_eof_from_truncation() {
        let packet = rtp(1, 10, true, &vec![0xaa; 70_000]);
        let mut bytes = b"$\x02\x80\x00".to_vec();
        bytes.extend_from_slice(&u32::try_from(packet.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(&packet);
        let parsed = read_pq_record(&mut Cursor::new(&bytes), 2, DEFAULT_MAX_RECORD_BYTES)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.packet.len(), packet.len());
        assert!(
            read_pq_record(
                &mut Cursor::new(Vec::<u8>::new()),
                2,
                DEFAULT_MAX_RECORD_BYTES
            )
            .unwrap()
            .is_none()
        );
        assert!(matches!(
            read_pq_record(&mut Cursor::new(&bytes[..6]), 2, DEFAULT_MAX_RECORD_BYTES),
            Err(PqStreamProtocolError::TruncatedRecord {
                stage: RecordTruncationStage::Header,
                ..
            })
        ));
        assert!(matches!(
            read_pq_record(
                &mut Cursor::new(&bytes[..bytes.len() - 1]),
                2,
                DEFAULT_MAX_RECORD_BYTES
            ),
            Err(PqStreamProtocolError::TruncatedRecord {
                stage: RecordTruncationStage::Payload,
                ..
            })
        ));
    }

    #[test]
    fn rtp_v2_extension_padding_pt_ssrc_wrap_and_gap_are_validated() {
        let mut packet = vec![0xb1, 98];
        packet.extend_from_slice(&9_u16.to_be_bytes());
        packet.extend_from_slice(&100_u32.to_be_bytes());
        packet.extend_from_slice(&0x1234_5678_u32.to_be_bytes());
        packet.extend_from_slice(&2_u32.to_be_bytes());
        packet.extend_from_slice(b"\xbe\xde\x00\x01ABCDpayload\x00\x00\x03");
        assert_eq!(RtpPacket::parse(&packet).unwrap().payload, b"payload");

        let mut validator = RtpValidator::new(3, 98, Some(0x1234_5678)).unwrap();
        assert!(
            validator
                .accept(parsed_rtp(65_534, 1, false, b"a"))
                .unwrap()
                .packets
                .is_empty()
        );
        assert!(
            validator
                .accept(parsed_rtp(65_535, 1, false, b"b"))
                .unwrap()
                .packets
                .is_empty()
        );
        assert_eq!(
            validator
                .accept(parsed_rtp(0, 1, false, b"c"))
                .unwrap()
                .packets
                .len(),
            3
        );
        let gap = validator.accept(parsed_rtp(2, 2, true, b"d")).unwrap();
        assert!(gap.had_gap);
        assert_eq!(gap.missing_packets, 1);
        assert_eq!(validator.gaps(), 1);
        let wrong_pt = RtpPacket::parse(&{
            let mut bytes = rtp(3, 3, true, b"e");
            bytes[1] = 97;
            bytes
        })
        .unwrap();
        assert!(validator.accept(wrong_pt).is_err());
    }

    #[test]
    fn h264_and_h265_single_aggregation_fu_and_timestamp_loss_are_handled() {
        let mut h264 = H264Depacketizer::default();
        assert_eq!(
            h264.push(&parsed_rtp(1, 1, true, b"\x65A")).unwrap()[0].annexb,
            b"\x00\x00\x00\x01\x65A"
        );
        let stap = b"\x78\x00\x02\x67A\x00\x02\x68B";
        assert_eq!(h264.push(&parsed_rtp(2, 2, true, stap)).unwrap().len(), 2);
        assert!(
            h264.push(&parsed_rtp(3, 3, false, b"\x7c\x85ab"))
                .unwrap()
                .is_empty()
        );
        assert!(
            h264.push(&parsed_rtp(4, 4, true, b"\x7c\x45cd"))
                .unwrap()
                .is_empty()
        );
        assert!(
            h264.push(&parsed_rtp(5, 5, false, b"\x7c\x85ab"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            h264.push(&parsed_rtp(6, 5, true, b"\x7c\x45cd")).unwrap()[0].annexb,
            b"\x00\x00\x00\x01\x65abcd"
        );

        let mut h265 = H265Depacketizer::default();
        assert_eq!(
            h265.push(&parsed_rtp(1, 1, true, b"\x26\x01A")).unwrap()[0].nal_type,
            19
        );
        let ap = b"\x60\x01\x00\x03\x40\x01A\x00\x03\x42\x01B";
        assert_eq!(h265.push(&parsed_rtp(2, 2, true, ap)).unwrap().len(), 2);
        assert!(
            h265.push(&parsed_rtp(3, 3, false, b"\x62\x01\x93ab"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            h265.push(&parsed_rtp(4, 3, true, b"\x62\x01\x53cd"))
                .unwrap()[0]
                .annexb,
            b"\x00\x00\x00\x01\x26\x01abcd"
        );
    }

    #[test]
    fn preview_resync_requires_parameter_sets_and_idr() {
        let mut gate = PreviewResync::new(VideoCodec::H265);
        gate.enter();
        let parameter_sets = AccessUnit {
            annexb: [
                START_CODE.as_slice(),
                b"\x40\x01A",
                START_CODE.as_slice(),
                b"\x42\x01B",
                START_CODE.as_slice(),
                b"\x44\x01C",
            ]
            .concat(),
            rtp_timestamp: 1,
            marker: true,
            parameter_sets: 7,
            idr: false,
        };
        assert!(gate.accept(parameter_sets).is_none());
        let idr = AccessUnit {
            annexb: [START_CODE.as_slice(), b"\x26\x01D"].concat(),
            rtp_timestamp: 2,
            marker: true,
            parameter_sets: 0,
            idr: true,
        };
        let recovered = gate.accept(idr).unwrap();
        assert!(!gate.waiting());
        assert!(recovered.annexb.ends_with(b"\x00\x00\x00\x01\x26\x01D"));
    }

    #[test]
    fn compact_real_capture_fixture_has_exact_description_and_first_pq_record() {
        let path = Path::new(
            "/media/psf/Home/Desktop/PQStream_Alternative/captures/live_path_test/pqstream.raw",
        );
        if !path.exists() {
            return;
        }
        let bytes = fs::read(path).unwrap();
        let marker = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let description = parse_media_description(&bytes[..marker]).unwrap();
        assert_eq!(
            (description.codec, description.payload_type),
            (VideoCodec::H265, 98)
        );
        let record = read_pq_record(
            &mut Cursor::new(&bytes[marker..]),
            description.rtp_channel,
            DEFAULT_MAX_RECORD_BYTES,
        )
        .unwrap()
        .unwrap();
        let packet = RtpPacket::parse(&record.packet).unwrap();
        assert_eq!(packet.ssrc, description.ssrc.unwrap());
        assert!(packet.payload.starts_with(START_CODE));
    }
}

/// 在公开入口统一执行长度/ASCII guard 后解析动态字段。
pub fn parse_media_description(raw: &[u8]) -> Result<MediaDescription, PqStreamProtocolError> {
    if raw.len() > DEFAULT_MAX_MEDIA_DESCRIPTION_BYTES {
        return Err(PqStreamProtocolError::InvalidMediaDescription(format!(
            "description exceeds {} bytes",
            DEFAULT_MAX_MEDIA_DESCRIPTION_BYTES
        )));
    }
    if !raw.ends_with(b"\r\n\r\n") {
        return Err(PqStreamProtocolError::InvalidMediaDescription(
            "description does not end with CRLF CRLF".to_owned(),
        ));
    }
    if !raw.is_ascii() {
        return Err(PqStreamProtocolError::InvalidMediaDescription(
            "description is not ASCII".to_owned(),
        ));
    }
    parse_media_description_impl(raw)
}
