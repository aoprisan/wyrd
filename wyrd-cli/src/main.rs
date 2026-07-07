//! The `wyrd` CLI: inspect a recording's async causality.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use wyrd_core::Recording;

mod render;
mod tui;

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
    /// Browse a recording interactively: stats, tasks, resources, and a
    /// why-blocked view, with a time cursor you can scrub across the recording.
    Tui {
        /// The recording file.
        file: PathBuf,
        /// How many longest-parks to show on the stats tab.
        #[arg(long, default_value_t = 10)]
        top: usize,
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
        Command::Tui { file, top } => {
            tui::run(&file, top)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}
