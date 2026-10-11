# AutoModpack bridge

An Infrarust plugin that lets [AutoModpack](https://github.com/Skidamek/AutoModpack) clients download a
backend's modpack when the backend sits behind Infrarust. It understands AutoModpack 5.x (checked against
5.0.0-rc.3) and the 4.x login format (checked against 4.0.6 and the AutoModpackVelocity bridge).

The crate builds as a `wasm32-wasip2` component that Infrarust loads as the plugin `automodpack`, and as a
normal Rust library for native tests. The protocol logic (parsers, the rewrite, the server cache, the
config file and pure decision functions) takes plain bytes and plain values and returns a plain answer.
`bridge` maps those answers onto the SDK's results and keeps the per-login state; `plugin` is the thin
entry point that subscribes to the events, calls the host and logs.

## How an AutoModpack client reaches the modpack host

During login the backend sends two login plugin queries:

| Channel                 | Message id | Request payload                     | Client answer                     |
|-------------------------|-----------:|-------------------------------------|-----------------------------------|
| `automodpack:handshake` |     `-100` | one Minecraft string, JSON          | one Minecraft string, JSON        |
| `automodpack:data`      |     `-101` | one Minecraft string, JSON          | one Minecraft string, a word      |

The handshake JSON is `{"loaders":[..],"amVersion":"..","mcVersion":".."}` in both generations.

The 5.x data JSON is written by a pretty-printing Gson:

```json
{
  "endpointHost": "",
  "endpointPort": -1,
  "secret": { "secret": "...", "timestamp": 1760183415 },
  "connectionMode": "HOLEPUNCH",
  "requireModpack": true
}
```

The 4.x data JSON is compact and HTML-escaped:
`{"address":"","port":-1,"modpackName":"..","secret":{..},"modRequired":true,"requiresMagic":true}`.

A blank host means "the host the player connected to", port `-1` means "the port the player connected
to". Behind Infrarust both point at the proxy. The client answers the data query with `false` (up to
date), `true` (update needed), `null` (host error), `rejected`, `declined` or `cancelled`; a 4.x client
that fails sends an empty answer.

The client then opens connections to the modpack host in one of three ways:

- **HOLEPUNCH** (5.x default). A real Minecraft connection to the game address: a Handshake with intention
  login (Forge and NeoForge clients append `\0FML3\0` to the host) and a Login Start whose 16-byte UUID is
  a holepunch marker. The Login Start always uses the 1.20.2+ layout `[string name][uuid]`, whatever
  protocol the handshake declares. The backend's mcholepunch claim handler recognises the marker and takes
  the connection over.
- **MAGIC**. Raw TCP to the endpoint, starting with `AMMH`, a big-endian u16 length and that many bytes of
  host. The backend answers `AMOK` and TLS follows. The backend serves it either on its Minecraft port
  (`bind-port` -1) or on a dedicated port that drops anything not starting with `AMMH`.
- **HTTP**. Raw TLS to a dedicated port, no preamble. 4.x packets with `requiresMagic: false` are this mode.

Clients store the endpoint and mode they were given and reuse them without logging in (at game start, and
when the login queries do not arrive), so what the proxy learned has to survive restarts.

## What the bridge does

| Infrarust event                                          | Function               | Outcome |
|----------------------------------------------------------|------------------------|---------|
| `LoginClaimEvent`: Login Start for server `S`            | `on_login_claim`       | `handoff(Handoff::route())` when the UUID is a holepunch marker and `S` serves HOLEPUNCH, else untouched (`Pass`) |
| `RawConnectionEvent`: first bytes start with `AMMH`      | `on_raw_connection`    | untouched, `need_more(n)`, `reject()` or a handoff to the server the AMMH host routes to |
| `LoginPluginRequestEvent` on `automodpack:handshake`     | `on_handshake_request` | remembers that AutoModpack asked and its version; the request is forwarded |
| `LoginPluginRequestEvent` on `automodpack:data`          | `on_data_request`      | records the advertised endpoint, then untouched (`Forward`) or `Replace(payload)` |
| `LoginPluginResponseEvent` on `automodpack:data`         | `ClientAnswer::read`   | logged at debug, forwarded untouched |
| `ServerConnectedEvent`: the backend accepted the login   | `on_login_finished`    | forgets `S` when no AutoModpack query crossed that login |
| `DisconnectEvent`                                        |                        | drops the player's pending login state |

