//! `why_slow` latency-attribution tests driven by synthetic recordings.

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

/// The buckets must partition the task's lifetime exactly.
fn assert_partitions(r: &wyrd_core::model::LatencyReport) {
    assert_eq!(
        r.own_poll_ns + r.resource_wait_ns + r.timer_wait_ns + r.sched_lag_ns + r.idle_ns,
        r.total_ns,
        "buckets must sum to total: {r:#?}"
    );
}

#[test]
fn pure_compute_task_is_all_poll_time() {
    let rec = recording(vec![
        (0, spawn(1, "worker")),
        (10, Event::PollStart { task: 1 }),
        (110, Event::PollEnd { task: 1 }),
        (110, Event::TaskEnd { id: 1 }),
    ]);
    let r = rec.why_slow(1, None, 5).unwrap();
    assert_eq!(r.total_ns, 110);
    assert_eq!(r.own_poll_ns, 100);
    assert_eq!(r.poll_count, 1);
    assert_eq!(r.idle_ns, 10); // spawn → first poll, no wake recorded
    assert_eq!(r.resource_wait_ns, 0);
    assert_partitions(&r);
}

#[test]
fn mutex_wait_is_attributed_to_the_holder() {
    // holder (task 1) grabs the mutex and computes 900ns inside a poll while
    // waiter (task 2) parks on it; a wake arrives, then a final short poll.
    let rec = recording(vec![
        (0, resource(10, "Mutex", 14)),
        (0, spawn(1, "holder")),
        (0, spawn(2, "waiter")),
        (10, Event::PollStart { task: 1 }),
        (20, acquired_by(10, 1)),
        (30, Event::PollStart { task: 2 }),
        (40, park(2, 10)),
        (50, Event::PollEnd { task: 2 }),
        // holder computes until 910, then releases and wakes the waiter
        (910, Event::PollEnd { task: 1 }),
        (
            920,
            Event::ResourceState {
                id: 10,
                field: "locked".into(),
                value: 0,
                op: StateOp::Override,
            },
        ),
        (
            920,
            Event::Wake {
                woken: 2,
                by: Some(1),
            },
        ),
        (940, Event::PollStart { task: 2 }),
        (950, Event::PollEnd { task: 2 }),
        (950, Event::TaskEnd { id: 2 }),
    ]);
    let r = rec.why_slow(2, None, 5).unwrap();
    assert_eq!(r.total_ns, 950);
    // parked 50 → woken 920, polled again 940.
    assert_eq!(r.resource_wait_ns, 870);
    assert_eq!(r.sched_lag_ns, 20);
    assert_partitions(&r);

    let w = &r.waits[0];
    assert_eq!(w.resource.concrete_type, "Mutex");
    assert_eq!(w.wait_ns, 870);
    assert_eq!(w.sched_lag_ns, 20);
    assert!(!w.is_timer);
    let holder = w.holder.as_ref().expect("wait must be blamed on holder");
    assert_eq!(holder.task.name.as_deref(), Some("holder"));
    // The holder was inside its own poll for the whole wait window (50..910
    // of the 50..920 wait): it was computing, not waiting.
    assert!(holder.polling_ns >= 850, "got {}", holder.polling_ns);
}

#[test]
fn timer_wait_is_not_a_resource_wait() {
    let rec = recording(vec![
        (0, spawn(1, "sleeper")),
        (0, resource(20, "Sleep", 33)),
        (10, Event::PollStart { task: 1 }),
        (
            20,
            Event::Park {
                task: 1,
                resource: 20,
                op_name: "poll_elapsed".into(),
            },
        ),
        (30, Event::PollEnd { task: 1 }),
        (1030, Event::Wake { woken: 1, by: None }),
        (1040, Event::PollStart { task: 1 }),
        (1050, Event::PollEnd { task: 1 }),
        (1050, Event::TaskEnd { id: 1 }),
    ]);
    let r = rec.why_slow(1, None, 5).unwrap();
    assert_eq!(r.timer_wait_ns, 1000);
    assert_eq!(r.resource_wait_ns, 0);
    assert_eq!(r.sched_lag_ns, 10);
    assert_partitions(&r);
    assert!(r.waits[0].is_timer);
    assert!(
        r.waits[0].holder.is_none(),
        "timers have no holder to blame"
    );
}

