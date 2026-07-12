//! World-state fold and causality queries over an ingested recording.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension};
use wyrd_weave::{Loc, ResourceId, TaskId, TaskKind, FIELD_ACQUIRED_BY};

use crate::error::CoreError;
use crate::model::*;

// --- identity helpers -------------------------------------------------------

fn parse_kind(s: &str) -> TaskKind {
    match s {
        "task" => TaskKind::Task,
        "blocking" => TaskKind::Blocking,
        "block_on" => TaskKind::BlockOn,
        _ => TaskKind::Other,
    }
}

pub(crate) fn task_ident(conn: &Connection, id: TaskId) -> Result<TaskIdent, CoreError> {
    let ident = conn
        .query_row(
            "SELECT name, kind, loc_file, loc_line FROM tasks WHERE id = ?1",
            params![id as i64],
            |r| {
                let name: Option<String> = r.get(0)?;
                let kind: String = r.get(1)?;
                let file: Option<String> = r.get(2)?;
                let line: Option<u32> = r.get(3)?;
                Ok(TaskIdent {
                    id,
                    name,
                    kind: parse_kind(&kind),
                    loc: Loc {
                        file,
                        line,
                        col: None,
                    },
                })
            },
        )
        .optional()?;
    Ok(ident.unwrap_or(TaskIdent {
        id,
        name: None,
        kind: TaskKind::Other,
        loc: Loc::default(),
    }))
}

pub(crate) fn resource_ident(
    conn: &Connection,
    id: ResourceId,
) -> Result<ResourceIdent, CoreError> {
    let ident = conn
        .query_row(
            "SELECT concrete_type, loc_file, loc_line FROM resources WHERE id = ?1",
            params![id as i64],
            |r| {
                let concrete_type: String = r.get(0)?;
                let file: Option<String> = r.get(1)?;
                let line: Option<u32> = r.get(2)?;
                Ok(ResourceIdent {
                    id,
                    concrete_type,
                    loc: Loc {
                        file,
                        line,
                        col: None,
                    },
                })
            },
        )
        .optional()?;
    Ok(ident.unwrap_or(ResourceIdent {
        id,
        concrete_type: "?".into(),
        loc: Loc::default(),
    }))
}

// --- time bounds ------------------------------------------------------------

/// Latest timestamp anywhere in the recording (end of recording).
pub(crate) fn max_ts(conn: &Connection) -> Result<u64, CoreError> {
    let v: Option<i64> = conn.query_row(
        "SELECT MAX(t) FROM (
            SELECT MAX(spawn_ts) t FROM tasks UNION ALL
            SELECT MAX(end_ts)   FROM tasks UNION ALL
            SELECT MAX(new_ts)   FROM resources UNION ALL
            SELECT MAX(drop_ts)  FROM resources UNION ALL
            SELECT MAX(start_ts) FROM polls UNION ALL
            SELECT MAX(end_ts)   FROM polls UNION ALL
            SELECT MAX(ts)       FROM parks UNION ALL
            SELECT MAX(ts)       FROM wakes UNION ALL
            SELECT MAX(ts)       FROM resource_state
        )",
        [],
        |r| r.get(0),
    )?;
    Ok(v.unwrap_or(0) as u64)
}

pub(crate) fn min_ts(conn: &Connection) -> Result<u64, CoreError> {
    let v: Option<i64> = conn.query_row(
        "SELECT MIN(t) FROM (
            SELECT MIN(spawn_ts) t FROM tasks UNION ALL
            SELECT MIN(new_ts)     FROM resources UNION ALL
            SELECT MIN(start_ts)   FROM polls UNION ALL
            SELECT MIN(ts)         FROM parks UNION ALL
            SELECT MIN(ts)         FROM resource_state
        )",
        [],
        |r| r.get(0),
    )?;
    Ok(v.unwrap_or(0) as u64)
}

/// Resolve a task selector (numeric id or task name) to a task id.
pub(crate) fn resolve_task(conn: &Connection, sel: &str) -> Result<TaskId, CoreError> {
    if let Ok(id) = sel.parse::<u64>() {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)",
            params![id as i64],
            |r| r.get(0),
        )?;
        if exists {
            return Ok(id);
        }
    }
    let by_name: Option<i64> = conn
        .query_row(
            "SELECT id FROM tasks WHERE name = ?1 ORDER BY spawn_ts LIMIT 1",
            params![sel],
            |r| r.get(0),
        )
        .optional()?;
    by_name
        .map(|id| id as u64)
        .ok_or_else(|| CoreError::UnknownTask(sel.to_owned()))
}

