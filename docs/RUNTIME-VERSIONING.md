# RUNTIME VERSIONING — 内置 Node.js 运行时版本管理

本文说明 **bga-dsh-client 内置版**（`DeepSeekHarness-…-bundled`）中三项运行时组件的版本
在哪里定义、如何升级（推荐一键脚本）、手工改动要同步哪些文件、以及如何验证。

涉及组件与当前默认版本：

| 组件 | 当前默认版本 | 作用 |
|---|---|---|
| Node.js | `v24.19.0` | 执行 dsh 的运行时（`runtime/nd/bin/node`） |
| @deepseek-ai/dsh | `0.1.0-rc.7` | DSH 本体（`runtime/rt/node_modules/…/lib/bin.js`） |
| pnpm | `11.22.0` | 供 dsh 的 `dsh plugin` 命令转发调用（插件管理） |

> 三者是**相互独立的版本链**，可分别升级；非内置版（plain）不携带任何运行时，
> 与本文无关。

---

## 0. 一键脚本（强烈推荐）

`./scripts/set-runtime-version.sh` 是版本升级的**首选入口**：自动完成「bundle-runtime.mjs
（唯一版本源）+ RUNTIME-VERSIONING.md 表格更新 + 可选重建 runtime + 冒烟验证」，
避免手工改漏导致 CI/本地版本不一致。

### 用法

```bash
./scripts/set-runtime-version.sh                                   # 交互式：选组件 → 选/输版本
./scripts/set-runtime-version.sh dsh 0.2.0                         # 直接改 dsh
./scripts/set-runtime-version.sh node v24.11.0 pnpm 10.1.0         # 一次改多个
./scripts/set-runtime-version.sh dsh 0.2.0 --no-rebuild            # 只改文件，不重建运行时
./scripts/set-runtime-version.sh dsh 0.2.0 --dry-run               # 只预览改动（零副作用）
./scripts/set-runtime-version.sh --help                            # 查看用法
```

### 交互模式说明（不带参数时）

1. 先打印当前版本：`当前内置运行时：node v24.10.0 / pnpm 11.22.0 / dsh 0.1.0-rc.7`；
2. 选择要升级的组件：`node` / `pnpm` / `dsh` / `全部`（全部会依次询问三个版本）；
3. **联网时**自动列出候选版本供编号选择：
   - node：nodejs.org `dist/index.json` **最近 30 个正式版**（过滤 nightly/rc）；
   - pnpm：`npm view pnpm versions` **最近 20 个正式版**（过滤 rc/beta）；
   - dsh：`npm view @deepseek-ai/dsh versions` **最近 20 个版本**（该包至今只有
     `-rc.x` 预发布、无正式版，故不过滤，否则候选会为空）；
   - 也可选「手动输入」（断网时直接手输）；
   - node 版本可省略 `v` 前缀，脚本自动补全。
4. 版本与当前相同会自动跳过该组件。

### 行为

- **改动文件**（全部精确替换，命中数逐一打印）：
  - `scripts/bundle-runtime.mjs` — 版本唯一事实来源（**构建/CI 自动读取，无需改其他**）；
  - `RUNTIME-VERSIONING.md` — 本页「当前默认版本」表格。
  - （release.yml / build-release.sh 均不硬编码版本，无需触碰。）
- **默认重建**：`rm -rf src-tauri/resources/runtime && node scripts/bundle-runtime.mjs`，
  然后用捆绑 node 冒烟输出三件套并对照 manifest：

  ```
  node : v24.11.0
  dsh  : 0.2.0
  pnpm : 11.22.1
  manifest: node=… dsh=… pnpm=…
  ```

- 结束时打印 `git diff --stat` 并提醒：dsh/pnpm 版本变化需提交新的
  `rt/package.json` + `rt/package-lock.json`；CI 的 runtime 缓存 key 已随版本自动失效。
- `--dry-run` 全程不落盘，只预览要改什么，建议正式执行前先跑一次。

---

## 1. 版本定义的入口（全链路）

版本**不依赖任何环境变量**（已移除 `BUNDLE_NODE_VER` / `BUNDLE_DSH_VERSION` /
`BUNDLE_PNPM_VERSION` 覆盖通道）。唯一需要维护的地方是 `scripts/bundle-runtime.mjs`
的三个常量（②），CI（①）与本地校验（③）都在构建时自动读取它：