The bridge keeps one `LoginWatch` per player, for the server the player is logging in to. A query for
another server starts a fresh watch, `ServerConnectedEvent` consumes it, `DisconnectEvent` drops it.

The magic handoff maps `RawDecision::Handoff` like this:

| Decision                  | `Handoff` |
|---------------------------|-----------|
| `port: None`              | `HandoffTarget::Server(server)`: the server's Minecraft port, through its load balancer |
| `port: Some(p)`           | `HandoffTarget::ServerPort { server, port: p }`: the host of the address the load balancer picks, on `p` |
| `replay: true`            | default replay: the bytes read so far, preamble included, go to the backend first |
| `replay: false`           | `without_replay()` and `with_prefix(data[consumed..])`: only what followed the preamble |
| `reply` not empty         | `with_reply(reply)`: `AMOK` written to the client before the relay starts |

`proxy_protocol_to_backend: true` adds `with_proxy_protocol(true)` to every magic handoff. Otherwise a
dedicated port gets `with_proxy_protocol(false)`, and the Minecraft port follows the Infrarust server's
`send_proxy_protocol`, as the holepunch handoff does, since that listener is the one the server setting
describes.

### Holepunch handoff

A marker login is handed to the backend as raw bytes with `Handoff::route()`: the proxy replays the
client's Handshake the way it forwards one in passthrough without identity forwarding (the server's
`domain_rewrite` applies and the Forge marker stays), then the Login Start exactly as received, and pipes
the rest both ways. The backend's claim handler only accepts a plain login handshake (intention 2, host of
at most 255 bytes, nothing after the intention); a login claim never adds forwarding data, so the handoff
keeps it acceptable.

### Magic handoff

`on_raw_connection` parses the AMMH preamble, normalises the host (ASCII lowercase, one trailing dot
removed) and asks the caller's resolver for the Infrarust server id. The plugin resolves with
`Config::server_by_domain`, the same domain routing as Minecraft handshakes. The original spelling is what gets replayed. `Handoff` carries:

- `server`: the resolved server id.
- `port`: `None` for the server's own Minecraft port, `Some(p)` for a dedicated modpack port.
- `consumed`: length of the AMMH preamble at the start of `data`.
- `replay`: `true` sends `data` unchanged to the backend, `false` sends `data[consumed..]`.
- `reply`: bytes to write back to the client before piping.

A MAGIC backend gets the preamble replayed and answers `AMOK` itself (`replay: true`, empty `reply`). An
HTTP backend never saw a preamble, so the proxy answers `AMOK` and strips it (`replay: false`,
`reply: AMOK`). Servers in HOLEPUNCH mode, unknown servers and HTTP servers without a known port are
rejected: nothing behind them would understand the bytes.

### Data rewrite

Every readable data request updates the cache with what the backend advertised (before any rewrite). The
packet is then rewritten so the client comes back through the proxy in MAGIC mode when all of these hold:

- `magic` is enabled;
- `rewrite_to_magic` allows the advertised mode (`magic-only`: MAGIC; `always`: MAGIC and HTTP; HOLEPUNCH
  is never rewritten because a HOLEPUNCH backend runs no AMMH gate);
- the proxy can route the client's AMMH to that server (MAGIC, or HTTP with a known port);
- the packet does not already say blank host, port `-1`, MAGIC.

The rewrite sets `endpointHost` to `""`, `endpointPort` to `-1` and `connectionMode` to `MAGIC` (5.x), or
`address` to `""`, `port` to `-1` and `requiresMagic` to `true` (4.x, the same edit the Velocity bridge
makes). Every other field, unknown ones included, keeps its value; the JSON is re-encoded compactly with
sorted keys, which Gson reads the same way.

The proxy assumes an advertised port lives on the backend server's own address. When the backend
advertises a public address that differs from the one Infrarust uses, declare the real port in
`servers`.

## Security rule

