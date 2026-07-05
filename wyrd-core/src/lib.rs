//! # wyrd-core
//!
//! Ingests wyrd recordings (produced by `wyrd-weave`) into SQLite and answers
//! causality questions over them:
//!
//! - [`Recording::world_state`] — fold events up to a timestamp into per-task
//!   status and per-resource holder/depth.
//! - [`Recording::why_blocked`] — walk a task's park → resource → holder chain,
//!   detecting deadlock cycles.
//! - [`Recording::stats`] — task counts, poll-time percentiles, longest parks,
//!   channel depths.
//!
//! Query results are plain serde-serializable structs (see [`model`]) so a
//! future MCP server and TUI can share them.

#![forbid(unsafe_code)]

mod error;
mod ingest;
pub mod model;
mod query;

use std::path::Path;

use rusqlite::Connection;

pub use error::CoreError;
pub use wyrd_weave::{ResourceId, TaskId};

use model::{BlockedReport, Stats, WorldState};

/// An ingested recording, backed by an in-memory SQLite database.
pub struct Recording {
    conn: Connection,
}

impl Recording {
    /// Ingest a recording file into a fresh in-memory database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CoreError> {
        let mut conn = Connection::open_in_memory()?;
        ingest::init_schema(&conn)?;
        ingest::ingest_file(&mut conn, path.as_ref())?;
        Ok(Self { conn })
    }

    /// Ingest an in-memory sequence of records (used by tests).
    pub fn from_records(
        records: impl IntoIterator<Item = wyrd_weave::Record>,
    ) -> Result<Self, CoreError> {
        let mut conn = Connection::open_in_memory()?;
        ingest::init_schema(&conn)?;
        ingest::ingest_records(&mut conn, records.into_iter().map(Ok))?;
        Ok(Self { conn })
    }

    /// The last timestamp in the recording (default query time).
    pub fn end_ts(&self) -> Result<u64, CoreError> {
        query::max_ts(&self.conn)
    }

    /// Resolve a task selector (numeric id or `task::Builder` name) to an id.
    pub fn resolve_task(&self, selector: &str) -> Result<TaskId, CoreError> {
        query::resolve_task(&self.conn, selector)
    }

    /// Fold the recording into a world snapshot at `at` (default: end).
    pub fn world_state(&self, at: Option<u64>) -> Result<WorldState, CoreError> {
        let at = self.at_or_end(at)?;
        query::world_state(&self.conn, at)
    }

    /// Explain why `task` is blocked at `at` (default: end), following the
    /// park → holder chain and reporting deadlocks.
    pub fn why_blocked(&self, task: TaskId, at: Option<u64>) -> Result<BlockedReport, CoreError> {
        let at = self.at_or_end(at)?;
        query::why_blocked(&self.conn, task, at)
    }

    /// Aggregate statistics over the whole recording.
    pub fn stats(&self, top_n: usize) -> Result<Stats, CoreError> {
        query::stats(&self.conn, top_n)
    }

    fn at_or_end(&self, at: Option<u64>) -> Result<u64, CoreError> {
        match at {
            Some(t) => Ok(t),
            None => self.end_ts(),
        }
    }
}
