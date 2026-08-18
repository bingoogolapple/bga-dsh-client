#!/usr/bin/env bash
# 双版本打包：内置 Node.js 版（bundled） / 现状非内置版（plain）
# 用法：
#   ./scripts/build-release.sh [plain|bundled|two]
#     plain    默认：不携带 Node/dsh 运行时（依赖系统 npx/dsh/pnpm）—— 现状打包
#     bundled  捆绑 Node + dsh（src-tauri/resources/runtime），离线可用，产物名带 -bundled
#     two      一次出两个版本：dist/release/plain + dist/release/bundled
# 需先 npm install（装 @tauri-apps/cli）；内置版打包前会按需生成内置运行时。
# 发版前先 ./scripts/set-app-version.sh 对齐客户端应用版本，
# 保证 tauri.conf.json 版本与 tag 一致（release.yml 有校验）。
# 内置运行时的版本（node/pnpm/dsh）定义与修改入口见 RUNTIME-VERSIONING.md。

set -euo pipefail
cd "$(dirname "$0")/.."

variant="${1:-plain}"
if [[ "$variant" != "plain" && "$variant" != "bundled" && "$variant" != "two" ]]; then
  echo "用法: $0 [plain|bundled|two]" >&2
  exit 1
fi

# 内置运行时版本来自 bundle-runtime.mjs 常量（唯一事实来源，本文件不硬编码）
default_ver() { # NODE_VER | DSH_VERSION | PNPM_VERSION
  sed -nE "s/.*const $1 = '([^']+)'.*/\1/p" scripts/bundle-runtime.mjs | head -1
}
DEFAULT_NODE_VER="$(default_ver NODE_VER)"
DEFAULT_DSH_VERSION="$(default_ver DSH_VERSION)"
DEFAULT_PNPM_VERSION="$(default_ver PNPM_VERSION)"

# 内置版附加配置：把 runtime 打进包 + 产物名加 -bundled 后缀与普通版区分
BUNDLE_EXTRA=()
if [[ "$variant" == "bundled" || "$variant" == "two" ]]; then
  # manifest 与目标（版本 + 平台）一致才复用；否则删除重建。
  # 平台参与比较：本地/CI 换平台构建（如交叉 RUNTIME_TARGET、Linux runner）时，
  # 防止把别的平台的 runtime 静默复用（node 二进制与 prebuilds 是平台相关的）。
  runtime_fresh() {
    local mf="src-tauri/resources/runtime/runtime-manifest.json"
    [[ -f "$mf" ]] || return 1
    # 目标平台与 bundle-runtime.mjs 的 manifest.platform（process.platform / RUNTIME_TARGET）对齐：
    # Windows 的 Git Bash uname 输出 MINGW64_*，需映射回 win32
    local plat="${RUNTIME_TARGET:-}"
    if [[ -z "$plat" ]]; then
      case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) plat="win32" ;;
        *) plat="$(uname -s | tr A-Z a-z)" ;;
      esac
    fi
    node -e '
      const fs = require("fs")
      const m = JSON.parse(fs.readFileSync(process.argv[1], "utf8"))
      const [dn, dd, dp, plat] = process.argv.slice(2)
      const want = { nodeVersion: dn, dshVersion: dd, pnpmVersion: String(dp), platform: plat }
      if (m.nodeVersion === want.nodeVersion && m.dshVersion === want.dshVersion && m.pnpmVersion === want.pnpmVersion && m.platform === want.platform) process.exit(0)
      console.error(`[build-release] 已有 runtime ${m.nodeVersion}/${m.dshVersion}/${m.pnpmVersion}(${m.platform}) 与目标 ${want.nodeVersion}/${want.dshVersion}/${want.pnpmVersion}(${want.platform}) 不一致，重新生成`)
      process.exit(1)
    ' "$mf" "$DEFAULT_NODE_VER" "$DEFAULT_DSH_VERSION" "$DEFAULT_PNPM_VERSION" "$plat"
  }
  if runtime_fresh; then
    echo "复用 src-tauri/resources/runtime（$(du -sh src-tauri/resources/runtime | cut -f1)）"
  else
    echo "生成/重建内置运行时（版本：node $DEFAULT_NODE_VER / dsh $DEFAULT_DSH_VERSION / pnpm $DEFAULT_PNPM_VERSION）…"
    rm -rf src-tauri/resources/runtime
    node scripts/bundle-runtime.mjs
  fi
  BUNDLE_EXTRA=(--config '{"bundle":{"resources":{"resources/runtime":"runtime"}}}')
fi

build_one() {
  variant="$1"
  out_dir="dist/release/$variant"
  rm -rf "$out_dir"
  mkdir -p "$out_dir"

  echo "==> tauri build ($variant)…"
  if [[ "$variant" == "bundled" ]]; then
    npm run tauri -- build "${BUNDLE_EXTRA[@]:-}"
  else
    npm run tauri -- build
  fi

  bundle_root="src-tauri/target/release/bundle"
  if [[ -d "$bundle_root/macos" ]]; then
    cp -R "$bundle_root/macos/"*.app "$out_dir/" 2>/dev/null || true
  fi
  if [[ -d "$bundle_root/dmg" ]]; then
    cp "$bundle_root/dmg/"*.dmg "$out_dir/" 2>/dev/null || true
  fi
  if [[ -d "$bundle_root/nsis" ]]; then
    cp "$bundle_root/nsis/"*.exe "$out_dir/" 2>/dev/null || true
  fi
  # Linux（deb 目标；appimage 产物若在也一并归档）
  if [[ -d "$bundle_root/deb" ]]; then
    cp "$bundle_root/deb/"*.deb "$out_dir/" 2>/dev/null || true
  fi
  if [[ -d "$bundle_root/appimage" ]]; then
    cp "$bundle_root/appimage/"*.AppImage "$out_dir/" 2>/dev/null || true
  fi
  # Tauri v2 的产物名不可经配置覆盖，内置版用后缀区分：
  #   dmg → DeepSeek..._0.0.2_aarch64-bundled.dmg；deb/appimage/exe 同理追加 -bundled
  if [[ "$variant" == "bundled" ]]; then
    for f in "$out_dir/"*.dmg "$out_dir/"*.deb "$out_dir/"*.AppImage "$out_dir/"*.exe; do
      [[ -f "$f" ]] && mv "$f" "${f%.*}-bundled.${f##*.}"
    done
    # Tauri 用 UDZO(zlib-9) 压 dmg；LZFSE(ULFO) 压缩率更高（约 -8%），再压一遍。
    # 注意 hdiutil -o 若路径不以 .dmg 结尾会自动补后缀，故用临时文件名再 mv。
    for f in "$out_dir/"*-bundled.dmg; do
      [[ -f "$f" ]] || continue
      tmp="$out_dir/.ulfo-tmp.dmg"
      hdiutil convert "$f" -format ULFO -o "$tmp" >/dev/null 2>&1 \
        && mv -f "$tmp" "$f" \
        && echo "  dmg 已转 ULFO（LZFSE）: $(du -h "$f" | cut -f1)"
    done
  fi
  echo "==> 产物: $out_dir"
  ls -la "$out_dir"
}

if [[ "$variant" == "two" ]]; then
  build_one plain
  build_one bundled
else
  build_one "$variant"
fi