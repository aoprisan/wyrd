//! `lint`: scan a recording for async anti-patterns.
//!
//! Reuses the same folds as `stats`/`why_blocked` and distills them into
//! actionable findings: deadlocks (errors), blocking-in-async long polls,
//! suspiciously long parks, and saturated channels (warnings). Timer waits
//! (`Sleep`/`Interval`/`Timeout`) are intentional and never flagged as parks.

use std::collections::{BTreeMap, HashSet};

use rusqlite::{params, Connection};
use wyrd_weave::{TaskId, TaskKind};

use crate::error::CoreError;
use crate::model::*;
use crate::query;

/// Resources a task is *supposed* to wait on for a long time.
fn is_timer(concrete_type: &str) -> bool {
    matches!(concrete_type, "Sleep" | "Interval" | "Timeout")
}

pub(crate) fn lint(conn: &Connection, at: u64, cfg: &LintConfig) -> Result<LintReport, CoreError> {
    let mut findings: Vec<LintFinding> = Vec::new();

    // --- deadlocks (errors) -------------------------------------------------
    // Walk why_blocked from every parked task; dedup cycles by their id set.
    let world = query::world_state(conn, at)?;
    let mut seen_cycles: HashSet<Vec<TaskId>> = HashSet::new();
    let mut deadlocked: HashSet<TaskId> = HashSet::new();
    for t in &world.tasks {
        if !matches!(t.status, TaskStatus::Parked { .. }) {
            continue;
        }
        let report = query::why_blocked(conn, t.ident.id, at)?;
        let BlockedOutcome::Deadlock { cycle } = &report.outcome else {
            continue;
        };
        deadlocked.extend(cycle.iter().copied());
        let mut key = cycle.clone();
        key.sort_unstable();
        if !seen_cycles.insert(key) {
            continue;
        }
        let mut idents = Vec::with_capacity(cycle.len());
        for id in cycle {
            idents.push(query::task_ident(conn, *id)?);
        }
        let resources = report
            .chain
            .iter()
            .filter(|l| cycle.contains(&l.task.id))
            .map(|l| l.waiting_on.clone())
            .collect();
        findings.push(LintFinding {
            severity: LintSeverity::Error,
            kind: LintKind::Deadlock {
                cycle: idents,
                resources,
            },
        });
    }

    // --- long polls (blocking-in-async) --------------------------------------
    // Per task: polls longer than the threshold, counting a poll still open at
    // `at` as lasting until `at` (a task stuck *inside* poll is the worst case).
    let mut long_polls: BTreeMap<TaskId, (u64, u64)> = BTreeMap::new(); // task -> (count, max)
    {
        let mut stmt = conn.prepare(
            "SELECT task, end_ts - start_ts FROM polls
             WHERE end_ts IS NOT NULL AND end_ts <= ?1 AND end_ts - start_ts > ?2",
        )?;
        let rows = stmt.query_map(params![at as i64, cfg.long_poll_ns as i64], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
        })?;
        for row in rows {
            let (task, dur) = row?;
            let e = long_polls.entry(task).or_insert((0, 0));
            e.0 += 1;
            e.1 = e.1.max(dur);
        }
        // Polls that span `at` (or never ended): count them as running until `at`.
        let mut stmt = conn.prepare(
            "SELECT task, start_ts FROM polls
             WHERE start_ts <= ?1 AND (end_ts IS NULL OR end_ts > ?1)",
        )?;
        let rows = stmt.query_map(params![at as i64], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
        })?;
        for row in rows {
            let (task, start) = row?;
            let dur = at.saturating_sub(start);
            if dur > cfg.long_poll_ns {
                let e = long_polls.entry(task).or_insert((0, 0));
                e.0 += 1;
                e.1 = e.1.max(dur);
            }
        }
    }
    for (task, (count, max_ns)) in long_polls {
        let ident = query::task_ident(conn, task)?;
        // `spawn_blocking` tasks (and the blocking pool's workers) are
        // *supposed* to block inside their one poll — not a finding.
        if ident.kind == TaskKind::Blocking {
            continue;
        }
        findings.push(LintFinding {
            severity: LintSeverity::Warning,
            kind: LintKind::LongPoll {
                task: ident,
                count,
                max_ns,
                threshold_ns: cfg.long_poll_ns,
            },
        });
    }

    // --- long parks -----------------------------------------------------------
    // Grouped by (task, resource, op); timers and already-reported deadlocked
    // tasks are skipped.
    let mut long_parks: BTreeMap<(TaskId, u64, String), (ParkStat, u64)> = BTreeMap::new();
    for p in query::park_episodes(conn, at)? {
        if p.dur_ns <= cfg.long_park_ns
            || is_timer(&p.resource.concrete_type)
            || deadlocked.contains(&p.task.id)
        {
            continue;
        }
        let key = (p.task.id, p.resource.id, p.op_name.clone());
        match long_parks.get_mut(&key) {
            Some((worst, count)) => {
                *count += 1;
                if p.dur_ns > worst.dur_ns {
                    *worst = p;
                }
            }
            None => {
                long_parks.insert(key, (p, 1));
            }
        }
    }
    for (_, (worst, count)) in long_parks {
        findings.push(LintFinding {
            severity: LintSeverity::Warning,
            kind: LintKind::LongPark {
                task: worst.task,
                resource: worst.resource,
                op_name: worst.op_name,
                count,
                max_ns: worst.dur_ns,
                threshold_ns: cfg.long_park_ns,
            },
        });
    }

    // --- saturated channels ----------------------------------------------------
    for c in query::channel_depths(conn)? {
        if c.max_depth >= c.capacity {
            findings.push(LintFinding {
                severity: LintSeverity::Warning,
                kind: LintKind::SaturatedChannel {
                    resource: c.resource,
                    capacity: c.capacity,
                    max_depth: c.max_depth,
                },
            });
        }
    }

    // Errors first, then by magnitude (worst offender first).
    findings.sort_by(|a, b| {
        (a.severity, std::cmp::Reverse(magnitude(a)))
            .cmp(&(b.severity, std::cmp::Reverse(magnitude(b))))
    });

    Ok(LintReport {
        at,
        config: cfg.clone(),
        findings,
    })
}

/// A rough "how bad is it" for ordering findings of equal severity.
fn magnitude(f: &LintFinding) -> u64 {
    match &f.kind {
        LintKind::Deadlock { .. } => u64::MAX,
        LintKind::LongPoll { max_ns, .. } | LintKind::LongPark { max_ns, .. } => *max_ns,
        LintKind::SaturatedChannel { max_depth, .. } => *max_depth as u64,
    }
}
