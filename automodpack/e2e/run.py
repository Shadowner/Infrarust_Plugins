import argparse
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import time
import zipfile
from dataclasses import dataclass, field, replace
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
DEFAULT_WASM = (
    Path(os.environ.get("CARGO_TARGET_DIR") or REPO / "target") / "wasm32-wasip2" / "release" / "infrarust_plugin_automodpack.wasm"
)
DEFAULT_PLUGIN_CONFIG = HERE / "plugin.toml"
PLUGIN_ID = "automodpack"

DEFAULT_TARGETS = "1.20.1-fabric,1.20.1-forge,1.21.11-fabric"
DEFAULT_CELLS = "plain"
BACKEND = "127.0.0.1:25565"
READY_LINE = "listener bound"
PLUGIN_SUFFIX = "+plugins"
GLYPH = {"pass": "✅", "fail": "❌", "known": "⚠️", "fixed": "🎉", "untrusted": "❔", "skip": "⏭️"}


@dataclass(frozen=True)
class Cell:
    id: str
    proxy: str | None
    amp_mode: str
    expect: str
    control: str | None
    about: str
    known: str = ""
    skip: str = ""
    plugins: bool = False
    amp_config: tuple = ()
    plugin_json: str = ""


MODPACK_PORT = 25567
PORT_ADVERTISED = (("bindPort", MODPACK_PORT), ("advertisedEndpointPort", MODPACK_PORT))
PORT_HIDDEN = (("bindPort", MODPACK_PORT),)
DECLARE_MAGIC = '{"servers": {"backend": {"mode": "magic"}}}'
DECLARE_MAGIC_PORT = '{"servers": {"backend": {"mode": "magic", "port": %d}}}' % MODPACK_PORT

CELLS = {
    cell.id: cell
    for cell in [
        Cell(
            "direct", None, "HOLEPUNCH", "pass", None,
            "No proxy. The client joins the AutoModpack server itself; the control every HOLEPUNCH cell leans on.",
        ),
        Cell(
            "direct-magic", None, "MAGIC", "pass", None,
            "No proxy, AutoModpack in MAGIC mode on the shared port; the control for the shared-port MAGIC cells.",
        ),
        Cell(
            "direct-magic-port", None, "MAGIC", "pass", None,
            f"No proxy, AutoModpack in MAGIC mode on a dedicated port {MODPACK_PORT} it advertises; the control for the dedicated-port cells.",
            amp_config=PORT_ADVERTISED,
        ),
        Cell(
            "passthrough", "passthrough", "HOLEPUNCH", "pass", "direct",
            "Infrarust relays bytes. Every HOLEPUNCH lane is a fresh handshake + Login Start the proxy routes like a player.",
        ),
        Cell(
            "zerocopy", "zero_copy", "HOLEPUNCH", "pass", "direct",
            "Same as passthrough through the splice forwarder.",
        ),
        Cell(
            "passthrough-magic", "passthrough", "MAGIC", "fail", "direct-magic",
            "Passthrough with AutoModpack in MAGIC mode on the shared port: the lanes open with raw AMMH bytes, not a Minecraft handshake. "
            "With the plugin the server is declared magic in config.json, since a forwarding mode never shows the plugin the data packet.",
            known="A raw AMMH opening is not a Minecraft handshake: the proxy waits for one and drops the lane when its handshake read times out. Needs the plugin (raw prefix claim).",
            plugin_json=DECLARE_MAGIC,
        ),
        Cell(
            "offline", "offline", "HOLEPUNCH", "fail", "direct",
            "Intercepted offline mode, backend online-mode=false. The AutoModpack login queries are relayed by the bridged login; "
            "the HOLEPUNCH lanes are extra logins carrying the marker UUID.",
            known="The proxy logs the lane in itself: on 1.20.1 its Login Start (1.20.2+ layout) is refused by the strict decode, on later versions the "
            "backend never sees the raw lane. Needs the plugin (login claim + handoff).",
        ),
        Cell(
            "offline-magic", "offline", "MAGIC", "fail", "direct-magic",
            "Intercepted offline mode with AutoModpack in MAGIC mode on the shared port.",
            known="Raw AMMH connections are dropped when the proxy's handshake read times out. Needs the plugin (raw prefix claim).",
        ),
        Cell(
            "offline-magic-port", "offline", "MAGIC", "pass", "direct-magic-port",
            f"Offline mode, AutoModpack in MAGIC mode on a dedicated port {MODPACK_PORT} it advertises: without the plugin the lanes go "
            "straight to that port and never touch the proxy; with it the data packet is rewritten so they come back through the proxy.",
            amp_config=PORT_ADVERTISED,
        ),
        Cell(
            "offline-magic-hidden", "offline", "MAGIC", "fail", "direct-magic-port",
            f"Offline mode, AutoModpack in MAGIC mode on a dedicated port {MODPACK_PORT} it does not advertise (port -1), so the lanes "
            "come back to the proxy's port. With the plugin the port is declared in config.json.",
            known="The lanes send AMMH to the proxy's port and are dropped when the handshake read times out. Needs the plugin (raw prefix claim + port declaration).",
            amp_config=PORT_HIDDEN,
            plugin_json=DECLARE_MAGIC_PORT,
        ),
        Cell(
            "client-only", "client_only", "HOLEPUNCH", "fail", "direct",
            "Intercepted client_only mode.",
            skip="The autotester's HeadlessMC client logs in offline (hmc.offline=true), so it cannot pass a session-server check; use the offline cells instead.",
        ),
    ]
}


