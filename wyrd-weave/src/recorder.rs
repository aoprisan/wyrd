//! A [`Recorder`]: the write side of a recording, decoupled from the tracing
//! layer.
//!
//! The [`WeaveLayer`](crate::WeaveLayer) is one producer of [`Event`]s, but not
//! the only possible one. A `Recorder` owns the monotonic clock and the handle
//! to the writer thread, and exposes a single [`emit`](Recorder::emit) method.
//! Anything that can construct wyrd [`Event`]s — for example a set of stable,
//! non-`tokio_unstable` wrapper types — can drive a recording through it.

use std::path::PathBuf;
use std::time::Instant;

use crate::error::WeaveError;
use crate::event::{Event, Record};
use crate::writer::{spawn_writer, FlushGuard, WriterHandle};

/// The write side of a recording: a monotonic clock plus a bounded, non-blocking
/// channel to the writer thread.
///
/// Cheap to clone — clones share the same writer thread and drop counter, so
/// multiple producers can record into one file.
#[derive(Clone)]
pub struct Recorder {
    start: Instant,
    writer: WriterHandle,
}

impl Recorder {
    /// Start configuring a recorder.
    pub fn builder() -> RecorderBuilder {
        RecorderBuilder::default()
    }

    /// Timestamp an event and hand it to the writer thread. Never blocks; if the
    /// queue is full the event is dropped and counted (see
    /// [`FlushGuard::dropped_events`]).
    pub fn emit(&self, event: Event) {
        let ts = self.start.elapsed().as_nanos() as u64;
        self.writer.send(Record { ts, event });
    }

    /// Nanoseconds elapsed since this recorder was built (the timebase used for
    /// event timestamps).
    pub fn elapsed_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    pub(crate) fn from_parts(start: Instant, writer: WriterHandle) -> Self {
        Self { start, writer }
    }
}

/// Builder for [`Recorder`].
pub struct RecorderBuilder {
    path: Option<PathBuf>,
    queue_capacity: usize,
    batch_size: usize,
}

impl Default for RecorderBuilder {
    fn default() -> Self {
        Self {
            path: None,
            queue_capacity: 64 * 1024,
            batch_size: 256,
        }
    }
}

impl RecorderBuilder {
    /// Destination recording file (required).
    pub fn file(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Bounded queue depth between producers and the writer thread.
    pub fn queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity;
        self
    }

    /// How many records to buffer before flushing to disk.
    pub fn batch_size(mut self, batch: usize) -> Self {
        self.batch_size = batch;
        self
    }

    /// Build the recorder and its finalization guard. Keep the guard alive for
    /// the duration of recording; dropping it flushes and closes the file.
    pub fn build(self) -> Result<(Recorder, FlushGuard), WeaveError> {
        let path = self.path.ok_or(WeaveError::NoPath)?;
        let (writer, guard) = spawn_writer(&path, self.queue_capacity, self.batch_size)?;
        Ok((Recorder::from_parts(Instant::now(), writer), guard))
    }
}
