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
    let mut current = app.state::<AppState>().settings.lock().unwrap().clone();
    current.stop_service_on_quit = stop_service_on_quit;
    let path = app
        .state::<AppState>()
        .config_path
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| tr(i18n::current(&app), "set.config_dir_missing", &[]))?;
    current.save(&path)?;
    *app.state::<AppState>().settings.lock().unwrap() = current;
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
/// 重启后服务可能尚未就绪，最多重试 3 次（每次间隔 2 秒），避免立即 fallback
/// 到 npx 缓存中的旧版本。
fn running_dsh_version() -> Option<String> {
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        if let Some(v) = try_running_dsh_version() {
            return Some(v);
        }
    }
    None
}

/// 单次 host.describe 探测。
fn try_running_dsh_version() -> Option<String> {
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
    // 用户显式选定的版本（dsh_version）：下载版/内置版都在 settings 里标记，
    // 它正是实际拉起的服务版本。
    let pinned = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .dsh_version
        .clone();
    // host.describe 查不到版本（旧版服务返回 0.0.1 占位符 / 探测失败）但服务在线时兜底：
    // - 用户显式选定了版本（无论内置还是下载版）→ 优先用该版本，它即实际运行版本；
    //   不能回退到 npx 缓存——npx 缓存是普通版 npx 拉起的包，与用户选定的下载/内置版无关，
    //   否则会显示切换前的旧 npx 版本（左下角 dsh 版本错乱）。
    // - 未选定版本（普通版走 npx）→ 才回退到应用自己的 npx 缓存目录读取。
    let running = running.or_else(|| {
        if service_up {
            if let Some(ref pv) = pinned {
                Some(pv.clone())
            } else {
                npx_cached_dsh_version(app)
            }
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
        try_probe_async(&app, false);
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
    try_probe_async(&app, false);
    placeholder
}

/// 服务重启后强制重新探测版本（绕过30秒冷却期），前端调用后立即刷新左下角版本显示。
#[tauri::command]
fn force_refresh_version_info(app: tauri::AppHandle) -> VersionInfo {
    // 用户显式选定版本（dsh_version）即实际拉起的服务版本——同步给出最可能的运行版本，
    // 避免 host.describe 探测稍慢/失败时，左下角先显示缓存里切换前的旧版本。
    let pinned = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .dsh_version
        .clone();
    let service_up = service::ServiceManager::is_up();
    if let Some(mut cached) = load_version_cache(&app) {
        // 修正 service_up：缓存可能是服务 down 时探测的，现在服务已重启，需实时检测。
        cached.service_up = service_up;
        // 选定版本且服务在线时，直接以该版本作为运行版本（探测兜底用，见 probe_versions）。
        if let Some(ref pv) = pinned {
            if service_up {
                cached.running = Some(pv.clone());
            }
        } else if !service_up {
            cached.running = None;
        }
        try_probe_async(&app, true);
        return relocalize_miss(cached, &app);
    }
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
    try_probe_async(&app, true);
    placeholder
}

/// 若距上次探测超过阈值（或 force=true），spawn 后台线程完整探测（不阻塞命令返回）。
fn try_probe_async(app: &tauri::AppHandle, force: bool) {
    {
        let state = app.state::<AppState>();
        let mut cache = state.version_cache.lock().unwrap();
        if !force {
            if let Some(t) = cache.last_probe {
                if t.elapsed() < PROBE_INTERVAL {
                    return;
                }
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

// ---------------------------------------------------------------------------
// DSH 版本管理（设置页「版本管理」面板）
// ---------------------------------------------------------------------------

/// 语义化版本比较：将 "0.1.10-rc.2" 拆为数字部分逐段比较，pre-release 标签按字典序兜底。
/// "0.1.10" > "0.1.9"（字典序会误判 "1" < "9"）。
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> (Vec<u64>, String) {
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, p.to_string()),
            None => (s, String::new()),
        };
        let nums: Vec<u64> = core.split('.').filter_map(|x| x.parse().ok()).collect();
        (nums, pre)
    };
    let (a_nums, a_pre) = parse(a);
    let (b_nums, b_pre) = parse(b);
    // 逐段比较数字部分
    let max_len = a_nums.len().max(b_nums.len());
    for i in 0..max_len {
        let av = a_nums.get(i).copied().unwrap_or(0);
        let bv = b_nums.get(i).copied().unwrap_or(0);
        match av.cmp(&bv) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    // 数字部分相同：无 pre-release 的版本更大（如 1.0.0 > 1.0.0-rc.1）
    match (a_pre.is_empty(), b_pre.is_empty()) {
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        _ => a_pre.cmp(&b_pre), // 都有或都没有：字典序
    }
}

/// `~/.dsh/bga-dsh-client/dsh-versions/` 目录：存放用户手动下载的各版本 dsh。
/// 与 settings.json 同级（~/.dsh/bga-dsh-client/），便于用户管理和清理。
pub(crate) fn dsh_versions_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    app.path()
        .home_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(".dsh")
        .join("bga-dsh-client")
        .join("dsh-versions")
}

/// 远程版本列表缓存路径（files_dir/dsh-versions-cache.json）。
fn dsh_remote_cache_path(app: &tauri::AppHandle) -> std::path::PathBuf {
    service::files_dir(app).join("dsh-versions-cache.json")
}

/// 版本信息条目（前端渲染用）。
#[derive(serde::Serialize, Clone)]
struct DshVersionEntry {
    version: String,
    local: bool,
    active: bool,
    builtin: bool,
}

/// 读取内置运行时的 dsh 版本（runtime-manifest.json → dshVersion）。
pub(crate) fn builtin_dsh_version(app: &tauri::AppHandle) -> Option<String> {
    let rt = service::runtime_root(app)?;
    let manifest = rt.join("runtime-manifest.json");
    let text = std::fs::read_to_string(manifest).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("dshVersion")?.as_str().map(String::from)
}

/// 读磁盘缓存的远程版本列表。
fn load_remote_versions_cache(app: &tauri::AppHandle) -> Vec<String> {
    let path = dsh_remote_cache_path(app);
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 写磁盘缓存的远程版本列表。
fn save_remote_versions_cache(app: &tauri::AppHandle, versions: &[String]) {
    let path = dsh_remote_cache_path(app);
    if let Ok(text) = serde_json::to_string_pretty(versions) {
        let _ = std::fs::write(path, text);
    }
}

/// 列出所有版本（本地已下载 + 内置 + 远程缓存），合并去重后返回。
#[tauri::command]
fn dsh_list_versions(app: tauri::AppHandle) -> Vec<DshVersionEntry> {
    let versions_dir = dsh_versions_dir(&app);
    let active = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .dsh_version
        .clone();
    let builtin = builtin_dsh_version(&app);

    let mut map: std::collections::HashMap<String, DshVersionEntry> =
        std::collections::HashMap::new();

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
    for rv in load_remote_versions_cache(&app) {
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
            .then_with(|| compare_versions(&b.version, &a.version))
    });
    result
}

/// 后台拉取远程版本列表（npm registry → 缓存 → 事件通知前端）。
#[tauri::command]
fn dsh_refresh_remote_versions(app: tauri::AppHandle) -> Result<(), String> {
    let handle = app.clone();
    std::thread::spawn(move || {
        let result = fetch_remote_versions_blocking(&handle);
        match result {
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
                        "list": dsh_list_versions(handle.clone()),
                        "error": error,
                    }),
                );
            }
        }
    });
    Ok(())
}

