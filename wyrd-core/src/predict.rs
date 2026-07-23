//! `predict`: potential-deadlock detection from runs that did **not** deadlock.
//!
//! Every other wyrd query is forensic — it explains something that already
//! went wrong in the recording. `predict` is proactive: it reconstructs, for
//! every task, which resources were *held while acquiring* others, builds the
//! lock-order graph over those observations, and reports cycles. A cycle
//! means the recorded schedules acquired the same locks in conflicting orders
//! — a deadlock waiting for the right interleaving, even if this particular
//! run sailed through.
//!
//! This is the classic lock-order-inversion ("Goodlock") analysis, adapted to
//! the wyrd event vocabulary, with the two standard false-positive filters:
//!
//! - **single-task cycles** are suppressed: one task alternating A→B and B→A
//!   over time cannot deadlock with itself (each edge needs a distinct task
//!   to be parked on it simultaneously);
//! - **gate-locked cycles** are suppressed: if every witness of every edge
//!   was made while holding one common *other* lock, that gate serializes
//!   the conflicting orders and the cycle cannot manifest.
//!
//! Cycles whose resource set actually deadlocked in this recording are marked
//! `observed` — `predict` then doubles as a confirmation of `why_blocked`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use rusqlite::{params, Connection};
use wyrd_weave::{ResourceId, TaskId, FIELD_ACQUIRED_BY};

use crate::error::CoreError;
use crate::model::*;
use crate::query;

/// Resources a task intentionally waits on; they never participate in
/// hold-and-wait edges.
fn is_timer(concrete_type: &str) -> bool {
    matches!(concrete_type, "Sleep" | "Interval" | "Timeout")
}

/// A closed interval during which `holder` held a resource.
#[derive(Debug, Clone)]
struct Hold {
    resource: ResourceId,
    holder: TaskId,
    start: u64,
    end: u64,
}

/// One observation of "task attempted to take `resource` at `ts`".
#[derive(Debug, Clone)]
struct Attempt {
    task: TaskId,
    resource: ResourceId,
    ts: u64,
    op_name: String,
}

/// One witness of a lock-order edge: `task` held `held` (among others) while
/// attempting the edge's target at `ts`.
#[derive(Debug, Clone)]
struct Witness {
    task: TaskId,
    ts: u64,
    op_name: String,
    /// Everything the task held at `ts` (the gate-lock check needs the full
    /// set, not just the edge's source).
    held: BTreeSet<ResourceId>,
}

/// Reconstruct hold intervals from the `acquired_by` / `locked` state stream.
///
/// An `acquired_by` opens a hold (closing any previous one on the same
/// resource — with several concurrent readers on an `RwLock` the holder is
/// approximate, matching wyrd's single-holder model everywhere else); a
/// `locked = 0` closes it. Holds still open are clipped at the resource's
/// drop, the holder's end, or `at`, whichever comes first.
fn hold_intervals(
    conn: &Connection,
    at: u64,
    task_end: &HashMap<TaskId, u64>,
    resource_drop: &HashMap<ResourceId, u64>,
) -> Result<(Vec<Hold>, Vec<Attempt>), CoreError> {
    let mut stmt = conn.prepare(
        "SELECT resource, field, value, ts FROM resource_state
         WHERE ts <= ?1 AND field IN (?2, 'locked') ORDER BY ts, id",
    )?;
    let rows = stmt.query_map(params![at as i64, FIELD_ACQUIRED_BY], |r| {
        Ok((
            r.get::<_, i64>(0)? as u64,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)? as u64,
        ))
    })?;

    let mut holds: Vec<Hold> = Vec::new();
    let mut open: HashMap<ResourceId, (TaskId, u64)> = HashMap::new();
    let mut acquires: Vec<Attempt> = Vec::new();

    for row in rows {
        let (resource, field, value, ts) = row?;
        if field == FIELD_ACQUIRED_BY {
            let task = value as u64;
            if let Some((holder, start)) = open.remove(&resource) {
                holds.push(Hold {
                    resource,
                    holder,
                    start,
                    end: ts,
                });
            }
            open.insert(resource, (task, ts));
            acquires.push(Attempt {
                task,
                resource,
                ts,
                op_name: "acquire".into(),
            });
        } else if value == 0 {
            // locked = 0: released.
            if let Some((holder, start)) = open.remove(&resource) {
                holds.push(Hold {
                    resource,
                    holder,
                    start,
                    end: ts,
                });
            }
        }
    }

    // Clip holds still open at `at`.
    for (resource, (holder, start)) in open {
        let mut end = at;
        if let Some(&d) = resource_drop.get(&resource) {
            end = end.min(d);
        }
        if let Some(&e) = task_end.get(&holder) {
            end = end.min(e);
        }
        holds.push(Hold {
            resource,
            holder,
            start,
            end: end.max(start),
        });
    }

    Ok((holds, acquires))
}

