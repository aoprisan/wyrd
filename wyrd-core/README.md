# wyrd-core

Ingest [wyrd](https://github.com/aoprisan/wyrd) recordings into SQLite and
answer async causality questions over them.

- `world_state(t)` — fold events up to a timestamp into per-task status
  (Running / Idle / Parked / Done) and per-resource holder / permits / depth.
- `why_blocked(task, t)` — walk the park → resource → holder chain, detecting
  **deadlock cycles**.
- `stats` — task counts, poll-time percentiles, longest parks, channel depths.
- `why_slow(task, t)` — **causal latency attribution**: decompose a task's
  lifetime into own poll time, resource waits (each blamed on the holder,
  with what the holder was doing during the wait), timer waits, scheduler
  lag (woken → polled, from tokio's waker events), and idle.
- `diff(baseline, current)` — **regression detection** between two runs:
  align tasks/resources by stable identity (name / `Type@file:line`),
  compare per-instance behavior, and report new deadlocks (errors),
  poll/wait regressions and new saturation (warnings), improvements (info).

```rust
let rec = wyrd_core::Recording::open("run.wyrd")?;
let task = rec.resolve_task("worker")?;
let report = rec.why_blocked(task, None)?; // None = end-of-recording
if report.is_deadlock() {
    eprintln!("deadlock: {:#?}", report.chain);
}
# Ok::<(), wyrd_core::CoreError>(())
```

Query results are plain serde-serializable structs (see the `model` module), so
a CLI, MCP server, or TUI can all share them. Reads recordings from either
[`wyrd-weave`](https://crates.io/crates/wyrd-weave) (deep) or
[`wyrd-shim`](https://crates.io/crates/wyrd-shim) (stable).

License: MIT OR Apache-2.0
