#!/usr/bin/env bash
set -euo pipefail

# 用法：./scripts/set-app-version.sh 0.2.0
# 作用：修改【客户端应用版本】（与 set-runtime-version.sh 的 node/pnpm/dsh 运行时版本无关）。
# 统一修改三处版本号：package.json / src-tauri/Cargo.toml / src-tauri/tauri.conf.json，
# 并同步被 git 跟踪的 src-tauri/Cargo.lock。
# 配套 .github/workflows/release.yml：打 v* tag 前先跑本脚本，保证
# tauri.conf.json 里的版本（决定 dmg 文件名）与 tag 一致。
# 注意：sed -i '' 是 macOS（BSD）写法，Linux 上需改成 sed -i。

cd "$(dirname "$0")/.."

if [ $# -ne 1 ]; then
  echo "用法: $0 <新版本号，semver 格式，如 0.2.0 或 0.2.0-beta.1>" >&2
  exit 1
fi
new="$1"

if ! printf '%s' "$new" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'; then
  echo "错误: 版本号 '$new' 不是合法 semver（例: 0.2.0、0.2.0-beta.1）" >&2
  exit 1
fi

# 读取三处当前版本（Cargo.toml 匹配行首 version，避免误伤依赖的 version = "2" 等）
pkg_old=$(sed -nE 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' package.json | head -1)
conf_old=$(sed -nE 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' src-tauri/tauri.conf.json | head -1)
cargo_old=$(sed -nE 's/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' src-tauri/Cargo.toml | head -1)

echo "当前版本: package.json=$pkg_old  tauri.conf.json=$conf_old  Cargo.toml=$cargo_old"

if [ -z "$pkg_old" ] || [ -z "$conf_old" ] || [ -z "$cargo_old" ]; then
  echo "错误: 某处未读取到版本号，请确认文件结构" >&2
  exit 1
fi
if [ "$pkg_old" != "$conf_old" ] || [ "$pkg_old" != "$cargo_old" ]; then
  echo "错误: 三处版本号不一致，请先手动对齐后再使用本脚本" >&2
  exit 1
fi

old="$pkg_old"
if [ "$old" = "$new" ]; then
  echo "版本号未变化（${old}），无需修改"
  exit 0
fi

sed -i '' "s/^[[:space:]]*\"version\": \"$old\"/\"version\": \"$new\"/" package.json
sed -i '' "s/^[[:space:]]*\"version\": \"$old\"/\"version\": \"$new\"/" src-tauri/tauri.conf.json
sed -i '' "s/^version = \"$old\"/version = \"$new\"/" src-tauri/Cargo.toml
echo "已更新: $old -> $new"

# 同步被 git 跟踪的 Cargo.lock 中本包条目的 version（只改 name = "bga-dsh-client" 后
# 紧跟的 version 行，等价于 cargo 构建时的自动更新，且不依赖网络/索引刷新）。
awk -v new="$new" -v old="$old" '
  /^name = "bga-dsh-client"$/ { seen = 1 }
  seen && /^version = / {
    # 字符串替换（非正则）：版本号里的 .、+ 等字符不具正则语义。
    idx = index($0, old)
    if (idx > 0) { $0 = substr($0, 1, idx - 1) new substr($0, idx + length(old)) }
    seen = 0
  }
  { print }
' src-tauri/Cargo.lock > src-tauri/Cargo.lock.tmp && mv src-tauri/Cargo.lock.tmp src-tauri/Cargo.lock

echo "--- 修改后 ---"
echo "package.json:                 $(sed -nE 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' package.json | head -1)"
echo "src-tauri/Cargo.toml:         $(sed -nE 's/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' src-tauri/Cargo.toml | head -1)"
echo "src-tauri/tauri.conf.json:    $(sed -nE 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' src-tauri/tauri.conf.json | head -1)"
echo
echo "发版示例:"
echo "  git add -A && git commit -m \"release v$new\""
echo "  git tag v$new && git push origin main --tags"