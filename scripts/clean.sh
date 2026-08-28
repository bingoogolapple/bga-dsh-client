#!/usr/bin/env bash
# 清理构建产物，回收磁盘空间。
#
# 背景：src-tauri/target/ 在长期开发后会累积到 10GB+（debug + release 双 profile、
# 每次 rustc 版本升级后的重建产物都在里面）。本脚本按粒度提供清理选项。
#
# 用法：
#   ./scripts/clean.sh            # 默认：清 debug 产物（最常用，保留 release）
#   ./scripts/clean.sh --all      # 清整个 target/（最彻底，下次全量重编译）
#   ./scripts/clean.sh --dist     # 只清 dist/（发布产物）
#   ./scripts/clean.sh --dry-run  # 只显示将释放多少空间，不实际删除
#
# 经 pnpm 调用（可加 `--`，本脚本会忽略该分隔符）：
#   pnpm run clean -- --dry-run
#   pnpm run clean:all
#
# 注意：清理只会拖慢下一次编译（需要重新编译），不会丢失任何源码或配置。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="src-tauri/target"
DIST="dist"

#  humana 可读的目录大小（macOS 与 Linux 的 du 都支持 -sh，但 stat 差异大，统一用 du）
dir_size() {
  if [ -d "$1" ]; then
    # du 的输出是右对齐的（macOS 上形如 " 14G\tpath"），需去掉前导空白
    du -sh "$1" 2>/dev/null | cut -f1 | tr -d '[:space:]'
  else
    echo "0"
  fi
}

# 目标体积（用于展示清理效果）
before=$(dir_size "$TARGET")
before_dist=$(dir_size "$DIST")

DRY_RUN=0
MODE="debug"

for arg in "$@"; do
  case "$arg" in
    # 吞掉 `--` 分隔符：`pnpm run clean -- --dry-run` 里的 `--` 会被 pnpm
    # 原样透传给脚本（npm 会剥掉，pnpm 不会），不处理就会落进 `*)` 分支报错。
    --) continue ;;
    --all) MODE="all" ;;
    --dist) MODE="dist" ;;
    --dry-run) DRY_RUN=1 ;;
    -h | --help)
      awk 'NR==1{next} /^#/{sub(/^# ?/,""); print; next} {exit}' "${BASH_SOURCE[0]}"
      exit 0
      ;;
    *)
      # 必须写 ${arg}：紧随其后的全角括号在非 C locale 下会被 bash 当成变量名的一部分
      # （报 `arg（...: unbound variable`）而不是预期的提示语。
      echo "未知参数: ${arg}" >&2
      echo "" >&2
      awk 'NR==1{next} /^#/{sub(/^# ?/,""); print; next} {exit}' "${BASH_SOURCE[0]}" >&2
      exit 1
      ;;
  esac
done

echo "清理前：target/ = ${before}，dist/ = ${before_dist}"

run() {
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "[dry-run] $*"
  else
    echo "执行: $*"
    "$@"
  fi
}

case "$MODE" in
  debug)
    # 只删 debug profile：保留 release 与依赖编译缓存（deps/），
    # 这样下次 dev 编译只需重编本项目代码，依赖不用重来。
    if [ -d "$TARGET/debug" ]; then
      run rm -rf "${TARGET:?}/debug"
    else
      echo "target/debug 不存在，跳过"
    fi
    ;;
  all)
    if [ -d "$TARGET" ]; then
      run rm -rf "${TARGET:?}"
    else
      echo "target/ 不存在，跳过"
    fi
    ;;
  dist)
    if [ -d "$DIST" ]; then
      run rm -rf "${DIST:?}"
    else
      echo "dist/ 不存在，跳过"
    fi
    ;;
esac

if [ "$DRY_RUN" -eq 1 ]; then
  echo ""
  echo "（--dry-run：未实际删除任何文件）"
else
  after=$(dir_size "$TARGET")
  after_dist=$(dir_size "$DIST")
  echo ""
  echo "清理后：target/ = ${after}，dist/ = ${after_dist}"
  echo "提示：dev profile 已配置 debug=0（见 src-tauri/Cargo.toml），"
  echo "      日常开发不会再产生数百 MB 的调试信息，无需频繁清理。"
fi
