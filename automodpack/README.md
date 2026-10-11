# AutoModpack bridge

An Infrarust plugin that lets [AutoModpack](https://github.com/Skidamek/AutoModpack) clients download a
backend's modpack when the backend sits behind Infrarust. It understands AutoModpack 5.x (checked against
5.0.0-rc.3) and the 4.x login format (checked against 4.0.6 and the AutoModpackVelocity bridge).

This crate holds the protocol logic only: parsers, the rewrite, the server cache, the config file and
pure decision functions. Wiring them to the Infrarust plugin SDK is a separate step; every function here
takes plain bytes and plain values and returns a plain answer, so it is tested natively.

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

| Infrarust event                                   | Function                              | Outcome |
|---------------------------------------------------|---------------------------------------|---------|
| Login Start for server `S`                        | `on_login_claim`                      | `HandoffRoute` when the UUID is a holepunch marker and `S` serves HOLEPUNCH, else `Pass` |
| First bytes of a connection, before any handshake | `on_raw_connection`                   | `Pass`, `NeedMore(n)`, `Reject(reason)` or `Handoff` to the server the AMMH host routes to |
| Backend sends `automodpack:handshake`             | `on_handshake_request`                | remembers that AutoModpack answered and its version |
| Backend sends `automodpack:data`                  | `on_data_request`                     | records the advertised endpoint, then `Forward` or `Replace(payload)` |
| Login to `S` finished                             | `on_login_finished`                   | forgets `S` when no AutoModpack query crossed that login |

### Holepunch handoff

A marker login is handed to the backend as raw bytes: the proxy replays the client's Handshake and Login
Start exactly as received and pipes the rest both ways. The backend's claim handler only accepts an
untouched login handshake (intention 2, host of at most 255 bytes, nothing after the intention), so the
glue must not rewrite the host or add forwarding data to it.

### Magic handoff

`on_raw_connection` parses the AMMH preamble, normalises the host (ASCII lowercase, one trailing dot
removed) and asks the caller's resolver for the Infrarust server id, using the same domain routing as
Minecraft handshakes. The original spelling is what gets replayed. `Handoff` carries:

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

`config.json` in the plugin data directory, JSON. A missing file means the defaults below. Unknown keys,
wrong types and impossible server declarations are errors; the glue should refuse to start the bridge on
an error rather than fall back to defaults.

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
| `proxy_protocol_to_backend` | `false`        | Ask the glue to send a PROXY protocol header before the handed-off bytes. Only enable it when the backend accepts one on the target port (5.x: `accept-proxy-protocol` on dedicated ports; the Minecraft port depends on the server software). |

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
empty cache plus the error to log. `Cache::save` writes a sibling `.tmp` file and renames it over the
cache. The glue should save after `on_data_request` changed the server's entry (compare
`cache.get(id)` before and after) and after `on_login_finished` returned an entry.

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
| `decide`     | the decision functions above and `LoginWatch`, the per-login state the glue keeps |

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

From the repository root:

```sh
cargo +nightly-2026-09-27 test -p infrarust-plugin-automodpack
cargo +nightly-2026-09-27 build -p infrarust-plugin-automodpack --target wasm32-wasip2 --release
```
