//! `predict` (lock-order-inversion) tests driven by synthetic recordings.

use wyrd_core::model::PredictConfig;
use wyrd_core::Recording;
use wyrd_weave::{Event, Loc, Record, StateOp, TaskKind, FIELD_ACQUIRED_BY};

fn loc(line: u32) -> Loc {
    Loc {
        file: Some("src/main.rs".into()),
        line: Some(line),
        col: None,
    }
}

fn recording(events: Vec<(u64, Event)>) -> Recording {
    let records = events.into_iter().map(|(ts, event)| Record { ts, event });
    Recording::from_records(records).expect("ingest synthetic recording")
}

fn spawn(id: u64, name: &str) -> Event {
    Event::TaskSpawn {
        id,
        parent: None,
        name: Some(name.into()),
        loc: loc(1),
        kind: TaskKind::Task,
    }
}

fn mutex(id: u64, line: u32) -> Event {
    Event::ResourceNew {
        id,
        parent: None,
        concrete_type: "Mutex".into(),
        loc: loc(line),
        is_internal: false,
    }
}

fn acquired_by(id: u64, task: u64) -> Event {
    Event::ResourceState {
        id,
        field: FIELD_ACQUIRED_BY.into(),
        value: task as i64,
        op: StateOp::Override,
    }
}

fn released(id: u64) -> Event {
    Event::ResourceState {
        id,
        field: "locked".into(),
        value: 0,
        op: StateOp::Override,
    }
}

fn park(task: u64, resource: u64) -> Event {
    Event::Park {
        task,
        resource,
        op_name: "lock".into(),
    }
}

fn end(id: u64) -> Event {
    Event::TaskEnd { id }
}

/// Task 1 takes A then B; task 2 takes B then A — but *staggered in time*, so
/// the run completes cleanly. This is the flagship case: no deadlock happened,
/// yet one is latent.
#[test]
fn clean_abba_inversion_is_predicted() {
    let rec = recording(vec![
        (1, mutex(100, 10)),
        (2, mutex(200, 20)),
        (3, spawn(1, "t1")),
        (4, spawn(2, "t2")),
        // t1: A → B, releases both.
        (10, acquired_by(100, 1)),
        (11, acquired_by(200, 1)),
        (12, released(200)),
        (13, released(100)),
        (14, end(1)),
        // t2 (later, no overlap): B → A, releases both.
        (20, acquired_by(200, 2)),
        (21, acquired_by(100, 2)),
        (22, released(100)),
        (23, released(200)),
        (24, end(2)),
    ]);
    let report = rec.predict(None, &PredictConfig::default()).unwrap();
    assert_eq!(report.cycles.len(), 1, "got: {:#?}", report);
    let cycle = &report.cycles[0];
    assert!(!cycle.observed, "no deadlock actually formed");
    assert_eq!(cycle.resources.len(), 2);
    assert_eq!(cycle.edges.len(), 2);
    // Each hop witnessed by a different task.
    assert_ne!(cycle.edges[0].task.id, cycle.edges[1].task.id);
    assert!(report.has_potential_deadlock());
    assert!(!report.has_observed_deadlock());
}

/// Both tasks always take a common gate lock G first: the inversion on A/B
/// can never manifest, and must be suppressed.
#[test]
fn gate_locked_inversion_is_suppressed() {
    let rec = recording(vec![
        (1, mutex(50, 5)), // gate
        (2, mutex(100, 10)),
        (3, mutex(200, 20)),
        (4, spawn(1, "t1")),
        (5, spawn(2, "t2")),
        // t1: G, A, B.
        (10, acquired_by(50, 1)),
        (11, acquired_by(100, 1)),
        (12, acquired_by(200, 1)),
        (13, released(200)),
        (14, released(100)),
        (15, released(50)),
        (16, end(1)),
        // t2: G, B, A.
        (20, acquired_by(50, 2)),
        (21, acquired_by(200, 2)),
        (22, acquired_by(100, 2)),
        (23, released(100)),
        (24, released(200)),
        (25, released(50)),
        (26, end(2)),
    ]);
    let report = rec.predict(None, &PredictConfig::default()).unwrap();
    assert!(
        report.cycles.is_empty(),
        "gate-locked cycle must be suppressed: {:#?}",
        report.cycles
    );
    assert!(report.guarded_suppressed >= 1, "got: {:#?}", report);
}