/// Resource sets of deadlock cycles that actually formed in this recording.
fn observed_deadlock_sets(
    conn: &Connection,
    at: u64,
) -> Result<Vec<BTreeSet<ResourceId>>, CoreError> {
    let world = query::world_state(conn, at)?;
    let mut seen_cycles: HashSet<Vec<TaskId>> = HashSet::new();
    let mut sets = Vec::new();
    for t in &world.tasks {
        if !matches!(t.status, TaskStatus::Parked { .. }) {
            continue;
        }
        let report = query::why_blocked(conn, t.ident.id, at)?;
        let BlockedOutcome::Deadlock { cycle } = &report.outcome else {
            continue;
        };
        let mut key = cycle.clone();
        key.sort_unstable();
        if !seen_cycles.insert(key) {
            continue;
        }
        sets.push(
            report
                .chain
                .iter()
                .filter(|l| cycle.contains(&l.task.id))
                .map(|l| l.waiting_on.id)
                .collect(),
        );
    }
    Ok(sets)
}

/// Search for one witness per edge such that the witnessing tasks are pairwise
/// distinct and no *gate lock* (a resource outside the cycle held by every
/// chosen witness) serializes them. Returns the chosen witnesses, or a flag
/// describing why no combination works.
enum WitnessSelection<'a> {
    Valid(Vec<&'a Witness>),
    /// Distinct-task combinations exist but all share a gate lock.
    Guarded,
    /// Every combination reuses a task — a one-task inversion can't deadlock.
    SingleTask,
}

fn select_witnesses<'a>(
    edge_witnesses: &[&'a [Witness]],
    cycle: &BTreeSet<ResourceId>,
) -> WitnessSelection<'a> {
    // Cap the combination search; witness lists are already deduped per task.
    const MAX_COMBOS: usize = 4096;

    fn rec<'a>(
        edge_witnesses: &[&'a [Witness]],
        cycle: &BTreeSet<ResourceId>,
        idx: usize,
        chosen: &mut Vec<&'a Witness>,
        tasks: &mut Vec<TaskId>,
        combos: &mut usize,
        saw_distinct: &mut bool,
    ) -> Option<Vec<&'a Witness>> {
        if *combos >= MAX_COMBOS {
            return None;
        }
        if idx == edge_witnesses.len() {
            *combos += 1;
            *saw_distinct = true;
            // Gate check: a lock outside the cycle held by *every* witness
            // serializes the conflicting acquisition orders.
            let mut gate: BTreeSet<ResourceId> = chosen[0].held.clone();
            for w in &chosen[1..] {
                gate = gate.intersection(&w.held).copied().collect();
            }
            let gated = gate.iter().any(|g| !cycle.contains(g));
            if gated {
                return None;
            }
            return Some(chosen.clone());
        }
        for w in edge_witnesses[idx] {
            if tasks.contains(&w.task) {
                continue;
            }
            chosen.push(w);
            tasks.push(w.task);
            if let Some(found) = rec(
                edge_witnesses,
                cycle,
                idx + 1,
                chosen,
                tasks,
                combos,
                saw_distinct,
            ) {
                return Some(found);
            }
            chosen.pop();
            tasks.pop();
        }
        None
    }

    let mut chosen = Vec::new();
    let mut tasks = Vec::new();
    let mut combos = 0usize;
    let mut saw_distinct = false;
    match rec(
        edge_witnesses,
        cycle,
        0,
        &mut chosen,
        &mut tasks,
        &mut combos,
        &mut saw_distinct,
    ) {
        Some(ws) => WitnessSelection::Valid(ws),
        None if saw_distinct => WitnessSelection::Guarded,
        None => WitnessSelection::SingleTask,
    }
}

