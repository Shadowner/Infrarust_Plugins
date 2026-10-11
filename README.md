# Infrarust plugins

WASM plugins for [Infrarust](https://github.com/Shadowner/Infrarust), the Rust Minecraft proxy. Each
plugin is a crate of this Cargo workspace, built as a `wasm32-wasip2` component for Infrarust to load and
as a normal Rust library for native unit tests.

| Crate                                           | What it does |
|-------------------------------------------------|--------------|
| [`automodpack`](automodpack/README.md)          | Lets AutoModpack clients download a backend's modpack through the proxy (holepunch handoff, AMMH routing, data packet rewrite). Plugin id `automodpack`; needs `connection-handoff` and `plugin-messaging`. |

## Download

Each plugin is released on its own, under the tag `<id>-v<version>`, with two assets: the component
`<id>.wasm` and its checksum `<id>.wasm.sha256`.

```sh
id=automodpack version=0.1.0
base=https://github.com/Shadowner/Infrarust_Plugins/releases/download/$id-v$version
curl -fLO "$base/$id.wasm"
curl -fLO "$base/$id.wasm.sha256"
sha256sum -c "$id.wasm.sha256"
```

Copy `<id>.wasm` to the top level of Infrarust's `plugins_dir`, over the previous `<id>.wasm`. The file
name carries no version on purpose: Infrarust refuses both files when two of them report the same plugin
id. The release notes give the plugin contract the component targets (`infrarust:plugin@0.3.x` loads
only on hosts with that major and minor), the Infrarust commit it was built against, its checksum and
the permissions to grant in `[plugins.<id>]`.

Plugin releases are never marked as the repository's latest release, since the newest release of one
plugin says nothing about another: always download by tag. The
[releases page](https://github.com/Shadowner/Infrarust_Plugins/releases) lists every plugin's versions.
Every CI run on `main` and on pull requests also uploads each plugin as a workflow artifact named
`<id>-<version>`, for testing unreleased changes.

## Requirements

- the toolchain and target named in `Cargo.toml` (`[workspace.metadata.infrarust] toolchain`):
  `rustup toolchain install nightly-2026-09-27 --target wasm32-wasip2 --component clippy,rustfmt`
- [Infrarust](https://github.com/Shadowner/Infrarust) cloned next to this repository (`../Infrarust`): the
  plugins depend on its `infrarust-plugin-sdk` by path
- `bash`, `jq`, `perl` and `python3` (3.11 or later) for `scripts/plugins.sh`

## Build and test

`scripts/plugins.sh` is what CI runs; run it locally to get the same result:

| Command                                | Does |
|----------------------------------------|------|
| `scripts/plugins.sh build`             | builds every plugin into `dist/<id>.wasm` and `dist/<id>.wasm.sha256` |
| `scripts/plugins.sh build automodpack` | builds one plugin |
| `scripts/plugins.sh check`             | plugin ids, fmt, clippy native and `wasm32-wasip2` with `-D warnings`, tests |
| `scripts/plugins.sh smoke automodpack` | builds, then loads `dist/automodpack.wasm` into the real Infrarust host |
| `scripts/plugins.sh help`              | lists every command |

The script uses the toolchain from `Cargo.toml` and passes `--locked`. The same checks by hand:

```sh
cargo +nightly-2026-09-27 build --target wasm32-wasip2 --release
cargo +nightly-2026-09-27 test
cargo +nightly-2026-09-27 clippy --all-targets -- -D warnings
cargo +nightly-2026-09-27 fmt --check
cargo +nightly-2026-09-27 clippy --all-targets --target wasm32-wasip2 -- -D warnings
```

The `#[plugin]` entry points compile for `wasm32` only; the native build is the library and its tests.
A plugin's `smoke` workspace (`automodpack/smoke`) is a separate Cargo workspace that runs the component
inside Infrarust's WASM host; CI runs it in its own job. The `e2e` benches (Docker, real clients) never
run in CI.

## Releasing a plugin

1. Bump `version` in the plugin's `Cargo.toml`, then run `scripts/plugins.sh lock` so `Cargo.lock` and
   the smoke workspace's `Cargo.lock` follow.
2. Commit, push, and wait for CI on `main`.
3. Tag the commit `<id>-v<version>` and push the tag:

   ```sh
   git tag automodpack-v0.2.0
   git push origin automodpack-v0.2.0
   ```

The `Release` workflow refuses a tag whose version differs from the crate's, runs that plugin's tests
and smoke tests, builds only that plugin and publishes the release with `<id>.wasm`, `<id>.wasm.sha256`
and generated notes. A version with a pre-release part (`0.2.0-rc.1`) is published as a pre-release.

## Adding a plugin

1. Create the crate with `crate-type = ["cdylib", "rlib"]` and `#[plugin(id = "<id>", ...)]` on its
   `Plugin` impl, and add it to `members` in the root `Cargo.toml`.
2. Declare it as a plugin in its `Cargo.toml`:

   ```toml
   [package.metadata.infrarust-plugin]
   id = "<id>"
   permissions = ["plugin-messaging"]
   smoke = { path = "smoke", component-env = "MY_PLUGIN_WASM" }
   ```

   `id` must equal the `#[plugin(id = ...)]` (`scripts/plugins.sh check-ids` and CI check it).
   `permissions` lists the opt-in capabilities printed in the release notes. `smoke` is optional: the
   directory of a separate workspace whose tests load the component, and the environment variable through
   which it receives the path of `dist/<id>.wasm`.

The workflows find plugins through this table; nothing else needs to change.

## Infrarust version

`[workspace.metadata.infrarust]` in the root `Cargo.toml` names the Infrarust repository and `ref` the
plugins build against, and the toolchain. CI checks out that ref next to this repository, so `../Infrarust`
resolves as it does locally.

The ref is `feat/intercepted_modded_support` for now: the connection handoff and login plugin message
events the AutoModpack bridge needs only exist on that branch. Until it is pushed to GitHub, CI and
releases fail at the Infrarust checkout. Once it is merged, set `ref` to `main` or, better, to an
Infrarust release tag, run `scripts/plugins.sh lock` if the SDK's dependencies changed, and commit.

## Conventions

- No code comments; names, types, tests and the crate READMEs carry the explanations.
- Plugin crates depend only on what compiles for `wasm32-wasip2`.
