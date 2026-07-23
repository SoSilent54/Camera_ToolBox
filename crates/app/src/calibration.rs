//! 标定数据集会话与结果安装事务。

use std::{collections::HashMap, sync::Arc};

use camera_toolbox_core::{
    BoardSpec, CalibrationDataError, CalibrationImageSize, CalibrationRequest, CalibrationSolution,
    ChessboardDetection, ChessboardDetectionOutcome, InitialIntrinsics,
};
use thiserror::Error;

use crate::{
    FileRef, FileSystem, FileSystemError, FileVersion, FsControl, ReadRequest, StreamFrameIdentity,
    StreamSessionId,
};

/// `OpenCV` 标定至少需要多个不同姿态；UI readiness 使用这一保守下限。
pub const MIN_CALIBRATION_VIEWS: usize = 3;

/// 直播帧进入标定数据集时的稳定身份；时间戳只保留在 provenance，不能参与幂等键。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StreamCaptureId {
    pub stream_id: StreamSessionId,
    pub channel: u16,
    pub frame_sequence: u64,
}

impl From<&StreamFrameIdentity> for StreamCaptureId {
    fn from(identity: &StreamFrameIdentity) -> Self {
        Self {
            stream_id: identity.stream_id.clone(),
            channel: identity.channel,
            frame_sequence: identity.frame_sequence,
        }
    }
}

/// 标定输入不等同于文件系统对象；直播快照不得伪造成 `FileRef`。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CalibrationInputKey {
    File(FileRef),
    StreamCapture(StreamCaptureId),
}

impl CalibrationInputKey {
    #[must_use]
    pub fn file_reference(&self) -> Option<&FileRef> {
        match self {
            Self::File(reference) => Some(reference),
            Self::StreamCapture(_) => None,
        }
    }
}

impl From<FileRef> for CalibrationInputKey {
    fn from(reference: FileRef) -> Self {
        Self::File(reference)
    }
}

/// 每次检测必须绑定不可变输入 revision，避免异步结果覆盖已替换的 source。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CalibrationInputRevision {
    File(FileVersion),
    EphemeralPng {
        content_sha256: String,
        encoded_bytes: u64,
    },
}

impl CalibrationInputRevision {
    #[must_use]
    pub const fn encoded_bytes(&self) -> u64 {
        match self {
            Self::File(version) => version.size,
            Self::EphemeralPng { encoded_bytes, .. } => *encoded_bytes,
        }
    }

    #[must_use]
    pub const fn file_version(&self) -> Option<FileVersion> {
        match self {
            Self::File(version) => Some(*version),
            Self::EphemeralPng { .. } => None,
        }
    }
}

