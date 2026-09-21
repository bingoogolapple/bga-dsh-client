//! DSH 版本管理（设置页「版本管理」面板）。
//!
//! 能力：
//! - 列出所有可选版本（本地已下载 + 内置 + 远程缓存），语义化倒序；
//! - 后台从 npm registry 拉取远程版本列表（1 小时缓存）；
//! - 后台 `npm install` 下载指定版本到 `~/.dsh/bga-dsh-client/dsh-versions/<ver>/`；
//! - 删除已下载版本、切换当前生效版本、切换 npm 下载源。
//!
//! 所有耗时操作（网络 / npm install / 删目录）都在后台线程执行，完成后通过
//! `dsh-versions-refreshed` / `dsh-download-progress` 事件通知前端，命令本身立即返回。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

/// 版本信息条目（前端渲染用）。
#[derive(Serialize, Clone)]
pub struct DshVersionEntry {
    version: String,
    local: bool,
    active: bool,
    builtin: bool,
}

/// 远程版本缓存过期时间（1小时）。
const REMOTE_CACHE_TTL_SECS: u64 = 3600;

/// 默认 npm registry（用户未指定且设置里没有时使用）。
const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";
static DOWNLOADS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
/// Serialize destructive operations on the versions directory. Download and delete
/// must never manipulate the same directory concurrently.
static VERSION_OPS: OnceLock<Mutex<()>> = OnceLock::new();

fn validate_version(version: &str) -> Result<(), String> {
    if semver::Version::parse(version.trim_start_matches('v')).is_err()
        || version.contains('/')
        || version.contains('\\')
        || version.contains("..")
    {
        return Err(format!("非法 dsh 版本: {version}"));
    }
    Ok(())
}

fn validated_registry(app: &AppHandle) -> Result<String, String> {
    let registry = crate::state::settings(app)
        .npm_registry
        .clone()
        .unwrap_or_else(|| DEFAULT_REGISTRY.into());
    let registry = registry.trim_end_matches('/');
    if [DEFAULT_REGISTRY, "https://registry.npmmirror.com"].contains(&registry) {
        Ok(registry.to_string())
    } else {
        Err(format!("不支持的 npm 下载源: {registry}"))
    }
}

fn version_path(root: &std::path::Path, version: &str) -> Result<PathBuf, String> {
    validate_version(version)?;
    if !root.exists() {
        return Ok(root.join(version));
    }
    let root = root
        .canonicalize()
        .map_err(|e| format!("版本目录不可用: {e}"))?;
    let candidate = root.join(version);
    if candidate.exists() {
        let actual = candidate
            .canonicalize()
            .map_err(|e| format!("版本目录不可用: {e}"))?;
        if actual.parent() != Some(root.as_path()) {
            return Err(format!("版本路径越界: {version}"));
        }
    }
    Ok(candidate)
}

/// `~/.dsh/bga-dsh-client/dsh-versions/` 目录：存放用户手动下载的各版本 dsh。
/// 与 settings.json 同级（~/.dsh/bga-dsh-client/），便于用户管理和清理。
pub fn dsh_versions_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .home_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(".dsh")
        .join("bga-dsh-client")
        .join("dsh-versions")
}

/// 远程版本列表缓存路径（files_dir/dsh-versions-cache.json）。
fn dsh_remote_cache_path(app: &AppHandle) -> PathBuf {
    crate::service::files_dir(app).join("dsh-versions-cache.json")
}

/// 读取内置运行时的 dsh 版本（runtime-manifest.json → dshVersion）。
pub fn builtin_dsh_version(app: &AppHandle) -> Option<String> {
    let rt = crate::service::runtime_root(app)?;
    let manifest = rt.join("runtime-manifest.json");
    let text = std::fs::read_to_string(manifest).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("dshVersion")?.as_str().map(String::from)
}

/// 读磁盘缓存的远程版本列表。
fn load_remote_versions_cache(app: &AppHandle) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(dsh_remote_cache_path(app)) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 写磁盘缓存的远程版本列表。
fn save_remote_versions_cache(app: &AppHandle, versions: &[String]) {
    if let Ok(text) = serde_json::to_string_pretty(versions) {
        let _ = std::fs::write(dsh_remote_cache_path(app), text);
    }
}

