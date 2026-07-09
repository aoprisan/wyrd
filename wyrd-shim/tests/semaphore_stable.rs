//! The `Semaphore` shim records permits and exhaustion parks on pure stable
//! Rust, and `lint` flags the saturation.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{parks, temp_path};
use wyrd_core::model::LintKind;
use wyrd_core::Recording;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semaphore_exhaustion_parks_and_lints_saturated() {
    let path = temp_path("semaphore");
    let guard = wyrd_shim::init(&path).expect("init recorder");

    let sem = Arc::new(wyrd_shim::Semaphore::new(2));

    let mut holders = Vec::new();
    for i in 0..2 {
        let s = sem.clone();
        holders.push(wyrd_shim::spawn_named(format!("holder-{i}"), async move {
            let _p = s.acquire().await.expect("acquire");
            tokio::time::sleep(Duration::from_millis(150)).await;
        }));
    }
    tokio::time::sleep(Duration::from_millis(40)).await;

    let s = sem.clone();
    let waiter = wyrd_shim::spawn_named("waiter", async move {
        let _p = s.acquire().await.expect("acquire");
    });

    let _ = waiter.await;
    for h in holders {
        let _ = h.await;
    }
    drop(guard);

    let rec = Recording::open(&path).expect("open recording");
    let parks = parks(&rec);
    assert!(
        parks.contains(&("waiter".into(), "Semaphore".into(), "acquire".into())),
        "waiter should have parked on the exhausted semaphore: {parks:?}"
    );

    // The semaphore hit 2/2: lint flags saturation with default thresholds.
    let report = rec
        .lint(None, &wyrd_core::model::LintConfig::default())
        .expect("lint");
    assert!(
        report.findings.iter().any(|f| matches!(
            &f.kind,
            LintKind::SaturatedChannel { resource, capacity: 2, max_depth: 2 }
                if resource.concrete_type == "Semaphore"
        )),
        "expected a saturated-channel finding: {:#?}",
        report.findings
    );

    // All permits returned by end-of-recording.
    let world = rec.world_state(None).expect("world_state");
    let sem_state = world
        .resources
        .iter()
        .find(|r| r.ident.concrete_type == "Semaphore")
        .expect("semaphore in world state");
    assert_eq!(sem_state.permits, Some(2), "permits restored on drop");

    let _ = std::fs::remove_file(&path);
}
