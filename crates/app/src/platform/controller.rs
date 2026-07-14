//! Dump job 的后台提交、取消与 event channel。

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
};

use thiserror::Error;

use crate::CaptureStore;

use super::{
    CapabilityVariant, DumpCancellation, DumpJobEvent, DumpJobState, DumpOperationControl,
    DumpServiceError, DumpTimeouts, LatestDecodedFrameSlot, OperationId, RemoteDiscoveryRequest,
    RemoteFileRequest, RemoteJobEvent, RemoteJobFailure, RemoteJobState, RemoteOpenDisposition,
    RemoteOperationControl, RemoteServiceError, RemoteTimeouts, RemoteWatchEvent,
    RemoteWatchRequest, ResolvedTargetBindings, StreamCancellation, StreamOpenRequest,
    StreamOperationControl, StreamServiceError, StreamSession, StreamSessionEvent, StreamSessionId,
    StreamTimeouts, TargetResolutionSnapshot, TypedCommandRequest, VerifiedDumpKind,
    VerifiedDumpRequest,
};

struct ControllerStreamSession {
    session: Arc<StreamSession>,
    service_id: String,
    _snapshot: Arc<TargetResolutionSnapshot>,
}

#[derive(Clone)]
struct RemoteEventBus {
    events: Arc<Mutex<VecDeque<RemoteJobEvent>>>,
    terminals: Arc<Mutex<BTreeMap<OperationId, RemoteJobEvent>>>,
}

impl RemoteEventBus {
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(VecDeque::with_capacity(REMOTE_EVENT_CAPACITY))),
            terminals: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn send(&self, event: RemoteJobEvent) -> Result<(), ()> {
        if event.state.is_terminal() {
            self.terminals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(event.operation_id.clone(), event);
            return Ok(());
        }
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(event.state, RemoteJobState::Running(_))
            && let Some(existing) = events.iter_mut().find(|existing| {
                existing.operation_id == event.operation_id
                    && matches!(existing.state, RemoteJobState::Running(_))
            })
        {
            *existing = event;
            return Ok(());
        }
        if events.len() == REMOTE_EVENT_CAPACITY {
            if let Some(index) = events.iter().position(remote_event_disposable) {
                events.remove(index);
            } else {
                events.pop_front();
            }
        }
        events.push_back(event);
        Ok(())
    }

    fn try_recv(&self) -> Option<RemoteJobEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .or_else(|| {
                self.terminals
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_first()
                    .map(|(_, event)| event)
            })
    }

    fn discard_nonterminal(&self, operation_id: &OperationId) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|event| event.operation_id != *operation_id);
    }
}

fn remote_event_disposable(event: &RemoteJobEvent) -> bool {
    matches!(
        event.state,
        RemoteJobState::Running(_)
            | RemoteJobState::Watch(RemoteWatchEvent::CandidateFailed { .. })
    )
}

/// 共享 store 上的轻量后台 controller；每个 still Dump 各自拥有线程和 socket。
pub struct PlatformController {
    store: CaptureStore,
    sender: Sender<DumpJobEvent>,
    receiver: Mutex<Receiver<DumpJobEvent>>,
    remote_bus: RemoteEventBus,
    stream_events: Arc<Mutex<VecDeque<StreamSessionEvent>>>,
    stream_terminals: Arc<Mutex<BTreeMap<StreamSessionId, StreamSessionEvent>>>,
    jobs: Arc<Mutex<BTreeMap<OperationId, DumpCancellation>>>,
    sessions: Arc<Mutex<BTreeMap<StreamSessionId, Arc<ControllerStreamSession>>>>,
    next_operation: AtomicU64,
    next_session: AtomicU64,
}

