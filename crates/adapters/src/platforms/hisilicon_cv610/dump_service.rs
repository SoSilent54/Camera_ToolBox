//! CV610 PQTools TCP 4321 one-shot Still Dump service。

use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpStream},
    sync::Arc,
    time::{Duration, Instant},
};

use camera_toolbox_app::{
    CaptureStore, DumpInitializationPolicy, DumpOperationControl, DumpOperationResult, DumpService,
    DumpServiceError, DumpSourceDescriptor, DumpStage, DumpTruncationStage, OperationId,
    VerifiedDumpRequest,
};
use camera_toolbox_core::{
    CaptureMetadata, ChromaOrder, EphemeralAsset, IntegrityState, MediaFormat, OwnedMediaPayload,
};
use socket2::{Domain, Protocol, Socket, Type};

use super::pqtools_protocol::{
    DEFAULT_MAX_RESPONSE_BYTES, PqtoolsProtocolError, ResponsePrefix, encode_request,
    read_payload_and_checksum, read_response_prefix,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cv610DumpEndpoint {
    pub address: IpAddr,
    pub port: u16,
}

impl Cv610DumpEndpoint {
    #[must_use]
    pub fn display(&self) -> String {
        SocketAddr::new(self.address, self.port).to_string()
    }
}

/// 只有由上层显式注入且 recipe id 精确匹配的 hook 才能发送 cold-init traffic。
pub trait ValidatedDumpInitializer: Send + Sync {
    fn recipe_id(&self) -> &str;

    /// # Errors
    ///
    /// recipe 执行失败、取消或 deadline 到期时返回 typed error。
    fn initialize(
        &self,
        endpoint: &Cv610DumpEndpoint,
        control: &DumpOperationControl,
    ) -> Result<(), DumpServiceError>;
}

pub type ValidatedInitializerRegistry = BTreeMap<String, Arc<dyn ValidatedDumpInitializer>>;

pub struct Cv610DumpService {
    service_id: String,
    endpoint: Cv610DumpEndpoint,
    initialization: DumpInitializationPolicy,
    initializer: Option<Arc<dyn ValidatedDumpInitializer>>,
    max_response_bytes: usize,
}

impl Cv610DumpService {
    /// 构造只允许 direct 或显式 injected recipe 的 service。
    ///
    /// # Errors
    ///
    /// id/host/port/上限无效，或 `ValidatedRecipe` 没有同名 hook 时返回错误。
    pub fn new(
        service_id: impl Into<String>,
        endpoint: Cv610DumpEndpoint,
        initialization: DumpInitializationPolicy,
        initializers: &ValidatedInitializerRegistry,
        max_response_bytes: usize,
    ) -> Result<Self, DumpServiceError> {
        let service_id = service_id.into();
        if service_id.trim().is_empty() {
            return Err(DumpServiceError::InvalidRequest(
                "dump service id must not be empty".to_owned(),
            ));
        }
        if endpoint.port == 0 {
            return Err(DumpServiceError::InvalidRequest(
                "CV610 dump endpoint must have a non-zero port".to_owned(),
            ));
        }
        if max_response_bytes < 4 {
            return Err(DumpServiceError::InvalidRequest(
                "max response bytes must include at least the checksum".to_owned(),
            ));
        }
        let initializer = match &initialization {
            DumpInitializationPolicy::Auto | DumpInitializationPolicy::DirectOnly => None,
            DumpInitializationPolicy::ValidatedRecipe { recipe_id } => {
                let hook = initializers.get(recipe_id).cloned().ok_or_else(|| {
                    DumpServiceError::InitializationRecipeUnavailable {
                        recipe_id: recipe_id.clone(),
                    }
                })?;
                if hook.recipe_id() != recipe_id {
                    return Err(DumpServiceError::InitializationRecipeUnavailable {
                        recipe_id: recipe_id.clone(),
                    });
                }
                Some(hook)
            }
        };
        Ok(Self {
            service_id,
            endpoint,
            initialization,
            initializer,
            max_response_bytes,
        })
    }

    #[must_use]
    pub fn with_default_limit(
        service_id: impl Into<String>,
        endpoint: Cv610DumpEndpoint,
        initialization: DumpInitializationPolicy,
        initializers: &ValidatedInitializerRegistry,
    ) -> Result<Self, DumpServiceError> {
        Self::new(
            service_id,
            endpoint,
            initialization,
            initializers,
            DEFAULT_MAX_RESPONSE_BYTES,
        )
    }

    fn run_capture(
        &self,
        operation_id: OperationId,
        request: VerifiedDumpRequest,
        control: &DumpOperationControl,
        store: &CaptureStore,
    ) -> Result<DumpOperationResult, DumpServiceError> {
        let started = Instant::now();
        let overall_deadline = started
            .checked_add(control.timeouts.overall)
            .ok_or_else(|| {
                DumpServiceError::InvalidRequest("overall deadline overflow".to_owned())
            })?;
        check_control(control, overall_deadline, DumpStage::Initializing)?;

        if let DumpInitializationPolicy::ValidatedRecipe { recipe_id } = &self.initialization {
            control.report(DumpStage::Initializing);
            let initializer = self.initializer.as_ref().ok_or_else(|| {
                DumpServiceError::InitializationRecipeUnavailable {
                    recipe_id: recipe_id.clone(),
                }
            })?;
            initializer
                .initialize(&self.endpoint, control)
                .map_err(|error| match error {
                    DumpServiceError::Cancelled { .. }
                    | DumpServiceError::DeadlineExceeded { .. } => error,
                    other => DumpServiceError::InitializationFailed {
                        recipe_id: recipe_id.clone(),
                        reason: other.to_string(),
                    },
                })?;
        }
        // Auto 和 DirectOnly 都直接发送已验证请求；绝不猜测初始化读取。
        check_control(control, overall_deadline, DumpStage::Connecting)?;
        control.report(DumpStage::Connecting);
        let stream = connect_operation_socket(&self.endpoint, control, overall_deadline)?;
        let mut stream = DeadlineStream::new(stream, control, overall_deadline);

        control.report(DumpStage::Requesting);
        write_request(
            &mut stream,
            &encode_request(request.kind),
            control,
            overall_deadline,
        )?;

        control.report(DumpStage::ReceivingMetadata);
        let prefix = read_response_prefix(&mut stream, request.kind, self.max_response_bytes)
            .map_err(|error| map_protocol_error(error, &stream, control))?;

        // metadata-derived 长度已 checked 且与 block 闭合，先做 store reservation，再分配最终 Arc。
        let reservation = store.reserve(operation_id, prefix.payload_length)?;
        let mut payload: Arc<[u8]> = vec![0_u8; prefix.payload_length].into();
        let final_buffer =
            Arc::get_mut(&mut payload).ok_or(DumpServiceError::AllocationFailed {
                bytes: prefix.payload_length,
            })?;

        control.report(DumpStage::ReceivingPayload);
        let verification = read_payload_and_checksum(&mut stream, &prefix, final_buffer)
            .map_err(|error| map_protocol_error(error, &stream, control))?;
        control.report(DumpStage::Verifying);
        check_control(control, overall_deadline, DumpStage::Verifying)?;

        let metadata = capture_metadata(&request, &prefix, verification.response_checksum);
        let asset = EphemeralAsset::new(
            request.asset_id,
            OwnedMediaPayload::from_bytes(payload),
            metadata,
            IntegrityState::Verified {
                algorithm: "sha256".to_owned(),
                digest: verification.payload_sha256.clone(),
            },
        );
        control.report(DumpStage::Publishing);
        let asset = store.publish_validated(reservation, asset)?;
        Ok(DumpOperationResult {
            asset,
            descriptor: prefix.descriptor,
            envelope: prefix.envelope,
            payload_sha256: verification.payload_sha256,
            response_checksum: verification.response_checksum,
        })
    }
}

impl DumpService for Cv610DumpService {
    fn service_id(&self) -> &str {
        &self.service_id
    }

    fn capture(
        &self,
        operation_id: OperationId,
        request: VerifiedDumpRequest,
        control: DumpOperationControl,
        store: &CaptureStore,
    ) -> Result<DumpOperationResult, DumpServiceError> {
        let _guard = InterruptGuard(control.cancellation.clone());
        self.run_capture(operation_id, request, &control, store)
    }
}

fn capture_metadata(
    request: &VerifiedDumpRequest,
    prefix: &ResponsePrefix,
    checksum: u32,
) -> CaptureMetadata {
    let mut attributes = BTreeMap::new();
    attributes.insert("protocol".to_owned(), "pqtools_dump".to_owned());
    attributes.insert(
        "command_code".to_owned(),
        format!(
            "0x{:02x}",
            super::pqtools_protocol::command_code(request.kind)
        ),
    );
    attributes.insert(
        "progress_tokens".to_owned(),
        prefix.envelope.progress_tokens.to_string(),
    );
    attributes.insert(
        "frame_count".to_owned(),
        prefix.envelope.frame_count.to_string(),
    );
    attributes.insert(
        "block_length".to_owned(),
        prefix.envelope.block_length.to_string(),
    );
    attributes.insert("response_checksum".to_owned(), format!("0x{checksum:08x}"));
    let format = match &prefix.descriptor {
        DumpSourceDescriptor::Raw {
            width,
            height,
            stride,
            bit_depth,
            metadata_words,
        } => {
            attributes.insert("width".to_owned(), width.to_string());
            attributes.insert("height".to_owned(), height.to_string());
            attributes.insert("stride".to_owned(), stride.to_string());
            attributes.insert("metadata_words".to_owned(), format!("{metadata_words:?}"));
            MediaFormat::RawPacked {
                bit_depth: *bit_depth,
            }
        }
        DumpSourceDescriptor::Jpeg {
            width,
            height,
            payload_len,
            reserved,
        } => {
            attributes.insert("width".to_owned(), width.to_string());
            attributes.insert("height".to_owned(), height.to_string());
            attributes.insert("jpeg_length".to_owned(), payload_len.to_string());
            attributes.insert("reserved".to_owned(), hex(reserved));
            MediaFormat::Jpeg
        }
        DumpSourceDescriptor::Nv21 {
            width,
            height,
            y_stride,
            chroma_stride,
            metadata_words,
        } => {
            attributes.insert("width".to_owned(), width.to_string());
            attributes.insert("height".to_owned(), height.to_string());
            attributes.insert("y_stride".to_owned(), y_stride.to_string());
            attributes.insert("chroma_stride".to_owned(), chroma_stride.to_string());
            attributes.insert("pixel_format".to_owned(), "nv21".to_owned());
            attributes.insert("metadata_words".to_owned(), format!("{metadata_words:?}"));
            MediaFormat::Yuv420Sp {
                chroma_order: ChromaOrder::Vu,
            }
        }
    };
    CaptureMetadata {
        format,
        source_name: request.source_name.clone(),
        attributes,
    }
}

fn connect_operation_socket(
    endpoint: &Cv610DumpEndpoint,
    control: &DumpOperationControl,
    overall_deadline: Instant,
) -> Result<TcpStream, DumpServiceError> {
    let address = SocketAddr::new(endpoint.address, endpoint.port);
    let connect_deadline = Instant::now()
        .checked_add(control.timeouts.connect)
        .map_or(overall_deadline, |deadline| deadline.min(overall_deadline));
    check_control(control, overall_deadline, DumpStage::Connecting)?;
    let domain = if address.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP)).map_err(|error| {
        DumpServiceError::Transport {
            stage: DumpStage::Connecting,
            reason: error.to_string(),
        }
    })?;
    socket
        .set_nonblocking(true)
        .map_err(|error| DumpServiceError::Transport {
            stage: DumpStage::Connecting,
            reason: error.to_string(),
        })?;
    let interrupt_socket = socket
        .try_clone()
        .map_err(|error| DumpServiceError::Transport {
            stage: DumpStage::Connecting,
            reason: error.to_string(),
        })?;
    control.cancellation.register_interrupt(Arc::new(move || {
        let _ = interrupt_socket.shutdown(Shutdown::Both);
    }));

    match socket.connect(&address.into()) {
        Ok(()) => return finish_connect(socket, control),
        Err(error) if connect_in_progress(&error) => {}
        Err(error) => {
            return Err(DumpServiceError::Transport {
                stage: DumpStage::Connecting,
                reason: error.to_string(),
            });
        }
    }
    loop {
        check_control(control, overall_deadline, DumpStage::Connecting)?;
        if Instant::now() >= connect_deadline {
            let _ = socket.shutdown(Shutdown::Both);
            if Instant::now() >= overall_deadline {
                return Err(DumpServiceError::DeadlineExceeded {
                    stage: DumpStage::Connecting,
                });
            }
            return Err(DumpServiceError::ConnectTimeout {
                timeout_ms: duration_ms(control.timeouts.connect),
            });
        }
        if let Some(error) = socket
            .take_error()
            .map_err(|error| DumpServiceError::Transport {
                stage: DumpStage::Connecting,
                reason: error.to_string(),
            })?
        {
            return Err(DumpServiceError::Transport {
                stage: DumpStage::Connecting,
                reason: error.to_string(),
            });
        }
        if socket.peer_addr().is_ok() {
            return finish_connect(socket, control);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn finish_connect(
    socket: Socket,
    control: &DumpOperationControl,
) -> Result<TcpStream, DumpServiceError> {
    socket
        .set_nonblocking(false)
        .map_err(|error| DumpServiceError::Transport {
            stage: DumpStage::Connecting,
            reason: error.to_string(),
        })?;
    let stream: TcpStream = socket.into();
    let interrupt_stream = stream
        .try_clone()
        .map_err(|error| DumpServiceError::Transport {
            stage: DumpStage::Connecting,
            reason: error.to_string(),
        })?;
    control.cancellation.register_interrupt(Arc::new(move || {
        let _ = interrupt_stream.shutdown(Shutdown::Both);
    }));
    if control.cancellation.is_cancelled() {
        let _ = stream.shutdown(Shutdown::Both);
        return Err(DumpServiceError::Cancelled {
            stage: DumpStage::Connecting,
        });
    }
    Ok(stream)
}

fn connect_in_progress(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || matches!(error.raw_os_error(), Some(36 | 115 | 10035))
}

fn write_request(
    stream: &mut DeadlineStream<'_>,
    request: &[u8],
    control: &DumpOperationControl,
    overall_deadline: Instant,
) -> Result<(), DumpServiceError> {
    let mut sent = 0_usize;
    while sent < request.len() {
        check_control(control, overall_deadline, DumpStage::Requesting)?;
        stream.prepare_timeout(DumpStage::Requesting)?;
        match stream.stream.write(&request[sent..]) {
            Ok(0) => {
                return Err(DumpServiceError::Transport {
                    stage: DumpStage::Requesting,
                    reason: "peer closed while request was being sent".to_owned(),
                });
            }
            Ok(count) => sent += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(stream.timeout_error(DumpStage::Requesting));
            }
            Err(error) => {
                return Err(DumpServiceError::Transport {
                    stage: DumpStage::Requesting,
                    reason: error.to_string(),
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum ReadFailure {
    Cancelled,
    Deadline,
    Idle,
}

struct DeadlineStream<'a> {
    stream: TcpStream,
    control: &'a DumpOperationControl,
    overall_deadline: Instant,
    last_failure: Option<ReadFailure>,
}

impl<'a> DeadlineStream<'a> {
    fn new(
        stream: TcpStream,
        control: &'a DumpOperationControl,
        overall_deadline: Instant,
    ) -> Self {
        Self {
            stream,
            control,
            overall_deadline,
            last_failure: None,
        }
    }

    fn prepare_timeout(&mut self, stage: DumpStage) -> Result<(), DumpServiceError> {
        check_control(self.control, self.overall_deadline, stage)?;
        let remaining = self
            .overall_deadline
            .saturating_duration_since(Instant::now());
        let timeout = self
            .control
            .timeouts
            .idle
            .min(remaining)
            .max(Duration::from_millis(1));
        self.stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| self.stream.set_write_timeout(Some(timeout)))
            .map_err(|error| DumpServiceError::Transport {
                stage,
                reason: error.to_string(),
            })
    }

    fn timeout_error(&mut self, stage: DumpStage) -> DumpServiceError {
        if self.control.cancellation.is_cancelled() {
            self.last_failure = Some(ReadFailure::Cancelled);
            DumpServiceError::Cancelled { stage }
        } else if Instant::now() >= self.overall_deadline {
            self.last_failure = Some(ReadFailure::Deadline);
            DumpServiceError::DeadlineExceeded { stage }
        } else {
            self.last_failure = Some(ReadFailure::Idle);
            DumpServiceError::IdleTimeout {
                stage,
                timeout_ms: duration_ms(self.control.timeouts.idle),
            }
        }
    }
}

impl Read for DeadlineStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.control.cancellation.is_cancelled() {
            self.last_failure = Some(ReadFailure::Cancelled);
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "dump cancelled",
            ));
        }
        if Instant::now() >= self.overall_deadline {
            self.last_failure = Some(ReadFailure::Deadline);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "overall deadline exceeded",
            ));
        }
        let remaining = self
            .overall_deadline
            .saturating_duration_since(Instant::now());
        let timeout = self
            .control
            .timeouts
            .idle
            .min(remaining)
            .max(Duration::from_millis(1));
        self.stream.set_read_timeout(Some(timeout))?;
        match self.stream.read(buffer) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                if Instant::now() >= self.overall_deadline {
                    self.last_failure = Some(ReadFailure::Deadline);
                } else {
                    self.last_failure = Some(ReadFailure::Idle);
                }
                Err(error)
            }
            other => other,
        }
    }
}

