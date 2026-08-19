//! 遥测模块：Sentry 错误监控 + 自定义事件上报。
//!
//! 设计原则：
//! - 匿名机器 ID（不可逆，不含用户个人信息）
//! - 仅上报应用行为事件，不收集文件内容、聊天记录、API Key
//! - 所有网络请求在后台线程执行，不阻塞主线程

use std::sync::Once;

use serde_json::json;

/// Sentry DSN（硬编码，公开值无需保密）
const SENTRY_DSN: &str =
    "https://5d43070fac4fee36dd3eb4e605d66525@o81414.ingest.us.sentry.io/4511930940456960";

/// 全局初始化标记，确保只初始化一次。
static INIT: Once = Once::new();

/// 初始化 Sentry（含 panic hook）。
/// 在 Tauri setup() 中调用，重复调用安全。
///
/// `env_info` 提供启动时探测到的环境信息（版本、运行时类型等），
/// 与 `init` 分离是为了在 setup 阶段探测完成后才上报 `app_started`。
pub fn init(app_version: &str) {
    INIT.call_once(|| {
        let sentry_guard = sentry::init((
            SENTRY_DSN,
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
pub fn capture_event(event_name: &str, extra: Option<serde_json::Value>) {
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

/// 上报错误到 Sentry。
pub fn capture_error(error: &str, error_type: Option<&str>) {
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

/// 基于 hostname + MAC 地址生成确定性匿名机器 ID。
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
