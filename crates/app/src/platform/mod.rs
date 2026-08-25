//! 平台 profile、候选能力与 Sensor/Mode 精确解析模型。

mod capability;
mod control;
mod controller;
mod eeprom;
mod events;
mod i2c;
mod ports;
mod profile;
mod profile_store;
mod remote;
mod snapshot;

pub use capability::{
    CapabilityKind, CapabilityLimits, CapabilityRequirement, CapabilityResolutionError,
    CapabilityRestriction, CapabilityVariant, ConcurrencyPolicy, Cv610Bindings,
    DefaultCapabilityResolver, EvidenceMaturity, EvidencedCapabilityDeclaration,
    InitializationState, LocalBindings, LocalOpenService, PlatformBindings,
    PlatformCapabilityDescriptor, PlatformCapabilityHandle, PlatformKind,
    ResolvedCapabilityDescriptor, ResolvedCapabilityHandle, ResolvedCv610Bindings,
    ResolvedLocalBindings, ResolvedSshManagedBindings, ResolvedTargetBindings,
    SensorPlatformCapabilityCell, SensorPlatformCapabilityMatrix, SshManagedBindings,
    TargetResolutionSnapshot,
};
pub use control::{
    ControlTargetSpec, EepromExecutor, HexArmControlClient, HexArmJointPositionsRequest,
    HexArmTargetConfig, HexArmTransport, I2cExecutor, SftpFileReader, SshCommandExecutor,
    SshConnection, SshConnectionService, X5ControlClient, X5233CapturePayload,
};
pub use controller::{DumpSubmitError, PlatformController, RemoteSubmitError, StreamSubmitError};
pub use eeprom::{
    EEPROM_EXPERIMENTAL_PROVISION_WARNING, EEPROM_HELPER_SCHEMA_VERSION, EepromDeviceState,
    EepromHelperAction, EepromHelperFailure, EepromHelperOutput, EepromHelperRequest,
    EepromHelperResult, EepromHelperTarget, EepromInspectResult, EepromPageWritePlan,
    EepromProvisionOperation, EepromProvisionService, EepromProvisionServiceError,
    EepromRollbackState, EepromSerialState, EepromWriteResult,
};
pub use events::{
    DumpJobEvent, DumpJobState, RemoteJobEvent, RemoteJobFailure, RemoteJobState,
    StreamSessionEvent,
};
    pub use i2c::{
        I2C_HELPER_MAX_MESSAGE_BYTES, I2C_HELPER_MAX_MESSAGES_PER_TRANSACTION,
        I2C_HELPER_MAX_REQUEST_BYTES, I2C_HELPER_MAX_TOTAL_READ_BYTES,
        I2C_HELPER_MAX_TRANSACTIONS_PER_REQUEST, I2C_HELPER_SCHEMA_VERSION, I2cBusInfo,
        I2cExecutionReport, I2cGuardedWriteRequest, I2cHelperAction, I2cHelperFailure,
        I2cHelperOperation, I2cHelperOutput, I2cHelperRequest, I2cHelperRequestValidationError,
        I2cHelperResult, I2cHelperService, I2cHelperServiceError, I2cChecksumValidation,
        I2cMapValidationContract, I2cMessageData, I2cMessageDirection, I2cMessageFlag,
        I2cMessageResult, I2cMessageSpec,
        I2cPageExecutionReport, I2cPageWrite, I2cReadRange, I2cReadReport, I2cReadRequest,
        I2cTaskExecutor, I2cTaskTarget, I2cTransactionResult, I2cTransactionSpec, I2cWriteRequest,
        validate_i2c_helper_action, validate_i2c_transfer_transactions, validate_map_image,
    };
pub use ports::{
    DecodedVideoFrame, DumpCancellation, DumpEnvelope, DumpOperationControl, DumpOperationResult,
    DumpService, DumpServiceError, DumpSourceDescriptor, DumpStage, DumpTimeouts,
    DumpTruncationStage, LatestDecodedFrameSlot, RecordingBranch, RecordingState, SourcePts,
    SourcePtsProvenance, StreamCancellation, StreamCodec, StreamFrameIdentity, StreamMediaInfo,
    StreamMetrics, StreamOpenRequest, StreamOperationControl, StreamRecordingRequest,
    StreamService, StreamServiceError, StreamServiceEvent, StreamSession, StreamStage,
    StreamTerminal, StreamTimeouts, VerifiedDumpKind, VerifiedDumpRequest, host_monotonic_time_ns,
};
pub use profile::{
    CapabilityResolutionKey, Cv610Config, Cv610DumpConfig, Cv610StreamConfig,
    DumpInitializationPolicy, LocalConfig, PlatformConfig, PlatformProfile, PlatformProfileId,
    ProfileError, RtspCodec, RtspLatencyMode, RtspStreamConfig, RtspTransport, SensorCatalog,
    SensorDescriptor, SensorId, SensorModeKey, SensorModeSnapshot, SensorSelection,
    SshManagedConfig, validate_ssh_host,
};
pub use profile_store::{PROFILE_STORE_SCHEMA_VERSION, ProfileStore, ProfileStoreError};
pub use remote::{
    CommandParameter, CommandResult, CommandService, CommandServiceError, CommandTerminal,
    RemoteDiscoveryRequest, RemoteDiscoverySource, RemoteFileOperationResult, RemoteFileRequest,
    RemoteFileService, RemoteFileStat, RemoteOpenDisposition, RemoteOperationControl,
    RemoteServiceError, RemoteStage, RemoteTimeouts, RemoteWatchEvent, RemoteWatchReporter,
    RemoteWatchRequest, RemoteWatchTerminal, TypedCommandRequest,
};
pub use snapshot::SnapshotHash;

/// controller operation 的稳定标识；CaptureStore 用它累计单操作 reservation。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(String);

impl OperationId {
    /// 创建非空 operation id。
    ///
    /// # Errors
    ///
    /// id 为空或只包含空白时返回错误。
    pub fn new(value: impl Into<String>) -> Result<Self, OperationIdError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(OperationIdError);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("operation id must not be empty")]
pub struct OperationIdError;

/// 单条持久 live stream 会话的稳定标识。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamSessionId(String);

impl StreamSessionId {
    pub fn new(value: impl Into<String>) -> Result<Self, StreamSessionIdError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(StreamSessionIdError);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stream session id must not be empty")]
pub struct StreamSessionIdError;
