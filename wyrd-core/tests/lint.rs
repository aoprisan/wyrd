//! Lint tests driven by synthetic recordings.

use wyrd_core::model::{LintConfig, LintKind, LintSeverity};
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

fn resource(id: u64, ty: &str, line: u32) -> Event {
    Event::ResourceNew {
        id,
        parent: None,
        concrete_type: ty.into(),
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

fn park(task: u64, resource: u64) -> Event {
    Event::Park {
        task,
        resource,
        op_name: "poll_acquire".into(),
    }
}

/// Tight thresholds so nanosecond-scale synthetic recordings trip them.
fn cfg(poll: u64, park: u64) -> LintConfig {
    LintConfig {
        long_poll_ns: poll,
        long_park_ns: park,
    }
}

#[test]
fn clean_recording_has_no_findings() {
    let rec = recording(vec![
        (1, spawn(1, "ok")),
        (2, Event::PollStart { task: 1 }),
        (3, Event::PollEnd { task: 1 }),
        (4, Event::TaskEnd { id: 1 }),
    ]);
    let report = rec.lint(None, &LintConfig::default()).unwrap();
    assert!(report.is_clean(), "got: {:#?}", report.findings);
    assert!(!report.has_errors());
}

#[test]
fn deadlock_is_an_error_and_reported_once() {
    let rec = recording(vec![
        (1, resource(100, "Mutex", 10)),
        (2, resource(200, "Mutex", 20)),
        (3, spawn(1, "t1")),
        (4, spawn(2, "t2")),
        (5, Event::PollStart { task: 1 }),
        (6, acquired_by(100, 1)),
        (7, Event::PollEnd { task: 1 }),
        (8, Event::PollStart { task: 2 }),
        (9, acquired_by(200, 2)),
        (10, Event::PollEnd { task: 2 }),
        (11, Event::PollStart { task: 1 }),
        (12, park(1, 200)),
        (13, Event::PollEnd { task: 1 }),
        (14, Event::PollStart { task: 2 }),
        (15, park(2, 100)),
        (16, Event::PollEnd { task: 2 }),
    ]);
    // Park thresholds high enough that only the deadlock is reported.
    let report = rec.lint(None, &cfg(1_000_000, 1_000_000)).unwrap();
    assert!(report.has_errors());

    let deadlocks: Vec<_> = report
        .findings
        .iter()
        .filter(|f| matches!(f.kind, LintKind::Deadlock { .. }))
        .collect();
    // One cycle, discovered from either entry point, reported exactly once.
    assert_eq!(deadlocks.len(), 1, "got: {:#?}", report.findings);
    assert_eq!(deadlocks[0].severity, LintSeverity::Error);
    let LintKind::Deadlock { cycle, resources } = &deadlocks[0].kind else {
        unreachable!()
    };
    assert_eq!(cycle.len(), 2);
    assert_eq!(resources.len(), 2);

    // The deadlocked tasks' parks are subsumed by the deadlock finding.
    assert!(
        !report
            .findings
            .iter()
            .any(|f| matches!(f.kind, LintKind::LongPark { .. })),
        "deadlocked tasks should not double-report as long parks: {:#?}",
        report.findings
    );
    // Errors sort first.
    assert_eq!(report.findings[0].severity, LintSeverity::Error);
}

#[test]
fn long_poll_flags_blocking_in_async() {
    let rec = recording(vec![
        (1, spawn(1, "cruncher")),
        // A 100ns poll and a 5ns poll against a 10ns threshold.
        (10, Event::PollStart { task: 1 }),
        (110, Event::PollEnd { task: 1 }),
        (120, Event::PollStart { task: 1 }),
        (125, Event::PollEnd { task: 1 }),
    ]);
    let report = rec.lint(None, &cfg(10, u64::MAX)).unwrap();
    let polls: Vec<_> = report
        .findings
        .iter()
        .filter_map(|f| match &f.kind {
            LintKind::LongPoll {
                task,
                count,
                max_ns,
                ..
            } => Some((task.label(), *count, *max_ns)),
            _ => None,
        })
        .collect();
    assert_eq!(polls, vec![("cruncher".to_string(), 1, 100)]);
    assert!(!report.has_errors(), "long polls are warnings");
}

#[test]
fn blocking_tasks_are_exempt_from_long_poll() {
    // A `spawn_blocking` task blocks inside its single poll by design.
    let rec = recording(vec![
        (
            1,
            Event::TaskSpawn {
                id: 1,
                parent: None,
                name: Some("blocker".into()),
                loc: loc(1),
                kind: TaskKind::Blocking,
            },
        ),
        (10, Event::PollStart { task: 1 }),
        (10_000, Event::PollEnd { task: 1 }),
    ]);
    let report = rec.lint(None, &cfg(10, u64::MAX)).unwrap();
    assert!(report.is_clean(), "got: {:#?}", report.findings);
}

#[test]
fn poll_still_open_at_query_time_counts_as_long() {
    // The task entered poll at t=5 and never came back: stuck inside poll
    // (e.g. a blocking std mutex). Query at t=1000.
    let rec = recording(vec![
        (1, spawn(1, "blocked-inside-poll")),
        (5, Event::PollStart { task: 1 }),
        (
            1000,
            Event::TaskSpawn {
                id: 2,
                parent: None,
                name: Some("marker".into()),
                loc: loc(2),
                kind: TaskKind::Task,
            },
        ),
    ]);
    let report = rec.lint(None, &cfg(100, u64::MAX)).unwrap();
    let found = report.findings.iter().any(|f| {
        matches!(&f.kind, LintKind::LongPoll { task, max_ns, .. }
            if task.name.as_deref() == Some("blocked-inside-poll") && *max_ns == 995)
    });
    assert!(found, "got: {:#?}", report.findings);
}

#[test]
fn long_park_flags_non_timer_waits_only() {
    let rec = recording(vec![
        (1, resource(100, "Mutex", 10)),
        (2, resource(300, "Sleep", 30)),
        (3, spawn(1, "waiter")),
        (4, spawn(2, "sleeper")),
        // waiter parks on the mutex at t=10 and never wakes (until end t=1000).
        (5, Event::PollStart { task: 1 }),
        (10, park(1, 100)),
        (11, Event::PollEnd { task: 1 }),
        // sleeper parks on a Sleep just as long — intentional, not a finding.
        (5, Event::PollStart { task: 2 }),
        (
            10,
            Event::Park {
                task: 2,
                resource: 300,
                op_name: "poll_elapsed".into(),
            },
        ),
        (11, Event::PollEnd { task: 2 }),
        (
            1000,
            Event::TaskSpawn {
                id: 3,
                parent: None,
                name: Some("marker".into()),
                loc: loc(2),
                kind: TaskKind::Task,
            },
        ),
    ]);
    let report = rec.lint(None, &cfg(u64::MAX, 100)).unwrap();
    let parks: Vec<_> = report
        .findings
        .iter()
        .filter_map(|f| match &f.kind {
            LintKind::LongPark {
                task,
                resource,
                count,
                max_ns,
                ..
            } => Some((
                task.label(),
                resource.concrete_type.clone(),
                *count,
                *max_ns,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        parks,
        vec![("waiter".to_string(), "Mutex".to_string(), 1, 990)],
        "only the mutex park is a finding; the Sleep is intentional"
    );
}

#[test]
fn saturated_channel_is_flagged() {
    let rec = recording(vec![
        (1, spawn(1, "producer")),
        (2, resource(400, "mpsc::channel", 40)),
        // capacity 2; two sends drain the permits to 0 → peak depth 2/2.
        (
            3,
            Event::ResourceState {
                id: 400,
                field: "permits".into(),
                value: 2,
                op: StateOp::Override,
            },
        ),
        (
            4,
            Event::ResourceState {
                id: 400,
                field: "permits".into(),
                value: 1,
                op: StateOp::Sub,
            },
        ),
        (
            5,
            Event::ResourceState {
                id: 400,
                field: "permits".into(),
                value: 1,
                op: StateOp::Sub,
            },
        ),
    ]);
    let report = rec.lint(None, &cfg(u64::MAX, u64::MAX)).unwrap();
    let sat: Vec<_> = report
        .findings
        .iter()
        .filter_map(|f| match &f.kind {
            LintKind::SaturatedChannel {
                capacity,
                max_depth,
                ..
            } => Some((*capacity, *max_depth)),
            _ => None,
        })
        .collect();
    assert_eq!(sat, vec![(2, 2)]);
}

#[test]
fn at_limits_what_lint_sees() {
    // Same long-park recording, but linted before the park happened.
    let rec = recording(vec![
        (1, resource(100, "Mutex", 10)),
        (2, spawn(1, "waiter")),
        (5, Event::PollStart { task: 1 }),
        (10, park(1, 100)),
        (11, Event::PollEnd { task: 1 }),
        (
            1000,
            Event::TaskSpawn {
                id: 3,
                parent: None,
                name: Some("marker".into()),
                loc: loc(2),
                kind: TaskKind::Task,
            },
        ),
    ]);
    let early = rec.lint(Some(8), &cfg(u64::MAX, 100)).unwrap();
    assert!(early.is_clean(), "got: {:#?}", early.findings);
    let late = rec.lint(None, &cfg(u64::MAX, 100)).unwrap();
    assert!(!late.is_clean());
}
