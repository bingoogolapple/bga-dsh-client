# 重打 Tag（已打 tag 的构建失败时如何修复后重新发版）

本文说明：**当某个 `v*` tag 触发的 GitHub Actions 构建失败时，为什么只改 `main` 分支没用、
以及重打 tag 的完整命令与注意事项。**

> 背景案例：`v0.0.4` 的构建挂在「前端格式检查（Prettier）」这一步。
> 根因是 `scripts/set-app-version.sh` 抹掉了 `package.json` 里 version 行的缩进
> （该 bug 已修复，见 §5）。代码修好后，仍**必须重打 tag** 才能让 CI 重新跑。

---

## 0. 先看结论：该选哪个方案？

| 情况 | 方案 | 说明 |
|---|---|---|
| 该版本**从未发布成功**（构建失败、没产出 Release） | **方案 A：重打同名 tag** | 没有用户拿到过产物，重写 tag 无副作用 |
| 该版本**已发布**（GitHub Release 存在、可能有人下载过） | **方案 B：升版本号发新版** | 已发布的版本号不可复用，详见 §4 |

判据（执行后按结果选）：

```bash
gh release list --limit 5          # 列表里有没有这个版本？
gh release view v0.0.4             # 能查到 = 已发布 → 走方案 B
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

# ② 确认修复已就绪（本地跑一遍 CI 的门禁）
pnpm run lint && pnpm run format:check
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --locked
```

> ⚠️ **重要约束**：CI 有一步「校验 tag 与版本号一致」，它会比对 tag 名与
> `src-tauri/tauri.conf.json` 中的 `version`。
> - 走方案 A（重打 `v0.0.4`）→ `tauri.conf.json` 的版本**必须仍是 `0.0.4`**；
> - 走方案 B（发 `v0.0.5`）→ 必须先用 `set-app-version.sh` 把三处版本改成 `0.0.5`。
>
> 两边不一致会直接在这一步失败（提示去跑 `./scripts/set-app-version.sh`）。

---

## 3. 方案 A：重打同名 tag（适用于从未发布成功的版本）

```bash
# ① 提交修复
git add package.json src-tauri/tauri.conf.json scripts/set-app-version.sh
git commit -m "fix(ci): 修复 set-app-version.sh 抹掉 version 行缩进导致 Prettier 检查失败"

# ② 删除本地 tag
git tag -d v0.0.4

# ③ 删除远端 tag（两种写法等价）
git push origin :refs/tags/v0.0.4
# 或：git push --delete origin v0.0.4

# ④ 在当前（已修复的）commit 上重新打 tag
git tag v0.0.4

# ⑤ 推送 main 与新 tag
git push origin main --tags
```

推送后 CI 会自动触发，用 `gh` 观察：

```bash
gh run list --limit 3                 # 找到新 run 的 id
gh run watch <run-id>                 # 实时跟踪
gh run view --job <job-id> --log-failed   # 失败时看具体报错
```

**其他协作者**（本地已有旧 tag）需要同步刷新，否则本地仍是旧指向：

```bash
git fetch --prune --tags
# 若仍残留同名旧 tag：git tag -d v0.0.4 && git fetch --tags
```

---

## 4. 方案 B：升版本号发新版（适用于已发布过的版本）

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

## 5. 本次 v0.0.4 事故的根因（已修复，供参考）

`scripts/set-app-version.sh` 原先这样改版本：

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

---

## 6. 避免再犯：发版前的自检

打 tag 之前，建议把 CI 的门禁在本地全跑一遍（这也是 CI 的实际执行顺序）：

```bash
pnpm install --frozen-lockfile
pnpm run lint                     # ESLint
pnpm run format:check             # Prettier ← 本次失败点
cd src-tauri
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cd ..
./scripts/build-release.sh two    # 可选：本地完整打一次包
```

版本号相关全部走脚本，不要手工编辑，以免再次引入格式或不一致问题：

- 应用版本：`./scripts/set-app-version.sh <x.y.z>`
- 内置运行时版本：`./scripts/set-runtime-version.sh`（详见
  [RUNTIME-VERSIONING.md](RUNTIME-VERSIONING.md)）
