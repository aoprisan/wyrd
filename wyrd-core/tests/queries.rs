//! Fold/query tests driven by synthetic recordings — deterministic and free of
//! any tokio dependency.

use wyrd_core::model::{BlockedOutcome, TaskStatus};
use wyrd_core::Recording;
use wyrd_weave::{Event, Loc, Record, StateOp, TaskKind, FIELD_ACQUIRED_BY};

fn loc(line: u32) -> Loc {
    Loc {
        file: Some("src/main.rs".into()),
        line: Some(line),
        col: None,
    }
}

/// Build a recording from `(ts, event)` pairs.
fn recording(events: Vec<(u64, Event)>) -> Recording {
    let records = events.into_iter().map(|(ts, event)| Record { ts, event });
    Recording::from_records(records).expect("ingest synthetic recording")
}

fn acquired_by(id: u64, task: u64) -> Event {
    Event::ResourceState {
        id,
        field: FIELD_ACQUIRED_BY.into(),
        value: task as i64,
        op: StateOp::Override,
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

fn spawn(id: u64, name: &str) -> Event {
    Event::TaskSpawn {
        id,
        parent: None,
        name: Some(name.into()),
        loc: loc(1),
        kind: TaskKind::Task,
    }
}

/// A → B / B → A deadlock over two mutexes.
fn deadlock_recording() -> Recording {
    recording(vec![
        (1, mutex(100, 10)), // Mutex A
        (2, mutex(200, 20)), // Mutex B
        (3, spawn(1, "t1")),
        (4, spawn(2, "t2")),
        // t1 acquires A
        (5, Event::PollStart { task: 1 }),
        (6, acquired_by(100, 1)),
        (7, Event::PollEnd { task: 1 }),
        // t2 acquires B
        (8, Event::PollStart { task: 2 }),
        (9, acquired_by(200, 2)),
        (10, Event::PollEnd { task: 2 }),
        // t1 parks on B (held by t2)
        (11, Event::PollStart { task: 1 }),
        (
            12,
            Event::Park {
                task: 1,
                resource: 200,
                op_name: "poll_acquire".into(),
            },
        ),
        (13, Event::PollEnd { task: 1 }),
        // t2 parks on A (held by t1)
        (14, Event::PollStart { task: 2 }),
        (
            15,
            Event::Park {
                task: 2,
                resource: 100,
                op_name: "poll_acquire".into(),
            },
        ),
        (16, Event::PollEnd { task: 2 }),
    ])
}

#[test]
fn detects_two_task_deadlock() {
    let rec = deadlock_recording();
    let t1 = rec.resolve_task("t1").unwrap();
    let report = rec.why_blocked(t1, None).unwrap();

    match &report.outcome {
        BlockedOutcome::Deadlock { cycle } => {
            assert_eq!(cycle.len(), 2);
            assert!(cycle.contains(&1) && cycle.contains(&2));
        }
        other => panic!("expected deadlock, got {other:?}"),
    }

    assert_eq!(report.chain.len(), 2);
    // First hop: t1 waiting on Mutex B (line 20), held by t2.
    assert_eq!(report.chain[0].waiting_on.id, 200);
    assert_eq!(report.chain[0].waiting_on.loc.line, Some(20));
    assert_eq!(report.chain[0].holder.as_ref().unwrap().id, 2);
    // Second hop: t2 waiting on Mutex A (line 10), held by t1.
    assert_eq!(report.chain[1].waiting_on.id, 100);
    assert_eq!(report.chain[1].holder.as_ref().unwrap().id, 1);
}

#[test]
fn world_state_reports_parked_and_holders() {
    let rec = deadlock_recording();
    let world = rec.world_state(None).unwrap();

    let t1 = world.tasks.iter().find(|t| t.ident.id == 1).unwrap();
    let t2 = world.tasks.iter().find(|t| t.ident.id == 2).unwrap();
    assert_eq!(t1.status, TaskStatus::Parked { resource: 200 });
    assert_eq!(t2.status, TaskStatus::Parked { resource: 100 });

    let a = world.resources.iter().find(|r| r.ident.id == 100).unwrap();
    let b = world.resources.iter().find(|r| r.ident.id == 200).unwrap();
    assert_eq!(a.holder, Some(1));
    assert_eq!(b.holder, Some(2));
}

#[test]
fn world_state_respects_query_time() {
    let rec = deadlock_recording();
    // Before t1 parks (ts=12): t1 holds A, not yet parked.
    let world = rec.world_state(Some(10)).unwrap();
    let t1 = world.tasks.iter().find(|t| t.ident.id == 1).unwrap();
    assert!(
        !matches!(t1.status, TaskStatus::Parked { .. }),
        "t1 should not be parked yet at t=10: {:?}",
        t1.status
    );
}

#[test]
fn not_blocked_when_idle() {
    let rec = recording(vec![
        (1, spawn(1, "solo")),
        (2, Event::PollStart { task: 1 }),
        (3, Event::PollEnd { task: 1 }),
    ]);
    let t = rec.resolve_task("solo").unwrap();
    let report = rec.why_blocked(t, None).unwrap();
    assert!(matches!(report.outcome, BlockedOutcome::NotBlocked));
    assert!(report.chain.is_empty());
}

#[test]
fn resource_root_when_no_holder() {
    // A task parked on a channel-like resource with no acquirer.
    let rec = recording(vec![
        (1, spawn(1, "waiter")),
        (
            2,
            Event::ResourceNew {
                id: 500,
                parent: None,
                concrete_type: "Semaphore".into(),
                loc: loc(30),
                is_internal: true,
            },
        ),
        (3, Event::PollStart { task: 1 }),
        (
            4,
            Event::Park {
                task: 1,
                resource: 500,
                op_name: "poll_acquire".into(),
            },
        ),
        (5, Event::PollEnd { task: 1 }),
    ]);
    let t = rec.resolve_task("waiter").unwrap();
    let report = rec.why_blocked(t, None).unwrap();
    assert!(
        matches!(
            report.outcome,
            BlockedOutcome::ResourceRoot { resource: 500 }
        ),
        "got {:?}",
        report.outcome
    );
    assert_eq!(report.chain.len(), 1);
}

#[test]
fn released_mutex_clears_holder() {
    let rec = recording(vec![
        (1, mutex(100, 10)),
        (2, spawn(1, "t1")),
        (3, Event::PollStart { task: 1 }),
        (4, acquired_by(100, 1)),
        (
            5,
            Event::ResourceState {
                id: 100,
                field: "locked".into(),
                value: 1,
                op: StateOp::Override,
            },
        ),
        // release
        (
            6,
            Event::ResourceState {
                id: 100,
                field: "locked".into(),
                value: 0,
                op: StateOp::Override,
            },
        ),
        (7, Event::PollEnd { task: 1 }),
    ]);
    let world = rec.world_state(None).unwrap();
    let m = world.resources.iter().find(|r| r.ident.id == 100).unwrap();
    assert_eq!(m.holder, None, "holder should clear on locked=0");
    assert_eq!(m.locked, Some(false));
}

#[test]
fn stats_counts_and_channel_depth() {
    let rec = recording(vec![
        (1, spawn(1, "t1")),
        (
            2,
            Event::ResourceNew {
                id: 300,
                parent: None,
                concrete_type: "Semaphore".into(),
                loc: loc(40),
                is_internal: true,
            },
        ),
        // capacity 2, then two acquisitions (permits 2 -> 0) => depth 2
        (
            3,
            Event::ResourceState {
                id: 300,
                field: "permits".into(),
                value: 2,
                op: StateOp::Override,
            },
        ),
        (
            4,
            Event::ResourceState {
                id: 300,
                field: "permits".into(),
                value: 1,
                op: StateOp::Sub,
            },
        ),
        (
            5,
            Event::ResourceState {
                id: 300,
                field: "permits".into(),
                value: 1,
                op: StateOp::Sub,
            },
        ),
        (6, Event::PollStart { task: 1 }),
        (16, Event::PollEnd { task: 1 }),
    ]);
    let stats = rec.stats(10).unwrap();
    assert_eq!(stats.task_count, 1);
    assert_eq!(stats.resource_count, 1);
    assert_eq!(stats.poll_time.count, 1);
    assert_eq!(stats.poll_time.max, 10);

    assert_eq!(stats.channel_depths.len(), 1);
    assert_eq!(stats.channel_depths[0].capacity, 2);
    assert_eq!(stats.channel_depths[0].max_depth, 2);
}