impl From<FileVersion> for CalibrationInputRevision {
    fn from(version: FileVersion) -> Self {
        Self::File(version)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationEncodedPng {
    pub bytes: Arc<[u8]>,
    pub image_size: CalibrationImageSize,
    pub source_revision: CalibrationInputRevision,
}

/// 通过统一文件端口有界读取 `PNG`，并在进入 `OpenCV` 前解析 `IHDR` 尺寸。
///
/// # Errors
///
/// 源版本变化、读取越界或 PNG header 无效时返回错误。
pub fn read_calibration_png(
    file_system: &dyn FileSystem,
    reference: &FileRef,
    expected_version: FileVersion,
    max_encoded_bytes: u64,
    control: &FsControl,
) -> Result<CalibrationEncodedPng, CalibrationInputError> {
    let entry = file_system.stat(reference, control)?;
    if entry.version != expected_version {
        return Err(CalibrationInputError::SourceChanged {
            expected: expected_version,
            actual: entry.version,
        });
    }
    if entry.version.size > max_encoded_bytes {
        return Err(CalibrationInputError::EncodedImageTooLarge {
            size: entry.version.size,
            limit: max_encoded_bytes,
        });
    }
    let capacity = usize::try_from(entry.version.size).map_err(|_| {
        CalibrationInputError::EncodedImageTooLarge {
            size: entry.version.size,
            limit: usize::MAX as u64,
        }
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    let outcome = file_system.read(
        reference,
        ReadRequest {
            offset: 0,
            max_bytes: max_encoded_bytes,
        },
        control,
        &mut |chunk| {
            bytes.extend_from_slice(chunk);
            Ok(())
        },
    )?;
    if outcome.source_version != expected_version || outcome.bytes_read != entry.version.size {
        return Err(CalibrationInputError::SourceChanged {
            expected: expected_version,
            actual: outcome.source_version,
        });
    }
    let image_size = parse_png_dimensions(&bytes)?;
    Ok(CalibrationEncodedPng {
        bytes: Arc::from(bytes),
        image_size,
        source_revision: outcome.source_version.into(),
    })
}

fn parse_png_dimensions(bytes: &[u8]) -> Result<CalibrationImageSize, CalibrationInputError> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24
        || &bytes[..8] != PNG_SIGNATURE
        || u32::from_be_bytes(bytes[8..12].try_into().expect("fixed slice")) != 13
        || &bytes[12..16] != b"IHDR"
    {
        return Err(CalibrationInputError::InvalidPngHeader);
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("fixed slice"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("fixed slice"));
    CalibrationImageSize::new(width, height).map_err(CalibrationInputError::InvalidData)
}

#[derive(Debug, Error, PartialEq)]
pub enum CalibrationInputError {
    #[error(transparent)]
    FileSystem(#[from] FileSystemError),
    #[error("calibration source changed: expected {expected:?}, got {actual:?}")]
    SourceChanged {
        expected: FileVersion,
        actual: FileVersion,
    },
    #[error("encoded calibration image is {size} bytes, limit is {limit} bytes")]
    EncodedImageTooLarge { size: u64, limit: u64 },
    #[error("calibration detector accepts a PNG with a valid IHDR header only")]
    InvalidPngHeader,
    #[error(transparent)]
    InvalidData(#[from] CalibrationDataError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CalibrationItemId(u64);

impl CalibrationItemId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CalibrationItemStatus {
    Pending,
    ReadQueued,
    Reading,
    DetectQueued,
    Detecting,
    Found(ChessboardDetection),
    NotFound { image_size: CalibrationImageSize },
    Failed(String),
}

impl CalibrationItemStatus {
    #[must_use]
    pub const fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::ReadQueued | Self::Reading | Self::DetectQueued | Self::Detecting
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationDatasetItem {
    pub id: CalibrationItemId,
    pub input: CalibrationInputKey,
    pub revision: CalibrationInputRevision,
    pub display_name: String,
    pub enabled: bool,
    pub status: CalibrationItemStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationJobToken {
    pub item_id: CalibrationItemId,
    detection_epoch: u64,
    job_id: u64,
    source_revision: CalibrationInputRevision,
    board: BoardSpec,
}

impl CalibrationJobToken {
    #[must_use]
    pub const fn board(&self) -> BoardSpec {
        self.board
    }

    #[must_use]
    pub fn source_revision(&self) -> &CalibrationInputRevision {
        &self.source_revision
    }
}

/// 自动候选的会话内身份；它在 Dataset item 创建前存在。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AutoCandidateId(u64);

impl AutoCandidateId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// 自动候选从冻结 PNG 到最终提交都必须保持一致的不可变绑定。
#[derive(Clone, Debug, PartialEq)]
pub struct AutoCandidateToken {
    id: AutoCandidateId,
    input: CalibrationInputKey,
    source_revision: CalibrationInputRevision,
    display_name: String,
    frame_identity: StreamFrameIdentity,
    board: BoardSpec,
    fit_manifest_revision: u64,
    admission_revision: u64,
}

impl AutoCandidateToken {
    #[must_use]
    pub const fn id(&self) -> AutoCandidateId {
        self.id
    }

    #[must_use]
    pub const fn board(&self) -> BoardSpec {
        self.board
    }

    #[must_use]
    pub fn source_revision(&self) -> &CalibrationInputRevision {
        &self.source_revision
    }

    #[must_use]
    pub fn frame_identity(&self) -> &StreamFrameIdentity {
        &self.frame_identity
    }

    #[must_use]
    pub const fn admission_revision(&self) -> u64 {
        self.admission_revision
    }
}

/// authoritative detection 与其原始候选 token 的单一提交对象。
#[derive(Clone, Debug, PartialEq)]
pub struct AutoCandidateCommit {
    token: AutoCandidateToken,
    observed_revision: CalibrationInputRevision,
    detection: ChessboardDetection,
}

impl AutoCandidateCommit {
    #[must_use]
    pub const fn new(
        token: AutoCandidateToken,
        observed_revision: CalibrationInputRevision,
        detection: ChessboardDetection,
    ) -> Self {
        Self {
            token,
            observed_revision,
            detection,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationSnapshot {
    pub item_ids: Vec<CalibrationItemId>,
    pub request: CalibrationRequest,
    solution_revision: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstalledCalibration {
    pub item_ids: Vec<CalibrationItemId>,
    pub request: CalibrationRequest,
    pub solution: CalibrationSolution,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddCalibrationItemOutcome {
    Added(CalibrationItemId),
    AlreadyPresent(CalibrationItemId),
    SourceChanged(CalibrationItemId),
}

#[derive(Clone, Debug)]
pub struct CalibrationSession {
    board: BoardSpec,
    items: Vec<CalibrationDatasetItem>,
    selected: Option<CalibrationItemId>,
    installed: Option<InstalledCalibration>,
    solution_revision: u64,
    detection_epoch: u64,
    auto_admission_revision: u64,
    active_detection_jobs: HashMap<CalibrationItemId, u64>,
    next_detection_job_id: u64,
    next_id: u64,
}

impl CalibrationSession {
    #[must_use]
    pub fn new(board: BoardSpec) -> Self {
        Self {
            board,
            items: Vec::new(),
            selected: None,
            installed: None,
            solution_revision: 1,
            detection_epoch: 1,
            auto_admission_revision: 1,
            active_detection_jobs: HashMap::new(),
            next_detection_job_id: 1,
            next_id: 1,
        }
    }

    #[must_use]
    pub const fn board(&self) -> BoardSpec {
        self.board
    }

    #[must_use]
    pub fn items(&self) -> &[CalibrationDatasetItem] {
        &self.items
    }

    #[must_use]
    pub const fn selected(&self) -> Option<CalibrationItemId> {
        self.selected
    }

    #[must_use]
    pub fn installed(&self) -> Option<&InstalledCalibration> {
        self.installed.as_ref()
    }

    /// 更新棋盘定义。
    ///
    /// 内角点行列改变时，既有检测与新 pattern 不再匹配，全部重置为 `Pending`；
    /// 仅相邻角点物理尺寸改变时，保留像素检测结果，只使既有标定解失效。
    ///
    /// # Errors
    ///
    /// 棋盘参数无效时返回错误。
    pub fn set_board(&mut self, board: BoardSpec) -> Result<(), CalibrationSessionError> {
        board.validate()?;
        if self.board == board {
            return Ok(());
        }
        let corner_layout_changed =
            self.board.inner_cols != board.inner_cols || self.board.inner_rows != board.inner_rows;
        self.board = board;
        for item in &mut self.items {
            if corner_layout_changed || item.status.is_busy() {
                item.status = CalibrationItemStatus::Pending;
            }
        }
        self.invalidate_detection_epoch();
        Ok(())
    }

    /// 清空全部检测与标定结果，保留 Dataset、选择和 Use 状态。
    pub fn reset_detections(&mut self) {
        for item in &mut self.items {
            item.status = CalibrationItemStatus::Pending;
        }
        self.invalidate_detection_epoch();
    }

    pub fn add_or_refresh(
        &mut self,
        input: impl Into<CalibrationInputKey>,
        revision: impl Into<CalibrationInputRevision>,
        display_name: String,
    ) -> AddCalibrationItemOutcome {
        let input = input.into();
        let revision = revision.into();
        if let Some(index) = self.items.iter().position(|item| item.input == input) {
            let item = &mut self.items[index];
            if item.revision == revision {
                return AddCalibrationItemOutcome::AlreadyPresent(item.id);
            }
            item.revision = revision;
            item.display_name = display_name;
            item.status = CalibrationItemStatus::Pending;
            let id = item.id;
            self.invalidate_detection_epoch();
            return AddCalibrationItemOutcome::SourceChanged(id);
        }

        let id = CalibrationItemId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.items.push(CalibrationDatasetItem {
            id,
            input,
            revision,
            display_name,
            enabled: true,
            status: CalibrationItemStatus::Pending,
        });
        self.selected.get_or_insert(id);
        self.invalidate_detection_epoch();
        AddCalibrationItemOutcome::Added(id)
    }

    /// 为尚未入库的直播 PNG 创建不可变候选绑定，不修改 Dataset。
    ///
    /// # Errors
    ///
    /// 输入不是匹配 identity 的 stream PNG、输入已存在或 revision 无效时返回错误。
    pub fn bind_auto_candidate(
        &self,
        id: AutoCandidateId,
        frame_identity: StreamFrameIdentity,
        source_revision: CalibrationInputRevision,
        display_name: String,
    ) -> Result<AutoCandidateToken, CalibrationSessionError> {
        let input = CalibrationInputKey::StreamCapture(StreamCaptureId::from(&frame_identity));
        if self.items.iter().any(|item| item.input == input) {
            return Err(CalibrationSessionError::AutoCandidateAlreadyPresent);
        }
        if !matches!(
            &source_revision,
            CalibrationInputRevision::EphemeralPng {
                content_sha256,
                encoded_bytes,
            } if !content_sha256.is_empty() && *encoded_bytes > 0
        ) {
            return Err(CalibrationSessionError::InvalidAutoCandidateRevision);
        }
        Ok(AutoCandidateToken {
            id,
            input,
            source_revision,
            display_name,
            frame_identity,
            board: self.board,
            fit_manifest_revision: self.solution_revision,
            admission_revision: self.auto_admission_revision,
        })
    }

    /// 原子提交已通过 authoritative detection 的自动候选。
    ///
    /// # Errors
    ///
    /// 任一不可变 binding 已过期、输入已存在或角点无效时返回错误，且 Dataset 不变。
    pub fn commit_auto_candidate(
        &mut self,
        commit: AutoCandidateCommit,
    ) -> Result<CalibrationItemId, CalibrationSessionError> {
        let AutoCandidateCommit {
            token,
            observed_revision,
            detection,
        } = commit;
        if token.board != self.board
            || token.fit_manifest_revision != self.solution_revision
            || token.admission_revision != self.auto_admission_revision
        {
            return Err(CalibrationSessionError::StaleAutoCandidate);
        }
        if observed_revision != token.source_revision {
            return Err(CalibrationSessionError::InvalidAutoCandidateRevision);
        }
        let expected_input =
            CalibrationInputKey::StreamCapture(StreamCaptureId::from(&token.frame_identity));
        if token.input != expected_input {
            return Err(CalibrationSessionError::InvalidAutoCandidateIdentity);
        }
        if self.items.iter().any(|item| item.input == token.input) {
            return Err(CalibrationSessionError::AutoCandidateAlreadyPresent);
        }
        detection.validate(token.board)?;

        let id = CalibrationItemId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.items.push(CalibrationDatasetItem {
            id,
            input: token.input,
            revision: token.source_revision,
            display_name: token.display_name,
            enabled: true,
            status: CalibrationItemStatus::Found(detection),
        });
        self.selected = Some(id);
        // 自动候选只改变 solver 输入；不得使其他 Dataset 检测令牌过期。
        self.invalidate_solution();
        Ok(id)
    }
    /// 使所有尚未提交的自动候选失效；不影响 Dataset detection token。
    pub fn invalidate_auto_admission(&mut self) {
        self.auto_admission_revision = self.auto_admission_revision.wrapping_add(1).max(1);
    }

    /// # Errors
    ///
    /// `id` 不属于当前数据集时返回错误。
    pub fn set_selected(&mut self, id: CalibrationItemId) -> Result<(), CalibrationSessionError> {
        self.item(id)?;
        self.selected = Some(id);
        Ok(())
    }

    /// # Errors
    ///
    /// `id` 不属于当前数据集时返回错误。
    pub fn set_enabled(
        &mut self,
        id: CalibrationItemId,
        enabled: bool,
    ) -> Result<(), CalibrationSessionError> {
        let item = self.item_mut(id)?;
        if item.enabled != enabled {
            item.enabled = enabled;
            self.invalidate_detection_epoch();
        }
        Ok(())
    }

    /// # Errors
    ///
    /// `id` 不属于当前数据集时返回错误。
    pub fn remove(&mut self, id: CalibrationItemId) -> Result<(), CalibrationSessionError> {
        let index = self
            .items
            .iter()
            .position(|item| item.id == id)
            .ok_or(CalibrationSessionError::UnknownItem(id))?;
        self.items.remove(index);
        if self.selected == Some(id) {
            self.selected = self
                .items
                .get(index.min(self.items.len().saturating_sub(1)))
                .map(|item| item.id);
        }
        self.invalidate_detection_epoch();
        Ok(())
    }

    pub fn clear(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.items.clear();
        self.selected = None;
        self.invalidate_detection_epoch();
    }

    /// 为文件输入创建检测令牌；读取 worker 取到任务后才转为 `Reading`。
    ///
    /// # Errors
    ///
    /// 数据项不存在或已有任务在途时返回错误。
    pub fn begin_detection(
        &mut self,
        id: CalibrationItemId,
    ) -> Result<CalibrationJobToken, CalibrationSessionError> {
        self.begin_detection_with_status(id, CalibrationItemStatus::ReadQueued)
    }

    /// 为已冻结、已预检的 encoded PNG 直接进入 detection queue 创建令牌。
    ///
    /// 调用方必须紧接着将任务放入 `pending_loaded`；channel 满时该状态保持为
    /// `DetectQueued`，直到 worker 发出 `Started`。
    ///
    /// # Errors
    ///
    /// 数据项不存在或已有任务在途时返回错误。
    pub fn begin_encoded_detection(
        &mut self,
        id: CalibrationItemId,
    ) -> Result<CalibrationJobToken, CalibrationSessionError> {
        self.begin_detection_with_status(id, CalibrationItemStatus::DetectQueued)
    }

    fn begin_detection_with_status(
        &mut self,
        id: CalibrationItemId,
        initial_status: CalibrationItemStatus,
    ) -> Result<CalibrationJobToken, CalibrationSessionError> {
        if self.active_detection_jobs.contains_key(&id) {
            return Err(CalibrationSessionError::ItemBusy(id));
        }
        let board = self.board;
        let source_revision = {
            let item = self.item_mut(id)?;
            if item.status.is_busy() {
                return Err(CalibrationSessionError::ItemBusy(id));
            }
            item.status = initial_status;
            item.revision.clone()
        };
        let job_id = self.next_detection_job_id;
        self.next_detection_job_id = self.next_detection_job_id.wrapping_add(1).max(1);
        self.active_detection_jobs.insert(id, job_id);
        self.invalidate_solution();
        Ok(CalibrationJobToken {
            item_id: id,
            detection_epoch: self.detection_epoch,
            job_id,
            source_revision,
            board,
        })
    }

    /// 将已进入 I/O worker 的读取任务从排队状态切换为活动读取。
    ///
    /// # Errors
    ///
    /// 令牌已过期、数据项不存在或任务不在读取队列中时返回错误。
    pub fn mark_reading(
        &mut self,
        token: &CalibrationJobToken,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_active_token(token)?;
        if !matches!(
            self.item(token.item_id)?.status,
            CalibrationItemStatus::ReadQueued
        ) {
            return Err(CalibrationSessionError::StaleResult);
        }
        self.item_mut(token.item_id)?.status = CalibrationItemStatus::Reading;
        Ok(())
    }

    /// 读取成功后标记为待检测；任务仍在检测 worker 的队列中。
    ///
    /// # Errors
    ///
    /// 令牌已过期、数据项不存在或读取尚未完成时返回错误。
    pub fn mark_detect_queued(
        &mut self,
        token: &CalibrationJobToken,
    ) -> Result<(), CalibrationSessionError> {
        self.mark_detection_queued_from(token, CalibrationItemStatus::Reading)
    }

    fn mark_detection_queued_from(
        &mut self,
        token: &CalibrationJobToken,
        expected_status: CalibrationItemStatus,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_active_token(token)?;
        if self.item(token.item_id)?.status != expected_status {
            return Err(CalibrationSessionError::StaleResult);
        }
        self.item_mut(token.item_id)?.status = CalibrationItemStatus::DetectQueued;
        Ok(())
    }

    /// 检测 worker 取到任务时标记为活动检测。
    ///
    /// # Errors
    ///
    /// 令牌已过期、数据项不存在或任务仍未进入检测队列时返回错误。
    pub fn mark_detecting(
        &mut self,
        token: &CalibrationJobToken,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_active_token(token)?;
        if !matches!(
            self.item(token.item_id)?.status,
            CalibrationItemStatus::DetectQueued
        ) {
            return Err(CalibrationSessionError::StaleResult);
        }
        self.item_mut(token.item_id)?.status = CalibrationItemStatus::Detecting;
        Ok(())
    }

    /// 仅在令牌和读取后版本仍匹配时安装检测结果。
    ///
    /// # Errors
    ///
    /// 令牌过期、源变化或检测结果无效时返回错误。
    pub fn install_detection(
        &mut self,
        token: &CalibrationJobToken,
        observed_revision: impl Into<CalibrationInputRevision>,
        outcome: ChessboardDetectionOutcome,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_active_token(token)?;
        if observed_revision.into() != token.source_revision {
            self.active_detection_jobs.remove(&token.item_id);
            self.item_mut(token.item_id)?.status = CalibrationItemStatus::Pending;
            self.invalidate_solution();
            return Err(CalibrationSessionError::SourceChanged(token.item_id));
        }
        let status = match outcome {
            ChessboardDetectionOutcome::Found(detection) => {
                detection.validate(token.board)?;
                CalibrationItemStatus::Found(detection)
            }
            ChessboardDetectionOutcome::NotFound { image_size } => {
                CalibrationItemStatus::NotFound { image_size }
            }
        };
        self.active_detection_jobs.remove(&token.item_id);
        self.item_mut(token.item_id)?.status = status;
        self.invalidate_solution();
        Ok(())
    }

    /// # Errors
    ///
    /// 令牌已过期或数据项不存在时返回错误。
    pub fn install_failure(
        &mut self,
        token: &CalibrationJobToken,
        message: String,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_active_token(token)?;
        self.active_detection_jobs.remove(&token.item_id);
        self.item_mut(token.item_id)?.status = CalibrationItemStatus::Failed(message);
        self.invalidate_solution();
        Ok(())
    }

    /// 用户取消检测时仅清除 busy 状态；后续到达的旧结果会被 active-token 校验拒绝。
    ///
    /// # Errors
    ///
    /// 令牌已过期或数据项不存在时返回错误。
    pub fn cancel_detection(
        &mut self,
        token: &CalibrationJobToken,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_active_token(token)?;
        self.active_detection_jobs.remove(&token.item_id);
        self.item_mut(token.item_id)?.status = CalibrationItemStatus::Pending;
        self.invalidate_solution();
        Ok(())
    }

    /// 快照所有 enabled 且检测成功的同尺寸图像。
    ///
    /// # Errors
    ///
    /// view 数量不足、尺寸不一致或初始内参无效时返回错误。
    pub fn calibration_snapshot(
        &self,
        initial_intrinsics: InitialIntrinsics,
    ) -> Result<CalibrationSnapshot, CalibrationSessionError> {
        let mut item_ids = Vec::new();
        let mut image_points = Vec::new();
        let mut image_size = None;
        for item in self.items.iter().filter(|item| item.enabled) {
            let CalibrationItemStatus::Found(detection) = &item.status else {
                continue;
            };
            if let Some(expected) = image_size {
                if expected != detection.image_size {
                    return Err(CalibrationSessionError::MixedImageSizes {
                        expected,
                        actual: detection.image_size,
                    });
                }
            } else {
                image_size = Some(detection.image_size);
            }
            item_ids.push(item.id);
            image_points.push(detection.corners.clone());
        }
        if item_ids.len() < MIN_CALIBRATION_VIEWS {
            return Err(CalibrationSessionError::NotEnoughViews {
                found: item_ids.len(),
                required: MIN_CALIBRATION_VIEWS,
            });
        }
        let image_size = image_size.ok_or(CalibrationSessionError::NotEnoughViews {
            found: 0,
            required: MIN_CALIBRATION_VIEWS,
        })?;
        let request = CalibrationRequest {
            image_size,
            board: self.board,
            image_points,
            initial_intrinsics,
        };
        request.validate()?;
        Ok(CalibrationSnapshot {
            item_ids,
            request,
            solution_revision: self.solution_revision,
        })
    }

    /// 仅在 session solution revision 未变化时安装并再次校验解算结果。
    ///
    /// # Errors
    ///
    /// 快照过期或解算结果不满足请求不变量时返回错误。
    pub fn install_solution(
        &mut self,
        snapshot: CalibrationSnapshot,
        solution: CalibrationSolution,
    ) -> Result<(), CalibrationSessionError> {
        if snapshot.solution_revision != self.solution_revision {
            return Err(CalibrationSessionError::StaleResult);
        }
        solution.validate_against(&snapshot.request)?;
        self.installed = Some(InstalledCalibration {
            item_ids: snapshot.item_ids,
            request: snapshot.request,
            solution,
        });
        Ok(())
    }

    fn validate_token(&self, token: &CalibrationJobToken) -> Result<(), CalibrationSessionError> {
        if token.detection_epoch != self.detection_epoch || token.board != self.board {
            return Err(CalibrationSessionError::StaleResult);
        }
        let item = self.item(token.item_id)?;
        if item.revision != token.source_revision {
            return Err(CalibrationSessionError::SourceChanged(token.item_id));
        }
        Ok(())
    }

    fn validate_active_token(
        &self,
        token: &CalibrationJobToken,
    ) -> Result<(), CalibrationSessionError> {
        self.validate_token(token)?;
        if self.active_detection_jobs.get(&token.item_id) != Some(&token.job_id)
            || !self.item(token.item_id)?.status.is_busy()
        {
            return Err(CalibrationSessionError::StaleResult);
        }
        Ok(())
    }

    fn item(
        &self,
        id: CalibrationItemId,
    ) -> Result<&CalibrationDatasetItem, CalibrationSessionError> {
        self.items
            .iter()
            .find(|item| item.id == id)
            .ok_or(CalibrationSessionError::UnknownItem(id))
    }

    fn item_mut(
        &mut self,
        id: CalibrationItemId,
    ) -> Result<&mut CalibrationDatasetItem, CalibrationSessionError> {
        self.items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or(CalibrationSessionError::UnknownItem(id))
    }

    fn invalidate_solution(&mut self) {
        self.solution_revision = self.solution_revision.wrapping_add(1);
        self.installed = None;
    }

    fn invalidate_detection_epoch(&mut self) {
        self.detection_epoch = self.detection_epoch.wrapping_add(1);
        self.active_detection_jobs.clear();
        for item in &mut self.items {
            if item.status.is_busy() {
                item.status = CalibrationItemStatus::Pending;
            }
        }
        self.invalidate_solution();
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum CalibrationSessionError {
    #[error("unknown calibration dataset item {0:?}")]
    UnknownItem(CalibrationItemId),
    #[error("calibration dataset item {0:?} is busy")]
    ItemBusy(CalibrationItemId),
    #[error("calibration source changed for item {0:?}")]
    SourceChanged(CalibrationItemId),
    #[error("calibration result is stale")]
    StaleResult,
    #[error("automatic candidate input already exists in the dataset")]
    AutoCandidateAlreadyPresent,
    #[error("automatic candidate binding is stale")]
    StaleAutoCandidate,
    #[error("automatic candidate PNG revision is invalid or mismatched")]
    InvalidAutoCandidateRevision,
    #[error("automatic candidate identity does not match its stream frame")]
    InvalidAutoCandidateIdentity,
    #[error("calibration needs at least {required} detected views, found {found}")]
    NotEnoughViews { found: usize, required: usize },
    #[error("calibration images must share one size: expected {expected:?}, got {actual:?}")]
    MixedImageSizes {
        expected: CalibrationImageSize,
        actual: CalibrationImageSize,
    },
    #[error(transparent)]
    InvalidData(#[from] CalibrationDataError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSourceId, SourcePath};
    use camera_toolbox_core::{CalibrationPoint, PANGBOT_CALIBRATION_FLAGS, ViewCalibrationResult};

    fn board() -> BoardSpec {
        BoardSpec::new(2, 2, 20.0).unwrap()
    }

    fn reference(name: &str) -> FileRef {
        FileRef::new(
            FileSourceId::new("local").unwrap(),
            SourcePath::new(name).unwrap(),
        )
    }

    fn version(size: u64) -> FileVersion {
        FileVersion {
            size,
            modified_millis: Some(size),
        }
    }

    fn detection() -> ChessboardDetectionOutcome {
        ChessboardDetectionOutcome::Found(ChessboardDetection {
            image_size: CalibrationImageSize::new(640, 480).unwrap(),
            corners: vec![
                CalibrationPoint::new(10.0, 10.0),
                CalibrationPoint::new(20.0, 10.0),
                CalibrationPoint::new(10.0, 20.0),
                CalibrationPoint::new(20.0, 20.0),
            ],
        })
    }

    fn intrinsics() -> InitialIntrinsics {
        InitialIntrinsics {
            camera_matrix: [500.0, 0.0, 320.0, 0.0, 500.0, 240.0, 0.0, 0.0, 1.0],
            distortion_coefficients: vec![0.0; 12],
        }
    }

    fn solution(snapshot: &CalibrationSnapshot) -> CalibrationSolution {
        CalibrationSolution {
            image_size: snapshot.request.image_size,
            camera_matrix: snapshot.request.initial_intrinsics.camera_matrix,
            distortion_coefficients: vec![0.0; 12],
            rms_error: 0.1,
            calibration_flags: PANGBOT_CALIBRATION_FLAGS,
            views: snapshot
                .request
                .image_points
                .iter()
                .map(|points| ViewCalibrationResult {
                    rotation_vector: [0.0; 3],
                    translation_vector: [0.0, 0.0, 1.0],
                    projected_points: points.clone(),
                    reprojection_rmse: 0.1,
                    max_reprojection_error: 0.2,
                })
                .collect(),
        }
    }

    fn add_found(session: &mut CalibrationSession, name: &str, size: u64) -> CalibrationItemId {
        let AddCalibrationItemOutcome::Added(id) =
            session.add_or_refresh(reference(name), version(size), name.to_owned())
        else {
            panic!("expected added item");
        };
        let token = session.begin_detection(id).unwrap();
        session.mark_reading(&token).unwrap();

        session.mark_detect_queued(&token).unwrap();
        session.mark_detecting(&token).unwrap();
        session
            .install_detection(&token, version(size), detection())
            .unwrap();
        id
    }

    #[test]
    fn encoded_input_queues_directly_without_a_fake_read_stage() {
        let mut session = CalibrationSession::new(board());
        let input = CalibrationInputKey::StreamCapture(StreamCaptureId {
            stream_id: StreamSessionId::new("stream-test").unwrap(),
            channel: 0,
            frame_sequence: 7,
        });
        let revision = CalibrationInputRevision::EphemeralPng {
            content_sha256: "a".repeat(64),
            encoded_bytes: 128,
        };
        let AddCalibrationItemOutcome::Added(id) =
            session.add_or_refresh(input, revision, "RTSP ch0 #7".to_owned())
        else {
            panic!();
        };

        let token = session.begin_encoded_detection(id).unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::DetectQueued
        ));
        session.mark_detecting(&token).unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::Detecting
        ));
    }

    #[test]
    fn detection_status_distinguishes_read_and_detect_queues() {
        let mut session = CalibrationSession::new(board());
        let AddCalibrationItemOutcome::Added(id) =
            session.add_or_refresh(reference("a.png"), version(10), "a.png".into())
        else {
            panic!();
        };
        let token = session.begin_detection(id).unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::ReadQueued
        ));
        assert_eq!(
            session.mark_detect_queued(&token),
            Err(CalibrationSessionError::StaleResult)
        );
        session.mark_reading(&token).unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::Reading
        ));
        session.mark_detect_queued(&token).unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::DetectQueued
        ));
        session.mark_detecting(&token).unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::Detecting
        ));
    }

    #[test]
    fn png_preflight_reads_ihdr_dimensions() {
        let mut bytes = vec![0_u8; 24];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes[8..12].copy_from_slice(&13_u32.to_be_bytes());
        bytes[12..16].copy_from_slice(b"IHDR");
        bytes[16..20].copy_from_slice(&640_u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&480_u32.to_be_bytes());
        assert_eq!(
            parse_png_dimensions(&bytes).unwrap(),
            CalibrationImageSize::new(640, 480).unwrap()
        );

        bytes[0] = 0;
        assert_eq!(
            parse_png_dimensions(&bytes),
            Err(CalibrationInputError::InvalidPngHeader)
        );
    }

    #[test]
    fn duplicate_refresh_invalidates_detection_and_solution_generation() {
        let mut session = CalibrationSession::new(board());
        let id = add_found(&mut session, "a.png", 10);
        assert_eq!(
            session.add_or_refresh(reference("a.png"), version(10), "a.png".into()),
            AddCalibrationItemOutcome::AlreadyPresent(id)
        );
        assert_eq!(
            session.add_or_refresh(reference("a.png"), version(11), "a.png".into()),
            AddCalibrationItemOutcome::SourceChanged(id)
        );
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::Pending
        ));
    }

    #[test]
    fn board_corner_layout_change_marks_all_items_pending() {
        let mut session = CalibrationSession::new(board());
        add_found(&mut session, "a.png", 10);
        session
            .set_board(BoardSpec::new(3, 2, 10.0).unwrap())
            .unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::Pending
        ));
    }

    #[test]
    fn square_size_change_preserves_detections_and_invalidates_solution() {
        let mut session = CalibrationSession::new(board());
        add_found(&mut session, "a.png", 10);
        add_found(&mut session, "b.png", 20);
        add_found(&mut session, "c.png", 30);
        let snapshot = session.calibration_snapshot(intrinsics()).unwrap();
        let solution = solution(&snapshot);
        session.install_solution(snapshot, solution).unwrap();

        session
            .set_board(BoardSpec::new(2, 2, 40.0).unwrap())
            .unwrap();

        assert!(
            session
                .items()
                .iter()
                .all(|item| matches!(item.status, CalibrationItemStatus::Found(_)))
        );
        assert!(session.installed().is_none());
        assert_eq!(session.board().square_size, 40.0);
        assert_eq!(
            session
                .calibration_snapshot(intrinsics())
                .unwrap()
                .request
                .board
                .square_size,
            40.0
        );
    }

    #[test]
    fn reset_detections_marks_every_item_pending() {
        let mut session = CalibrationSession::new(board());
        add_found(&mut session, "a.png", 10);
        let disabled = add_found(&mut session, "b.png", 20);
        session.set_enabled(disabled, false).unwrap();

        session.reset_detections();

        assert!(
            session
                .items()
                .iter()
                .all(|item| matches!(item.status, CalibrationItemStatus::Pending))
        );
        assert!(!session.items()[1].enabled);
        assert!(matches!(
            session.calibration_snapshot(intrinsics()),
            Err(CalibrationSessionError::NotEnoughViews { found: 0, .. })
        ));
    }

    #[test]
    fn stale_detection_result_is_rejected_after_dataset_change() {
        let mut session = CalibrationSession::new(board());
        let AddCalibrationItemOutcome::Added(id) =
            session.add_or_refresh(reference("a.png"), version(10), "a.png".into())
        else {
            panic!();
        };
        let token = session.begin_detection(id).unwrap();
        session.add_or_refresh(reference("b.png"), version(20), "b.png".into());
        assert_eq!(
            session.install_detection(&token, version(10), detection()),
            Err(CalibrationSessionError::StaleResult)
        );
    }

    #[test]
    fn concurrent_detection_tokens_install_in_reverse_order() {
        let mut session = CalibrationSession::new(board());
        let AddCalibrationItemOutcome::Added(first) =
            session.add_or_refresh(reference("a.png"), version(10), "a.png".into())
        else {
            panic!();
        };
        let AddCalibrationItemOutcome::Added(second) =
            session.add_or_refresh(reference("b.png"), version(20), "b.png".into())
        else {
            panic!();
        };
        let first_token = session.begin_detection(first).unwrap();
        let second_token = session.begin_detection(second).unwrap();
        session.mark_reading(&first_token).unwrap();
        session.mark_reading(&second_token).unwrap();
        session.mark_detect_queued(&first_token).unwrap();
        session.mark_detect_queued(&second_token).unwrap();
        session.mark_detecting(&first_token).unwrap();
        session.mark_detecting(&second_token).unwrap();

        session
            .install_detection(&second_token, version(20), detection())
            .unwrap();
        session
            .install_detection(&first_token, version(10), detection())
            .unwrap();

        assert!(
            session
                .items()
                .iter()
                .all(|item| matches!(item.status, CalibrationItemStatus::Found(_)))
        );
    }

    #[test]
    fn restarted_detection_rejects_result_from_cancelled_job() {
        let mut session = CalibrationSession::new(board());
        let AddCalibrationItemOutcome::Added(id) =
            session.add_or_refresh(reference("a.png"), version(10), "a.png".into())
        else {
            panic!();
        };
        let cancelled_token = session.begin_detection(id).unwrap();
        session.cancel_detection(&cancelled_token).unwrap();
        let active_token = session.begin_detection(id).unwrap();
        session.mark_reading(&active_token).unwrap();
        session.mark_detect_queued(&active_token).unwrap();
        session.mark_detecting(&active_token).unwrap();

        assert_eq!(
            session.install_detection(&cancelled_token, version(10), detection()),
            Err(CalibrationSessionError::StaleResult)
        );
        session
            .install_detection(&active_token, version(10), detection())
            .unwrap();
        assert!(matches!(
            session.items()[0].status,
            CalibrationItemStatus::Found(_)
        ));
    }

    #[test]
    fn snapshot_requires_three_same_size_found_views() {
        let mut session = CalibrationSession::new(board());
        add_found(&mut session, "a.png", 10);
        add_found(&mut session, "b.png", 20);
        assert!(matches!(
            session.calibration_snapshot(intrinsics()),
            Err(CalibrationSessionError::NotEnoughViews { found: 2, .. })
        ));
        add_found(&mut session, "c.png", 30);
        let snapshot = session.calibration_snapshot(intrinsics()).unwrap();
        assert_eq!(snapshot.item_ids.len(), 3);
    }

    #[test]
    fn solution_install_is_transactional() {
        let mut session = CalibrationSession::new(board());
        add_found(&mut session, "a.png", 10);
        add_found(&mut session, "b.png", 20);
        add_found(&mut session, "c.png", 30);
        let snapshot = session.calibration_snapshot(intrinsics()).unwrap();
        let solution = solution(&snapshot);
        session.install_solution(snapshot, solution).unwrap();
        assert!(session.installed().is_some());
        session.set_enabled(session.items()[0].id, false).unwrap();
        assert!(session.installed().is_none());
    }

    fn auto_candidate_fixture(
        session: &CalibrationSession,
        candidate_id: u64,
        sequence: u64,
    ) -> (AutoCandidateToken, CalibrationInputRevision) {
        let identity = StreamFrameIdentity::unavailable(
            StreamSessionId::new("auto-capture-test").unwrap(),
            0,
            sequence,
            "test fixture",
        );
        let revision = CalibrationInputRevision::EphemeralPng {
            content_sha256: format!("{sequence:064x}"),
            encoded_bytes: 128,
        };
        let token = session
            .bind_auto_candidate(
                AutoCandidateId::new(candidate_id),
                identity,
                revision.clone(),
                format!("candidate-{sequence}"),
            )
            .unwrap();
        (token, revision)
    }

    #[test]
    fn stale_auto_admission_revision_cannot_mutate_dataset() {
        let mut session = CalibrationSession::new(board());
        let (token, revision) = auto_candidate_fixture(&session, 1, 7);
        session.invalidate_auto_admission();
        let ChessboardDetectionOutcome::Found(found) = detection() else {
            panic!("fixture must contain a found detection");
        };

        assert_eq!(
            session.commit_auto_candidate(AutoCandidateCommit::new(token, revision, found)),
            Err(CalibrationSessionError::StaleAutoCandidate)
        );
        assert!(session.items().is_empty());
        assert_eq!(session.selected(), None);
    }

    #[test]
    fn auto_candidate_commit_preserves_active_dataset_detection_token() {
        let mut session = CalibrationSession::new(board());
        let AddCalibrationItemOutcome::Added(dataset_id) = session.add_or_refresh(
            reference("dataset.png"),
            version(10),
            "dataset.png".to_owned(),
        ) else {
            panic!("expected dataset item");
        };
        let dataset_token = session.begin_detection(dataset_id).unwrap();
        let (candidate_token, candidate_revision) = auto_candidate_fixture(&session, 1, 8);
        let ChessboardDetectionOutcome::Found(candidate_detection) = detection() else {
            panic!("fixture must contain a found detection");
        };
        session
            .commit_auto_candidate(AutoCandidateCommit::new(
                candidate_token,
                candidate_revision,
                candidate_detection,
            ))
            .unwrap();

        session.mark_reading(&dataset_token).unwrap();
        session.mark_detect_queued(&dataset_token).unwrap();
        session.mark_detecting(&dataset_token).unwrap();
        session
            .install_detection(&dataset_token, version(10), detection())
            .unwrap();
        assert!(matches!(
            session.item(dataset_id).unwrap().status,
            CalibrationItemStatus::Found(_)
        ));
        assert_eq!(session.items().len(), 2);
    }
}
