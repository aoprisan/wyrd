//! `diff`: behavioral regression detection between two recordings.
//!
//! Aligns tasks and resources across runs by *stable identity* (a task's
//! name, else `kind@file:line`; a resource's `Type@file:line`) — span ids are
//! meaningless across processes — then compares per-group behavior and emits
//! triaged verdicts: a new deadlock is an error, poll/wait growth beyond the
//! thresholds is a regression warning, shrinkage is an improvement. Built to
//! gate CI: record a baseline on main, diff against it on every PR.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection};

use crate::error::CoreError;
use crate::model::*;
use crate::query;
use crate::Recording;

fn is_timer(concrete_type: &str) -> bool {
    matches!(concrete_type, "Sleep" | "Interval" | "Timeout")
}

fn kind_str(kind: wyrd_weave::TaskKind) -> &'static str {
    match kind {
        wyrd_weave::TaskKind::Task => "task",
        wyrd_weave::TaskKind::Blocking => "blocking",
        wyrd_weave::TaskKind::BlockOn => "block_on",
        wyrd_weave::TaskKind::Other => "task",
    }
}

/// Stable cross-run identity of a task: its name, else `kind@loc`, else a
/// kind-wide bucket (unnamed, unlocated tasks are indistinguishable).
fn task_key(ident: &TaskIdent) -> String {
    if let Some(name) = &ident.name {
        return name.clone();
    }
    if !ident.loc.is_empty() {
        return format!("{}@{}", kind_str(ident.kind), ident.loc);
    }
    format!("<unnamed {}>", kind_str(ident.kind))
}

/// Stable cross-run identity of a resource: `Type@loc`, else the type alone.
fn resource_key(ident: &ResourceIdent) -> String {
    if !ident.loc.is_empty() {
        format!("{}@{}", ident.concrete_type, ident.loc)
    } else {
        ident.concrete_type.clone()
    }
}

/// Everything the diff needs from one run.
struct RunProfile {
    summary: RunSummary,
    tasks: BTreeMap<String, TaskGroupStats>,
    resources: BTreeMap<String, ResourceGroupStats>,
    /// Deadlock cycles as sorted sets of task keys.
    deadlocks: BTreeSet<Vec<String>>,
}

