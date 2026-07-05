//! Prove the shim reproduces wyrd's deadlock report on **pure stable Rust** —
//! no `--cfg tokio_unstable`, no tracing layer, just the wrapper types.

use std::sync::Arc;
use std::time::Duration;

use wyrd_core::model::{BlockedOutcome, TaskStatus};
use wyrd_core::Recording;

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("wyrd-shim-{tag}-{}.wyrd", std::process::id()));
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stable_deadlock_reports_two_cycle() {
    let path = temp_path("deadlock");
    let guard = wyrd_shim::init(&path).expect("init recorder");

    // Two mutexes, created here (loc = this file).
    let mutex_a = Arc::new(wyrd_shim::Mutex::new(0u64));
    let mutex_b = Arc::new(wyrd_shim::Mutex::new(0u64));

    let (a1, b1) = (mutex_a.clone(), mutex_b.clone());
    let _t1 = wyrd_shim::spawn_named("dead-ab", async move {
        let _ga = a1.lock().await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        let _gb = b1.lock().await; // parks forever
    });

    let (a2, b2) = (mutex_a.clone(), mutex_b.clone());
    let _t2 = wyrd_shim::spawn_named("dead-ba", async move {
        let _gb = b2.lock().await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        let _ga = a2.lock().await; // parks forever
    });

    // Let the deadlock form, then freeze the recording with both parked.
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(guard);

    let rec = Recording::open(&path).expect("open recording");

    // Both tasks parked at end-of-recording.
    let world = rec.world_state(None).expect("world_state");
    let parked = world
        .tasks
        .iter()
        .filter(|t| matches!(t.status, TaskStatus::Parked { .. }))
        .filter(|t| t.ident.name.as_deref().is_some_and(|n| n.starts_with("dead-")))
        .count();
    assert_eq!(parked, 2, "expected both tasks parked: {:#?}", world.tasks);

    // why_blocked detects the 2-cycle.
    let ab = rec.resolve_task("dead-ab").expect("resolve dead-ab");
    let report = rec.why_blocked(ab, None).expect("why_blocked");
    let cycle = match &report.outcome {
        BlockedOutcome::Deadlock { cycle } => cycle,
        other => panic!("expected deadlock, got {other:?}\n{report:#?}"),
    };
    assert_eq!(cycle.len(), 2, "cycle: {cycle:?}");

    // Two distinct mutexes, each named by source location in this test file,
    // each with a holder.
    assert_eq!(report.chain.len(), 2, "chain: {:#?}", report.chain);
    let mut locs: Vec<String> = Vec::new();
    for link in &report.chain {
        assert_eq!(link.waiting_on.concrete_type, "Mutex");
        let file = link.waiting_on.loc.file.as_deref().unwrap_or("");
        assert!(
            file.contains("deadlock_stable.rs"),
            "mutex not named by source location: {:?}",
            link.waiting_on.loc
        );
        assert!(link.holder.is_some(), "link missing holder: {link:?}");
        locs.push(link.waiting_on.loc.to_string());
    }
    locs.sort();
    locs.dedup();
    assert_eq!(locs.len(), 2, "expected two distinct mutex locations: {locs:?}");

    let _ = std::fs::remove_file(&path);
}
