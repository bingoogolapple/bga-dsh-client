#!/usr/bin/env bash
# 一键修改内置运行时版本（node / pnpm / dsh）：同步 bundle-runtime.mjs（全仓唯一版本源，
# CI 与 build-release.sh 构建时自动读取）与 RUNTIME-VERSIONING.md 表格，可选重建验证。
# 用法：
#   ./scripts/set-runtime-version.sh                 # 交互式：选择组件 → 选择/输入版本
#   ./scripts/set-runtime-version.sh node v24.11.0   # 非交互：直接改指定组件
#   ./scripts/set-runtime-version.sh pnpm 10.1.0 dsh 0.2.0   # 一次改多个
#   ./scripts/set-runtime-version.sh dsh 0.2.0 --no-rebuild  # 只改文件，不重建运行时
#   ./scripts/set-runtime-version.sh node v24.11.0 --dry-run  # 只预览要改什么

set -euo pipefail
cd "$(dirname "$0")/.."

DRY=0
REBUILD=1
CLI_PARTS=()   # 命令行传入的 (kind value) 对
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY=1; shift ;;
    --no-rebuild) REBUILD=0; shift ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    node|pnpm|dsh)
      [[ $# -ge 2 ]] || { echo "缺少版本号: $1 <版本>"; exit 1; }
      CLI_PARTS+=("$1" "$2"); shift 2 ;;
    *) echo "未知参数: $1（支持 node|pnpm|dsh <版本> / --dry-run / --no-rebuild）" >&2; exit 1 ;;
  esac
done

need() { command -v "$1" >/dev/null 2>&1 || { echo "缺少依赖命令: $1"; exit 1; }; }
need node; need python3

# ---------- 读当前版本（唯一来源：bundle-runtime.mjs 默认值） ----------
read_cur() { # kind(NODE|PNPM|DSH) -> echo 当前版本
  local kind="$1"
  local line
  line=$(grep -m1 "const ${kind}.*= '" scripts/bundle-runtime.mjs || true)
  echo "$line" | sed -E "s/.*= '([^']+)'.*/\1/" | tr -d " \t\r"
}
CUR_NODE="$(read_cur NODE)"; CUR_PNPM="$(read_cur PNPM)"; CUR_DSH="$(read_cur DSH)"

# ---------- 文本替换（精确 + 断言唯一语义） ----------
edit_file() { # path old new —— DRY=1 时只预览
  local p="$1" old="$2" new="$3" rc
  rc=0
  python3 - "$p" "$old" "$new" "$DRY" << 'EOF' || rc=$?
import sys
p, old, new, dry = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4] == '1'
s = open(p, encoding='utf-8').read()
n = s.count(old)
if n == 0:
    print(f"  ⚠ {p}: 未找到 {old!r}（可能已改过？），跳过")
    sys.exit(2)
tag = "将改" if dry else "已改"
print(f"  {tag} {p}: {old} → {new}（{n} 处）")
if not dry:
    open(p, 'w', encoding='utf-8').write(s.replace(old, new))
EOF
  [[ $rc -eq 2 ]] && return 0   # 未命中算警告不中断
  return $rc
}

# ---------- 各组件修改 ----------
apply_node() { # newver
  local new="$1" old="$CUR_NODE"
  [[ "$new" == "$old" ]] && { echo "node 版本无变化（${old}），跳过"; return; }
  edit_file scripts/bundle-runtime.mjs     "= '$old'"                    "= '$new'"
  edit_file RUNTIME-VERSIONING.md                  "| Node.js | \`$old\` |"       "| Node.js | \`$new\` |"
  CUR_NODE="$new"
}

apply_pnpm() {
  local new="$1" old="$CUR_PNPM"
  [[ "$new" == "$old" ]] && { echo "pnpm 版本无变化（${old}），跳过"; return; }
  edit_file scripts/bundle-runtime.mjs     "= '$old'"                       "= '$new'"
  edit_file RUNTIME-VERSIONING.md                  "| pnpm | \`$old\` |"             "| pnpm | \`$new\` |"
  CUR_PNPM="$new"
}