#[test]
fn wait_without_wake_runs_to_next_poll() {
    // No Wake event recorded: the whole gap counts as resource wait, none as
    // scheduler lag.
    let rec = recording(vec![
        (0, resource(10, "Mutex", 14)),
        (0, spawn(1, "waiter")),
        (10, Event::PollStart { task: 1 }),
        (20, park(1, 10)),
        (30, Event::PollEnd { task: 1 }),
        (530, Event::PollStart { task: 1 }),
        (540, Event::PollEnd { task: 1 }),
        (540, Event::TaskEnd { id: 1 }),
    ]);
    let r = rec.why_slow(1, None, 5).unwrap();
    assert_eq!(r.resource_wait_ns, 500);
    assert_eq!(r.sched_lag_ns, 0);
    assert_partitions(&r);
}

#[test]
fn still_parked_task_clips_at_query_time() {
    let rec = recording(vec![
        (0, resource(10, "Mutex", 14)),
        (0, spawn(1, "stuck")),
        (10, Event::PollStart { task: 1 }),
        (20, park(1, 10)),
        (30, Event::PollEnd { task: 1 }),
        // recording continues elsewhere
        (
            2030,
            Event::Wake {
                woken: 99,
                by: None,
            },
        ),
    ]);
    let r = rec.why_slow(1, None, 5).unwrap();
    assert_eq!(r.to_ts, 2030);
    assert_eq!(r.resource_wait_ns, 2000);
    assert_partitions(&r);
}

#[test]
fn holder_parked_elsewhere_names_the_next_hop() {
    // waiter → mutex A held by middle; middle itself parked on mutex B.
    let rec = recording(vec![
        (0, resource(10, "Mutex", 14)),
        (0, resource(11, "Mutex", 15)),
        (0, spawn(1, "waiter")),
        (0, spawn(2, "middle")),
        (5, Event::PollStart { task: 2 }),
        (6, acquired_by(10, 2)),
        (7, park(2, 11)),
        (8, Event::PollEnd { task: 2 }),
        (10, Event::PollStart { task: 1 }),
        (20, park(1, 10)),
        (30, Event::PollEnd { task: 1 }),
        (
            1030,
            Event::Wake {
                woken: 99,
                by: None,
            },
        ), // just advances the clock
    ]);
    let r = rec.why_slow(1, None, 5).unwrap();
    let holder = r.waits[0].holder.as_ref().expect("holder attribution");
    assert_eq!(holder.task.name.as_deref(), Some("middle"));
    let next = holder.parked_on.as_ref().expect("middle is parked on B");
    assert_eq!(next.loc.line, Some(15));
    assert!(holder.parked_ns > 0);
}

#[test]
fn pick_slow_task_prefers_most_parked() {
    let rec = recording(vec![
        (0, resource(10, "Mutex", 14)),
        (0, spawn(1, "busy")),
        (0, spawn(2, "parked-long")),
        (10, Event::PollStart { task: 1 }),
        (500, Event::PollEnd { task: 1 }),
        (10, Event::PollStart { task: 2 }),
        (15, park(2, 10)),
        (20, Event::PollEnd { task: 2 }),
        (
            2000,
            Event::Wake {
                woken: 99,
                by: None,
            },
        ),
    ]);
    let picked = rec.pick_slow_task(None).unwrap();
    assert_eq!(picked, Some(2));
}

#[test]
fn pick_slow_task_falls_back_to_longest_lived() {
    let rec = recording(vec![
        (0, spawn(1, "short")),
        (10, Event::TaskEnd { id: 1 }),
        (0, spawn(2, "long")),
        (500, Event::TaskEnd { id: 2 }),
    ]);
    let picked = rec.pick_slow_task(None).unwrap();
    assert_eq!(picked, Some(2));
}
