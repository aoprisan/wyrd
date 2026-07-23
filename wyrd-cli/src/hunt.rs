//! `wyrd hunt`: a concurrency fuzzer for shim-instrumented binaries.
//!
//! Runs the target command many times, each under a different chaos seed
//! (`WYRD_CHAOS_SEED`), with a per-run recording (`WYRD_RECORD`) and a hang
//! watchdog. Each recording — including ones truncated by the watchdog's kill
//! — is analyzed for *observed* deadlocks (`lint`) and *latent* lock-order
//! inversions (`predict`), and findings are aggregated across seeds by the
//! resources' stable identity (`Type@file:line`), so the report reads
//! "this cycle deadlocked under seeds 3 and 11, and was predicted in 14 more".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use wyrd_core::model::{LintConfig, LintKind, PredictConfig};
use wyrd_core::Recording;

use crate::render::ms;

pub struct HuntOpts {
    pub runs: u64,
    pub seed_start: u64,
    pub timeout: Duration,
    pub record_dir: Option<PathBuf>,
    pub prob: Option<f64>,
    pub max_delay_us: Option<u64>,
    pub keep_clean: bool,
    pub json: bool,
}

/// How one fuzzed run of the target ended.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RunOutcome {
    /// Exited by itself within the timeout.
    Exited { code: Option<i32> },
    /// Killed by the watchdog: the strongest hang signal.
    Hung,
}

#[derive(Debug, Clone, Serialize)]
struct RunReport {
    seed: u64,
    outcome: RunOutcome,
    /// Wall-clock duration of the child.
    wall_ns: u64,
    /// Deadlock cycles `lint` found in this run's recording.
    observed_deadlocks: u64,
    /// Latent cycles `predict` found.
    predicted_cycles: u64,
    /// Recording was cut off mid-frame (killed writer) but still analyzable.
    truncated: bool,
    /// The recording could not be read at all.
    no_recording: bool,
    recording: PathBuf,
}