apply_dsh() {
  local new="$1" old="$CUR_DSH"
  [[ "$new" == "$old" ]] && { echo "dsh 版本无变化（${old}），跳过"; return; }
  edit_file scripts/bundle-runtime.mjs     "= '$old'"                         "= '$new'"
  edit_file RUNTIME-VERSIONING.md                  "| @deepseek-ai/dsh | \`$old\` |"   "| @deepseek-ai/dsh | \`$new\` |"
  CUR_DSH="$new"
}

# ---------- 版本选择（联网时给出候选，否则手输） ----------
ask_node() {
  local list v
  # 只取正式版（版本号不含 '-'，过滤 nightly/rc 等 pre-release），最近 30 个
  list=$(curl -s -m 20 https://nodejs.org/dist/index.json 2>/dev/null \
    | python3 -c "import json,sys; d=json.load(sys.stdin); lst=[x['version'] for x in d if '-' not in x['version']]; print('\n'.join(lst[:30]))" 2>/dev/null || true)
  if [[ -n "$list" ]]; then
    echo "Node.js 可选版本（最近 30 个正式版）:" >&2   # 提示走 stderr，避免被 $(ask_node) 捕获
    select v in $list 手动输入; do
      [[ -n "$v" ]] && break
    done
  fi
  while [[ -z "${v:-}" || "$v" == "手动输入" ]]; do
    read -rp "输入 node 版本（如 v24.10.0，可省略 v）: " v
  done
  [[ "$v" != v* ]] && v="v$v"
  echo "$v"
}

ask_npm_pkg() { # pkg
  local pkg="$1" list v cond
  # pnpm 等正式版发布流程的包只展示正式版；@deepseek-ai/dsh 至今全是 rc 预发布
  # （无正式版），不过滤。三者列表均倒序（最新在前）：npm versions 是升序，
  # 取最后 20 个后反转；node 的 index.json 本身即降序。
  if [[ "$pkg" == "@deepseek-ai/dsh" ]]; then
    cond="True"
    label="最近 20 个版本"
  else
    cond="'-' not in x"   # 过滤 nightly/rc/beta 等 pre-release
    label="最近 20 个正式版"
  fi
  list=$(npm view "$pkg" versions --json 2>/dev/null \
    | python3 -c "import json,sys; v=json.load(sys.stdin); v=v if isinstance(v,list) else v.get('versions',[]); lst=[x for x in v if $cond]; print('\n'.join(lst[-20:][::-1]))" 2>/dev/null || true)
  if [[ -n "$list" ]]; then
    echo "$pkg 可选版本（${label}）:" >&2   # 提示走 stderr，避免被 $(ask_npm_pkg) 捕获
    select v in $list 手动输入; do
      [[ -n "$v" ]] && break
    done
  fi
  while [[ -z "${v:-}" || "$v" == "手动输入" ]]; do
    read -rp "输入 $pkg 版本（如 $(echo "$list" | head -1)；latest 不推荐，会漂移）: " v
  done
  echo "$v"
}