fn profile(conn: &Connection) -> Result<RunProfile, CoreError> {
    let end = query::max_ts(conn)?;
    let start = query::min_ts(conn)?;

    // --- per-task poll totals (completed polls; open ones clipped at end) ---
    let mut task_polls: BTreeMap<u64, (u64, u64)> = BTreeMap::new(); // id -> (total, max)
    {
        let mut stmt = conn.prepare(
            "SELECT task, COALESCE(MIN(end_ts, ?1), ?1) - start_ts FROM polls
             WHERE start_ts <= ?1",
        )?;
        let rows = stmt.query_map(params![end as i64], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)?.max(0) as u64,
            ))
        })?;
        for row in rows {
            let (task, dur) = row?;
            let e = task_polls.entry(task).or_default();
            e.0 += dur;
            e.1 = e.1.max(dur);
        }
    }

    // --- per-task and per-resource wait totals from park episodes ----------
    // A task can re-park on every poll attempt, so raw episodes overlap;
    // merge each task's intervals before summing or totals exceed wall time.
    let mut task_intervals: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
    let mut res_intervals: BTreeMap<u64, BTreeMap<u64, Vec<(u64, u64)>>> = BTreeMap::new();
    for p in query::park_episodes(conn, end)? {
        if is_timer(&p.resource.concrete_type) {
            continue;
        }
        let iv = (p.since_ts, p.since_ts + p.dur_ns);
        task_intervals.entry(p.task.id).or_default().push(iv);
        res_intervals
            .entry(p.resource.id)
            .or_default()
            .entry(p.task.id)
            .or_default()
            .push(iv);
    }
    /// Merge overlapping intervals; return (total covered, longest merged run).
    fn merged(mut iv: Vec<(u64, u64)>) -> (u64, u64) {
        iv.sort_unstable();
        let (mut total, mut max, mut cur) = (0u64, 0u64, None::<(u64, u64)>);
        for (s, e) in iv {
            match &mut cur {
                Some((_, ce)) if s <= *ce => *ce = (*ce).max(e),
                _ => {
                    if let Some((cs, ce)) = cur {
                        total += ce - cs;
                        max = max.max(ce - cs);
                    }
                    cur = Some((s, e));
                }
            }
        }
        if let Some((cs, ce)) = cur {
            total += ce - cs;
            max = max.max(ce - cs);
        }
        (total, max)
    }
    let task_waits: BTreeMap<u64, (u64, u64)> = task_intervals
        .into_iter()
        .map(|(id, iv)| (id, merged(iv)))
        .collect();
    let res_waits: BTreeMap<u64, (u64, BTreeSet<u64>)> = res_intervals
        .into_iter()
        .map(|(id, by_task)| {
            let waiters: BTreeSet<u64> = by_task.keys().copied().collect();
            let total = by_task.into_values().map(|iv| merged(iv).0).sum();
            (id, (total, waiters))
        })
        .collect();

    // --- fold tasks into groups --------------------------------------------
    let mut tasks: BTreeMap<String, TaskGroupStats> = BTreeMap::new();
    let mut task_ids: Vec<i64> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM tasks")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        for row in rows {
            task_ids.push(row?);
        }
    }
    let task_count = task_ids.len() as u64;
    let mut total_poll_ns = 0u64;
    let mut total_wait_ns = 0u64;
    for id in task_ids {
        let id = id as u64;
        let ident = query::task_ident(conn, id)?;
        let key = task_key(&ident);
        let (poll_total, poll_max) = task_polls.get(&id).copied().unwrap_or((0, 0));
        let (wait_total, wait_max) = task_waits.get(&id).copied().unwrap_or((0, 0));
        total_poll_ns += poll_total;
        total_wait_ns += wait_total;
        let g = tasks.entry(key).or_insert(TaskGroupStats {
            instances: 0,
            total_poll_ns: 0,
            max_poll_ns: 0,
            total_wait_ns: 0,
            max_wait_ns: 0,
        });
        g.instances += 1;
        g.total_poll_ns += poll_total;
        g.max_poll_ns = g.max_poll_ns.max(poll_max);
        g.total_wait_ns += wait_total;
        g.max_wait_ns = g.max_wait_ns.max(wait_max);
    }

    // --- fold resources into groups (skip timers and internals) ------------
    let mut resources: BTreeMap<String, ResourceGroupStats> = BTreeMap::new();
    let mut res_ids: Vec<i64> = Vec::new();
    let mut resource_count = 0u64;
    {
        let mut stmt = conn.prepare("SELECT id FROM resources")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        for row in rows {
            res_ids.push(row?);
            resource_count += 1;
        }
    }
    let depths: BTreeMap<u64, (i64, i64)> = query::channel_depths(conn)?
        .into_iter()
        .map(|c| (c.resource.id, (c.capacity, c.max_depth)))
        .collect();
    for id in res_ids {
        let id = id as u64;
        let ident = query::resource_ident(conn, id)?;
        if is_timer(&ident.concrete_type) {
            continue;
        }
        let key = resource_key(&ident);
        let (wait_total, waiters) = res_waits
            .get(&id)
            .map(|(t, w)| (*t, w.len() as u64))
            .unwrap_or((0, 0));
        let depth = depths.get(&id).copied();
        let g = resources.entry(key).or_insert(ResourceGroupStats {
            instances: 0,
            total_wait_ns: 0,
            waiters: 0,
            capacity: None,
            max_depth: None,
        });
        g.instances += 1;
        g.total_wait_ns += wait_total;
        g.waiters += waiters;
        if let Some((cap, dep)) = depth {
            g.capacity = Some(g.capacity.map_or(cap, |c: i64| c.max(cap)));
            g.max_depth = Some(g.max_depth.map_or(dep, |d: i64| d.max(dep)));
        }
    }

    // --- deadlocks, as cross-run-comparable key sets ------------------------
    let lint = crate::lint::lint(conn, end, &LintConfig::default())?;
    let mut deadlocks: BTreeSet<Vec<String>> = BTreeSet::new();
    for f in &lint.findings {
        if let LintKind::Deadlock { cycle, .. } = &f.kind {
            let mut keys: Vec<String> = cycle.iter().map(task_key).collect();
            keys.sort();
            deadlocks.insert(keys);
        }
    }

    Ok(RunProfile {
        summary: RunSummary {
            duration_ns: end.saturating_sub(start),
            task_count,
            resource_count,
            total_poll_ns,
            total_wait_ns,
            deadlocks: deadlocks.len() as u64,
        },
        tasks,
        resources,
        deadlocks,
    })
}

/// Did `current` grow past `baseline` by both thresholds?
fn regressed(baseline: u64, current: u64, cfg: &DiffConfig) -> bool {
    current.saturating_sub(baseline) > cfg.abs_floor_ns
        && (current as f64) > (baseline as f64) * cfg.ratio
}

