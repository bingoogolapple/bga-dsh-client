# Sentry 遥测说明

本应用使用 [Sentry](https://sentry.io) 进行错误监控和使用统计，帮助开发者了解应用的运行状况和使用情况。

## 设计原则

- **匿名化**：所有数据基于硬件生成的匿名机器 ID，不可逆推出用户身份
- **最小化**：仅收集应用运行必要的技术信息
- **透明化**：本文件完整列出所有收集的数据项
- **本地优先**：不收集文件内容、聊天记录、API Key 等私有数据

## DSN 说明

本应用的 Sentry DSN（客户端密钥）硬编码在源码中，这是 Sentry 官方推荐的做法。DSN 是只写端点，只能向 Sentry 发送数据，无法读取项目信息或配置。

## Dashboard 链接

- [Issues（错误列表）](https://bingoogolapple.sentry.io/issues/?project=4511930940456960) — 崩溃/错误详情、堆栈、影响用户数
- [Releases（版本使用量）](https://bingoogolapple.sentry.io/explore/releases/?project=4511930940456960) — 各版本使用人数、崩溃率趋势

## 收集的数据

### 1. 应用启动事件 (`app_started`)

每次应用启动时上报一次。

| 字段 | 示例值 | 说明 |
|------|--------|------|
| `version` | `0.0.2` | 客户端版本号 |
| `os` | `macos` / `windows` / `linux` | 操作系统 |
| `arch` | `aarch64` / `x86_64` | CPU 架构 |
| `has_bundled_runtime` | `true` / `false` | 是否使用内置 Node.js 运行时 |
| `node_version` | `v20.11.0` | 系统 Node.js 版本 |
| `pnpm_version` | `9.1.0` | 系统 pnpm 版本 |
| `dsh_version` | `0.0.1` | dsh 版本 |
| `service_was_up` | `true` / `false` | 启动时服务是否已在运行 |

### 2. 服务启动事件 (`service_started`)

DSH 服务启动成功时上报。

| 字段 | 示例值 | 说明 |
|------|--------|------|
| `method` | `内置 Node.js（应用自带 dsh，离线可用）` | 使用的拉起方式 |
| `startup_ms` | `12500` | 服务启动耗时（毫秒） |

### 3. 服务启动失败 (`service_start_failed`)

DSH 服务启动失败时上报（作为错误事件）。

| 字段 | 示例值 | 说明 |
|------|--------|------|
| `error_type` | `service_start_failed` | 错误类型 |

### 4. 服务停止事件 (`service_stopped`)

用户主动停止服务时上报，无额外字段。

### 5. 运行时检测结果 (`runtime_detection_result`)

应用启动时自动探测服务状态后上报。

| 字段 | 值 | 含义 |
|------|-----|------|
| `detection` | `orphan_takeover` | 接管了上次放生的服务 |
| `detection` | `external_reuse` | 复用了外部启动的服务 |
| `detection` | `fresh_start` | 全新启动（最常见） |

### 6. 设置保存事件 (`settings_saved`)

用户保存设置时上报。

| 字段 | 示例值 | 说明 |
|------|--------|------|
| `launch_method` | `npx` / `dsh` / `pnpm` / `builtin` | 选择的拉起方式 |
| `stop_service_on_quit` | `true` / `false` | 退出时是否停止服务 |

### 7. 设置页打开事件 (`settings_panel_opened`)

用户打开设置窗口时上报，无额外字段。

### 8. 局域网配对事件 (`pairing_started` / `pairing_stopped`)

用户启停局域网代理服务时上报，无额外字段。

### 9. 崩溃报告（自动）

应用发生 panic 时，Sentry 自动捕获并上报：

| 字段 | 说明 |
|------|------|
| 堆栈信息 | 错误发生位置（文件名、行号、函数名） |
| OS / 架构 | 运行环境 |
| 应用版本 | 发生崩溃时的版本 |
| 匿名机器 ID | 用于去重统计影响用户数 |

> 崩溃报告不包含任何文件内容、用户输入或网络请求数据。

## 不收集的数据

以下数据**不会**被收集：

- ❌ 文件路径（如 `launch_dir` 的具体值）
- ❌ 局域网 IP 地址
- ❌ 配对码
- ❌ 日志内容
- ❌ 聊天记录或 API 内容
- ❌ API Key 或密钥
- ❌ 用户名或邮箱（machine_id 是不可逆的哈希值）

## 如何禁用遥测

如需完全禁用遥测，可在编译时移除 `telemetry` 模块的调用，或设置环境变量 `SENTRY_DSN` 为空。

## 相关文件

- `src-tauri/src/telemetry.rs` — 遥测模块实现
- `src-tauri/Cargo.toml` — Sentry 依赖配置