pub(crate) fn predict(
    conn: &Connection,
    at: u64,
    cfg: &PredictConfig,
) -> Result<PredictReport, CoreError> {
    // --- load the world ------------------------------------------------------
    let mut task_end: HashMap<TaskId, u64> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, end_ts FROM tasks WHERE end_ts IS NOT NULL")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (id, end) = row?;
            task_end.insert(id, end as u64);
        }
    }
    let mut resource_drop: HashMap<ResourceId, u64> = HashMap::new();
    let mut timer_resources: HashSet<ResourceId> = HashSet::new();
    {
        let mut stmt = conn.prepare("SELECT id, concrete_type, drop_ts FROM resources")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })?;
        for row in rows {
            let (id, ty, drop_ts) = row?;
            if let Some(d) = drop_ts {
                resource_drop.insert(id, d as u64);
            }
            if is_timer(&ty) {
                timer_resources.insert(id);
            }
        }
    }

    let (holds, acquires) = hold_intervals(conn, at, &task_end, &resource_drop)?;

    // Resources with holder tracking: only these can be the *source* of a
    // hold-and-wait edge.
    let tracked: HashSet<ResourceId> = holds.iter().map(|h| h.resource).collect();

    // --- collect attempts: successful acquires plus contended parks ----------
    let mut attempts = acquires;
    {
        let mut stmt =
            conn.prepare("SELECT task, resource, op_name, ts FROM parks WHERE ts <= ?1")?;
        let rows = stmt.query_map(params![at as i64], |r| {
            Ok(Attempt {
                task: r.get::<_, i64>(0)? as u64,
                resource: r.get::<_, i64>(1)? as u64,
                ts: r.get::<_, i64>(3)? as u64,
                op_name: r.get::<_, String>(2)?,
            })
        })?;
        for row in rows {
            let a = row?;
            if !timer_resources.contains(&a.resource) {
                attempts.push(a);
            }
        }
    }
    let acquisition_count = attempts.len() as u64;

    // --- build the lock-order graph ------------------------------------------
    // edge (held → acquiring) with at most one (earliest) witness per task.
    let mut edges: BTreeMap<(ResourceId, ResourceId), Vec<Witness>> = BTreeMap::new();
    for a in &attempts {
        let held: BTreeSet<ResourceId> = holds
            .iter()
            .filter(|h| {
                h.holder == a.task && h.start < a.ts && h.end > a.ts && h.resource != a.resource
            })
            .map(|h| h.resource)
            .collect();
        for &from in &held {
            let witness = Witness {
                task: a.task,
                ts: a.ts,
                op_name: a.op_name.clone(),
                held: held.clone(),
            };
            let list = edges.entry((from, a.resource)).or_default();
            match list.iter_mut().find(|w| w.task == a.task) {
                Some(existing) => {
                    if witness.ts < existing.ts {
                        *existing = witness;
                    }
                }
                None => list.push(witness),
            }
        }
    }

    // --- enumerate simple cycles (canonical: smallest resource id first) -----
    let mut adjacency: BTreeMap<ResourceId, Vec<ResourceId>> = BTreeMap::new();
    for &(from, to) in edges.keys() {
        // Only holder-tracked resources can close a cycle.
        if tracked.contains(&from) && tracked.contains(&to) {
            adjacency.entry(from).or_default().push(to);
        }
    }

    let mut raw_cycles: Vec<Vec<ResourceId>> = Vec::new();
    let nodes: Vec<ResourceId> = adjacency.keys().copied().collect();
    for &start in &nodes {
        let mut path = vec![start];
        dfs_cycles(
            &adjacency,
            start,
            start,
            cfg.max_cycle_len.max(2),
            &mut path,
            &mut raw_cycles,
            cfg.max_cycles.saturating_mul(8).max(64),
        );
    }

    // --- validate cycles against witnesses -----------------------------------
    let mut cycles: Vec<PredictedCycle> = Vec::new();
    let mut guarded_suppressed = 0u64;
    let mut single_task_suppressed = 0u64;
    let observed_sets = observed_deadlock_sets(conn, at)?;

    for cycle in &raw_cycles {
        let cycle_set: BTreeSet<ResourceId> = cycle.iter().copied().collect();
        let edge_witnesses: Vec<&[Witness]> = (0..cycle.len())
            .map(|i| {
                let from = cycle[i];
                let to = cycle[(i + 1) % cycle.len()];
                edges.get(&(from, to)).map(|v| v.as_slice()).unwrap_or(&[])
            })
            .collect();
        match select_witnesses(&edge_witnesses, &cycle_set) {
            WitnessSelection::Valid(ws) => {
                let mut edge_views = Vec::with_capacity(cycle.len());
                for (i, w) in ws.iter().enumerate() {
                    let from = cycle[i];
                    let to = cycle[(i + 1) % cycle.len()];
                    edge_views.push(LockOrderEdge {
                        held: query::resource_ident(conn, from)?,
                        acquiring: query::resource_ident(conn, to)?,
                        task: query::task_ident(conn, w.task)?,
                        at: w.ts,
                        op_name: w.op_name.clone(),
                    });
                }
                let mut resources = Vec::with_capacity(cycle.len());
                for &r in cycle {
                    resources.push(query::resource_ident(conn, r)?);
                }
                let observed = observed_sets.contains(&cycle_set);
                cycles.push(PredictedCycle {
                    resources,
                    edges: edge_views,
                    observed,
                });
            }
            WitnessSelection::Guarded => guarded_suppressed += 1,
            WitnessSelection::SingleTask => single_task_suppressed += 1,
        }
    }

    // Observed first, then shortest cycle, then earliest first witness.
    cycles.sort_by(|a, b| {
        (
            std::cmp::Reverse(a.observed),
            a.resources.len(),
            first_ts(a),
        )
            .cmp(&(
                std::cmp::Reverse(b.observed),
                b.resources.len(),
                first_ts(b),
            ))
    });
    cycles.truncate(cfg.max_cycles);

    Ok(PredictReport {
        at,
        config: cfg.clone(),
        acquisitions: acquisition_count,
        lock_count: tracked.len() as u64,
        order_edges: edges.len() as u64,
        cycles,
        guarded_suppressed,
        single_task_suppressed,
    })
}

fn first_ts(c: &PredictedCycle) -> u64 {
    c.edges.iter().map(|e| e.at).min().unwrap_or(0)
}

/// Depth-first enumeration of simple cycles through `start`, visiting only
/// nodes `>= start` so each cycle is found exactly once (rooted at its
/// smallest resource id).
fn dfs_cycles(
    adjacency: &BTreeMap<ResourceId, Vec<ResourceId>>,
    start: ResourceId,
    current: ResourceId,
    max_len: usize,
    path: &mut Vec<ResourceId>,
    out: &mut Vec<Vec<ResourceId>>,
    cap: usize,
) {
    if out.len() >= cap {
        return;
    }
    let Some(nexts) = adjacency.get(&current) else {
        return;
    };
    for &next in nexts {
        if next == start && path.len() >= 2 {
            out.push(path.clone());
            if out.len() >= cap {
                return;
            }
            continue;
        }
        if next <= start || path.contains(&next) || path.len() >= max_len {
            continue;
        }
        path.push(next);
        dfs_cycles(adjacency, start, next, max_len, path, out, cap);
        path.pop();
    }
}
