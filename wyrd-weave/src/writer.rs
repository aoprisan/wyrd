//! The dedicated writer thread and its handle.
//!
//! The instrumentation layer must never block a runtime worker, so emitted
//! records cross a *bounded* `std::sync::mpsc` channel to a single writer
//! thread. On overflow we drop the record and bump a counter rather than
//! block. Serialization and disk I/O all happen off the hot path.

use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::error::WeaveError;
use crate::event::Record;
use crate::format::{self, FrameWriter};

enum Msg {
    Record(Box<Record>),
    /// Flush and terminate. Sent (blocking) exactly once by the [`FlushGuard`].
    Shutdown,
}

/// Cheap, cloneable producer handle held by the layer.
#[derive(Clone)]
pub(crate) struct WriterHandle {
    tx: SyncSender<Msg>,
    dropped: Arc<AtomicU64>,
}

impl WriterHandle {
    /// Enqueue a record without blocking. If the queue is full or the writer
    /// has gone away, the record is dropped and the drop counter incremented.
    pub(crate) fn send(&self, record: Record) {
        match self.tx.try_send(Msg::Record(Box::new(record))) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Flushes and joins the writer thread when dropped.
///
/// Returned alongside the layer from [`crate::WeaveLayer::builder`]. Keep it
/// alive for the duration of the instrumented program (e.g. bind it in `main`);
/// dropping it finalizes the recording. It sends an explicit shutdown so the
/// writer stops even though the layer still holds a producer handle.
pub struct FlushGuard {
    tx: Option<SyncSender<Msg>>,
    handle: Option<JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
}

impl FlushGuard {
    /// Number of records dropped due to queue overflow so far.
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for FlushGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            // Blocking send: the writer is still draining, so this lands even
            // if the queue was momentarily full.
            let _ = tx.send(Msg::Shutdown);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn the writer thread over `path` and return a producer handle plus the
/// finalization guard.
pub(crate) fn spawn_writer(
    path: &Path,
    capacity: usize,
    batch: usize,
) -> Result<(WriterHandle, FlushGuard), WeaveError> {
    let writer = format::file_writer(path)?;
    let (tx, rx) = sync_channel::<Msg>(capacity.max(1));
    let dropped = Arc::new(AtomicU64::new(0));

    let handle = std::thread::Builder::new()
        .name("wyrd-weave-writer".into())
        .spawn(move || writer_loop(rx, writer, batch.max(1)))?;

    let producer = WriterHandle {
        tx: tx.clone(),
        dropped: dropped.clone(),
    };
    let guard = FlushGuard {
        tx: Some(tx),
        handle: Some(handle),
        dropped,
    };
    Ok((producer, guard))
}

fn writer_loop<W: std::io::Write>(
    rx: Receiver<Msg>,
    mut writer: FrameWriter<BufWriter<W>>,
    batch: usize,
) {
    /// Flush after this much quiet, so a recording is analyzable even when
    /// the process is killed mid-run (a deadlocked app goes quiet exactly
    /// when its recording matters most).
    const IDLE_FLUSH: std::time::Duration = std::time::Duration::from_millis(100);

    // Put the header on disk immediately: a hung-then-killed process must
    // still leave a readable (if truncated) recording.
    let _ = writer.flush();

    let mut since_flush = 0usize;
    loop {
        match rx.recv_timeout(IDLE_FLUSH) {
            Ok(Msg::Record(record)) => {
                if let Err(_e) = writer.write_record(&record) {
                    #[cfg(feature = "diag")]
                    tracing::error!(target: "wyrd::weave", error = %_e, "writer failed; stopping");
                    break;
                }
                since_flush += 1;
                if since_flush >= batch {
                    let _ = writer.flush();
                    since_flush = 0;
                }
            }
            Ok(Msg::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => {
                if since_flush > 0 {
                    let _ = writer.flush();
                    since_flush = 0;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = writer.flush();
}