fn task_done_at(conn: &Connection, id: TaskId, t: u64) -> Result<bool, CoreError> {
    let end: Option<Option<i64>> = conn
        .query_row(
            "SELECT end_ts FROM tasks WHERE id = ?1",
            params![id as i64],
            |r| r.get(0),
        )
        .optional()?;
    Ok(matches!(end, Some(Some(e)) if (e as u64) <= t))
}

// --- per-entity folds -------------------------------------------------------

pub(crate) fn task_status(conn: &Connection, id: TaskId, t: u64) -> Result<TaskStatus, CoreError> {
    if task_done_at(conn, id, t)? {
        return Ok(TaskStatus::Done);
    }
    // Currently being polled?
    let polling: bool = conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM polls
            WHERE task = ?1 AND start_ts <= ?2 AND (end_ts IS NULL OR end_ts > ?2)
        )",
        params![id as i64, t as i64],
        |r| r.get(0),
    )?;
    if polling {
        return Ok(TaskStatus::Running);
    }
    // Last completed poll at or before t.
    let last: Option<(i64, i64)> = conn
        .query_row(
            "SELECT start_ts, end_ts FROM polls
             WHERE task = ?1 AND end_ts IS NOT NULL AND end_ts <= ?2
             ORDER BY end_ts DESC LIMIT 1",
            params![id as i64, t as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((start, end)) = last {
        // Did that poll end by parking on a resource?
        let parked: Option<i64> = conn
            .query_row(
                "SELECT resource FROM parks
                 WHERE task = ?1 AND ts >= ?2 AND ts <= ?3
                 ORDER BY ts DESC LIMIT 1",
                params![id as i64, start, end],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(res) = parked {
            return Ok(TaskStatus::Parked {
                resource: res as u64,
            });
        }
    }
    Ok(TaskStatus::Idle)
}

/// The presumed holder of a resource at time `t`: the last successful acquirer
/// that has not since released (mutex `locked = 0`), whose resource is still
/// alive, and who is not already done.
pub(crate) fn resource_holder(
    conn: &Connection,
    id: ResourceId,
    t: u64,
) -> Result<Option<TaskId>, CoreError> {
    // Resource dropped by t → nobody holds it.
    let dropped: Option<Option<i64>> = conn
        .query_row(
            "SELECT drop_ts FROM resources WHERE id = ?1",
            params![id as i64],
            |r| r.get(0),
        )
        .optional()?;
    if matches!(dropped, Some(Some(d)) if (d as u64) <= t) {
        return Ok(None);
    }

    let mut stmt = conn.prepare(
        "SELECT field, value FROM resource_state
         WHERE resource = ?1 AND ts <= ?2 ORDER BY ts, id",
    )?;
    let rows = stmt.query_map(params![id as i64, t as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;

    let mut holder: Option<TaskId> = None;
    for row in rows {
        let (field, value) = row?;
        match field.as_str() {
            FIELD_ACQUIRED_BY => holder = Some(value as u64),
            "locked" if value == 0 => holder = None,
            _ => {}
        }
    }

    // A holder that has itself completed no longer holds anything.
    if let Some(h) = holder {
        if task_done_at(conn, h, t)? {
            holder = None;
        }
    }
    Ok(holder)
}

/// Folded permit/lock state of a resource at an instant.
#[derive(Default)]
struct PermitState {
    permits: Option<i64>,
    capacity: Option<i64>,
    locked: Option<bool>,
}

/// Fold a resource's permit/lock state up to time `t`.
fn resource_permits(conn: &Connection, id: ResourceId, t: u64) -> Result<PermitState, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT field, value, op FROM resource_state
         WHERE resource = ?1 AND ts <= ?2 ORDER BY ts, id",
    )?;
    let rows = stmt.query_map(params![id as i64, t as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;

    let mut st = PermitState::default();
    for row in rows {
        let (field, value, op) = row?;
        match field.as_str() {
            "permits" => match op.as_str() {
                "add" => st.permits = Some(st.permits.unwrap_or(0) + value),
                "sub" => st.permits = Some(st.permits.unwrap_or(0) - value),
                _ => {
                    st.permits = Some(value);
                    if st.capacity.is_none() {
                        st.capacity = Some(value);
                    }
                }
            },
            "locked" => st.locked = Some(value != 0),
            _ => {}
        }
    }
    Ok(st)
}

// --- world state ------------------------------------------------------------

pub(crate) fn world_state(conn: &Connection, at: u64) -> Result<WorldState, CoreError> {
    let mut task_ids: Vec<i64> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM tasks ORDER BY spawn_ts, id")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        for row in rows {
            task_ids.push(row?);
        }
    }
    let mut tasks = Vec::with_capacity(task_ids.len());
    for id in task_ids {
        let id = id as u64;
        // Only surface tasks that exist by `at`.
        let (spawn, parent): (i64, Option<i64>) = conn.query_row(
            "SELECT spawn_ts, parent FROM tasks WHERE id = ?1",
            params![id as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if (spawn as u64) > at {
            continue;
        }
        tasks.push(TaskState {
            ident: task_ident(conn, id)?,
            status: task_status(conn, id, at)?,
            parent: parent.map(|p| p as u64),
        });
    }

    let mut res_ids: Vec<i64> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM resources ORDER BY new_ts, id")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        for row in rows {
            res_ids.push(row?);
        }
    }
    let mut resources = Vec::with_capacity(res_ids.len());
    for id in res_ids {
        let id = id as u64;
        let new_ts: i64 = conn.query_row(
            "SELECT new_ts FROM resources WHERE id = ?1",
            params![id as i64],
            |r| r.get(0),
        )?;
        if (new_ts as u64) > at {
            continue;
        }
        let st = resource_permits(conn, id, at)?;
        let depth = match (st.capacity, st.permits) {
            (Some(c), Some(p)) => Some(c - p),
            _ => None,
        };
        resources.push(ResourceStateView {
            ident: resource_ident(conn, id)?,
            holder: resource_holder(conn, id, at)?,
            locked: st.locked,
            permits: st.permits,
            capacity: st.capacity,
            depth,
        });
    }

    Ok(WorldState {
        at,
        tasks,
        resources,
    })
}

// --- why blocked ------------------------------------------------------------

/// When did `task` most recently park on `resource` at or before `t`?
fn latest_park(
    conn: &Connection,
    task: TaskId,
    resource: ResourceId,
    t: u64,
) -> Result<(u64, String), CoreError> {
    // Prefer a specific op (e.g. `poll_acquire`) over the generic mutex-level
    // `poll`, then the most recent.
    let row = conn
        .query_row(
            "SELECT ts, op_name FROM parks
             WHERE task = ?1 AND resource = ?2 AND ts <= ?3
             ORDER BY (op_name = 'poll') ASC, ts DESC LIMIT 1",
            params![task as i64, resource as i64, t as i64],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?)),
        )
        .optional()?;
    Ok(row.unwrap_or((t, "poll".into())))
}

