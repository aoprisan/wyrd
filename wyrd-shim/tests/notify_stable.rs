//! The `Notify` shim records a waiter's park on pure stable Rust.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{parks, temp_path};
use wyrd_core::Recording;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_wait_records_a_park() {
    let path = temp_path("notify");
    let guard = wyrd_shim::init(&path).expect("init recorder");

    let notify = Arc::new(wyrd_shim::Notify::new());

    let n = notify.clone();
    let waiter = wyrd_shim::spawn_named("signal-waiter", async move {
        n.notified().await;
    });

    tokio::time::sleep(Duration::from_millis(80)).await;
    notify.notify_one();
    let _ = waiter.await;
    drop(guard);

    let rec = Recording::open(&path).expect("open recording");
    let parks = parks(&rec);
    assert!(
        parks.contains(&("signal-waiter".into(), "Notify".into(), "notified".into())),
        "waiter should have parked on the Notify: {parks:?}"
    );

    let _ = std::fs::remove_file(&path);
}
