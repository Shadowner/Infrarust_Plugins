# Infrarust plugins

WASM plugins for [Infrarust](https://github.com/Shadowner/Infrarust), the Rust Minecraft proxy. Each
plugin is a crate of this Cargo workspace, built as a `wasm32-wasip2` component for Infrarust to load and
as a normal Rust library for native unit tests.

| Crate                                           | What it does |
|-------------------------------------------------|--------------|
| [`automodpack`](automodpack/README.md)          | Lets AutoModpack clients download a backend's modpack through the proxy (holepunch handoff, AMMH routing, data packet rewrite). Protocol logic only for now, the SDK wiring comes next. |

## Requirements

- the `nightly-2026-09-27` toolchain with the `wasm32-wasip2` target:
  `rustup toolchain install nightly-2026-09-27 --target wasm32-wasip2`

## Build

```sh
cargo +nightly-2026-09-27 build --target wasm32-wasip2 --release
```

The components land in `target/wasm32-wasip2/release/*.wasm`.

## Test

Tests run natively:

```sh
cargo +nightly-2026-09-27 test
cargo +nightly-2026-09-27 clippy --all-targets -- -D warnings
cargo +nightly-2026-09-27 fmt --check
cargo +nightly-2026-09-27 check --target wasm32-wasip2
```

## Conventions

- No code comments; names, types, tests and the crate READMEs carry the explanations.
- Plugin crates depend only on what compiles for `wasm32-wasip2`.