/// 列出所有版本（本地已下载 + 内置 + 远程缓存），合并去重后返回。
///
/// 返回值写成 `Result<Vec<DshVersionEntry>, String>` 而非直接返回 Vec：
/// `#[tauri::command]` 对 `-> Result<(), String>` 这类签名依赖 never type fallback，
/// 在 Rust 2024 会变成硬错误；统一返回 Result 既规避该问题，也让前端能区分错误。
#[tauri::command]
pub fn dsh_list_versions(app: AppHandle) -> Result<Vec<DshVersionEntry>, String> {
    Ok(list_versions(&app))
}

/// 列出所有版本（内部实现，供命令与其他后台线程复用，不经过 Tauri 序列化层）。
pub fn list_versions(app: &AppHandle) -> Vec<DshVersionEntry> {
    let versions_dir = dsh_versions_dir(app);
    let active = crate::state::settings(app).dsh_version.clone();
    let builtin = builtin_dsh_version(app);

    let mut map: HashMap<String, DshVersionEntry> = HashMap::new();

    // 1. 本地已下载
    if let Ok(entries) = std::fs::read_dir(&versions_dir) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let ver = entry.file_name().to_string_lossy().to_string();
            let bin_js = entry
                .path()
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib")
                .join("bin.js");
            if bin_js.exists() {
                let active_flag = active.as_deref() == Some(ver.as_str());
                let builtin_flag = builtin.as_deref() == Some(ver.as_str());
                map.insert(
                    ver.clone(),
                    DshVersionEntry {
                        version: ver,
                        local: true,
                        active: active_flag,
                        builtin: builtin_flag,
                    },
                );
            }
        }
    }

    // 2. 内置版本（可能不在 dsh-versions 目录里）
    if let Some(ref bv) = builtin {
        map.entry(bv.clone()).or_insert_with(|| DshVersionEntry {
            version: bv.clone(),
            local: false,
            active: active.as_deref() == Some(bv.as_str()),
            builtin: true,
        });
    }

    // 3. 远程缓存
    for rv in load_remote_versions_cache(app) {
        map.entry(rv.clone()).or_insert_with(|| DshVersionEntry {
            version: rv,
            local: false,
            active: false,
            builtin: false,
        });
    }

    // 排序：active 优先，然后版本号语义化倒序（0.1.10 > 0.1.9，而非字典序）
    let mut result: Vec<DshVersionEntry> = map.into_values().collect();
    result.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then_with(|| crate::version::compare_versions(&b.version, &a.version))
    });
    result
}

/// 后台拉取远程版本列表（npm registry → 缓存 → 事件通知前端）。
#[tauri::command]
pub fn dsh_refresh_remote_versions(app: AppHandle) -> Result<(), String> {
    let handle = app.clone();
    std::thread::spawn(move || match fetch_remote_versions_blocking(&handle) {
        Ok(list) => {
            let _ = handle.emit(
                "dsh-versions-refreshed",
                serde_json::json!({
                    "ok": true,
                    "action": "refresh",
                    "list": list,
                }),
            );
        }
        Err(error) => {
            let _ = handle.emit(
                "dsh-versions-refreshed",
                serde_json::json!({
                    "ok": false,
                    "action": "refresh",
                    "list": list_versions(&handle),
                    "error": error,
                }),
            );
        }
    });
    Ok(())
}

/// 检查远程版本缓存是否过期，过期则自动后台刷新。
/// 前端在打开版本管理页面时调用，避免用户手动点击「刷新」。
#[tauri::command]
pub fn dsh_maybe_refresh_remote_versions(app: AppHandle) -> Result<(), String> {
    let path = dsh_remote_cache_path(&app);
    // 缓存文件不存在 → 需要刷新
    let stale = match std::fs::metadata(&path) {
        Ok(meta) => match meta.modified() {
            Ok(mtime) => mtime
                .elapsed()
                .map(|d| d.as_secs() > REMOTE_CACHE_TTL_SECS)
                .unwrap_or(true),
            Err(_) => true,
        },
        Err(_) => true,
    };
    if stale {
        dsh_refresh_remote_versions(app)?;
    }
    Ok(())
}