### ① CI 发版（自动跟随，无需改版本）
`.github/workflows/release.yml` **不硬编码任何版本**：构建的第一步（`读取内置运行时版本`
step）用 sed 从 ② 提取三个常量并写入 step 输出，之后的 runtime 缓存 key 与
`./scripts/build-release.sh two` 都使用它们：

```yaml
- name: 读取内置运行时版本
  id: ver
  run: |
    echo "NODE_VER=$(sed -nE "s/.*const NODE_VER = '([^']+)'.*/\1/p" scripts/bundle-runtime.mjs | head -1)" >> "$GITHUB_OUTPUT"
    ...
```

- 该 step 输出的版本参与 `actions/cache` 的缓存 key
  （`dsh-runtime-<arch>-<node>-<dsh>-<pnpm>-<hash>`）。
  **只要 ② 的任一版本变化，key 自动失效 → 该架构下次发版重新生成运行时并存入新缓存。**

### ② 本地构建与 CI 共同的事实来源（唯一入口）
`scripts/bundle-runtime.mjs` 第 21 / 23 / 24 行：

```js
const NODE_VER = 'v24.10.0'
const DSH_VERSION = '0.1.0-rc.7'
const PNPM_VERSION = '11.22.0'
```

**这是整个链路唯一的版本定义点**：`scripts/build-release.sh`（`default_ver()`）
与 CI `release.yml`（读取 step）都从这里提取，改这一处即处处跟随。
（历史上有 `BUNDLE_*` 环境变量可临时覆盖，现已移除：试打其他版本只能改这里或
用 git stash。）

### ③ 本地构建的版本校验兜底（自动跟随，无需改）
`scripts/build-release.sh` — `runtime_fresh()` 函数与提示文案使用 `DEFAULT_*` 变量
（见 ②），例如：

```bash
DEFAULT_NODE_VER="$(default_ver NODE_VER)"   # 从 bundle-runtime.mjs 提取
...
echo "生成/重建内置运行时（版本：node $DEFAULT_NODE_VER / dsh $DEFAULT_DSH_VERSION / pnpm $DEFAULT_PNPM_VERSION）"
```

> ⚠️ 注意：**`build-release.sh` 只是打包脚本，全程只读版本、从不修改**——
> 它不动 `bundle-runtime.mjs`，也不动本文件（RUNTIME-VERSIONING.md 只是说明文档，
> 表格是快照不是数据源）。改版本请走 `set-runtime-version.sh` 或手工修改 ②，
> 本页表格不会因打包而自动更新。

`runtime_fresh` 把已有 `runtime-manifest.json` 的版本与目标版本（env 或默认值）比对：
一致 → 复用现有 runtime；不一致 → **自动删除并重建**。这保证了改版本后
忘删旧 runtime 也不会打出旧版 dsh。CI 与本地在同一入口（②）取版本，天然一致，
不存在不同步问题。

---

## 2. 生成物（不手改，重建时自动更新）

运行 `scripts/bundle-runtime.mjs`（或打包脚本触发）后自动产生/更新，**不要手工编辑**：

```
src-tauri/resources/runtime/
├── nd/                          # Node 运行时（darwin/linux: bin/node；win32: node.exe）
│   └── …（解压根经过瘦身）
├── rt/
│   ├── package.json             # dependencies: @deepseek-ai/dsh + pnpm 精确版本
│   ├── package-lock.json        # npm lock：dsh 完整依赖树（七层）
│   └── node_modules/            # 安装产物（约 330M，gitignore）
└── runtime-manifest.json        # nodeVersion / dshVersion / pnpmVersion / 平台 / 校验和
```

**lock 文件必须提交**（`rt/package.json` + `rt/package-lock.json` 已允许入库，
`nd/` 与 `rt/node_modules/` 已 gitignore）：提交后本机/CI 重建时走 `npm ci`
（秒级、确定性），否则会退化为 `npm install` 全量决议（约 2 分钟且结果可漂移）。

---

## 3. 升级步骤

### 方式 A：一键脚本（推荐）

```bash
./scripts/set-runtime-version.sh dsh 0.2.0 --dry-run   # 先预览
./scripts/set-runtime-version.sh dsh 0.2.0             # 应用 + 重建 + 冒烟
git add src-tauri/resources/runtime/rt/package.json \
        src-tauri/resources/runtime/rt/package-lock.json   # 提交新 lock
```