/// One distinct cycle, aggregated across every run that exhibited it. Keyed
/// by the participating resources' stable labels, so different runs (with
/// different span ids) fold together.
#[derive(Debug, Clone, Serialize)]
struct CycleSighting {
    /// The resources in the cycle, by stable label.
    resources: Vec<String>,
    /// Seeds under which this cycle actually deadlocked.
    observed_seeds: Vec<u64>,
    /// Seeds under which it was only predicted (latent).
    predicted_seeds: Vec<u64>,
    /// A human-readable witness from one run: "task held A while taking B".
    witness: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct HuntReport {
    command: Vec<String>,
    runs: Vec<RunReport>,
    cycles: Vec<CycleSighting>,
    hung_runs: u64,
    deadlocked_runs: u64,
}

impl HuntReport {
    fn worst_exit(&self) -> u8 {
        if self.hung_runs > 0 || self.deadlocked_runs > 0 {
            2
        } else if !self.cycles.is_empty() {
            1
        } else {
            0
        }
    }
}

pub fn run(
    command: &[String],
    opts: &HuntOpts,
) -> Result<std::process::ExitCode, Box<dyn std::error::Error>> {
    let (program, args) = command
        .split_first()
        .ok_or("hunt needs a command to run: wyrd hunt [options] -- <cmd> [args...]")?;

    let dir = match &opts.record_dir {
        Some(d) => d.clone(),
        None => {
            let mut d = std::env::temp_dir();
            d.push(format!("wyrd-hunt-{}", std::process::id()));
            d
        }
    };
    std::fs::create_dir_all(&dir)?;

    let mut runs: Vec<RunReport> = Vec::new();
    let mut cycles: BTreeMap<Vec<String>, CycleSighting> = BTreeMap::new();

    for i in 0..opts.runs {
        let seed = opts.seed_start + i;
        let recording = dir.join(format!("hunt-{seed}.wyrd"));
        let _ = std::fs::remove_file(&recording);

        let mut cmd = Command::new(program);
        cmd.args(args)
            .env("WYRD_CHAOS", "1")
            .env("WYRD_CHAOS_SEED", seed.to_string())
            .env("WYRD_RECORD", &recording)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(p) = opts.prob {
            cmd.env("WYRD_CHAOS_PROB", p.to_string());
        }
        if let Some(us) = opts.max_delay_us {
            cmd.env("WYRD_CHAOS_MAX_DELAY_US", us.to_string());
        }

        let started = Instant::now();
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot spawn {program}: {e}"))?;

        // Watchdog: poll until exit or deadline, then kill.
        let outcome = loop {
            match child.try_wait()? {
                Some(status) => {
                    break RunOutcome::Exited {
                        code: status.code(),
                    }
                }
                None if started.elapsed() >= opts.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break RunOutcome::Hung;
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        let wall_ns = started.elapsed().as_nanos() as u64;

        // Analyze the recording, tolerating a kill-truncated tail.
        let mut report = RunReport {
            seed,
            outcome: outcome.clone(),
            wall_ns,
            observed_deadlocks: 0,
            predicted_cycles: 0,
            truncated: false,
            no_recording: false,
            recording: recording.clone(),
        };
        match Recording::open_lossy(&recording) {
            Ok((rec, truncated)) => {
                report.truncated = truncated;
                analyze_run(&rec, seed, &mut report, &mut cycles);
            }
            Err(_) => report.no_recording = true,
        }

        if !opts.json {
            eprintln!("{}", run_line(&report));
        }
        let interesting = report.outcome == RunOutcome::Hung
            || report.observed_deadlocks > 0
            || report.predicted_cycles > 0;
        if !interesting && !opts.keep_clean {
            let _ = std::fs::remove_file(&recording);
        }
        runs.push(report);
    }

    let mut cycles: Vec<CycleSighting> = cycles.into_values().collect();
    // Cycles that actually fired first, then the most-often-predicted.
    cycles.sort_by(|a, b| {
        (
            std::cmp::Reverse(a.observed_seeds.len()),
            std::cmp::Reverse(a.predicted_seeds.len()),
        )
            .cmp(&(
                std::cmp::Reverse(b.observed_seeds.len()),
                std::cmp::Reverse(b.predicted_seeds.len()),
            ))
    });

    let hunt = HuntReport {
        command: command.to_vec(),
        hung_runs: runs
            .iter()
            .filter(|r| r.outcome == RunOutcome::Hung)
            .count() as u64,
        deadlocked_runs: runs.iter().filter(|r| r.observed_deadlocks > 0).count() as u64,
        runs,
        cycles,
    };

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&hunt)?);
    } else {
        render(&hunt, &dir);
    }
    Ok(std::process::ExitCode::from(hunt.worst_exit()))
}

/// Fold one run's lint + predict results into the per-run report and the
/// cross-run cycle aggregation.
fn analyze_run(
    rec: &Recording,
    seed: u64,
    report: &mut RunReport,
    cycles: &mut BTreeMap<Vec<String>, CycleSighting>,
) {
    // Observed deadlocks: lint's deadlock findings carry the resource idents.
    if let Ok(lint) = rec.lint(None, &LintConfig::default()) {
        for f in &lint.findings {
            let LintKind::Deadlock {
                cycle: tasks,
                resources,
            } = &f.kind
            else {
                continue;
            };
            report.observed_deadlocks += 1;
            let key = cycle_key(resources.iter().map(|r| r.label()));
            let entry = cycles.entry(key.clone()).or_insert_with(|| CycleSighting {
                resources: key,
                observed_seeds: Vec::new(),
                predicted_seeds: Vec::new(),
                witness: tasks
                    .iter()
                    .zip(resources)
                    .map(|(t, r)| format!("{} parked on {}", t.label(), r.label()))
                    .collect(),
            });
            if !entry.observed_seeds.contains(&seed) {
                entry.observed_seeds.push(seed);
            }
        }
    }

    if let Ok(predict) = rec.predict(None, &PredictConfig::default()) {
        for c in &predict.cycles {
            if !c.observed {
                report.predicted_cycles += 1;
            }
            let key = cycle_key(c.resources.iter().map(|r| r.label()));
            let entry = cycles.entry(key.clone()).or_insert_with(|| CycleSighting {
                resources: key,
                observed_seeds: Vec::new(),
                predicted_seeds: Vec::new(),
                witness: c
                    .edges
                    .iter()
                    .map(|e| {
                        format!(
                            "{} held {} while taking {}",
                            e.task.label(),
                            e.held.label(),
                            e.acquiring.label(),
                        )
                    })
                    .collect(),
            });
            if c.observed {
                if !entry.observed_seeds.contains(&seed) {
                    entry.observed_seeds.push(seed);
                }
            } else if !entry.predicted_seeds.contains(&seed) {
                entry.predicted_seeds.push(seed);
            }
        }
    }
}

