#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

echo "[quality] Rust formatting"
cargo fmt --manifest-path src-tauri/Cargo.toml --check

echo "[quality] Rust lint"
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --locked --offline -- -D warnings

echo "[quality] frontend lint"
pnpm run lint

echo "[quality] frontend formatting"
pnpm run format:check

echo "[cross-platform] host unit tests"
cargo test --manifest-path src-tauri/Cargo.toml --locked

for target in \
  aarch64-apple-darwin \
  x86_64-apple-darwin \
  x86_64-unknown-linux-gnu \
  x86_64-pc-windows-msvc; do
  if rustup target list --installed | grep -qx "$target"; then
    if [[ "$target" == "x86_64-pc-windows-msvc" ]] && ! command -v clang-cl >/dev/null 2>&1 && [[ -z "${VCINSTALLDIR:-}" ]]; then
      echo "[cross-platform] Windows MSVC 工具链/SDK 不在当前主机，跳过本地交叉编译；CI 会在 Windows 强制执行。"
      continue
    fi
    echo "[cross-platform] $target compile check"
    cargo test --manifest-path src-tauri/Cargo.toml --target "$target" --no-run --locked
  else
    echo "[cross-platform] $target 未安装，跳过本地交叉编译；CI 会在对应平台强制执行。"
  fi
done
