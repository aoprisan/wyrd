//! # wyrd-shim
//!
//! Stable-Rust tokio wrappers that record the **same** wyrd causality events as
//! the `wyrd-weave` tracing layer — **without** `--cfg tokio_unstable`.
//!
//! Where `wyrd-weave` reads tokio's internal (unstable) instrumentation and so
//! sees *every* task and resource, this shim instead instruments a handful of
//! wrapper types from the outside. The trade-off:
//!
//! - **+** works on stable Rust, no `tokio_unstable`, and gives cleaner holder
//!   tracking and real source locations (via `#[track_caller]`).
//! - **−** only sees tasks spawned with [`spawn`] and resources built from this
//!   crate's [`Mutex`]/[`mpsc`] — not primitives used inside dependencies.
//!
//! Because it emits the identical [`wyrd_weave::Event`] vocabulary, recordings
//! are consumed by the unchanged `wyrd-core` / `wyrd` CLI.
//!
//! ```no_run
//! # async fn ex() {
//! let _guard = wyrd_shim::init("run.wyrd").expect("init recording");
//! let lock = std::sync::Arc::new(wyrd_shim::Mutex::new(0u64));
//! let l = lock.clone();
//! wyrd_shim::spawn(async move {
//!     let mut g = l.lock().await;
//!     *g += 1;
//! });
//! # }
//! ```

#![forbid(unsafe_code)]

use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use wyrd_weave::{Event, FlushGuard, Loc, Recorder, TaskId, TaskKind};

pub mod mpsc;
mod sync;

pub use sync::{Mutex, MutexGuard};

/// Error returned by [`init`].
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error(transparent)]
    Weave(#[from] wyrd_weave::WeaveError),
    #[error("wyrd-shim recorder already initialized")]
    AlreadyInitialized,
}

static RECORDER: OnceLock<Recorder> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

tokio::task_local! {
    /// The wyrd task id of the task currently executing on this future.
    static CURRENT_TASK: TaskId;
}

/// Initialize the global recorder, writing to `path`. Returns a guard that
/// flushes and finalizes the recording when dropped. Call once per process.
pub fn init(path: impl Into<std::path::PathBuf>) -> Result<FlushGuard, InitError> {
    let (recorder, guard) = Recorder::builder().file(path).build()?;
    RECORDER
        .set(recorder)
        .map_err(|_| InitError::AlreadyInitialized)?;
    Ok(guard)
}

/// The wyrd task id of the currently executing [`spawn`]ed task, if any.
pub fn current_task() -> Option<TaskId> {
    CURRENT_TASK.try_with(|id| *id).ok()
}

pub(crate) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn emit(event: Event) {
    if let Some(recorder) = RECORDER.get() {
        recorder.emit(event);
    }
}

pub(crate) fn loc_of(loc: &Location<'static>) -> Loc {
    Loc {
        file: Some(loc.file().to_string()),
        line: Some(loc.line()),
        col: Some(loc.column()),
    }
}

/// Spawn a task, recording its spawn (with the spawner as parent), each poll,
/// and its completion.
#[track_caller]
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    spawn_with(None, Location::caller(), future)
}

/// Like [`spawn`], but attaches a human-readable name to the task.
#[track_caller]
pub fn spawn_named<F>(name: impl Into<String>, future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    spawn_with(Some(name.into()), Location::caller(), future)
}

fn spawn_with<F>(
    name: Option<String>,
    loc: &'static Location<'static>,
    future: F,
) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let id = next_id();
    let parent = current_task();
    emit(Event::TaskSpawn {
        id,
        parent,
        name,
        loc: loc_of(loc),
        kind: TaskKind::Task,
    });
    let instrumented = Instrumented {
        inner: future,
        _end: TaskEndGuard { id },
    };
    tokio::spawn(CURRENT_TASK.scope(id, instrumented))
}

/// Emits `TaskEnd` exactly once, when the instrumented future is dropped.
struct TaskEndGuard {
    id: TaskId,
}

impl Drop for TaskEndGuard {
    fn drop(&mut self) {
        emit(Event::TaskEnd { id: self.id });
    }
}

pin_project! {
    /// Wraps a task future to emit `PollStart`/`PollEnd` around every poll and
    /// `TaskEnd` when dropped. The enclosing [`CURRENT_TASK`] scope makes the
    /// task id available to resource wrappers polled within.
    struct Instrumented<F> {
        #[pin]
        inner: F,
        _end: TaskEndGuard,
    }
}

impl<F: Future> Future for Instrumented<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let id = CURRENT_TASK.try_with(|id| *id).unwrap_or(0);
        emit(Event::PollStart { task: id });
        let this = self.project();
        let out = this.inner.poll(cx);
        emit(Event::PollEnd { task: id });
        out
    }
}
