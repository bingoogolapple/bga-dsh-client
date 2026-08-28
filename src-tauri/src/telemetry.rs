//! 遥测模块：Sentry 错误监控 + 自定义事件上报。
//!
//! # 隐私原则（重要）
//!
//! - **默认关闭（opt-in）**：`ENABLED` 初始为 false，只有用户在设置页显式开启
//!   （经 `set_telemetry_enabled` 命令）后才上报任何数据。旧配置文件无
//!   `telemetry_enabled` 字段时同样是关闭。
//! - **构建可裁剪**：Sentry DSN 由编译期环境变量 `SENTRY_DSN` 注入
//!   （`option_env!`）。CI / 个人 fork 不注入该变量 → 模块整体空转，
//!   既不联网也不链接上报路径。DSN 不再硬编码在源码里。
//! - 匿名机器 ID（hash(hostname+user)，不可逆，不含用户个人信息）。
//! - 仅上报应用行为事件，不收集文件内容、聊天记录、API Key。
//! - 所有网络请求在后台线程执行，不阻塞主线程。
//!
//! # 三重开关的关系
//!
//! 上报发生 ⟺ `init(enabled=true)` 被调用过 **且** 构建时注入了 `SENTRY_DSN`。
//! 运行期再开关只影响 `ENABLED`，不影响已初始化的 Sentry client（它保持挂载
//! 以便捕获 panic，但事件函数在此直接返回）。

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

use serde_json::json;

/// Sentry DSN：**编译期注入**，不再硬编码。
///
/// 未设置 `SENTRY_DSN` 环境变量时编译 → 本模块所有上报变成空操作。
/// 这样 CI 构建、他人 fork 自行构建都不会把数据发给作者的 Sentry 项目。
const SENTRY_DSN: Option<&str> = option_env!("SENTRY_DSN");

/// 归一化后的 DSN：空字符串 / 纯空白一律视为「未注入」。
///
/// 必要性：CI 里 `env: SENTRY_DSN: ${{ ... || '' }}` 在手动触发时会把该变量
/// **设为空串而不是取消设置**，此时 `option_env!` 返回 `Some("")` 而非 `None`。
/// 若不归一化，`Some("")` 会被当成"注入了 DSN"，进而在用户开启开关时走到
/// `Dsn::from_str("")` 失败分支并打印一条误导性的「格式非法」告警。
/// 归一化后，「空串」与「未设置」行为完全一致（都为空转）。
fn dsn() -> Option<&'static str> {
    match SENTRY_DSN {
        Some(s) if !s.trim().is_empty() => Some(s.trim()),
        _ => None,
    }
}

/// 运行期开关（对应用户设置里的「匿名使用统计」）。默认 false。
static ENABLED: AtomicBool = AtomicBool::new(false);

/// 全局初始化标记，确保只初始化一次。
static INIT: Once = Once::new();

/// 当前是否允许上报（供前端查询与内部短路判断）。
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 运行期切换开关（用户在设置页改动时调用）。
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// 初始化 Sentry（含 panic hook）。
///
/// 在 Tauri setup() 中调用，重复调用安全。
///
/// - `enabled=false`（默认）：只登记开关状态，不初始化 Sentry、不联网；
///   用户后续在设置页开启时，本函数已被 `Once` 消耗，故开启走
///   `set_enabled` + 惰性初始化（见 `ensure_client`）。
/// - 未注入 `SENTRY_DSN` 时：即使 enabled 也保持空转。
pub fn init(app_version: &str, enabled: bool) {
    set_enabled(enabled);
    if !enabled || dsn().is_none() {
        return;
    }
    INIT.call_once(|| {
        let Ok(sentry_dsn) = sentry::types::Dsn::from_str(dsn().unwrap_or_default()) else {
            eprintln!("DeepSeekHarness: SENTRY_DSN 格式非法，遥测已禁用");
            set_enabled(false);
            return;
        };
        let sentry_guard = sentry::init((
            sentry_dsn,
            sentry::ClientOptions {
                release: sentry::release_name!(),
                environment: Some(
                    if cfg!(debug_assertions) {
                        "development"
                    } else {
                        "production"
                    }
                    .into(),
                ),
                traces_sample_rate: 0.0, // 不采集性能追踪（免费版额度有限）
                ..Default::default()
            },
        ));

        // 设置全局用户信息（仅匿名 ID，不含个人信息）
        sentry::configure_scope(|scope| {
            scope.set_user(Some(sentry::User {
                id: Some(machine_id()),
                ..Default::default()
            }));
            scope.set_tag("app_version", app_version);
            scope.set_tag("os", std::env::consts::OS);
            scope.set_tag("arch", std::env::consts::ARCH);
        });

        // 注意：sentry_guard 必须在进程退出前保持 alive，
        // 这里通过 leak 实现全局生命周期（退出时自动 flush）。
        std::mem::forget(sentry_guard);
    });
}

