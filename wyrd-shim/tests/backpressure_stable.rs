//! Prove the shim records mpsc backpressure (channel depth + a park on a full
//! send) on pure stable Rust.

use std::time::Duration;

use wyrd_core::Recording;

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("wyrd-shim-{tag}-{}.wyrd", std::process::id()));
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stable_mpsc_records_backpressure() {
    let path = temp_path("mpsc");
    let guard = wyrd_shim::init(&path).expect("init recorder");

    let (tx, mut rx) = wyrd_shim::mpsc::channel::<u64>(2);

    let producer = wyrd_shim::spawn_named("producer", async move {
        for i in 0..6 {
            if tx.send(i).await.is_err() {
                break;
            }
        }
    });
    let consumer = wyrd_shim::spawn_named("consumer", async move {
        while let Some(_v) = rx.recv().await {
            tokio::time::sleep(Duration::from_millis(8)).await; // slow consumer
        }
    });

    let _ = tokio::join!(producer, consumer);
    drop(guard);

    let rec = Recording::open(&path).expect("open recording");
    let stats = rec.stats(10).expect("stats");

    // The bounded channel (capacity 2) filled up at least once.
    let chan = stats
        .channel_depths
        .iter()
        .find(|c| c.capacity == 2)
        .unwrap_or_else(|| panic!("no capacity-2 channel in {:#?}", stats.channel_depths));
    assert!(
        chan.max_depth >= 1,
        "expected the channel to hold items: {chan:?}"
    );

    // The slow consumer forced the producer to park on a full send.
    let producer_parked = stats
        .longest_parks
        .iter()
        .any(|p| p.op_name == "send" && p.task.name.as_deref() == Some("producer"));
    assert!(
        producer_parked,
        "expected a backpressure park on send: {:#?}",
        stats.longest_parks
    );

    let _ = std::fs::remove_file(&path);
}
