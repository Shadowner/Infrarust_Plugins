#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
DEFAULT_TARGETS="1.20.1-fabric,1.20.1-forge,1.21.11-fabric"

usage() {
  cat <<'USAGE'
Usage: ./run.sh --infrarust <dir> --automodpack <dir> [options]

AutoModpack's own Docker autotester (real server + real HeadlessMC client) with
Infrarust inserted between the client and the server, optionally with this
repository's automodpack plugin loaded into the proxy.

  --infrarust <dir>       Infrarust checkout. Its release binary is built there
                          (unless --skip-build) and used; INFRARUST_BINARY
                          overrides the binary. Also read from INFRARUST_DIR.
  --automodpack <dir>     AutoModpack checkout whose autotester has --join-address
                          and --server-config (branch infrarust-autotest). Also
                          read from AUTOMODPACK_DIR.
  --artifact-dir <dir>    Directory holding the autotest one-jar. Default <automodpack>/merged.
  --targets <list>        Autotester target ids. Default 1.20.1-fabric,1.20.1-forge,1.21.11-fabric
  --scenarios <list>      Autotester scenarios. Default download-only (sync adds a
                          restart and an in-game rejoin through the proxy).
  --cells <list>          Cells to run, see --list. Default: every cell that is
                          not skipped and loads no plugin.
  --list                  Print every cell with its proxy mode and expectation.
  --port <n>              Infrarust listen port. Default 25580.
  --log-level <filter>    Infrarust log filter. Default info,infrarust_core=debug,
                          which logs every handshake and Login Start the proxy parses.
  --plugins-dir <dir>     WASM plugins (*.wasm) loaded into every cell named
                          <cell>+plugins. Default: this repository's
                          target/wasm32-wasip2/release/infrarust_plugin_automodpack.wasm
  --plugin-config <file>  TOML appended to infrarust.toml of every +plugins cell.
                          Default: plugin.toml ([plugins.automodpack] permissions),
                          plus network and an allow rule for the dedicated-port cells.
  --plugin-data <dir>     Copied to plugins_dir/automodpack/ of the +plugins cells,
                          instead of the per-cell config.json.
  --work-dir <dir>        Rendered proxy configs and run output. Default ./.lab
  --hmc-cache <dir>       HeadlessMC cache shared with plain autotester runs.
                          Default <automodpack>/autotester/.hmc-cache
  --case-timeout <s>      Budget for one autotester invocation. Default 2400.
  --report <name>         Report basename under reports/. Default real-modded.
  --report-only           Rebuild the report from the last run's logs in --work-dir
                          without running anything (e.g. after editing a cell).

  --skip-build            Do not rebuild the proxy first.
  --build-plugin          Build this repository's plugin for wasm32-wasip2 first.
  --build-automodpack     Build the autotest one-jar for --targets first
                          (needs JAVA_HOME on a JDK 25+).
  --build-images          Build the autotester client images first.

cargo runs with whatever toolchain your environment selects; set
RUSTUP_TOOLCHAIN when the default one is older than Infrarust's MSRV.

Exit codes: 0 all as expected · 1 a cell failed that should pass ·
2 a known gap stopped reproducing (update the expectation).

Requires: uv, python3 >= 3.11, a Docker API (rootless podman works: when
DOCKER_HOST does not answer, a private `podman system service` is started for
the run). Uses host networking, so ports 25565 and --port must be free.
USAGE
}

SKIP_BUILD=0
REPORT_ONLY=0
BUILD_PLUGIN=0
BUILD_AMP=0
BUILD_IMAGES=0
INFRA_DIR="${INFRARUST_DIR:-}"
AMP_DIR="${AUTOMODPACK_DIR:-}"
TARGETS="$DEFAULT_TARGETS"
ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    --build-plugin) BUILD_PLUGIN=1; shift ;;
    --build-automodpack) BUILD_AMP=1; shift ;;
    --build-images) BUILD_IMAGES=1; shift ;;
    --report-only) REPORT_ONLY=1; ARGS+=("$1"); shift ;;
    --infrarust) INFRA_DIR="$2"; shift 2 ;;
    --automodpack) AMP_DIR="$2"; shift 2 ;;
    --targets) TARGETS="$2"; ARGS+=("$1" "$2"); shift 2 ;;
    --list) exec python3 -I "$HERE/run.py" --binary /dev/null --infrarust . --list ;;
    -h|--help) usage; exit 0 ;;
    *) ARGS+=("$1"); shift ;;
  esac
done

