#!/usr/bin/env bash
# Validate a native CI installer and exercise the app's default DSH startup path.
set -euo pipefail

if [[ $# != 3 || "$1" != /* || ! -f "$1" || "$3" != /* || "$3" != *.png ]]; then
  echo "Usage: $0 <absolute plain Intel DMG> <app version> <absolute screenshot PNG>" >&2
  exit 2
fi

dmg=$1
expected_version=$2
screenshot=$3
if [[ "$(uname -s)" != Darwin || "$(uname -m)" != x86_64 || "$(id -u)" == 0 ]]; then
  echo "Smoke test requires a non-root native Intel macOS runner" >&2
  exit 2
fi
sw_vers

app_config="$HOME/Library/Application Support/cn.bingoogolapple.dsh"
if [[ -e "$app_config" || -e "$HOME/.dsh/bga-dsh-client" ]]; then
  echo "Smoke test requires a fresh app configuration and empty app npm cache" >&2
  exit 2
fi

scratch="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/dsh-macos-smoke.XXXXXXXX")"
mount_dir="$scratch/volume"
mounted=0
cleanup() {
  local status=$?
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

# Compile before launch so screenshot tooling adds no time to the cold start.
script_dir="$(cd -- "$(dirname -- "$0")" && pwd)"
swiftc "$script_dir/capture-macos-window.swift" -o "$scratch/capture-window"
mkdir -p -- "$(dirname -- "$screenshot")"
rm -f -- "$screenshot"

python3 - "$app" "$app_config" "$expected_version" "$screenshot" <<'PY'
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
screenshot = Path(sys.argv[4])
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
subprocess.run(["lipo", str(exe), "-verify_arch", "x86_64"], check=True)

if (app / "Contents/Resources/runtime").exists():
    raise SystemExit("Plain installer unexpectedly contains the bundled runtime")
node_version = subprocess.check_output(["node", "--version"], text=True).strip()
expected_node = os.environ.get("NODE_VERSION", "").removeprefix("v")
if expected_node and node_version != f"v{expected_node}":
    raise SystemExit(f"Unexpected system Node version: {node_version}")
print(f"System Node: {node_version}", flush=True)
subprocess.run(["npx", "--version"], check=True)

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

print("Launching the installed Intel app with no configuration or npm cache", flush=True)
started = time.monotonic()
with app_log.open("wb") as output:
    proc = subprocess.Popen([exe], cwd=scratch, env=env, stdout=output, stderr=subprocess.STDOUT,
                            start_new_session=True)
    groups = set()
    try:
        deadline = time.monotonic() + 780
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
        if "npx --yes @deepseek-ai/dsh@latest web --no-open" not in service_log.read_text(errors="replace"):
            raise RuntimeError("App did not use the default npx latest command")

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
        packages = list((app_config / "npm-cache/_npx").glob("*/node_modules/@deepseek-ai/dsh/package.json"))
        if len(packages) != 1:
            raise RuntimeError("Expected one dsh package in the newly populated app npm cache")
        dsh_version = json.loads(packages[0].read_text())["version"]
        seconds = time.monotonic() - started
        print(f"App {expected_version} (Intel macOS, plain) started dsh {dsh_version} after {seconds:.1f} seconds. "
              "Unauthenticated HTTP 401 and authenticated HTTP 200 confirmed.", flush=True)
        subprocess.run([scratch / "capture-window", str(proc.pid), str(screenshot)], check=True, timeout=120)
        print("PASS: Captured the running Intel macOS client with its dsh interface.", flush=True)
    except Exception:
        show_failure_logs()
        raise
    finally:
        stop_app(proc, groups)
        shutil.rmtree(app_config, ignore_errors=True)
PY