# ---------- 组装要改的 (kind, value) ----------
declare -a PAIRS=()
if [[ ${#CLI_PARTS[@]} -gt 0 ]]; then
  i=0
  while [[ $i -lt ${#CLI_PARTS[@]} ]]; do
    PAIRS+=("${CLI_PARTS[$i]}" "${CLI_PARTS[$((i+1))]}")
    i=$((i+2))
  done
else
  echo "当前内置运行时：node $CUR_NODE / pnpm $CUR_PNPM / dsh $CUR_DSH"
  echo "选择要升级的组件："
  select kind in node pnpm dsh 全部; do
    case "$kind" in
      node) PAIRS+=(node    "$(ask_node)") ;;
      pnpm) PAIRS+=(pnpm    "$(ask_npm_pkg pnpm)") ;;
      dsh)  PAIRS+=(dsh     "$(ask_npm_pkg @deepseek-ai/dsh)") ;;
      全部)
        PAIRS+=(node "$(ask_node)")
        PAIRS+=(pnpm "$(ask_npm_pkg pnpm)")
        PAIRS+=(dsh  "$(ask_npm_pkg @deepseek-ai/dsh)")
        ;;
    esac
    break
  done
fi

# ---------- 应用修改 ----------
[[ ${#PAIRS[@]} -eq 0 ]] && { echo "没有要改的组件"; exit 0; }
echo ""
echo "==> 将要修改（$( [[ $DRY == 1 ]] && echo 预览 || echo 生效 )）："
for ((i=0; i<${#PAIRS[@]}; i+=2)); do
  local_kind="${PAIRS[$i]}"; local_val="${PAIRS[$((i+1))]}"
  echo "  - ${local_kind}: $(read_cur "$(echo "$local_kind" | tr a-z A-Z | cut -c1-4)") → $local_val"
done
if [[ $DRY == 1 ]]; then
  for ((i=0; i<${#PAIRS[@]}; i+=2)); do
    case "${PAIRS[$i]}" in node) apply_node "${PAIRS[$((i+1))]}" ;;
                               pnpm) apply_pnpm "${PAIRS[$((i+1))]}" ;;
                               dsh)  apply_dsh  "${PAIRS[$((i+1))]}" ;; esac
  done
  echo "（dry-run 结束，未做任何修改）"
  exit 0
fi

if [[ $REBUILD == 0 ]]; then
  echo "==> 开始修改（--no-rebuild）…"
else
  echo "==> 开始修改…"
fi
for ((i=0; i<${#PAIRS[@]}; i+=2)); do
  case "${PAIRS[$i]}" in node) apply_node "${PAIRS[$((i+1))]}" ;;
                             pnpm) apply_pnpm "${PAIRS[$((i+1))]}" ;;
                             dsh)  apply_dsh  "${PAIRS[$((i+1))]}" ;; esac
done

# ---------- 重建 + 验证 ----------
[[ $REBUILD == 0 ]] && { echo "完成（未重建）。下次打包（release.yml 或 build-release.sh）会自动重建并校验。"; git diff --stat; exit 0; }

echo ""
echo "==> 重建内置运行时（下载/安装 + 瘦身，需几分钟）…"
rm -rf src-tauri/resources/runtime
node scripts/bundle-runtime.mjs

echo ""
echo "==> 验证（新运行时）:"
NODE_BIN="src-tauri/resources/runtime/nd/bin/node"
DSH_BIN="src-tauri/resources/runtime/rt/node_modules/@deepseek-ai/dsh/lib/bin.js"
PNPM_BIN="src-tauri/resources/runtime/rt/node_modules/pnpm/bin/pnpm.cjs"
printf "  node : %s\n" "$("$NODE_BIN" --version)"
printf "  dsh  : %s\n" "$("$NODE_BIN" "$DSH_BIN" --version)"
printf "  pnpm : %s\n" "$("$NODE_BIN" "$PNPM_BIN" --version)"
echo "  manifest:"
python3 - << 'EOF'
import json
m = json.load(open('src-tauri/resources/runtime/runtime-manifest.json'))
print(f"    node={m['nodeVersion']} dsh={m['dshVersion']} pnpm={m['pnpmVersion']}")
EOF

echo ""
echo "==> 提醒："
echo "  1. 若 dsh/pnpm 版本变化，请提交新的 src-tauri/resources/runtime/rt/package.json 与 package-lock.json（保证后续 npm ci 秒装）"
echo "  2. CI 发版前请把改动一起推送；cache key 已随版本自动失效"
echo "  3. 改动摘要："
git diff --stat -- scripts/bundle-runtime.mjs RUNTIME-VERSIONING.md