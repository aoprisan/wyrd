# wyrd

**Async causality inspection for tokio applications.** wyrd records what
tokio's own instrumentation knows about your tasks and resources, then lets you
ask *why is this task stuck?* — walking the park → resource → holder chain and
naming deadlocks.

> Phases 1 + 2: the instrumentation layer, event recording, and a one-shot
> `wyrd why-blocked` / `wyrd stats` CLI. No TUI yet.

## Workspace

| crate | what it is |
|-------|------------|
| [`wyrd-weave`](wyrd-weave) | a `tracing_subscriber::Layer` that normalizes tokio's internal spans/events into a compact causality event stream, written to disk on a dedicated writer thread. |
| [`wyrd-core`](wyrd-core) | ingests recordings into SQLite; world-state fold + `why_blocked` / `stats` queries returning serde structs. |
| [`wyrd-cli`](wyrd-cli) | the `wyrd` binary: `wyrd why-blocked <recording>` and `wyrd stats <recording>`. |
| [`examples/demo`](examples/demo) | a tokio app exhibiting a spawn tree, mutex contention, mpsc backpressure, and an intentional two-mutex deadlock. |

## ⚠️ Instrumented apps require `tokio_unstable`

tokio only emits the spans/events wyrd relies on when built with **both**:

- `RUSTFLAGS="--cfg tokio_unstable"`, and
- tokio's `"tracing"` feature.

Without these, the `WeaveLayer` records nothing useful. wyrd-core and wyrd-cli
themselves do not need the flag — only the program you are instrumenting does.

## Quick start

Record the demo's deadlock and explain it:

```console
$ RUSTFLAGS="--cfg tokio_unstable" \
    cargo run -p wyrd-demo -- --scenario deadlock --record run.wyrd

$ cargo run -p wyrd-cli -- why-blocked run.wyrd
⛔ DEADLOCK — deadlock-ab is in a 2-task cycle:
  deadlock-ab  --[poll_acquire, parked 559.2ms]-->  Mutex@examples/demo/src/main.rs:171  (held by deadlock-ba)
  ↳ deadlock-ba  --[poll_acquire, parked 559.2ms]-->  Mutex@examples/demo/src/main.rs:170  (held by deadlock-ab)

   cycle: deadlock-ab → deadlock-ba → (back to start)
```

`why-blocked` exits `2` on a detected deadlock. Add `--json` for structured
output, `--task <name|id>` to pick a task, `--at <ns>` to evaluate at a specific
time (default: end-of-recording).

```console
$ cargo run -p wyrd-cli -- stats run.wyrd
recording span : 707.5ms
tasks          : 12
poll time      : n=48 p50=91.1µs p90=242.1µs p99=706.9ms max=706.9ms
longest parks  : ...
channel depths : Semaphore@... peak 2/2
```

## Instrumenting your own app

```rust
use tracing_subscriber::prelude::*;

fn main() {
    let (layer, guard) = wyrd_weave::WeaveLayer::builder()
        .file("run.wyrd")
        .build()
        .expect("open recording");
    tracing_subscriber::registry().with(layer).init();

    // ... build the runtime and run your workload ...

    drop(guard); // flush & finalize the recording
}
```

## How task attribution works

tokio's `poll_op` and `waker` events reach only the *resource* through their
explicit span chain — never the task. wyrd-weave recovers the task from a
per-thread stack of entered `runtime.spawn` spans: the innermost one is the task
being polled on that thread. It also collapses a `Mutex`'s internal `Semaphore`
into the mutex, and skips tokio's cooperative-budget `poll_acquire` (the first
of two emitted per poll) so holder tracking records the real acquirer.

## Design notes

- No `unwrap` in library code; errors are `thiserror` enums.
- Recordings are versioned, length-prefixed `postcard` frames (see
  `wyrd-weave/src/format.rs`); decode one with
  `cargo run -p wyrd-weave --example dump -- run.wyrd`.
- wyrd's own diagnostics are gated behind each crate's `diag` feature.
- Query results are plain serde structs so a future MCP server and TUI can share
  them.

## Testing

```console
$ cargo test --workspace                              # unit + fold tests
$ RUSTFLAGS="--cfg tokio_unstable" cargo test --workspace   # + end-to-end deadlock integration test
```

The integration test (`examples/demo/tests/deadlock.rs`) runs the demo's
deadlock scenario under a watchdog, records it, and asserts `why_blocked`
reports the 2-cycle with both mutexes named by source location. It is a no-op
without `tokio_unstable`.

## License

MIT OR Apache-2.0.
