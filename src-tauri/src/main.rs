//! DeepSeekHarness 桌面壳：DSH Web GUI 的 Tauri 桌面应用。
//!
//! 行为要点：
//! - 启动时探测 127.0.0.1:3080，已有服务则直接复用，否则按设置拉起；
//! - 关闭窗口只是隐藏到托盘，服务不停；
//! - 托盘菜单提供 客户端设置 / 启动服务 / 重启服务 / 停止服务 / 退出应用；
//! - 退出应用时按设置决定 停止 或 放生 本应用启动的服务（放生的服务下次启动自动接管）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod i18n;
mod pairing;
mod service;
mod settings;
mod telemetry;
mod tray;
mod update;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tauri::{Emitter, Manager};

use crate::i18n::{tr, Locale};
use crate::pairing::Pairing;
use crate::service::ServiceManager;
use crate::settings::Settings;
use crate::tray::TrayMenu;

pub struct AppState {
    pub sm: ServiceManager,
    pub settings: Mutex<Settings>,
    pub config_path: Mutex<Option<PathBuf>>,
    /// 当前界面语言（来自 Harness 配置，watcher 动态更新）。
    pub locale: Mutex<Locale>,
    /// 托盘联动菜单（build_tray 后填充）。
    pub tray: Mutex<Option<Arc<TrayMenu>>>,
    /// 局域网扫码配对网关。
    pub pairing: Mutex<Pairing>,
    /// 设置页左下角版本信息缓存（磁盘 version-cache.json + 内存渲染快照）。
    /// get_version_info 秒回缓存，后台线程异步完整探测后刷新。
    pub version_cache: Mutex<VersionCache>,
}

/// Web 前端读取当前语言（zh / en）。
#[tauri::command]
fn get_locale(app: tauri::AppHandle) -> String {
    i18n::current(&app).as_str().into()
}

#[tauri::command]
fn query_status(app: tauri::AppHandle) -> service::ServiceInfo {
    app.state::<AppState>().sm.info(&app)
}

/// 应用是否内置 Node.js 运行时（false 时为普通版）。前端据此选择展示
/// 内置 runtime 版本还是系统 PATH 版本。
#[tauri::command]
fn has_bundled_runtime(app: tauri::AppHandle) -> bool {
    service::runtime_root(&app).is_some()
}

#[tauri::command]
fn get_settings(app: tauri::AppHandle) -> Settings {
    app.state::<AppState>().settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(app: tauri::AppHandle, stop_service_on_quit: bool) -> Result<(), String> {
    let s = Settings {
        stop_service_on_quit,
    };
    let path = app
        .state::<AppState>()
        .config_path
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| tr(i18n::current(&app), "set.config_dir_missing", &[]))?;
    s.save(&path)?;
    *app.state::<AppState>().settings.lock().unwrap() = s;
    crate::telemetry::capture_event(
        "settings_saved",
        Some(serde_json::json!({
            "stop_service_on_quit": stop_service_on_quit,
        })),
    );
    Ok(())
}

#[tauri::command]
fn read_service_log(app: tauri::AppHandle, limit: Option<usize>) -> Vec<String> {
    service::read_log_tail(&app, limit.unwrap_or(200))
}

#[tauri::command]
fn read_pairing_log(app: tauri::AppHandle) -> Vec<String> {
    pairing::read_log_tail(&app, 200)
}

