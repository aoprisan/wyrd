---
name: wyrd-debug
description: Diagnose stuck, deadlocked, or slow tokio applications with wyrd. Use when the user reports a hung/deadlocked async app, mutex contention, channel backpressure, asks "why is this task stuck?" or "why is this slow?", wants two runs compared for async regressions, or when a .wyrd recording file needs inspecting.
---

# Debugging tokio apps with wyrd

wyrd answers *why is an async task stuck?* by recording tokio's instrumentation
to a `.wyrd` file, then walking the park → resource → holder chain: which task
waits on which mutex/channel/timer, who holds it, and whether the waits form a
deadlock cycle.

## Workflow

1. **Get a recording** (skip if the user already has a `.wyrd` file).
2. **Inspect it** — prefer the `wyrd` MCP tools (`stats`, `why_blocked`,
   `world_state`); the `wyrd` CLI is the fallback.
3. **Map the outcome to a fix** (table below), citing the `file:line`
   locations from the report.

## 1. Producing a recording

Two producers, one format — pick by whether `tokio_unstable` is acceptable:

**Deep + universal (`wyrd-weave`)** — sees every task/resource, zero code
changes beyond installing the layer, but the *instrumented app* must be built
with `RUSTFLAGS="--cfg tokio_unstable"` and tokio's `"tracing"` feature:

```rust
let (layer, guard) = wyrd_weave::WeaveLayer::builder()
    .file("run.wyrd").build()?;
tracing_subscriber::registry().with(layer).init();
// ... run the workload ...
drop(guard); // flush & finalize
```

**Stable + scoped (`wyrd-shim`)** — no RUSTFLAGS, pure stable Rust, but only
covers what you route through the wrappers:

```rust
let _guard = wyrd_shim::init("run.wyrd")?;
// swap tokio::spawn / tokio::sync::Mutex / tokio::sync::mpsc for wyrd_shim::*
```

To demo without touching user code, this repo's examples produce recordings:

```console
$ cargo run -p wyrd-shim --example deadlock -- run.wyrd        # stable, no flags
$ RUSTFLAGS="--cfg tokio_unstable" cargo run -p wyrd-demo -- --scenario deadlock --record run.wyrd
```

If a recording is empty or reports nothing, the usual cause is a missing
`tokio_unstable` cfg or tokio built without the `"tracing"` feature.

## 2. Inspecting a recording

The repo's `.mcp.json` registers the `wyrd` MCP server (`cargo run -p
wyrd-mcp`), which exposes six tools. All but `diff` take a `recording` path;
timestamps are ns since recording start.

| tool | use it to |
|------|-----------|
| `lint` | triage first: deadlocks (errors), blocking-in-async long polls, long non-timer parks, saturated channels — with tunable `long_poll_ms` / `long_park_ms` |
| `stats` | orient: duration, task count, poll percentiles, longest parks, peak channel depths |
| `why_blocked` | explain one task's blockage; omit `task` to auto-pick the most interesting parked task, or pass a task name / span id from `world_state` |
| `why_slow` | attribute one task's *latency*: own poll time vs resource waits (blamed on the holder, with what the holder was doing) vs timer waits vs scheduler lag; omit `task` to pick the most-parked task |
| `diff` | compare a `baseline` recording against a `current` one by stable task/resource identity: new deadlocks (error), mean poll/wait regressions and new saturation (warnings), improvements (info) |
| `world_state` | list every task (running/idle/parked/done) and resource (holder, locked, permits, depth), optionally at a timestamp `at` |

Start with `lint` — its findings usually *are* the answer. For "why is it
slow?" go straight to `why_slow`; when a wait's holder is itself parked, the
report names the next resource — call `why_slow` on the holder to follow the
chain. For "did my change make things worse?" record both runs and `diff`
them. Use `world_state` with `at` to replay how a situation developed.

CLI fallback (same queries, `--json` for structured output):

```console
$ cargo run -p wyrd-cli -- lint run.wyrd [--long-poll-ms MS] [--long-park-ms MS] [--json]
$ cargo run -p wyrd-cli -- why-blocked run.wyrd [--task NAME|ID] [--at NS] [--json]
$ cargo run -p wyrd-cli -- why-slow run.wyrd [--task NAME|ID] [--at NS] [--top N] [--json]
$ cargo run -p wyrd-cli -- diff baseline.wyrd current.wyrd [--ratio R] [--floor-ms MS] [--json]
$ cargo run -p wyrd-cli -- stats run.wyrd [--top N] [--json]
```

`why-blocked` exits 2 when it detects a deadlock; `lint` exits 2 on errors
(deadlocks) and 1 on warnings — useful in scripts/tests. To monitor a *running*
app instead of a finished recording, `wyrd watch run.wyrd [--stuck-ms MS]
[--for SECS] [--json]` tails the growing file and alerts (full why-blocked
chain) when a task gets stuck, exiting 2 the moment a deadlock forms.

## 3. Reading a `why_blocked` report

The report has a `chain` (each hop: task → op → resource → since/wait →
holder) and an `outcome`:

| outcome | meaning | typical fix |
|---------|---------|-------------|
| `deadlock` (with `cycle`) | tasks each hold what the next needs | enforce a lock ordering, or merge the mutexes |
| `active_holder` | chain ends at a running/idle task that hasn't released | holder does slow work (or `.await`s) while holding the lock — shrink the critical section |
| `resource_root` | chain ends at a resource nobody holds: full channel, timer, semaphore | backpressure: size the channel, add consumers; or it's a legitimate timer wait |
| `not_blocked` | the task isn't parked | pick another task (see `world_state` for parked ones) |

The classic web-server smell: request tasks all share one spawn location, so
the *resources* pinpoint the bug — e.g. a `Mutex@src/main.rs:47` held across a
`Sleep@src/main.rs:40` `.await` means a lock held across an await point.
