//! The `RwLock` shim records contended writes as parks on pure stable Rust.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{parks, temp_path};
use wyrd_core::Recording;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rwlock_writer_parks_behind_reader() {
    let path = temp_path("rwlock");
    let guard = wyrd_shim::init(&path).expect("init recorder");

    let lock = Arc::new(wyrd_shim::RwLock::new(0u64));

    let l = lock.clone();
    let reader = wyrd_shim::spawn_named("reader", async move {
        let g = l.read().await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        drop(g);
    });
    // Let the reader win the lock first.
    tokio::time::sleep(Duration::from_millis(40)).await;

    let l = lock.clone();
    let writer = wyrd_shim::spawn_named("writer", async move {
        let mut g = l.write().await;
        *g += 1;
    });

    let _ = writer.await;
    let _ = reader.await;
    drop(guard);

    let rec = Recording::open(&path).expect("open recording");
    let parks = parks(&rec);
    assert!(
        parks.contains(&("writer".into(), "RwLock".into(), "write".into())),
        "writer should have parked on the RwLock: {parks:?}"
    );
    // The reader acquired uncontended: no park for it on the lock.
    assert!(
        !parks.iter().any(|(t, r, _)| t == "reader" && r == "RwLock"),
        "reader must not park: {parks:?}"
    );

    // The lock reports itself free once every guard has dropped.
    let world = rec.world_state(None).expect("world_state");
    let lock_state = world
        .resources
        .iter()
        .find(|r| r.ident.concrete_type == "RwLock")
        .expect("rwlock in world state");
    assert_eq!(lock_state.locked, Some(false), "all guards dropped");

    let _ = std::fs::remove_file(&path);
}