// ---------------------------------------------------------------------------
// 版本信息（设置页左下角展示）
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct ToolVersions {
    node: String,
    pnpm: String,
    dsh: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct VersionInfo {
    /// 内置运行时包（resources/runtime）内的版本。
    runtime: ToolVersions,
    /// 系统 PATH 上当前生效的版本。
    system: ToolVersions,
    /// 运行中服务（127.0.0.1:DSH_PORT）自报的版本；离线或查询失败为 None。
    running: Option<String>,
    /// 3080 端口当前是否有服务在监听（host.describe 查不到版本但服务在线时，
    /// 前端可据此展示「运行中（版本未知）」而非误导性的「未安装」）。
    service_up: bool,
}

/// 执行 `prog [args]` 并捕获 stdout 原文（trim 后）；超时或失败返回 None。
/// 不经 shell（Windows 系统命令走 cmd /C，见 sys_version）。
/// `extra_path`：可选的 PATH 覆盖值（仅 Unix 生效），用于在 Dock 启动等短 PATH 场景下定位 node/pnpm/dsh。
fn run_capture(
    prog: &Path,
    args: &[&std::ffi::OsStr],
    timeout: std::time::Duration,
    _extra_path: Option<&str>,
) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;
    let mut builder = Command::new(prog);
    builder.args(args);
    #[cfg(not(windows))]
    if let Some(path) = _extra_path {
        builder.env("PATH", path);
    }
    let mut child = builder
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    // 在独立线程主动 drain stdout/stderr：版本命令输出很小，但若某命令在退出前
    // 输出超过管道缓冲（64KB）会阻塞自身、永不退出，最终只能等超时被误杀。主动读掉即无此死锁。
    let mut stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take()?;
    let out_handle = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err_handle = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
            Err(_) => return None,
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    };
    let out = out_handle.join().unwrap_or_default();
    let _err = err_handle.join().unwrap_or_default();
    if !status.success() {
        #[cfg(debug_assertions)]
        eprintln!("[probe] {prog:?} failed (exit {:?}): {_err}", status.code());
        return None;
    }
    Some(out.trim().to_string())
}

/// 系统 PATH 上某命令的版本（Windows 需经 cmd /C 解析 .cmd shim）。
/// `extra_path`：Unix 下拼入 nvm/pnpm home 等路径，避免 Dock 启动时找不到工具。
/// 版本探测超时用 8s：Corepack 的 pnpm shim 冷启动可达 2.7~4s，写死 3s 会被误杀成「未安装」。
#[cfg(not(windows))]
fn sys_version(cmd: &str, extra_path: Option<&str>) -> Option<String> {
    use std::ffi::OsStr;
    run_capture(
        Path::new(cmd),
        &[OsStr::new("--version")],
        std::time::Duration::from_secs(8),
        extra_path,
    )
}

#[cfg(windows)]
fn sys_version(cmd: &str, _extra_path: Option<&str>) -> Option<String> {
    use std::ffi::OsStr;
    run_capture(
        Path::new("cmd"),
        &[OsStr::new("/C"), OsStr::new(cmd), OsStr::new("--version")],
        std::time::Duration::from_secs(8),
        None,
    )
}

/// 探测运行中服务的真实版本：POST /api/host.describe（127.0.0.1:DSH_PORT）。
/// 离线 / 超时 / 非本协议响应一律返回 None（前端回退到 PATH / 内置探测）。
/// 服务端在 host.describe 中自报 @deepseek-ai/dsh 包的版本，因此 npx 拉起的
/// 服务也能拿到真实运行版本，而不是 PATH 上未必存在的 dsh 探测值。
fn running_dsh_version() -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;
    let body = serde_json::json!({
        "type": "client-request",
        "rpcId": "version-probe",
        "method": "host.describe",
        "payload": {},
    });
    let resp: serde_json::Value = client
        .post(format!(
            "http://127.0.0.1:{}/api/host.describe",
            service::DSH_PORT
        ))
        .json(&body)
        .send()
        .ok()?
        .json()
        .ok()?;
    let version = resp.get("result")?.get("value")?.get("version")?.as_str()?;
    // 旧构建的占位符（0.0.1）不代表真实版本，忽略并回退到 PATH / 内置探测。
    if version == "0.0.1" {
        return None;
    }
    Some(version.to_owned())
}

/// 兜底探测：host.describe 查不到版本（旧版服务返回 0.0.1 占位符 / 探测失败）时，
/// 直接从应用自己的 npx 缓存目录读取 @deepseek-ai/dsh 包的版本。
/// npx 拉起的服务其包就躺在 <files_dir>/npm-cache/_npx/<hash>/node_modules/
/// @deepseek-ai/dsh/package.json 里，缓存里的版本即实际拉起服务的那个包的版本。
/// 遍历全部缓存目录取 mtime 最新的（防止读到历史残留的旧包）。
fn npx_cached_dsh_version(app: &tauri::AppHandle) -> Option<String> {
    let npx_dir = service::files_dir(app).join("npm-cache").join("_npx");
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(&npx_dir).ok()?.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let manifest = entry
            .path()
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("package.json");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(version) = json.get("version").and_then(|v| v.as_str()) else {
            continue;
        };
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let newer = best.as_ref().map(|(bt, _)| mtime > *bt).unwrap_or(true);
        if newer {
            best = Some((mtime, version.to_owned()));
        }
    }
    best.map(|(_, v)| v)
}

