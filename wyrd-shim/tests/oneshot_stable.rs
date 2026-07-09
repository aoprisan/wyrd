//! The `oneshot` shim records the receiver's park on pure stable Rust.

mod common;

use std::time::Duration;

use common::{parks, temp_path};
use wyrd_core::Recording;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oneshot_recv_records_a_park() {
    let path = temp_path("oneshot");
    let guard = wyrd_shim::init(&path).expect("init recorder");

    let (tx, rx) = wyrd_shim::oneshot::channel::<u64>();

    let receiver =
        wyrd_shim::spawn_named(
            "oneshot-waiter",
            async move { rx.await.expect("value arrives") },
        );

    tokio::time::sleep(Duration::from_millis(80)).await;
    tx.send(7).expect("send");
    assert_eq!(receiver.await.expect("join"), 7);
    drop(guard);

    let rec = Recording::open(&path).expect("open recording");
    let parks = parks(&rec);
    assert!(
        parks.contains(&(
            "oneshot-waiter".into(),
            "oneshot::channel".into(),
            "recv".into()
        )),
        "receiver should have parked on the oneshot: {parks:?}"
    );

    let _ = std::fs::remove_file(&path);
}
