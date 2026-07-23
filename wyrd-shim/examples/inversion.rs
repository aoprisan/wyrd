//! A lock-order inversion that *usually* completes cleanly — the bug wyrd's
//! forensic commands can't see, and `wyrd predict` / `wyrd hunt` exist for.
//!
//! `worker-ab` takes A then B; `worker-ba` takes B then A. Their iterations
//! are phase-shifted so the hold windows rarely overlap: the program almost
//! always finishes and every test on it passes. The deadlock is still there,
//! one unlucky interleaving away.
//!
//! ```text
//! # a clean run still reveals the latent cycle:
//! cargo run -p wyrd-shim --example inversion -- run.wyrd
//! cargo run -p wyrd-cli  -- predict run.wyrd
//!
//! # hunt for a seed that actually triggers it:
//! cargo run -p wyrd-cli  -- hunt --runs 16 -- \
//!     target/debug/examples/inversion
//! ```
//!
//! The recording path comes from `WYRD_RECORD` (as `wyrd hunt` sets it), or
//! the first CLI argument, or `run.wyrd`. Exits 3 if the deadlock fires.

use std::sync::Arc;
use std::time::Duration;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let guard = match std::env::args().nth(1) {
        Some(path) => wyrd_shim::init(path),
        None => wyrd_shim::init_from_env(),
    }
    .expect("init recording");

    let mutex_a = Arc::new(wyrd_shim::Mutex::new(0u64));
    let mutex_b = Arc::new(wyrd_shim::Mutex::new(0u64));

    const ITERS: usize = 40;

    let (a1, b1) = (mutex_a.clone(), mutex_b.clone());
    let ab = wyrd_shim::spawn_named("worker-ab", async move {
        for _ in 0..ITERS {
            let mut ga = a1.lock().await;
            let mut gb = b1.lock().await;
            *ga += 1;
            *gb += 1;
            drop(gb);
            drop(ga);
            tokio::time::sleep(Duration::from_micros(800)).await;
        }
    });

    let (a2, b2) = (mutex_a.clone(), mutex_b.clone());
    let ba = wyrd_shim::spawn_named("worker-ba", async move {
        // Phase shift: half a period, so the hold windows normally miss.
        tokio::time::sleep(Duration::from_micros(400)).await;
        for _ in 0..ITERS {
            let mut gb = b2.lock().await;
            let mut ga = a2.lock().await;
            *gb += 1;
            *ga += 1;
            drop(ga);
            drop(gb);
            tokio::time::sleep(Duration::from_micros(800)).await;
        }
    });

    let joined = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = ab.await;
        let _ = ba.await;
    })
    .await;

    drop(guard); // flush the recording either way

    match joined {
        Ok(()) => {
            println!("completed cleanly (the inversion is still latent — run `wyrd predict`)")
        }
        Err(_) => {
            println!("DEADLOCKED — the latent inversion fired");
            std::process::exit(3);
        }
    }
}
