//! An axum server whose `/contended` handler holds a shared `tokio::sync::Mutex`
//! while it "works", so concurrent requests pile up behind the holder —
//! instrumented with wyrd so you can see the contention with `wyrd why-blocked`.
//!
//! Build with tokio's instrumentation enabled (see `.cargo/config.toml` or set
//! `RUSTFLAGS="--cfg tokio_unstable"`), then:
//!
//! ```text
//! # self-driving: fires concurrent requests, freezes the recording mid-flight
//! cargo run -p wyrd-axum-example -- --record axum.wyrd
//! wyrd why-blocked axum.wyrd
//! wyrd stats axum.wyrd
//!
//! # or run a real server and drive it yourself, Ctrl-C to flush:
//! cargo run -p wyrd-axum-example -- serve --port 3000 --record axum.wyrd
//! # in another shell: hit http://localhost:3000/contended a few times at once
//! ```

use std::sync::Arc;
use std::time::Duration;

use axum::{extract::State, routing::get, Router};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing_subscriber::prelude::*;

/// How long the handler holds the mutex while "working".
const HOLD: Duration = Duration::from_millis(400);

#[derive(Clone)]
struct AppState {
    /// A shared resource every request must lock — the contention point.
    counter: Arc<tokio::sync::Mutex<u64>>,
}

async fn contended(State(state): State<AppState>) -> String {
    // Requests serialize here: only one holds the lock at a time, and it holds
    // it for HOLD while it works, so everyone else parks waiting.
    let mut n = state.counter.lock().await;
    tokio::time::sleep(HOLD).await;
    *n += 1;
    format!("count = {}\n", *n)
}

fn app() -> Router {
    let state = AppState {
        counter: Arc::new(tokio::sync::Mutex::new(0)),
    };
    Router::new()
        .route("/contended", get(contended))
        .with_state(state)
}

#[derive(Parser)]
#[command(name = "wyrd-axum-example", about = "axum + wyrd contention demo")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Where to write the recording.
    #[arg(long, default_value = "axum.wyrd", global = true)]
    record: String,
}

#[derive(Subcommand)]
enum Command {
    /// Fire N concurrent requests, freeze the recording while they contend, exit.
    Load {
        /// Number of concurrent requests.
        #[arg(long, default_value_t = 6)]
        requests: usize,
        /// Freeze the recording this many ms after firing (< HOLD to catch parks).
        #[arg(long, default_value_t = 150)]
        freeze_after_ms: u64,
    },
    /// Run a real server until Ctrl-C, then flush.
    Serve {
        #[arg(long, default_value_t = 3000)]
        port: u16,
    },
}

fn main() {
    let cli = Cli::parse();

    // 1. Install the wyrd layer BEFORE the runtime runs.
    let (layer, guard) = wyrd_weave::WeaveLayer::builder()
        .file(&cli.record)
        .build()
        .expect("open recording");
    tracing_subscriber::registry().with(layer).init();

    // 2. Build the runtime AFTER the subscriber is set.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build runtime");

    match cli.command.unwrap_or(Command::Load {
        requests: 6,
        freeze_after_ms: 150,
    }) {
        Command::Load {
            requests,
            freeze_after_ms,
        } => {
            runtime.block_on(run_load(requests, freeze_after_ms));
            // Freeze the recording with requests still parked, then exit hard
            // (in-flight tasks never resolve within the window).
            drop(guard);
            println!(
                "wrote {}: run `wyrd why-blocked {}` and `wyrd stats {}`",
                cli.record, cli.record, cli.record
            );
            std::process::exit(0);
        }
        Command::Serve { port } => {
            runtime.block_on(run_serve(port));
            drop(runtime);
            drop(guard);
            println!("wrote {}", cli.record);
        }
    }
}

/// Self-driving load: start the server, fire concurrent requests, and return
/// while they are still contending so the recording freezes mid-park.
async fn run_load(requests: usize, freeze_after_ms: u64) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app()).await;
    });

    for _ in 0..requests {
        tokio::spawn(async move {
            let _ = http_get(addr, "/contended").await;
        });
    }

    // Return mid-flight: one request holds the lock, the rest are parked on it.
    tokio::time::sleep(Duration::from_millis(freeze_after_ms)).await;
}

async fn run_serve(port: u16) {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    println!("serving http://0.0.0.0:{port}/contended (Ctrl-C to flush recording)");
    axum::serve(listener, app())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .unwrap();
}

/// Minimal HTTP/1.1 GET over a raw TCP socket (avoids pulling in an HTTP client).
async fn http_get(addr: std::net::SocketAddr, path: &str) -> std::io::Result<()> {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    Ok(())
}