/// 拉取远程版本 → 写缓存 → 返回合并后的完整列表（在后台线程调用，可阻塞）。
fn fetch_remote_versions_blocking(app: &AppHandle) -> Result<Vec<DshVersionEntry>, String> {
    let registry = validated_registry(app)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp: serde_json::Value = client
        .get(format!("{registry}/@deepseek-ai%2Fdsh"))
        .send()
        .map_err(|e| format!("网络请求失败: {}", e))?
        .json()
        .map_err(|e| format!("解析响应失败: {}", e))?;
    let versions_map = resp
        .get("versions")
        .and_then(|v| v.as_object())
        .ok_or_else(|| "无法获取版本列表".to_string())?;
    let mut versions: Vec<String> = versions_map.keys().cloned().collect();
    versions.sort_by(|a, b| crate::version::compare_versions(b, a));
    save_remote_versions_cache(app, &versions);
    Ok(list_versions(app))
}

/// 后台下载指定版本（spawn 线程执行 npm install）。
#[tauri::command]
pub fn dsh_download_version(app: AppHandle, version: String) -> Result<(), String> {
    validate_version(&version)?;
    let downloads = DOWNLOADS.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let mut active = downloads.lock().unwrap_or_else(|e| e.into_inner());
    if !active.insert(version.clone()) {
        return Err(format!("版本 {} 正在下载", version));
    }
    drop(active);
    let root = dsh_versions_dir(&app);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let target = version_path(&root, &version)?;
    let _operation = VERSION_OPS
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let bin_js = target
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");
    if bin_js.exists() {
        downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&version);
        return Err(format!("版本 {} 已下载", version));
    }
    // 目录存在但 bin.js 不存在 → 上次下载失败残留，清理后重新下载
    if target.exists() {
        let _ = std::fs::remove_dir_all(&target);
    }
    if let Err(e) = std::fs::create_dir_all(&target) {
        downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&version);
        return Err(e.to_string());
    }
    drop(_operation);

    let handle = app.clone();
    let ver = version.clone();
    std::thread::spawn(move || {
        let _operation = VERSION_OPS
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let result = do_download_dsh(&handle, &ver, &target);
        let stage = match &result {
            Ok(()) => "done",
            Err(_) => "error",
        };
        let message = result.err().unwrap_or_default();
        let _ = handle.emit(
            "dsh-download-progress",
            serde_json::json!({
                "version": ver,
                "stage": stage,
                "message": message,
            }),
        );
        if stage == "error" {
            let _ = std::fs::remove_dir_all(&target);
        }
        if let Some(set) = DOWNLOADS.get() {
            set.lock().unwrap_or_else(|e| e.into_inner()).remove(&ver);
        }
    });
    Ok(())
}