/// 运行时才被开启时补做初始化（用户首次打开开关）。
///
/// `init` 在启动时以 enabled=false 调用过，此时 `Once` 已被消耗而无法重试；
/// 但那时我们根本没有 `call_once`，所以这里可以安全地在首次开启时初始化。
fn ensure_client() {
    if dsn().is_none() {
        return;
    }
    // 已初始化过则 call_once 无副作用；未初始化则执行真正的 init。
    INIT.call_once(|| {
        let Ok(sentry_dsn) = sentry::types::Dsn::from_str(dsn().unwrap_or_default()) else {
            return;
        };
        let sentry_guard = sentry::init((
            sentry_dsn,
            sentry::ClientOptions {
                release: sentry::release_name!(),
                environment: Some("production".into()),
                traces_sample_rate: 0.0,
                ..Default::default()
            },
        ));
        sentry::configure_scope(|scope| {
            scope.set_user(Some(sentry::User {
                id: Some(machine_id()),
                ..Default::default()
            }));
            scope.set_tag("os", std::env::consts::OS);
            scope.set_tag("arch", std::env::consts::ARCH);
        });
        std::mem::forget(sentry_guard);
    });
}

/// 启动时环境信息（由 main.rs setup 阶段填充）。
pub struct EnvInfo {
    pub app_version: String,
    pub has_bundled_runtime: bool,
    pub node_version: Option<String>,
    pub pnpm_version: Option<String>,
    pub dsh_version: Option<String>,
    pub service_was_up: bool,
}

/// 上报增强版 app_started 事件（含运行时环境、版本、服务状态等）。
pub fn report_app_started(info: &EnvInfo) {
    capture_event(
        "app_started",
        Some(json!({
            "version": &info.app_version,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "has_bundled_runtime": info.has_bundled_runtime,
            "node_version": info.node_version.as_deref().unwrap_or("unknown"),
            "pnpm_version": info.pnpm_version.as_deref().unwrap_or("unknown"),
            "dsh_version": info.dsh_version.as_deref().unwrap_or("unknown"),
            "service_was_up": info.service_was_up,
        })),
    );
}

/// 上报自定义事件到 Sentry（非阻塞，失败静默忽略）。
///
/// 开关关闭 / 构建未注入 DSN 时**立即返回**，不 spawn 线程、不发网络请求。
pub fn capture_event(event_name: &str, extra: Option<serde_json::Value>) {
    if !enabled() || dsn().is_none() {
        return;
    }
    ensure_client();
    let event_name = event_name.to_string();
    std::thread::spawn(move || {
        sentry::with_scope(
            |scope| {
                if let Some(data) = extra {
                    for (k, v) in data.as_object().unwrap_or(&Default::default()) {
                        scope.set_extra(k, sentry::protocol::Value::String(v.to_string()));
                    }
                }
            },
            || {
                sentry::capture_message(&event_name, sentry::Level::Info);
            },
        );
    });
}

/// 上报错误到 Sentry。开关关闭时不上报（错误仅落本地日志）。
pub fn capture_error(error: &str, error_type: Option<&str>) {
    if !enabled() || dsn().is_none() {
        return;
    }
    ensure_client();
    let error = error.to_string();
    let error_type = error_type.map(|s| s.to_string());
    std::thread::spawn(move || {
        sentry::with_scope(
            |scope| {
                if let Some(t) = error_type {
                    scope.set_tag("error_type", &t);
                }
            },
            || {
                sentry::capture_message(&error, sentry::Level::Error);
            },
        );
    });
}