pub(crate) fn why_blocked(
    conn: &Connection,
    task: TaskId,
    t: u64,
) -> Result<BlockedReport, CoreError> {
    let mut chain: Vec<BlockedLink> = Vec::new();
    let mut seen: HashSet<TaskId> = HashSet::new();
    let mut current = task;

    let outcome = loop {
        let status = task_status(conn, current, t)?;
        let TaskStatus::Parked { resource } = status else {
            break if chain.is_empty() {
                BlockedOutcome::NotBlocked
            } else {
                BlockedOutcome::ActiveHolder { task: current }
            };
        };

        let (since_ts, op_name) = latest_park(conn, current, resource, t)?;
        let holder = resource_holder(conn, resource, t)?;
        let holder_ident = match holder {
            Some(h) => Some(task_ident(conn, h)?),
            None => None,
        };
        chain.push(BlockedLink {
            task: task_ident(conn, current)?,
            waiting_on: resource_ident(conn, resource)?,
            op_name,
            since_ts,
            wait_ns: t.saturating_sub(since_ts),
            holder: holder_ident,
        });
        seen.insert(current);

        match holder {
            None => break BlockedOutcome::ResourceRoot { resource },
            Some(h) => {
                if seen.contains(&h) {
                    // Close the cycle: the tasks forming the deadlock.
                    let mut cycle: Vec<TaskId> = chain.iter().map(|l| l.task.id).collect();
                    // Trim to the cycle starting at h.
                    if let Some(pos) = cycle.iter().position(|&x| x == h) {
                        cycle = cycle[pos..].to_vec();
                    }
                    break BlockedOutcome::Deadlock { cycle };
                }
                current = h;
            }
        }
    };

    Ok(BlockedReport {
        task,
        at: t,
        chain,
        outcome,
    })
}

// --- stats ------------------------------------------------------------------

fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