@dataclass
class Case:
    target: str
    scenario: str
    cell: Cell
    status: str = "skip"
    ok: bool | None = None
    duration: float = 0.0
    error: str = ""
    failed_step: str = ""
    case_dir: str = ""
    evidence: dict = field(default_factory=dict)
    sessions: dict = field(default_factory=dict)
    note: str = ""


def resolve_cell(name: str) -> Cell:
    base = name[: -len(PLUGIN_SUFFIX)] if name.endswith(PLUGIN_SUFFIX) else name
    if base not in CELLS:
        raise SystemExit(f"unknown cell {name!r}; known: {', '.join(CELLS)} (append {PLUGIN_SUFFIX} to a proxied cell)")
    cell = CELLS[base]
    if base == name:
        return cell
    if cell.proxy is None:
        raise SystemExit(f"cell {name!r}: plugins only apply to proxied cells")
    return replace(
        cell,
        id=name,
        plugins=True,
        expect="pass",
        known="",
        about=f"{cell.about} The automodpack plugin is loaded.",
    )


def expand_cells(spec: str) -> list[str]:
    plain = [cell.id for cell in CELLS.values() if not cell.skip]
    plugins = [f"{cell.id}{PLUGIN_SUFFIX}" for cell in CELLS.values() if cell.proxy and not cell.skip]
    groups = {"plain": plain, "plugins": plugins, "all": plain + plugins}
    names: list[str] = []
    for item in split(spec):
        for name in groups.get(item, [item]):
            if name not in names:
                names.append(name)
    return names


def split(value: str) -> list[str]:
    return [item.strip() for item in value.split(",") if item.strip()]


def log(message: str) -> None:
    print(message, flush=True)


def port_open(port: int, host: str = "127.0.0.1") -> bool:
    with socket.socket() as sock:
        sock.settimeout(0.5)
        return sock.connect_ex((host, port)) == 0


def jar_targets(artifact_dir: Path) -> set[str] | None:
    jars = sorted(artifact_dir.glob("automodpack-*.jar"))
    if len(jars) != 1:
        return None
    with zipfile.ZipFile(jars[0]) as jar:
        if "impl/manifest.json" not in jar.namelist():
            return None
        manifest = json.loads(jar.read("impl/manifest.json"))
    found = set()
    for entry in manifest.get("impls", []):
        for key in ("id", "target"):
            if isinstance(entry.get(key), str):
                found.add(entry[key])
    return found or None


