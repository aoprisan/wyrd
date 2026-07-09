//! An instrumented [`RwLock`] wrapping `tokio::sync::RwLock`.

use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::atomic::{AtomicI64, Ordering};

use tokio::sync as tsync;
use wyrd_weave::{Event, ResourceId, StateOp, FIELD_ACQUIRED_BY};

use crate::{current_task, emit, loc_of, next_id};

/// A drop-in-ish `tokio::sync::RwLock` that records contended reads/writes as
/// parks and tracks a *presumed* holder: the most recent acquirer (reader or
/// writer). With several concurrent readers the holder is approximate — the
/// last reader in wins — but the lock only reports itself free again once
/// every guard has dropped.
pub struct RwLock<T> {
    inner: tsync::RwLock<T>,
    id: ResourceId,
    /// Live guards (readers, or 1 for a writer); `locked = 0` is emitted only
    /// when this returns to zero.
    guards: AtomicI64,
}

impl<T> RwLock<T> {
    /// Create a lock, recording its birth and source location.
    #[track_caller]
    pub fn new(value: T) -> Self {
        let id = next_id();
        emit(Event::ResourceNew {
            id,
            parent: None,
            concrete_type: "RwLock".into(),
            loc: loc_of(Location::caller()),
            is_internal: false,
        });
        emit(Event::ResourceState {
            id,
            field: "locked".into(),
            value: 0,
            op: StateOp::Override,
        });
        Self {
            inner: tsync::RwLock::new(value),
            id,
            guards: AtomicI64::new(0),
        }
    }

    /// This resource's wyrd id.
    pub fn id(&self) -> ResourceId {
        self.id
    }

    fn record_acquired(&self) {
        if let Some(t) = current_task() {
            emit(Event::ResourceState {
                id: self.id,
                field: FIELD_ACQUIRED_BY.into(),
                value: t as i64,
                op: StateOp::Override,
            });
        }
        if self.guards.fetch_add(1, Ordering::AcqRel) == 0 {
            emit(Event::ResourceState {
                id: self.id,
                field: "locked".into(),
                value: 1,
                op: StateOp::Override,
            });
        }
    }

    fn record_released(&self) {
        if self.guards.fetch_sub(1, Ordering::AcqRel) == 1 {
            emit(Event::ResourceState {
                id: self.id,
                field: "locked".into(),
                value: 0,
                op: StateOp::Override,
            });
        }
    }

    /// Acquire a shared read guard, recording a park iff a writer holds the
    /// lock (or writers are queued ahead).
    pub async fn read(&self) -> RwLockReadGuard<'_, T> {
        let guard = match self.inner.try_read() {
            Ok(g) => g,
            Err(_) => {
                if let Some(t) = current_task() {
                    emit(Event::Park {
                        task: t,
                        resource: self.id,
                        op_name: "read".into(),
                    });
                }
                self.inner.read().await
            }
        };
        self.record_acquired();
        RwLockReadGuard {
            inner: guard,
            lock: self,
        }
    }

    /// Acquire the exclusive write guard, recording a park iff contended.
    pub async fn write(&self) -> RwLockWriteGuard<'_, T> {
        let guard = match self.inner.try_write() {
            Ok(g) => g,
            Err(_) => {
                if let Some(t) = current_task() {
                    emit(Event::Park {
                        task: t,
                        resource: self.id,
                        op_name: "write".into(),
                    });
                }
                self.inner.write().await
            }
        };
        self.record_acquired();
        RwLockWriteGuard {
            inner: guard,
            lock: self,
        }
    }
}

/// Shared read guard; releases its share of the lock state on drop.
pub struct RwLockReadGuard<'a, T> {
    inner: tsync::RwLockReadGuard<'a, T>,
    lock: &'a RwLock<T>,
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.record_released();
    }
}

/// Exclusive write guard; releases the lock state on drop.
pub struct RwLockWriteGuard<'a, T> {
    inner: tsync::RwLockWriteGuard<'a, T>,
    lock: &'a RwLock<T>,
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.record_released();
    }
}
