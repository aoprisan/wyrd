//! # wyrd-weave
//!
//! A `tracing_subscriber::Layer` that normalizes tokio's internal
//! instrumentation (spans and events under the `tokio::*` / `runtime::*`
//! targets, available with `RUSTFLAGS="--cfg tokio_unstable"` and tokio's
//! `tracing` feature) into a compact, on-disk causality event stream.
//!
//! The layer never blocks a runtime worker: normalized [`Record`]s are handed
//! to a dedicated writer thread over a bounded queue and serialized with
//! `postcard` into length-prefixed frames.
//!
//! ```no_run
//! use tracing_subscriber::prelude::*;
//!
//! let (layer, guard) = wyrd_weave::WeaveLayer::builder()
//!     .file("run.wyrd")
//!     .build()
//!     .expect("open recording");
//! tracing_subscriber::registry().with(layer).init();
//! // ... run the instrumented tokio program ...
//! drop(guard); // flush & finalize the recording
//! ```
//!
//! **The instrumented application must be built with**
//! `RUSTFLAGS="--cfg tokio_unstable"` and tokio's `"tracing"` feature, or tokio
//! emits none of the spans wyrd relies on.

#![forbid(unsafe_code)]

mod error;
mod event;
mod format;
mod layer;
mod writer;

pub use error::WeaveError;
pub use event::{Event, Loc, Record, ResourceId, StateOp, TaskId, TaskKind, FIELD_ACQUIRED_BY};
pub use format::{file_writer, read_records, FrameReader, FrameWriter, MAGIC, VERSION};
pub use layer::{WeaveLayer, WeaveLayerBuilder};
pub use writer::FlushGuard;
