//! End-to-end: a run that completes *cleanly* on stable Rust still yields a
//! `predict` finding — the lock-order inversion is caught without a deadlock
//! ever happening.

use std::sync::Arc;

use wyrd_core::model::PredictConfig;
use wyrd_core::Recording;

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("wyrd-shim-{tag}-{}.wyrd", std::process::id()));
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_run_predicts_lock_order_inversion() {
    let path = temp_path("predict");
    let guard = wyrd_shim::init(&path).expect("init recorder");

    let mutex_a = Arc::new(wyrd_shim::Mutex::new(0u64));
    let mutex_b = Arc::new(wyrd_shim::Mutex::new(0u64));

    // Strictly sequential tasks — zero chance of an actual deadlock — with
    // opposite acquisition orders.
    let (a1, b1) = (mutex_a.clone(), mutex_b.clone());
    let t1 = wyrd_shim::spawn_named("order-ab", async move {
        let _ga = a1.lock().await;
        let _gb = b1.lock().await;
    });
    t1.await.expect("order-ab");

    let (a2, b2) = (mutex_a.clone(), mutex_b.clone());
    let t2 = wyrd_shim::spawn_named("order-ba", async move {
        let _gb = b2.lock().await;
        let _ga = a2.lock().await;
    });
    t2.await.expect("order-ba");

    drop(guard);

    let rec = Recording::open(&path).expect("open recording");

    // Nothing is blocked: the forensic view is clean...
    let lint = rec.lint(None, &Default::default()).expect("lint");
    assert!(
        !lint.has_errors(),
        "no actual deadlock: {:#?}",
        lint.findings
    );

    // ...but the latent inversion is predicted.
    let report = rec
        .predict(None, &PredictConfig::default())
        .expect("predict");
    assert_eq!(report.cycles.len(), 1, "got: {:#?}", report);
    let cycle = &report.cycles[0];
    assert!(!cycle.observed);
    assert_eq!(cycle.resources.len(), 2);
    // Witnesses are the two differently-ordered tasks.
    let mut tasks: Vec<_> = cycle
        .edges
        .iter()
        .map(|e| e.task.name.clone().unwrap_or_default())
        .collect();
    tasks.sort();
    assert_eq!(tasks, vec!["order-ab".to_string(), "order-ba".to_string()]);
    // Real source locations from #[track_caller].
    for r in &cycle.resources {
        assert!(
            r.loc
                .file
                .as_deref()
                .is_some_and(|f| f.contains("predict_stable")),
            "expected this file as the lock's loc: {:?}",
            r.loc
        );
    }

    let _ = std::fs::remove_file(&path);
}