/// Canonical cross-run identity of a cycle: its resource labels, sorted.
fn cycle_key(labels: impl Iterator<Item = String>) -> Vec<String> {
    let mut key: Vec<String> = labels.collect();
    key.sort();
    key.dedup();
    key
}

fn run_line(r: &RunReport) -> String {
    let status = match &r.outcome {
        RunOutcome::Hung => "HUNG (killed)".to_string(),
        RunOutcome::Exited { code: Some(0) } => "exit 0".to_string(),
        RunOutcome::Exited { code: Some(c) } => format!("exit {c}"),
        RunOutcome::Exited { code: None } => "killed by signal".to_string(),
    };
    let mut notes = Vec::new();
    if r.observed_deadlocks > 0 {
        notes.push(format!("⛔ {} deadlock(s)", r.observed_deadlocks));
    }
    if r.predicted_cycles > 0 {
        notes.push(format!("⚠ {} latent cycle(s)", r.predicted_cycles));
    }
    if r.truncated {
        notes.push("truncated recording".into());
    }
    if r.no_recording {
        notes.push("no recording produced".into());
    }
    let notes = if notes.is_empty() {
        "clean".to_string()
    } else {
        notes.join(", ")
    };
    format!(
        "  seed {:>4}  {:>14}  {:>9}  {notes}",
        r.seed,
        status,
        ms(r.wall_ns),
    )
}

fn render(hunt: &HuntReport, dir: &Path) {
    println!();
    println!(
        "hunted `{}` × {} runs: {} hung, {} deadlocked, {} distinct cycle(s)",
        hunt.command.join(" "),
        hunt.runs.len(),
        hunt.hung_runs,
        hunt.deadlocked_runs,
        hunt.cycles.len(),
    );

    for c in &hunt.cycles {
        println!();
        if c.observed_seeds.is_empty() {
            println!(
                "⚠ latent cycle (predicted under {} seed(s), never fired): {}",
                c.predicted_seeds.len(),
                c.resources.join(" ↔ "),
            );
        } else {
            println!(
                "⛔ cycle DEADLOCKED under seed(s) {:?}{}: {}",
                c.observed_seeds,
                if c.predicted_seeds.is_empty() {
                    String::new()
                } else {
                    format!(
                        " (and was predicted under {} more)",
                        c.predicted_seeds.len()
                    )
                },
                c.resources.join(" ↔ "),
            );
        }
        for w in &c.witness {
            println!("    • {w}");
        }
        if let Some(seed) = c.observed_seeds.first().or(c.predicted_seeds.first()) {
            println!(
                "    ↳ inspect: wyrd why-blocked {}/hunt-{seed}.wyrd   (or wyrd predict …)",
                dir.display(),
            );
        }
    }

    println!();
    if hunt.hung_runs > 0 || hunt.deadlocked_runs > 0 {
        println!(
            "verdict: reproduced — recordings for failing seeds kept in {}",
            dir.display()
        );
    } else if !hunt.cycles.is_empty() {
        println!(
            "verdict: no run failed, but latent lock-order cycles exist — fix the ordering \
             or re-hunt with more runs / higher WYRD_CHAOS_MAX_DELAY_US"
        );
    } else {
        println!("verdict: clean — no hangs, deadlocks, or lock-order inversions observed");
    }
}