/// Diff two ingested recordings: `baseline` (the known-good run) against
/// `current`. See [`DiffReport`] for what comes back.
pub fn diff(
    baseline: &Recording,
    current: &Recording,
    config: &DiffConfig,
) -> Result<DiffReport, CoreError> {
    let base = profile(baseline.conn())?;
    let cur = profile(current.conn())?;

    let mut findings: Vec<DiffFinding> = Vec::new();

    // --- deadlocks ----------------------------------------------------------
    for cycle in cur.deadlocks.difference(&base.deadlocks) {
        findings.push(DiffFinding {
            severity: DiffSeverity::Error,
            kind: DiffKind::NewDeadlock {
                cycle: cycle.clone(),
            },
        });
    }
    for cycle in base.deadlocks.difference(&cur.deadlocks) {
        findings.push(DiffFinding {
            severity: DiffSeverity::Info,
            kind: DiffKind::FixedDeadlock {
                cycle: cycle.clone(),
            },
        });
    }

    // --- task groups ----------------------------------------------------------
    let task_keys: BTreeSet<&String> = base.tasks.keys().chain(cur.tasks.keys()).collect();
    let mut task_groups: Vec<TaskGroupDiff> = Vec::new();
    for key in task_keys {
        let b = base.tasks.get(key);
        let c = cur.tasks.get(key);
        match (b, c) {
            (Some(b), Some(c)) => {
                if regressed(b.mean_poll_ns(), c.mean_poll_ns(), config) {
                    findings.push(DiffFinding {
                        severity: DiffSeverity::Warning,
                        kind: DiffKind::PollRegression {
                            key: key.clone(),
                            baseline_ns: b.mean_poll_ns(),
                            current_ns: c.mean_poll_ns(),
                        },
                    });
                } else if regressed(c.mean_poll_ns(), b.mean_poll_ns(), config) {
                    findings.push(DiffFinding {
                        severity: DiffSeverity::Info,
                        kind: DiffKind::PollImprovement {
                            key: key.clone(),
                            baseline_ns: b.mean_poll_ns(),
                            current_ns: c.mean_poll_ns(),
                        },
                    });
                }
                if regressed(b.mean_wait_ns(), c.mean_wait_ns(), config) {
                    findings.push(DiffFinding {
                        severity: DiffSeverity::Warning,
                        kind: DiffKind::WaitRegression {
                            key: key.clone(),
                            baseline_ns: b.mean_wait_ns(),
                            current_ns: c.mean_wait_ns(),
                        },
                    });
                } else if regressed(c.mean_wait_ns(), b.mean_wait_ns(), config) {
                    findings.push(DiffFinding {
                        severity: DiffSeverity::Info,
                        kind: DiffKind::WaitImprovement {
                            key: key.clone(),
                            baseline_ns: b.mean_wait_ns(),
                            current_ns: c.mean_wait_ns(),
                        },
                    });
                }
            }
            (None, Some(c)) => {
                if c.total_poll_ns + c.total_wait_ns > config.abs_floor_ns {
                    findings.push(DiffFinding {
                        severity: DiffSeverity::Info,
                        kind: DiffKind::NewTaskGroup { key: key.clone() },
                    });
                }
            }
            (Some(b), None) => {
                if b.total_poll_ns + b.total_wait_ns > config.abs_floor_ns {
                    findings.push(DiffFinding {
                        severity: DiffSeverity::Info,
                        kind: DiffKind::RemovedTaskGroup { key: key.clone() },
                    });
                }
            }
            (None, None) => unreachable!(),
        }
        task_groups.push(TaskGroupDiff {
            key: key.clone(),
            baseline: b.cloned(),
            current: c.cloned(),
        });
    }

    // --- resource groups -------------------------------------------------------
    let res_keys: BTreeSet<&String> = base.resources.keys().chain(cur.resources.keys()).collect();
    let mut resource_groups: Vec<ResourceGroupDiff> = Vec::new();
    for key in res_keys {
        let b = base.resources.get(key);
        let c = cur.resources.get(key);
        let saturated = |s: &ResourceGroupStats| matches!((s.capacity, s.max_depth), (Some(c), Some(d)) if d >= c);
        if let Some(c) = c {
            if saturated(c) && !b.is_some_and(saturated) {
                findings.push(DiffFinding {
                    severity: DiffSeverity::Warning,
                    kind: DiffKind::NewSaturation {
                        key: key.clone(),
                        capacity: c.capacity.unwrap_or(0),
                        max_depth: c.max_depth.unwrap_or(0),
                    },
                });
            }
        }
        resource_groups.push(ResourceGroupDiff {
            key: key.clone(),
            baseline: b.cloned(),
            current: c.cloned(),
        });
    }

    // Biggest behavioral change first.
    let task_delta = |d: &TaskGroupDiff| {
        let side = |s: &Option<TaskGroupStats>| {
            s.as_ref()
                .map_or(0i128, |s| (s.total_poll_ns + s.total_wait_ns) as i128)
        };
        (side(&d.current) - side(&d.baseline)).unsigned_abs()
    };
    task_groups.sort_by(|a, b| task_delta(b).cmp(&task_delta(a)).then(a.key.cmp(&b.key)));
    let res_delta = |d: &ResourceGroupDiff| {
        let side =
            |s: &Option<ResourceGroupStats>| s.as_ref().map_or(0i128, |s| s.total_wait_ns as i128);
        (side(&d.current) - side(&d.baseline)).unsigned_abs()
    };
    resource_groups.sort_by(|a, b| res_delta(b).cmp(&res_delta(a)).then(a.key.cmp(&b.key)));

    // Errors, then warnings, then info; stable within a class.
    findings.sort_by_key(|f| f.severity);

    Ok(DiffReport {
        config: config.clone(),
        baseline: base.summary,
        current: cur.summary,
        task_groups,
        resource_groups,
        findings,
    })
}