/// 完整探测一次版本信息（7 个探测并行），返回 VersionInfo。
/// 超时放宽到 8s：Corepack 的 pnpm shim 冷启动可达 2.7~4s，3s 会被误杀成「未安装」。
/// 该函数只做探测不写缓存，供后台线程与启动遥测复用。
fn probe_versions(app: &tauri::AppHandle) -> VersionInfo {
    use std::ffi::OsStr;
    // 内置运行时入口（node 可执行 + dsh 的 bin.js；pnpm 走 pnpm.cjs，均用内置 node 直跑，跨平台安全）
    let (node_bin, dsh_js, pnpm_cjs) = match service::runtime_root(app) {
        Some(rt) => match service::runtime_entry(&rt) {
            Some((node, dsh)) => {
                let pnpm = rt
                    .join("rt")
                    .join("node_modules")
                    .join("pnpm")
                    .join("bin")
                    .join("pnpm.cjs");
                (Some(node), Some(dsh), pnpm.is_file().then_some(pnpm))
            }
            None => (None, None, None),
        },
        None => (None, None, None),
    };
    let timeout = std::time::Duration::from_secs(8);
    // 系统探测：拼入 nvm/pnpm home 等路径（Dock 启动时默认 PATH 极短）
    #[cfg(not(windows))]
    let sys_path = {
        let mut dirs = service::path_dirs();
        dirs.reverse(); // nvm 等高优先路径排前面
        let extra = dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let existing = std::env::var("PATH").unwrap_or_default();
        if extra.is_empty() {
            existing
        } else {
            format!("{extra}:{existing}")
        }
    };
    // 七个探测并行跑（<1s 返回），互不阻塞
    let (r_node, r_pnpm, r_dsh, s_node, s_pnpm, s_dsh, running) = std::thread::scope(|s| {
        let rn = s.spawn(|| match &node_bin {
            Some(b) => run_capture(b, &[OsStr::new("--version")], timeout, None),
            None => None,
        });
        let rp = s.spawn(|| match (&node_bin, &pnpm_cjs) {
            (Some(b), Some(p)) => {
                run_capture(b, &[p.as_os_str(), OsStr::new("--version")], timeout, None)
            }
            _ => None,
        });
        let rd = s.spawn(|| match (&node_bin, &dsh_js) {
            (Some(b), Some(d)) => {
                run_capture(b, &[d.as_os_str(), OsStr::new("--version")], timeout, None)
            }
            _ => None,
        });
        #[cfg(not(windows))]
        let sn = s.spawn(|| sys_version("node", Some(&sys_path)));
        #[cfg(not(windows))]
        let sp = s.spawn(|| sys_version("pnpm", Some(&sys_path)));
        #[cfg(not(windows))]
        let sd = s.spawn(|| sys_version("dsh", Some(&sys_path)));
        #[cfg(windows)]
        let sn = s.spawn(|| sys_version("node", None));
        #[cfg(windows)]
        let sp = s.spawn(|| sys_version("pnpm", None));
        #[cfg(windows)]
        let sd = s.spawn(|| sys_version("dsh", None));
        let rv = s.spawn(running_dsh_version);
        (
            rn.join().unwrap_or(None),
            rp.join().unwrap_or(None),
            rd.join().unwrap_or(None),
            sn.join().unwrap_or(None),
            sp.join().unwrap_or(None),
            sd.join().unwrap_or(None),
            rv.join().unwrap_or(None),
        )
    });
    let service_up = service::ServiceManager::is_up();
    // host.describe 查不到版本（旧版服务返回 0.0.1 占位符 / 探测失败）但服务在线时，
    // 从应用自己的 npx 缓存读 dsh 包版本兜底——该包正是实际拉起服务的那个包。
    let running = running.or_else(|| {
        if service_up {
            npx_cached_dsh_version(app)
        } else {
            None
        }
    });
    let miss = tr(i18n::current(app), "ver.not_installed", &[]).to_string();
    VersionInfo {
        runtime: ToolVersions {
            node: r_node.unwrap_or_else(|| miss.clone()),
            pnpm: r_pnpm.unwrap_or_else(|| miss.clone()),
            dsh: r_dsh.unwrap_or_else(|| miss.clone()),
        },
        system: ToolVersions {
            node: s_node.unwrap_or_else(|| miss.clone()),
            pnpm: s_pnpm.unwrap_or_else(|| miss.clone()),
            dsh: s_dsh.unwrap_or_else(|| miss.clone()),
        },
        running,
        service_up,
    }
}