pub(crate) fn stats(conn: &Connection, top_n: usize) -> Result<Stats, CoreError> {
    let task_count: u64 =
        conn.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get::<_, i64>(0))? as u64;
    let resource_count: u64 =
        conn.query_row("SELECT COUNT(*) FROM resources", [], |r| r.get::<_, i64>(0))? as u64;

    let lo = min_ts(conn)?;
    let hi = max_ts(conn)?;
    let duration_ns = hi.saturating_sub(lo);

    // Poll durations.
    let mut durations: Vec<u64> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT end_ts - start_ts FROM polls WHERE end_ts IS NOT NULL AND end_ts >= start_ts",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        for row in rows {
            durations.push(row? as u64);
        }
    }
    durations.sort_unstable();
    let poll_time = PollPercentiles {
        count: durations.len() as u64,
        p50: percentile(&durations, 0.50),
        p90: percentile(&durations, 0.90),
        p99: percentile(&durations, 0.99),
        max: durations.last().copied().unwrap_or(0),
    };

    // Longest parks: each park lasts until the task's next poll, else until the
    // task ends, else until end-of-recording.
    let mut parks = park_episodes(conn, hi)?;
    parks.sort_by(|a, b| b.dur_ns.cmp(&a.dur_ns));
    parks.truncate(top_n);

    let channel_depths = channel_depths(conn)?;

    Ok(Stats {
        duration_ns,
        task_count,
        resource_count,
        poll_time,
        longest_parks: parks,
        channel_depths,
    })
}

/// Every park episode starting at or before `hi`, unsorted. Each park lasts
/// until the task's next poll, else until the task ends, else until `hi`;
/// episodes still open at `hi` are clipped there.
pub(crate) fn park_episodes(conn: &Connection, hi: u64) -> Result<Vec<ParkStat>, CoreError> {
    let mut parks: Vec<ParkStat> = Vec::new();
    let mut stmt =
        conn.prepare("SELECT task, resource, op_name, ts FROM parks WHERE ts <= ?1 ORDER BY ts")?;
    let rows = stmt.query_map(params![hi as i64], |r| {
        Ok((
            r.get::<_, i64>(0)? as u64,
            r.get::<_, i64>(1)? as u64,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)? as u64,
        ))
    })?;
    for row in rows {
        let (task, resource, op_name, ts) = row?;
        let next_poll: Option<i64> = conn
            .query_row(
                "SELECT MIN(start_ts) FROM polls WHERE task = ?1 AND start_ts > ?2",
                params![task as i64, ts as i64],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let end = match next_poll {
            Some(n) => n as u64,
            None => {
                let te: Option<Option<i64>> = conn
                    .query_row(
                        "SELECT end_ts FROM tasks WHERE id = ?1",
                        params![task as i64],
                        |r| r.get(0),
                    )
                    .optional()?;
                match te {
                    Some(Some(e)) => e as u64,
                    _ => hi,
                }
            }
        };
        parks.push(ParkStat {
            task: task_ident(conn, task)?,
            resource: resource_ident(conn, resource)?,
            op_name,
            since_ts: ts,
            dur_ns: end.min(hi).saturating_sub(ts),
        });
    }
    Ok(parks)
}

/// Max observed depth of every bounded resource (capacity > 1), deepest first.
pub(crate) fn channel_depths(conn: &Connection) -> Result<Vec<ChannelDepth>, CoreError> {
    let mut channel_depths: Vec<ChannelDepth> = Vec::new();
    let mut res_ids: Vec<i64> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM resources ORDER BY new_ts, id")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        for row in rows {
            res_ids.push(row?);
        }
    }
    for id in res_ids {
        let id = id as u64;
        if let Some((capacity, max_depth)) = max_channel_depth(conn, id)? {
            if capacity > 1 {
                channel_depths.push(ChannelDepth {
                    resource: resource_ident(conn, id)?,
                    capacity,
                    max_depth,
                });
            }
        }
    }
    channel_depths.sort_by(|a, b| b.max_depth.cmp(&a.max_depth));
    Ok(channel_depths)
}

/// Fold a resource's permits over all time, returning `(capacity, max_depth)`
/// where `max_depth = capacity - min(permits)`.
fn max_channel_depth(conn: &Connection, id: ResourceId) -> Result<Option<(i64, i64)>, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT value, op FROM resource_state
         WHERE resource = ?1 AND field = 'permits' ORDER BY ts, id",
    )?;
    let rows = stmt.query_map(params![id as i64], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;

    let mut permits: Option<i64> = None;
    let mut capacity: Option<i64> = None;
    let mut min_permits: Option<i64> = None;
    for row in rows {
        let (value, op) = row?;
        let p = match op.as_str() {
            "add" => permits.unwrap_or(0) + value,
            "sub" => permits.unwrap_or(0) - value,
            _ => {
                if capacity.is_none() {
                    capacity = Some(value);
                }
                value
            }
        };
        permits = Some(p);
        min_permits = Some(min_permits.map_or(p, |m: i64| m.min(p)));
    }
    match (capacity, min_permits) {
        (Some(c), Some(m)) => Ok(Some((c, c - m))),
        _ => Ok(None),
    }
}
