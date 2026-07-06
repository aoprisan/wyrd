//! End-to-end integration test: run the demo's deadlock scenario under a
//! watchdog, record it, and assert that wyrd-core's `why_blocked` reports the
//! two-task cycle with both mutexes named by source location.
//!
//! Only meaningful with `--cfg tokio_unstable` (otherwise tokio emits no
//! instrumentation); without it the test is a no-op so plain `cargo test`
//! stays green.

#[cfg(tokio_unstable)]
mod unstable {
    use std::path::PathBuf;
    use std::process::Command;

    use wyrd_core::model::{BlockedOutcome, TaskStatus};
    use wyrd_core::Recording;

    fn unique_recording_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("wyrd-deadlock-{}.wyrd", std::process::id()));
        p
    }

    #[test]
    fn deadlock_scenario_reports_two_cycle() {
        let bin = env!("CARGO_BIN_EXE_wyrd-demo");
        let recording = unique_recording_path();

        let status = Command::new(bin)
            .args(["--scenario", "deadlock", "--watchdog-ms", "500", "--record"])
            .arg(&recording)
            .status()
            .expect("failed to run wyrd-demo");
        assert!(status.success(), "wyrd-demo exited with {status:?}");

        let rec = Recording::open(&recording).expect("open recording");

        // Both deadlocked tasks should be parked at end-of-recording.
        let world = rec.world_state(None).expect("world_state");
        let parked: Vec<_> = world
            .tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Parked { .. }))
            .filter(|t| {
                t.ident
                    .name
                    .as_deref()
                    .is_some_and(|n| n.starts_with("deadlock"))
            })
            .collect();
        assert_eq!(
            parked.len(),
            2,
            "expected both deadlock tasks parked, got: {:#?}",
            world.tasks
        );

        // why_blocked on one of them must detect the 2-task deadlock cycle.
        let ab = rec
            .resolve_task("deadlock-ab")
            .expect("resolve deadlock-ab");
        let report = rec.why_blocked(ab, None).expect("why_blocked");

        let cycle = match &report.outcome {
            BlockedOutcome::Deadlock { cycle } => cycle,
            other => panic!("expected deadlock, got {other:?}\nreport: {report:#?}"),
        };
        assert_eq!(cycle.len(), 2, "expected a 2-task cycle, got {cycle:?}");

        // The chain must implicate exactly two mutexes, each named by its
        // source location in the demo.
        assert_eq!(report.chain.len(), 2, "chain: {:#?}", report.chain);
        let mut mutex_locs: Vec<String> = report
            .chain
            .iter()
            .map(|link| {
                assert_eq!(
                    link.waiting_on.concrete_type, "Mutex",
                    "expected a Mutex, got {:?}",
                    link.waiting_on
                );
                let loc = &link.waiting_on.loc;
                assert!(
                    loc.file
                        .as_deref()
                        .is_some_and(|f| f.contains("examples/demo/src/main.rs")),
                    "mutex not named by source location: {loc:?}"
                );
                loc.to_string()
            })
            .collect();
        mutex_locs.sort();
        mutex_locs.dedup();
        assert_eq!(
            mutex_locs.len(),
            2,
            "expected two distinct mutex locations, got {mutex_locs:?}"
        );

        // Each task in the chain holds the resource the next one waits on.
        for link in &report.chain {
            assert!(
                link.holder.is_some(),
                "every deadlock link should have a holder: {link:?}"
            );
        }

        let _ = std::fs::remove_file(&recording);
    }
}
