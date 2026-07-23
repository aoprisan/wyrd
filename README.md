# wyrd

**Async causality inspection for tokio applications.** wyrd records what
tokio's own instrumentation knows about your tasks and resources, then lets you
ask *why is this task stuck?* — walking the park → resource → holder chain and
naming deadlocks — *where did this task's time go?* (`wyrd why-slow`: latency
attribution that blames each wait on its holder), *did my change make
async behavior worse?* (`wyrd diff`: regression verdicts between two runs) —
and, uniquely, *what deadlock is hiding in this run that passed?*
(`wyrd predict`: lock-order-inversion detection from clean recordings, and
`wyrd hunt`: a seeded concurrency fuzzer that provokes the interleavings your
tests never hit).

> Phases 1–5: the instrumentation layer, event recording, one-shot
> `wyrd why-blocked` / `wyrd stats` / `wyrd lint` commands, an interactive
> `wyrd tui` for browsing a recording (stats, tasks, spawn tree, resources,
> why-blocked) with a scrubbable time cursor, a headless `wyrd watch`
> for CI-style live monitoring, causal latency attribution (`wyrd why-slow`),
> run-over-run regression diffing (`wyrd diff`), potential-deadlock
> prediction from passing runs (`wyrd predict`), and schedule-perturbation
> fuzzing (`wyrd hunt` + wyrd-shim chaos mode).

## Workspace