/// 版本缓存状态：内存里只存「上次完整探测时间」，探测结果落在磁盘
/// version-cache.json，这样设置窗口每次打开都能秒回最近一次结果，
/// 之后由后台线程异步重新探测并 emit 刷新（不阻塞窗口）。
pub struct VersionCache {
    pub last_probe: Option<std::time::Instant>,
}

impl VersionCache {
    pub fn new() -> Self {
        Self { last_probe: None }
    }
}

impl Default for VersionCache {
    fn default() -> Self {
        Self::new()
    }
}

/// 版本缓存文件路径（files_dir/version-cache.json）。
fn version_cache_path(app: &tauri::AppHandle) -> PathBuf {
    service::files_dir(app).join("version-cache.json")
}

/// 读磁盘缓存（首次冷启动 / 文件缺失返回 None）。
fn load_version_cache(app: &tauri::AppHandle) -> Option<VersionInfo> {
    let path = version_cache_path(app);
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 写磁盘缓存。
fn save_version_cache(app: &tauri::AppHandle, vi: &VersionInfo) {
    let path = version_cache_path(app);
    if let Ok(text) = serde_json::to_string_pretty(vi) {
        let _ = std::fs::write(path, text);
    }
}

/// 重映射缓存里的「未安装」文案到当前语言。
/// 探测时「未安装」按当次语言写死进缓存（version-cache.json 跨启动/跨语言复用），
/// 语言切换或下次以另一语言启动时，读缓存会拿到旧语言文案；这里读时统一归一。
fn relocalize_miss(mut vi: VersionInfo, app: &tauri::AppHandle) -> VersionInfo {
    let cur = tr(i18n::current(app), "ver.not_installed", &[]);
    let zh = tr(i18n::Locale::Zh, "ver.not_installed", &[]);
    let en = tr(i18n::Locale::En, "ver.not_installed", &[]);
    for field in [
        &mut vi.runtime.node,
        &mut vi.runtime.pnpm,
        &mut vi.runtime.dsh,
        &mut vi.system.node,
        &mut vi.system.pnpm,
        &mut vi.system.dsh,
    ] {
        if field.as_str() == zh.as_str() || field.as_str() == en.as_str() {
            *field = cur.clone();
        }
    }
    vi
}

/// 距上次完整探测超过该时长才重新探测（设置窗口高频开关时避免反复跑慢探测）。
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// 设置页左下角版本信息：**秒回缓存，不阻塞**。
/// - 有内存/磁盘缓存 → 立即返回；
/// - 距上次探测超过 PROBE_INTERVAL（或从未探测）→ 后台线程跑完整探测，
///   结束后写磁盘缓存、更新内存时间戳并 emit `version-refreshed` 让前端刷新。
#[tauri::command]
fn get_version_info(app: tauri::AppHandle) -> VersionInfo {
    // 优先返回磁盘缓存（秒回）。
    if let Some(cached) = load_version_cache(&app) {
        try_probe_async(&app);
        return relocalize_miss(cached, &app);
    }
    // 无任何缓存：返回占位（app 版本 + 空），后台探测补上真实值。
    let placeholder = VersionInfo {
        runtime: ToolVersions {
            node: "—".into(),
            pnpm: "—".into(),
            dsh: "—".into(),
        },
        system: ToolVersions {
            node: "—".into(),
            pnpm: "—".into(),
            dsh: "—".into(),
        },
        running: None,
        service_up: service::ServiceManager::is_up(),
    };
    try_probe_async(&app);
    placeholder
}

/// 若距上次探测超过阈值，spawn 后台线程完整探测（不阻塞命令返回）。
fn try_probe_async(app: &tauri::AppHandle) {
    {
        let state = app.state::<AppState>();
        let mut cache = state.version_cache.lock().unwrap();
        if let Some(t) = cache.last_probe {
            if t.elapsed() < PROBE_INTERVAL {
                return;
            }
        }
        cache.last_probe = Some(std::time::Instant::now());
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        let vi = probe_versions(&handle);
        let _ = handle.emit("version-refreshed", vi.clone());
        save_version_cache(&handle, &vi);
    });
}

