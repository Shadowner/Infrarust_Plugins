# Real modded bench: AutoModpack through Infrarust

Runs AutoModpack's own Docker autotester with Infrarust between the client and
the server. The autotester starts a real Fabric or Forge server with AutoModpack
(`itzg/minecraft-server`), launches a real modded client in a HeadlessMC
container, drives the client's own UI through a file bridge (trust the
certificate, accept the pack, wait for the files) and checks what landed on
disk. This bench only inserts the proxy, one Infrarust process per cell, and
optionally loads this repository's plugin into it.

It is a plain script meant to be run by hand on your own machine.

```sh
# first time: the instrumented one-jar for the targets (Gradle needs JDK 25),
# the autotester images, and the plugin
JAVA_HOME=/path/to/jdk-25 ./run.sh --infrarust ../../../Infrarust --automodpack ../../../AutoModpack \
    --build-automodpack --build-images --build-plugin

./run.sh --infrarust ../../../Infrarust --automodpack ../../../AutoModpack --skip-build     # default grid
./run.sh ... --skip-build --list                                                           # cells
./run.sh ... --skip-build --targets 1.21.11-fabric --cells direct,passthrough --scenarios sync
./run.sh ... --skip-build --cells all                                                       # with the plugin cells
./run.sh ... --report-only                                                                 # re-render the last run
```

`--infrarust` is an Infrarust checkout: the script builds its release binary
there (`cargo build --release -p infrarust`) unless `--skip-build`, and runs
`target/release/infrarust` (or `$CARGO_TARGET_DIR/release/infrarust`,
or `$INFRARUST_BINARY`). cargo uses your default toolchain; set
`RUSTUP_TOOLCHAIN` when it is older than Infrarust's MSRV.

## Requirements

- `uv`, Python 3.11+, cargo, and a Docker API. Rootless podman works: when
  `DOCKER_HOST` does not answer, the script starts a private
  `podman system service` for the run, and on a rootless engine it runs the
  containers as uid 0, which is your own user on the host (otherwise the client
  container cannot write its bind mounts).
- An AutoModpack checkout whose autotester has `--join-address`, `--network`
  and `--server-config`: branch `infrarust-autotest` of the fork. Those three
  options let a run join through a proxy without editing the autotester's
  settings or scenarios. The script refuses to start without them, and refuses
  a one-jar (`merged/automodpack-*.jar`) that has no impl for a requested target.
- Host networking: ports 25565 (the server), 25567 (the dedicated-port cells)
  and `--port` (25580, the proxy) must be free.

## Cells

A cell is one autotester run. The rendered proxy config (under `.lab/proxies/`)
binds `[::]:25580` and routes `domains = ["*"]` to `127.0.0.1:25565`:

| cell | `proxy_mode` | AutoModpack mode | expected without the plugin |
|---|---|---|---|
| `direct` | (no proxy) | HOLEPUNCH | pass, control |
| `direct-magic` | (no proxy) | MAGIC, shared port | pass, control |
| `direct-magic-port` | (no proxy) | MAGIC, port 25567 advertised | pass, control |
| `passthrough` | passthrough | HOLEPUNCH | pass |
| `zerocopy` | zero_copy | HOLEPUNCH | pass |
| `passthrough-magic` | passthrough | MAGIC, shared port | fail: raw `AMMH` opening |
| `offline` | offline | HOLEPUNCH | fail: marker logins need the plugin |
| `offline-magic` | offline | MAGIC, shared port | fail: raw `AMMH` opening |
| `offline-magic-port` | offline | MAGIC, port 25567 advertised | pass: the lanes skip the proxy |
| `offline-magic-hidden` | offline | MAGIC, port 25567 not advertised | fail: `AMMH` comes back to the proxy |
| `client-only` | client_only | HOLEPUNCH | skipped |