之后推送 tag 发版即可（CI 读取到新版本后缓存 key 自动失效）。

### 方式 B：手工（示例：dsh 0.1.0-rc.7 → 0.2.0）

1. 确认版本存在：`npm view @deepseek-ai/dsh versions`；
2. 改 `scripts/bundle-runtime.mjs` 18 行默认值 → `'0.2.0'`
   （这是唯一版本定义点：CI 的 release.yml 与本地 build-release.sh 都自动读取，
   `RUNTIME-VERSIONING.md` 表格记得同步）；
3. 本地验证（会触发重建）：
   ```bash
   rm -rf src-tauri/resources/runtime        # 或直接打包，runtime_fresh 会自动重建
   ./scripts/build-release.sh two
   cat src-tauri/resources/runtime/runtime-manifest.json   # 确认三个字段
   src-tauri/resources/runtime/nd/bin/node \
     src-tauri/resources/runtime/rt/node_modules/@deepseek-ai/dsh/lib/bin.js --version
   ```
4. 提交新的 `rt/package.json` + `rt/package-lock.json`（dsh 版本变化必然引起 lock 更新）；
5. 推送 tag 发版：CI 读取新版本 → cache key 自动 miss → 各架构重建新版本运行时并重新缓存。

升级 Node 或 pnpm 同理，只是第 1 步换成检查 nodejs.org / npm 上对应版本
（node 需确认目标平台包存在，见 §5）。

---

## 4. 不要改（常见误解）

| 位置 | 为什么不能动 |
|---|---|
| `release.yml` 里 `node-version: 22`、`pnpm/action-setup@v4 version: 9` | 这是**构建客户端外壳的构建工具**（跑 tauri build / 前端），**不是**捆绑进应用的运行时版本 |
| 仓库根 `package.json`（`@tauri-apps/cli`） | 构建工具版本，与运行时无关 |
| `src-tauri/Cargo.toml` / `tauri.conf.json` version | 应用自身版本号（与 tag 对齐用），不属于运行时 |
| `ui/` 前端 | 前端不感知运行时版本 |

---

## 5. 约束与注意事项

- **Node 版本必须存在相应平台包**：脚本按目标平台拼 URL，缺了会下载失败——
  - macOS arm64 / x64：`https://nodejs.org/dist/<ver>/node-<ver>-darwin-{arm64,x64}.tar.gz`
  - Windows（交叉打包 `RUNTIME_TARGET=win32`）：`node-<ver>-win-x64.zip`
  - Linux（交叉打包 `RUNTIME_TARGET=linux`，可选 `TARGET_ARCH=arm64`）：
    `node-<ver>-linux-{x64,arm64}.tar.gz`
  - Linux 的官方路径是 ubuntu-24.04 runner 上不设 RUNTIME_TARGET 本机构建（deb 产物）
- **runtime 按平台生成，重用时校验平台**：`build-release.sh` 的 `runtime_fresh()` 除版本外
  还比对 `runtime-manifest.json` 的 `platform` 字段（node 二进制与 node-pty/sharp 等 prebuilds
  是平台相关的）；换平台构建（交叉或 Linux runner）会触发重建而非静默复用旧平台 runtime。
- **建议精确锁定 dsh 版本**：写成 `latest` 会导致依赖漂移（重建时装到最新版），
  且与已提交的 lock、manifest 不一致。
- **版本漂移检测是兜底不是主入口**：`runtime_fresh` 只在打包前校验一次；日常改版本
  以「改 ② 单一入口 + 提交新 lock」为准（或用 §0 一键脚本）。
- **CI 缓存注意事项**：缓存 step 刻意**不设 `restore-keys`**，避免恢复出旧版本
  runtime 被静默复用；版本变化靠 key 失效走完整重建。
- **pnpm 仅 dsh 插件管理需要**：`dsh plugin` 命令把参数转发给 PATH 中的 pnpm，
  内置版承诺离线可用，因此 pnpm 必须一并捆绑（详见打包脚本注释与
  `dsh-desktop-hub` 的 `src/core/harness.ts` 中 runtime PATH 说明）。
- **构建环境提示**：本地跑 `scripts/build-release.sh` 前确保 `cargo` 在 PATH
  （如 `export PATH="$HOME/.cargo/bin:$PATH"`），否则 `tauri build` 会因找不到
  `cargo metadata` 失败。