fn map_protocol_error(
    error: PqtoolsProtocolError,
    stream: &DeadlineStream<'_>,
    control: &DumpOperationControl,
) -> DumpServiceError {
    let stage = protocol_stage(&error);
    if control.cancellation.is_cancelled() {
        return DumpServiceError::Cancelled { stage };
    }
    match stream.last_failure {
        Some(ReadFailure::Cancelled) => return DumpServiceError::Cancelled { stage },
        Some(ReadFailure::Deadline) => return DumpServiceError::DeadlineExceeded { stage },
        Some(ReadFailure::Idle) => {
            return DumpServiceError::IdleTimeout {
                stage,
                timeout_ms: duration_ms(control.timeouts.idle),
            };
        }
        None => {}
    }
    match error {
        PqtoolsProtocolError::Truncated {
            stage,
            expected,
            received,
        } => DumpServiceError::Truncated {
            stage,
            expected,
            received,
        },
        PqtoolsProtocolError::PeerClosedBeforeChecksum { received } => {
            DumpServiceError::PeerClosedBeforeChecksum { received }
        }
        PqtoolsProtocolError::ServerRejected(message) => {
            DumpServiceError::ServerRejected { message }
        }
        PqtoolsProtocolError::ResponseTooLarge { declared, limit } => {
            DumpServiceError::ResponseTooLarge { declared, limit }
        }
        PqtoolsProtocolError::ChecksumMismatch {
            received,
            calculated,
        } => DumpServiceError::ChecksumMismatch {
            received,
            calculated,
        },
        PqtoolsProtocolError::Io { source, .. } => DumpServiceError::Transport {
            stage,
            reason: source.to_string(),
        },
        other => DumpServiceError::ProtocolViolation {
            stage,
            reason: other.to_string(),
        },
    }
}

