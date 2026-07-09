//! An instrumented oneshot [`channel`] wrapping `tokio::sync::oneshot`.

use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tokio::sync::oneshot as toneshot;
use wyrd_weave::{Event, ResourceId};

use crate::{current_task, emit, loc_of, next_id};

pub use toneshot::error::RecvError;

/// Create a oneshot channel, recording it as a resource. Awaiting the
/// receiver before the value arrives records a park; there is no holder, so
/// a stuck receiver shows the *channel* as the root cause (someone forgot to
/// send, or the sender is itself stuck).
#[track_caller]
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let id = next_id();
    emit(Event::ResourceNew {
        id,
        parent: None,
        concrete_type: "oneshot::channel".into(),
        loc: loc_of(Location::caller()),
        is_internal: false,
    });
    let (tx, rx) = toneshot::channel();
    (
        Sender { inner: tx },
        Receiver {
            inner: rx,
            id,
            parked: false,
        },
    )
}

/// The sending half.
pub struct Sender<T> {
    inner: toneshot::Sender<T>,
}

impl<T> Sender<T> {
    /// Send the value, completing the receiver. Passes through untouched —
    /// sending never blocks.
    pub fn send(self, value: T) -> Result<(), T> {
        self.inner.send(value)
    }
}

pin_project! {
    /// The receiving half; a future recording a park the first time it goes
    /// pending.
    pub struct Receiver<T> {
        #[pin]
        inner: toneshot::Receiver<T>,
        id: ResourceId,
        parked: bool,
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Ready(out) => Poll::Ready(out),
            Poll::Pending => {
                if !*this.parked {
                    *this.parked = true;
                    if let Some(t) = current_task() {
                        emit(Event::Park {
                            task: t,
                            resource: *this.id,
                            op_name: "recv".into(),
                        });
                    }
                }
                Poll::Pending
            }
        }
    }
}
