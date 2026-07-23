//! An instrumented [`Semaphore`] wrapping `tokio::sync::Semaphore`.
//!
//! Permit accounting follows the wyrd model: `permits` is free capacity,
//! decremented on acquire and restored when the permit drops, so the CLI's
//! depth/saturation reports work unchanged. A blocked `acquire` records a
//! park (the semaphore is exhausted — backpressure).

use std::panic::Location;

use tokio::sync as tsync;
use wyrd_weave::{Event, ResourceId, StateOp};

use crate::{current_task, emit, loc_of, next_id};

pub use tsync::AcquireError;

/// A counting semaphore that records its permit level and contended acquires.
pub struct Semaphore {
    inner: tsync::Semaphore,
    id: ResourceId,
}

impl Semaphore {
    /// Create a semaphore with `permits` free permits, recording its birth,
    /// source location, and capacity.
    #[track_caller]
    pub fn new(permits: usize) -> Self {
        let id = next_id();
        emit(Event::ResourceNew {
            id,
            parent: None,
            concrete_type: "Semaphore".into(),
            loc: loc_of(Location::caller()),
            is_internal: false,
        });
        emit(Event::ResourceState {
            id,
            field: "permits".into(),
            value: permits as i64,
            op: StateOp::Override,
        });
        Self {
            inner: tsync::Semaphore::new(permits),
            id,
        }
    }

    /// This resource's wyrd id.
    pub fn id(&self) -> ResourceId {
        self.id
    }

    /// Currently free permits.
    pub fn available_permits(&self) -> usize {
        self.inner.available_permits()
    }

    /// Add `n` free permits, keeping the recorded level in step.
    pub fn add_permits(&self, n: usize) {
        self.inner.add_permits(n);
        emit(Event::ResourceState {
            id: self.id,
            field: "permits".into(),
            value: n as i64,
            op: StateOp::Add,
        });
    }

    /// Acquire one permit, recording a park iff none are free.
    pub async fn acquire(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        crate::chaos::chaos_point().await;
        let permit = match self.inner.try_acquire() {
            Ok(p) => p,
            Err(_) => {
                // Exhausted (or closed — acquire below reports that).
                if let Some(t) = current_task() {
                    emit(Event::Park {
                        task: t,
                        resource: self.id,
                        op_name: "acquire".into(),
                    });
                }
                self.inner.acquire().await?
            }
        };
        emit(Event::ResourceState {
            id: self.id,
            field: "permits".into(),
            value: 1,
            op: StateOp::Sub,
        });
        Ok(SemaphorePermit {
            _inner: permit,
            id: self.id,
        })
    }
}

/// RAII permit; restores the recorded permit level on drop.
pub struct SemaphorePermit<'a> {
    _inner: tsync::SemaphorePermit<'a>,
    id: ResourceId,
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        emit(Event::ResourceState {
            id: self.id,
            field: "permits".into(),
            value: 1,
            op: StateOp::Add,
        });
    }
}