def render_proxy(home: Path, cell: Cell, args) -> Path:
    if home.exists():
        shutil.rmtree(home)
    (home / "servers").mkdir(parents=True)
    plugins = home / "plugins"
    plugins.mkdir()
    if cell.plugins:
        for wasm in args.plugin_wasm:
            shutil.copy2(wasm, plugins / wasm.name)
        if args.plugin_data:
            shutil.copytree(args.plugin_data, plugins / PLUGIN_ID)
        elif cell.plugin_json:
            (plugins / PLUGIN_ID).mkdir()
            (plugins / PLUGIN_ID / "config.json").write_text(cell.plugin_json + "\n", encoding="utf-8")
    lines = [
        f'bind = "[::]:{args.port}"',
        f'servers_dir = "{home / "servers"}"',
        f'plugins_dir = "{plugins}"',
        'connect_timeout = "5s"',
        "",
        "[rate_limit]",
        "enabled = false",
        "",
    ]
    if cell.plugins:
        lines += [
            "[wasm]",
            f'cache_dir = "{home / "wasm-cache"}"',
            "",
        ]
        lines += [args.plugin_config.read_text(encoding="utf-8"), ""]
    config = home / "infrarust.toml"
    config.write_text("\n".join(lines), encoding="utf-8")
    (home / "servers" / "backend.toml").write_text(
        "\n".join(
            [
                'name = "backend"',
                'domains = ["*"]',
                f'addresses = ["{BACKEND}"]',
                f'proxy_mode = "{cell.proxy}"',
                "",
            ]
        ),
        encoding="utf-8",
    )
    return config


class Proxy:
    def __init__(self, binary: Path, config: Path, log_path: Path, log_level: str):
        self.binary, self.config, self.log_path, self.log_level = binary, config, log_path, log_level
        self.child = None
        self.handle = None

    def start(self, port: int, timeout: float = 30) -> None:
        if port_open(port):
            raise RuntimeError(f"port {port} is already in use; a previous run may have left a proxy behind")
        self.handle = self.log_path.open("w", encoding="utf-8")
        env = dict(os.environ)
        env.pop("RUST_LOG", None)
        self.child = subprocess.Popen(
            [str(self.binary), "--config", str(self.config), "--log-level", self.log_level],
            cwd=self.config.parent,
            stdin=subprocess.DEVNULL,
            stdout=self.handle,
            stderr=subprocess.STDOUT,
            env=env,
        )
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.child.poll() is not None:
                raise RuntimeError(f"proxy exited {self.child.returncode}:\n{tail(self.log_path, 30)}")
            if READY_LINE in self.log_path.read_text(encoding="utf-8", errors="replace"):
                return
            time.sleep(0.2)
        raise RuntimeError(f"proxy never bound within {timeout:.0f}s:\n{tail(self.log_path, 30)}")

    def stop(self) -> None:
        if self.child and self.child.poll() is None:
            self.child.send_signal(signal.SIGTERM)
            try:
                self.child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.child.kill()
                self.child.wait()
        if self.handle:
            self.handle.close()


def tail(path: Path, count: int) -> str:
    try:
        return "\n".join(path.read_text(encoding="utf-8", errors="replace").splitlines()[-count:])
    except OSError:
        return ""


ANSI = re.compile(r"\x1b\[[0-9;]*m")
SIGNAL = re.compile(
    r"(?i)(error|exception|warn|fail|refus|reject|disconnect|lost connection|kick|closed|timed? ?out|invalid|unexpected)"
)
GAME_TOPIC = re.compile(
    r"(?i)(automodpack|holepunch|ammh|magic|takeover|endpoint|handoff|lost connection|disconnect|login|lane)"
)
GAME_ALWAYS = re.compile(r"(AutoModpack endpoint:|connect requested host=|send login start|send magic|connect completed)")
GAME_NOISE = re.compile(
    r"(?i)(Leftovers from an older AutoModpack|Error loading class|Failed to verify authentication|profile key pair|"
    r"InvalidCredentialsException|MinecraftClientHttpException|OFFLINE/INSECURE|Native library|^\s+at )"
)
CAUSE = re.compile(r"^(\S+(Exception|Error|Failure)\b.*|\s+at pl\.skidam\..*)$")
PROXY_READY = "infrarust is ready"
PROXY_SIGNAL = re.compile(
    r"(WARN|ERROR|session started|session ended|client disconnected|connection error|refus|reject|layout=Mismatched)"
)


def shape_of(line: str) -> str:
    return re.sub(r"\d+", "N", re.sub(r"^\S*\s*\[?[\d:.\-TZ ]+\]?", "", line))[:160]