/// 执行 npm install @deepseek-ai/dsh@<version>（阻塞，在后台线程调用）。
///
/// 要点：
/// 1. 注入 `npm_config_cache` 到应用缓存目录（绕开用户 ~/.npm 可能的 root 属主/损坏）；
///    注意不能用 files_dir/npm-cache（服务启动用的共享缓存），必须用独立目录；
/// 2. 读取用户选择的 registry（官方源 / 淘宝镜像）；
/// 3. 带 540s 超时 + 管道防死锁，错误消息取 stderr 尾部（非空时）或 stdout 尾部。
fn do_download_dsh(app: &AppHandle, version: &str, target: &std::path::Path) -> Result<(), String> {
    let _ = app.emit(
        "dsh-download-progress",
        serde_json::json!({
            "version": version,
            "stage": "installing",
            "message": "",
        }),
    );

    let registry = validated_registry(app)?;

    let cache_dir = crate::service::files_dir(app).join("dsh-download-cache");
    let _ = std::fs::create_dir_all(&cache_dir);

    #[cfg(not(windows))]
    let cmd = if let Some(rt) = crate::service::runtime_root(app) {
        if let Some((node, _)) = crate::service::runtime_entry(&rt) {
            let npm = if cfg!(windows) {
                rt.join("nd/node_modules/npm/bin/npm-cli.js")
            } else {
                rt.join("nd/lib/node_modules/npm/bin/npm-cli.js")
            };
            format!(
                "exec \"{}\" \"{}\" install --save-exact --registry \"{}\" @deepseek-ai/dsh@{}",
                node.display(),
                npm.display(),
                registry,
                version
            )
        } else {
            format!(
                "{}npm install --save-exact --registry \"{}\" @deepseek-ai/dsh@{}",
                crate::service::shell_path_prefix(),
                registry,
                version
            )
        }
    } else {
        format!(
            "{}npm install --save-exact --registry \"{}\" @deepseek-ai/dsh@{}",
            crate::service::shell_path_prefix(),
            registry,
            version
        )
    };
    #[cfg(windows)]
    let cmd = if let Some(rt) = crate::service::runtime_root(app) {
        if let Some((node, _)) = crate::service::runtime_entry(&rt) {
            let npm = rt.join("nd/node_modules/npm/bin/npm-cli.js");
            format!(
                "\"{}\" \"{}\" install --save-exact --registry \"{}\" @deepseek-ai/dsh@{}",
                node.display(),
                npm.display(),
                registry,
                version
            )
        } else {
            format!("npm install --save-exact --registry \"{registry}\" @deepseek-ai/dsh@{version}")
        }
    } else {
        format!("npm install --save-exact --registry \"{registry}\" @deepseek-ai/dsh@{version}")
    };

    // 带超时 + 管道防死锁的 subprocess 执行（与 version::run_capture 同模式）
    use std::io::Read;
    #[cfg(not(windows))]
    use std::process::Command;
    use std::process::Stdio;
    use std::time::Instant;

    #[cfg(not(windows))]
    let mut child = Command::new("sh")
        .arg("-lc")
        .arg(&cmd)
        .current_dir(target)
        .env("npm_config_cache", &cache_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("npm 进程启动失败: {}", e))?;
    #[cfg(windows)]
    let mut child = crate::hidden_command("cmd")
        .arg("/C")
        .arg(&cmd)
        .current_dir(target)
        .env("npm_config_cache", &cache_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("npm 进程启动失败: {}", e))?;

    // 独立线程 drain stdout/stderr，避免大输出（>64KB 管道缓冲）互相阻塞
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(ref mut so) = stdout {
            let _ = so.read_to_string(&mut s);
        }
        s
    });
    let err_handle = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(ref mut se) = stderr {
            let _ = se.read_to_string(&mut s);
        }
        s
    });

    let timeout = std::time::Duration::from_secs(540);
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("npm install 超时（超过 {} 秒）", timeout.as_secs()));
                }
            }
            Err(e) => return Err(format!("npm 进程异常: {}", e)),
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let out = out_handle.join().unwrap_or_default();
    let err = err_handle.join().unwrap_or_default();

    if !status.success() {
        // 取 stderr 尾部作为错误详情；空时取 stdout 尾部（npm 有时把错误写到 stdout）
        let detail = if !err.trim().is_empty() { err } else { out };
        let tail: String = detail
            .lines()
            .rev()
            .take(10)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("npm install 失败：\n{}", tail));
    }

    // 验证安装
    let bin_js = target
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");
    if !bin_js.exists() {
        return Err("安装验证失败：找不到 bin.js".into());
    }
    Ok(())
}

/// 后台删除已下载版本。
#[tauri::command]
pub fn dsh_delete_version(app: AppHandle, version: String) -> Result<(), String> {
    validate_version(&version)?;
    let target = version_path(&dsh_versions_dir(&app), &version)?;
    if !target.exists() {
        return Err(format!("版本 {} 不存在", version));
    }
    // 不允许删除正在使用的版本
    if crate::state::settings(&app).dsh_version.as_deref() == Some(&version) {
        return Err(format!("版本 {} 正在使用，无法删除", version));
    }
    if DOWNLOADS
        .get_or_init(|| Mutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&version)
    {
        return Err(format!("版本 {} 正在下载，无法删除", version));
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        let _operation = VERSION_OPS
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let result = std::fs::remove_dir_all(&target);
        let _ = handle.emit(
            "dsh-versions-refreshed",
            serde_json::json!({
                "ok": result.is_ok(),
                "action": "delete",
                "list": list_versions(&handle),
                "error": result.err().map(|e| e.to_string()),
            }),
        );
    });
    Ok(())
}

