//! Human-readable rendering of query results.

use wyrd_core::model::{
    BlockedOutcome, BlockedReport, DiffKind, DiffReport, DiffSeverity, LatencyReport, LintKind,
    LintReport, LintSeverity, PredictReport, Stats, TaskGroupStats,
};

pub(crate) fn ms(ns: u64) -> String {
    if ns >= 1_000_000 {
        format!("{:.1}ms", ns as f64 / 1_000_000.0)
    } else if ns >= 1_000 {
        format!("{:.1}µs", ns as f64 / 1_000.0)
    } else {
        format!("{ns}ns")
    }
}

pub fn render_blocked(report: &BlockedReport) {
    let head = report
        .chain
        .first()
        .map(|l| l.task.label())
        .unwrap_or_else(|| format!("task#{}", report.task));

    match &report.outcome {
        BlockedOutcome::NotBlocked => {
            println!("✓ {head} is not blocked at t={}ns.", report.at);
            return;
        }
        BlockedOutcome::Deadlock { cycle } => {
            println!("⛔ DEADLOCK — {head} is in a {}-task cycle:", cycle.len());
        }
        BlockedOutcome::ResourceRoot { .. } => {
            let root = report
                .chain
                .last()
                .map(|l| l.waiting_on.label())
                .unwrap_or_default();
            println!("⏳ {head} is blocked; root cause is {root} (no tracked holder — timer, full channel, or external):");
        }
        BlockedOutcome::ActiveHolder { .. } => {
            println!("⏳ {head} is blocked behind an active (running/idle) holder:");
        }
    }

    for (i, link) in report.chain.iter().enumerate() {
        let arrow = if i == 0 { "  " } else { "  ↳ " };
        let holder = match &link.holder {
            Some(h) => format!("held by {}", h.label()),
            None => "no holder (channel full / timer / external)".to_string(),
        };
        println!(
            "{arrow}{task}  --[{op}, parked {wait}]-->  {res}  ({holder})",
            task = link.task.label(),
            op = link.op_name,
            wait = ms(link.wait_ns),
            res = link.waiting_on.label(),
        );
    }

    if let BlockedOutcome::Deadlock { cycle } = &report.outcome {
        let names: Vec<String> = report
            .chain
            .iter()
            .filter(|l| cycle.contains(&l.task.id))
            .map(|l| l.task.label())
            .collect();
        println!();
        println!("   cycle: {} → (back to start)", names.join(" → "));
        println!("   resources involved:");
        for link in report.chain.iter().filter(|l| cycle.contains(&l.task.id)) {
            println!(
                "     • {} at {}",
                link.waiting_on.concrete_type, link.waiting_on.loc
            );
        }
    }
}

pub fn render_lint(report: &LintReport) {
    if report.is_clean() {
        println!("✓ no findings at t={}.", ms(report.at));
        return;
    }

    for f in &report.findings {
        let tag = match f.severity {
            LintSeverity::Error => "⛔ error",
            LintSeverity::Warning => "⚠ warning",
        };
        match &f.kind {
            LintKind::Deadlock { cycle, resources } => {
                let names: Vec<String> = cycle.iter().map(|t| t.label()).collect();
                println!(
                    "{tag}: deadlock — {}-task cycle: {} → (back to start)",
                    cycle.len(),
                    names.join(" → "),
                );
                for r in resources {
                    println!("    • {}", r.label());
                }
            }
            LintKind::LongPoll {
                task,
                count,
                max_ns,
                threshold_ns,
            } => {
                println!(
                    "{tag}: long poll — {} spent up to {} inside a single poll \
                     ({count} poll{} over the {} threshold); blocking or heavy \
                     compute in async code",
                    task.label(),
                    ms(*max_ns),
                    if *count == 1 { "" } else { "s" },
                    ms(*threshold_ns),
                );
            }
            LintKind::LongPark {
                task,
                resource,
                op_name,
                count,
                max_ns,
                threshold_ns,
            } => {
                println!(
                    "{tag}: long park — {} parked up to {} on {} [{op_name}] \
                     ({count} park{} over the {} threshold)",
                    task.label(),
                    ms(*max_ns),
                    resource.label(),
                    if *count == 1 { "" } else { "s" },
                    ms(*threshold_ns),
                );
            }
            LintKind::SaturatedChannel {
                resource,
                capacity,
                max_depth,
            } => {
                println!(
                    "{tag}: saturated channel — {} peaked at {max_depth}/{capacity}; \
                     senders were (or will be) parked on backpressure",
                    resource.label(),
                );
            }
        }
    }

    let errors = report
        .findings
        .iter()
        .filter(|f| f.severity == LintSeverity::Error)
        .count();
    let warnings = report.findings.len() - errors;
    println!();
    println!(
        "{} finding{} ({errors} error{}, {warnings} warning{})",
        report.findings.len(),
        if report.findings.len() == 1 { "" } else { "s" },
        if errors == 1 { "" } else { "s" },
        if warnings == 1 { "" } else { "s" },
    );
}

