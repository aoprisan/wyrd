# Releasing wyrd to crates.io

The crates form a dependency chain, so they must be published **in order**,
waiting for each to appear in the crates.io index before the next:

```
wyrd-weave   →   wyrd-core   →   wyrd-shim
                     ↘             (wyrd-shim needs wyrd-weave)
                       wyrd-cli   (needs wyrd-core + wyrd-weave)
```

1. Bump the version in the root `[workspace.package]` (and the `version` on the
   internal deps in `[workspace.dependencies]`) together.
2. `cargo login <token>` (once).
3. Publish in order, pausing for the index between steps:

   ```console
   cargo publish -p wyrd-weave
   cargo publish -p wyrd-core
   cargo publish -p wyrd-shim
   cargo publish -p wyrd-cli
   ```

Notes:

- The example crates (`examples/demo`, `examples/axum`) are `publish = false`.
- Each publishable crate carries `version` on its internal path deps, so cargo
  rewrites `path = ...` to the crates.io version at publish time.
- `cargo package -p <crate> --no-verify` validates a crate's metadata locally;
  downstream crates only fully package once their dependency is on crates.io.
- The `wyrd` binary is provided by the `wyrd-cli` package, so end users run
  `cargo install wyrd-cli`.