[[ -n "$INFRA_DIR" ]] || { echo "error: --infrarust <dir> is required (or set INFRARUST_DIR)" >&2; exit 1; }
[[ -n "$AMP_DIR" ]] || { echo "error: --automodpack <dir> is required (or set AUTOMODPACK_DIR)" >&2; exit 1; }
INFRA_DIR="$(cd "$INFRA_DIR" && pwd)"
AMP_DIR="$(cd "$AMP_DIR" && pwd)"
[[ -f "$INFRA_DIR/crates/infrarust/Cargo.toml" ]] || { echo "error: $INFRA_DIR is not an Infrarust checkout" >&2; exit 1; }
BINARY="${INFRARUST_BINARY:-${CARGO_TARGET_DIR:-$INFRA_DIR/target}/release/infrarust}"

if [[ $REPORT_ONLY -eq 1 ]]; then
  exec python3 -I "$HERE/run.py" --binary "$BINARY" --infrarust "$INFRA_DIR" --automodpack "$AMP_DIR" "${ARGS[@]}"
fi

for tool in uv python3 cargo; do
  command -v "$tool" >/dev/null || { echo "error: $tool is required but not installed" >&2; exit 1; }
done

docker_answers() {
  python3 -I - "$1" <<'PY'
import socket, sys
path = sys.argv[1]
try:
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(2)
        sock.connect(path)
        sock.sendall(b"GET /_ping HTTP/1.0\r\n\r\n")
        sys.exit(0 if b"OK" in sock.recv(4096) else 1)
except OSError:
    sys.exit(1)
PY
}

PODMAN_PID=""
cleanup() {
  if [[ -n "$PODMAN_PID" ]]; then
    kill "$PODMAN_PID" 2>/dev/null || true
    rm -f "$PODMAN_SOCK"
  fi
}
trap cleanup EXIT

SOCK_PATH="${DOCKER_HOST:-unix:///var/run/docker.sock}"
SOCK_PATH="${SOCK_PATH#unix://}"
if ! docker_answers "$SOCK_PATH"; then
  command -v podman >/dev/null || { echo "error: no Docker API at $SOCK_PATH and no podman to start one" >&2; exit 1; }
  PODMAN_SOCK="${XDG_RUNTIME_DIR:-/tmp}/amp-e2e-$$.sock"
  echo "==> starting a private podman API on $PODMAN_SOCK"
  podman system service --time=0 "unix://$PODMAN_SOCK" >/dev/null 2>&1 &
  PODMAN_PID=$!
  for _ in $(seq 1 50); do
    docker_answers "$PODMAN_SOCK" && break
    sleep 0.2
  done
  docker_answers "$PODMAN_SOCK" || { echo "error: podman API did not come up on $PODMAN_SOCK" >&2; exit 1; }
  export DOCKER_HOST="unix://$PODMAN_SOCK"
  SOCK_PATH="$PODMAN_SOCK"
fi

if [[ " ${ARGS[*]} " != *" --container-uid "* ]] \
  && python3 -I - "$SOCK_PATH" <<'PY'
import socket, sys
with socket.socket(socket.AF_UNIX) as sock:
    sock.settimeout(5)
    sock.connect(sys.argv[1])
    sock.sendall(b"GET /info HTTP/1.0\r\n\r\n")
    data = b""
    while chunk := sock.recv(65536):
        data += chunk
sys.exit(0 if b"name=rootless" in data else 1)
PY
then
  echo "==> rootless container engine: containers run as uid 0, which is your own user on the host"
  ARGS+=(--container-uid 0)
fi

if [[ $SKIP_BUILD -eq 0 ]]; then
  echo "==> building infrarust in $INFRA_DIR (use --skip-build to skip)"
  cargo build --release -p infrarust --manifest-path "$INFRA_DIR/Cargo.toml"
fi
[[ -x "$BINARY" ]] || { echo "error: proxy binary not found at $BINARY (set INFRARUST_BINARY or CARGO_TARGET_DIR)" >&2; exit 1; }

if [[ $BUILD_PLUGIN -eq 1 ]]; then
  echo "==> building the automodpack plugin"
  cargo build --release --target wasm32-wasip2 -p infrarust-plugin-automodpack --manifest-path "$REPO/Cargo.toml"
fi

if [[ $BUILD_AMP -eq 1 ]]; then
  echo "==> building the AutoModpack autotest one-jar for $TARGETS"
  (cd "$AMP_DIR" && ./gradlew :oneJar -Pautomodpack.autotest "-Pautomodpack.targets=$TARGETS" --console=plain)
fi

if [[ $BUILD_IMAGES -eq 1 ]]; then
  echo "==> building the autotester images"
  (cd "$AMP_DIR" && uv --project autotester run autotester build-images)
fi

cd "$HERE"
status=0
python3 -I "$HERE/run.py" --binary "$BINARY" --infrarust "$INFRA_DIR" --automodpack "$AMP_DIR" "${ARGS[@]}" || status=$?
exit "$status"
