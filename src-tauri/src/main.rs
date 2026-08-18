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

use tauri::Manager;

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

/// 应用是否内置 Node.js 运行时（false 时为非内置版，前端隐藏「内置 Node.js」方式）。
#[tauri::command]
fn has_bundled_runtime(app: tauri::AppHandle) -> bool {
    service::runtime_root(&app).is_some()
}

#[tauri::command]
fn get_settings(app: tauri::AppHandle) -> Settings {
    app.state::<AppState>().settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    launch_method: String,
    launch_dir: String,
    stop_service_on_quit: bool,
) -> Result<(), String> {
    let locale = i18n::current(&app);
    let s = Settings::from_parts(&launch_method, &launch_dir, stop_service_on_quit, locale)?;
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
            "launch_method": launch_method,
            "stop_service_on_quit": stop_service_on_quit,
        })),
    );
    Ok(())
}

#[tauri::command]
async fn pick_dir(app: tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog()
        .file()
        .set_title(tr(i18n::current(&app), "pick.dir_title", &[]))
        .pick_folder(move |p| {
            let _ = tx.send(
                p.and_then(|fp| fp.into_path().ok())
                    .and_then(|pb| pb.to_str().map(String::from)),
            );
        });
    // 对话框回调在主线程执行；本命令放到阻塞池等待，绝不占用主线程
    // （同步命令会在主线程 recv，回调永远跑不到 → Finder 打开即卡死）。
    tauri::async_runtime::spawn_blocking(move || {
        rx.recv_timeout(std::time::Duration::from_secs(600))
            .ok()
            .flatten()
    })
    .await
    .ok()
    .flatten()
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

#[derive(serde::Serialize)]
struct ToolVersions {
    node: String,
    pnpm: String,
    dsh: String,
}

#[derive(serde::Serialize)]
struct VersionInfo {
    /// 客户端版本（Cargo.toml / tauri.conf.json 一致）。
    app: String,
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
fn run_capture(prog: &Path, args: &[&std::ffi::OsStr], timeout: std::time::Duration, extra_path: Option<&str>) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;
    let mut builder = Command::new(prog);
    builder.args(args);
    #[cfg(not(windows))]
    if let Some(path) = extra_path {
        builder.env("PATH", path);
    }
    let mut child = builder
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                if !st.success() {
                    return None;
                }
                let mut s = String::new();
                child.stdout.take()?.read_to_string(&mut s).ok()?;
                return Some(s.trim().to_string());
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    return None;
                }
            }
            Err(_) => return None,
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
}

/// 系统 PATH 上某命令的版本（Windows 需经 cmd /C 解析 .cmd shim）。
/// `extra_path`：Unix 下拼入 nvm/pnpm home 等路径，避免 Dock 启动时找不到工具。
#[cfg(not(windows))]
fn sys_version(cmd: &str, extra_path: Option<&str>) -> Option<String> {
    use std::ffi::OsStr;
    run_capture(
        Path::new(cmd),
        &[OsStr::new("--version")],
        std::time::Duration::from_secs(3),
        extra_path,
    )
}

#[cfg(windows)]
fn sys_version(cmd: &str, _extra_path: Option<&str>) -> Option<String> {
    use std::ffi::OsStr;
    run_capture(
        Path::new("cmd"),
        &[OsStr::new("/C"), OsStr::new(cmd), OsStr::new("--version")],
        std::time::Duration::from_secs(3),
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
        .post(format!("http://127.0.0.1:{}/api/host.describe", service::DSH_PORT))
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

#[tauri::command]
fn get_version_info(app: tauri::AppHandle) -> VersionInfo {
    use std::ffi::OsStr;
    let app_ver = app.package_info().version.to_string();
    // 内置运行时入口（node 可执行 + dsh 的 bin.js；pnpm 走 pnpm.cjs，均用内置 node 直跑，跨平台安全）
    let (node_bin, dsh_js, pnpm_cjs) = match service::runtime_root(&app) {
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
    let timeout = std::time::Duration::from_secs(3);
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
        let rn = s.spawn(|| {
            match &node_bin {
                Some(b) => run_capture(b, &[OsStr::new("--version")], timeout, None),
                None => None,
            }
        });
        let rp = s.spawn(|| match (&node_bin, &pnpm_cjs) {
            (Some(b), Some(p)) => run_capture(b, &[p.as_os_str(), OsStr::new("--version")], timeout, None),
            _ => None,
        });
        let rd = s.spawn(|| match (&node_bin, &dsh_js) {
            (Some(b), Some(d)) => run_capture(b, &[d.as_os_str(), OsStr::new("--version")], timeout, None),
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
            npx_cached_dsh_version(&app)
        } else {
            None
        }
    });
    let miss = tr(i18n::current(&app), "ver.not_installed", &[]).to_string();
    VersionInfo {
        app: app_ver,
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
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            sm: ServiceManager::new(),
            settings: Mutex::new(Settings::default()),
            config_path: Mutex::new(None),
            locale: Mutex::new(Locale::default()),
            tray: Mutex::new(None),
            pairing: Mutex::new(Pairing::new()),
        })
        .invoke_handler(tauri::generate_handler![
            query_status,
            has_bundled_runtime,
            get_settings,
            save_settings,
            pick_dir,
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

            // 上报增强版启动事件（含环境探测结果）
            let has_bundled = service::runtime_root(app.handle()).is_some();
            let service_up = service::ServiceManager::is_up();
            let vi = get_version_info(app.handle().clone());
            let miss = tr(i18n::current(app.handle()), "ver.not_installed", &[]);
            telemetry::report_app_started(&telemetry::EnvInfo {
                app_version: app_ver,
                has_bundled_runtime: has_bundled,
                node_version: Some(vi.runtime.node).filter(|v| v.as_str() != miss.as_str()),
                pnpm_version: Some(vi.runtime.pnpm).filter(|v| v.as_str() != miss.as_str()),
                dsh_version: Some(vi.runtime.dsh).filter(|v| v.as_str() != miss.as_str()),
                service_was_up: service_up,
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