fn protocol_stage(error: &PqtoolsProtocolError) -> DumpStage {
    match error {
        PqtoolsProtocolError::Truncated { stage, .. } | PqtoolsProtocolError::Io { stage, .. } => {
            truncation_dump_stage(*stage)
        }
        PqtoolsProtocolError::PeerClosedBeforeChecksum { .. }
        | PqtoolsProtocolError::ChecksumMismatch { .. }
        | PqtoolsProtocolError::InvalidJpeg(_) => DumpStage::Verifying,
        PqtoolsProtocolError::FinalBufferLengthMismatch { .. } => DumpStage::ReceivingPayload,
        _ => DumpStage::ReceivingMetadata,
    }
}

const fn truncation_dump_stage(stage: DumpTruncationStage) -> DumpStage {
    match stage {
        DumpTruncationStage::Marker
        | DumpTruncationStage::Envelope
        | DumpTruncationStage::Metadata
        | DumpTruncationStage::ErrorText => DumpStage::ReceivingMetadata,
        DumpTruncationStage::Payload => DumpStage::ReceivingPayload,
        DumpTruncationStage::Checksum => DumpStage::Verifying,
    }
}

fn check_control(
    control: &DumpOperationControl,
    overall_deadline: Instant,
    stage: DumpStage,
) -> Result<(), DumpServiceError> {
    if control.cancellation.is_cancelled() {
        Err(DumpServiceError::Cancelled { stage })
    } else if Instant::now() >= overall_deadline {
        Err(DumpServiceError::DeadlineExceeded { stage })
    } else {
        Ok(())
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

struct InterruptGuard(camera_toolbox_app::DumpCancellation);

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        self.0.clear_interrupt();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
    };

    use camera_toolbox_app::{
        CaptureStoreLimits, DumpCancellation, DumpTimeouts, VerifiedDumpKind,
    };
    use camera_toolbox_core::{AssetId, OwnedMediaPayload};

    use super::*;

    #[test]
    fn local_tcp_smoke_sends_exact_request_and_publishes_jpeg() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (received_sender, received_receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 128];
            socket.read_exact(&mut request).unwrap();
            received_sender.send(request).unwrap();
            let response = jpeg_response(6, 4, &[0xff, 0xd8, 1, 2, 0xff, 0xd9]);
            for chunk in response.chunks(7) {
                socket.write_all(chunk).unwrap();
            }
        });

        let service = Cv610DumpService::with_default_limit(
            "cv610-dump",
            Cv610DumpEndpoint {
                address: "127.0.0.1".parse().unwrap(),
                port,
            },
            DumpInitializationPolicy::DirectOnly,
            &BTreeMap::new(),
        )
        .unwrap();
        let store = CaptureStore::new(CaptureStoreLimits::new(1024, 1024).unwrap());
        let result = service
            .capture(
                OperationId::new("smoke").unwrap(),
                VerifiedDumpRequest::new(
                    VerifiedDumpKind::Jpeg,
                    AssetId::new("jpeg-1").unwrap(),
                    "fake-server.jpg",
                )
                .unwrap(),
                DumpOperationControl::new(DumpTimeouts::default(), DumpCancellation::default())
                    .unwrap(),
                &store,
            )
            .unwrap();
        server.join().unwrap();
        assert_eq!(
            received_receiver.recv().unwrap(),
            encode_request(VerifiedDumpKind::Jpeg)
        );
        let OwnedMediaPayload::Bytes(bytes) = &result.asset.source else {
            panic!("JPEG must use one authoritative bytes payload")
        };
        assert_eq!(bytes.as_ref(), &[0xff, 0xd8, 1, 2, 0xff, 0xd9]);
        assert_eq!(store.stats().unwrap().published_bytes, 6);
    }

    #[test]
    fn server_rejection_does_not_publish_partial_asset() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 128];
            socket.read_exact(&mut request).unwrap();
            socket.write_all(b"\xeerc mode inconformity!\0").unwrap();
        });
        let service = Cv610DumpService::with_default_limit(
            "cv610-rejected",
            Cv610DumpEndpoint {
                address: "127.0.0.1".parse().unwrap(),
                port,
            },
            DumpInitializationPolicy::DirectOnly,
            &BTreeMap::new(),
        )
        .unwrap();
        let store = CaptureStore::new(CaptureStoreLimits::new(1024, 1024).unwrap());
        let error = service
            .capture(
                OperationId::new("rejected").unwrap(),
                VerifiedDumpRequest::new(
                    VerifiedDumpKind::Raw12,
                    AssetId::new("must-not-publish").unwrap(),
                    "rejected.raw",
                )
                .unwrap(),
                DumpOperationControl::new(DumpTimeouts::default(), DumpCancellation::default())
                    .unwrap(),
                &store,
            )
            .unwrap_err();
        server.join().unwrap();
        assert_eq!(
            error,
            DumpServiceError::ServerRejected {
                message: "rc mode inconformity!".to_owned()
            }
        );
        assert_eq!(store.stats().unwrap(), Default::default());
    }

    fn capture_with_timeouts(port: u16, timeouts: DumpTimeouts, suffix: &str) -> DumpServiceError {
        let service = Cv610DumpService::with_default_limit(
            format!("deadline-{suffix}"),
            Cv610DumpEndpoint {
                address: "127.0.0.1".parse().unwrap(),
                port,
            },
            DumpInitializationPolicy::DirectOnly,
            &BTreeMap::new(),
        )
        .unwrap();
        let store = CaptureStore::new(CaptureStoreLimits::new(1024, 1024).unwrap());
        let error = service
            .capture(
                OperationId::new(format!("deadline-{suffix}")).unwrap(),
                VerifiedDumpRequest::new(
                    VerifiedDumpKind::Jpeg,
                    AssetId::new(format!("deadline-{suffix}")).unwrap(),
                    format!("deadline-{suffix}.jpg"),
                )
                .unwrap(),
                DumpOperationControl::new(timeouts, DumpCancellation::default()).unwrap(),
                &store,
            )
            .unwrap_err();
        assert_eq!(store.stats().unwrap(), Default::default());
        error
    }

    #[test]
    fn stalled_server_reports_idle_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 128];
            socket.read_exact(&mut request).unwrap();
            thread::sleep(Duration::from_millis(200));
        });
        let error = capture_with_timeouts(
            port,
            DumpTimeouts {
                connect: Duration::from_millis(100),
                idle: Duration::from_millis(50),
                overall: Duration::from_secs(1),
            },
            "idle",
        );
        server.join().unwrap();
        assert_eq!(
            error,
            DumpServiceError::IdleTimeout {
                stage: DumpStage::ReceivingMetadata,
                timeout_ms: 50
            }
        );
    }

    #[test]
    fn continuing_progress_cannot_extend_overall_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 128];
            socket.read_exact(&mut request).unwrap();
            for _ in 0..30 {
                if socket.write_all(&[0xc0]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let error = capture_with_timeouts(
            port,
            DumpTimeouts {
                connect: Duration::from_millis(100),
                idle: Duration::from_millis(100),
                overall: Duration::from_millis(250),
            },
            "overall",
        );
        server.join().unwrap();
        assert_eq!(
            error,
            DumpServiceError::DeadlineExceeded {
                stage: DumpStage::ReceivingMetadata
            }
        );
    }

    #[test]
    fn cancellation_shutdown_interrupts_blocked_receive() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(Barrier::new(2));
        let server_barrier = Arc::clone(&accepted);
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 128];
            socket.read_exact(&mut request).unwrap();
            server_barrier.wait();
            let mut byte = [0_u8; 1];
            let _ = socket.read(&mut byte);
        });
        let service = Arc::new(
            Cv610DumpService::with_default_limit(
                "cv610-dump",
                Cv610DumpEndpoint {
                    address: "127.0.0.1".parse().unwrap(),
                    port,
                },
                DumpInitializationPolicy::Auto,
                &BTreeMap::new(),
            )
            .unwrap(),
        );
        let store = CaptureStore::new(CaptureStoreLimits::new(1024, 1024).unwrap());
        let cancellation = DumpCancellation::default();
        let worker_cancel = cancellation.clone();
        let worker = thread::spawn(move || {
            service.capture(
                OperationId::new("cancel").unwrap(),
                VerifiedDumpRequest::new(
                    VerifiedDumpKind::Jpeg,
                    AssetId::new("cancelled").unwrap(),
                    "cancelled.jpg",
                )
                .unwrap(),
                DumpOperationControl::new(
                    DumpTimeouts {
                        connect: Duration::from_secs(1),
                        idle: Duration::from_secs(5),
                        overall: Duration::from_secs(5),
                    },
                    worker_cancel,
                )
                .unwrap(),
                &store,
            )
        });
        accepted.wait();
        cancellation.cancel();
        let error = worker.join().unwrap().unwrap_err();
        assert!(matches!(
            error,
            DumpServiceError::Cancelled {
                stage: DumpStage::ReceivingMetadata
            }
        ));
        server.join().unwrap();
    }

    struct RecordingInitializer {
        called: Arc<AtomicUsize>,
    }

    impl ValidatedDumpInitializer for RecordingInitializer {
        fn recipe_id(&self) -> &str {
            "known-recipe"
        }

        fn initialize(
            &self,
            _endpoint: &Cv610DumpEndpoint,
            _control: &DumpOperationControl,
        ) -> Result<(), DumpServiceError> {
            self.called.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn run_policy_capture(
        policy: DumpInitializationPolicy,
        registry: &ValidatedInitializerRegistry,
        called: &Arc<AtomicUsize>,
        expected_calls_before_request: usize,
        suffix: &str,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_called = Arc::clone(called);
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 128];
            socket.read_exact(&mut request).unwrap();
            assert_eq!(
                server_called.load(Ordering::SeqCst),
                expected_calls_before_request,
                "initializer ordering differs before direct request"
            );
            assert_eq!(request, encode_request(VerifiedDumpKind::Jpeg));
            socket
                .write_all(&jpeg_response(2, 2, &[0xff, 0xd8, 0xff, 0xd9]))
                .unwrap();
        });
        let service = Cv610DumpService::new(
            format!("policy-{suffix}"),
            Cv610DumpEndpoint {
                address: "127.0.0.1".parse().unwrap(),
                port,
            },
            policy,
            registry,
            1024,
        )
        .unwrap();
        let store = CaptureStore::new(CaptureStoreLimits::new(1024, 1024).unwrap());
        service
            .capture(
                OperationId::new(format!("operation-{suffix}")).unwrap(),
                VerifiedDumpRequest::new(
                    VerifiedDumpKind::Jpeg,
                    AssetId::new(format!("asset-{suffix}")).unwrap(),
                    format!("{suffix}.jpg"),
                )
                .unwrap(),
                DumpOperationControl::new(DumpTimeouts::default(), DumpCancellation::default())
                    .unwrap(),
                &store,
            )
            .unwrap();
        server.join().unwrap();
    }

    #[test]
    fn only_named_validated_policy_invokes_injected_hook_before_direct_request() {
        let called = Arc::new(AtomicUsize::new(0));
        let hook: Arc<dyn ValidatedDumpInitializer> = Arc::new(RecordingInitializer {
            called: Arc::clone(&called),
        });
        let registry = BTreeMap::from([("known-recipe".to_owned(), hook)]);

        run_policy_capture(
            DumpInitializationPolicy::Auto,
            &registry,
            &called,
            0,
            "auto",
        );
        run_policy_capture(
            DumpInitializationPolicy::DirectOnly,
            &registry,
            &called,
            0,
            "direct",
        );
        assert_eq!(called.load(Ordering::SeqCst), 0);

        run_policy_capture(
            DumpInitializationPolicy::ValidatedRecipe {
                recipe_id: "known-recipe".to_owned(),
            },
            &registry,
            &called,
            1,
            "validated",
        );
        assert_eq!(called.load(Ordering::SeqCst), 1);

        assert!(matches!(
            Cv610DumpService::new(
                "missing",
                Cv610DumpEndpoint {
                    address: "127.0.0.1".parse().unwrap(),
                    port: 1,
                },
                DumpInitializationPolicy::ValidatedRecipe {
                    recipe_id: "unknown".to_owned(),
                },
                &registry,
                1024,
            ),
            Err(DumpServiceError::InitializationRecipeUnavailable { .. })
        ));
    }

    fn jpeg_response(width: u16, height: u16, payload: &[u8]) -> Vec<u8> {
        let mut metadata = [0_u8; 48];
        metadata[..8].copy_from_slice(b"OTSI_JPG");
        metadata[8..10].copy_from_slice(&width.to_le_bytes());
        metadata[10..12].copy_from_slice(&height.to_le_bytes());
        metadata[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let mut response = vec![0xc0, 0xd0];
        response.extend_from_slice(&1_u32.to_le_bytes());
        response.extend_from_slice(&((metadata.len() + payload.len() + 4) as u32).to_le_bytes());
        response.extend_from_slice(&metadata);
        response.extend_from_slice(payload);
        let checksum = metadata
            .iter()
            .chain(payload)
            .fold(0_u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
        response.extend_from_slice(&checksum.to_le_bytes());
        response
    }
}