A handed-off login reaches the backend without passing through Infrarust's authentication or forwarding.
The bridge therefore only hands a marker login to a server that was seen sending a HOLEPUNCH data packet
through the proxy, or that the config declares as `holepunch`. A 5.x backend only sends a HOLEPUNCH data
packet while its claim handler is registered, so a fresh observation means the takeover is waiting for
the connection. Anyone can forge a marker; the marker is not a credential.

A server leaves the cache when a login to it finishes without any AutoModpack query. Between a backend
dropping AutoModpack and the next normal login through the proxy, marker logins still reach it: on a
backend with modern or legacy forwarding they normally fail the forwarding check, but an offline-mode
backend without forwarding would let them in under the marker's name. Set `holepunch` to `false` or declare
servers explicitly when that matters.

## Config file

`config.json` in the plugin data directory (`plugins_dir/automodpack/`), JSON. A missing file means the
defaults below. Unknown keys, wrong types and impossible server declarations are errors: the plugin then
fails to enable with the reason in the proxy log, and Infrarust handles every connection as if the bridge
were not installed. It never falls back to the defaults over a broken file.

```json
{
  "holepunch": true,
  "magic": true,
  "rewrite_to_magic": "magic-only",
  "servers": {},
  "proxy_protocol_to_backend": false
}
```

| Key                         | Default        | Meaning |
|-----------------------------|----------------|---------|
| `holepunch`                 | `true`         | Hand marker logins to backends seen (or declared) serving HOLEPUNCH. `false` passes every login to Infrarust's normal path. |
| `magic`                     | `true`         | Recognise AMMH connections and route them by host. `false` also disables every data rewrite, since a rewritten client would come back with AMMH. |
| `rewrite_to_magic`          | `"magic-only"` | `"never"`: forward data packets untouched. `"magic-only"`: rewrite packets of MAGIC backends so their clients come back through the proxy. `"always"`: also rewrite HTTP backends, the proxy then answers `AMOK` and pipes TLS to the backend's HTTP port. |
| `servers`                   | `{}`           | Static declarations by Infrarust server id, `{"mode": "holepunch" \| "magic" \| "http", "port": n}`. A declaration wins over what the cache learned. `magic` without `port` means the Minecraft port; `http` needs a `port`; `holepunch` takes no `port`. |
| `proxy_protocol_to_backend` | `false`        | `true` sends a PROXY protocol v2 header before every magic handoff. Only enable it when the backend reads one on the target port (5.x: `accept-proxy-protocol: true` on a dedicated port; the Minecraft port depends on the server software). With `false`, dedicated ports get no header and the Minecraft port follows the Infrarust server's `send_proxy_protocol`. Holepunch handoffs always follow `send_proxy_protocol`. |

## Server cache

`server-cache.json` in the plugin data directory, written by the plugin:

```json
{
  "version": 1,
  "servers": {
    "lobby": {
      "mode": "magic",
      "advertised_port": 24454,
      "advertised_host": "mc.example.net",
      "seen_at_ms": 1760183415000,
      "amp_version": "5.0.0-rc.3"
    }
  }
}
```

`Cache::load` never fails: a missing file gives an empty cache, an unreadable or corrupt one gives an
empty cache plus a warning in the log. `Cache::save` writes a sibling `.tmp` file and renames it over the
cache. The plugin saves after every data packet that changed the server's entry (`seen_at_ms` included)
and after a login that made it forget a server. It loads the file again whenever the host starts a fresh
instance after a fault, so what it learned survives a recovery and a restart.

## Install

