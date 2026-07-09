//! `wyrd watch`: headless live monitoring of a growing recording.
//!
//! The headless sibling of `wyrd tui --follow`, built for CI jobs, logs, and
//! terminals without a TTY: re-fold the recording on an interval, and when a
//! task has been parked on a non-timer resource beyond a threshold, print its
//! full `why-blocked` chain once per park episode. A detected deadlock is
//! terminal: the report is printed and the process exits `2`.
//!
//! Like follow mode, the producer is never touched — the recorded program
//! keeps appending frames and every cost lives in this observer process.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use wyrd_core::model::{BlockedOutcome, BlockedReport, TaskStatus};
use wyrd_core::TaskId;

use crate::follow::load_follow;
use crate::render;

pub(crate) struct WatchOpts {
    /// Alert when a task has been parked longer than this.
    pub stuck_ns: u64,
    /// How often to re-fold the recording.
    pub interval: Duration,
    /// Stop watching (exit 0/1) after this long; `None` = watch forever.
    pub run_for: Option<Duration>,
    /// Emit newline-delimited JSON instead of human-readable text.
    pub json: bool,
}

/// One alert produced by a tick.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "alert", rename_all = "snake_case")]
pub(crate) enum Alert {
    /// A task crossed the stuck threshold on a park.
    Stuck { report: BlockedReport },
    /// A hold-and-wait cycle formed. Terminal.
    Deadlock { report: BlockedReport },
}

/// Resources a task is *supposed* to wait on for a long time.
fn is_timer(concrete_type: &str) -> bool {
    matches!(concrete_type, "Sleep" | "Interval" | "Timeout")
}

/// Re-folds the recording each tick and remembers which park episodes and
/// deadlock cycles it has already alerted on.
pub(crate) struct Watcher {
    path: PathBuf,
    stuck_ns: u64,
    /// Park episodes already reported: `(task, resource, park timestamp)`.
    alerted: HashSet<(TaskId, u64, u64)>,
    /// Deadlock cycles already reported (sorted task ids).
    cycles: HashSet<Vec<TaskId>>,
}

impl Watcher {
    pub(crate) fn new(path: &Path, stuck_ns: u64) -> Self {
        Self {
            path: path.to_path_buf(),
            stuck_ns,
            alerted: HashSet::new(),
            cycles: HashSet::new(),
        }
    }

    /// Fold the file's current contents and return the alerts that are new
    /// since the last tick.
    pub(crate) fn tick(&mut self) -> Vec<Alert> {
        let rec = load_follow(&self.path);
        let Ok(end) = rec.end_ts() else {
            return Vec::new();
        };
        let Ok(world) = rec.world_state(Some(end)) else {
            return Vec::new();
        };

        let mut alerts = Vec::new();
        for t in &world.tasks {
            let TaskStatus::Parked { resource } = t.status else {
                continue;
            };
            let Ok(report) = rec.why_blocked(t.ident.id, Some(end)) else {
                continue;
            };
            let Some(head) = report.chain.first() else {
                continue;
            };

            if let BlockedOutcome::Deadlock { cycle } = &report.outcome {
                let mut key = cycle.clone();
                key.sort_unstable();
                if self.cycles.insert(key) {
                    alerts.push(Alert::Deadlock { report });
                }
                continue;
            }

            // Timers are intentional waits, not stuck tasks.
            if is_timer(&head.waiting_on.concrete_type) {
                continue;
            }
            if head.wait_ns < self.stuck_ns {
                continue;
            }
            if self.alerted.insert((t.ident.id, resource, head.since_ts)) {
                alerts.push(Alert::Stuck { report });
            }
        }
        alerts
    }
}

fn print_alert(alert: &Alert, json: bool) {
    if json {
        // One JSON object per line, for log pipelines.
        if let Ok(line) = serde_json::to_string(alert) {
            println!("{line}");
        }
        return;
    }
    match alert {
        Alert::Stuck { report } => {
            println!("--- stuck task ---");
            render::render_blocked(report);
        }
        Alert::Deadlock { report } => {
            println!("--- DEADLOCK ---");
            render::render_blocked(report);
        }
    }
}