impl PlatformController {
    #[must_use]
    pub fn new(store: CaptureStore) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            store,
            sender,
            receiver: Mutex::new(receiver),
            remote_bus: RemoteEventBus::new(),
            stream_events: Arc::new(Mutex::new(VecDeque::with_capacity(STREAM_EVENT_CAPACITY))),
            stream_terminals: Arc::new(Mutex::new(BTreeMap::new())),
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            next_operation: AtomicU64::new(1),
            next_session: AtomicU64::new(1),
        }
    }

    /// 在后台执行 resolved CV610 Dump handle，并立即返回 operation id。
    ///
    /// # Errors
    ///
    /// snapshot 没有 Dump、variant 被 capability 收窄、timeout 无效、job 表损坏或线程创建失败时返回错误。
    pub fn submit_dump(
        &self,
        snapshot: Arc<TargetResolutionSnapshot>,
        request: VerifiedDumpRequest,
        timeouts: DumpTimeouts,
    ) -> Result<OperationId, DumpSubmitError> {
        let ResolvedTargetBindings::Cv610(bindings) = snapshot.bindings.as_ref() else {
            return Err(DumpSubmitError::DumpUnavailable);
        };
        let handle = bindings
            .dump
            .as_ref()
            .ok_or(DumpSubmitError::DumpUnavailable)?;
        let variant = capability_variant(request.kind);
        if !handle.descriptor.supported_variants.contains(&variant) {
            return Err(DumpSubmitError::UnsupportedVariant(request.kind));
        }

        let sequence = self.next_operation.fetch_add(1, Ordering::Relaxed);
        if sequence == 0 {
            return Err(DumpSubmitError::OperationIdExhausted);
        }
        let operation_id = OperationId::new(format!("dump-{sequence}"))
            .map_err(|_| DumpSubmitError::OperationIdExhausted)?;
        let cancellation = DumpCancellation::default();
        let service = Arc::clone(&handle.service);
        let service_id = service.service_id().to_owned();
        let reporter_sender = self.sender.clone();
        let reporter_operation = operation_id.clone();
        let reporter_service_id = service_id.clone();
        let reporter = Arc::new(move |stage| {
            let _ = reporter_sender.send(DumpJobEvent {
                operation_id: reporter_operation.clone(),
                service_id: reporter_service_id.clone(),
                state: DumpJobState::Running(stage),
            });
        });
        let control =
            DumpOperationControl::new(timeouts, cancellation.clone())?.with_reporter(reporter);

        self.jobs
            .lock()
            .map_err(|_| DumpSubmitError::JobRegistryPoisoned)?
            .insert(operation_id.clone(), cancellation);
        let _ = self.sender.send(DumpJobEvent {
            operation_id: operation_id.clone(),
            service_id: service_id.clone(),
            state: DumpJobState::Queued,
        });

        let store = self.store.clone();
        let jobs = Arc::clone(&self.jobs);
        let thread_sender = self.sender.clone();
        let thread_operation = operation_id.clone();
        let thread_service_id = service_id.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("cv610-{}", operation_id.as_str()))
            .spawn(move || {
                let _snapshot = snapshot;
                let result = service.capture(thread_operation.clone(), request, control, &store);
                let state = match result {
                    Ok(result) => DumpJobState::Succeeded {
                        asset_id: result.asset.id.clone(),
                        payload_sha256: result.payload_sha256,
                    },
                    Err(error) => DumpJobState::Failed(error),
                };
                let _ = thread_sender.send(DumpJobEvent {
                    operation_id: thread_operation.clone(),
                    service_id: thread_service_id,
                    state,
                });
                jobs.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&thread_operation);
            });
        if let Err(error) = spawn {
            self.jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&operation_id);
            let reason = error.to_string();
            let _ = self.sender.send(DumpJobEvent {
                operation_id: operation_id.clone(),
                service_id,
                state: DumpJobState::Failed(DumpServiceError::Transport {
                    stage: super::DumpStage::Connecting,
                    reason: format!("failed to spawn dump worker: {reason}"),
                }),
            });
            return Err(DumpSubmitError::ThreadSpawn(reason));
        }
        Ok(operation_id)
    }

    /// 请求取消；已结束或不存在的 operation 返回 `false`。
    #[must_use]
    pub fn cancel(&self, operation_id: &OperationId) -> bool {
        let cancellation = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(operation_id)
            .cloned();
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
            true
        } else {
            false
        }
    }

    /// 非阻塞读取一个 event。
    pub fn try_recv(&self) -> Result<Option<DumpJobEvent>, DumpSubmitError> {
        match self
            .receiver
            .lock()
            .map_err(|_| DumpSubmitError::EventReceiverPoisoned)?
            .try_recv()
        {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => Ok(None),
        }
    }

    pub fn submit_command(
        &self,
        snapshot: Arc<TargetResolutionSnapshot>,
        request: TypedCommandRequest,
        timeouts: RemoteTimeouts,
    ) -> Result<OperationId, RemoteSubmitError> {
        let timeouts = timeouts.validate()?;
        let ResolvedTargetBindings::SshManaged(bindings) = snapshot.bindings.as_ref() else {
            return Err(RemoteSubmitError::CommandUnavailable);
        };
        let service = Arc::clone(
            &bindings
                .command
                .as_ref()
                .ok_or(RemoteSubmitError::CommandUnavailable)?
                .service,
        );
        let (operation_id, cancellation) =
            self.begin_remote_job("ssh-command", service.service_id())?;
        let reporter_sender = self.remote_bus.clone();
        let reporter_operation = operation_id.clone();
        let reporter_service = service.service_id().to_owned();
        let reporter = Arc::new(move |stage| {
            let _ = reporter_sender.send(RemoteJobEvent {
                operation_id: reporter_operation.clone(),
                service_id: reporter_service.clone(),
                state: RemoteJobState::Running(stage),
            });
        });
        let control = RemoteOperationControl::new(timeouts, cancellation)?.with_reporter(reporter);
        let sender = self.remote_bus.clone();
        let jobs = Arc::clone(&self.jobs);
        let thread_operation = operation_id.clone();
        let service_id = service.service_id().to_owned();
        let spawn = std::thread::Builder::new()
            .name(format!("ssh-command-{}", operation_id.as_str()))
            .spawn(move || {
                let _snapshot = snapshot;
                let state = match service.execute(request, control) {
                    Ok(result) => RemoteJobState::CommandCompleted(result),
                    Err(error) => RemoteJobState::Failed(RemoteJobFailure::Command(error)),
                };
                let _ = sender.send(RemoteJobEvent {
                    operation_id: thread_operation.clone(),
                    service_id,
                    state,
                });
                jobs.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&thread_operation);
            });
        self.finish_remote_spawn(operation_id, spawn)
    }

    pub fn submit_remote_fetch(
        &self,
        snapshot: Arc<TargetResolutionSnapshot>,
        request: RemoteFileRequest,
        timeouts: RemoteTimeouts,
    ) -> Result<OperationId, RemoteSubmitError> {
        let timeouts = timeouts.validate()?;
        let ResolvedTargetBindings::SshManaged(bindings) = snapshot.bindings.as_ref() else {
            return Err(RemoteSubmitError::RemoteFileUnavailable);
        };
        let service = Arc::clone(
            &bindings
                .remote_file
                .as_ref()
                .ok_or(RemoteSubmitError::RemoteFileUnavailable)?
                .service,
        );
        let (operation_id, cancellation) =
            self.begin_remote_job("ssh-fetch", service.service_id())?;
        let reporter_sender = self.remote_bus.clone();
        let reporter_operation = operation_id.clone();
        let reporter_service = service.service_id().to_owned();
        let control = RemoteOperationControl::new(timeouts, cancellation)?.with_reporter(Arc::new(
            move |stage| {
                let _ = reporter_sender.send(RemoteJobEvent {
                    operation_id: reporter_operation.clone(),
                    service_id: reporter_service.clone(),
                    state: RemoteJobState::Running(stage),
                });
            },
        ));
        let sender = self.remote_bus.clone();
        let jobs = Arc::clone(&self.jobs);
        let store = self.store.clone();
        let thread_operation = operation_id.clone();
        let service_id = service.service_id().to_owned();
        let spawn = std::thread::Builder::new()
            .name(format!("ssh-fetch-{}", operation_id.as_str()))
            .spawn(move || {
                let _snapshot = snapshot;
                let state = match service.fetch(thread_operation.clone(), request, control, &store)
                {
                    Ok(result) => RemoteJobState::AssetReady {
                        asset_id: result.asset.id.clone(),
                        payload_sha256: result.payload_sha256,
                        source: result.discovery_source,
                        open: RemoteOpenDisposition::None,
                    },
                    Err(error) => RemoteJobState::Failed(RemoteJobFailure::RemoteFile(error)),
                };
                let _ = sender.send(RemoteJobEvent {
                    operation_id: thread_operation.clone(),
                    service_id,
                    state,
                });
                jobs.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&thread_operation);
            });
        self.finish_remote_spawn(operation_id, spawn)
    }

    pub fn submit_remote_discover(
        &self,
        snapshot: Arc<TargetResolutionSnapshot>,
        request: RemoteDiscoveryRequest,
        timeouts: RemoteTimeouts,
    ) -> Result<OperationId, RemoteSubmitError> {
        let timeouts = timeouts.validate()?;
        let ResolvedTargetBindings::SshManaged(bindings) = snapshot.bindings.as_ref() else {
            return Err(RemoteSubmitError::RemoteFileUnavailable);
        };
        let service = Arc::clone(
            &bindings
                .remote_file
                .as_ref()
                .ok_or(RemoteSubmitError::RemoteFileUnavailable)?
                .service,
        );
        let (operation_id, cancellation) =
            self.begin_remote_job("ssh-discover", service.service_id())?;
        let reporter_sender = self.remote_bus.clone();
        let reporter_operation = operation_id.clone();
        let reporter_service = service.service_id().to_owned();
        let control = RemoteOperationControl::new(timeouts, cancellation)?.with_reporter(Arc::new(
            move |stage| {
                let _ = reporter_sender.send(RemoteJobEvent {
                    operation_id: reporter_operation.clone(),
                    service_id: reporter_service.clone(),
                    state: RemoteJobState::Running(stage),
                });
            },
        ));
        let sender = self.remote_bus.clone();
        let jobs = Arc::clone(&self.jobs);
        let store = self.store.clone();
        let thread_operation = operation_id.clone();
        let service_id = service.service_id().to_owned();
        let spawn = std::thread::Builder::new()
            .name(format!("ssh-discover-{}", operation_id.as_str()))
            .spawn(move || {
                let _snapshot = snapshot;
                let state = match service.discover_and_fetch(
                    thread_operation.clone(),
                    request,
                    control,
                    &store,
                ) {
                    Ok(mut results) if results.len() == 1 => {
                        let result = results.remove(0);
                        RemoteJobState::AssetReady {
                            asset_id: result.asset.id.clone(),
                            payload_sha256: result.payload_sha256,
                            source: result.discovery_source,
                            open: RemoteOpenDisposition::None,
                        }
                    }
                    Ok(results) => RemoteJobState::Failed(RemoteJobFailure::RemoteFile(
                        RemoteServiceError::InvalidRequest(format!(
                            "one-shot remote capture expected one artifact, got {}",
                            results.len()
                        )),
                    )),
                    Err(error) => RemoteJobState::Failed(RemoteJobFailure::RemoteFile(error)),
                };
                let _ = sender.send(RemoteJobEvent {
                    operation_id: thread_operation.clone(),
                    service_id,
                    state,
                });
                jobs.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&thread_operation);
            });
        self.finish_remote_spawn(operation_id, spawn)
    }

    pub fn submit_remote_watch(
        &self,
        snapshot: Arc<TargetResolutionSnapshot>,
        request: RemoteWatchRequest,
        timeouts: RemoteTimeouts,
    ) -> Result<OperationId, RemoteSubmitError> {
        let timeouts = timeouts.validate()?;
        let ResolvedTargetBindings::SshManaged(bindings) = snapshot.bindings.as_ref() else {
            return Err(RemoteSubmitError::RemoteFileUnavailable);
        };
        let service = Arc::clone(
            &bindings
                .remote_file
                .as_ref()
                .ok_or(RemoteSubmitError::RemoteFileUnavailable)?
                .service,
        );
        let (operation_id, cancellation) =
            self.begin_remote_job("ssh-watch", service.service_id())?;
        let control = RemoteOperationControl::new(timeouts, cancellation)?;
        let sender = self.remote_bus.clone();
        let jobs = Arc::clone(&self.jobs);
        let store = self.store.clone();
        let thread_operation = operation_id.clone();
        let service_id = service.service_id().to_owned();
        let watch_sender = self.remote_bus.clone();
        let watch_operation = operation_id.clone();
        let watch_service = service.service_id().to_owned();
        let reporter = Arc::new(move |event: RemoteWatchEvent| {
            let _ = watch_sender.send(RemoteJobEvent {
                operation_id: watch_operation.clone(),
                service_id: watch_service.clone(),
                state: RemoteJobState::Watch(event),
            });
        });
        let spawn = std::thread::Builder::new()
            .name(format!("ssh-watch-{}", operation_id.as_str()))
            .spawn(move || {
                let _snapshot = snapshot;
                if let Err(error) =
                    service.watch(thread_operation.clone(), request, control, &store, reporter)
                {
                    let _ = sender.send(RemoteJobEvent {
                        operation_id: thread_operation.clone(),
                        service_id,
                        state: RemoteJobState::Failed(RemoteJobFailure::RemoteFile(error)),
                    });
                }
                jobs.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&thread_operation);
            });
        self.finish_remote_spawn(operation_id, spawn)
    }

    pub fn try_recv_remote(&self) -> Result<Option<RemoteJobEvent>, RemoteSubmitError> {
        Ok(self.remote_bus.try_recv())
    }

    fn begin_remote_job(
        &self,
        prefix: &str,
        service_id: &str,
    ) -> Result<(OperationId, DumpCancellation), RemoteSubmitError> {
        let sequence = self.next_operation.fetch_add(1, Ordering::Relaxed);
        if sequence == 0 {
            return Err(RemoteSubmitError::OperationIdExhausted);
        }
        let operation_id = OperationId::new(format!("{prefix}-{sequence}"))
            .map_err(|_| RemoteSubmitError::OperationIdExhausted)?;
        let cancellation = DumpCancellation::default();
        self.jobs
            .lock()
            .map_err(|_| RemoteSubmitError::JobRegistryPoisoned)?
            .insert(operation_id.clone(), cancellation.clone());
        let _ = self.remote_bus.send(RemoteJobEvent {
            operation_id: operation_id.clone(),
            service_id: service_id.to_owned(),
            state: RemoteJobState::Queued,
        });
        Ok((operation_id, cancellation))
    }

    fn finish_remote_spawn(
        &self,
        operation_id: OperationId,
        spawn: std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<OperationId, RemoteSubmitError> {
        if let Err(error) = spawn {
            self.jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&operation_id);
            self.remote_bus.discard_nonterminal(&operation_id);
            return Err(RemoteSubmitError::ThreadSpawn(error.to_string()));
        }
        Ok(operation_id)
    }

    /// 启动 resolved CV610 StreamService；仅创建 worker，不在调用线程执行网络 I/O。
    pub fn submit_stream(
        &self,
        snapshot: Arc<TargetResolutionSnapshot>,
        request: StreamOpenRequest,
        timeouts: StreamTimeouts,
    ) -> Result<StreamSessionId, StreamSubmitError> {
        request.validate()?;
        let ResolvedTargetBindings::Cv610(bindings) = snapshot.bindings.as_ref() else {
            return Err(StreamSubmitError::StreamUnavailable);
        };
        let handle = bindings
            .stream
            .as_ref()
            .ok_or(StreamSubmitError::StreamUnavailable)?;
        if !handle
            .descriptor
            .supported_variants
            .iter()
            .any(|variant| matches!(variant, CapabilityVariant::H264 | CapabilityVariant::H265))
        {
            return Err(StreamSubmitError::StreamUnavailable);
        }
        let concurrent_limit = handle
            .descriptor
            .hard_limits
            .max_concurrent_operations
            .unwrap_or(1) as usize;
        let service_id = handle.service.service_id().to_owned();
        let active_for_service = self
            .sessions
            .lock()
            .map_err(|_| StreamSubmitError::SessionRegistryPoisoned)?
            .values()
            .filter(|session| session.service_id == service_id)
            .count();
        if active_for_service >= concurrent_limit {
            return Err(StreamSubmitError::TooManySessions {
                limit: concurrent_limit,
            });
        }
        let sequence = self.next_session.fetch_add(1, Ordering::Relaxed);
        if sequence == 0 {
            return Err(StreamSubmitError::SessionIdExhausted);
        }
        let session_id = StreamSessionId::new(format!("stream-{sequence}"))
            .map_err(|_| StreamSubmitError::SessionIdExhausted)?;
        let cancellation = StreamCancellation::default();
        let events = Arc::clone(&self.stream_events);
        let terminals = Arc::clone(&self.stream_terminals);
        let reporter_session = session_id.clone();
        let reporter_service = service_id.clone();
        let reporter = Arc::new(move |event| {
            push_stream_event(
                &events,
                &terminals,
                StreamSessionEvent {
                    session_id: reporter_session.clone(),
                    service_id: reporter_service.clone(),
                    event,
                },
            );
        });
        let control = StreamOperationControl::new(timeouts, cancellation, reporter)?;
        let session = handle.service.open(session_id.clone(), request, control)?;
        if session.session_id != session_id {
            return Err(StreamSubmitError::SessionIdentityMismatch);
        }
        self.sessions
            .lock()
            .map_err(|_| StreamSubmitError::SessionRegistryPoisoned)?
            .insert(
                session_id.clone(),
                Arc::new(ControllerStreamSession {
                    session: Arc::new(session),
                    service_id,
                    _snapshot: snapshot,
                }),
            );
        Ok(session_id)
    }

    #[must_use]
    pub fn request_stream_close(&self, session_id: &StreamSessionId) -> bool {
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned();
        if let Some(session) = session {
            session.session.request_close();
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn force_stream_cleanup(&self, session_id: &StreamSessionId) -> bool {
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned();
        session.is_some_and(|session| session.session.force_cleanup())
    }

    #[must_use]
    pub fn latest_stream_frame(
        &self,
        session_id: &StreamSessionId,
    ) -> Option<Arc<LatestDecodedFrameSlot>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .map(|session| Arc::clone(&session.session.latest_frame))
    }

    pub fn try_recv_stream(&self) -> Result<Option<StreamSessionEvent>, StreamSubmitError> {
        let event = self
            .stream_events
            .lock()
            .map_err(|_| StreamSubmitError::EventReceiverPoisoned)?
            .pop_front()
            .or_else(|| {
                self.stream_terminals
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_first()
                    .map(|(_, event)| event)
            });
        if let Some(event) = event.as_ref()
            && matches!(event.event, super::StreamServiceEvent::Terminal(_))
        {
            // latest frame 的 Arc 已由 live document 持有；controller 只释放 session transport ownership。
            self.sessions
                .lock()
                .map_err(|_| StreamSubmitError::SessionRegistryPoisoned)?
                .remove(&event.session_id);
        }
        Ok(event)
    }

    /// GUI 退出时对所有 live session 发出 transport close，不等待 worker。
    pub fn close_all_streams(&self) {
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for session in sessions {
            session.session.request_close();
        }
    }

    /// 进程退出没有 GUI deadline；立即关闭 transport 并执行本地 sidecar cleanup。
    pub fn force_close_all_streams(&self) {
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for session in sessions {
            session.session.request_close();
            let _ = session.session.force_cleanup();
        }
    }
}

