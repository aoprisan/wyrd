//! Human-readable rendering of query results.

use wyrd_core::model::{BlockedOutcome, BlockedReport, Stats};

fn ms(ns: u64) -> String {
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
