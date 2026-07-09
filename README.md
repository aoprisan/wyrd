# wyrd

**Async causality inspection for tokio applications.** wyrd records what
tokio's own instrumentation knows about your tasks and resources, then lets you
ask *why is this task stuck?* — walking the park → resource → holder chain and
naming deadlocks.

> Phases 1 + 2: the instrumentation layer, event recording, one-shot
> `wyrd why-blocked` / `wyrd stats` commands, and an interactive
> `wyrd tui` for browsing a recording (stats, tasks, resources, why-blocked)
> with a scrubbable time cursor.

## Workspace

| crate | what it is |
|-------|------------|
| [`wyrd-weave`](wyrd-weave) | a `tracing_subscriber::Layer` that normalizes tokio's internal spans/events into a compact causality event stream, written to disk on a dedicated writer thread. |
| [`wyrd-core`](wyrd-core) | ingests recordings into SQLite; world-state fold + `why_blocked` / `stats` queries returning serde structs. |
| [`wyrd-cli`](wyrd-cli) | the `wyrd` binary: `wyrd why-blocked <recording>`, `wyrd stats <recording>`, and `wyrd tui <recording>` (interactive [ratatui](https://ratatui.rs) viewer). |
| [`wyrd-mcp`](wyrd-mcp) | an MCP server exposing the same queries (`why_blocked`, `stats`, `world_state`) to AI agents over stdio (see [below](#asking-an-ai-agent-mcp--claude-code)). |
| [`wyrd-shim`](wyrd-shim) | **stable-Rust** `spawn` / `Mutex` / `mpsc` wrappers that record the same events **without** `tokio_unstable` (see below). |
| [`examples/demo`](examples/demo) | a tokio app exhibiting a spawn tree, mutex contention, mpsc backpressure, and an intentional two-mutex deadlock. |
| [`examples/axum`](examples/axum) | an axum server whose handler holds a shared mutex across an `.await`, so requests serialize — self-driving, produces a recording you can inspect. |

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

## Browsing a recording interactively (`wyrd tui`)

For anything larger than a toy, the one-shot commands get tedious — you want to
scan the task list, pick one, and see *why it's stuck* without re-running.
`wyrd tui` opens a recording in a terminal UI:

```console
$ cargo run -p wyrd-cli -- tui run.wyrd
```

- **Stats** — the same overview `wyrd stats` prints (span, poll-time
  percentiles, longest parks, channel depths).
- **Tasks** — every task at the cursor time and its status (running / idle /
  `parked on <resource>` / done). Select one and press <kbd>Enter</kbd> to jump
  to…
- **Resources** — each resource with its presumed holder, lock state, and
  channel depth.
- **Why-blocked** — the selected task's park → resource → holder chain, with
  deadlock cycles named and highlighted (the `wyrd why-blocked` view, live).

A **time cursor** runs along the bottom: <kbd>[</kbd> / <kbd>]</kbd> scrub
backward/forward, <kbd>g</kbd> / <kbd>G</kbd> jump to the start/end of the
recording. The Tasks, Resources, and Why-blocked views all re-fold to that
instant, so you can watch state evolve across the run. <kbd>◂</kbd>/<kbd>▸</kbd>
switch tabs, <kbd>↑</kbd>/<kbd>↓</kbd> move the selection, <kbd>q</kbd> quits.

### Live monitoring with `--follow`

Point the TUI at a recording that's still being written and it tails it like
`tail -f`, re-folding on an interval so you can watch a **running** app:

```console
$ myapp &                          # writes run.wyrd as it goes (weave layer installed)
$ wyrd tui --follow run.wyrd       # ● live — updates as new frames land
```

The header shows <kbd>● live</kbd> while the cursor tracks the growing tail; the
Why-blocked tab auto-surfaces the most-stuck task, so a deadlock appears the
moment its cycle forms. Scrub back with <kbd>[</kbd> to freeze and inspect
(<kbd>⏸ frozen</kbd>); <kbd>G</kbd> snaps back to live.

This is the **zero-added-overhead** design: the recorded program is never
touched — it keeps appending frames exactly as before, and all the folding and
rendering cost lives in this separate viewer process. The trade-offs are honest:
latency is bounded by the recorder's flush cadence, history is whatever the file
holds, and it reads the file — it does not attach to the process or its memory.

## Real-world shape: an axum server

[`examples/axum`](examples/axum) is a self-driving reproduction of a classic web
anti-pattern — a handler that holds a shared mutex across an `.await`:

```console
$ cargo run -p wyrd-axum-example -- load --record axum.wyrd   # needs tokio_unstable
$ wyrd why-blocked axum.wyrd
⏳ ... is blocked; root cause is Sleep@examples/axum/src/main.rs:40 ...:
  task@axum-0.7.9/src/serve.rs:253  --[poll_acquire, parked 148ms]-->  Mutex@examples/axum/src/main.rs:47  (held by task@axum-0.7.9/src/serve.rs:253)
  ↳ task@axum-0.7.9/src/serve.rs:253 --[poll_elapsed, parked 148ms]--> Sleep@examples/axum/src/main.rs:40 (no holder)
```

Read: one request task holds the mutex at `main.rs:47` while sleeping at
`main.rs:40`; the others are stuck behind it. Request tasks are spawned by axum
so they share a source location — the *resources* are what pinpoint the bug.
`load` mode fires concurrent requests and freezes the recording mid-contention;
`serve --port 3000` runs a real server and flushes on Ctrl-C.

## Using wyrd in your own project

**Install the analyzer** (`wyrd` CLI — pure stable, no `tokio_unstable`):

```console
$ cargo install --git https://github.com/aoprisan/wyrd wyrd-cli
$ wyrd --help
```

**Instrument your app**, either:

- *Deep + universal* — add `wyrd-weave` and build with `tokio_unstable` (see
  [above](#-instrumented-apps-require-tokio_unstable)); or
- *Stable + scoped* — add `wyrd-shim` and swap `tokio::spawn` /
  `tokio::sync::Mutex` / `tokio::sync::mpsc` for `wyrd_shim::*`.

```toml
[dependencies]
wyrd-weave = { git = "https://github.com/aoprisan/wyrd" }   # or wyrd-shim
```

Then: run your app → get a `.wyrd` file → `wyrd why-blocked file.wyrd`. The
recording format is stable across the two producers, so the same CLI reads
either. (Not yet published to crates.io; once it is, this becomes
`cargo install wyrd-cli` and `wyrd-weave = "0.1"`.)

## Asking an AI agent (MCP + Claude Code)

`wyrd-mcp` serves the same queries over the
[Model Context Protocol](https://modelcontextprotocol.io) (stdio transport),
so an agent can inspect recordings itself: `stats` to orient, `why_blocked` to
walk the park → holder chain (deadlock cycles included), and `world_state` to
snapshot every task/resource at a timestamp. Results carry both readable text
and `structuredContent` — the same serde structs the CLI prints with `--json`.

This repo ships a [`.mcp.json`](.mcp.json) that registers the server with
Claude Code automatically, plus a skill
([`.claude/skills/wyrd-debug`](.claude/skills/wyrd-debug/SKILL.md)) that
teaches it the record → inspect → fix workflow — so inside this repo you can
just ask *"why is my app stuck?"* and point it at a `.wyrd` file. For other
projects, register the binary yourself:

```console
$ claude mcp add wyrd -- cargo run --quiet --manifest-path /path/to/wyrd/Cargo.toml -p wyrd-mcp
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

## Two producers, one event vocabulary

The normalized [`wyrd_weave::Event`] stream is the stable interface, so the
*producer* is swappable:

```
wyrd-weave  (tokio_unstable tracing Layer) ─┐
                                            ├─→ Event stream → wyrd-core → wyrd
wyrd-shim   (stable wrapper types)  ─────────┘
```

| | `wyrd-weave` (unstable layer) | `wyrd-shim` (stable wrappers) |
|---|---|---|
| Requires `tokio_unstable` | **yes** | **no** |
| Coverage | every task/resource, incl. inside dependencies, zero code changes | only what you route through `wyrd_shim::{spawn, Mutex, mpsc}` |
| Source locations | from tokio (sometimes missing, e.g. mpsc) | exact `file:line:col` via `#[track_caller]` |
| Holder signal | inferred from `poll_acquire` readiness | observed directly (try-lock → acquire → guard drop) |

Use the shim when you can't (or won't) enable `tokio_unstable` and are willing
to swap `tokio::spawn`/`tokio::sync::Mutex`/`tokio::sync::mpsc` for the wyrd
wrappers; use the layer when you need to see into code you don't control.

```rust
let _guard = wyrd_shim::init("run.wyrd").unwrap();       // stable, no RUSTFLAGS
let lock = std::sync::Arc::new(wyrd_shim::Mutex::new(0));
wyrd_shim::spawn(async move { /* ... */ });
```

```console
$ cargo run -p wyrd-shim --example deadlock -- run.wyrd   # note: no tokio_unstable
$ cargo run -p wyrd-cli  -- why-blocked run.wyrd           # same report, same CLI
⛔ DEADLOCK — worker-ab is in a 2-task cycle:
  worker-ab  --[lock, ...]-->  Mutex@wyrd-shim/examples/deadlock.rs:20  (held by worker-ba)
  ↳ worker-ba  --[lock, ...]-->  Mutex@wyrd-shim/examples/deadlock.rs:19  (held by worker-ab)
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
- Query results are plain serde structs, shared verbatim by the CLI, the MCP
  server, and the `wyrd tui` viewer.

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