const fn capability_variant(kind: VerifiedDumpKind) -> CapabilityVariant {
    match kind {
        VerifiedDumpKind::Raw10 => CapabilityVariant::Raw10,
        VerifiedDumpKind::Raw12 => CapabilityVariant::Raw12,
        VerifiedDumpKind::Jpeg => CapabilityVariant::Jpeg,
        VerifiedDumpKind::Nv21 => CapabilityVariant::Nv21,
    }
}

const REMOTE_EVENT_CAPACITY: usize = 128;
const STREAM_EVENT_CAPACITY: usize = 64;

/// Metrics 按 session 合并；终态独立按 session 保存，永不参与可丢弃事件队列。
fn push_stream_event(
    events: &Mutex<VecDeque<StreamSessionEvent>>,
    terminals: &Mutex<BTreeMap<StreamSessionId, StreamSessionEvent>>,
    event: StreamSessionEvent,
) {
    let is_terminal = matches!(event.event, super::StreamServiceEvent::Terminal(_));
    let mut events = events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if matches!(event.event, super::StreamServiceEvent::Metrics(_))
        && let Some(existing) = events.iter_mut().find(|existing| {
            existing.session_id == event.session_id
                && matches!(existing.event, super::StreamServiceEvent::Metrics(_))
        })
    {
        *existing = event;
        return;
    }
    if events.len() == STREAM_EVENT_CAPACITY {
        if let Some(index) = events.iter().position(disposable_stream_event) {
            events.remove(index);
        } else if is_terminal {
            drop(events);
            terminals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(event.session_id.clone(), event);
            return;
        } else {
            return;
        }
    }
    events.push_back(event);
}