/// Entry point for the `watch` subcommand.
pub(crate) fn run(file: &Path, opts: WatchOpts) -> ExitCode {
    let mut watcher = Watcher::new(file, opts.stuck_ns);
    let deadline = opts.run_for.map(|d| Instant::now() + d);
    let mut warned = false;

    if !opts.json {
        eprintln!(
            "watching {} (tick {}ms, stuck threshold {}ms) — ctrl-c to stop",
            file.display(),
            opts.interval.as_millis(),
            opts.stuck_ns / 1_000_000,
        );
    }

    loop {
        for alert in watcher.tick() {
            print_alert(&alert, opts.json);
            match alert {
                Alert::Deadlock { .. } => return ExitCode::from(2),
                Alert::Stuck { .. } => warned = true,
            }
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return if warned {
                    ExitCode::from(1)
                } else {
                    ExitCode::SUCCESS
                };
            }
        }
        std::thread::sleep(opts.interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyrd_weave::{Event, FrameWriter, Loc, Record, StateOp, TaskKind, FIELD_ACQUIRED_BY};

    fn loc(line: u32) -> Loc {
        Loc {
            file: Some("src/main.rs".into()),
            line: Some(line),
            col: None,
        }
    }

    /// The canonical two-mutex deadlock, growable: the first 10 events are a
    /// healthy prefix (both tasks holding one lock each), the rest form the
    /// cycle.
    fn deadlock_records() -> Vec<Record> {
        let events: Vec<(u64, Event)> = vec![
            (
                1,
                Event::ResourceNew {
                    id: 100,
                    parent: None,
                    concrete_type: "Mutex".into(),
                    loc: loc(10),
                    is_internal: false,
                },
            ),
            (
                2,
                Event::ResourceNew {
                    id: 200,
                    parent: None,
                    concrete_type: "Mutex".into(),
                    loc: loc(20),
                    is_internal: false,
                },
            ),
            (
                3,
                Event::TaskSpawn {
                    id: 1,
                    parent: None,
                    name: Some("t1".into()),
                    loc: loc(1),
                    kind: TaskKind::Task,
                },
            ),
            (
                4,
                Event::TaskSpawn {
                    id: 2,
                    parent: None,
                    name: Some("t2".into()),
                    loc: loc(1),
                    kind: TaskKind::Task,
                },
            ),
            (5, Event::PollStart { task: 1 }),
            (
                6,
                Event::ResourceState {
                    id: 100,
                    field: FIELD_ACQUIRED_BY.into(),
                    value: 1,
                    op: StateOp::Override,
                },
            ),
            (7, Event::PollEnd { task: 1 }),
            (8, Event::PollStart { task: 2 }),
            (
                9,
                Event::ResourceState {
                    id: 200,
                    field: FIELD_ACQUIRED_BY.into(),
                    value: 2,
                    op: StateOp::Override,
                },
            ),
            (10, Event::PollEnd { task: 2 }),
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
        ];
        events
            .into_iter()
            .map(|(ts, event)| Record { ts, event })
            .collect()
    }

    fn to_frames(records: &[Record]) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut w = FrameWriter::new(&mut buf).expect("header");
        for r in records {
            w.write_record(r).expect("frame");
        }
        w.flush().expect("flush");
        buf
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wyrd-watch-{tag}-{}.wyrd", std::process::id()))
    }

    #[test]
    fn deadlock_alerts_once_per_cycle() {
        let path = temp_path("deadlock");
        let records = deadlock_records();

        // Healthy prefix: no alerts.
        std::fs::write(&path, to_frames(&records[..10])).unwrap();
        let mut w = Watcher::new(&path, 1);
        assert!(w.tick().is_empty(), "no alerts while healthy");

        // Cycle forms: exactly one deadlock alert (not one per member task).
        std::fs::write(&path, to_frames(&records)).unwrap();
        let alerts = w.tick();
        assert_eq!(alerts.len(), 1, "got: {alerts:?}");
        assert!(matches!(&alerts[0], Alert::Deadlock { report } if report.is_deadlock()));

        // Same cycle on the next tick: already reported.
        assert!(w.tick().is_empty(), "cycle must not re-alert");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stuck_task_alerts_once_per_park_episode() {
        let path = temp_path("stuck");
        // One task parked on a mutex someone holds, no cycle: t2 holds 100,
        // t1 parks on it and stays parked to end-of-recording (t=500).
        let records: Vec<Record> = vec![
            Record {
                ts: 1,
                event: Event::ResourceNew {
                    id: 100,
                    parent: None,
                    concrete_type: "Mutex".into(),
                    loc: loc(10),
                    is_internal: false,
                },
            },
            Record {
                ts: 2,
                event: Event::TaskSpawn {
                    id: 1,
                    parent: None,
                    name: Some("t1".into()),
                    loc: loc(1),
                    kind: TaskKind::Task,
                },
            },
            Record {
                ts: 3,
                event: Event::TaskSpawn {
                    id: 2,
                    parent: None,
                    name: Some("t2".into()),
                    loc: loc(2),
                    kind: TaskKind::Task,
                },
            },
            Record {
                ts: 4,
                event: Event::ResourceState {
                    id: 100,
                    field: FIELD_ACQUIRED_BY.into(),
                    value: 2,
                    op: StateOp::Override,
                },
            },
            Record {
                ts: 10,
                event: Event::PollStart { task: 1 },
            },
            Record {
                ts: 11,
                event: Event::Park {
                    task: 1,
                    resource: 100,
                    op_name: "poll_acquire".into(),
                },
            },
            Record {
                ts: 12,
                event: Event::PollEnd { task: 1 },
            },
            Record {
                ts: 500,
                event: Event::Wake { woken: 2, by: None },
            },
        ];
        std::fs::write(&path, to_frames(&records)).unwrap();

        // Threshold above the wait: silent.
        let mut quiet = Watcher::new(&path, 1_000);
        assert!(quiet.tick().is_empty(), "wait below threshold");

        // Threshold below the wait (489ns): one Stuck alert, then silence.
        let mut w = Watcher::new(&path, 100);
        let alerts = w.tick();
        assert_eq!(alerts.len(), 1, "got: {alerts:?}");
        let Alert::Stuck { report } = &alerts[0] else {
            panic!("expected Stuck, got {alerts:?}");
        };
        assert_eq!(report.chain[0].task.name.as_deref(), Some("t1"));
        assert!(w.tick().is_empty(), "same episode must not re-alert");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_yields_no_alerts() {
        let mut w = Watcher::new(Path::new("/nonexistent/never.wyrd"), 1);
        assert!(w.tick().is_empty());
    }
}
