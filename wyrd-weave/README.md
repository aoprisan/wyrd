# wyrd-weave

A [`tracing_subscriber`] `Layer` that normalizes tokio's internal
instrumentation into a compact, on-disk **causality event stream** — the
recording side of [wyrd](https://github.com/aoprisan/wyrd).

It never blocks a runtime worker: normalized events are handed to a dedicated
writer thread over a bounded queue and serialized as length-prefixed `postcard`
frames.

```rust
use tracing_subscriber::prelude::*;

let (layer, guard) = wyrd_weave::WeaveLayer::builder()
    .file("run.wyrd")
    .build()
    .expect("open recording");
tracing_subscriber::registry().with(layer).init();
// ... run your tokio program ...
drop(guard); // flush & finalize
```

> **Requires** the instrumented app to be built with
> `RUSTFLAGS="--cfg tokio_unstable"` and tokio's `"tracing"` feature — otherwise
> tokio emits none of the spans this layer relies on.

Recordings are read and analyzed by [`wyrd-core`] and the `wyrd` CLI. For a
stable-Rust producer that needs no `tokio_unstable`, see [`wyrd-shim`].

[`tracing_subscriber`]: https://docs.rs/tracing-subscriber
[`wyrd-core`]: https://crates.io/crates/wyrd-core
[`wyrd-shim`]: https://crates.io/crates/wyrd-shim

License: MIT OR Apache-2.0
