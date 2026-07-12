//! The `wyrd` CLI: inspect a recording's async causality.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use wyrd_core::Recording;

mod follow;
mod render;
mod tui;
mod watch;

#[derive(Parser)]
#[command(
    name = "wyrd",
    about = "async causality inspection for tokio recordings",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Explain why a task is blocked, following the park → holder chain.
    WhyBlocked {
        /// The recording file.
        file: PathBuf,
        /// Task to inspect, by `task::Builder` name or span id. Defaults to a
        /// task that is currently parked (if any).
        #[arg(long)]
        task: Option<String>,
        /// Timestamp (ns) to evaluate at. Defaults to end-of-recording.
        #[arg(long)]
        at: Option<u64>,
        /// Emit JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Explain where a task's time went: own poll time, resource waits blamed
    /// on their holders, timer waits, scheduler lag (woken → polled), idle.
    WhySlow {
        /// The recording file.
        file: PathBuf,
        /// Task to inspect, by `task::Builder` name or span id. Defaults to
        /// the task with the most time spent parked.
        #[arg(long)]
        task: Option<String>,
        /// Timestamp (ns) to clip the window at. Defaults to end-of-recording.
        #[arg(long)]
        at: Option<u64>,
        /// How many top wait episodes to show.
        #[arg(long, default_value_t = 5)]
        top: usize,
        /// Emit JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Compare two recordings (baseline vs current) by stable task/resource
    /// identity and report regressions: new deadlocks (exit 2), poll/wait
    /// growth and new saturation (exit 1), improvements (exit 0). Built to
    /// gate CI: record a baseline on main, diff against it on every PR.
    Diff {
        /// The known-good recording.
        baseline: PathBuf,
        /// The recording to judge.
        current: PathBuf,
        /// Relative growth needed to flag a regression (1.5 = +50%).
        #[arg(long, default_value_t = 1.5)]
        ratio: f64,
        /// Absolute growth (ms) needed to flag — silences noise on tiny values.
        #[arg(long, default_value_t = 1.0)]
        floor_ms: f64,
        /// Emit JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Summarize a recording: task count, poll-time percentiles, longest parks,
    /// channel depths.
    Stats {
        /// The recording file.
        file: PathBuf,
        /// How many longest-parks to show.
        #[arg(long, default_value_t = 10)]
        top: usize,
        /// Emit JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Scan a recording for async anti-patterns: deadlocks, blocking-in-async
    /// long polls, suspiciously long parks, and saturated channels.
    /// Exits 2 on errors (deadlocks), 1 on warnings, 0 when clean.
    Lint {
        /// The recording file.
        file: PathBuf,
        /// Flag any single poll longer than this (blocking-in-async).
        #[arg(long, default_value_t = 1.0)]
        long_poll_ms: f64,
        /// Flag any non-timer park longer than this.
        #[arg(long, default_value_t = 1000.0)]
        long_park_ms: f64,
        /// Timestamp (ns) to evaluate at. Defaults to end-of-recording.
        #[arg(long)]
        at: Option<u64>,
        /// Emit JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Browse a recording interactively: stats, tasks, resources, and a
    /// why-blocked view, with a time cursor you can scrub across the recording.
    Tui {
        /// The recording file.
        file: PathBuf,
        /// How many longest-parks to show on the stats tab.
        #[arg(long, default_value_t = 10)]
        top: usize,
        /// Follow the recording as it grows (like `tail -f`): re-fold on an
        /// interval so you can watch a running app's async state live. The
        /// recorded program is never touched — all cost is in this viewer.
        #[arg(long)]
        follow: bool,
    },
    /// Watch a growing recording headlessly (for CI and logs): alert with the
    /// full why-blocked chain when a task is parked beyond a threshold, and
    /// exit 2 the moment a deadlock forms. Exits 1 at --for if tasks got
    /// stuck, 0 if all clear.
    Watch {
        /// The recording file (may not exist yet; watch waits for data).
        file: PathBuf,
        /// Alert when a task has been parked on a non-timer resource this long.
        #[arg(long, default_value_t = 1000.0)]
        stuck_ms: f64,
        /// How often to re-read the recording.
        #[arg(long, default_value_t = 500)]
        interval_ms: u64,
        /// Stop after this many seconds (exit 0/1). Default: watch forever.
        #[arg(long, value_name = "SECS")]
        r#for: Option<f64>,
        /// Emit newline-delimited JSON alerts instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wyrd: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::WhyBlocked {
            file,
            task,
            at,
            json,
        } => {
            let rec = Recording::open(&file)?;
            let task_id = match task {
                Some(sel) => rec.resolve_task(&sel)?,
                None => rec
                    .pick_blocked_task(at)?
                    .ok_or("recording contains no tasks")?,
            };
            let report = rec.why_blocked(task_id, at)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                render::render_blocked(&report);
            }
            // Exit code 2 on a detected deadlock, so scripts/tests can gate.
            Ok(if report.is_deadlock() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::WhySlow {
            file,
            task,
            at,
            top,
            json,
        } => {
            let rec = Recording::open(&file)?;
            let task_id = match task {
                Some(sel) => rec.resolve_task(&sel)?,
                None => rec
                    .pick_slow_task(at)?
                    .ok_or("recording contains no tasks")?,
            };
            let report = rec.why_slow(task_id, at, top)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                render::render_latency(&report);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Diff {
            baseline,
            current,
            ratio,
            floor_ms,
            json,
        } => {
            let base = Recording::open(&baseline)?;
            let cur = Recording::open(&current)?;
            let config = wyrd_core::model::DiffConfig {
                ratio,
                abs_floor_ns: (floor_ms * 1e6) as u64,
            };
            let report = wyrd_core::diff(&base, &cur, &config)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                render::render_diff(&report);
            }
            Ok(if report.has_errors() {
                ExitCode::from(2)
            } else if report.has_regressions() {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Stats { file, top, json } => {
            let rec = Recording::open(&file)?;
            let stats = rec.stats(top)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                render::render_stats(&stats);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Lint {
            file,
            long_poll_ms,
            long_park_ms,
            at,
            json,
        } => {
            let rec = Recording::open(&file)?;
            let config = wyrd_core::model::LintConfig {
                long_poll_ns: (long_poll_ms * 1e6) as u64,
                long_park_ns: (long_park_ms * 1e6) as u64,
            };
            let report = rec.lint(at, &config)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                render::render_lint(&report);
            }
            Ok(if report.has_errors() {
                ExitCode::from(2)
            } else if report.is_clean() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Command::Tui { file, top, follow } => {
            tui::run(&file, top, follow)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Watch {
            file,
            stuck_ms,
            interval_ms,
            r#for,
            json,
        } => {
            let opts = watch::WatchOpts {
                stuck_ns: (stuck_ms * 1e6) as u64,
                interval: std::time::Duration::from_millis(interval_ms),
                run_for: r#for.map(std::time::Duration::from_secs_f64),
                json,
            };
            Ok(watch::run(&file, opts))
        }
    }
}
