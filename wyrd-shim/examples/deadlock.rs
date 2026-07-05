//! Reproduce a two-mutex deadlock and record it on pure stable Rust (no
//! `tokio_unstable`), then inspect it with:
//!
//! ```text
//! cargo run -p wyrd-shim --example deadlock -- run.wyrd
//! cargo run -p wyrd-cli  -- why-blocked run.wyrd
//! ```

use std::sync::Arc;
use std::time::Duration;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "run.wyrd".to_string());
    let guard = wyrd_shim::init(&path).expect("init recording");

    let mutex_a = Arc::new(wyrd_shim::Mutex::new(0u64));
    let mutex_b = Arc::new(wyrd_shim::Mutex::new(0u64));

    let (a1, b1) = (mutex_a.clone(), mutex_b.clone());
    let _ab = wyrd_shim::spawn_named("worker-ab", async move {
        let _ga = a1.lock().await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        let _gb = b1.lock().await;
    });

    let (a2, b2) = (mutex_a.clone(), mutex_b.clone());
    let _ba = wyrd_shim::spawn_named("worker-ba", async move {
        let _gb = b2.lock().await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        let _ga = a2.lock().await;
    });

    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(guard); // freeze the recording with both tasks deadlocked
    println!("wrote {path}");
}