Build the component (see [Building and testing](#building-and-testing)) and copy it to the top level of
Infrarust's `plugins_dir`. The file name does not matter; the plugin id is `automodpack`:

```sh
cp target/wasm32-wasip2/release/infrarust_plugin_automodpack.wasm /srv/infrarust/plugins/automodpack.wasm
```

The plugin's data directory is `plugins_dir/automodpack/`, created by the proxy on load and mounted as `/`
inside the plugin. Put `config.json` there if the defaults do not fit; the plugin writes
`server-cache.json` there itself.

Grant the two opt-in capabilities in `infrarust.toml`:

```toml
plugins_dir = "./plugins"

[plugins.automodpack]
permissions = ["connection-handoff", "plugin-messaging"]
```

| Capability           | Used for |
|----------------------|----------|
| `connection-handoff` | `ConnectionClaims::register_prefix(b"AMMH")`, the `RawConnectionEvent` and `LoginClaimEvent` subscriptions, the handoffs |
| `plugin-messaging`   | `Messaging::register` for `automodpack:handshake` and `automodpack:data`, the `LoginPluginRequestEvent` and `LoginPluginResponseEvent` subscriptions |
| `event-bus`          | baseline: `ServerConnectedEvent` and `DisconnectEvent` |
| `config-read`        | baseline: `Config::server_by_domain`, which resolves an AMMH host with the same domain routing as a handshake |

Do not `deny` the two baseline capabilities. Every handoff targets a configured server (`Route`,
`Server` or `ServerPort`), never an `Address`. HOLEPUNCH and MAGIC on the Minecraft port need nothing
more. A dedicated modpack port (MAGIC or HTTP with a `bind-port`) is a `ServerPort` off the server's own
port, which Infrarust only accepts from a WASM plugin when its network allow-list covers every address of
that server on that port:

```toml
[plugins.automodpack]
permissions = ["connection-handoff", "plugin-messaging", "network"]

[plugins.automodpack.wasm.network]
allow = ["10.0.0.5:24454"]
```

Without a capability the plugin fails to enable and the log names the missing grant. Without the allow
rule, Infrarust ignores the dedicated-port handoff and logs a rate-limited warning.

`[rate_limit]`, when enabled, counts every Minecraft handshake per IP, and a holepunch lane is one: an
AutoModpack client opens about five lanes at once when it downloads. The default of 3 per 10 seconds
refuses most of them; raise `max_connections` (20 is comfortable) when the rate limiter is on. AMMH
connections are claimed before the rate limiter and are not counted.

### Infrarust servers

The bridge learns what a backend serves from the data packet it sends during the login. Infrarust only
reads login plugin messages for servers in the `offline` and `client_only` proxy modes. For a server in a
forwarding mode (`passthrough`, `zero_copy`, `server_only`) the bridge never sees the packet, cannot
rewrite it, and forgets the server after every login: declare such a server in `config.json` `servers`,
and keep the backend advertising an endpoint the clients can reach through the proxy (the defaults do).

With holepunch enabled the plugin listens to `LoginClaimEvent`, so Infrarust reads a Login Start whose
layout does not match the declared protocol on every route, and hands it to the plugin. Holepunch marker
logins always use the 1.20.2 layout. A login the bridge leaves alone is refused on an `offline` or
`client_only` route exactly as before.

### Backend AutoModpack settings

These are the AutoModpack 5.x server settings (`automodpack/server.conf`).

HOLEPUNCH, the default (`connection-mode: HOLEPUNCH`): nothing to change. Once a player has logged in
through the proxy, the bridge hands marker logins for that server to its Minecraft port with the client's
handshake and Login Start replayed untouched, and the backend's claim handler takes them over.

MAGIC (`connection-mode: MAGIC`):

- on the Minecraft port, `bind-port: -1`, with `advertised-endpoint-host` and `advertised-endpoint-port`
  left at their defaults (`""` and `-1`). The packet already sends the client back to the address it
  connected to, the proxy; the bridge forwards it untouched and routes the AMMH connections to the
  server's Minecraft port.
- on a dedicated port, `bind-port: 24454` (any port the proxy can reach on the backend host). Either
  advertise it with `advertised-endpoint-port: 24454`, and the bridge learns the port and rewrites the
  packet to point at the proxy, or leave `advertised-endpoint-*` at their defaults and declare
  `"servers": {"<server id>": {"mode": "magic", "port": 24454}}` in `config.json`. The proxy connects to
  the host of the address Infrarust uses for that server, on that port.

HTTP (`connection-mode: HTTP`, dedicated `bind-port`) is only bridged with `rewrite_to_magic: "always"`
and a known port: the proxy answers `AMOK` itself and relays TLS to the HTTP port.

To pass the real client address to a dedicated port, set `accept-proxy-protocol: true` on the backend and
`proxy_protocol_to_backend: true` in `config.json`.

## Faults and limits

The plugin never hands a connection off on its own initiative: every handoff follows a cache entry or a
config declaration, as the security rule above requires. When the plugin is not there to answer (failed to
enable, trapped, recovering or quarantined), every hook keeps its default: a marker login goes through
Infrarust's normal login, an `AMMH` connection is handled as Minecraft and closed, and data packets reach
the client unchanged. A fresh instance reloads `config.json` and `server-cache.json` before it answers
again.

A player switching servers is already past the login, so the backend's queries cannot reach the client
(`client_reachable` is false). The bridge leaves the request alone and the proxy answers "not
understood", which AutoModpack reads as a client without AutoModpack: a backend with
`require-modpack: true` refuses the switch, one without it lets the player in without syncing its
modpack. The query still counts as AutoModpack seen, so the server stays in the cache. Join such a
server directly to download its modpack.

## Modules

| Module       | Contents |
|--------------|----------|
| `wire`       | VarInt and Minecraft string codec with the 32767-char limit |
| `mode`       | `ConnectionMode`: wire names `HOLEPUNCH`/`MAGIC`/`HTTP`, file names lowercase |
| `holepunch`  | `is_marker_uuid`, `marker_nonce`, `login_start_marker` |
| `ammh`       | `parse`, `AMMH`, `AMOK`, `validate_host`, `lookup_key`, `encode` |
| `login_data` | `DataPacket` parse and rewrite, `ClientAnswer`, `HandshakeInfo`, channel names and ids |
| `cache`      | `Cache`, `ServerEntry`, JSON persistence |
| `config`     | `Config`, `ServerOverride`, `RewriteToMagic` |
| `decide`     | the decision functions above and `LoginWatch`, the per-login state |
| `bridge`     | `Bridge`: config, cache and per-player watches, and the mapping of decisions to SDK results and `Handoff`s |
| `plugin`     | the `#[plugin]` entry point, compiled for `wasm32` only |

## Test data

- `testdata/holepunch_vectors.json` comes from running the closed mcholepunch jar shipped with
  AutoModpack (`libs/mcholepunch-core-0.1.0-SNAPSHOT.jar`, sha256 recorded in the file). A throwaway Java
  harness called `HolepunchMarker.create` and `HolepunchMarker.decode` for 32 nonces, 26 UUIDs the jar
  rejects, every single-bit flip of one marker, and `LoginCodec.loginStart` for 9 Login Start frames. The
  Rust decoder must agree with every `jar_decodes` value, and the test-only encoder must reproduce every
  marker bit for bit.
- `testdata/gson/` holds data and handshake packets serialized by Gson 2.8.9 with the same settings as
  each AutoModpack generation, plus the Velocity bridge's rewrite of a 4.x packet.

## Building and testing

The crate depends on `infrarust-plugin-sdk` by path, `../Infrarust/crates/infrarust-plugin-sdk` from the
repository root, so the Infrarust repository has to sit next to this one. From the repository root:

```sh
cargo +nightly-2026-09-27 test -p infrarust-plugin-automodpack
cargo +nightly-2026-09-27 build -p infrarust-plugin-automodpack --target wasm32-wasip2 --release
```

The component is `target/wasm32-wasip2/release/infrarust_plugin_automodpack.wasm`.

`smoke/` is a separate workspace that loads that component into Infrarust's WASM host
(`infrarust-loader-wasm` with mocked proxy services), enables it with the capabilities above and fires
the events at it: discovery and metadata, the `AMMH` prefix claim, a data packet rewrite and the cache
file it writes, the magic and HTTP handoffs, holepunch claims before and after a backend is learned, the
revocation after a plain login, a restart from a saved cache, disabled modes, a broken config and missing
capabilities. Its build script builds the component first. It compiles wasmtime and Infrarust's core, so
it is kept out of the main workspace:

```sh
cd automodpack/smoke
cargo +nightly-2026-09-27 test
```

Set `AUTOMODPACK_WASM=/path/to/component.wasm` to test a component built elsewhere.
