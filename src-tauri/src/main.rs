//! DeepSeekHarness 桌面壳：DSH Web GUI 的 Tauri 桌面应用。
//!
//! 行为要点：
//! - 启动时探测 127.0.0.1:3080，已有服务则直接复用，否则按设置拉起；
//! - 关闭窗口只是隐藏到托盘，服务不停；
//! - 托盘菜单提供 客户端设置 / 启动服务 / 重启服务 / 停止服务 / 退出应用；
//! - 退出应用时按设置决定 停止 或 放生 本应用启动的服务（放生的服务下次启动自动接管）。
//!
//! # 模块职责
//!
//! 本文件只做**装配**：状态定义、命令注册、生命周期钩子。业务逻辑分属：
//! - `service`：DSH 服务进程生命周期；
//! - `pairing`：局域网扫码配对网关；
//! - `version`：版本信息探测与缓存；
//! - `dsh`：DSH 版本管理与下载；
//! - `update`：应用更新检测；
//! - `settings` / `i18n` / `telemetry` / `tray`；
//! - `state`：AppState 各字段的加锁辅助（中毒安全）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dsh;
mod i18n;
mod pairing;
mod service;
mod settings;
mod state;
mod telemetry;
mod tray;
mod update;
mod version;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::Manager;

use crate::dsh::{
    dsh_active_version, dsh_delete_version, dsh_download_version, dsh_get_registry,
    dsh_list_versions, dsh_maybe_refresh_remote_versions, dsh_refresh_remote_versions,
    dsh_set_active_version, dsh_set_registry, dsh_switch_active_version,
};
use crate::i18n::Locale;
use crate::pairing::Pairing;
use crate::service::ServiceManager;
use crate::settings::Settings;
use crate::tray::TrayMenu;
use crate::version::{force_refresh_version_info, get_version_info, VersionCache};

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
    /// 新版 dsh 的进程启动令牌（从 service.log 的启动行解析；每个 dsh 进程一变）。
    pub dsh_token: Mutex<Option<String>>,
    pub dsh_session_token: Mutex<Option<String>>,
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
    crate::state::settings(&app)
}

#[tauri::command]
fn save_settings(app: tauri::AppHandle, stop_service_on_quit: bool) -> Result<(), String> {
    crate::state::update_settings(&app, |s| s.stop_service_on_quit = stop_service_on_quit)?;
    crate::telemetry::capture_event(
        "settings_saved",
        Some(serde_json::json!({
            "stop_service_on_quit": stop_service_on_quit,
        })),
    );
    Ok(())
}

/// 开关匿名使用统计（Sentry 遥测）。默认关闭（opt-in）。
///
/// 关闭后 `telemetry::enabled()` 恒为 false，所有事件上报立即返回、不发网络请求。
/// 开关状态落盘 settings.json，下次启动保持。
#[tauri::command]
fn set_telemetry_enabled(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    crate::state::update_settings(&app, |s| s.telemetry_enabled = enabled)?;
    telemetry::set_enabled(enabled);
    Ok(())
}

/// 当前匿名使用统计开关状态（设置页渲染开关用）。
#[tauri::command]
fn get_telemetry_enabled(app: tauri::AppHandle) -> bool {
    telemetry::enabled() && crate::state::settings(&app).telemetry_enabled
}

#[tauri::command]
fn read_service_log(app: tauri::AppHandle, limit: Option<usize>) -> Vec<String> {
    service::read_log_tail(&app, limit.unwrap_or(200))
}