fn disposable_stream_event(event: &StreamSessionEvent) -> bool {
    matches!(
        event.event,
        super::StreamServiceEvent::Metrics(_) | super::StreamServiceEvent::Stage(_)
    )
}

#[derive(Debug, Error)]
pub enum RemoteSubmitError {
    #[error("resolved target does not provide SSH-managed command")]
    CommandUnavailable,
    #[error("resolved target does not provide SSH-managed remote files")]
    RemoteFileUnavailable,
    #[error("operation id space exhausted")]
    OperationIdExhausted,
    #[error("remote job registry is poisoned")]
    JobRegistryPoisoned,
    #[error("remote event receiver is poisoned")]
    EventReceiverPoisoned,
    #[error("failed to spawn remote worker: {0}")]
    ThreadSpawn(String),
    #[error(transparent)]
    Control(#[from] RemoteServiceError),
}

#[derive(Debug, Error)]
pub enum StreamSubmitError {
    #[error("resolved target does not provide CV610 live stream")]
    StreamUnavailable,
    #[error("stream session concurrency limit {limit} reached")]
    TooManySessions { limit: usize },
    #[error("stream session id space exhausted")]
    SessionIdExhausted,
    #[error("stream service returned a mismatched session id")]
    SessionIdentityMismatch,
    #[error("stream session registry is poisoned")]
    SessionRegistryPoisoned,
    #[error("stream event queue is poisoned")]
    EventReceiverPoisoned,
    #[error(transparent)]
    Service(#[from] StreamServiceError),
}

#[derive(Debug, Error)]
pub enum DumpSubmitError {
    #[error("resolved target does not provide CV610 still Dump")]
    DumpUnavailable,
    #[error("resolved target does not support requested variant {0:?}")]
    UnsupportedVariant(VerifiedDumpKind),
    #[error("operation id space exhausted")]
    OperationIdExhausted,
    #[error("dump job registry is poisoned")]
    JobRegistryPoisoned,
    #[error("dump event receiver is poisoned")]
    EventReceiverPoisoned,
    #[error("failed to spawn dump worker: {0}")]
    ThreadSpawn(String),
    #[error(transparent)]
    Control(#[from] DumpServiceError),
}
