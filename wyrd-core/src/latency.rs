//! `why_slow`: causal latency attribution.
//!
//! Decomposes a task's lifetime into *where the time actually went*: its own
//! poll time, waits on resources (blamed on the holder and what the holder
//! was doing meanwhile), intentional timer waits, scheduler lag (woken but
//! not yet polled — this is the only query that reads the `wakes` table),
//! and unattributed idle time. The buckets partition the window exactly.

use rusqlite::{params, Connection, OptionalExtension};
use wyrd_weave::{ResourceId, TaskId};

use crate::error::CoreError;
use crate::model::*;
use crate::query;

/// Resources a task is *supposed* to wait on for a long time.
fn is_timer(concrete_type: &str) -> bool {
    matches!(concrete_type, "Sleep" | "Interval" | "Timeout")
}

/// A poll interval of the inspected task, clipped to the window.
struct PollSpan {
    start: u64,
    end: u64,
}

/// Load `task`'s polls overlapping `[w0, w1]`, clipped to the window. Open
/// polls (no `PollEnd` yet) run to `w1`.
fn polls_in(conn: &Connection, task: TaskId, w0: u64, w1: u64) -> Result<Vec<PollSpan>, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT start_ts, end_ts FROM polls
         WHERE task = ?1 AND start_ts <= ?2 AND (end_ts IS NULL OR end_ts >= ?3)
         ORDER BY start_ts",
    )?;
    let rows = stmt.query_map(params![task as i64, w1 as i64, w0 as i64], |r| {
        Ok((r.get::<_, i64>(0)? as u64, r.get::<_, Option<i64>>(1)?))
    })?;
    let mut polls = Vec::new();
    for row in rows {
        let (start, end) = row?;
        let end = end.map_or(w1, |e| (e as u64).min(w1));
        let start = start.max(w0);
        if end >= start {
            polls.push(PollSpan { start, end });
        }
    }
    Ok(polls)
}

/// The latest park `task` registered during the poll `[p_start, p_end]`, if
/// any: how that poll ended.
fn park_in_poll(
    conn: &Connection,
    task: TaskId,
    p_start: u64,
    p_end: u64,
) -> Result<Option<(ResourceId, String, u64)>, CoreError> {
    let row = conn
        .query_row(
            "SELECT resource, op_name, ts FROM parks
             WHERE task = ?1 AND ts >= ?2 AND ts <= ?3
             ORDER BY (op_name = 'poll') ASC, ts DESC LIMIT 1",
            params![task as i64, p_start as i64, p_end as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as u64,
                ))
            },
        )
        .optional()?;
    Ok(row)
}

