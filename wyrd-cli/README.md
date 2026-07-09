# wyrd

The command-line analyzer for [wyrd](https://github.com/aoprisan/wyrd)
recordings — *why is this async task stuck?*

```console
$ cargo install wyrd-cli        # installs the `wyrd` binary (pure stable Rust)

$ wyrd why-blocked run.wyrd
⛔ DEADLOCK — worker-ab is in a 2-task cycle:
  worker-ab  --[lock, parked 559ms]-->  Mutex@src/main.rs:20  (held by worker-ba)
  ↳ worker-ba --[lock, parked 559ms]--> Mutex@src/main.rs:19 (held by worker-ab)

$ wyrd stats run.wyrd
recording span : 707ms
tasks          : 12
poll time      : n=48 p50=91µs p90=242µs p99=707ms max=707ms
longest parks  : ...
channel depths : ...
```

`wyrd why-blocked <file> [--task NAME|ID] [--at TS] [--json]` (exit code 2 on a
detected deadlock) and `wyrd stats <file> [--top N] [--json]`. `wyrd lint
<file>` distills the folds into triaged findings — deadlocks (errors),
blocking-in-async long polls, long non-timer parks, saturated channels — with
CI-friendly exit codes (2 errors / 1 warnings / 0 clean). For interactive
exploration, `wyrd tui <file>` opens a terminal UI with Stats / Tasks / Tree
(spawn tree) / Resources / Why-blocked tabs and a scrubbable time cursor
(`[` `]` to move, `g`/`G` for start/end). Add `--follow` to tail a recording
that's still being written (like `tail -f`) and watch a running app live, with
zero added overhead on the recorded program — or use `wyrd watch <file>
[--stuck-ms MS] [--for SECS] [--json]` for the same live monitoring headlessly
(CI, logs): it alerts with a full why-blocked chain when a task gets stuck and
exits 2 the moment a deadlock forms. The CLI itself
needs no `tokio_unstable`; only the recorded app does (or use `wyrd-shim` for a
stable recorder). Recordings are produced by
[`wyrd-weave`](https://crates.io/crates/wyrd-weave) or
[`wyrd-shim`](https://crates.io/crates/wyrd-shim).

License: MIT OR Apache-2.0