#[tauri::command]
fn get_pairing_info(app: tauri::AppHandle) -> Result<pairing::PairingInfo, String> {
    pairing::info(&app)
}

#[tauri::command]
fn pairing_regen(app: tauri::AppHandle) -> Result<pairing::PairingInfo, String> {
    pairing::regen(&app)
}

#[tauri::command]
fn pairing_start(app: tauri::AppHandle) -> Result<pairing::PairingInfo, String> {
    pairing::ensure_started(&app)?;
    crate::telemetry::capture_event("pairing_started", None);
    pairing::info(&app)
}

#[tauri::command]
fn pairing_stop(app: tauri::AppHandle) {
    pairing::stop_pairing(&app);
    crate::telemetry::capture_event("pairing_stopped", None);
}

#[tauri::command]
fn pairing_restart(app: tauri::AppHandle) -> Result<pairing::PairingInfo, String> {
    pairing::restart(&app)?;
    pairing::info(&app)
}

#[tauri::command]
fn copy_pairing_url(app: tauri::AppHandle) -> Result<(), String> {
    pairing::copy_url(&app)
}

#[tauri::command]
fn copy_qr_image(app: tauri::AppHandle) -> Result<(), String> {
    pairing::copy_qr_image(&app)
}

#[tauri::command]
fn service_start(app: tauri::AppHandle) {
    app.state::<AppState>().sm.start(&app);
}

#[tauri::command]
fn get_app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

// ---------------------------------------------------------------------------
// 应用更新检测（设置页版本区：检查更新 / 忽略 / 前往下载）
// ---------------------------------------------------------------------------

/// 当前更新状态（读本地缓存，不做网络请求）。
#[tauri::command]
fn get_update_info(app: tauri::AppHandle) -> update::UpdateInfo {
    update::info(&app)
}

/// 手动触发一次更新检查（后台线程执行，完成后广播 update-available 事件）。
#[tauri::command]
fn check_for_update(app: tauri::AppHandle) {
    update::trigger_check(&app);
}

/// 用系统浏览器打开 GitHub Releases 下载页。
#[tauri::command]
fn open_download_page() {
    update::open_download();
}

/// 忽略当前最新版本（直到发布更新的版本才重新提示）。
#[tauri::command]
fn dismiss_update(app: tauri::AppHandle) {
    update::dismiss(&app);
}

// ---------------------------------------------------------------------------
// 打赏支持作者（OpenCode Go 邀请链接：订阅双方各得 $5）
// ---------------------------------------------------------------------------

/// OpenCode Go 邀请链接（含作者推荐码，经此链接订阅双方各得 $5 额度）。
const OPENCODE_REF_URL: &str = "https://opencode.ai/go?ref=8CYK5082AG";

/// 用系统浏览器打开 OpenCode Go 邀请链接。
#[tauri::command]
fn open_opencode_ref() {
    crate::open_url(OPENCODE_REF_URL);
}

#[tauri::command]
fn service_restart(app: tauri::AppHandle) {
    app.state::<AppState>().sm.restart(&app);
}

#[tauri::command]
fn service_stop(app: tauri::AppHandle) {
    app.state::<AppState>().sm.stop(&app);
}

#[tauri::command]
fn open_settings_window(app: tauri::AppHandle) {
    tray::open_settings(&app);
}

