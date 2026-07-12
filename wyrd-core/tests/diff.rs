//! `diff` regression-detection tests driven by synthetic recording pairs.

use wyrd_core::model::{DiffConfig, DiffKind, DiffSeverity};
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

/// A run where task "worker" spends `poll_ns` in one poll.
fn run_with_poll(poll_ns: u64) -> Recording {
    recording(vec![
        (0, spawn(1, "worker")),
        (10, Event::PollStart { task: 1 }),
        (10 + poll_ns, Event::PollEnd { task: 1 }),
        (10 + poll_ns, Event::TaskEnd { id: 1 }),
    ])
}

/// Tight thresholds so nanosecond-scale synthetic recordings trip them.
fn cfg() -> DiffConfig {
    DiffConfig {
        ratio: 1.5,
        abs_floor_ns: 100,
    }
}

#[test]
fn identical_runs_are_clean() {
    let a = run_with_poll(1000);
    let b = run_with_poll(1000);
    let report = wyrd_core::diff(&a, &b, &cfg()).unwrap();
    assert!(report.findings.is_empty(), "got: {:#?}", report.findings);
    assert!(!report.has_errors());
    assert!(!report.has_regressions());
}

#[test]
fn poll_growth_is_a_regression_and_shrinkage_an_improvement() {
    let base = run_with_poll(1000);
    let cur = run_with_poll(5000);
    let report = wyrd_core::diff(&base, &cur, &cfg()).unwrap();
    assert!(report.has_regressions());
    assert!(matches!(
        &report.findings[0].kind,
        DiffKind::PollRegression { key, baseline_ns: 1000, current_ns: 5000 } if key == "worker"
    ));

    // The reverse direction is an improvement, not a regression.
    let report = wyrd_core::diff(&cur, &base, &cfg()).unwrap();
    assert!(!report.has_regressions());
    assert!(matches!(
        &report.findings[0].kind,
        DiffKind::PollImprovement { .. }
    ));
}

#[test]
fn growth_below_the_floor_is_ignored() {
    let base = run_with_poll(10);
    let cur = run_with_poll(60); // ×6, but only +50ns < floor of 100ns
    let report = wyrd_core::diff(&base, &cur, &cfg()).unwrap();
    assert!(report.findings.is_empty(), "got: {:#?}", report.findings);
}

#[test]
fn tasks_align_by_name_not_span_id() {
    // Same logical task, wildly different span ids across runs.
    let base = recording(vec![
        (0, spawn(101, "worker")),
        (10, Event::PollStart { task: 101 }),
        (1010, Event::PollEnd { task: 101 }),
        (1010, Event::TaskEnd { id: 101 }),
    ]);
    let cur = recording(vec![
        (0, spawn(999_777, "worker")),
        (10, Event::PollStart { task: 999_777 }),
        (1010, Event::PollEnd { task: 999_777 }),
        (1010, Event::TaskEnd { id: 999_777 }),
    ]);
    let report = wyrd_core::diff(&base, &cur, &cfg()).unwrap();
    assert!(report.findings.is_empty(), "got: {:#?}", report.findings);
    assert_eq!(report.task_groups.len(), 1);
    assert_eq!(report.task_groups[0].key, "worker");
    assert!(report.task_groups[0].baseline.is_some());
    assert!(report.task_groups[0].current.is_some());
}

#[test]
fn instance_counts_normalize_totals() {
    // Baseline: one worker polling 1000ns. Current: four workers, each
    // polling 1000ns — total ×4 but per-instance flat. Not a regression.
    let base = run_with_poll(1000);
    let mut events = Vec::new();
    for i in 1..=4u64 {
        events.push((0, spawn(i, "worker")));
        events.push((10, Event::PollStart { task: i }));
        events.push((1010, Event::PollEnd { task: i }));
        events.push((1010, Event::TaskEnd { id: i }));
    }
    let cur = recording(events);
    let report = wyrd_core::diff(&base, &cur, &cfg()).unwrap();
    assert!(report.findings.is_empty(), "got: {:#?}", report.findings);
    assert_eq!(report.current.task_count, 4);
}

#[test]
fn new_deadlock_is_an_error_and_fixing_it_an_info() {
    let clean = run_with_poll(1000);
    let deadlocked = recording(vec![
        (0, resource(10, "Mutex", 170)),
        (0, resource(11, "Mutex", 171)),
        (0, spawn(1, "ab")),
        (0, spawn(2, "ba")),
        (10, Event::PollStart { task: 1 }),
        (11, acquired_by(10, 1)),
        (12, Event::PollEnd { task: 1 }),
        (13, Event::PollStart { task: 2 }),
        (14, acquired_by(11, 2)),
        (15, Event::PollEnd { task: 2 }),
        (20, Event::PollStart { task: 1 }),
        (21, park(1, 11)),
        (22, Event::PollEnd { task: 1 }),
        (23, Event::PollStart { task: 2 }),
        (24, park(2, 10)),
        (25, Event::PollEnd { task: 2 }),
        (
            1000,
            Event::Wake {
                woken: 99,
                by: None,
            },
        ),
    ]);

    let report = wyrd_core::diff(&clean, &deadlocked, &cfg()).unwrap();
    assert!(report.has_errors());
    assert_eq!(report.findings[0].severity, DiffSeverity::Error);
    assert!(matches!(
        &report.findings[0].kind,
        DiffKind::NewDeadlock { cycle } if cycle.contains(&"ab".to_string())
    ));
    assert_eq!(report.current.deadlocks, 1);

    // Fixing it is good news, not an error.
    let report = wyrd_core::diff(&deadlocked, &clean, &cfg()).unwrap();
    assert!(!report.has_errors());
    assert!(report
        .findings
        .iter()
        .any(|f| matches!(&f.kind, DiffKind::FixedDeadlock { .. })));
}

