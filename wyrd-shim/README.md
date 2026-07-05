# wyrd-shim

Stable-Rust tokio wrappers that record the same
[wyrd](https://github.com/aoprisan/wyrd) causality events as `wyrd-weave` —
**without** `--cfg tokio_unstable`.

Where `wyrd-weave` reads tokio's unstable internals and sees *everything*, this
shim instruments a few wrapper types from the outside. The trade-off: it only
sees tasks/resources routed through `wyrd_shim::{spawn, Mutex, mpsc}` (not
primitives inside dependencies), but it works on stable, gives exact
`file:line:col` source locations, and tracks holders from observed state.

```rust
let _guard = wyrd_shim::init("run.wyrd")?;              // no RUSTFLAGS needed
let lock = std::sync::Arc::new(wyrd_shim::Mutex::new(0));
let l = lock.clone();
wyrd_shim::spawn(async move { *l.lock().await += 1; });
# Ok::<(), wyrd_shim::InitError>(())
```

Emits the identical `wyrd_weave::Event` vocabulary, so the same
[`wyrd-core`](https://crates.io/crates/wyrd-core) / `wyrd` CLI read its
recordings.

License: MIT OR Apache-2.0