/// 设置用户选定的 DSH 版本（Some 表示指定版本，None 表示恢复默认）。
#[tauri::command]
pub fn dsh_set_active_version(app: AppHandle, version: Option<String>) -> Result<(), String> {
    if let Some(ref v) = version {
        validate_version(v)?;
        let target = version_path(&dsh_versions_dir(&app), v)?;
        let installed = target
            .join("node_modules/@deepseek-ai/dsh/lib/bin.js")
            .is_file();
        let builtin = crate::dsh::builtin_dsh_version(&app).as_deref() == Some(v.as_str());
        if !installed && !builtin {
            return Err(format!("版本 {} 尚未完成安装", v));
        }
        let info = app.state::<crate::AppState>().sm.info(&app);
        if info.state == crate::service::ServiceState::Running && !info.mine {
            return Err("当前运行的是外部 DSH，无法切换版本".into());
        }
    }
    let target = version.clone();
    crate::state::update_settings(&app, |s| s.dsh_version = target)?;
    crate::telemetry::capture_event(
        "dsh_active_version_set",
        Some(serde_json::json!({ "version": version })),
    );
    Ok(())
}

/// Atomically switch the selected version when the service is running.  The
/// previous setting is retained until the new process reaches Running; a
/// failed candidate is rolled back and the previous process is started again.
#[tauri::command]
pub fn dsh_switch_active_version(app: AppHandle, version: Option<String>) -> Result<(), String> {
    let previous = crate::state::settings(&app).dsh_version.clone();
    let was_running =
        app.state::<crate::AppState>().sm.info(&app).state == crate::service::ServiceState::Running;
    dsh_set_active_version(app.clone(), version)?;
    if !was_running {
        return Ok(());
    }
    app.state::<crate::AppState>().sm.restart(&app);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let state = app.state::<crate::AppState>().sm.info(&app).state;
        if state == crate::service::ServiceState::Running {
            return Ok(());
        }
        if state == crate::service::ServiceState::Error {
            break;
        }
    }
    // Restore the known-good selection and restart it.  Return the original
    // failure after rollback so the UI can explain that the candidate failed.
    let _ = dsh_set_active_version(app.clone(), previous);
    app.state::<crate::AppState>().sm.restart(&app);
    Err("新版本启动失败，已自动恢复上一版本".into())
}

/// 获取当前选定的 DSH 版本。
#[tauri::command]
pub fn dsh_active_version(app: AppHandle) -> Option<String> {
    crate::state::settings(&app).dsh_version.clone()
}

/// 保存用户选择的 npm 下载源（官方源 / 淘宝镜像）。
#[tauri::command]
pub fn dsh_set_registry(app: AppHandle, registry: String) -> Result<(), String> {
    let valid = [DEFAULT_REGISTRY, "https://registry.npmmirror.com"];
    if !valid.contains(&registry.trim_end_matches('/')) {
        return Err(format!("不支持的 npm 下载源: {registry}"));
    }
    let registry = registry.trim_end_matches('/').to_string();
    let r = registry.clone();
    crate::state::update_settings(&app, |s| s.npm_registry = Some(r))?;
    crate::telemetry::capture_event(
        "dsh_registry_set",
        Some(serde_json::json!({ "registry": registry })),
    );
    Ok(())
}

/// 获取当前选择的 npm 下载源（None 时前端显示为默认官方源）。
#[tauri::command]
pub fn dsh_get_registry(app: AppHandle) -> Option<String> {
    crate::state::settings(&app).npm_registry.clone()
}

#[cfg(test)]
mod tests {
    use super::validate_version;

    #[test]
    fn rejects_path_like_versions() {
        assert!(validate_version("../tmp").is_err());
        assert!(validate_version("0.1.2/../../x").is_err());
        assert!(validate_version("0.1.2").is_ok());
        assert!(validate_version("v0.1.2").is_ok());
    }
}