#[test]
fn same_deadlock_in_both_runs_is_not_new() {
    let deadlocked = || {
        recording(vec![
            (0, resource(10, "Mutex", 170)),
            (0, resource(11, "Mutex", 171)),
            (0, spawn(1, "ab")),
            (0, spawn(2, "ba")),
            (10, Event::PollStart { task: 1 }),
            (11, acquired_by(10, 1)),
            (12, Event::PollEnd { task: 1 }),
            (13, Event::PollStart { task: 2 }),
            (14, acquired_by(11, 2)),
            (15, Event::PollEnd { task: 2 }),
            (20, Event::PollStart { task: 1 }),
            (21, park(1, 11)),
            (22, Event::PollEnd { task: 1 }),
            (23, Event::PollStart { task: 2 }),
            (24, park(2, 10)),
            (25, Event::PollEnd { task: 2 }),
            (
                1000,
                Event::Wake {
                    woken: 99,
                    by: None,
                },
            ),
        ])
    };
    let report = wyrd_core::diff(&deadlocked(), &deadlocked(), &cfg()).unwrap();
    assert!(!report.has_errors(), "got: {:#?}", report.findings);
}

#[test]
fn new_saturation_is_a_regression() {
    let channel = |depth_events: Vec<(u64, Event)>| {
        let mut events = vec![
            (0, resource(20, "Semaphore", 88)),
            (
                1,
                Event::ResourceState {
                    id: 20,
                    field: "permits".into(),
                    value: 2,
                    op: StateOp::Override,
                },
            ),
            (0, spawn(1, "producer")),
            (10, Event::PollStart { task: 1 }),
            (20, Event::PollEnd { task: 1 }),
        ];
        events.extend(depth_events);
        events.push((5000, Event::TaskEnd { id: 1 }));
        recording(events)
    };
    let sub = |ts: u64| {
        (
            ts,
            Event::ResourceState {
                id: 20,
                field: "permits".into(),
                value: 1,
                op: StateOp::Sub,
            },
        )
    };
    let base = channel(vec![sub(100)]); // depth 1/2
    let cur = channel(vec![sub(100), sub(200)]); // depth 2/2: saturated
    let report = wyrd_core::diff(&base, &cur, &cfg()).unwrap();
    assert!(report.has_regressions());
    assert!(matches!(
        &report.findings[0].kind,
        DiffKind::NewSaturation {
            capacity: 2,
            max_depth: 2,
            ..
        }
    ));
}

#[test]
fn wait_growth_on_a_mutex_is_a_regression() {
    let run = |wait_ns: u64| {
        recording(vec![
            (0, resource(10, "Mutex", 14)),
            (0, spawn(1, "handler")),
            (10, Event::PollStart { task: 1 }),
            (20, park(1, 10)),
            (30, Event::PollEnd { task: 1 }),
            (30 + wait_ns, Event::PollStart { task: 1 }),
            (40 + wait_ns, Event::PollEnd { task: 1 }),
            (40 + wait_ns, Event::TaskEnd { id: 1 }),
        ])
    };
    let report = wyrd_core::diff(&run(100), &run(10_000), &cfg()).unwrap();
    assert!(report.has_regressions());
    assert!(report.findings.iter().any(|f| matches!(
        &f.kind,
        DiffKind::WaitRegression { key, .. } if key == "handler"
    )));
    // The mutex resource group shows the wait on both sides.
    let rg = report
        .resource_groups
        .iter()
        .find(|g| g.key.starts_with("Mutex"))
        .expect("mutex group");
    assert!(
        rg.current.as_ref().unwrap().total_wait_ns > rg.baseline.as_ref().unwrap().total_wait_ns
    );
}

#[test]
fn appearing_and_disappearing_groups_are_noted() {
    let base = run_with_poll(1000);
    let cur = recording(vec![
        (0, spawn(1, "worker")),
        (10, Event::PollStart { task: 1 }),
        (1010, Event::PollEnd { task: 1 }),
        (1010, Event::TaskEnd { id: 1 }),
        (0, spawn(2, "newcomer")),
        (10, Event::PollStart { task: 2 }),
        (2010, Event::PollEnd { task: 2 }),
        (2010, Event::TaskEnd { id: 2 }),
    ]);
    let report = wyrd_core::diff(&base, &cur, &cfg()).unwrap();
    assert!(report.findings.iter().any(|f| matches!(
        &f.kind,
        DiffKind::NewTaskGroup { key } if key == "newcomer"
    )));
    let report = wyrd_core::diff(&cur, &base, &cfg()).unwrap();
    assert!(report.findings.iter().any(|f| matches!(
        &f.kind,
        DiffKind::RemovedTaskGroup { key } if key == "newcomer"
    )));
}