#[tauri::command]
fn show_main_window(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// 用系统默认方式打开 URL（浏览器）。
pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 重复启动时聚焦主窗口。
            crate::tray::show_main_window(app);
        }))
        .manage(AppState {
            sm: ServiceManager::new(),
            settings: Mutex::new(Settings::default()),
            config_path: Mutex::new(None),
            locale: Mutex::new(Locale::default()),
            tray: Mutex::new(None),
            pairing: Mutex::new(Pairing::new()),
            version_cache: Mutex::new(VersionCache::new()),
        })
        .invoke_handler(tauri::generate_handler![
            query_status,
            has_bundled_runtime,
            get_settings,
            save_settings,
            read_service_log,
            read_pairing_log,
            get_version_info,
            get_locale,
            service_start,
            service_restart,
            service_stop,
            get_app_version,
            get_update_info,
            check_for_update,
            open_download_page,
            dismiss_update,
            open_opencode_ref,
            open_settings_window,
            show_main_window,
            get_pairing_info,
            pairing_regen,
            pairing_start,
            pairing_stop,
            pairing_restart,
            copy_pairing_url,
            copy_qr_image
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // 关窗只隐藏：服务继续运行，随时可从托盘恢复窗口。
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|app| {
            use tauri::Manager;
            let handle = app.handle().clone();

            // Sentry 遥测初始化（含 panic hook，仅首次调用生效）
            let app_ver = app.package_info().version.to_string();
            telemetry::init(&app_ver);

            // 日志轮转：超过 5MB 的 service.log / pairing.log 在启动时滚为 .1（保留两份旧档）
            service::rotate_logs(&handle, 5 * 1024 * 1024);

            // 设置文件：<home>/.dsh/bga-dsh-client/settings.json。
            let dir = app
                .path()
                .home_dir()
                .unwrap_or_else(|_| std::env::temp_dir())
                .join(".dsh")
                .join("bga-dsh-client");
            let cfg_path = dir.join("settings.json");
            let loaded = Settings::load(&cfg_path);
            {
                let state = app.state::<AppState>();
                *state.settings.lock().unwrap() = loaded;
                *state.config_path.lock().unwrap() = Some(cfg_path);
            }

            let tray_menu = tray::build_tray(&handle)?;
            tray::store_menu(&handle, tray_menu);
            tray::refresh_menu_now(&handle);

            // 读取 Harness 配置文件中的语言偏好并启动监听（语言切换时托盘/窗口标题/前端联动）。
            let locale = i18n::resolve();
            *handle.state::<AppState>().locale.lock().unwrap() = locale;
            i18n::store_global(locale);
            tray::apply_locale(&handle);
            i18n::start_watcher(handle.clone());

            service::start_log_tailer(&handle);
            service::auto_boot(&handle);
            service::start_heartbeat(&handle);
            // 应用更新检测：启动 5 秒后后台检查一次（遵守 24h 间隔）。
            update::startup_check(&handle);

            // 版本探测与启动遥测移到后台线程：probe_versions 并行跑 7 个探测、单次最坏 8s
            // （pnpm Corepack shim 冷启动可达数秒），同步执行会阻塞 setup、拖慢主窗口首帧。
            // 先标记「已探测」，避免设置窗口首次打开时 try_probe_async 又重复跑一次完整探测。
            {
                let state = app.state::<AppState>();
                let mut cache = state.version_cache.lock().unwrap();
                cache.last_probe = Some(std::time::Instant::now());
            }
            let has_bundled = service::runtime_root(app.handle()).is_some();
            let service_up = service::ServiceManager::is_up();
            let probe_handle = app.handle().clone();
            std::thread::spawn(move || {
                let vi = probe_versions(&probe_handle);
                // 与 try_probe_async 一致：探测完成后广播，使启动期间已打开的
                // 设置窗口也能刷新（否则它拿到的磁盘缓存可能在本次启动内不再更新）。
                let _ = probe_handle.emit("version-refreshed", vi.clone());
                save_version_cache(&probe_handle, &vi);
                let miss = tr(i18n::current(&probe_handle), "ver.not_installed", &[]);
                telemetry::report_app_started(&telemetry::EnvInfo {
                    app_version: app_ver,
                    has_bundled_runtime: has_bundled,
                    node_version: Some(vi.runtime.node).filter(|v| v.as_str() != miss.as_str()),
                    pnpm_version: Some(vi.runtime.pnpm).filter(|v| v.as_str() != miss.as_str()),
                    dsh_version: Some(vi.runtime.dsh).filter(|v| v.as_str() != miss.as_str()),
                    service_was_up: service_up,
                });
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building DeepSeekHarness")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                if let Some(state) = app_handle.try_state::<AppState>() {
                    // 按设置决定退出时 停止 还是 放生 本应用启动的服务。
                    if state.settings.lock().unwrap().stop_service_on_quit {
                        state.sm.shutdown(app_handle);
                    } else {
                        state.sm.detach();
                    }
                }
            }
        });
}