| crate | what it is |
|-------|------------|
| [`wyrd-weave`](wyrd-weave) | a `tracing_subscriber::Layer` that normalizes tokio's internal spans/events into a compact causality event stream, written to disk on a dedicated writer thread. |
| [`wyrd-core`](wyrd-core) | ingests recordings into SQLite; world-state fold + `why_blocked` / `stats` queries returning serde structs. |
| [`wyrd-cli`](wyrd-cli) | the `wyrd` binary: `why-blocked`, `why-slow`, `diff`, `stats`, `lint`, `predict`, `hunt`, `watch`, and `wyrd tui <recording>` (interactive [ratatui](https://ratatui.rs) viewer). |
| [`wyrd-mcp`](wyrd-mcp) | an MCP server exposing the same queries (`why_blocked`, `why_slow`, `diff`, `stats`, `lint`, `predict`, `world_state`) to AI agents over stdio (see [below](#asking-an-ai-agent-mcp--claude-code)). |
| [`wyrd-shim`](wyrd-shim) | **stable-Rust** `spawn` / `Mutex` / `mpsc` wrappers that record the same events **without** `tokio_unstable`, plus env-driven [chaos mode](#hunting-latent-races-wyrd-hunt--chaos-mode) (see below). |
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

## Explaining latency (`wyrd why-slow`)

`why-blocked` answers *why is this task stuck?*; `why-slow` answers *where did
this task's time actually go?* It decomposes a task's lifetime into its own
poll time (compute), waits on resources — each **blamed on the holder**, with
what the holder was doing during your wait — intentional timer waits,
scheduler lag (woken but not yet polled), and idle:

```console
$ wyrd why-slow run.wyrd --task mutex-waiter
task mutex-waiter: 31.2ms from spawn (+911.4µs) to end

  own polls         511.6µs    1.6%  (4 polls)
  resource wait      24.3ms   77.9%
  timer wait          6.2ms   19.7%
  scheduler lag     133.2µs    0.4%  (woken → polled)
  idle/other        106.5µs    0.3%

top waits:
  1.     24.3ms  Mutex@examples/demo/src/main.rs:131 [poll_acquire] at +7.6ms
       └ +73.9µs scheduler lag after the wake
       └ held by mutex-holder: itself parked 24.2ms on Sleep@examples/demo/src/main.rs:135 — the chain continues there
```

That last line is the classic bug caught red-handed: the holder kept the lock
across a `sleep().await`. When a wait's holder is itself parked, the report
names the next resource — run `why-slow` on the holder to follow the chain.
The five buckets partition the lifetime exactly, so nothing hides. Omit
`--task` to auto-pick the most-parked task; `--json` for the structured
report. Scheduler-lag attribution comes from tokio's waker events: the gap
between a task's wake and its next poll is executor delay, not the resource's
fault.

## Catching regressions between runs (`wyrd diff`)

`wyrd diff` compares two recordings — a known-good baseline and the run to
judge — and reports *behavioral* regressions. Span ids mean nothing across
processes, so tasks are aligned by stable identity (their name, else
`kind@file:line`) and resources by `Type@file:line`, then compared
per-instance (2× the tasks doing 2× the work is not a regression):

```console
$ wyrd diff baseline.wyrd current.wyrd
baseline: 32.5ms span, 5 tasks, 64.3ms poll, 48.8ms wait
current : 602.1ms span, 5 tasks, 1205.5ms poll, 2236.3ms wait, 1 deadlock(s)
⛔ error: NEW DEADLOCK — cycle: deadlock-ab → deadlock-ba → (back to start); not present in baseline
⚠ regression: hasher mean poll time 1.2ms → 19.8ms (×16.5)
✓ note: deadlock fixed — baseline cycle a → b is gone
```

A metric regresses when it grows by more than `--ratio` (default ×1.5) **and**
more than `--floor-ms` (default 1ms — silences noise on tiny values). Exit
codes gate CI: `2` on a new deadlock, `1` on regressions (poll/wait growth,
newly saturated channels), `0` when clean or improved. Record a baseline on
`main`, record on the PR, `wyrd diff` the pair — async behavior is now a
reviewable, gateable artifact.

## Predicting deadlocks that didn't happen (`wyrd predict`)

Everything above is forensic: it explains a failure the recording already
contains. `wyrd predict` is proactive — it finds the deadlock hiding in a run
that **passed**. From the recorded holder state it reconstructs which locks
each task *held while acquiring* others, builds the lock-order graph, and
reports cycles: tasks acquiring the same locks in conflicting orders. That is
a deadlock waiting for the right interleaving, even if this run — and every
run your test suite will ever do — sails through:

```console
$ cargo run -p wyrd-shim --example inversion -- run.wyrd
completed cleanly (the inversion is still latent — run `wyrd predict`)

$ wyrd predict run.wyrd
⚠ POTENTIAL DEADLOCK — 2-lock cycle acquired in conflicting orders (did not fire in this run):
  Mutex@wyrd-shim/examples/inversion.rs:33 → Mutex@wyrd-shim/examples/inversion.rs:34
    worker-ab held Mutex@…/inversion.rs:33 while taking Mutex@…/inversion.rs:34 [acquire] at +162.8µs
    worker-ba held Mutex@…/inversion.rs:34 while taking Mutex@…/inversion.rs:33 [acquire] at +2.4ms

analyzed 160 acquisitions across 2 lock(s); 2 order edge(s); 1 cycle(s) reported, …
fix: make every task acquire these locks in one canonical order (or merge them / guard both orders behind a common gate lock).
```

This is the classic lock-order-inversion ("Goodlock") analysis, with the two
standard false-positive filters: cycles whose every edge comes from a single
task are suppressed (it can't deadlock with itself), and cycles serialized by
a common *gate lock* held across all witnesses are suppressed (the order
conflict can never manifest). Cycles that *did* deadlock in the recording are
marked `observed`. Exit codes gate CI: `2` observed, `1` latent, `0` clean —
run it on the recording of any passing test and fail the build on lock-order
inversions nobody has hit yet.

## Hunting latent races (`wyrd hunt` + chaos mode)

`predict` finds inversions the schedule happened to expose. `wyrd hunt` goes
looking for trouble: it runs your (shim-instrumented) binary many times under
**chaos mode** — seeded schedule perturbation that injects tiny randomized
delays right before lock/channel acquisitions and at task startup, widening
the race windows your scheduler normally jumps over — with a hang watchdog,
then analyzes every run's recording and aggregates findings across seeds:

```console
$ wyrd hunt --runs 16 -- target/debug/examples/inversion
  seed    1          exit 3   5010.2ms  ⛔ 1 deadlock(s)
  seed    2          exit 0    111.4ms  ⚠ 1 latent cycle(s)
  ...

hunted `target/debug/examples/inversion` × 16 runs: 0 hung, 9 deadlocked, 1 distinct cycle(s)

⛔ cycle DEADLOCKED under seed(s) [1, 3, 7, …]: Mutex@…/inversion.rs:33 ↔ Mutex@…/inversion.rs:34
    • worker-ab parked on Mutex@…/inversion.rs:34
    • worker-ba parked on Mutex@…/inversion.rs:33
    ↳ inspect: wyrd why-blocked /tmp/wyrd-hunt-1234/hunt-1.wyrd   (or wyrd predict …)

verdict: reproduced — recordings for failing seeds kept in /tmp/wyrd-hunt-1234
```

Chaos mode is configured entirely through the environment — any
shim-instrumented binary becomes fuzzable without a rebuild:

| variable | meaning | default |
|---|---|---|
| `WYRD_CHAOS` | `1`/`true`/`on` enables chaos | off |
| `WYRD_CHAOS_SEED` | seed for the delay stream (a seed that provokes a bug keeps provoking it) | `0x5EED` |
| `WYRD_CHAOS_PROB` | per-site injection probability | `0.25` |
| `WYRD_CHAOS_MAX_DELAY_US` | max injected delay, µs (0 = yield only) | `500` |

`hunt` sets these per run (seed = `--seed-start + index`) plus `WYRD_RECORD`
with the per-seed recording path — have the target call
`wyrd_shim::init_from_env()` to pick both up. Runs that hang are killed by the
watchdog (`--timeout-s`) and counted as the strongest failure signal; their
recordings are still analyzable, because the writer thread flushes on idle
precisely so a deadlocked-then-killed process leaves its evidence on disk.
Findings are keyed by the resources' stable `Type@file:line` identity, so the
same cycle observed under different seeds folds into one report line. Exit
codes: `2` if any run hung or deadlocked, `1` if only latent cycles were
found, `0` clean. Where [loom](https://github.com/tokio-rs/loom) exhaustively
model-checks code rewritten against its simulated types, `hunt` fuzzes your
*real* binary on the *real* tokio runtime — no test harness rewrite, just the
shim wrappers you already record with.

## Linting a recording (`wyrd lint`)

`wyrd lint` distills the same folds into triaged findings — the "is anything
wrong with this app?" command, built for CI gates:

```console
$ wyrd lint run.wyrd
⛔ error: deadlock — 2-task cycle: deadlock-ab → deadlock-ba → (back to start)
    • Mutex@examples/demo/src/main.rs:171
    • Mutex@examples/demo/src/main.rs:170
⚠ warning: long poll — hasher spent up to 18.2ms inside a single poll (3 polls
   over the 1.0ms threshold); blocking or heavy compute in async code
⚠ warning: saturated channel — mpsc::channel@src/main.rs:88 peaked at 2/2; …

3 findings (1 error, 2 warnings)
```

It flags **deadlocks** (errors), **blocking-in-async** (any single poll over
`--long-poll-ms`, default 1ms — including a task still stuck *inside* a poll;
`spawn_blocking` tasks are exempt, they block by design),
**long parks** on non-timer resources (`--long-park-ms`, default 1000ms;
`Sleep`/`Interval` waits are intentional and never flagged), and **saturated
channels** (a bounded channel/semaphore that hit capacity). Exit codes gate
scripts: `2` on any error, `1` on warnings only, `0` when clean. `--json`
emits the full structured report.

## Watching a live app headlessly (`wyrd watch`)

`wyrd tui --follow` is for humans; `wyrd watch` is the same live tailing for
CI jobs, logs, and terminals without a TTY. It re-folds the growing recording
on an interval and prints a full why-blocked chain the moment a task has been
parked (on a non-timer resource) beyond a threshold — and exits `2` the moment
a deadlock forms:

```console
$ myapp &                                    # keeps appending to run.wyrd
$ wyrd watch run.wyrd --stuck-ms 500 --for 30
--- DEADLOCK ---
⛔ DEADLOCK — worker-ab is in a 2-task cycle:
  ...
$ echo $?
2
```

Each stuck-task episode alerts once (no spam while it stays stuck); `--for
SECS` bounds the watch for CI (exit `1` if anything got stuck, `0` if all
clear), and `--json` emits newline-delimited JSON alerts for log pipelines.
Like follow mode, the recorded program is never touched.

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
- **Tree** — the task spawn tree at the cursor time: who spawned whom, each
  task with its live status, so a stuck subtree is visible at a glance.
- **Resources** — each resource with its presumed holder, lock state, and
  channel depth.
- **Why-blocked** — the selected task's park → resource → holder chain, with
  deadlock cycles named and highlighted (the `wyrd why-blocked` view, live).

A **time cursor** runs along the bottom: <kbd>[</kbd> / <kbd>]</kbd> scrub
backward/forward, <kbd>g</kbd> / <kbd>G</kbd> jump to the start/end of the
recording. The Tasks, Tree, Resources, and Why-blocked views all re-fold to that
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
so an agent can inspect recordings itself: `lint` for triaged findings,
`stats` to orient, `why_blocked` to walk the park → holder chain (deadlock
cycles included), `why_slow` to attribute a task's latency (and follow the
blame chain holder by holder), `diff` to judge a run against a baseline,
`predict` to surface latent lock-order inversions even in a clean run, and
`world_state` to snapshot every task/resource at a
timestamp. Results carry both readable text
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
| Coverage | every task/resource, incl. inside dependencies, zero code changes | only what you route through `wyrd_shim::{spawn, Mutex, RwLock, Semaphore, Notify, mpsc, oneshot}` |
| Source locations | from tokio (sometimes missing, e.g. mpsc) | exact `file:line:col` via `#[track_caller]` |
| Holder signal | inferred from `poll_acquire` readiness | observed directly (try-lock → acquire → guard drop) |

Use the shim when you can't (or won't) enable `tokio_unstable` and are willing
to swap the `tokio::sync` primitives (`Mutex`, `RwLock`, `Semaphore`, `Notify`,
`mpsc`, `oneshot`) and `tokio::spawn` for the wyrd wrappers; use the layer when
you need to see into code you don't control.

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
