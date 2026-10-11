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
cell to pass. Some cells need more than the defaults:

- `passthrough-magic+plugins` writes a `config.json` declaring the backend
  `magic`: a forwarding mode never shows the plugin the login queries, so it
  cannot learn the mode.
- `offline-magic-hidden+plugins` declares `magic` on port 25567, since the
  backend advertises `-1`.
- both dedicated-port cells (`offline-magic-port+plugins`,
  `offline-magic-hidden+plugins`) grant `network` and allow
  `127.0.0.1:25567` under `[plugins.automodpack.wasm.network]`: Infrarust only
  takes a handoff to a port other than the server's own from a plugin whose
  allow-list covers it.

`--plugins-dir` replaces the default
`target/wasm32-wasip2/release/infrarust_plugin_automodpack.wasm`,
`--plugin-config FILE` replaces the TOML of every `+plugins` cell, and
`--plugin-data DIR` replaces the per-cell `config.json` with a directory copied
to `plugins_dir/automodpack/`.

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
and for every other login name (`1p/1l`), the other names being HOLEPUNCH lanes
(random names), then the connections the plugin claimed and handed off to the
backend (`/1h`). A "What the proxy saw" table adds, per proxied cell, the Login
Starts accepted with a layout that does not match their protocol (the lanes on
1.20.1), the handoffs by level (`login` or `raw`), the bytes the backend sent
the client over them, and the first connection error.

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
shared with plain autotester runs through `--hmc-cache`) 1.3 GB for the three
targets, and each target keeps a server volume (130 to 170 MB). The one-jar for three
targets builds in about 6 min, the Infrarust release binary in 5 to 11 min. A
passing cell takes 25 to 45 s once warm, a failing one about 4 min, because the
scenario waits 180 s for a certificate prompt the client never reaches: the
full `--cells all` grid on three targets is 51 runs in about 66 min, the `sync`
run below 12 runs in 14 min. Each run directory keeps the client's game
directory, about 35 MB per cell.

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

## Results, 2026-10-11

Infrarust `feat/intercepted_modded_support` at `6b72d0d5`, AutoModpack
5.0.0-rc.3 (`infrarust-autotest`, one-jar with 1.20.1-fabric, 1.20.1-forge and
1.21.11-fabric), the plugin built from `main`. `download-only`, every cell
(`--cells all`), 51 runs in 66 min: ✅ 39, ⚠️ 12, nothing unexpected.

| cell | 1.20.1-fabric | 1.20.1-forge | 1.21.11-fabric |
|---|---|---|---|
| `direct`, `direct-magic`, `direct-magic-port` | ✅ ✅ ✅ | ✅ ✅ ✅ | ✅ ✅ ✅ |
| `passthrough`, `zerocopy` | ✅ ✅ | ✅ ✅ | ✅ ✅ |
| `passthrough-magic` | ⚠️ | ⚠️ | ⚠️ |
| `offline` | ⚠️ | ⚠️ | ⚠️ |
| `offline-magic` | ⚠️ | ⚠️ | ⚠️ |
| `offline-magic-port` | ✅ (lanes skip the proxy) | ✅ | ✅ |
| `offline-magic-hidden` | ⚠️ | ⚠️ | ⚠️ |
| every `+plugins` cell (7) | ✅ | ✅ | ✅ |

The run at `35716be4` (first commit where every hook fires, before the review
fixes) gave the same 51 verdicts, the same handoff counts and the same errors.
The seven plain cells of the first grid, on the branch before the hooks
(`89d46057`), gave 12 ✅ and 9 ⚠️, the same as today.

What the proxy logged, per family:

- `passthrough` and `zerocopy`: one session for the player and one for the
  HOLEPUNCH lane. On 1.20.1 the lane's Login Start does not match protocol 763
  and is forwarded unchanged (`login start does not match its protocol layout,
  forwarding it unchanged`). With the plugin loaded nothing changes: a
  forwarding mode never shows the plugin the data packet, so it leaves the lane
  to the normal path.
- `offline`, 1.20.1 (Fabric and Forge): the proxy logs the player in, the
  bridged login carries the AutoModpack queries to the client (its log prints
  `AutoModpack endpoint: localhost:25580`), then the lane dies at its Login
  Start: `connection error ... error="protocol error: invalid: bool value must
  be 0 or 1"` (the strict decode reads the 1.20.2+ layout as 1.20.1's). Client:
  `HolepunchFailure: server closed during negotiation`.
- `offline`, 1.21.11: the layouts agree, so the proxy logs the lane in as a
  second player and opens a backend login for it. The backend recognises the
  marker (`marker accepted name=2y0isjoiovcjm`) and starts its takeover, which
  the proxy cannot relay: ten seconds later `backend dropped the player`, server
  `HolepunchFailure: Minecraft channel closed`, client `waiting for takeover
  query` then `connect failed`.
- every MAGIC cell whose lanes reach the proxy without the plugin
  (`passthrough-magic`, `offline-magic`, `offline-magic-hidden`): the proxy
  reads `AMMH` as the start of a handshake and drops it when the read times out,
  `connection error ... error="connection timeout: handshake read timed out"`;
  the client fails in `LaneConnector.performMagicHandshake` on
  `java.io.EOFException`.
- `offline-magic-port` passes without the plugin because the client downloads
  from `localhost:25567` directly; only the login crosses the proxy.
- with the plugin: `offline+plugins` shows `login claimed by a plugin` then
  `connection handed off ... level=login replayed=49`; the MAGIC cells show one
  `connection handed off ... level=raw replayed=15` each, to `backend`
  (shared port) or to `server backend port 25567`. In `offline-magic-port+plugins`
  the plugin learns the advertised port from the data packet
  (`backend serves AutoModpack 5.0.0-rc.3 in MAGIC mode, advertising (connection host):25567`)
  and rewrites it, so the client's endpoint becomes `localhost:25580` instead of
  `localhost:25567` and the lanes come back through the proxy. About 5.8 KB
  (MAGIC) and 6.0 KB (HOLEPUNCH) went to the client over each handoff.

`sync` (`--scenarios sync --cells direct,passthrough,offline+plugins,offline-magic+plugins`),
12 runs, all ✅. After the download the client restarts with the pack, the
server restarts, and the client rejoins and reaches the world: the server logs
`AutoTest joined the game` through the proxy, offline mode included. The proxy
saw two player sessions per cell and three lanes: the first download, one lane
the restarted client opens at boot with the endpoint it saved (before any
login), and one during the rejoin. With the plugin, all three are handoffs
(`login claimed by a plugin` + `connection handed off ... level=login`, or
`level=raw` for MAGIC).

The allow-list rule holds: `offline-magic-hidden+plugins` with plain
`plugin.toml` (no `network`, no allow rule) fails as it should, the proxy
logging `wasm plugin set a result it is not allowed to set; ignoring it ...
reason="the plugin's network allow-list does not permit the server-port target
on every address of that server"`, after which the AMMH bytes fall through to
the handshake parser and time out ten seconds later.
