#!/usr/bin/env bash
# DSH_SCREENSHOT_PATH optionally retains the verified real window as a PNG.
# DSH_SMOKE_VERSION additionally exercises an explicitly selected DSH version.
set -euo pipefail

if [[ "${1:-}" != --in-display ]]; then
  if [[ $# != 5 || "$1" != /* || ! -f "$1" ]]; then
    echo "Usage: $0 <absolute deb> <plain|bundled> <app version> <node version> <bundled dsh version>" >&2
    exit 2
  fi
  deb="$1"
  variant="$2"
  app_version="$3"
  [[ "$variant" == plain || "$variant" == bundled ]]

  . /etc/os-release
  printf 'Testing on %s (%s)\n' "$PRETTY_NAME" "$(dpkg --print-architecture)"
  [[ "$ID" == ubuntu && "$VERSION_ID" == 24.04 ]]
  getconf GNU_LIBC_VERSION
  dpkg-deb -f "$deb" Package Version Architecture Depends
  package="$(dpkg-deb -f "$deb" Package)"
  [[ "$(dpkg-deb -f "$deb" Version)" == "$app_version" ]]
  [[ "$(dpkg-deb -f "$deb" Architecture)" == amd64 && "$(dpkg --print-architecture)" == amd64 ]]
  if dpkg-query -W -f='${db:Status-Status}\n' "$package" 2>/dev/null | grep -Fx installed; then
    echo "Package $package is already installed" >&2
    exit 1
  fi

  sudo apt-get -o APT::Update::Error-Mode=any -o Acquire::Retries=3 update
  sudo apt-get -o Acquire::Retries=3 install -y "$deb" xvfb xauth dbus-x11 x11-utils openbox wmctrl \
    scrot tesseract-ocr tesseract-ocr-chi-sim python3-pil python3-gi gir1.2-atspi-2.0 \
    at-spi2-core gsettings-desktop-schemas fonts-noto-cjk
  dpkg-query -W -f='${Status} ${Version}\n' "$package" | grep -Fx "install ok installed $app_version"
  probe_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/dsh-linux.XXXXXXXX")"
  screenshot="${DSH_SCREENSHOT_PATH:-$probe_dir/client-window.png}"
  if [[ "$screenshot" != /* || "$screenshot" != *.png ]]; then
    echo 'DSH_SCREENSHOT_PATH must be an absolute PNG path' >&2
    exit 2
  fi
  # Session helpers can write to the XDG directories until Xvfb and D-Bus exit.
  # Keep the smoke test's exit status if the runner must finish removing scratch files.
  trap 'rm -rf -- "$probe_dir" || echo "Unable to fully remove $probe_dir" >&2' EXIT
  xvfb-run -a --server-args='-screen 0 1920x1200x24' \
    dbus-run-session -- bash "$(realpath -e "$0")" --in-display "$variant" "$app_version" "$4" "$5" "$probe_dir" "$screenshot"
  exit
fi

variant="$2"
app_version="$3"
node_version="v${4#v}"
bundled_dsh_version="$5"
app=/usr/bin/DeepSeekHarness
runtime=/usr/lib/DeepSeekHarness/runtime
test -x "$app"
test ! -e "$HOME/.dsh/bga-dsh-client/settings.json"
if [[ "$variant" == plain ]]; then
  test ! -e "$runtime"
  expected_node="$(readlink -f "$(command -v node)")"
  command -v npx
else
  expected_node="$runtime/nd/bin/node"
  test -x "$expected_node"
fi
test "$("$expected_node" --version)" = "$node_version"

probe_dir="$6"
screenshot="$7"
script_dir="$(cd -- "$(dirname -- "$0")" && pwd)"
# The selected-version regression must never write a real user's settings or
# version directory. Default cold starts use this same isolated HOME too.
export HOME="$probe_dir/home"
mkdir -p "$HOME"
export XDG_CONFIG_HOME="$probe_dir/config"
export XDG_CACHE_HOME="$probe_dir/cache"
export XDG_DATA_HOME="$probe_dir/data"
export DSH_HOME="$probe_dir/dsh"
export DSH_AGENTS_HOME="$probe_dir/agents"
export npm_config_cache="$probe_dir/empty-external-npm-cache"
export DO_NOT_TRACK=1 LIBGL_ALWAYS_SOFTWARE=1 NO_AT_BRIDGE=0 GDK_SCALE=1 GDK_DPI_SCALE=1
app_dir="$XDG_CONFIG_HOME/cn.bingoogolapple.dsh"
cd "$probe_dir"

selected_version="${DSH_SMOKE_VERSION:-}"
if [[ -n "$selected_version" ]]; then
  python3 - "$selected_version" <<'PY_VERSION'
import re
import sys
version = sys.argv[1]
if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?", version) or ".." in version:
    raise SystemExit("DSH_SMOKE_VERSION must be a concrete package version")
PY_VERSION
  version_dir="$HOME/.dsh/bga-dsh-client/dsh-versions/$selected_version"
  mkdir -p "$version_dir"
  if [[ "$variant" == bundled ]]; then
    npm_command=("$expected_node" "$runtime/nd/lib/node_modules/npm/bin/npm-cli.js")
  else
    npm_command=("$(command -v npm)")
  fi
  timeout 600s "${npm_command[@]}" install --prefix "$version_dir" --save-exact --no-audit --no-fund \
    "@deepseek-ai/dsh@$selected_version"
  python3 - "$HOME/.dsh/bga-dsh-client/settings.json" "$selected_version" <<'PY_SETTINGS'
import json
from pathlib import Path
import sys
path = Path(sys.argv[1])
path.parent.mkdir(parents=True, exist_ok=True)
path.write_text(json.dumps({"dsh_version": sys.argv[2], "stop_service_on_quit": True}))
PY_SETTINGS
fi

cleanup() {
  result=$?
  if ((result != 0)); then
    wmctrl -lp >&2 || true
    shopt -s nullglob
    for log in "$probe_dir/window-manager.log" "$probe_dir/app.log" "$app_dir/service.log" "$app_dir/npm-cache/_logs/"*-debug-0.log; do
      if [[ -f "$log" ]]; then
        echo "--- $(basename "$log") (last 30 lines) ---" >&2
        tail -30 "$log" | sed -E 's/(token=)[A-Za-z0-9_-]+/\1[redacted]/gi; s/^.*(_authToken|authorization|password)[[:space:]]*[:=].*$/[redacted credential line]/i' >&2
      fi
    done
  fi
  if [[ -n "${app_pid:-}" ]]; then
    while read -r group; do
      kill -- "-$group" 2>/dev/null || true
    done < <(ps -eo pid=,ppid=,pgid= | awk -v parent="$app_pid" '$2 == parent && $1 == $3 {print $1}')
    kill "$app_pid" 2>/dev/null || true
  fi
  if [[ -n "${wm_pid:-}" ]]; then
    kill "$wm_pid" 2>/dev/null || true
  fi
  exit "$result"
}
trap cleanup EXIT

if [[ -n "$(ss -H -ltn 'sport = :3080')" ]]; then
  echo 'Expected an unused dsh port' >&2
  exit 1
fi

openbox >"$probe_dir/window-manager.log" 2>&1 &
wm_pid=$!
for ((attempt=0; attempt<15; attempt++)); do
  if wmctrl -m >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
wmctrl -m >/dev/null
# Start the accessibility bus before WebKitGTK creates either document.
gsettings set org.gnome.desktop.interface toolkit-accessibility true
gdbus call --session --dest org.a11y.Bus --object-path /org/a11y/bus \
  --method org.a11y.Bus.GetAddress >/dev/null

started=$SECONDS
"$app" >"$probe_dir/app.log" 2>&1 &
app_pid=$!
for ((attempt=0; attempt<45; attempt++)); do
  kill -0 "$app_pid"
  window="$(wmctrl -lp | awk -v pid="$app_pid" '$3 == pid && !found {print $1; found=1}')"
  if [[ -n "$window" ]] && xwininfo -id "$window" 2>/dev/null | grep -Fq 'Map State: IsViewable'; then
    break
  fi
  sleep 1
done
test -n "$window"
xwininfo -id "$window" | grep -F 'Map State: IsViewable'
echo 'Installed desktop window is visible'

deadline=$((started + 780))
while (( SECONDS < deadline )); do
  kill -0 "$app_pid"
  if [[ -f "$app_dir/service.log" ]] && grep -Eq 'Service failed to start|服务启动失败' "$app_dir/service.log"; then
    echo 'Installed app reported that dsh failed to start' >&2
    exit 1
  fi
  http="$(curl --noproxy '*' --max-time 2 -s -o /dev/null -w '%{http_code}' http://127.0.0.1:3080/ || true)"
  if [[ "$http" == 401 && -s "$app_dir/service.pid" && -s "$app_dir/launch-token" ]]; then
    break
  fi
  sleep 1
done
if [[ "$http" != 401 || ! -s "$app_dir/service.pid" || ! -s "$app_dir/launch-token" ]]; then
  echo 'Timed out waiting for the installed app to start authenticated dsh' >&2
  exit 1
fi

listener="$(ss -H -ltnp 'sport = :3080')"
[[ "$listener" =~ pid=([0-9]+) ]]
listener_pid="${BASH_REMATCH[1]}"
service_pid="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["pid"])' "$app_dir/service.pid")"
test "$(ps -o ppid= -p "$service_pid" | tr -d '[:space:]')" = "$app_pid"
test "$(ps -o pgid= -p "$listener_pid" | tr -d '[:space:]')" = "$service_pid"
test "$(readlink -f "/proc/$listener_pid/exe")" = "$(readlink -f "$expected_node")"
if [[ -n "$selected_version" ]]; then
  cli="$HOME/.dsh/bga-dsh-client/dsh-versions/$selected_version/node_modules/@deepseek-ai/dsh/lib/bin.js"
  tr '\0' '\n' <"/proc/$listener_pid/cmdline" | grep -Fxq "$cli"
elif [[ "$variant" == plain ]]; then
  grep -Fq 'npx --yes @deepseek-ai/dsh@latest web --no-open' "$app_dir/service.log"
  cli_arg="$(tr '\0' '\n' <"/proc/$listener_pid/cmdline" | grep -E '/_npx/.*/(dsh|@deepseek-ai/dsh/lib/bin\.js)$')"
  cli="$(readlink -f "$cli_arg")"
  [[ "$cli" == "$app_dir/npm-cache/_npx/"*/node_modules/@deepseek-ai/dsh/lib/bin.js ]]
else
  cli="$runtime/rt/node_modules/@deepseek-ai/dsh/lib/bin.js"
  tr '\0' '\n' <"/proc/$listener_pid/cmdline" | grep -Fxq "$cli"
fi
test -f "$cli"
dsh_version="$(timeout 15s "$expected_node" "$cli" --version)"
if [[ -n "$selected_version" ]]; then
  test "$dsh_version" = "$selected_version"
elif [[ "$variant" == bundled ]]; then
  test "$dsh_version" = "$bundled_dsh_version"
fi

python3 - "$app_dir/launch-token" <<'PY_AUTH'
import http.cookiejar
import pathlib
import sys
import urllib.parse
import urllib.request

token = pathlib.Path(sys.argv[1]).read_text().strip()
if not token:
    raise SystemExit("dsh launch token is empty")
cookies = http.cookiejar.CookieJar()
opener = urllib.request.build_opener(
    urllib.request.ProxyHandler({}),
    urllib.request.HTTPCookieProcessor(cookies),
)
query = urllib.parse.urlencode({"token": token})
try:
    with opener.open("http://127.0.0.1:3080/?" + query, timeout=10) as response:
        if response.status != 200 or response.headers.get_content_type() != "text/html" or not list(cookies):
            raise RuntimeError("dsh did not serve HTML with a session cookie")
except Exception as error:
    raise SystemExit(f"Authenticated Web page failed: {type(error).__name__}") from None
PY_AUTH

kill -0 "$app_pid"
/usr/bin/python3 "$script_dir/capture-linux-window.py" "$app_pid" "$window" "$screenshot"
echo 'PASS: The real Linux WebKitGTK window showed DSH content before and after toolbar refresh'
printf 'PASS: App %s (Ubuntu 24.04, %s) started dsh %s with Node %s after %s seconds. HTTP 401 and authenticated HTTP 200 confirmed.\n' \
  "$app_version" "$variant" "$dsh_version" "$node_version" "$((SECONDS - started))"