/// A single task alternating A→B and B→A cannot deadlock with itself.
#[test]
fn single_task_inversion_is_suppressed() {
    let rec = recording(vec![
        (1, mutex(100, 10)),
        (2, mutex(200, 20)),
        (3, spawn(1, "t1")),
        // A → B.
        (10, acquired_by(100, 1)),
        (11, acquired_by(200, 1)),
        (12, released(200)),
        (13, released(100)),
        // B → A.
        (20, acquired_by(200, 1)),
        (21, acquired_by(100, 1)),
        (22, released(100)),
        (23, released(200)),
        (24, end(1)),
    ]);
    let report = rec.predict(None, &PredictConfig::default()).unwrap();
    assert!(report.cycles.is_empty(), "got: {:#?}", report.cycles);
    assert!(report.single_task_suppressed >= 1, "got: {:#?}", report);
}

/// An actual recorded deadlock is found via the park edges and flagged
/// `observed`.
#[test]
fn observed_deadlock_is_marked() {
    let rec = recording(vec![
        (1, mutex(100, 10)),
        (2, mutex(200, 20)),
        (3, spawn(1, "t1")),
        (4, spawn(2, "t2")),
        (10, acquired_by(100, 1)),
        (11, acquired_by(200, 2)),
        // Both now park on the other's lock, forever.
        (12, Event::PollStart { task: 1 }),
        (13, park(1, 200)),
        (14, Event::PollEnd { task: 1 }),
        (15, Event::PollStart { task: 2 }),
        (16, park(2, 100)),
        (17, Event::PollEnd { task: 2 }),
    ]);
    let report = rec.predict(None, &PredictConfig::default()).unwrap();
    assert_eq!(report.cycles.len(), 1, "got: {:#?}", report.cycles);
    assert!(report.cycles[0].observed);
    assert!(report.has_observed_deadlock());
}

/// Consistent ordering (both tasks A → B) has edges but no cycle.
#[test]
fn consistent_order_is_clean() {
    let rec = recording(vec![
        (1, mutex(100, 10)),
        (2, mutex(200, 20)),
        (3, spawn(1, "t1")),
        (4, spawn(2, "t2")),
        (10, acquired_by(100, 1)),
        (11, acquired_by(200, 1)),
        (12, released(200)),
        (13, released(100)),
        (14, end(1)),
        (20, acquired_by(100, 2)),
        (21, acquired_by(200, 2)),
        (22, released(200)),
        (23, released(100)),
        (24, end(2)),
    ]);
    let report = rec.predict(None, &PredictConfig::default()).unwrap();
    assert!(report.cycles.is_empty(), "got: {:#?}", report.cycles);
    assert_eq!(report.guarded_suppressed, 0);
    assert_eq!(report.single_task_suppressed, 0);
    assert!(report.order_edges >= 1);
}

/// Three tasks, three locks, a 3-cycle: t1 holds A taking B, t2 holds B
/// taking C, t3 holds C taking A — staggered so nothing actually deadlocks.
#[test]
fn three_cycle_is_predicted() {
    let rec = recording(vec![
        (1, mutex(100, 10)),
        (2, mutex(200, 20)),
        (3, mutex(300, 30)),
        (4, spawn(1, "t1")),
        (5, spawn(2, "t2")),
        (6, spawn(3, "t3")),
        // t1: A → B.
        (10, acquired_by(100, 1)),
        (11, acquired_by(200, 1)),
        (12, released(200)),
        (13, released(100)),
        (14, end(1)),
        // t2: B → C.
        (20, acquired_by(200, 2)),
        (21, acquired_by(300, 2)),
        (22, released(300)),
        (23, released(200)),
        (24, end(2)),
        // t3: C → A.
        (30, acquired_by(300, 3)),
        (31, acquired_by(100, 3)),
        (32, released(100)),
        (33, released(300)),
        (34, end(3)),
    ]);
    let report = rec.predict(None, &PredictConfig::default()).unwrap();
    assert_eq!(report.cycles.len(), 1, "got: {:#?}", report.cycles);
    assert_eq!(report.cycles[0].resources.len(), 3);
    assert!(!report.cycles[0].observed);
}

/// Timers never participate: holding a lock across a sleep is a latency bug
/// (why-slow's beat), not a lock-order inversion.
#[test]
fn timer_waits_do_not_form_edges() {
    let rec = recording(vec![
        (1, mutex(100, 10)),
        (
            2,
            Event::ResourceNew {
                id: 900,
                parent: None,
                concrete_type: "Sleep".into(),
                loc: loc(90),
                is_internal: false,
            },
        ),
        (3, spawn(1, "t1")),
        (10, acquired_by(100, 1)),
        (11, park(1, 900)), // sleep while holding the mutex
        (12, released(100)),
        (13, end(1)),
    ]);
    let report = rec.predict(None, &PredictConfig::default()).unwrap();
    assert!(report.cycles.is_empty());
    assert_eq!(report.order_edges, 0, "got: {:#?}", report);
}
