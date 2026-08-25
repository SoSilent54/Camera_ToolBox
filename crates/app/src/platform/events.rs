//! Controller 向前端暴露的 scoped Dump job events。

use camera_toolbox_core::AssetId;

use super::{
    CommandResult, CommandServiceError, DumpServiceError, DumpStage, OperationId,
    RemoteDiscoverySource, RemoteOpenDisposition, RemoteServiceError, RemoteStage,
    RemoteWatchEvent, StreamServiceEvent, StreamSessionId,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpJobEvent {
    pub operation_id: OperationId,
    pub service_id: String,
    pub state: DumpJobState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DumpJobState {
    Queued,
    Running(DumpStage),
    Succeeded {
        asset_id: AssetId,
        payload_sha256: String,
    },
    Failed(DumpServiceError),
}

impl DumpJobState {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded { .. } | Self::Failed(_))
    }
}

#[derive(Debug, Clone)]
pub struct RemoteJobEvent {
    pub operation_id: OperationId,
    pub service_id: String,
    pub state: RemoteJobState,
}

#[derive(Debug, Clone)]
pub enum RemoteJobState {
    Queued,
    Running(RemoteStage),
    CommandCompleted(CommandResult),
    AssetReady {
        asset_id: AssetId,
        payload_sha256: String,
        source: RemoteDiscoverySource,
        open: RemoteOpenDisposition,
    },
    Watch(RemoteWatchEvent),
    Failed(RemoteJobFailure),
}

impl RemoteJobState {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::CommandCompleted(_)
                | Self::AssetReady { .. }
                | Self::Watch(RemoteWatchEvent::Terminal(_))
                | Self::Failed(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteJobFailure {
    Command(CommandServiceError),
    RemoteFile(RemoteServiceError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSessionEvent {
    pub session_id: StreamSessionId,
    pub service_id: String,
    pub event: StreamServiceEvent,
}

impl StreamSessionEvent {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self.event, StreamServiceEvent::Terminal(_))
    }
}
