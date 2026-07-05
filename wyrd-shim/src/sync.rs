//! An instrumented [`Mutex`] wrapping `tokio::sync::Mutex`.

use std::ops::{Deref, DerefMut};
use std::panic::Location;

use tokio::sync as tsync;
use wyrd_weave::{Event, ResourceId, StateOp, FIELD_ACQUIRED_BY};

use crate::{current_task, emit, loc_of, next_id};

/// A drop-in-ish `tokio::sync::Mutex` that records acquire/park/release into the
/// active wyrd recording, using observed state rather than tokio internals:
/// contention is a failed `try_lock`, the holder is whoever a `lock().await`
/// resolves for, and release is the guard's `Drop`.
pub struct Mutex<T> {
    inner: tsync::Mutex<T>,
    id: ResourceId,
}

impl<T> Mutex<T> {
    /// Create a mutex, recording its birth and source location.
    #[track_caller]
    pub fn new(value: T) -> Self {
        let id = next_id();
        emit(Event::ResourceNew {
            id,
            parent: None,
            concrete_type: "Mutex".into(),
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
            inner: tsync::Mutex::new(value),
            id,
        }
    }

    /// This resource's wyrd id.
    pub fn id(&self) -> ResourceId {
        self.id
    }

    /// Lock the mutex, recording a park iff the lock is contended and recording
    /// the acquirer as the holder.
    pub async fn lock(&self) -> MutexGuard<'_, T> {
        let task = current_task();
        let guard = match self.inner.try_lock() {
            // Uncontended: no park.
            Ok(g) => g,
            // Contended: record the park, then wait for the lock.
            Err(_) => {
                if let Some(t) = task {
                    emit(Event::Park {
                        task: t,
                        resource: self.id,
                        op_name: "lock".into(),
                    });
                }
                self.inner.lock().await
            }
        };
        if let Some(t) = task {
            emit(Event::ResourceState {
                id: self.id,
                field: FIELD_ACQUIRED_BY.into(),
                value: t as i64,
                op: StateOp::Override,
            });
        }
        emit(Event::ResourceState {
            id: self.id,
            field: "locked".into(),
            value: 1,
            op: StateOp::Override,
        });
        MutexGuard {
            inner: guard,
            id: self.id,
        }
    }
}

/// RAII guard; records the release (`locked = 0`) on drop.
pub struct MutexGuard<'a, T> {
    inner: tsync::MutexGuard<'a, T>,
    id: ResourceId,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        emit(Event::ResourceState {
            id: self.id,
            field: "locked".into(),
            value: 0,
            op: StateOp::Override,
        });
    }
}
