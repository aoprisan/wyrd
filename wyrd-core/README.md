# wyrd-core

Ingest [wyrd](https://github.com/aoprisan/wyrd) recordings into SQLite and
answer async causality questions over them.

- `world_state(t)` — fold events up to a timestamp into per-task status
  (Running / Idle / Parked / Done) and per-resource holder / permits / depth.
- `why_blocked(task, t)` — walk the park → resource → holder chain, detecting
  **deadlock cycles**.
- `stats` — task counts, poll-time percentiles, longest parks, channel depths.

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
