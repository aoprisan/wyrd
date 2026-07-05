//! The normalized wyrd event vocabulary.
//!
//! These are the events wyrd-weave distills tokio's raw span/event firehose
//! into. Identifiers are `u64` tracing span ids (unique among *live* spans);
//! timestamps are monotonic nanoseconds measured from layer construction.

use serde::{Deserialize, Serialize};

/// A tokio task, identified by its `runtime.spawn` span id.
pub type TaskId = u64;

/// A tokio resource (mutex, semaphore, channel, sleep, ...), identified by its
/// `runtime.resource` span id. Internal child resources (e.g. a `Mutex`'s
/// backing `Semaphore`) are collapsed into their parent, so a `ResourceId`
/// always names a user-visible resource.
pub type ResourceId = u64;

/// Reserved [`Event::ResourceState`] field name that records a successful
/// `poll_acquire` (`is_ready = true`): its `value` is the [`TaskId`] that now
/// holds the resource. wyrd-core folds this into per-resource holder state.
pub const FIELD_ACQUIRED_BY: &str = "acquired_by";

/// What flavour of task a `runtime.spawn` span describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskKind {
    /// A normal async task (`tokio::spawn`).
    Task,
    /// A blocking task (`spawn_blocking`, runtime workers).
    Blocking,
    /// The `block_on` root task.
    BlockOn,
    /// An unrecognized `kind` value.
    Other,
}

impl TaskKind {
    pub(crate) fn parse(s: Option<&str>) -> Self {
        match s {
            Some("task") => TaskKind::Task,
            Some("blocking") => TaskKind::Blocking,
            Some("block_on") => TaskKind::BlockOn,
            _ => TaskKind::Other,
        }
    }
}

/// How a resource `state_update` mutates a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateOp {
    /// Absolute assignment (`override`).
    Override,
    /// Increment by `value`.
    Add,
    /// Decrement by `value`.
    Sub,
}

impl StateOp {
    pub(crate) fn parse(s: Option<&str>) -> Self {
        match s {
            Some("add") => StateOp::Add,
            Some("sub") => StateOp::Sub,
            _ => StateOp::Override,
        }
    }
}

/// Source location captured from tokio's `loc.*` fields. Every field is
/// optional because internal resources often omit them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loc {
    pub file: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
}

impl Loc {
    /// `true` if no location information was captured.
    pub fn is_empty(&self) -> bool {
        self.file.is_none() && self.line.is_none() && self.col.is_none()
    }
}

impl std::fmt::Display for Loc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.file, self.line) {
            (Some(file), Some(line)) => write!(f, "{file}:{line}"),
            (Some(file), None) => write!(f, "{file}"),
            _ => write!(f, "<unknown>"),
        }
    }
}

/// A single normalized causality event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// A task span was created. `parent` is the spawning task, if any.
    TaskSpawn {
        id: TaskId,
        parent: Option<TaskId>,
        name: Option<String>,
        loc: Loc,
        kind: TaskKind,
    },
    /// A task span was entered (a poll began).
    PollStart { task: TaskId },
    /// A task span was exited (a poll ended).
    PollEnd { task: TaskId },
    /// A task parked on a resource: `poll_*` returned not-ready.
    Park {
        task: TaskId,
        resource: ResourceId,
        op_name: String,
    },
    /// A task woke another (`waker.wake` / `waker.wake_by_ref`). `by` is the
    /// task doing the waking, or `None` for the runtime/timer driver.
    Wake {
        woken: TaskId,
        by: Option<TaskId>,
    },
    /// A resource span was created.
    ResourceNew {
        id: ResourceId,
        parent: Option<ResourceId>,
        concrete_type: String,
        loc: Loc,
        is_internal: bool,
    },
    /// A resource field changed. See [`FIELD_ACQUIRED_BY`] for the reserved
    /// holder-tracking field.
    ResourceState {
        id: ResourceId,
        field: String,
        value: i64,
        op: StateOp,
    },
    /// A task span closed (the task completed).
    TaskEnd { id: TaskId },
    /// A resource span closed (the resource was dropped).
    ResourceDrop { id: ResourceId },
}

/// A timestamped event as stored in a recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Monotonic nanoseconds since the layer was built.
    pub ts: u64,
    pub event: Event,
}
