//! 应用更新检测：查询 GitHub Releases 最新版本，与当前版本比较，
//! 结果缓存到本地（update-cache.json），设置页据此展示「有新版本」提示。
//!
//! 行为要点：
//! - 启动 5 秒后后台检查一次，遵守 24h 最小间隔（避免频繁请求 GitHub API）；
//! - 设置页「检查更新」可手动触发；检查完成后通过 `update-available` 事件广播；
//! - 用户可「忽略」某个版本：忽略后不再高亮提示，直到发布更新的版本；
//! - 网络失败静默降级：保留上次成功缓存，不打扰用户。

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

/// 两次自动检查的最小间隔：24 小时。
const CHECK_INTERVAL: u64 = 24 * 60 * 60;
/// HTTP 请求超时。
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
/// GitHub Releases API（latest release 接口，返回最新非 pre-release、非 draft）。
const RELEASE_API: &str =
    "https://api.github.com/repos/bingoogolapple/bga-dsh-client/releases/latest";
/// 浏览器打开的下载页。
const DOWNLOAD_URL: &str = "https://github.com/bingoogolapple/bga-dsh-client/releases/latest";

/// 本地缓存的更新检查结果（update-cache.json，与 settings.json 同目录）。
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct UpdateCache {
    /// 最近一次成功检查到的最新版本号（已去掉 v 前缀）。
    #[serde(default)]
    pub latest_version: Option<String>,
    /// 最近一次成功检查的时间（Unix 秒）。
    #[serde(default)]
    pub checked_at: Option<u64>,
    /// 是否已忽略当前最新版本（用户点过「忽略」）。
    #[serde(default)]
    pub dismissed: bool,
    /// 被忽略时的版本号；发布更新的版本后自动取消忽略。
    #[serde(default)]
    pub dismissed_version: Option<String>,
}

/// 前端可读的更新状态。
#[derive(Serialize, Clone)]
pub struct UpdateInfo {
    /// 当前客户端版本。
    pub current: String,
    /// 最新版本（None = 尚未成功检查过）。
    pub latest: Option<String>,
    /// 是否有可用更新（latest > current）。
    pub has_update: bool,
    /// 是否已忽略当前最新版本。
    pub dismissed: bool,
    /// 最近一次成功检查时间（Unix 秒）。
    pub checked_at: Option<u64>,
    /// 最近一次检查结果：ok / error / idle（从未检查过）。
    pub status: String,
    /// 下载页 URL。
    pub download_url: String,
}

/// GitHub Releases API 响应（只取需要的字段）。
#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 缓存文件路径：与 settings.json 同目录（config_path 的父目录）。
/// config_path 在 setup 阶段已填充；更新检查均在 setup 之后触发，取不到时回退临时目录。
fn cache_path(handle: &AppHandle) -> PathBuf {
    let dir = handle
        .state::<AppState>()
        .config_path
        .lock()
        .unwrap()
        .clone();
    dir.and_then(|p| p.parent().map(|d| d.join("update-cache.json")))
        .unwrap_or_else(|| std::env::temp_dir().join("bga-dsh-client-update-cache.json"))
}

