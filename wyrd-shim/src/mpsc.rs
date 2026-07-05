//! An instrumented bounded [`channel`] wrapping `tokio::sync::mpsc`.
//!
//! Depth is tracked exactly as the wyrd model expects: `permits` are free
//! capacity (`capacity - depth`), decremented on a successful send and
//! incremented on a receive. A send that blocks on a full channel records a
//! park (backpressure).

use std::panic::Location;

use tokio::sync::mpsc as tmpsc;
use wyrd_weave::{Event, ResourceId, StateOp};

use crate::{current_task, emit, loc_of, next_id};

pub use tmpsc::error::SendError;

/// Create a bounded channel, recording it as a resource.
#[track_caller]
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let id = next_id();
    emit(Event::ResourceNew {
        id,
        parent: None,
        concrete_type: "mpsc::channel".into(),
        loc: loc_of(Location::caller()),
        is_internal: false,
    });
    // permits == free capacity.
    emit(Event::ResourceState {
        id,
        field: "permits".into(),
        value: capacity as i64,
        op: StateOp::Override,
    });
    let (tx, rx) = tmpsc::channel(capacity);
    (Sender { inner: tx, id }, Receiver { inner: rx, id })
}

fn permit_delta(id: ResourceId, op: StateOp) {
    emit(Event::ResourceState {
        id,
        field: "permits".into(),
        value: 1,
        op,
    });
}

/// The sending half.
pub struct Sender<T> {
    inner: tmpsc::Sender<T>,
    id: ResourceId,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            id: self.id,
        }
    }
}

impl<T> Sender<T> {
    /// This channel's wyrd resource id.
    pub fn id(&self) -> ResourceId {
        self.id
    }

    /// Send a value, recording a park if the channel is full (backpressure).
    pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
        let task = current_task();
        match self.inner.try_send(value) {
            Ok(()) => {
                permit_delta(self.id, StateOp::Sub);
                Ok(())
            }
            Err(tmpsc::error::TrySendError::Full(value)) => {
                if let Some(t) = task {
                    emit(Event::Park {
                        task: t,
                        resource: self.id,
                        op_name: "send".into(),
                    });
                }
                let result = self.inner.send(value).await;
                if result.is_ok() {
                    permit_delta(self.id, StateOp::Sub);
                }
                result
            }
            Err(tmpsc::error::TrySendError::Closed(value)) => Err(SendError(value)),
        }
    }
}

/// The receiving half.
pub struct Receiver<T> {
    inner: tmpsc::Receiver<T>,
    id: ResourceId,
}

impl<T> Receiver<T> {
    /// This channel's wyrd resource id.
    pub fn id(&self) -> ResourceId {
        self.id
    }

    /// Receive the next value, freeing a permit (reducing depth).
    pub async fn recv(&mut self) -> Option<T> {
        let value = self.inner.recv().await;
        if value.is_some() {
            permit_delta(self.id, StateOp::Add);
        }
        value
    }
}