def decisive(path: Path | None, source: str, limit: int = 14) -> list[str]:
    if path is None or not path.is_file():
        return []
    lines = [ANSI.sub("", raw).rstrip() for raw in path.read_text(encoding="utf-8", errors="replace").splitlines()]
    if source == "proxy":
        ready = next((i for i, line in enumerate(lines) if PROXY_READY in line), -1)
        lines = [line for line in lines[ready + 1 :] if PROXY_SIGNAL.search(line) and "shutdown signal" not in line]
    else:
        kept = []
        for index, line in enumerate(lines):
            if GAME_NOISE.search(line):
                continue
            if GAME_ALWAYS.search(line) or (GAME_TOPIC.search(line) and SIGNAL.search(line)):
                kept.append(line)
                if "ERROR" in line:
                    kept += [cause for cause in lines[index + 1 : index + 4] if CAUSE.match(cause)][:2]
        lines = kept
    picked: list[str] = []
    seen: set[str] = set()
    for line in lines:
        shape = shape_of(line)
        if not line or shape in seen:
            continue
        seen.add(shape)
        picked.append(line[:400])
    return picked[-limit:]


SESSION = re.compile(r"session started .*?\busername=(\S+)")
PLAYER = "AutoTest"


CONNECTION_ERROR = re.compile(r"connection error .*?error=\"([^\"]*)\"")


def proxy_sessions(path: Path | None) -> dict:
    if path is None or not path.is_file():
        return {}
    text = ANSI.sub("", path.read_text(encoding="utf-8", errors="replace"))
    names = SESSION.findall(text)
    errors = CONNECTION_ERROR.findall(text)
    return {
        "player": sum(1 for n in names if n == PLAYER),
        "lanes": sum(1 for n in names if n != PLAYER),
        "lenient_login_starts": text.count("layout=Mismatched"),
        "connection_errors": len(errors),
        "first_connection_error": errors[0] if errors else "",
    }


def first(paths) -> Path | None:
    for path in sorted(paths):
        return path
    return None


def interrupt(child: subprocess.Popen, grace: float = 120) -> None:
    child.send_signal(signal.SIGINT)
    try:
        child.wait(timeout=grace)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        child.wait()


def slug_of(case: Case) -> str:
    return f"{case.target}__{case.scenario}__{case.cell.id.replace('+', '_')}"