/// 基于 hostname + 用户名生成确定性匿名机器 ID（默认哈希器使用固定密钥，跨启动稳定）。
/// 同一台机器每次启动生成相同的 UUID，不可逆推出原始信息。
fn machine_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();

    // hostname（跨平台可用）
    if let Ok(name) = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")) {
        name.hash(&mut hasher);
    }

    // 用户名作为辅助（不可逆，仅增加区分度）
    if let Ok(user) = std::env::var("USER").or_else(|_| std::env::var("USERNAME")) {
        user.hash(&mut hasher);
    }

    let hash = hasher.finish();

    // 格式化为 UUID v5 风格（仅格式一致，非标准 UUID v5）
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        hash >> 32,
        (hash >> 16) & 0xFFFF,
        hash & 0xFFFF,
        0x5000 | (hash >> 48) & 0x0FFF,
        hash & 0xFFFFFFFFFFFF
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 串行化所有触碰全局 ENABLED 的用例。
    ///
    /// `ENABLED` 是**进程级** AtomicBool，而 `cargo test` 默认多线程并行跑用例。
    /// 若两个用例同时读写它，会互相干扰（表现为随机失败的 flaky test：
    /// 单独跑 telemetry 通过、跑全量却失败，或反之）。
    /// 凡是要断言全局开关状态的用例，都必须先取这把锁。
    fn telemetry_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 关闭状态下 capture_event / capture_error 都是空操作（不 spawn、不联网）。
    #[test]
    fn disabled_by_default() {
        let _guard = telemetry_lock();
        let before = enabled();
        set_enabled(false);
        assert!(!enabled());
        // 空操作路径：不应 panic，也不应意外打开开关
        capture_event("unit_test_event", None);
        capture_error("unit_test_error", Some("test"));
        assert!(!enabled(), "上报空操作不应意外打开开关");
        set_enabled(before);
    }

    /// 开关可运行期切换。
    #[test]
    fn toggle_works() {
        let _guard = telemetry_lock();
        let before = enabled();
        set_enabled(true);
        assert!(enabled());
        set_enabled(false);
        assert!(!enabled());
        set_enabled(before);
    }

    /// 开启后上报走"空转"路径（未注入 DSN 的构建里不会真的联网），
    /// 且**不会**把开关自动关掉。
    #[test]
    fn enabled_path_does_not_mutate_flag() {
        let _guard = telemetry_lock();
        let before = enabled();
        set_enabled(true);
        capture_event("enabled_path_test", Some(json!({"k": 1})));
        capture_error("enabled_path_test_err", None);
        assert!(enabled(), "上报不应把已开启的开关关掉");
        set_enabled(before);
    }

    /// 机器 ID 是稳定、匿名、UUID 形态的（同一进程内多次调用结果一致）。
    #[test]
    fn machine_id_is_stable_and_shaped() {
        let a = machine_id();
        let b = machine_id();
        assert_eq!(a, b, "同一环境内应稳定");
        assert_eq!(a.len(), 36, "UUID 形态长度应为 36，实际: {a}");
        assert_eq!(a.chars().filter(|c| *c == '-').count(), 4);
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "只含十六进制字符与连字符: {a}"
        );
    }

    /// 未注入 DSN 的构建里，即便 enabled 也不应尝试初始化/上报（不 panic）。
    ///
    /// 注意：CI 与本地默认构建都不注入 SENTRY_DSN，故 dsn().is_none() 成立，
    /// 这条用例在两种构建下都验证"安全空转"。
    #[test]
    fn no_dsn_build_is_noop() {
        let _guard = telemetry_lock();
        let before = enabled();
        set_enabled(true);
        // 不注入 DSN 时不应 panic，也不应改变开关
        capture_event("should_be_dropped", Some(json!({"k": "v"})));
        capture_error("should_be_dropped", None);
        report_app_started(&EnvInfo {
            app_version: "0.0.0-test".into(),
            has_bundled_runtime: false,
            node_version: None,
            pnpm_version: None,
            dsh_version: None,
            service_was_up: false,
        });
        set_enabled(before);
    }
}
