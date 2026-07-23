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
//! - [`Recording::lint`] — distill the above into findings: deadlocks,
//!   blocking-in-async long polls, long parks, saturated channels.
//!
//! Query results are plain serde-serializable structs (see [`model`]) shared
//! by the `wyrd` CLI and the `wyrd-mcp` MCP server.

#![forbid(unsafe_code)]

mod diff;
mod error;
mod ingest;
mod latency;
mod lint;
pub mod model;
mod predict;
mod query;

use std::path::Path;

use rusqlite::Connection;

pub use diff::diff;
pub use error::CoreError;
pub use wyrd_weave::{ResourceId, TaskId};

use model::{
    BlockedReport, LatencyReport, LintConfig, LintReport, PredictConfig, PredictReport, Stats,
    TaskStatus, WorldState,
};

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

    /// Ingest a recording file, tolerating a truncated tail — the normal
    /// shape of a recording whose process was killed mid-write (a hung or
    /// deadlocked run under `wyrd hunt`, a crashed app, ...). Frames after the
    /// first undecodable one are discarded; returns whether truncation was
    /// detected alongside the recording.
    pub fn open_lossy(path: impl AsRef<Path>) -> Result<(Self, bool), CoreError> {
        let file = std::fs::File::open(path.as_ref()).map_err(wyrd_weave::WeaveError::from)?;
        let mut reader = wyrd_weave::FrameReader::new(std::io::BufReader::new(file))?;
        let mut records = Vec::new();
        let mut truncated = false;
        loop {
            match reader.next_record() {
                Ok(Some(r)) => records.push(r),
                Ok(None) => break,
                Err(_) => {
                    truncated = true;
                    break;
                }
            }
        }
        Ok((Self::from_records(records)?, truncated))
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

    /// Scan the recording (up to `at`, default: end) for async anti-patterns:
    /// deadlocks, blocking-in-async long polls, suspiciously long parks, and
    /// saturated channels. See [`model::LintConfig`] for the thresholds.
    pub fn lint(&self, at: Option<u64>, config: &LintConfig) -> Result<LintReport, CoreError> {
        let at = self.at_or_end(at)?;
        lint::lint(&self.conn, at, config)
    }

    /// Scan the recording (up to `at`, default: end) for **potential**
    /// deadlocks: lock-order inversions witnessed by distinct tasks with no
    /// common gate lock — cycles that could deadlock under another
    /// interleaving even though this run may have completed cleanly. Cycles
    /// that *did* deadlock in this recording are marked `observed`.
    pub fn predict(
        &self,
        at: Option<u64>,
        config: &PredictConfig,
    ) -> Result<PredictReport, CoreError> {
        let at = self.at_or_end(at)?;
        predict::predict(&self.conn, at, config)
    }

    /// Attribute where a task's lifetime went: own poll time, resource waits
    /// (blamed on holders), timer waits, scheduler lag (woken → polled), and
    /// idle. `at` clips the window for still-running tasks (default: end);
    /// `top_n` bounds the reported wait episodes.
    pub fn why_slow(
        &self,
        task: TaskId,
        at: Option<u64>,
        top_n: usize,
    ) -> Result<LatencyReport, CoreError> {
        let at = self.at_or_end(at)?;
        latency::why_slow(&self.conn, task, at, top_n)
    }

    /// Choose a task worth latency-explaining when the caller didn't name
    /// one: the task with the most total parked time, else the longest-lived.
    pub fn pick_slow_task(&self, at: Option<u64>) -> Result<Option<TaskId>, CoreError> {
        let at = self.at_or_end(at)?;
        latency::pick_slow_task(&self.conn, at)
    }

    /// The underlying connection, for sibling modules ([`diff`]).
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Choose a task worth explaining when the caller didn't name one. Prefer
    /// a task blocked *behind another task* (parked on a resource someone else
    /// holds) — the interesting case — then any parked task, then the
    /// last-spawned task. `None` only if the recording contains no tasks.
    pub fn pick_blocked_task(&self, at: Option<u64>) -> Result<Option<TaskId>, CoreError> {
        let world = self.world_state(at)?;
        let holder_of = |resource| {
            world
                .resources
                .iter()
                .find(|r| r.ident.id == resource)
                .and_then(|r| r.holder)
        };

        // 1. Parked on a resource held by a *different* task.
        for t in &world.tasks {
            if let TaskStatus::Parked { resource } = t.status {
                if holder_of(resource).is_some_and(|h| h != t.ident.id) {
                    return Ok(Some(t.ident.id));
                }
            }
        }
        // 2. Any parked task.
        if let Some(t) = world
            .tasks
            .iter()
            .find(|t| matches!(t.status, TaskStatus::Parked { .. }))
        {
            return Ok(Some(t.ident.id));
        }
        // 3. Fall back to the last-spawned task.
        Ok(world.tasks.last().map(|t| t.ident.id))
    }

    fn at_or_end(&self, at: Option<u64>) -> Result<u64, CoreError> {
        match at {
            Some(t) => Ok(t),
            None => self.end_ts(),
        }
    }
}