def run_case(case: Case, args, binary: Path, run_root: Path, proxy_root: Path) -> None:
    cell = case.cell
    slug = slug_of(case)
    out_dir = run_root / slug
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    proxy = None
    proxy_log = None
    command = [
        "uv", "--project", str(args.automodpack / "autotester"), "run", "autotester", "run",
        "--target", case.target,
        "--scenario", case.scenario,
        "--network", "host",
        "--jobs", "1",
        "--artifact-dir", str(args.artifact_dir),
        "--out-dir", str(out_dir),
        "--server-config", f"connectionMode={cell.amp_mode}",
    ]
    for key, value in cell.amp_config:
        command += ["--server-config", f"{key}={value}"]
    if args.container_uid is not None:
        command += ["--docker-uid", str(args.container_uid), "--docker-gid", str(args.container_uid)]
    if cell.proxy:
        command += ["--join-address", f"127.0.0.1:{args.port}"]
        config = render_proxy(proxy_root / slug, cell, args)
        proxy_log = out_dir / "infrarust.log"
        proxy = Proxy(binary, config, proxy_log, args.log_level)
    log(f"==> {case.target} · {case.scenario} · {cell.id}")
    (out_dir / "command.txt").write_text(" ".join(command) + "\n", encoding="utf-8")
    started = time.monotonic()
    autotester_log = out_dir / "autotester.log"
    try:
        for port in [25565, *(value for key, value in cell.amp_config if key == "bindPort")]:
            if port_open(port):
                raise RuntimeError(f"port {port} is already in use; the host-network Minecraft server needs it")
        if proxy:
            proxy.start(args.port)
        with autotester_log.open("w", encoding="utf-8") as handle:
            child = subprocess.Popen(
                command,
                cwd=args.automodpack,
                stdin=subprocess.DEVNULL,
                stdout=handle,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                child.wait(timeout=args.case_timeout)
            except subprocess.TimeoutExpired:
                interrupt(child)
                case.error = f"autotester exceeded {args.case_timeout}s"
            except KeyboardInterrupt:
                interrupt(child)
                raise
    except RuntimeError as error:
        case.error = str(error)
    finally:
        if proxy:
            proxy.stop()
    case.duration = time.monotonic() - started
    (out_dir / "case.json").write_text(json.dumps({"duration": case.duration, "error": case.error}), encoding="utf-8")
    collect(case, out_dir, proxy_log, autotester_log)


def reload_case(case: Case, run_root: Path) -> bool:
    out_dir = run_root / slug_of(case)
    if (out_dir / "case.json").is_file():
        saved = json.loads((out_dir / "case.json").read_text(encoding="utf-8"))
    elif (out_dir / "results.json").is_file():
        results = json.loads((out_dir / "results.json").read_text(encoding="utf-8")).get("results") or [{}]
        saved = {"duration": results[0].get("duration", 0)}
    else:
        return False
    case.duration = float(saved.get("duration", 0))
    case.error = str(saved.get("error") or "")
    proxy_log = out_dir / "infrarust.log"
    collect(case, out_dir, proxy_log if proxy_log.is_file() else None, out_dir / "autotester.log")
    return True


def collect(case: Case, out_dir: Path, proxy_log: Path | None, autotester_log: Path) -> None:
    case.case_dir = str(out_dir)
    results = out_dir / "results.json"
    result = {}
    if results.is_file():
        payload = json.loads(results.read_text(encoding="utf-8"))
        entries = payload.get("results") or []
        result = entries[0] if entries else {}
    case.ok = bool(result.get("ok")) if result else False
    if result.get("error"):
        case.error = str(result["error"]).strip()
    elif not result and not case.error:
        case.error = "the autotester produced no result: " + tail(autotester_log, 5)
    failed = [step for step in result.get("steps", []) if not step.get("ok", True)]
    if failed:
        case.failed_step = str(failed[-1].get("name") or failed[-1].get("verb") or "")
    run_dir = first(path for path in out_dir.glob(f"{case.target}-*") if path.is_dir())
    server_log = first(run_dir.glob("amp-*-s-*.log")) if run_dir else None
    client_out = first(run_dir.glob("amp-*-c-*.log")) if run_dir else None
    client_game = run_dir / "client" / "game" / "logs" / "latest.log" if run_dir else None
    case.evidence = {
        "proxy": decisive(proxy_log, "proxy"),
        "server": decisive(server_log, "server"),
        "client": decisive(client_game if client_game and client_game.is_file() else client_out, "client"),
    }
    case.sessions = proxy_sessions(proxy_log)
    if case.cell.proxy and case.ok and not case.sessions.get("player"):
        case.ok = False
        case.error = "the scenario passed but the proxy never saw the player: the client did not go through Infrarust"
    case.evidence["files"] = {
        "proxy": str(proxy_log) if proxy_log else None,
        "server": str(server_log) if server_log else None,
        "client": str(client_game) if client_game and client_game.is_file() else (str(client_out) if client_out else None),
        "autotester": str(autotester_log),
    }


def classify(cases: list[Case]) -> None:
    by_key = {(c.target, c.scenario, c.cell.id): c for c in cases}
    for case in cases:
        if case.cell.skip:
            case.status = "skip"
            case.note = case.cell.skip
            continue
        if case.ok:
            case.status = "pass" if case.cell.expect == "pass" else "fixed"
        else:
            case.status = "fail" if case.cell.expect == "pass" else "known"
        control = by_key.get((case.target, case.scenario, case.cell.control)) if case.cell.control else None
        if control is not None and control.ok is False and not case.ok:
            case.status = "untrusted"
            case.note = f"the {control.cell.id} control failed too, so this cell proves nothing about the proxy"


def shorten(text: str, width: int = 300) -> str:
    text = " ".join(text.split())
    return text if len(text) <= width else text[: width - 1] + "…"


def render(cases: list[Case], cells: list[Cell], meta: dict) -> str:
    rows = sorted({(c.target, c.scenario) for c in cases}, key=lambda row: (row[1], row[0]))
    by_key = {(c.target, c.scenario, c.cell.id): c for c in cases}
    header = ["target", "scenario", *[cell.id for cell in cells]]
    lines = [
        "# AutoModpack through Infrarust (real modded tier)",
        "",
        "Each cell is AutoModpack's own Docker autotester: a real Fabric/Forge server with",
        "AutoModpack and a real HeadlessMC client, both on host networking, with the",
        "client joining either the server itself or an Infrarust proxy in front of it.",
        "A green cell means the scenario's whole flow passed (certificate trust, modpack",
        "download over the selected AutoModpack connection mode, file verification).",
        "",
        f"Run {meta['started']} · Infrarust `{meta['infrarust']}` · AutoModpack `{meta['automodpack']}` · "
        f"plugin repo `{meta.get('plugin', '?')}` · proxy port {meta['port']}",
        "",
        f"| {' | '.join(header)} |",
        f"|{'|'.join('---' for _ in header)}|",
    ]
    for target, scenario in rows:
        glyphs = []
        for cell in cells:
            case = by_key.get((target, scenario, cell.id))
            if not case or case.status == "skip":
                glyphs.append(GLYPH["skip"])
                continue
            seen = f" · {case.sessions.get('player', 0)}p/{case.sessions.get('lanes', 0)}l" if case.cell.proxy else ""
            glyphs.append(f"{GLYPH[case.status]} {case.duration:.0f}s{seen}")
        lines.append(f"| {target} | {scenario} | {' | '.join(glyphs)} |")
    lines += [
        "",
        "✅ passed as expected · ❌ failed, expected to pass · ⚠️ known gap, failed as expected ·",
        "🎉 expected to fail but passed (update the expectation) · ❔ untrusted, its control failed too · ⏭️ skipped",
        "",
        "`Np/Ml`: sessions the proxy logged for the player (`AutoTest`) and for every other login name,",
        "which are AutoModpack HOLEPUNCH lanes (random names carrying the marker UUID). A proxied cell",
        "that passes without a player session is failed: the client never went through Infrarust.",
        "",
        "## Cells",
        "",
        "| cell | proxy_mode | AutoModpack mode | expected | what it exercises |",
        "|---|---|---|---|---|",
    ]
    for cell in cells:
        lines.append(
            f"| `{cell.id}` | {cell.proxy or '(none)'} | {cell.amp_mode} | {cell.expect} | {cell.about} |"
        )
    proxied = [c for c in cases if c.cell.proxy and c.status != "skip"]
    if proxied:
        lines += [
            "",
            "## What the proxy saw",
            "",
            "| target | scenario | cell | player sessions | lane sessions | lenient Login Starts | connection errors |",
            "|---|---|---|---|---|---|---|",
        ]
        for case in sorted(proxied, key=lambda c: (c.scenario, c.target, c.cell.id)):
            seen = case.sessions
            error = seen.get("first_connection_error", "")
            errors = f"{seen.get('connection_errors', 0)}" + (f" (`{shorten(error, 90)}`)" if error else "")
            lines.append(
                f"| {case.target} | {case.scenario} | `{case.cell.id}` | {seen.get('player', 0)} | {seen.get('lanes', 0)} | "
                f"{seen.get('lenient_login_starts', 0)} | {errors} |"
            )
    noted = [c for c in cases if c.status in ("fail", "known", "untrusted", "fixed")]
    if noted:
        lines += ["", "## Failures and known gaps", ""]
    for case in sorted(noted, key=lambda c: (c.cell.id, c.target)):
        lines += [f"### {case.target} · {case.scenario} · {case.cell.id} {GLYPH[case.status]}", ""]
        if case.cell.known:
            lines += [f"Known gap: {case.cell.known}", ""]
        if case.note:
            lines += [f"Note: {case.note}", ""]
        if case.failed_step:
            lines += [f"Failed step: **{case.failed_step}**", ""]
        if case.error:
            lines += [f"Error: `{shorten(case.error)}`", ""]
        for source in ("proxy", "server", "client"):
            excerpt = case.evidence.get(source) or []
            if excerpt:
                lines += [f"{source.capitalize()} log:", "", "```", *excerpt, "```", ""]
        lines += [f"Logs: `{case.case_dir}`", ""]
    skipped = [c for c in cases if c.status == "skip"]
    if skipped:
        lines += ["", "## Skipped", ""]
        for reason in sorted({c.note for c in skipped}):
            ids = sorted({c.cell.id for c in skipped if c.note == reason})
            lines.append(f"- {', '.join(ids)}: {reason}")
    return "\n".join(lines) + "\n"


def git_head(path: Path) -> str:
    try:
        out = subprocess.run(
            ["git", "-C", str(path), "rev-parse", "--abbrev-ref", "HEAD"], capture_output=True, text=True, check=True
        ).stdout.strip()
        sha = subprocess.run(
            ["git", "-C", str(path), "rev-parse", "--short", "HEAD"], capture_output=True, text=True, check=True
        ).stdout.strip()
        return f"{out}@{sha}"
    except (OSError, subprocess.CalledProcessError):
        return "?"


def link_hmc_cache(run_root: Path, cache: Path) -> None:
    cache.mkdir(parents=True, exist_ok=True)
    link = run_root / ".hmc-cache"
    if link.is_symlink() and link.resolve() == cache.resolve():
        return
    if link.is_symlink() or link.is_file():
        link.unlink()
    elif link.is_dir():
        return
    link.symlink_to(cache.resolve(), target_is_directory=True)


def main() -> int:
    parser = argparse.ArgumentParser(prog="realmodded.sh")
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--automodpack", type=Path, default=os.environ.get("AUTOMODPACK_DIR"))
    parser.add_argument("--artifact-dir", type=Path)
    parser.add_argument("--targets", default=DEFAULT_TARGETS)
    parser.add_argument("--scenarios", default="download-only")
    parser.add_argument("--cells", default=DEFAULT_CELLS)
    parser.add_argument("--port", type=int, default=25580)
    parser.add_argument("--log-level", default="info,infrarust_core=debug")
    parser.add_argument("--plugins-dir", type=Path)
    parser.add_argument("--plugin-config", type=Path, default=DEFAULT_PLUGIN_CONFIG)
    parser.add_argument("--plugin-data", type=Path)
    parser.add_argument("--infrarust", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, default=HERE / ".lab")
    parser.add_argument("--hmc-cache", type=Path)
    parser.add_argument("--case-timeout", type=int, default=2400)
    parser.add_argument("--container-uid", type=int)
    parser.add_argument("--report", default="real-modded")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--report-only", action="store_true")
    args = parser.parse_args()

    if args.list:
        for cell in CELLS.values():
            flag = f" (skipped: {cell.skip})" if cell.skip else ""
            extra = " ".join(f"{k}={v}" for k, v in cell.amp_config)
            log(f"{cell.id:21} proxy={cell.proxy or '-':12} amp={cell.amp_mode:9} {extra:44} expect={cell.expect}{flag}")
        log(f"append {PLUGIN_SUFFIX} to a proxied cell to load the plugin into it (expected to pass), e.g. offline{PLUGIN_SUFFIX}")
        log("groups for --cells: plain (every runnable cell without the plugin, the default), plugins (every +plugins cell), all")
        return 0
    if not args.automodpack:
        parser.error("--automodpack DIR (or AUTOMODPACK_DIR) is required")
    args.automodpack = args.automodpack.resolve()
    if not (args.automodpack / "autotester" / "settings.yaml").is_file():
        parser.error(f"{args.automodpack} is not an AutoModpack checkout with an autotester/")
    args.artifact_dir = (args.artifact_dir or args.automodpack / "merged").resolve()
    cells = [resolve_cell(name) for name in expand_cells(args.cells)]
    args.plugin_wasm = sorted(args.plugins_dir.glob("*.wasm")) if args.plugins_dir else [DEFAULT_WASM]
    if any(cell.plugins for cell in cells) and not args.report_only:
        missing_wasm = [str(path) for path in args.plugin_wasm if not path.is_file()] or ([] if args.plugin_wasm else ["*.wasm"])
        if missing_wasm:
            parser.error(
                f"cells ending in {PLUGIN_SUFFIX} need the plugin: build it with --build-plugin or pass --plugins-dir "
                f"(missing {', '.join(missing_wasm)})"
            )
        if not args.plugin_config.is_file():
            parser.error(f"--plugin-config {args.plugin_config} does not exist")
        if args.plugin_data and not args.plugin_data.is_dir():
            parser.error(f"--plugin-data {args.plugin_data} is not a directory")
    targets = split(args.targets)
    scenarios = split(args.scenarios)

    run_root = args.work_dir.resolve() / "runs"
    proxy_root = args.work_dir.resolve() / "proxies"
    cases = [Case(target, scenario, cell) for scenario in scenarios for target in targets for cell in cells]
    meta = {
        "started": datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC"),
        "infrarust": git_head(args.infrarust),
        "automodpack": git_head(args.automodpack),
        "plugin": git_head(REPO),
        "port": args.port,
        "targets": targets,
        "scenarios": scenarios,
        "cells": [cell.id for cell in cells],
        "plugin_wasm": [str(path) for path in args.plugin_wasm] if any(cell.plugins for cell in cells) else [],
    }
    if args.report_only:
        previous = HERE / "reports" / f"{args.report}.json"
        if previous.is_file():
            meta["started"] = json.loads(previous.read_text(encoding="utf-8")).get("meta", {}).get("started", meta["started"])
        cases = [c for c in cases if c.cell.skip or reload_case(c, run_root)]
        return finish(cases, cells, meta, args)

    help_text = subprocess.run(
        ["uv", "--project", str(args.automodpack / "autotester"), "run", "autotester", "run", "--help"],
        cwd=args.automodpack, capture_output=True, text=True,
    ).stdout
    if "--join-address" not in help_text or "--server-config" not in help_text:
        parser.error(
            "this autotester has no --join-address/--server-config; check out the AutoModpack branch "
            "infrarust-autotest (or any autotester with those options)"
        )
    present = jar_targets(args.artifact_dir)
    if present is None:
        parser.error(f"no single automodpack-*.jar one-jar in {args.artifact_dir}; build it with --build-automodpack")
    missing = [t for t in targets if t not in present]
    if missing:
        parser.error(f"the one-jar in {args.artifact_dir} has no impl for {', '.join(missing)} (it has {', '.join(sorted(present))})")

    run_root.mkdir(parents=True, exist_ok=True)
    link_hmc_cache(run_root, (args.hmc_cache or args.automodpack / "autotester" / ".hmc-cache").resolve())

    try:
        for case in cases:
            if case.cell.skip:
                log(f"==> {case.target} · {case.scenario} · {case.cell.id}: skipped")
                continue
            run_case(case, args, args.binary.resolve(), run_root, proxy_root)
            log(f"    {'PASS' if case.ok else 'FAIL'} in {case.duration:.0f}s" + (f" · {shorten(case.error, 200)}" if case.error and not case.ok else ""))
    except KeyboardInterrupt:
        log("interrupted; writing the report for the cases that ran")
        cases = [c for c in cases if c.ok is not None or c.cell.skip]
    return finish(cases, cells, meta, args)


def finish(cases: list[Case], cells: list[Cell], meta: dict, args) -> int:
    classify(cases)

    reports = HERE / "reports"
    reports.mkdir(exist_ok=True)
    (reports / f"{args.report}.md").write_text(render(cases, cells, meta), encoding="utf-8")
    payload = {
        "meta": meta,
        "cases": [
            {
                "target": c.target,
                "scenario": c.scenario,
                "cell": c.cell.id,
                "proxy_mode": c.cell.proxy,
                "automodpack_mode": c.cell.amp_mode,
                "plugins": c.cell.plugins,
                "expect": c.cell.expect,
                "status": c.status,
                "ok": c.ok,
                "duration": round(c.duration, 1),
                "failed_step": c.failed_step,
                "error": c.error,
                "note": c.note,
                "proxy_sessions": c.sessions,
                "case_dir": c.case_dir,
                "evidence": c.evidence,
            }
            for c in cases
        ],
    }
    (reports / f"{args.report}.json").write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    log(f"report: {reports / (args.report + '.md')}")
    counts = {status: sum(1 for c in cases if c.status == status) for status in GLYPH}
    log(" · ".join(f"{GLYPH[s]} {n}" for s, n in counts.items() if n))
    if counts["fail"]:
        return 1
    if counts["fixed"]:
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