/// First wake of `task` strictly inside `(g0, g1]`.
fn first_wake_in(
    conn: &Connection,
    task: TaskId,
    g0: u64,
    g1: u64,
) -> Result<Option<u64>, CoreError> {
    let v: Option<i64> = conn
        .query_row(
            "SELECT MIN(ts) FROM wakes WHERE woken = ?1 AND ts > ?2 AND ts <= ?3",
            params![task as i64, g0 as i64, g1 as i64],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(v.map(|v| v as u64))
}

/// What `holder` was doing during `[w0, w1]`: polling, parked (on what), or
/// neither.
fn holder_activity(
    conn: &Connection,
    holder: TaskId,
    w0: u64,
    w1: u64,
) -> Result<HolderActivity, CoreError> {
    let polling_ns: u64 = polls_in(conn, holder, w0, w1)?
        .iter()
        .map(|p| p.end - p.start)
        .sum();

    // The holder's park episodes overlapping the window, clipped to it.
    let mut parked_ns = 0u64;
    let mut dominant: Option<(u64, ResourceId)> = None;
    for p in query::park_episodes(conn, w1)? {
        if p.task.id != holder {
            continue;
        }
        let start = p.since_ts.max(w0);
        let end = (p.since_ts + p.dur_ns).min(w1);
        if end <= start {
            continue;
        }
        let dur = end - start;
        parked_ns += dur;
        if dominant.map_or(true, |(d, _)| dur > d) {
            dominant = Some((dur, p.resource.id));
        }
    }
    let parked_on = match dominant {
        Some((_, res)) => Some(query::resource_ident(conn, res)?),
        None => None,
    };

    Ok(HolderActivity {
        task: query::task_ident(conn, holder)?,
        polling_ns,
        parked_ns,
        parked_on,
    })
}

pub(crate) fn why_slow(
    conn: &Connection,
    task: TaskId,
    at: u64,
    top_n: usize,
) -> Result<LatencyReport, CoreError> {
    let ident = query::task_ident(conn, task)?;

    // Window: spawn → end (or `at` when still alive / ended later).
    let (spawn_ts, end_ts): (i64, Option<i64>) = conn
        .query_row(
            "SELECT spawn_ts, end_ts FROM tasks WHERE id = ?1",
            params![task as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| CoreError::UnknownTask(format!("task#{task}")))?;
    let w0 = (spawn_ts as u64).min(at);
    let w1 = end_ts.map_or(at, |e| (e as u64).min(at));
    let total_ns = w1.saturating_sub(w0);

    let polls = polls_in(conn, task, w0, w1)?;
    let own_poll_ns: u64 = polls.iter().map(|p| p.end - p.start).sum();
    let poll_count = polls.len() as u64;

    // Walk the gaps between activity. Each gap is classified by how the
    // preceding poll ended (parked → wait, else idle) and split at the first
    // wake inside it (after the wake the task is runnable: scheduler lag).
    let mut resource_wait_ns = 0u64;
    let mut timer_wait_ns = 0u64;
    let mut sched_lag_ns = 0u64;
    let mut idle_ns = 0u64;
    let mut waits: Vec<WaitEpisode> = Vec::new();

    // (gap start, poll that preceded it — None for the spawn → first-poll gap)
    let mut gaps: Vec<(u64, u64, Option<&PollSpan>)> = Vec::new();
    let mut cursor = w0;
    for (i, p) in polls.iter().enumerate() {
        if p.start > cursor {
            let prev = if i == 0 { None } else { Some(&polls[i - 1]) };
            gaps.push((cursor, p.start, prev));
        }
        cursor = cursor.max(p.end);
    }
    if w1 > cursor {
        gaps.push((cursor, w1, polls.last()));
    }

    for (g0, g1, prev_poll) in gaps {
        let gap = g1 - g0;
        let park = match prev_poll {
            Some(p) => park_in_poll(conn, task, p.start, p.end)?,
            None => None,
        };
        let wake = first_wake_in(conn, task, g0, g1)?;
        let (pre_wake, post_wake) = match wake {
            Some(w) => (w - g0, g1 - w),
            None => (gap, 0),
        };
        sched_lag_ns += post_wake;

        match park {
            Some((resource, op_name, since_ts)) => {
                let res_ident = query::resource_ident(conn, resource)?;
                let timer = is_timer(&res_ident.concrete_type);
                if timer {
                    timer_wait_ns += pre_wake;
                } else {
                    resource_wait_ns += pre_wake;
                }
                // Blame: who held the resource while we waited?
                let wait_end = wake.unwrap_or(g1);
                let holder = if timer {
                    None
                } else {
                    match query::resource_holder(conn, resource, since_ts.max(g0))? {
                        Some(h) if h != task => Some(holder_activity(conn, h, g0, wait_end)?),
                        _ => None,
                    }
                };
                waits.push(WaitEpisode {
                    resource: res_ident,
                    op_name,
                    since_ts,
                    wait_ns: pre_wake,
                    sched_lag_ns: post_wake,
                    is_timer: timer,
                    holder,
                });
            }
            None => idle_ns += pre_wake,
        }
    }

    waits.sort_by(|a, b| b.wait_ns.cmp(&a.wait_ns));
    waits.truncate(top_n);

    Ok(LatencyReport {
        task: ident,
        from_ts: w0,
        to_ts: w1,
        total_ns,
        own_poll_ns,
        poll_count,
        resource_wait_ns,
        timer_wait_ns,
        sched_lag_ns,
        idle_ns,
        waits,
    })
}

/// Choose a task worth explaining when the caller didn't name one: the task
/// that spent the longest total time parked (timers included — a task
/// sleeping its life away is still the slow one), else the longest-lived.
pub(crate) fn pick_slow_task(conn: &Connection, at: u64) -> Result<Option<TaskId>, CoreError> {
    let mut best: Option<(u64, TaskId)> = None;
    let mut totals: std::collections::HashMap<TaskId, u64> = std::collections::HashMap::new();
    for p in query::park_episodes(conn, at)? {
        *totals.entry(p.task.id).or_default() += p.dur_ns;
    }
    for (task, total) in totals {
        if best.map_or(true, |(b, bt)| total > b || (total == b && task < bt)) {
            best = Some((total, task));
        }
    }
    if let Some((_, task)) = best {
        return Ok(Some(task));
    }
    // No parks anywhere: fall back to the longest-lived task.
    let row: Option<i64> = conn
        .query_row(
            "SELECT id FROM tasks WHERE spawn_ts <= ?1
             ORDER BY COALESCE(MIN(end_ts, ?1), ?1) - spawn_ts DESC, id LIMIT 1",
            params![at as i64],
            |r| r.get(0),
        )
        .optional()?;
    Ok(row.map(|id| id as u64))
}
