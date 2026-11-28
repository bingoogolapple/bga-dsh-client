#!/usr/bin/env bash
# Validate a native CI installer and exercise the app's default DSH startup path.
# DSH_SMOKE_VERSION additionally exercises an explicitly selected DSH version.
set -euo pipefail

if [[ $# -lt 3 || $# -gt 5 ]]; then
  echo "Usage: $0 <absolute DMG> <app version> <absolute screenshot PNG or ''> [plain|bundled] [absolute app ZIP]" >&2
  exit 2
fi

dmg=$1
expected_version=$2
screenshot=$3
variant=${4:-plain}
archive=${5:-}
if [[ "$dmg" != /* || ! -f "$dmg" || -z "$expected_version" ||
      ( -n "$screenshot" && ( "$screenshot" != /* || "$screenshot" != *.png ) ) ||
      ( -n "$archive" && ( "$archive" != /* || ! -f "$archive" ) ) ]]; then
  echo "Supply an absolute DMG, the app version, and valid screenshot or ZIP paths if used" >&2
  exit 2
fi
if [[ "$variant" != plain && "$variant" != bundled ]]; then
  echo "Unknown package variant: $variant" >&2
  exit 2
fi
arch=$(uname -m)
if [[ "$(uname -s)" != Darwin || ( "$arch" != x86_64 && "$arch" != arm64 ) || "$(id -u)" == 0 ]]; then
  echo "Smoke test requires a non-root native macOS runner" >&2
  exit 2
fi
sw_vers
script_dir="$(cd -- "$(dirname -- "$0")" && pwd)"

app_config="$HOME/Library/Application Support/cn.bingoogolapple.dsh"
settings_dir="$HOME/.dsh/bga-dsh-client"
if [[ -e "$app_config" || -e "$settings_dir" ]]; then
  echo "Smoke test requires a fresh app configuration and empty app npm cache" >&2
  exit 2
fi

scratch="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/dsh-macos-smoke.XXXXXXXX")"
mount_dir="$scratch/volume"
mounted=0
cleanup() {
  local status=$?
  if [[ -f "$scratch/owns-settings" ]]; then
    rm -rf -- "$settings_dir"
  fi
  if [[ "$mounted" == 1 ]]; then
    if ! hdiutil detach "$mount_dir" && ! hdiutil detach -force "$mount_dir"; then
      echo "Unable to detach the test image: $mount_dir" >&2
      if [[ "$status" == 0 ]]; then status=1; fi
      exit "$status"
    fi
  fi
  rm -rf -- "$scratch"
  exit "$status"
}
trap cleanup EXIT

mkdir "$mount_dir"
hdiutil verify "$dmg"
hdiutil attach -readonly -nobrowse -noautoopen -mountpoint "$mount_dir" "$dmg"
mounted=1
if [[ ! -L "$mount_dir/Applications" || "$(readlink "$mount_dir/Applications")" != /Applications ]]; then
  echo "Missing Applications link in DMG" >&2
  exit 1
fi

app="$scratch/DeepSeekHarness.app"
ditto "$mount_dir/DeepSeekHarness.app" "$app"
hdiutil detach "$mount_dir" || hdiutil detach -force "$mount_dir"
mounted=0
codesign --verify --strict "$app"

# The ZIP is an alternative installer. Compare its app with the app that will
# be cold-started from the DMG, including executable modes and symlink targets.
if [[ -n "$archive" ]]; then
  mkdir "$scratch/zip"
  ditto -x -k "$archive" "$scratch/zip"
  codesign --verify --strict "$scratch/zip/DeepSeekHarness.app"
  python3 - "$app" "$scratch/zip/DeepSeekHarness.app" <<'PY'
import filecmp
import os
from pathlib import Path
import stat
import sys


def compare(left, right, relative):
    left_mode, right_mode = left.lstat().st_mode, right.lstat().st_mode
    if left_mode != right_mode:
        raise SystemExit(f"DMG and ZIP file modes differ at {relative}")
    if stat.S_ISDIR(left_mode):
        entries = {entry.name for entry in left.iterdir()}
        if entries != {entry.name for entry in right.iterdir()}:
            raise SystemExit(f"DMG and ZIP have different directory contents at {relative}")
        for name in sorted(entries):
            compare(left / name, right / name, relative / name)
    elif stat.S_ISLNK(left_mode):
        if os.readlink(left) != os.readlink(right):
            raise SystemExit(f"DMG and ZIP symlink targets differ at {relative}")
    elif not stat.S_ISREG(left_mode) or not filecmp.cmp(left, right, shallow=False):
        raise SystemExit(f"DMG and ZIP files differ at {relative}")


compare(Path(sys.argv[1]), Path(sys.argv[2]), Path("DeepSeekHarness.app"))
print("ZIP and DMG app contents, permissions and symbolic links match", flush=True)
PY
  rm -rf -- "$scratch/zip"
fi

# Every installer must show the DSH interface, even when no screenshot artifact
# was requested. Compile before launch so this adds no time to the cold start.
if [[ -z "$screenshot" ]]; then screenshot="$scratch/client-window.png"; fi
swiftc "$script_dir/capture-macos-window.swift" -o "$scratch/capture-window"
mkdir -p -- "$(dirname -- "$screenshot")"
rm -f -- "$screenshot"

if [[ "$variant" == bundled ]]; then
  NODE_VERSION=${NODE_VERSION:-$(sed -nE "s/^const NODE_VER = '([^']+)'$/\1/p" "$script_dir/bundle-runtime.mjs")}
  BUNDLED_DSH_VERSION=${BUNDLED_DSH_VERSION:-$(sed -nE "s/^const DSH_VERSION = '([^']+)'$/\1/p" "$script_dir/bundle-runtime.mjs")}
  export NODE_VERSION BUNDLED_DSH_VERSION
fi

python3 - "$app" "$app_config" "$expected_version" "$arch" "$variant" "$screenshot" <<'PY'
import http.cookiejar
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

app = Path(sys.argv[1]).resolve()
app_config = Path(sys.argv[2])
expected_version = sys.argv[3]
arch, variant, screenshot = sys.argv[4:7]
scratch = app.parent
base_url = "http://127.0.0.1:3080/"
app_log = scratch / "app.log"
service_log = app_config / "service.log"
token_path = app_config / "launch-token"
pid_path = app_config / "service.pid"

with (app / "Contents/Info.plist").open("rb") as stream:
    info = plistlib.load(stream)
if info["CFBundleShortVersionString"] != expected_version:
    raise SystemExit("The installer contains the wrong App version")
exe = app / "Contents/MacOS" / info["CFBundleExecutable"]
subprocess.run(["lipo", str(exe), "-verify_arch", arch], check=True)

runtime = app / "Contents/Resources/runtime"
expected_node = os.environ.get("NODE_VERSION", "").removeprefix("v")
if variant == "bundled":
    node = runtime / "nd/bin/node"
    entry = runtime / "rt/node_modules/@deepseek-ai/dsh/lib/bin.js"
    manifest = json.loads((runtime / "runtime-manifest.json").read_text())
    expected_dsh = os.environ["BUNDLED_DSH_VERSION"]
    expected_runtime = {"nodeVersion": f"v{expected_node}", "dshVersion": expected_dsh,
                        "platform": "darwin", "arch": "arm64" if arch == "arm64" else "x64"}
    if any(manifest.get(key) != value for key, value in expected_runtime.items()):
        raise SystemExit("The bundled runtime manifest does not match the target or requested versions")
    subprocess.run(["lipo", str(node), "-verify_arch", arch], check=True)
    node_version = subprocess.check_output([node, "--version"], text=True).strip()
    dsh_version = json.loads((entry.parents[1] / "package.json").read_text())["version"]
    if not entry.is_file():
        raise SystemExit("The bundled dsh entry point is missing")
    if not expected_node or not expected_dsh or node_version != f"v{expected_node}" or dsh_version != expected_dsh:
        raise SystemExit(f"Unexpected bundled Node or dsh version: {node_version}, {dsh_version}")
    print(f"Bundled Node: {node_version}; bundled dsh: {dsh_version}", flush=True)
    expected_command = f'exec "{node}" "{entry}" web --no-open'
else:
    if runtime.exists():
        raise SystemExit("Plain installer unexpectedly contains the bundled runtime")
    node_version = subprocess.check_output(["node", "--version"], text=True).strip()
    if expected_node and node_version != f"v{expected_node}":
        raise SystemExit(f"Unexpected system Node version: {node_version}")
    print(f"System Node: {node_version}", flush=True)
    subprocess.run(["npx", "--version"], check=True)
    expected_command = "exec npx --yes @deepseek-ai/dsh@latest web --no-open"

# Ensure port 3080 cannot refer to a reused external service.
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as port:
    try:
        port.bind(("127.0.0.1", 3080))
    except OSError:
        raise SystemExit("Port 3080 is already in use")

def child_process_groups(parent):
    rows = subprocess.check_output(["ps", "-axo", "pid=,ppid=,pgid="], text=True)
    return {
        pid for pid, ppid, pgid in (map(int, row.split()) for row in rows.splitlines() if row.strip())
        if ppid == parent and pgid == pid
    }

def stop_app(proc, groups):
    # The App deliberately gives its DSH subprocess a separate process group.
    groups.update(child_process_groups(proc.pid))
    groups.add(proc.pid)
    for signum in (signal.SIGTERM, signal.SIGKILL):
        for group in groups:
            try:
                os.killpg(group, signum)
            except ProcessLookupError:
                pass
        if signum == signal.SIGTERM:
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
    proc.wait(timeout=5)

def show_failure_logs():
    token = token_path.read_text().strip() if token_path.is_file() else ""
    npm_logs = list((app_config / "npm-cache/_logs").glob("*-debug-0.log"))
    newest = sorted(npm_logs, key=lambda path: path.stat().st_mtime)[-1:]
    for log in (app_log, service_log, *newest):
        if not log.is_file():
            continue
        body = log.read_text(errors="replace")
        if token:
            body = body.replace(token, "<redacted>")
        body = re.sub(r"(?i)([?&]token=)[^\s&\"'<>]+", r"\1<redacted>", body)
        body = re.sub(r"(?im)^.*(?:authorization|_authToken|_password)\s*[:=].*$",
                      "<redacted credential line>", body)
        print(f"--- {log.name} (last 25 lines) ---", file=sys.stderr)
        print("\n".join(body.splitlines()[-25:]), file=sys.stderr)

env = os.environ.copy()
env.update(
    DSH_HOME=str(scratch / "dsh"),
    DSH_AGENTS_HOME=str(scratch / "agents"),
    DO_NOT_TRACK="1",
    npm_config_cache=str(scratch / "empty-external-npm-cache"),
)

selected_version = os.environ.get("DSH_SMOKE_VERSION", "")
selected_entry = None
selected_node = None
if selected_version:
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise SystemExit("Selected DSH smoke requires a fresh GitHub Actions runner")
    if (not re.fullmatch(r"\d+\.\d+\.\d+(?:-[A-Za-z0-9][A-Za-z0-9.-]*)?", selected_version)
            or ".." in selected_version):
        raise SystemExit("DSH_SMOKE_VERSION must be a concrete package version")
    settings_dir = Path.home() / ".dsh/bga-dsh-client"
    if settings_dir.exists() or app_config.exists():
        raise SystemExit("Selected DSH smoke requires a fresh client profile")
    system_node = shutil.which("node")
    system_npm = shutil.which("npm")
    if variant == "plain" and (not system_node or not system_npm):
        raise SystemExit("Selected plain DSH smoke requires system Node and npm")
    selected_node = node if variant == "bundled" else Path(system_node).resolve()
    npm_command = ([str(node), str(runtime / "nd/lib/node_modules/npm/bin/npm-cli.js")]
                   if variant == "bundled" else [system_npm])
    # The shell's EXIT trap removes only this exclusively created test profile,
    # including when npm preparation fails before the app-launch try/finally.
    settings_dir.mkdir(parents=True, exist_ok=False)
    (scratch / "owns-settings").touch()
    version_dir = settings_dir / "dsh-versions" / selected_version
    version_dir.mkdir(parents=True)
    subprocess.run(
        [*npm_command, "install", "--prefix", str(version_dir), "--save-exact",
         "--no-audit", "--no-fund", "--registry", "https://registry.npmjs.org",
         f"@deepseek-ai/dsh@{selected_version}"],
        cwd=scratch, env=env, check=True, timeout=600,
    )
    selected_entry = version_dir / "node_modules/@deepseek-ai/dsh/lib/bin.js"
    if not selected_entry.is_file():
        raise SystemExit("Selected DSH entry point is missing")
    installed_version = json.loads((selected_entry.parents[1] / "package.json").read_text())["version"]
    reported_version = subprocess.check_output(
        [selected_node, selected_entry, "--version"], cwd=scratch, env=env,
        text=True, timeout=30,
    ).strip()
    if installed_version != selected_version or reported_version != selected_version:
        raise SystemExit("Selected DSH package and CLI version do not match")
    (settings_dir / "settings.json").write_text(json.dumps({
        "dsh_version": selected_version, "stop_service_on_quit": True,
        "telemetry_enabled": False,
    }))
    launch_node = str(node) if variant == "bundled" else "node"
    expected_command = f'exec "{launch_node}" "{selected_entry}" web --no-open'
    print(f"Prepared selected dsh {selected_version} for the {arch} {variant} installer", flush=True)

startup_mode = f"selected dsh {selected_version}" if selected_version else "no configuration or npm cache"
print(f"Launching the installed {arch} {variant} app with {startup_mode}", flush=True)
started = time.monotonic()
with app_log.open("wb") as output:
    proc = subprocess.Popen([exe], cwd=scratch, env=env, stdout=output, stderr=subprocess.STDOUT,
                            start_new_session=True)
    groups = set()
    try:
        deadline = time.monotonic() + (780 if variant == "plain" else 240)
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                raise RuntimeError(f"App exited before DSH startup ({proc.returncode})")
            children = child_process_groups(proc.pid)
            groups.update(children)
            try:
                token = token_path.read_text().strip()
                service_pid = json.loads(pid_path.read_text())["pid"]
            except (FileNotFoundError, json.JSONDecodeError, KeyError):
                if service_log.is_file():
                    log = service_log.read_text(errors="replace")
                    if "Service failed to start" in log or "服务启动失败" in log:
                        raise RuntimeError("Installed app reported that dsh failed to start")
                time.sleep(0.5)
                continue
            if token and service_pid in children:
                break
            time.sleep(0.5)
        else:
            raise RuntimeError("Timed out waiting for the app's own DSH process and launch token")
        if expected_command not in service_log.read_text(errors="replace"):
            raise RuntimeError(f"App did not use the expected {variant} startup command")
        listeners = subprocess.check_output(
            ["lsof", "-nP", "-iTCP:3080", "-sTCP:LISTEN", "-t"], text=True
        ).splitlines()
        own_listeners = [pid for pid in listeners if os.getpgid(int(pid)) == service_pid]
        if not own_listeners:
            raise RuntimeError("The dsh listener does not belong to the app's service process group")
        if selected_version:
            matched_listener = False
            for pid in own_listeners:
                executable_files = subprocess.check_output(
                    ["lsof", "-a", "-p", pid, "-d", "txt", "-Fn"], text=True,
                ).splitlines()
                matching_node = any(
                    line.startswith("n") and Path(line[1:]).resolve() == selected_node.resolve()
                    for line in executable_files
                )
                command = subprocess.check_output(["ps", "-ww", "-p", pid, "-o", "args="], text=True)
                if matching_node and str(selected_entry) in command:
                    matched_listener = True
                    break
            if not matched_listener:
                raise RuntimeError("The selected DSH listener did not use the expected Node and entry point")

        cookies = http.cookiejar.CookieJar()
        client = urllib.request.build_opener(
            urllib.request.ProxyHandler({}),
            urllib.request.HTTPCookieProcessor(cookies),
        )
        try:
            with client.open(base_url, timeout=10) as response:
                raise RuntimeError(f"Unauthenticated DSH unexpectedly returned HTTP {response.status}")
        except urllib.error.HTTPError as exc:
            if exc.code != 401:
                raise RuntimeError(f"Unexpected unauthenticated status HTTP {exc.code}") from None
        try:
            query = urllib.parse.urlencode({"token": token})
            with client.open(f"{base_url}?{query}", timeout=10) as response:
                if response.status != 200 or response.headers.get_content_type() != "text/html" or not list(cookies):
                    raise RuntimeError("Authenticated DSH did not serve HTML with a session cookie")
        except urllib.error.HTTPError as exc:
            raise RuntimeError(f"DSH authentication returned HTTP {exc.code}") from None
        except urllib.error.URLError:
            raise RuntimeError("DSH authentication failed to connect") from None

        if proc.poll() is not None:
            raise RuntimeError("App exited while checking the DSH service")
        npx_cache = app_config / "npm-cache/_npx"
        if selected_version:
            dsh_version = json.loads((selected_entry.parents[1] / "package.json").read_text())["version"]
            if dsh_version != selected_version or npx_cache.exists():
                raise RuntimeError("Selected DSH startup used the wrong package or unexpectedly populated the npx cache")
        elif variant == "plain":
            packages = list(npx_cache.glob("*/node_modules/@deepseek-ai/dsh/package.json"))
            if len(packages) != 1:
                raise RuntimeError("Expected one dsh package in the newly populated app npm cache")
            dsh_version = json.loads(packages[0].read_text())["version"]
        elif npx_cache.exists():
            raise RuntimeError("Bundled installer unexpectedly populated the app npx cache")
        seconds = time.monotonic() - started
        print(f"App {expected_version} (macOS {arch}, {variant}) started dsh {dsh_version} after {seconds:.1f} seconds. "
              "Unauthenticated HTTP 401 and authenticated HTTP 200 confirmed.", flush=True)
        subprocess.run([scratch / "capture-window", str(proc.pid), screenshot], check=True, timeout=120)
        print("PASS: Captured the running macOS client with its dsh interface.", flush=True)
    except Exception:
        show_failure_logs()
        raise
    finally:
        stop_app(proc, groups)
        shutil.rmtree(app_config, ignore_errors=True)
PY
