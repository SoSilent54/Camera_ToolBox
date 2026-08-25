//! 无落盘兜底的全局 bounded ephemeral source store。

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use camera_toolbox_core::{AssetId, EphemeralAsset};
use thiserror::Error;

use crate::platform::OperationId;

/// `CaptureStore` 的预算配置，单位均为 `bytes`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureStoreLimits {
    pub per_operation_bytes: usize,
    pub global_bytes: usize,
}

impl CaptureStoreLimits {
    /// 创建非零且单操作不超过全局的预算。
    ///
    /// # Errors
    ///
    /// 任一预算为零或单操作预算大于全局预算时返回错误。
    pub fn new(per_operation_bytes: usize, global_bytes: usize) -> Result<Self, CaptureStoreError> {
        if per_operation_bytes == 0 || global_bytes == 0 {
            return Err(CaptureStoreError::ZeroBudget);
        }
        if per_operation_bytes > global_bytes {
            return Err(CaptureStoreError::PerOperationLimitExceedsGlobal {
                per_operation_bytes,
                global_bytes,
            });
        }
        Ok(Self {
            per_operation_bytes,
            global_bytes,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStoreStats {
    pub reserved_bytes: usize,
    pub published_bytes: usize,
    pub reservation_count: usize,
    pub asset_count: usize,
}

/// 线程安全的会话内 source store。所有 ownership 都保留在内存中。
#[derive(Debug, Clone)]
pub struct CaptureStore {
    inner: Arc<CaptureStoreInner>,
}

#[derive(Debug)]
struct CaptureStoreInner {
    limits: CaptureStoreLimits,
    next_reservation_id: AtomicU64,
    state: Mutex<CaptureStoreState>,
}

#[derive(Debug, Default)]
struct CaptureStoreState {
    reserved_bytes: usize,
    published_bytes: usize,
    operation_reserved: BTreeMap<OperationId, usize>,
    reservations: HashMap<u64, ReservationRecord>,
    assets: BTreeMap<AssetId, Arc<EphemeralAsset>>,
}

#[derive(Debug)]
struct ReservationRecord {
    operation_id: OperationId,
    bytes: usize,
}

impl CaptureStore {
    #[must_use]
    pub fn new(limits: CaptureStoreLimits) -> Self {
        Self {
            inner: Arc::new(CaptureStoreInner {
                limits,
                next_reservation_id: AtomicU64::new(1),
                state: Mutex::new(CaptureStoreState::default()),
            }),
        }
    }

    #[must_use]
    pub fn limits(&self) -> CaptureStoreLimits {
        self.inner.limits
    }

    /// 在接收 payload 前同时预留单操作和全局预算。
    ///
    /// Reservation 被取消、失败或离开作用域时自动回滚，绝不转为磁盘 spill。
    ///
    /// # Errors
    ///
    /// 长度为零、算术溢出或任一预算不足时返回 typed error。
    pub fn reserve(
        &self,
        operation_id: OperationId,
        declared_len: usize,
    ) -> Result<AssetReservation, CaptureStoreError> {
        if declared_len == 0 {
            return Err(CaptureStoreError::EmptyReservation);
        }
        let mut state = self.lock_state()?;
        let operation_current = state
            .operation_reserved
            .get(&operation_id)
            .copied()
            .unwrap_or(0);
        let operation_requested = operation_current
            .checked_add(declared_len)
            .ok_or(CaptureStoreError::BudgetArithmeticOverflow)?;
        if operation_requested > self.inner.limits.per_operation_bytes {
            return Err(CaptureStoreError::PerOperationBudgetExceeded {
                limit: self.inner.limits.per_operation_bytes,
                requested: operation_requested,
            });
        }

        let accounted = state
            .reserved_bytes
            .checked_add(state.published_bytes)
            .and_then(|bytes| bytes.checked_add(declared_len))
            .ok_or(CaptureStoreError::BudgetArithmeticOverflow)?;
        if accounted > self.inner.limits.global_bytes {
            return Err(CaptureStoreError::GlobalBudgetExceeded {
                limit: self.inner.limits.global_bytes,
                requested: accounted,
            });
        }

        let reservation_id = self
            .inner
            .next_reservation_id
            .fetch_add(1, Ordering::Relaxed);
        if reservation_id == 0 || state.reservations.contains_key(&reservation_id) {
            return Err(CaptureStoreError::ReservationIdExhausted);
        }
        state.reserved_bytes += declared_len;
        state
            .operation_reserved
            .insert(operation_id.clone(), operation_requested);
        state.reservations.insert(
            reservation_id,
            ReservationRecord {
                operation_id: operation_id.clone(),
                bytes: declared_len,
            },
        );
        drop(state);

        Ok(AssetReservation {
            store: Arc::clone(&self.inner),
            reservation_id: Some(reservation_id),
            operation_id,
            reserved_bytes: declared_len,
        })
    }

    /// 将已验证 operation-owned payload 原地转交 store，并释放未使用 reservation。
    ///
    /// # Errors
    ///
    /// reservation 不属于本 store、payload 超过 reservation、完整性未验证或 id 重复时返回错误。
    pub fn publish_validated(
        &self,
        mut reservation: AssetReservation,
        asset: EphemeralAsset,
    ) -> Result<Arc<EphemeralAsset>, CaptureStoreError> {
        if !Arc::ptr_eq(&self.inner, &reservation.store) {
            return Err(CaptureStoreError::ForeignReservation);
        }
        if !asset.integrity.is_verified() {
            return Err(CaptureStoreError::IntegrityNotVerified);
        }
        let payload_bytes = asset.byte_len()?;
        if payload_bytes > reservation.reserved_bytes {
            return Err(CaptureStoreError::PayloadExceedsReservation {
                reserved: reservation.reserved_bytes,
                actual: payload_bytes,
            });
        }
        let reservation_id = reservation
            .reservation_id
            .ok_or(CaptureStoreError::ReservationAlreadyConsumed)?;

        let mut state = self.lock_state()?;
        if state.assets.contains_key(&asset.id) {
            return Err(CaptureStoreError::DuplicateAsset(asset.id));
        }
        let Some(record) = state.reservations.remove(&reservation_id) else {
            return Err(CaptureStoreError::ReservationAlreadyConsumed);
        };
        if record.operation_id != reservation.operation_id
            || record.bytes != reservation.reserved_bytes
        {
            // 内部账本不一致时恢复 record，避免预算静默丢失。
            state.reservations.insert(reservation_id, record);
            return Err(CaptureStoreError::ReservationLedgerMismatch);
        }

        subtract_operation_reservation(&mut state, &record.operation_id, record.bytes);
        state.reserved_bytes -= record.bytes;
        state.published_bytes = state
            .published_bytes
            .checked_add(payload_bytes)
            .ok_or(CaptureStoreError::BudgetArithmeticOverflow)?;
        let asset = Arc::new(asset);
        state.assets.insert(asset.id.clone(), Arc::clone(&asset));
        reservation.reservation_id = None;
        Ok(asset)
    }

    /// 借用已发布 asset。外部 Arc 存活时 `release` 会明确返回 `AssetInUse`。
    ///
    /// # Errors
    ///
    /// store 锁损坏时返回错误。
    pub fn get(&self, id: &AssetId) -> Result<Option<Arc<EphemeralAsset>>, CaptureStoreError> {
        Ok(self.lock_state()?.assets.get(id).cloned())
    }

    /// 删除 store 的最后一个 asset 引用并归还全局预算。
    ///
    /// # Errors
    ///
    /// asset 不存在、仍有借用 Arc 或账本异常时返回错误。
    pub fn release(&self, id: &AssetId) -> Result<(), CaptureStoreError> {
        let mut state = self.lock_state()?;
        let asset = state
            .assets
            .get(id)
            .ok_or_else(|| CaptureStoreError::UnknownAsset(id.clone()))?;
        let strong_count = Arc::strong_count(asset);
        if strong_count != 1 {
            return Err(CaptureStoreError::AssetInUse {
                id: id.clone(),
                external_references: strong_count - 1,
            });
        }
        let payload_bytes = asset.byte_len()?;
        let removed = state
            .assets
            .remove(id)
            .ok_or_else(|| CaptureStoreError::UnknownAsset(id.clone()))?;
        state.published_bytes = state
            .published_bytes
            .checked_sub(payload_bytes)
            .ok_or(CaptureStoreError::ReservationLedgerMismatch)?;
        drop(removed);
        Ok(())
    }

    /// 获取当前精确预算账本。
    ///
    /// # Errors
    ///
    /// store 锁损坏时返回错误。
    pub fn stats(&self) -> Result<CaptureStoreStats, CaptureStoreError> {
        let state = self.lock_state()?;
        Ok(CaptureStoreStats {
            reserved_bytes: state.reserved_bytes,
            published_bytes: state.published_bytes,
            reservation_count: state.reservations.len(),
            asset_count: state.assets.len(),
        })
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, CaptureStoreState>, CaptureStoreError> {
        self.inner
            .state
            .lock()
            .map_err(|_| CaptureStoreError::StoreLockPoisoned)
    }
}

/// operation-owned RAII reservation。Drop 是失败、取消和 early-return 的 rollback。
#[derive(Debug)]
pub struct AssetReservation {
    store: Arc<CaptureStoreInner>,
    reservation_id: Option<u64>,
    operation_id: OperationId,
    reserved_bytes: usize,
}

impl AssetReservation {
    #[must_use]
    pub fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    #[must_use]
    pub const fn reserved_bytes(&self) -> usize {
        self.reserved_bytes
    }
}

impl Drop for AssetReservation {
    fn drop(&mut self) {
        let Some(reservation_id) = self.reservation_id.take() else {
            return;
        };
        // Drop 不能传播 poison；恢复内部值以保证预算仍被归还。
        let mut state = self
            .store
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = state.reservations.remove(&reservation_id) else {
            return;
        };
        state.reserved_bytes = state.reserved_bytes.saturating_sub(record.bytes);
        subtract_operation_reservation(&mut state, &record.operation_id, record.bytes);
    }
}

fn subtract_operation_reservation(
    state: &mut CaptureStoreState,
    operation_id: &OperationId,
    bytes: usize,
) {
    let Some(current) = state.operation_reserved.get_mut(operation_id) else {
        return;
    };
    *current = current.saturating_sub(bytes);
    if *current == 0 {
        state.operation_reserved.remove(operation_id);
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CaptureStoreError {
    #[error("capture store budgets must be non-zero")]
    ZeroBudget,
    #[error("per-operation limit {per_operation_bytes} exceeds global limit {global_bytes} bytes")]
    PerOperationLimitExceedsGlobal {
        per_operation_bytes: usize,
        global_bytes: usize,
    },
    #[error("zero-byte reservations are not allowed")]
    EmptyReservation,
    #[error("capture store budget arithmetic overflow")]
    BudgetArithmeticOverflow,
    #[error("per-operation memory budget exceeded: requested {requested}, limit {limit}")]
    PerOperationBudgetExceeded { limit: usize, requested: usize },
    #[error("global ephemeral memory budget exceeded: requested {requested}, limit {limit}")]
    GlobalBudgetExceeded { limit: usize, requested: usize },
    #[error("reservation id space exhausted")]
    ReservationIdExhausted,
    #[error("reservation belongs to another CaptureStore")]
    ForeignReservation,
    #[error("asset integrity has not been verified")]
    IntegrityNotVerified,
    #[error("payload exceeds reservation: actual {actual}, reserved {reserved}")]
    PayloadExceedsReservation { reserved: usize, actual: usize },
    #[error("reservation was already consumed or rolled back")]
    ReservationAlreadyConsumed,
    #[error("reservation accounting ledger mismatch")]
    ReservationLedgerMismatch,
    #[error("asset already exists: {0}")]
    DuplicateAsset(AssetId),
    #[error("asset not found: {0}")]
    UnknownAsset(AssetId),
    #[error("asset {id} is still in use by {external_references} external reference(s)")]
    AssetInUse {
        id: AssetId,
        external_references: usize,
    },
    #[error("capture store lock is poisoned")]
    StoreLockPoisoned,
    #[error(transparent)]
    Asset(#[from] camera_toolbox_core::AssetError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use camera_toolbox_core::{CaptureMetadata, IntegrityState, MediaFormat, OwnedMediaPayload};

    use super::*;

    fn operation(value: &str) -> OperationId {
        OperationId::new(value).unwrap()
    }

    fn asset(id: &str, bytes: usize) -> EphemeralAsset {
        EphemeralAsset::new(
            AssetId::new(id).unwrap(),
            OwnedMediaPayload::from_bytes(Arc::<[u8]>::from(vec![7; bytes])),
            CaptureMetadata {
                format: MediaFormat::Binary,
                source_name: "unit-test".to_owned(),
                attributes: BTreeMap::new(),
            },
            IntegrityState::Verified {
                algorithm: "sha256".to_owned(),
                digest: format!("digest-{id}"),
            },
        )
    }

    #[test]
    fn enforces_cumulative_operation_and_global_budgets() {
        let store = CaptureStore::new(CaptureStoreLimits::new(6, 10).unwrap());
        let first = store.reserve(operation("op-a"), 4).unwrap();
        let operation_error = store.reserve(operation("op-a"), 3).unwrap_err();
        assert_eq!(
            operation_error,
            CaptureStoreError::PerOperationBudgetExceeded {
                limit: 6,
                requested: 7
            }
        );
        let second = store.reserve(operation("op-b"), 6).unwrap();
        let global_error = store.reserve(operation("op-c"), 1).unwrap_err();
        assert_eq!(
            global_error,
            CaptureStoreError::GlobalBudgetExceeded {
                limit: 10,
                requested: 11
            }
        );
        assert_eq!(
            store.stats().unwrap(),
            CaptureStoreStats {
                reserved_bytes: 10,
                published_bytes: 0,
                reservation_count: 2,
                asset_count: 0,
            }
        );
        drop((first, second));
    }

    #[test]
    fn dropping_or_failed_publish_rolls_back_reservation() {
        let store = CaptureStore::new(CaptureStoreLimits::new(8, 8).unwrap());
        {
            let _reservation = store.reserve(operation("cancelled"), 8).unwrap();
            assert_eq!(store.stats().unwrap().reserved_bytes, 8);
        }
        assert_eq!(store.stats().unwrap(), CaptureStoreStats::default());

        let reservation = store.reserve(operation("invalid"), 4).unwrap();
        let mut invalid = asset("invalid", 4);
        invalid.integrity = IntegrityState::Unverified;
        assert_eq!(
            store.publish_validated(reservation, invalid).unwrap_err(),
            CaptureStoreError::IntegrityNotVerified
        );
        assert_eq!(store.stats().unwrap(), CaptureStoreStats::default());
    }

    #[test]
    fn publish_transfers_budget_and_release_returns_it() {
        let store = CaptureStore::new(CaptureStoreLimits::new(8, 8).unwrap());
        let reservation = store.reserve(operation("capture"), 8).unwrap();
        let published = store
            .publish_validated(reservation, asset("frame-1", 6))
            .unwrap();
        assert_eq!(
            store.stats().unwrap(),
            CaptureStoreStats {
                reserved_bytes: 0,
                published_bytes: 6,
                reservation_count: 0,
                asset_count: 1,
            }
        );
        assert_eq!(
            store.release(&published.id).unwrap_err(),
            CaptureStoreError::AssetInUse {
                id: published.id.clone(),
                external_references: 1
            }
        );
        let id = published.id.clone();
        drop(published);
        store.release(&id).unwrap();
        assert_eq!(store.stats().unwrap(), CaptureStoreStats::default());
    }
}
