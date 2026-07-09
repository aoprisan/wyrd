//! An instrumented [`Notify`] wrapping `tokio::sync::Notify`.

use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tokio::sync as tsync;
use wyrd_weave::{Event, ResourceId};

use crate::{current_task, emit, loc_of, next_id};

/// A notify handle that records a park when a waiter goes pending. There is
/// no holder to track — a task parked here shows up as waiting on an external
/// signal (`ResourceRoot` in `why-blocked`), which is exactly what it is.
pub struct Notify {
    inner: tsync::Notify,
    id: ResourceId,
}

impl Notify {
    /// Create a notify, recording its birth and source location.
    #[track_caller]
    pub fn new() -> Self {
        let id = next_id();
        emit(Event::ResourceNew {
            id,
            parent: None,
            concrete_type: "Notify".into(),
            loc: loc_of(Location::caller()),
            is_internal: false,
        });
        Self {
            inner: tsync::Notify::new(),
            id,
        }
    }

    /// This resource's wyrd id.
    pub fn id(&self) -> ResourceId {
        self.id
    }

    /// Wait for a notification, recording a park the first time the wait goes
    /// pending.
    pub fn notified(&self) -> Notified<'_> {
        Notified {
            inner: self.inner.notified(),
            id: self.id,
            parked: false,
        }
    }

    /// Wake one waiter (or store a permit for the next).
    pub fn notify_one(&self) {
        self.inner.notify_one();
    }

    /// Wake every current waiter.
    pub fn notify_waiters(&self) {
        self.inner.notify_waiters();
    }
}

impl Default for Notify {
    #[track_caller]
    fn default() -> Self {
        Self::new()
    }
}

pin_project! {
    /// Future returned by [`Notify::notified`].
    pub struct Notified<'a> {
        #[pin]
        inner: tsync::futures::Notified<'a>,
        id: ResourceId,
        parked: bool,
    }
}

impl Future for Notified<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Ready(()) => Poll::Ready(()),
            Poll::Pending => {
                if !*this.parked {
                    *this.parked = true;
                    if let Some(t) = current_task() {
                        emit(Event::Park {
                            task: t,
                            resource: *this.id,
                            op_name: "notified".into(),
                        });
                    }
                }
                Poll::Pending
            }
        }
    }
}
