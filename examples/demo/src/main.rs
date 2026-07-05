//! wyrd-demo: a tokio program that deliberately exhibits the situations wyrd
//! is built to explain — a spawn tree, mutex contention, bounded-mpsc
//! backpressure, and a two-task / two-mutex deadlock.
//!
//! Build & run with tokio's tracing instrumentation enabled:
//!
//! ```text
//! RUSTFLAGS="--cfg tokio_unstable" \
//!   cargo run -p wyrd-demo -- --scenario deadlock --record run.wyrd
//! ```
//!
//! The deadlock scenario never completes, so it runs under a watchdog: after
//! `--watchdog-ms` the stuck tasks are aborted and the recording is finalized.

use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use tokio::sync::{mpsc, Mutex};
use tracing_subscriber::prelude::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Scenario {
    /// Run every scenario in sequence, deadlock last.
    All,
    /// A parent task spawning a small tree of children.
    Spawn,
    /// Two tasks contending for one mutex.
    Contention,
    /// A bounded mpsc channel with a slow consumer (backpressure).
    Mpsc,
    /// Two tasks acquiring two mutexes in opposite order (deadlock).
    Deadlock,
}

#[derive(Parser, Debug)]
#[command(name = "wyrd-demo", about = "tokio causality demo workloads for wyrd")]
struct Args {
    /// Which workload to run.
    #[arg(long, value_enum, default_value = "all")]
    scenario: Scenario,

    /// Where to write the wyrd recording.
    #[arg(long, default_value = "wyrd-demo.wyrd")]
    record: String,

    /// Watchdog: abort stuck tasks after this many milliseconds (deadlock).
    #[arg(long, default_value_t = 600)]
    watchdog_ms: u64,
}

fn main() {
    let args = Args::parse();

    let (layer, guard) = wyrd_weave::WeaveLayer::builder()
        .file(&args.record)
        .build()
        .expect("failed to open recording file");
    tracing_subscriber::registry().with(layer).init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    runtime.block_on(run(args));

    // Shut the runtime down first so task/resource spans close (emitting their
    // end events), then drop the guard to flush and finalize the recording.
    drop(runtime);
    let dropped = guard.dropped_events();
    drop(guard);
    if dropped > 0 {
        eprintln!("warning: {dropped} events dropped due to queue overflow");
    }
}

async fn run(args: Args) {
    match args.scenario {
        Scenario::Spawn => spawn_tree().await,
        Scenario::Contention => contention().await,
        Scenario::Mpsc => mpsc_backpressure().await,
        Scenario::Deadlock => deadlock(args.watchdog_ms).await,
        Scenario::All => {
            spawn_tree().await;
            contention().await;
            mpsc_backpressure().await;
            deadlock(args.watchdog_ms).await;
        }
    }
    println!("scenario {:?} complete", args.scenario);
}

/// A parent task spawning two children. Exercises the spawn tree. Uses a plain
/// `tokio::spawn` for one child to exercise wyrd's loc-based identity fallback.
async fn spawn_tree() {
    let parent = spawn_named("parent", async {
        let mut kids = Vec::new();
        // Named child (unstable task::Builder API).
        kids.push(spawn_named("child-named", async {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }));
        // Unnamed child: wyrd falls back to source location for identity.
        kids.push(tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }));
        for k in kids {
            let _ = k.await;
        }
    });
    let _ = parent.await;
}

/// Two tasks fight over one mutex; the holder sleeps while holding it.
async fn contention() {
    let lock = Arc::new(Mutex::new(0u64));
    let l1 = lock.clone();
    let holder = spawn_named("mutex-holder", async move {
        let mut g = l1.lock().await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        *g += 1;
    });
    let l2 = lock.clone();
    let waiter = spawn_named("mutex-waiter", async move {
        tokio::time::sleep(Duration::from_millis(5)).await; // lose the race
        let mut g = l2.lock().await; // parks here until the holder releases
        *g += 1;
    });
    let _ = tokio::join!(holder, waiter);
}

/// A bounded mpsc channel (capacity 2) with a slow consumer: the producer
/// blocks on `send` once the channel fills.
async fn mpsc_backpressure() {
    let (tx, mut rx) = mpsc::channel::<u64>(2);
    let producer = spawn_named("producer", async move {
        for i in 0..6 {
            // Blocks when the channel is full (backpressure).
            if tx.send(i).await.is_err() {
                break;
            }
        }
    });
    let consumer = spawn_named("consumer", async move {
        while let Some(_v) = rx.recv().await {
            tokio::time::sleep(Duration::from_millis(8)).await; // slow
        }
    });
    let _ = tokio::join!(producer, consumer);
}

/// The classic AB / BA deadlock. Never resolves on its own, so a watchdog
/// aborts the tasks after `watchdog_ms`.
async fn deadlock(watchdog_ms: u64) {
    let mutex_a = Arc::new(Mutex::new(0u64));
    let mutex_b = Arc::new(Mutex::new(0u64));

    let (a1, b1) = (mutex_a.clone(), mutex_b.clone());
    let task1 = spawn_named("deadlock-ab", async move {
        let _ga = a1.lock().await;
        tokio::time::sleep(Duration::from_millis(40)).await; // let the other grab B
        let _gb = b1.lock().await; // parks forever
        unreachable!("deadlock-ab should never acquire B");
    });

    let (a2, b2) = (mutex_a.clone(), mutex_b.clone());
    let task2 = spawn_named("deadlock-ba", async move {
        let _gb = b2.lock().await;
        tokio::time::sleep(Duration::from_millis(40)).await; // let the other grab A
        let _ga = a2.lock().await; // parks forever
        unreachable!("deadlock-ba should never acquire A");
    });

    // Watchdog: give the deadlock time to form, then abort the stuck tasks.
    tokio::time::sleep(Duration::from_millis(watchdog_ms)).await;
    task1.abort();
    task2.abort();
    let _ = task1.await;
    let _ = task2.await;
}

/// Spawn a named task via the unstable `task::Builder` API, degrading to a
/// panic only if the runtime is shutting down (nothing sensible to return).
fn spawn_named<F>(name: &str, fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    match tokio::task::Builder::new().name(name).spawn(fut) {
        Ok(handle) => handle,
        Err(e) => panic!("failed to spawn task {name}: {e}"),
    }
}
