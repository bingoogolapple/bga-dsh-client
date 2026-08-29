# 重打 Tag（构建失败后如何修复并重新发版）

本文说明：**当某个 `v*` tag 触发的 GitHub Actions 构建失败时，为什么只改 `main` 分支没用、
以及重打 tag 的完整命令与注意事项**；并覆盖「部分平台失败但 Release 已被创建」这种
**残缺发布**的清理步骤。

> 背景案例：`v0.0.4` 连续两轮构建失败——先挂在「前端格式检查（Prettier）」
> （根因是 `scripts/set-app-version.sh` 抹掉了 `package.json` 里 version 行的缩进），
> 修好后又挂在 **Windows 平台的 clippy**（根因见 §6）。
> 更麻烦的是：由于 matrix 用了 `fail-fast: false`，第二轮里 macOS / Linux 照常跑完，
> **创建了一个缺少 Windows 产物的残缺 Release**。
> 所以修复后除了重打 tag，还得**先删掉那个 Release**（见 §3）。

---

## 0. 先看结论：该选哪个方案？

| 情况 | 方案 | 说明 |
|---|---|---|
| **从未产出 Release**（构建在上传前就失败） | **方案 A：重打同名 tag** | 没有用户拿到过产物，重写 tag 无副作用 |
| **已产出残缺 Release**（部分平台失败，但 Release 被创建了） | **先删 Release + 删 tag，再走方案 A** | 产物缺平台、不可用，应整包作废重发，见 §3 |
| **已完整发布**且可能有人在用 | **方案 B：升版本号发新版** | 已发布的版本号不可复用，详见 §5 |

判据：

```bash
gh release list --limit 5          # 列表里有没有这个版本？
gh release view v0.0.4             # 能查到 = 已产出 Release → 先看 §3 判残缺
```

---

## 1. 为什么改了代码 CI 还是不会重跑

CI 的触发条件是 **推送 tag**（`.github/workflows/release.yml` 的 `on.push.tags`），
而 tag 是**指向某个具体 commit 的不可变指针**：

```
v0.0.4  →  2f675a3 (chore(release): 发布 0.0.4 版本)   ← 构建是在这个 commit 上跑的
```

在 `main` 上提交修复会生成**新的 commit**，但 `v0.0.4` 仍然指着旧的那个：

```
v0.0.4  →  2f675a3  (坏)
main    →  2f675a3 → a1b2c3d  (修好了，但没有任何 tag 指向它)
```

没有 tag 指向新 commit，就不会触发新的 workflow；对旧 tag 点 "Re-run" 也只是
**用同一个旧 commit 重跑一遍，必然再失败**。

所以必须让 tag 重新指向修复后的 commit —— 这就是「重打 tag」。

---

## 2. 前置检查

```bash
# ① 确认 tag 当前指向哪个 commit、本地是否已推送
git rev-list -n1 v0.0.4
git log -1 --format='%h %s' v0.0.4
git rev-list --count origin/main..HEAD    # 非 0 = 本地有未推送的提交

# ② 确认是否已产生 Release、是否残缺（见 §3）
gh release view v0.0.4

# ③ 确认修复已就绪（本地跑一遍 CI 的门禁）
pnpm run lint && pnpm run format:check
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --locked
```

> ⚠️ **重要约束**：CI 有一步「校验 tag 与版本号一致」，它会比对 tag 名与
> `src-tauri/tauri.conf.json` 中的 `version`。
> - 走方案 A（重打 `v0.0.4`）→ `tauri.conf.json` 的版本**必须仍是 `0.0.4`**；
> - 走方案 B（发 `v0.0.5`）→ 必须先用 `set-app-version.sh` 把三处版本改成 `0.0.5`。
>
> 两边不一致会直接在这一步失败（提示去跑 `./scripts/set-app-version.sh`）。

> ⚠️ **跨平台提示**：`clippy` 这类检查在不同平台的结果可能不同（代码里存在
> `#[cfg(windows)]` / `#[cfg(not(windows))]` 分支）。**只在 macOS 跑通不代表
> Windows 能过**。若改动涉及平台分支，本地无法覆盖时，优先保证 host 门禁通过，
> 再看 CI 上其它平台的报错（见 §6 的真实案例）。

---

## 3. 若已产生 Release：先删 Release，再删 tag

### 3.1 为什么会有「残缺 Release」

`release.yml` 的 matrix 里设了 `fail-fast: false` —— 某个平台失败时，其余平台
**不会**被取消，会继续跑完。而「创建 Release 并上传」这一步的条件只是
`if: github.ref_type == 'tag'`，**并不检查同 run 内其它 job 是否成功**。

于是一旦只有部分平台失败（例如只有 Windows 挂在 clippy），先跑完的 macOS / Linux
会照常创建 Release 并上传自己的产物，产出一个**缺平台的残次 Release**。

### 3.2 判断是否残缺：核对产物清单