pub fn render_predict(report: &PredictReport) {
    if report.cycles.is_empty() {
        println!(
            "✓ no lock-order inversions: {} acquisitions across {} lock(s), {} order edge(s), \
             all consistent.",
            report.acquisitions, report.lock_count, report.order_edges,
        );
        if report.guarded_suppressed > 0 || report.single_task_suppressed > 0 {
            println!(
                "  ({} gate-locked and {} single-task cycle(s) suppressed as unable to deadlock)",
                report.guarded_suppressed, report.single_task_suppressed,
            );
        }
        return;
    }

    for c in &report.cycles {
        let labels: Vec<String> = c.resources.iter().map(|r| r.label()).collect();
        if c.observed {
            println!(
                "⛔ DEADLOCK (observed in this run) — {}-lock cycle: {}",
                c.resources.len(),
                labels.join(" → "),
            );
        } else {
            println!(
                "⚠ POTENTIAL DEADLOCK — {}-lock cycle acquired in conflicting orders \
                 (did not fire in this run): {}",
                c.resources.len(),
                labels.join(" → "),
            );
        }
        for e in &c.edges {
            println!(
                "    {task} held {held} while taking {next} [{op}] at +{at}",
                task = e.task.label(),
                held = e.held.label(),
                next = e.acquiring.label(),
                op = e.op_name,
                at = ms(e.at),
            );
        }
        println!();
    }

    println!(
        "analyzed {} acquisitions across {} lock(s); {} order edge(s); \
         {} cycle(s) reported, {} gate-locked and {} single-task suppressed",
        report.acquisitions,
        report.lock_count,
        report.order_edges,
        report.cycles.len(),
        report.guarded_suppressed,
        report.single_task_suppressed,
    );
    println!(
        "fix: make every task acquire these locks in one canonical order \
         (or merge them / guard both orders behind a common gate lock)."
    );
}

pub fn render_latency(report: &LatencyReport) {
    let pct = |ns: u64| {
        if report.total_ns == 0 {
            0.0
        } else {
            ns as f64 * 100.0 / report.total_ns as f64
        }
    };
    println!(
        "task {}: {} from spawn (+{}) to {}",
        report.task.label(),
        ms(report.total_ns),
        ms(report.from_ts),
        if report.to_ts == report.from_ts + report.total_ns {
            "end"
        } else {
            "now"
        },
    );
    println!();
    let row = |label: &str, ns: u64, extra: &str| {
        println!("  {label:<14} {:>10}  {:>5.1}%{extra}", ms(ns), pct(ns));
    };
    row(
        "own polls",
        report.own_poll_ns,
        &format!(
            "  ({} poll{})",
            report.poll_count,
            if report.poll_count == 1 { "" } else { "s" }
        ),
    );
    row("resource wait", report.resource_wait_ns, "");
    row("timer wait", report.timer_wait_ns, "");
    row("scheduler lag", report.sched_lag_ns, "  (woken → polled)");
    row("idle/other", report.idle_ns, "");

    if report.waits.is_empty() {
        return;
    }
    println!();
    println!("top waits:");
    for (i, w) in report.waits.iter().enumerate() {
        let timer = if w.is_timer { " [timer]" } else { "" };
        println!(
            "  {}. {:>10}  {} [{}] at +{}{timer}",
            i + 1,
            ms(w.wait_ns),
            w.resource.label(),
            w.op_name,
            ms(w.since_ts),
        );
        if w.sched_lag_ns > 0 {
            println!(
                "       └ +{} scheduler lag after the wake",
                ms(w.sched_lag_ns)
            );
        }
        if let Some(h) = &w.holder {
            let doing = if let Some(next) = &h.parked_on {
                format!(
                    "itself parked {} on {} — the chain continues there",
                    ms(h.parked_ns),
                    next.label(),
                )
            } else if h.polling_ns * 10 >= w.wait_ns.max(1) * 6 {
                format!(
                    "inside poll for {} of the wait — busy (blocking in async?)",
                    ms(h.polling_ns),
                )
            } else {
                format!(
                    "polling {} / parked {} during the wait",
                    ms(h.polling_ns),
                    ms(h.parked_ns),
                )
            };
            println!("       └ held by {}: {doing}", h.task.label());
        }
    }
}

