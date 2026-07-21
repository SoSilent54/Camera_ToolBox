//! 候选平台能力、稀疏 Sensor×Platform cell 与无副作用解析器。

use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};

use super::{
    eeprom::EepromProvisionService,
    ports::{DumpService, StreamService},
    remote::{CommandService, RemoteFileService},
};
use thiserror::Error;

use super::{
    profile::{
        CapabilityResolutionKey, PlatformProfileId, SensorModeKey, SensorModeSnapshot,
        SensorSelection,
    },
    snapshot::SnapshotHash,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformKind {
    Local,
    HisiliconCv610,
    SshManaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    LocalOpen,
    Dump,
    Stream,
    RemoteFile,
    Command,
    RegisterRead,
    RegisterWrite,
    ExposureControl,
    EepromProvision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityVariant {
    LocalRaw,
    Raw10,
    Raw12,
    Jpeg,
    Nv21,
    H264,
    H265,
    RemoteFetch,
    RemoteWatch,
    StandardExecCommand,
    ArgvSubsystemCommand,
    RegisterAccess,
    Exposure,
    EepromFullProvision,
    EepromUpdateCalibration,
    EepromInspectDryRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityRequirement {
    PlatformOnly,
    SensorScoped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceMaturity {
    Unknown,
    ProfileDeclared,
    UserConfirmed,
    ProtocolVerified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitializationState {
    NotRequired,
    Unknown,
    DirectProbe,
    ValidatedRecipe { recipe_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrencyPolicy {
    Independent,
    Exclusive,
    UnverifiedExclusive,
}

/// 候选能力的硬限制；`None` 表示该维度不适用，不表示无界。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityLimits {
    pub max_payload_bytes: Option<u64>,
    pub max_concurrent_operations: Option<u32>,
}

/// provider bind 后的候选事实，不携带 Sensor/Mode 推断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformCapabilityDescriptor {
    pub capability: CapabilityKind,
    pub requirement: CapabilityRequirement,
    pub provider: PlatformKind,
    pub driver: String,
    pub evidence: EvidenceMaturity,
    pub minimum_sensor_evidence: EvidenceMaturity,
    pub initialization: InitializationState,
    pub concurrency: ConcurrencyPolicy,
    pub hard_limits: CapabilityLimits,
    pub supported_variants: BTreeSet<CapabilityVariant>,
    pub platform_snapshot_hash: SnapshotHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRestriction {
    pub capability: CapabilityKind,
    pub allowed_variants: BTreeSet<CapabilityVariant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidencedCapabilityDeclaration {
    pub capability: CapabilityKind,
    pub supported: bool,
    pub evidence: EvidenceMaturity,
}

/// 可选、稀疏的精确 `(SensorId, ModeId) × PlatformProfileId` cell。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorPlatformCapabilityCell {
    pub sensor_mode: SensorModeKey,
    pub platform_id: PlatformProfileId,
    pub restrictions: Vec<CapabilityRestriction>,
    pub declarations: Vec<EvidencedCapabilityDeclaration>,
}

impl SensorPlatformCapabilityCell {
    /// 对 cell 全量 typed 字段计算确定性 SHA-256。
    #[must_use]
    pub fn snapshot_hash(&self) -> SnapshotHash {
        let mut hash = SnapshotHash::builder("camera-toolbox/capability-cell/v1");
        hash.string(self.sensor_mode.sensor_id.as_str());
        hash.string(&self.sensor_mode.mode_id);
        hash.string(self.platform_id.as_str());
        hash.u64(self.restrictions.len() as u64);
        for capability in ALL_CAPABILITY_KINDS {
            if let Some(restriction) = self
                .restrictions
                .iter()
                .find(|restriction| restriction.capability == capability)
            {
                hash.u8(capability_kind_tag(restriction.capability));
                hash.u64(restriction.allowed_variants.len() as u64);
                for variant in &restriction.allowed_variants {
                    hash.u8(capability_variant_tag(*variant));
                }
            }
        }
        hash.u64(self.declarations.len() as u64);
        for capability in ALL_CAPABILITY_KINDS {
            if let Some(declaration) = self
                .declarations
                .iter()
                .find(|declaration| declaration.capability == capability)
            {
                hash.u8(capability_kind_tag(declaration.capability));
                hash.boolean(declaration.supported);
                hash.u8(evidence_tag(declaration.evidence));
            }
        }
        hash.finish()
    }
}

const ALL_CAPABILITY_KINDS: [CapabilityKind; 9] = [
    CapabilityKind::LocalOpen,
    CapabilityKind::Dump,
    CapabilityKind::Stream,
    CapabilityKind::RemoteFile,
    CapabilityKind::Command,
    CapabilityKind::RegisterRead,
    CapabilityKind::RegisterWrite,
    CapabilityKind::ExposureControl,
    CapabilityKind::EepromProvision,
];

const fn capability_kind_tag(value: CapabilityKind) -> u8 {
    value as u8
}

const fn capability_variant_tag(value: CapabilityVariant) -> u8 {
    value as u8
}

const fn evidence_tag(value: EvidenceMaturity) -> u8 {
    value as u8
}

/// 稀疏 capability matrix；查询只接受精确三元主键。
#[derive(Debug, Clone, Default)]
pub struct SensorPlatformCapabilityMatrix {
    cells: std::collections::BTreeMap<
        (SensorModeKey, PlatformProfileId),
        SensorPlatformCapabilityCell,
    >,
}

impl SensorPlatformCapabilityMatrix {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cells: std::collections::BTreeMap::new(),
        }
    }
    /// 插入 cell，拒绝重复主键。
    ///
    /// # Errors
    ///
    /// 相同 Sensor/Mode/Platform cell 已存在时返回错误。
    pub fn insert(
        &mut self,
        mut cell: SensorPlatformCapabilityCell,
    ) -> Result<(), CapabilityResolutionError> {
        validate_cell(&cell)?;
        cell.restrictions
            .sort_by_key(|restriction| restriction.capability);
        cell.declarations
            .sort_by_key(|declaration| declaration.capability);
        let key = (cell.sensor_mode.clone(), cell.platform_id.clone());
        if self.cells.contains_key(&key) {
            return Err(CapabilityResolutionError::DuplicateCapabilityCell);
        }
        self.cells.insert(key, cell);
        Ok(())
    }

    pub(crate) fn upsert(
        &mut self,
        mut cell: SensorPlatformCapabilityCell,
    ) -> Result<(), CapabilityResolutionError> {
        validate_cell(&cell)?;
        cell.restrictions
            .sort_by_key(|restriction| restriction.capability);
        cell.declarations
            .sort_by_key(|declaration| declaration.capability);
        let key = (cell.sensor_mode.clone(), cell.platform_id.clone());
        self.cells.insert(key, cell);
        Ok(())
    }

    pub(crate) fn remove(&mut self, sensor_mode: &SensorModeKey, platform_id: &PlatformProfileId) {
        self.cells
            .remove(&(sensor_mode.clone(), platform_id.clone()));
    }

    #[must_use]
    pub fn get(
        &self,
        sensor_mode: &SensorModeKey,
        platform_id: &PlatformProfileId,
    ) -> Option<&SensorPlatformCapabilityCell> {
        self.cells.get(&(sensor_mode.clone(), platform_id.clone()))
    }

    pub fn iter(&self) -> impl Iterator<Item = &SensorPlatformCapabilityCell> {
        self.cells.values()
    }
}

fn cell_key_matches(
    cell: &SensorPlatformCapabilityCell,
    sensor_mode: &SensorModeKey,
    platform_id: &PlatformProfileId,
) -> bool {
    cell.sensor_mode == *sensor_mode && cell.platform_id == *platform_id
}

fn validate_cell(cell: &SensorPlatformCapabilityCell) -> Result<(), CapabilityResolutionError> {
    let mut restrictions = BTreeSet::new();
    for restriction in &cell.restrictions {
        if !restrictions.insert(restriction.capability) {
            return Err(CapabilityResolutionError::DuplicateCellRestriction(
                restriction.capability,
            ));
        }
    }
    let mut declarations = BTreeSet::new();
    for declaration in &cell.declarations {
        if !declarations.insert(declaration.capability) {
            return Err(CapabilityResolutionError::DuplicateCellDeclaration(
                declaration.capability,
            ));
        }
    }
    Ok(())
}

/// provider 内部候选 handle；service 的 Arc 在解析时直接复用。
pub struct PlatformCapabilityHandle<S: ?Sized> {
    pub service: Arc<S>,
    pub descriptor: Arc<PlatformCapabilityDescriptor>,
}

impl<S: ?Sized> Clone for PlatformCapabilityHandle<S> {
    fn clone(&self) -> Self {
        Self {
            service: Arc::clone(&self.service),
            descriptor: Arc::clone(&self.descriptor),
        }
    }
}

/// resolver 输出的 handle；descriptor 必定为本次 key 新派生的不可变对象。
pub struct ResolvedCapabilityHandle<S: ?Sized> {
    pub service: Arc<S>,
    pub descriptor: Arc<ResolvedCapabilityDescriptor>,
}

impl<S: ?Sized> Clone for ResolvedCapabilityHandle<S> {
    fn clone(&self) -> Self {
        Self {
            service: Arc::clone(&self.service),
            descriptor: Arc::clone(&self.descriptor),
        }
    }
}

/// Slice A 的 owned service 边界；operation methods 在后续 slice 的具体端口中定义。
/// 稳定 service id 用于 job/event 诊断，且不会把 provider transport 类型擦除。
pub trait LocalOpenService: Send + Sync {
    fn service_id(&self) -> &str;
}

/// 某一 resolution key 下的最终能力事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCapabilityDescriptor {
    pub capability: CapabilityKind,
    pub requirement: CapabilityRequirement,
    pub provider: PlatformKind,
    pub driver: String,
    pub resolution_key: CapabilityResolutionKey,
    pub evidence: EvidenceMaturity,
    pub initialization: InitializationState,
    pub concurrency: ConcurrencyPolicy,
    pub hard_limits: CapabilityLimits,
    pub supported_variants: BTreeSet<CapabilityVariant>,
    pub platform_snapshot_hash: SnapshotHash,
    pub sensor_mode_snapshot_hash: Option<SnapshotHash>,
    pub capability_cell_snapshot_hash: Option<SnapshotHash>,
    pub aggregate_hash: SnapshotHash,
}

/// Local provider 只持有本地打开能力，不混入网络字段。
#[derive(Clone)]
pub struct LocalBindings {
    pub platform_id: PlatformProfileId,
    pub platform_snapshot_hash: SnapshotHash,
    pub open: Option<PlatformCapabilityHandle<dyn LocalOpenService>>,
}

impl LocalBindings {
    /// # Errors
    ///
    /// handle descriptor 与 Local/provider snapshot 不一致时返回错误。
    pub fn new(
        platform_id: PlatformProfileId,
        platform_snapshot_hash: SnapshotHash,
        open: Option<PlatformCapabilityHandle<dyn LocalOpenService>>,
    ) -> Result<Self, CapabilityResolutionError> {
        validate_handle(
            PlatformKind::Local,
            CapabilityKind::LocalOpen,
            CapabilityRequirement::PlatformOnly,
            platform_snapshot_hash,
            open.as_ref(),
        )?;
        Ok(Self {
            platform_id,
            platform_snapshot_hash,
            open,
        })
    }
}

/// CV610 当前只暴露已经设计的 `PlatformOnly` Dump/Stream handles。
#[derive(Clone)]
pub struct Cv610Bindings {
    pub platform_id: PlatformProfileId,
    pub platform_snapshot_hash: SnapshotHash,
    pub dump: Option<PlatformCapabilityHandle<dyn DumpService>>,
    pub stream: Option<PlatformCapabilityHandle<dyn StreamService>>,
}

impl Cv610Bindings {
    /// # Errors
    ///
    /// 任一 handle descriptor 与 CV610/provider snapshot/capability 不一致时返回错误。
    pub fn new(
        platform_id: PlatformProfileId,
        platform_snapshot_hash: SnapshotHash,
        dump: Option<PlatformCapabilityHandle<dyn DumpService>>,
        stream: Option<PlatformCapabilityHandle<dyn StreamService>>,
    ) -> Result<Self, CapabilityResolutionError> {
        validate_handle(
            PlatformKind::HisiliconCv610,
            CapabilityKind::Dump,
            CapabilityRequirement::PlatformOnly,
            platform_snapshot_hash,
            dump.as_ref(),
        )?;
        validate_handle(
            PlatformKind::HisiliconCv610,
            CapabilityKind::Stream,
            CapabilityRequirement::PlatformOnly,
            platform_snapshot_hash,
            stream.as_ref(),
        )?;
        Ok(Self {
            platform_id,
            platform_snapshot_hash,
            dump,
            stream,
        })
    }
}

/// SSH-managed 暴露 PlatformOnly 文件/命令能力与精确 Sensor×Platform gated EEPROM helper。
#[derive(Clone)]
pub struct SshManagedBindings {
    pub platform_id: PlatformProfileId,
    pub platform_snapshot_hash: SnapshotHash,
    pub remote_file: Option<PlatformCapabilityHandle<dyn RemoteFileService>>,
    pub command: Option<PlatformCapabilityHandle<dyn CommandService>>,
    pub eeprom: Option<PlatformCapabilityHandle<dyn EepromProvisionService>>,
}

impl SshManagedBindings {
    /// # Errors
    ///
    /// 任一 handle descriptor 与 SSH/provider snapshot/capability 不一致时返回错误。
    pub fn new(
        platform_id: PlatformProfileId,
        platform_snapshot_hash: SnapshotHash,
        remote_file: Option<PlatformCapabilityHandle<dyn RemoteFileService>>,
        command: Option<PlatformCapabilityHandle<dyn CommandService>>,
        eeprom: Option<PlatformCapabilityHandle<dyn EepromProvisionService>>,
    ) -> Result<Self, CapabilityResolutionError> {
        validate_handle(
            PlatformKind::SshManaged,
            CapabilityKind::RemoteFile,
            CapabilityRequirement::PlatformOnly,
            platform_snapshot_hash,
            remote_file.as_ref(),
        )?;
        validate_handle(
            PlatformKind::SshManaged,
            CapabilityKind::Command,
            CapabilityRequirement::PlatformOnly,
            platform_snapshot_hash,
            command.as_ref(),
        )?;
        validate_handle(
            PlatformKind::SshManaged,
            CapabilityKind::EepromProvision,
            CapabilityRequirement::SensorScoped,
            platform_snapshot_hash,
            eeprom.as_ref(),
        )?;
        Ok(Self {
            platform_id,
            platform_snapshot_hash,
            remote_file,
            command,
            eeprom,
        })
    }
}

/// enum 本身不再套 Arc；每个 variant 只持有一层对应 typed Arc。
#[derive(Clone)]
pub enum PlatformBindings {
    Local(Arc<LocalBindings>),
    Cv610(Arc<Cv610Bindings>),
    SshManaged(Arc<SshManagedBindings>),
}

impl PlatformBindings {
    #[must_use]
    pub fn platform_id(&self) -> &PlatformProfileId {
        match self {
            Self::Local(bindings) => &bindings.platform_id,
            Self::Cv610(bindings) => &bindings.platform_id,
            Self::SshManaged(bindings) => &bindings.platform_id,
        }
    }

    #[must_use]
    pub const fn platform_kind(&self) -> PlatformKind {
        match self {
            Self::Local(_) => PlatformKind::Local,
            Self::Cv610(_) => PlatformKind::HisiliconCv610,
            Self::SshManaged(_) => PlatformKind::SshManaged,
        }
    }

    #[must_use]
    pub fn platform_snapshot_hash(&self) -> SnapshotHash {
        match self {
            Self::Local(bindings) => bindings.platform_snapshot_hash,
            Self::Cv610(bindings) => bindings.platform_snapshot_hash,
            Self::SshManaged(bindings) => bindings.platform_snapshot_hash,
        }
    }
}

#[derive(Clone)]
pub struct ResolvedLocalBindings {
    pub open: Option<ResolvedCapabilityHandle<dyn LocalOpenService>>,
}

#[derive(Clone)]
pub struct ResolvedCv610Bindings {
    pub dump: Option<ResolvedCapabilityHandle<dyn DumpService>>,
    pub stream: Option<ResolvedCapabilityHandle<dyn StreamService>>,
}

#[derive(Clone)]
pub struct ResolvedSshManagedBindings {
    pub remote_file: Option<ResolvedCapabilityHandle<dyn RemoteFileService>>,
    pub command: Option<ResolvedCapabilityHandle<dyn CommandService>>,
    pub eeprom: Option<ResolvedCapabilityHandle<dyn EepromProvisionService>>,
}

#[derive(Clone)]
pub enum ResolvedTargetBindings {
    Local(ResolvedLocalBindings),
    Cv610(ResolvedCv610Bindings),
    SshManaged(ResolvedSshManagedBindings),
}

impl ResolvedTargetBindings {
    #[must_use]
    pub fn capability(
        &self,
        capability: CapabilityKind,
    ) -> Option<&Arc<ResolvedCapabilityDescriptor>> {
        match (self, capability) {
            (Self::Local(bindings), CapabilityKind::LocalOpen) => {
                bindings.open.as_ref().map(|handle| &handle.descriptor)
            }
            (Self::Cv610(bindings), CapabilityKind::Dump) => {
                bindings.dump.as_ref().map(|handle| &handle.descriptor)
            }
            (Self::Cv610(bindings), CapabilityKind::Stream) => {
                bindings.stream.as_ref().map(|handle| &handle.descriptor)
            }
            (Self::SshManaged(bindings), CapabilityKind::RemoteFile) => bindings
                .remote_file
                .as_ref()
                .map(|handle| &handle.descriptor),
            (Self::SshManaged(bindings), CapabilityKind::Command) => {
                bindings.command.as_ref().map(|handle| &handle.descriptor)
            }
            (Self::SshManaged(bindings), CapabilityKind::EepromProvision) => {
                bindings.eeprom.as_ref().map(|handle| &handle.descriptor)
            }
            _ => None,
        }
    }
}

/// job/session 提交时持有的完整、不可变能力解析快照。
#[derive(Clone)]
pub struct TargetResolutionSnapshot {
    pub key: CapabilityResolutionKey,
    pub bindings: Arc<ResolvedTargetBindings>,
    pub platform_snapshot_hash: SnapshotHash,
    pub sensor_mode_snapshot_hash: Option<SnapshotHash>,
    pub capability_cell_snapshot_hash: Option<SnapshotHash>,
    pub aggregate_hash: SnapshotHash,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultCapabilityResolver;

impl DefaultCapabilityResolver {
    /// 从 typed candidate handles 派生完整 target snapshot，不修改 candidate descriptor。
    ///
    /// `PlatformOnly` 在 Unbound 或缺 cell 时透传；`SensorScoped` 只有精确 cell
    /// 的 positive declaration 且 evidence 达标时才可见。
    ///
    /// # Errors
    ///
    /// key、platform、SensorMode 或 cell 主键不一致，或 hash 失败时返回错误。
    pub fn resolve(
        &self,
        key: &CapabilityResolutionKey,
        platform: &PlatformBindings,
        sensor: Option<&SensorModeSnapshot>,
        cell: Option<&SensorPlatformCapabilityCell>,
    ) -> Result<TargetResolutionSnapshot, CapabilityResolutionError> {
        if key.platform_id != *platform.platform_id() {
            return Err(CapabilityResolutionError::PlatformKeyMismatch);
        }
        validate_sensor_input(key, sensor, cell)?;

        let platform_hash = platform.platform_snapshot_hash();
        let sensor_hash = sensor.map(|snapshot| snapshot.snapshot_hash);
        let cell_hash = cell.map(SensorPlatformCapabilityCell::snapshot_hash);
        let aggregate_hash = SnapshotHash::aggregate(platform_hash, sensor_hash, cell_hash);
        let resolved = match platform {
            PlatformBindings::Local(bindings) => {
                ResolvedTargetBindings::Local(ResolvedLocalBindings {
                    open: resolve_typed_handle(
                        key,
                        bindings.open.as_ref(),
                        sensor_hash,
                        cell,
                        cell_hash,
                        aggregate_hash,
                    ),
                })
            }
            PlatformBindings::Cv610(bindings) => {
                ResolvedTargetBindings::Cv610(ResolvedCv610Bindings {
                    dump: resolve_typed_handle(
                        key,
                        bindings.dump.as_ref(),
                        sensor_hash,
                        cell,
                        cell_hash,
                        aggregate_hash,
                    ),
                    stream: resolve_typed_handle(
                        key,
                        bindings.stream.as_ref(),
                        sensor_hash,
                        cell,
                        cell_hash,
                        aggregate_hash,
                    ),
                })
            }
            PlatformBindings::SshManaged(bindings) => {
                ResolvedTargetBindings::SshManaged(ResolvedSshManagedBindings {
                    remote_file: resolve_typed_handle(
                        key,
                        bindings.remote_file.as_ref(),
                        sensor_hash,
                        cell,
                        cell_hash,
                        aggregate_hash,
                    ),
                    command: resolve_typed_handle(
                        key,
                        bindings.command.as_ref(),
                        sensor_hash,
                        cell,
                        cell_hash,
                        aggregate_hash,
                    ),
                    eeprom: resolve_typed_handle(
                        key,
                        bindings.eeprom.as_ref(),
                        sensor_hash,
                        cell,
                        cell_hash,
                        aggregate_hash,
                    ),
                })
            }
        };

        Ok(TargetResolutionSnapshot {
            key: key.clone(),
            bindings: Arc::new(resolved),
            platform_snapshot_hash: platform_hash,
            sensor_mode_snapshot_hash: sensor_hash,
            capability_cell_snapshot_hash: cell_hash,
            aggregate_hash,
        })
    }

    /// 为未来 `SensorScoped` typed service 派生 handle；复用与 target resolve 完全相同的 key/cell gate。
    ///
    /// # Errors
    ///
    /// SensorMode/cell 与 resolution key 不精确匹配时返回错误。
    pub fn resolve_handle<S: ?Sized>(
        &self,
        key: &CapabilityResolutionKey,
        owner_platform_id: &PlatformProfileId,
        handle: &PlatformCapabilityHandle<S>,
        sensor: Option<&SensorModeSnapshot>,
        cell: Option<&SensorPlatformCapabilityCell>,
    ) -> Result<Option<ResolvedCapabilityHandle<S>>, CapabilityResolutionError> {
        if key.platform_id != *owner_platform_id {
            return Err(CapabilityResolutionError::PlatformKeyMismatch);
        }
        validate_sensor_input(key, sensor, cell)?;
        let sensor_hash = sensor.map(|snapshot| snapshot.snapshot_hash);
        let cell_hash = cell.map(SensorPlatformCapabilityCell::snapshot_hash);
        let aggregate_hash = SnapshotHash::aggregate(
            handle.descriptor.platform_snapshot_hash,
            sensor_hash,
            cell_hash,
        );
        Ok(resolve_typed_handle(
            key,
            Some(handle),
            sensor_hash,
            cell,
            cell_hash,
            aggregate_hash,
        ))
    }
}

fn resolve_typed_handle<S: ?Sized>(
    key: &CapabilityResolutionKey,
    handle: Option<&PlatformCapabilityHandle<S>>,
    sensor_hash: Option<SnapshotHash>,
    cell: Option<&SensorPlatformCapabilityCell>,
    cell_hash: Option<SnapshotHash>,
    aggregate_hash: SnapshotHash,
) -> Option<ResolvedCapabilityHandle<S>> {
    let handle = handle?;
    derive_descriptor(
        &handle.descriptor,
        key,
        sensor_hash,
        cell,
        cell_hash,
        aggregate_hash,
    )
    .map(|descriptor| ResolvedCapabilityHandle {
        service: Arc::clone(&handle.service),
        descriptor: Arc::new(descriptor),
    })
}

fn validate_sensor_input(
    key: &CapabilityResolutionKey,
    sensor: Option<&SensorModeSnapshot>,
    cell: Option<&SensorPlatformCapabilityCell>,
) -> Result<(), CapabilityResolutionError> {
    if let Some(cell) = cell {
        validate_cell(cell)?;
    }
    match &key.sensor {
        SensorSelection::Unbound => {
            if sensor.is_some() || cell.is_some() {
                return Err(CapabilityResolutionError::UnexpectedSensorContext);
            }
        }
        SensorSelection::Mode(mode_key) => {
            let sensor = sensor.ok_or(CapabilityResolutionError::MissingSensorSnapshot)?;
            if sensor.key != *mode_key {
                return Err(CapabilityResolutionError::SensorSnapshotKeyMismatch);
            }
            if let Some(cell) = cell
                && !cell_key_matches(cell, mode_key, &key.platform_id)
            {
                return Err(CapabilityResolutionError::CapabilityCellKeyMismatch);
            }
        }
    }
    Ok(())
}

fn derive_descriptor(
    candidate: &PlatformCapabilityDescriptor,
    key: &CapabilityResolutionKey,
    sensor_hash: Option<SnapshotHash>,
    cell: Option<&SensorPlatformCapabilityCell>,
    cell_hash: Option<SnapshotHash>,
    aggregate_hash: SnapshotHash,
) -> Option<ResolvedCapabilityDescriptor> {
    let declaration = cell.and_then(|cell| {
        cell.declarations
            .iter()
            .find(|item| item.capability == candidate.capability)
    });

    match candidate.requirement {
        CapabilityRequirement::PlatformOnly => {
            if declaration.is_some_and(|declaration| !declaration.supported) {
                return None;
            }
        }
        CapabilityRequirement::SensorScoped => {
            if !matches!(key.sensor, SensorSelection::Mode(_)) {
                return None;
            }
            let declaration = declaration?;
            if !declaration.supported || declaration.evidence < candidate.minimum_sensor_evidence {
                return None;
            }
        }
    }

    let mut variants = candidate.supported_variants.clone();
    if let Some(restriction) = cell.and_then(|cell| {
        cell.restrictions
            .iter()
            .find(|item| item.capability == candidate.capability)
    }) {
        variants.retain(|variant| restriction.allowed_variants.contains(variant));
    }
    if variants.is_empty() {
        return None;
    }

    let evidence = declaration.map_or(candidate.evidence, |declaration| {
        candidate.evidence.max(declaration.evidence)
    });
    Some(ResolvedCapabilityDescriptor {
        capability: candidate.capability,
        requirement: candidate.requirement,
        provider: candidate.provider,
        driver: candidate.driver.clone(),
        resolution_key: key.clone(),
        evidence,
        initialization: candidate.initialization.clone(),
        concurrency: candidate.concurrency,
        hard_limits: candidate.hard_limits.clone(),
        supported_variants: variants,
        platform_snapshot_hash: candidate.platform_snapshot_hash,
        sensor_mode_snapshot_hash: sensor_hash,
        capability_cell_snapshot_hash: cell_hash,
        aggregate_hash,
    })
}

fn validate_handle<S: ?Sized>(
    provider: PlatformKind,
    capability: CapabilityKind,
    requirement: CapabilityRequirement,
    platform_hash: SnapshotHash,
    handle: Option<&PlatformCapabilityHandle<S>>,
) -> Result<(), CapabilityResolutionError> {
    let Some(handle) = handle else {
        return Ok(());
    };
    if handle.descriptor.provider != provider {
        return Err(CapabilityResolutionError::BindingVariantMismatch);
    }
    if handle.descriptor.capability != capability {
        return Err(CapabilityResolutionError::HandleCapabilityMismatch);
    }
    if handle.descriptor.requirement != requirement {
        return Err(CapabilityResolutionError::HandleRequirementMismatch);
    }
    if handle.descriptor.supported_variants.is_empty() {
        return Err(CapabilityResolutionError::EmptyCandidateVariants);
    }
    if handle.descriptor.platform_snapshot_hash != platform_hash {
        return Err(CapabilityResolutionError::CandidatePlatformHashMismatch);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum CapabilityResolutionError {
    #[error("resolution key references another platform")]
    PlatformKeyMismatch,
    #[error("unbound resolution cannot include SensorMode or capability cell")]
    UnexpectedSensorContext,
    #[error("bound resolution requires a SensorMode snapshot")]
    MissingSensorSnapshot,
    #[error("SensorMode snapshot does not match resolution key")]
    SensorSnapshotKeyMismatch,
    #[error("capability cell does not exactly match resolution key")]
    CapabilityCellKeyMismatch,
    #[error("duplicate SensorMode × Platform capability cell")]
    DuplicateCapabilityCell,
    #[error("duplicate restriction for capability {0:?} in capability cell")]
    DuplicateCellRestriction(CapabilityKind),
    #[error("duplicate declaration for capability {0:?} in capability cell")]
    DuplicateCellDeclaration(CapabilityKind),
    #[error("binding variant and candidate provider differ")]
    BindingVariantMismatch,
    #[error("typed handle and descriptor capability differ")]
    HandleCapabilityMismatch,
    #[error("candidate descriptor uses another platform snapshot hash")]
    CandidatePlatformHashMismatch,
    #[error("typed handle requirement does not match its binding slot")]
    HandleRequirementMismatch,
    #[error("candidate capability must support at least one variant")]
    EmptyCandidateVariants,
}

#[cfg(test)]
mod tests {
    use camera_toolbox_core::{BayerPattern, RawSpec, sensor::SensorMode};

    use super::*;
    use crate::platform::profile::{PlatformProfileId, SensorId};
    use crate::{
        EepromHelperResult, EepromProvisionOperation, EepromProvisionServiceError,
        RemoteOperationControl,
    };

    struct TestDumpService {
        id: &'static str,
    }
    impl DumpService for TestDumpService {
        fn service_id(&self) -> &str {
            self.id
        }

        fn capture(
            &self,
            _operation_id: crate::OperationId,
            _request: crate::VerifiedDumpRequest,
            _control: crate::DumpOperationControl,
            _store: &crate::CaptureStore,
        ) -> Result<crate::DumpOperationResult, crate::DumpServiceError> {
            Err(crate::DumpServiceError::ProtocolViolation {
                stage: crate::DumpStage::Connecting,
                reason: "test service is not executable".to_owned(),
            })
        }
    }
    struct TestSensorService;

    struct TestEepromService;

    impl EepromProvisionService for TestEepromService {
        fn service_id(&self) -> &str {
            "test-eeprom"
        }

        fn execute(
            &self,
            request: EepromProvisionOperation,
            _control: RemoteOperationControl,
        ) -> Result<EepromHelperResult, EepromProvisionServiceError> {
            Err(EepromProvisionServiceError::TargetNotConfigured(
                request.sensor_mode,
            ))
        }
    }

    fn platform_id(value: &str) -> PlatformProfileId {
        PlatformProfileId::new(value).unwrap()
    }
    fn mode_key(sensor: &str, mode: &str) -> SensorModeKey {
        SensorModeKey {
            sensor_id: SensorId::new(sensor).unwrap(),
            mode_id: mode.to_owned(),
        }
    }
    fn sensor_snapshot(key: SensorModeKey) -> SensorModeSnapshot {
        SensorModeSnapshot::new(
            key.clone(),
            SensorMode {
                id: key.mode_id,
                raw: RawSpec {
                    width: 1920,
                    height: 1080,
                    bit_depth: 12,
                    bayer: BayerPattern::Bggr,
                },
                is_wdr: false,
            },
        )
        .unwrap()
    }
    fn descriptor(
        hash: SnapshotHash,
        kind: CapabilityKind,
        requirement: CapabilityRequirement,
        variants: &[CapabilityVariant],
    ) -> Arc<PlatformCapabilityDescriptor> {
        Arc::new(PlatformCapabilityDescriptor {
            capability: kind,
            requirement,
            provider: PlatformKind::HisiliconCv610,
            driver: "cv610-test".to_owned(),
            evidence: EvidenceMaturity::ProtocolVerified,
            minimum_sensor_evidence: EvidenceMaturity::ProfileDeclared,
            initialization: InitializationState::DirectProbe,
            concurrency: ConcurrencyPolicy::UnverifiedExclusive,
            hard_limits: CapabilityLimits {
                max_payload_bytes: Some(256 * 1024 * 1024),
                max_concurrent_operations: Some(1),
            },
            supported_variants: variants.iter().copied().collect(),
            platform_snapshot_hash: hash,
        })
    }
    fn dump_handle(
        descriptor: Arc<PlatformCapabilityDescriptor>,
    ) -> PlatformCapabilityHandle<dyn DumpService> {
        PlatformCapabilityHandle {
            service: Arc::new(TestDumpService { id: "test-dump" }),
            descriptor,
        }
    }
    fn bindings(
        id: PlatformProfileId,
        hash: SnapshotHash,
        dump: PlatformCapabilityHandle<dyn DumpService>,
    ) -> PlatformBindings {
        PlatformBindings::Cv610(Arc::new(
            Cv610Bindings::new(id, hash, Some(dump), None).unwrap(),
        ))
    }

    #[test]
    fn unbound_passes_platform_only_service_arc() {
        let id = platform_id("cv610-a");
        let hash = SnapshotHash::digest_bytes(b"platform-a");
        let dump = dump_handle(descriptor(
            hash,
            CapabilityKind::Dump,
            CapabilityRequirement::PlatformOnly,
            &[CapabilityVariant::Raw10, CapabilityVariant::Raw12],
        ));
        let service = Arc::clone(&dump.service);
        let platform = bindings(id.clone(), hash, dump);
        let snapshot = DefaultCapabilityResolver
            .resolve(
                &CapabilityResolutionKey {
                    platform_id: id,
                    sensor: SensorSelection::Unbound,
                },
                &platform,
                None,
                None,
            )
            .unwrap();
        let ResolvedTargetBindings::Cv610(resolved) = snapshot.bindings.as_ref() else {
            panic!("expected CV610 bindings")
        };
        assert!(Arc::ptr_eq(
            &service,
            &resolved.dump.as_ref().unwrap().service
        ));
        assert_eq!(
            resolved.dump.as_ref().unwrap().service.service_id(),
            "test-dump"
        );
    }

    #[test]
    fn pair_specific_restriction_does_not_leak() {
        let id = platform_id("cv610-a");
        let hash = SnapshotHash::digest_bytes(b"platform-a");
        let platform = bindings(
            id.clone(),
            hash,
            dump_handle(descriptor(
                hash,
                CapabilityKind::Dump,
                CapabilityRequirement::PlatformOnly,
                &[CapabilityVariant::Raw10, CapabilityVariant::Raw12],
            )),
        );
        let first_key = mode_key("sensor-a", "linear");
        let second_key = mode_key("sensor-b", "linear");
        let cell = SensorPlatformCapabilityCell {
            sensor_mode: first_key.clone(),
            platform_id: id.clone(),
            restrictions: vec![CapabilityRestriction {
                capability: CapabilityKind::Dump,
                allowed_variants: [CapabilityVariant::Raw10].into_iter().collect(),
            }],
            declarations: Vec::new(),
        };
        let first_sensor = sensor_snapshot(first_key.clone());
        let first = DefaultCapabilityResolver
            .resolve(
                &CapabilityResolutionKey {
                    platform_id: id.clone(),
                    sensor: SensorSelection::Mode(first_key),
                },
                &platform,
                Some(&first_sensor),
                Some(&cell),
            )
            .unwrap();
        let second_sensor = sensor_snapshot(second_key.clone());
        let second = DefaultCapabilityResolver
            .resolve(
                &CapabilityResolutionKey {
                    platform_id: id,
                    sensor: SensorSelection::Mode(second_key),
                },
                &platform,
                Some(&second_sensor),
                None,
            )
            .unwrap();
        assert_eq!(
            first
                .bindings
                .capability(CapabilityKind::Dump)
                .unwrap()
                .supported_variants,
            [CapabilityVariant::Raw10].into_iter().collect()
        );
        assert_eq!(
            second
                .bindings
                .capability(CapabilityKind::Dump)
                .unwrap()
                .supported_variants,
            [CapabilityVariant::Raw10, CapabilityVariant::Raw12]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn descriptor_is_derived_without_mutating_candidate() {
        let id = platform_id("cv610-a");
        let hash = SnapshotHash::digest_bytes(b"platform-a");
        let candidate = descriptor(
            hash,
            CapabilityKind::Dump,
            CapabilityRequirement::PlatformOnly,
            &[CapabilityVariant::Raw10, CapabilityVariant::Raw12],
        );
        let platform = bindings(id.clone(), hash, dump_handle(Arc::clone(&candidate)));
        let key = mode_key("sensor-a", "linear");
        let sensor = sensor_snapshot(key.clone());
        let cell = SensorPlatformCapabilityCell {
            sensor_mode: key.clone(),
            platform_id: id.clone(),
            restrictions: vec![CapabilityRestriction {
                capability: CapabilityKind::Dump,
                allowed_variants: [CapabilityVariant::Raw12].into_iter().collect(),
            }],
            declarations: Vec::new(),
        };
        let snapshot = DefaultCapabilityResolver
            .resolve(
                &CapabilityResolutionKey {
                    platform_id: id,
                    sensor: SensorSelection::Mode(key),
                },
                &platform,
                Some(&sensor),
                Some(&cell),
            )
            .unwrap();
        assert_eq!(candidate.supported_variants.len(), 2);
        assert_eq!(
            snapshot
                .bindings
                .capability(CapabilityKind::Dump)
                .unwrap()
                .supported_variants,
            [CapabilityVariant::Raw12].into_iter().collect()
        );
    }

    #[test]
    fn sensor_scoped_generic_handle_requires_exact_cell() {
        let id = platform_id("cv610-a");
        let hash = SnapshotHash::digest_bytes(b"platform-a");
        let handle = PlatformCapabilityHandle {
            service: Arc::new(TestSensorService),
            descriptor: descriptor(
                hash,
                CapabilityKind::RegisterRead,
                CapabilityRequirement::SensorScoped,
                &[CapabilityVariant::RegisterAccess],
            ),
        };
        let unbound_key = CapabilityResolutionKey {
            platform_id: id.clone(),
            sensor: SensorSelection::Unbound,
        };
        assert!(
            DefaultCapabilityResolver
                .resolve_handle(&unbound_key, &id, &handle, None, None)
                .unwrap()
                .is_none()
        );
        let key = mode_key("sensor-a", "linear");
        let sensor = sensor_snapshot(key.clone());
        let resolution_key = CapabilityResolutionKey {
            platform_id: id.clone(),
            sensor: SensorSelection::Mode(key.clone()),
        };
        assert!(
            DefaultCapabilityResolver
                .resolve_handle(&resolution_key, &id, &handle, Some(&sensor), None)
                .unwrap()
                .is_none()
        );
        let cell = SensorPlatformCapabilityCell {
            sensor_mode: key,
            platform_id: id.clone(),
            restrictions: Vec::new(),
            declarations: vec![EvidencedCapabilityDeclaration {
                capability: CapabilityKind::RegisterRead,
                supported: true,
                evidence: EvidenceMaturity::ProfileDeclared,
            }],
        };
        assert!(
            DefaultCapabilityResolver
                .resolve_handle(&resolution_key, &id, &handle, Some(&sensor), Some(&cell))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            DefaultCapabilityResolver
                .resolve_handle(
                    &resolution_key,
                    &platform_id("other"),
                    &handle,
                    Some(&sensor),
                    Some(&cell)
                )
                .err()
                .unwrap()
                .to_string(),
            "resolution key references another platform"
        );
    }

    #[test]
    fn target_snapshot_and_descriptor_carry_every_input_hash() {
        let id = platform_id("cv610-a");
        let platform_hash = SnapshotHash::digest_bytes(b"platform-a");
        let platform = bindings(
            id.clone(),
            platform_hash,
            dump_handle(descriptor(
                platform_hash,
                CapabilityKind::Dump,
                CapabilityRequirement::PlatformOnly,
                &[CapabilityVariant::Raw12],
            )),
        );
        let key = mode_key("sensor-a", "linear");
        let sensor = sensor_snapshot(key.clone());
        let cell = SensorPlatformCapabilityCell {
            sensor_mode: key.clone(),
            platform_id: id.clone(),
            restrictions: Vec::new(),
            declarations: Vec::new(),
        };
        let cell_hash = cell.snapshot_hash();
        let snapshot = DefaultCapabilityResolver
            .resolve(
                &CapabilityResolutionKey {
                    platform_id: id,
                    sensor: SensorSelection::Mode(key),
                },
                &platform,
                Some(&sensor),
                Some(&cell),
            )
            .unwrap();
        let resolved = snapshot.bindings.capability(CapabilityKind::Dump).unwrap();
        assert_eq!(snapshot.platform_snapshot_hash, platform_hash);
        assert_eq!(
            snapshot.sensor_mode_snapshot_hash,
            Some(sensor.snapshot_hash)
        );
        assert_eq!(snapshot.capability_cell_snapshot_hash, Some(cell_hash));
        assert_eq!(resolved.platform_snapshot_hash, platform_hash);
        assert_eq!(
            resolved.sensor_mode_snapshot_hash,
            Some(sensor.snapshot_hash)
        );
        assert_eq!(resolved.capability_cell_snapshot_hash, Some(cell_hash));
        assert_eq!(resolved.aggregate_hash, snapshot.aggregate_hash);
    }

    #[test]
    fn duplicate_cell_rules_are_rejected() {
        let capability = CapabilityKind::Dump;
        let cell = SensorPlatformCapabilityCell {
            sensor_mode: mode_key("sensor-a", "linear"),
            platform_id: platform_id("cv610-a"),
            restrictions: vec![
                CapabilityRestriction {
                    capability,
                    allowed_variants: [CapabilityVariant::Raw10].into_iter().collect(),
                },
                CapabilityRestriction {
                    capability,
                    allowed_variants: [CapabilityVariant::Raw12].into_iter().collect(),
                },
            ],
            declarations: Vec::new(),
        };
        let mut matrix = SensorPlatformCapabilityMatrix::default();
        assert!(matches!(
            matrix.insert(cell),
            Err(CapabilityResolutionError::DuplicateCellRestriction(
                CapabilityKind::Dump
            ))
        ));
    }

    #[test]
    fn eeprom_candidate_requires_positive_exact_sensor_cell() {
        let id = platform_id("ssh-a");
        let platform_hash = SnapshotHash::digest_bytes(b"ssh-platform-a");
        let descriptor = Arc::new(PlatformCapabilityDescriptor {
            capability: CapabilityKind::EepromProvision,
            requirement: CapabilityRequirement::SensorScoped,
            provider: PlatformKind::SshManaged,
            driver: "ssh-eeprom-test".to_owned(),
            evidence: EvidenceMaturity::ProtocolVerified,
            minimum_sensor_evidence: EvidenceMaturity::UserConfirmed,
            initialization: InitializationState::DirectProbe,
            concurrency: ConcurrencyPolicy::Exclusive,
            hard_limits: CapabilityLimits {
                max_payload_bytes: Some(308),
                max_concurrent_operations: Some(1),
            },
            supported_variants: [
                CapabilityVariant::EepromFullProvision,
                CapabilityVariant::EepromUpdateCalibration,
            ]
            .into_iter()
            .collect(),
            platform_snapshot_hash: platform_hash,
        });
        let service: Arc<dyn EepromProvisionService> = Arc::new(TestEepromService);
        let platform = PlatformBindings::SshManaged(Arc::new(
            SshManagedBindings::new(
                id.clone(),
                platform_hash,
                None,
                None,
                Some(PlatformCapabilityHandle {
                    service,
                    descriptor,
                }),
            )
            .unwrap(),
        ));
        let mode = mode_key("sensor-a", "linear");
        let sensor = sensor_snapshot(mode.clone());
        let key = CapabilityResolutionKey {
            platform_id: id.clone(),
            sensor: SensorSelection::Mode(mode.clone()),
        };
        let undeclared = SensorPlatformCapabilityCell {
            sensor_mode: mode.clone(),
            platform_id: id.clone(),
            restrictions: Vec::new(),
            declarations: Vec::new(),
        };
        let snapshot = DefaultCapabilityResolver
            .resolve(&key, &platform, Some(&sensor), Some(&undeclared))
            .unwrap();
        let ResolvedTargetBindings::SshManaged(bindings) = snapshot.bindings.as_ref() else {
            panic!("expected SSH bindings")
        };
        assert!(bindings.eeprom.is_none());

        let declared = SensorPlatformCapabilityCell {
            declarations: vec![EvidencedCapabilityDeclaration {
                capability: CapabilityKind::EepromProvision,
                supported: true,
                evidence: EvidenceMaturity::UserConfirmed,
            }],
            ..undeclared
        };
        let snapshot = DefaultCapabilityResolver
            .resolve(&key, &platform, Some(&sensor), Some(&declared))
            .unwrap();
        let ResolvedTargetBindings::SshManaged(bindings) = snapshot.bindings.as_ref() else {
            panic!("expected SSH bindings")
        };
        let eeprom = bindings.eeprom.as_ref().expect("declaration must resolve");
        assert_eq!(eeprom.service.service_id(), "test-eeprom");
        assert_eq!(
            eeprom.descriptor.supported_variants,
            [
                CapabilityVariant::EepromFullProvision,
                CapabilityVariant::EepromUpdateCalibration,
            ]
            .into_iter()
            .collect()
        );
    }
}