完整的一轮（4 平台 × plain / bundled，macOS 另带 zip）应产出 **12 个附件**：

| 平台 | plain | bundled |
|---|---|---|
| macOS aarch64 | `*_aarch64.dmg`、`DeepSeek-Harness-aarch64-*.zip` | `*_aarch64-bundled.dmg`、`DeepSeek-Harness-bundled-aarch64-*.zip` |
| macOS x86_64 | `*_x64.dmg`、`DeepSeek-Harness-x86_64-*.zip` | `*_x64-bundled.dmg`、`DeepSeek-Harness-bundled-x86_64-*.zip` |
| Linux | `*_amd64.deb` | `*_amd64-bundled.deb` |
| Windows | `*_x64-setup.exe` | `*-bundled-setup.exe` |

（以上对应 `release.yml` 中 `softprops/action-gh-release` 的 `files:` glob：
`dist/release/{plain,bundled}/*.{dmg,deb,exe}` + macOS 的两个 zip。）

```bash
gh release view v0.0.4     # 逐条比对 asset 列表
```

v0.0.4 当时的实际情况 —— **只有 6 个附件，Windows 与 macOS Intel 全缺**：

```
asset:  DeepSeek-Harness-aarch64-v0.0.4.zip          ← macOS aarch64（完整）
asset:  DeepSeek-Harness-bundled-aarch64-v0.0.4.zip
asset:  DeepSeekHarness_0.0.4_aarch64-bundled.dmg
asset:  DeepSeekHarness_0.0.4_aarch64.dmg
asset:  DeepSeekHarness_0.0.4_amd64-bundled.deb      ← Linux（完整）
asset:  DeepSeekHarness_0.0.4_amd64.deb
        （缺 macOS x86_64 的 4 个 + Windows 的 2 个）
```

### 3.3 删除 Release 与 tag

> ⚠️ **GitHub 上 Release 与 tag 是两个独立对象**：
> - `gh release delete` 只删 Release 及其附件，**tag 仍在**；
> - `git push --delete origin <tag>` 只删 tag，Release 不会连带消失（会变成指向空 tag 的孤儿）。
>
> 因此**两者都要删**，顺序无所谓。

```bash
# ① 删 Release 及其全部附件（--yes 跳过交互确认）
gh release delete v0.0.4 --yes

# ② 删远端 tag（两种写法等价）
git push --delete origin v0.0.4
# 或：git push origin :refs/tags/v0.0.4

# ③ 确认都清干净了
gh release list --limit 5
git ls-remote --tags origin | grep -o 'refs/tags/v[0-9.]*$'
```

> 💡 若 `git push --delete` 报 `Error in the HTTP2 framing layer` 之类的错误，
> 那是**瞬时网络故障，直接重试即可**。它与 Release 是否存在无关——GitHub 允许
> 删掉带 Release 的 tag。

### 3.4 残缺 Release 之后用哪个版本号？

通常**可以直接重打同名版本**：残缺 Release 本身就是坏的，且缺平台的用户根本没拿到产物。

但如果有大量用户已经装上了某个平台的可用产物（例如 macOS 用户装了能跑的 dmg），
再权衡是否升版本号——原因见 §5 的自动更新机制说明。

---

## 4. 方案 A：重打同名 tag

```bash
# ① 提交修复
git add package.json src-tauri/tauri.conf.json scripts/set-app-version.sh
git commit -m "fix(ci): 修复 set-app-version.sh 抹掉 version 行缩进导致 Prettier 检查失败"

# ② 删除本地 tag
git tag -d v0.0.4

# ③ 删除远端 tag（若 §3 已删过可跳过）
git push --delete origin v0.0.4

# ④ 在当前（已修复的）commit 上重新打 tag
git tag v0.0.4

# ⑤ 推送前先校验：tag 指向的 commit 里，版本号必须与 tag 名一致
git log -1 --format='%h %s' v0.0.4
git show v0.0.4:src-tauri/tauri.conf.json | grep -m1 '"version"'

# ⑥ 推送 main 与新 tag
git push origin main --tags
```

推送后 CI 会自动触发，用 `gh` 观察：

```bash
gh run list --limit 3                      # 找到新 run 的 id
gh run watch <run-id>                      # 实时跟踪
gh run view --job <job-id> --log-failed    # 失败时看具体报错
```

若 GitHub 因 run 未完成而拒绝给日志，可直接走 API 下载：

```bash
curl -sL -H "Authorization: Bearer $(gh auth token)" \
  "https://api.github.com/repos/<owner>/<repo>/actions/jobs/<job-id>/logs" -o /tmp/job.log
```

**其他协作者**（本地已有旧 tag）需要同步刷新，否则本地仍是旧指向：

```bash
git fetch --prune --tags
# 若仍残留同名旧 tag：git tag -d v0.0.4 && git fetch --tags
```

---