pub fn render_diff(report: &DiffReport) {
    let delta = |b: u64, c: u64| -> String {
        if b == c {
            "=".into()
        } else if b == 0 {
            format!("{} → {}", ms(b), ms(c))
        } else {
            format!("{} → {} (×{:.1})", ms(b), ms(c), c as f64 / b as f64)
        }
    };

    println!(
        "baseline: {} span, {} tasks, {} poll, {} wait{}",
        ms(report.baseline.duration_ns),
        report.baseline.task_count,
        ms(report.baseline.total_poll_ns),
        ms(report.baseline.total_wait_ns),
        if report.baseline.deadlocks > 0 {
            format!(", {} deadlock(s)", report.baseline.deadlocks)
        } else {
            String::new()
        },
    );
    println!(
        "current : {} span, {} tasks, {} poll, {} wait{}",
        ms(report.current.duration_ns),
        report.current.task_count,
        ms(report.current.total_poll_ns),
        ms(report.current.total_wait_ns),
        if report.current.deadlocks > 0 {
            format!(", {} deadlock(s)", report.current.deadlocks)
        } else {
            String::new()
        },
    );

    if report.findings.is_empty() {
        println!();
        println!("✓ no behavioral changes beyond the thresholds.");
    }

    for f in &report.findings {
        let tag = match f.severity {
            DiffSeverity::Error => "⛔ error",
            DiffSeverity::Warning => "⚠ regression",
            DiffSeverity::Info => "✓ note",
        };
        match &f.kind {
            DiffKind::NewDeadlock { cycle } => {
                println!(
                    "{tag}: NEW DEADLOCK — cycle: {} → (back to start); not present in baseline",
                    cycle.join(" → "),
                );
            }
            DiffKind::FixedDeadlock { cycle } => {
                println!(
                    "{tag}: deadlock fixed — baseline cycle {} is gone",
                    cycle.join(" → "),
                );
            }
            DiffKind::PollRegression {
                key,
                baseline_ns,
                current_ns,
            } => {
                println!(
                    "{tag}: {key} mean poll time {}",
                    delta(*baseline_ns, *current_ns),
                );
            }
            DiffKind::WaitRegression {
                key,
                baseline_ns,
                current_ns,
            } => {
                println!(
                    "{tag}: {key} mean wait time {}",
                    delta(*baseline_ns, *current_ns),
                );
            }
            DiffKind::NewSaturation {
                key,
                capacity,
                max_depth,
            } => {
                println!(
                    "{tag}: {key} newly saturated — peaked at {max_depth}/{capacity} \
                     (baseline never hit capacity)",
                );
            }
            DiffKind::PollImprovement {
                key,
                baseline_ns,
                current_ns,
            } => {
                println!(
                    "{tag}: {key} mean poll time improved {}",
                    delta(*baseline_ns, *current_ns),
                );
            }
            DiffKind::WaitImprovement {
                key,
                baseline_ns,
                current_ns,
            } => {
                println!(
                    "{tag}: {key} mean wait time improved {}",
                    delta(*baseline_ns, *current_ns),
                );
            }
            DiffKind::NewTaskGroup { key } => {
                println!("{tag}: new task group in current run: {key}");
            }
            DiffKind::RemovedTaskGroup { key } => {
                println!("{tag}: task group gone from current run: {key}");
            }
        }
    }

    // The biggest movers, for context under the verdicts.
    let moved: Vec<_> = report
        .task_groups
        .iter()
        .filter(|g| g.baseline.is_some() || g.current.is_some())
        .take(8)
        .collect();
    if !moved.is_empty() {
        println!();
        println!("task groups (biggest change first):");
        let side = |s: &Option<TaskGroupStats>| match s {
            Some(s) => format!(
                "n={} poll {} wait {}",
                s.instances,
                ms(s.mean_poll_ns()),
                ms(s.mean_wait_ns()),
            ),
            None => "—".to_string(),
        };
        for g in moved {
            println!(
                "  {:<28} {}  |  {}",
                g.key,
                side(&g.baseline),
                side(&g.current),
            );
        }
    }
}

pub fn render_stats(stats: &Stats) {
    println!("recording span : {}", ms(stats.duration_ns));
    println!("tasks          : {}", stats.task_count);
    println!("resources      : {}", stats.resource_count);
    println!();
    println!(
        "poll time      : n={} p50={} p90={} p99={} max={}",
        stats.poll_time.count,
        ms(stats.poll_time.p50),
        ms(stats.poll_time.p90),
        ms(stats.poll_time.p99),
        ms(stats.poll_time.max),
    );

    if !stats.longest_parks.is_empty() {
        println!();
        println!("longest parks  :");
        for p in &stats.longest_parks {
            println!(
                "  {:>10}  {} on {} [{}]",
                ms(p.dur_ns),
                p.task.label(),
                p.resource.label(),
                p.op_name,
            );
        }
    }

    if !stats.channel_depths.is_empty() {
        println!();
        println!("channel depths :");
        for c in &stats.channel_depths {
            println!(
                "  {} peak {}/{}",
                c.resource.label(),
                c.max_depth,
                c.capacity,
            );
        }
    }
}