/// 远程版本缓存过期时间（1小时）。
const REMOTE_CACHE_TTL_SECS: u64 = 3600;

/// 检查远程版本缓存是否过期，过期则自动后台刷新。
/// 前端在打开版本管理页面时调用，避免用户手动点击「刷新」。
#[tauri::command]
fn dsh_maybe_refresh_remote_versions(app: tauri::AppHandle) -> Result<(), String> {
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
fn fetch_remote_versions_blocking(app: &tauri::AppHandle) -> Result<Vec<DshVersionEntry>, String> {
    let registry = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .npm_registry
        .clone()
        .unwrap_or_else(|| "https://registry.npmjs.org".into());
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
    versions.sort_by(|a, b| b.cmp(a));
    save_remote_versions_cache(app, &versions);
    Ok(dsh_list_versions(app.clone()))
}

/// 后台下载指定版本（spawn 线程执行 npm install）。
#[tauri::command]
fn dsh_download_version(app: tauri::AppHandle, version: String) -> Result<(), String> {
    let versions_dir = dsh_versions_dir(&app);
    let target = versions_dir.join(&version);
    let bin_js = target
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");
    if bin_js.exists() {
        return Err(format!("版本 {} 已下载", version));
    }
    // 目录存在但 bin.js 不存在 → 上次下载失败残留，清理后重新下载
    if target.exists() {
        let _ = std::fs::remove_dir_all(&target);
    }
    std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;

    let handle = app.clone();
    let ver = version.clone();
    std::thread::spawn(move || {
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
    });
    Ok(())
}

/// 执行 npm install @deepseek-ai/dsh@<version>（阻塞，在后台线程调用）。
/// 修复要点：
/// 1. 注入 `npm_config_cache` 到应用缓存目录（绕开用户 ~/.npm 可能的 root 属主/损坏）
/// 2. 读取用户选择的 registry（官方源 / 淘宝镜像）
/// 3. 带 180s 超时 + 管道防死锁，错误消息取 stderr 尾部（非空时）或 stdout 尾部
fn do_download_dsh(
    app: &tauri::AppHandle,
    version: &str,
    target: &std::path::Path,
) -> Result<(), String> {
    let _ = app.emit(
        "dsh-download-progress",
        serde_json::json!({
            "version": version,
            "stage": "installing",
            "message": "",
        }),
    );

    // 读取用户选择的 npm 下载源（默认官方源）
    let registry = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .npm_registry
        .clone()
        .unwrap_or_else(|| "https://registry.npmjs.org".into());

    // 与服务启动一致：注入独立 npm 缓存目录，绕开用户 ~/.npm / 共享缓存的权限损坏。
    // 注意：不能用 files_dir/npm-cache（服务启动用的共享缓存，其 _cacache/tmp 可能有
    // 权限问题导致 EPERM），必须用独立目录确保干净。
    let cache_dir = service::files_dir(app).join("dsh-download-cache");
    let _ = std::fs::create_dir_all(&cache_dir);

    #[cfg(not(windows))]
    let cmd = format!(
        "{path_prefix}npm install --save-exact --registry {registry} @deepseek-ai/dsh@{ver}",
        path_prefix = service::shell_path_prefix(),
        registry = registry,
        ver = version,
    );
    #[cfg(windows)]
    let cmd = format!(
        "npm install --save-exact --registry {registry} @deepseek-ai/dsh@{ver}",
        registry = registry,
        ver = version,
    );

    // 带超时 + 管道防死锁的 subprocess 执行（与 run_capture 同模式）
    use std::io::Read;
    use std::process::{Command, Stdio};
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
    let mut child = Command::new("cmd")
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
fn dsh_delete_version(app: tauri::AppHandle, version: String) -> Result<(), String> {
    let versions_dir = dsh_versions_dir(&app);
    let target = versions_dir.join(&version);
    if !target.exists() {
        return Err(format!("版本 {} 不存在", version));
    }
    // 不允许删除正在使用的版本
    if app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .dsh_version
        .as_deref()
        == Some(&version)
    {
        return Err(format!("版本 {} 正在使用，无法删除", version));
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        let _ = std::fs::remove_dir_all(&target);
        let _ = handle.emit(
            "dsh-versions-refreshed",
            serde_json::json!({
                "ok": true,
                "action": "delete",
                "list": dsh_list_versions(handle.clone()),
            }),
        );
    });
    Ok(())
}

/// 设置用户选定的 DSH 版本（Some 表示指定版本，None 表示恢复默认）。
#[tauri::command]
fn dsh_set_active_version(app: tauri::AppHandle, version: Option<String>) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut settings = state.settings.lock().unwrap();
    settings.dsh_version = version;
    let path = state
        .config_path
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "配置目录不存在".to_string())?;
    settings.save(&path)?;
    Ok(())
}

/// 获取当前选定的 DSH 版本。
#[tauri::command]
fn dsh_active_version(app: tauri::AppHandle) -> Option<String> {
    app.state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .dsh_version
        .clone()
}

/// 保存用户选择的 npm 下载源（官方源 / 淘宝镜像）。
#[tauri::command]
fn dsh_set_registry(app: tauri::AppHandle, registry: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut settings = state.settings.lock().unwrap();
    settings.npm_registry = Some(registry);
    let path = state
        .config_path
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "配置目录不存在".to_string())?;
    settings.save(&path)?;
    Ok(())
}

/// 获取当前选择的 npm 下载源（None 时前端显示为默认官方源）。
#[tauri::command]
fn dsh_get_registry(app: tauri::AppHandle) -> Option<String> {
    app.state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .npm_registry
        .clone()
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
            copy_qr_image,
            dsh_list_versions,
            dsh_refresh_remote_versions,
            dsh_maybe_refresh_remote_versions,
            dsh_download_version,
            dsh_delete_version,
            dsh_set_active_version,
            dsh_active_version,
            dsh_set_registry,
            dsh_get_registry,
            force_refresh_version_info
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