## 5. 方案 B：升版本号发新版（适用于已完整发布过的版本）

```bash
# ① 一键改三处版本 + 同步 Cargo.lock（package.json / tauri.conf.json / Cargo.toml）
./scripts/set-app-version.sh 0.0.5

# ② 提交并打新 tag
git add -A
git commit -m "release v0.0.5"
git tag v0.0.5

# ③ 推送
git push origin main --tags
```

> ⚠️ **已发布过的版本号不要复用**。本应用内置了自动更新检查
> （`src-tauri/src/update.rs`，用 `version_gt(current, latest)` 比较）：
> 版本号相同就意味着「没有更新」。如果 `v0.0.4` 已经有人装上，
> 而你重打 `v0.0.4` 换了二进制，那些用户**永远收不到新版本**。
> 这种情况下必须发新的版本号。

---

## 6. 本次 v0.0.4 事故的两个根因（均已修复，供参考）

### 6.1 `set-app-version.sh` 抹掉 JSON 缩进

原先这样改版本：

```bash
# 错误写法：只吞不吐
sed -i '' "s/^[[:space:]]*\"version\": \"$old\"/\"version\": \"$new\"/" package.json
```

匹配段 `^[[:space:]]*` 吃掉了行首缩进，替换段却没有还回去 —— **每次发版都会把
version 行的缩进抹平**：

```json
  "name": "bga-dsh-client",
"version": "0.0.4",        ← 缩进丢失 → Prettier 检查失败
  "private": true,
```

同一处 bug 也抹平了 `src-tauri/tauri.conf.json` 的 version 行，只是它不在
Prettier 的检查范围（`format:check` 只匹配仓库根目录的 `*.json`）内，所以没一起报错。

现已改为**捕获缩进并原样写回**：

```bash
sed_inplace -E "s/^([[:space:]]*)\"version\": \"$esc_old\"/\1\"version\": \"$new\"/" package.json
```

顺带修了两处：版本号里的 `.` `+` 会做正则转义；新增 `sed_inplace()` 抹平
BSD(macOS) / GNU(Linux) 的 `sed -i` 差异，脚本现在跨平台可用。

### 6.2 拆分模块时丢掉 `_extra_path` 的下划线前缀（仅 Windows 报错）

`version.rs` 的 `run_capture` 有个参数**只在 `#[cfg(not(windows))]` 分支里被消费**，
原作者因此给它加了下划线前缀：

```rust
fn run_capture(
    prog: &Path,
    args: &[&OsStr],
    timeout: Duration,
    _extra_path: Option<&str>,   // ← 下划线前缀：Windows 上不用它
) -> Option<String>
```

把 `main.rs` 拆成 `version.rs` 时，这个前缀被当成多余字符"清理"掉了，于是 Windows
上该参数未被使用，被 clippy 的 `unused_variables` 拦下（CI 是 `-D warnings`，
警告即失败）。**macOS / Linux 完全不受影响**，所以只有 Windows job 挂了。

两个教训：

1. **下划线前缀、`#[allow(...)]`、`#[cfg(...)]` 往往是有意为之**，重构时不要顺手"清理"；
2. **只在一个平台跑通不代表全平台通过**——涉及 `cfg` 分支的改动要特别留意。

---

## 7. 避免再犯：发版前的自检

打 tag 之前，建议把 CI 的门禁在本地全跑一遍（这也是 CI 的实际执行顺序）：

```bash
pnpm install --frozen-lockfile
pnpm run lint                     # ESLint
pnpm run format:check             # Prettier ← v0.0.4 第一轮失败点
cd src-tauri
cargo fmt --check
cargo clippy --all-targets -- -D warnings   # ← v0.0.4 第二轮失败点（仅 Windows）
cargo test --locked
cd ..
./scripts/build-release.sh two    # 可选：本地完整打一次包
```

版本号相关全部走脚本，不要手工编辑，以免再次引入格式或不一致问题：

- 应用版本：`./scripts/set-app-version.sh <x.y.z>`
- 内置运行时版本：`./scripts/set-runtime-version.sh`（详见
  [RUNTIME-VERSIONING.md](RUNTIME-VERSIONING.md)）

### 建议：给「创建 Release」加防残缺保护

`fail-fast: false` 的初衷是"一个平台失败不拖累其它平台的构建"，但副作用是
**会把残缺产物发布出去**。建议后续把发布步骤从 matrix job 里拆出来，单独用一个
`needs: build-and-release` 的 job 执行，这样只有全部平台成功才会创建 Release：

```yaml
publish:
  needs: build-and-release          # 全部 matrix job 成功才执行
  if: github.ref_type == 'tag'
  steps:
    - uses: softprops/action-gh-release@v3
      # 用 actions/download-artifact 汇总各平台产物后上传
```

在此之前，发版后请**按 §3.2 核对附件清单**，缺了就走一遍 §3 的清理。