The dedicated-port cells run the AutoModpack server with `bind-port 25567`;
`-port` advertises it (`advertised-endpoint-port 25567`, the lanes then go to
that port directly), `-hidden` keeps `-1` (the lanes come back to the port the
player joined, the proxy's).

Appending `+plugins` to a proxied cell (`offline+plugins`) copies the plugin
into that proxy's `plugins_dir`, appends `plugin.toml` (the
`[plugins.automodpack]` permissions) to its `infrarust.toml`, and expects the
cell to pass. Two cells also write the plugin's `config.json`, because the
plugin cannot learn what it needs from the data packet there:
`passthrough-magic+plugins` declares the backend `magic` (a forwarding mode never
shows the plugin the login queries), `offline-magic-hidden+plugins` declares
`magic` on port 25567 (the backend advertises `-1`). `--plugins-dir` replaces the
default `target/wasm32-wasip2/release/infrarust_plugin_automodpack.wasm`,
`--plugin-config` replaces `plugin.toml`, and `--plugin-data DIR` replaces the
per-cell `config.json` with a directory copied to `plugins_dir/automodpack/`.

`--cells` takes cell names and three groups: `plain` (every runnable cell
without the plugin, the default), `plugins` (every proxied cell with `+plugins`)
and `all`.

AutoModpack 5 moves the modpack over lanes, extra TCP connections to the host
and port the player joined. In HOLEPUNCH mode (the default) a lane is a real
handshake plus a Login Start whose UUID is a marker, in the 1.20.2+ layout
whatever the protocol; the server then takes the stream over. In MAGIC mode a
lane opens with `AMMH` + length + host, which is not a Minecraft packet.

`client-only` is skipped: the autotester client logs in offline
(`hmc.offline=true`), so it cannot pass a session-server check. The `offline`
cells exercise the same intercepted path. Running `client_only` would need
authlib-injector and a session server inside the client image.

## The verdict

A cell passes when the scenario's whole flow passes. For `download-only`:
certificate trust, pack acceptance, every hosted file on disk. `sync` adds a
client restart with the pack loaded, a server restart and an in-game rejoin.

A proxied cell must also show the player in the proxy's own log: a pass without
a `session started` for `AutoTest` is failed, because the client then never went
through Infrarust. The grid prints the sessions the proxy logged for the player
and for every other login name (`1p/1l`); the other names are HOLEPUNCH lanes,
which use random names. A "What the proxy saw" table adds, per proxied cell, the
Login Starts accepted with a layout that does not match their protocol (the
lanes on 1.20.1) and the first connection error.

Proxied cells whose control (`direct`, or `direct-magic` for the MAGIC cells)
failed on the same target are reported untrusted rather than failed. Each
failing cell lists the failed step and the decisive lines of the proxy log
(after startup, at `info,infrarust_core=debug`), the server log and the client's
`latest.log`. Raw logs stay in `.lab/runs/<target>__<scenario>__<cell>/`, the
reports in `reports/real-modded.md` and `reports/real-modded.json`.

Exit codes: `0` everything as expected, `1` a cell failed that should pass, `2`
a cell expected to fail passed (update its expectation in `run.py`).

## Costs

Measured on the first runs: the client image is 1.12 GB (about 6 min to build,
mostly HeadlessMC's Gradle build), the `itzg/minecraft-server` java17 and java21
images are about 0.8 GB each, the HeadlessMC cache (Minecraft, loaders, assets,
shared with plain autotester runs through `--hmc-cache`) about 0.7 GB per target,
and each target keeps a server volume. The one-jar for three targets builds in
about 6 min. A passing cell takes 35 to 110 s once warm, a failing one about
4 min, because the scenario waits 180 s for a certificate prompt the client
never reaches.

## Traps

- The autotester's `run` stops every Gradle daemon of your Gradle home
  (`gradlew --stop`): do not run the bench while an AutoModpack build is going.
- Editing `run.sh` while it runs breaks the running copy, since bash reads a
  script as it goes. `run.py` can be edited freely.
- The proxy must listen on IPv6 too. The 1.20.1 client writes the host it
  reverse-resolved (`localhost`) into the handshake, AutoModpack resolves that
  name again for each lane, and under Forge it resolves to `::1` first: a proxy
  bound to `127.0.0.1` only takes the player and refuses every lane. A real
  deployment has the same trap when the proxy's name has an AAAA record and it
  listens on IPv4 only.
- Interrupting with Ctrl-C lets the autotester remove its containers; a second
  Ctrl-C can leave `amp-*` containers behind (`podman rm -f`).