#[tauri::command]
fn read_pairing_log(app: tauri::AppHandle) -> Vec<String> {
    pairing::read_log_tail(&app, 200)
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
fn service_restart(app: tauri::AppHandle) {
    app.state::<AppState>().sm.restart(&app);
}

#[tauri::command]
fn service_stop(app: tauri::AppHandle) {
    app.state::<AppState>().sm.stop(&app);
}

/// 主窗口要加载的 DSH 地址（是否带启动令牌）。
///
/// 新版 dsh 的浏览器接口要用进程启动令牌换会话 cookie（详见 `service::launch_url`）。
/// 令牌来自子进程 stdout 那行 `dsh web: …?token=…`，它比「服务已就绪」晚 1~3 秒
/// 才打印，所以本应用刚拉起服务时短等一会儿；外部启动的服务 stdout 不进
/// service.log，等也没用，直接给裸地址（之前换过的 cookie 通常还在有效期内）。
#[tauri::command]
async fn dsh_launch_url(app: tauri::AppHandle) -> String {
    let state = app.state::<AppState>();
    // 外部服务的输出不在本应用日志中：即使磁盘上有上一次服务的
    // token，也不能把它误认为当前服务的 token。旧版 dsh（< 0.1.2-alpha.1）
    // 没有 token，直接使用裸地址即可。
    if !state.sm.info(&app).mine {
        return service::launch_url(None);
    }
    // dsh only started printing a browser launch token in 0.1.2-alpha.1.
    // A pinned 0.1.0/0.1.1 build can never satisfy the wait below, so return
    // its plain URL immediately when switching between legacy versions.
    if crate::state::settings(&app)
        .dsh_version
        .as_deref()
        .is_some_and(service::is_legacy_without_launch_token)
    {
        return service::launch_url(None);
    }
    // 历史日志回填或尾随线程已经抓到令牌时无需等待。
    let cached_token = { crate::state::lock(&state.dsh_token).clone() };
    if let Some(token) = cached_token {
        return service::install_browser_session(&app, &token).await;
    }
    const TOKEN_WAIT: Duration = Duration::from_secs(6);
    let deadline = Instant::now() + TOKEN_WAIT;
    loop {
        let captured_token = { crate::state::lock(&state.dsh_token).clone() };
        if let Some(token) = captured_token {
            return service::install_browser_session(&app, &token).await;
        }
        if !ServiceManager::is_up() || Instant::now() >= deadline {
            return service::launch_url(None);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
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

/// 用系统浏览器打开 OpenCode Go 邀请链接。
/// 链接本身来自编译期常量 `OPENCODE_REF_URL`（见文件末尾），改链接无需改动逻辑。
#[tauri::command]
fn open_opencode_ref() {
    crate::open_url(OPENCODE_REF_URL);
}

#[tauri::command]
fn open_settings_window(app: tauri::AppHandle) {
    tray::open_settings(&app);
}

/// Create a Windows child process without flashing a console window from the
/// GUI subsystem application. Used by all background command probes/actions.
#[cfg(windows)]
pub(crate) fn hidden_command<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = std::process::Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
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
    let _ = hidden_command("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// OpenCode Go 邀请链接（含作者推荐码，经此链接订阅双方各得 $5 额度）。
const OPENCODE_REF_URL: &str = "https://opencode.ai/go?ref=8CYK5082AG";

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
            dsh_token: Mutex::new(None),
            dsh_session_token: Mutex::new(None),
            version_cache: Mutex::new(VersionCache::new()),
        })
        .invoke_handler(tauri::generate_handler![
            query_status,
            has_bundled_runtime,
            get_settings,
            save_settings,
            set_telemetry_enabled,
            get_telemetry_enabled,
            read_service_log,
            read_pairing_log,
            get_version_info,
            get_locale,
            service_start,
            service_restart,
            service_stop,
            dsh_launch_url,
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
            dsh_switch_active_version,
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
            let handle = app.handle().clone();

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
                *crate::state::lock(&state.settings) = loaded.clone();
                *crate::state::lock(&state.config_path) = Some(cfg_path);
            }

            // Sentry 遥测初始化（含 panic hook，仅首次调用生效）。
            // 默认关闭：仅当用户在设置页显式开启（settings.telemetry_enabled）
            // 且构建时注入了 SENTRY_DSN 时才真正启用。
            let app_ver = app.package_info().version.to_string();
            telemetry::init(&app_ver, loaded.telemetry_enabled);

            // 日志轮转：超过 5MB 的 service.log / pairing.log 在启动时滚为 .1（保留两份旧档）
            service::rotate_logs(&handle, 5 * 1024 * 1024);

            let tray_menu = tray::build_tray(&handle)?;
            tray::store_menu(&handle, tray_menu);
            tray::refresh_menu_now(&handle);

            // 读取 Harness 配置文件中的语言偏好并启动监听（语言切换时托盘/窗口标题/前端联动）。
            let locale = i18n::resolve();
            *crate::state::lock(&handle.state::<AppState>().locale) = locale;
            i18n::store_global(locale);
            tray::apply_locale(&handle);
            i18n::start_watcher(handle.clone());

            // 服务可能在应用启动前就已在跑（放生的孤儿 / 外部服务）：那行启动输出
            // 早于本次会话，先从令牌文件（或历史日志）里把令牌捞回来；再把日志里
            // 残留的明文令牌抹掉；最后才启动尾随线程——它会 seek 到文件末尾，
            // 在此之前改动文件大小是安全的。
            service::prime_launch_token(&handle);
            service::scrub_launch_tokens(&handle);
            service::start_log_tailer(&handle);
            service::auto_boot(&handle);
            service::start_heartbeat(&handle);
            // 应用更新检测：启动 5 秒后后台检查一次（遵守 24h 间隔）。
            update::startup_check(&handle);

            // 版本探测与启动遥测移到后台线程：probe_versions 并行跑 7 个探测、单次最坏 8s
            // （pnpm Corepack shim 冷启动可达数秒），同步执行会阻塞 setup、拖慢主窗口首帧。
            // 先标记「已探测」，避免设置窗口首次打开时 try_probe_async 又重复跑一次完整探测。
            version::mark_probed(&handle);
            version::startup_probe(&handle, app_ver, service::ServiceManager::is_up());

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building DeepSeekHarness")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                if let Some(state) = app_handle.try_state::<AppState>() {
                    // 按设置决定退出时 停止 还是 放生 本应用启动的服务。
                    let stop = crate::state::lock(&state.settings).stop_service_on_quit;
                    if stop {
                        state.sm.shutdown(app_handle);
                    } else {
                        state.sm.detach();
                    }
                }
            }
        });
}
