## ⬇️ Mac 用户选哪个？/ Which one to download? (macOS only)

**中文：** 仅 Mac 需要选择架构（Linux / Windows 用户跳过此步）。打开「左上角  → 关于本机」，看「芯片」一栏即可确定：

| 芯片 | 下载 |
| --- | --- |
| **Apple（M 系列）**：M1 / M2 / M3 / M4…（2020 年后机型） | `*_aarch64.dmg`（或 zip 版 `DeepSeek-Harness-aarch64-*.zip`） |
| **Intel**：Core i3 / i5 / i7…（2020 年前机型） | `*_x64.dmg`（或 zip 版 `DeepSeek-Harness-x86_64-*.zip`） |

**English:** (macOS only — Linux / Windows users can skip this section.) Click the Apple logo  → **About This Mac**, check the "Chip" line:

| Chip | Download |
| --- | --- |
| **Apple (M-series)**: M1 / M2 / M3 / M4… (2020 and later) | `*_aarch64.dmg` (or zip: `DeepSeek-Harness-aarch64-*.zip`) |
| **Intel**: Core i3 / i5 / i7… (before 2020) | `*_x64.dmg` (or zip: `DeepSeek-Harness-x86_64-*.zip`) |

## 安装 / Installation

**Mac (中文):** 打开下载的 dmg，把 `DeepSeekHarness.app` 拖入「应用程序」目录。首次打开若提示「无法打开，因为无法验证开发者」，这是 macOS Gatekeeper 对未做开发者签名应用的保护，处理方式二选一：右键 app → 打开；或「系统设置 → 隐私与安全性 → 仍要打开」，放行一次后即可正常使用（本应用未做开发者签名，属正常现象）。若提示「已损坏，无法打开」（macOS 对未签名应用的常见误报，文件通常没坏）：先确认下载的架构与你的 Mac 匹配（Intel Mac 装 aarch64 版会报错）；仍不行则在「终端」执行 `xattr -dr com.apple.quarantine /Applications/DeepSeekHarness.app` 后重新打开。

**Mac (English):** Open the downloaded dmg and drag `DeepSeekHarness.app` into the **Applications** folder. If macOS says "cannot be opened because the developer cannot be verified" the first time, that's Gatekeeper protecting the unsigned app: right-click the app → Open, or go to **System Settings → Privacy & Security → Open Anyway**; after allowing it once, it works normally (this app is not developer-signed, which is normal). If macOS says the app is "damaged and can't be opened" (a common false positive for unsigned apps; the file is usually fine): make sure you downloaded the build matching your Mac's architecture (an Intel Mac installing the `aarch64` build will error); if it still fails, run `xattr -dr com.apple.quarantine /Applications/DeepSeekHarness.app` in Terminal and open it again.

**Linux (中文):** 下载 `*.deb`（内置版为 `*-bundled.deb`）后 `sudo dpkg -i <文件>.deb`（缺依赖时先 `sudo apt -f install`）。应用不提供 AppImage。

**Linux (English):** Download the `*.deb` (`*-bundled.deb` for the embedded-runtime build) and run `sudo dpkg -i <file>.deb` (run `sudo apt -f install` first if dependencies are missing). No AppImage is provided.

**Windows (中文):** 下载 `*-setup.exe`（内置版为 `*-setup-bundled.exe`）安装即可。若提示「Windows 已保护你的电脑」，点「更多信息 → 仍要运行」（未做代码签名，属正常现象）。

**Windows (English):** Download and run `*-setup.exe` (`*-setup-bundled.exe` for the embedded-runtime build). If SmartScreen shows "Windows protected your PC", click **More info → Run anyway** (the app is not code-signed, which is normal).

## 使用 / Usage

**中文：** 启动后自动探测并拉起 DSH Web 服务（默认 `127.0.0.1:3080`），常驻系统托盘；设置页可配置退出时是否停止服务、检测应用更新，以及开启局域网扫码配对访问（默认 `0.0.0.0:18080`）。

**English:** On launch the app auto-detects and starts the DSH Web service (default `127.0.0.1:3080`) and lives in the system tray; the settings page lets you choose whether to stop the service on quit, check for app updates, and enable LAN QR-code pairing access (default `0.0.0.0:18080`).