fn load_cache(handle: &AppHandle) -> UpdateCache {
    fs::read_to_string(cache_path(handle))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(handle: &AppHandle, cache: &UpdateCache) {
    let path = cache_path(handle);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(cache) {
        let _ = fs::write(path, json);
    }
}

/// 版本比较：latest 是否严格大于 current（兼容 v 前缀；解析失败视为无更新）。
fn version_gt(current: &str, latest: &str) -> bool {
    match (
        semver::Version::parse(current.trim_start_matches('v')),
        semver::Version::parse(latest.trim_start_matches('v')),
    ) {
        (Ok(c), Ok(l)) => l > c,
        _ => false,
    }
}

fn build_info(handle: &AppHandle, cache: &UpdateCache, status: &str) -> UpdateInfo {
    let current = handle.package_info().version.to_string();
    let has_update = cache
        .latest_version
        .as_deref()
        .is_some_and(|l| version_gt(&current, l));
    UpdateInfo {
        current,
        latest: cache.latest_version.clone(),
        has_update,
        dismissed: cache.dismissed,
        checked_at: cache.checked_at,
        status: status.into(),
        download_url: DOWNLOAD_URL.into(),
    }
}

/// 当前缓存状态（前端打开设置页时读取，不做网络请求）。
pub fn info(handle: &AppHandle) -> UpdateInfo {
    let cache = load_cache(handle);
    let status = if cache.checked_at.is_none() {
        "idle"
    } else {
        "ok"
    };
    build_info(handle, &cache, status)
}

/// 真正执行一次网络检查（阻塞，需在后台线程调用），完成后写缓存并广播 `update-available`。
fn check_now(handle: &AppHandle) {
    let mut cache = load_cache(handle);
    let current = handle.package_info().version.to_string();
    let client = reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(format!("DeepSeekHarness/{current}"))
        .build();
    let release = client.ok().and_then(|c| {
        c.get(RELEASE_API)
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .ok()?
            .json::<GhRelease>()
            .ok()
    });
    match release {
        Some(rel) => {
            let latest = rel.tag_name.trim_start_matches('v').to_string();
            cache.latest_version = Some(latest.clone());
            cache.checked_at = Some(now_secs());
            // 被忽略的版本号与最新版本一致 → 保持忽略；发布更新的版本 → 取消忽略重新提示。
            if cache.dismissed && cache.dismissed_version.as_deref() != Some(latest.as_str()) {
                cache.dismissed = false;
            }
            save_cache(handle, &cache);
            let info = build_info(handle, &cache, "ok");
            crate::telemetry::capture_event(
                "update_check_result",
                Some(serde_json::json!({
                    "current": current,
                    "latest": latest,
                    "has_update": info.has_update,
                })),
            );
            let _ = handle.emit("update-available", &info);
        }
        None => {
            // 网络失败：广播 error（设置页提示检查失败），保留上次缓存。
            let info = build_info(handle, &cache, "error");
            let _ = handle.emit("update-available", &info);
        }
    }
}

/// 手动触发检查（后台线程执行，不阻塞调用方）。
pub fn trigger_check(handle: &AppHandle) {
    let h = handle.clone();
    std::thread::spawn(move || check_now(&h));
}

/// 启动延迟检查：5 秒后执行，遵守 24h 间隔（从未检查过则立即查）。
pub fn startup_check(handle: &AppHandle) {
    let h = handle.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        let cache = load_cache(&h);
        let due = cache
            .checked_at
            .is_none_or(|t| now_secs().saturating_sub(t) >= CHECK_INTERVAL);
        if due {
            check_now(&h);
        }
    });
}

/// 忽略当前最新版本（直到发布更新的版本才重新提示）。
pub fn dismiss(handle: &AppHandle) {
    let mut cache = load_cache(handle);
    if let Some(latest) = cache.latest_version.clone() {
        cache.dismissed = true;
        cache.dismissed_version = Some(latest);
        save_cache(handle, &cache);
    }
    let _ = handle.emit("update-available", &info(handle));
}

/// 在系统浏览器打开下载页。
pub fn open_download() {
    crate::open_url(DOWNLOAD_URL);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版本比较：语义化数值比较（非字符串），兼容 v 前缀；非法版本视为无更新。
    #[test]
    fn version_gt_works() {
        assert!(version_gt("0.0.2", "0.0.3"));
        assert!(version_gt("0.0.2", "1.0.0"));
        // semver 数值比较：0.9.0 < 0.10.0（字符串比较会得出相反结果）
        assert!(version_gt("0.9.0", "0.10.0"));
        assert!(!version_gt("0.0.3", "0.0.2"));
        assert!(!version_gt("0.0.2", "0.0.2"));
        // v 前缀兼容（GitHub tag 形如 v0.0.2）
        assert!(version_gt("v0.0.2", "v0.0.3"));
        // 非法版本：保守视为无更新
        assert!(!version_gt("abc", "0.1.0"));
        assert!(!version_gt("0.1.0", "not-a-version"));
    }

    /// 缓存字段序列化/反序列化完整（不依赖文件系统）。
    #[test]
    fn cache_roundtrip() {
        let cache = UpdateCache {
            latest_version: Some("0.3.0".into()),
            checked_at: Some(123456),
            dismissed: true,
            dismissed_version: Some("0.3.0".into()),
        };
        let json = serde_json::to_string(&cache).unwrap();
        let back: UpdateCache = serde_json::from_str(&json).unwrap();
        assert_eq!(back.latest_version, cache.latest_version);
        assert_eq!(back.checked_at, cache.checked_at);
        assert!(back.dismissed);
        assert_eq!(back.dismissed_version, cache.dismissed_version);
    }

    /// 缓存字段全部缺省时能解析为默认值（兼容精简/损坏的旧缓存）。
    #[test]
    fn cache_defaults() {
        let back: UpdateCache = serde_json::from_str("{}").unwrap();
        assert!(back.latest_version.is_none());
        assert!(back.checked_at.is_none());
        assert!(!back.dismissed);
        assert!(back.dismissed_version.is_none());
    }
}
