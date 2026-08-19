//! 系统托盘：常驻菜单「客户端设置 / 显示主界面 / 浏览器中打开 dsh 服务页面 / 启动服务 / 重启服务 /
//! 停止服务 / 局域网代理服务控制 / 打赏支持作者 / GitHub Star / 退出应用」。
//! 服务相关菜单项会根据服务状态动态启用/禁用（运行中禁启动、未运行禁停止/重启）。
//! 菜单文案随 Harness 配置的语言（zh/en）动态切换（apply_locale）。

use std::sync::Arc;

use tauri::menu::{IconMenuItem, Menu, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::i18n::{tr, Locale};
use crate::service::{ServiceInfo, ServiceState};
use crate::AppState;

/// 需要随语言/服务状态联动的菜单项句柄（所有文案随语言切换；服务相关菜单启用状态随服务状态）。
pub struct TrayMenu {
    pub settings: IconMenuItem<tauri::Wry>,
    pub lan: IconMenuItem<tauri::Wry>,
    pub donate: IconMenuItem<tauri::Wry>,
    pub show: IconMenuItem<tauri::Wry>,
    pub browser: IconMenuItem<tauri::Wry>,
    pub start: IconMenuItem<tauri::Wry>,
    pub restart: IconMenuItem<tauri::Wry>,
    pub stop: IconMenuItem<tauri::Wry>,
    pub quit: IconMenuItem<tauri::Wry>,
}

impl TrayMenu {
    /// 按当前语言刷新所有菜单项文案（与 apply_locale 共用，供 build 与 watcher 调用）。
    fn set_labels(&self, locale: Locale) {
        let _ = self.settings.set_text(tr(locale, "tray.settings", &[]));
        let _ = self.lan.set_text(tr(locale, "tray.lan_service", &[]));
        let _ = self.donate.set_text(tr(locale, "tray.donate", &[]));
        let _ = self.show.set_text(tr(locale, "tray.show_main", &[]));
        let _ = self.browser.set_text(tr(locale, "tray.open_browser", &[]));
        let _ = self.start.set_text(tr(locale, "tray.start_service", &[]));
        let _ = self
            .restart
            .set_text(tr(locale, "tray.restart_service", &[]));
        let _ = self.stop.set_text(tr(locale, "tray.stop_service", &[]));
        let _ = self.quit.set_text(tr(locale, "tray.quit", &[]));
    }
}

/// 读取内嵌的菜单项图标（36x36 单色 PNG，macOS 在 18pt 显示、Retina 下按 2x 渲染）。
fn menu_icon(name: &str) -> tauri::image::Image<'static> {
    let bytes: &[u8] = match name {
        "settings" => include_bytes!("../icons/menu/settings.png"),
        "show" => include_bytes!("../icons/menu/show.png"),
        "browser" => include_bytes!("../icons/menu/browser.png"),
        "github" => include_bytes!("../icons/menu/github.png"),
        "start" => include_bytes!("../icons/menu/start.png"),
        "restart" => include_bytes!("../icons/menu/restart.png"),
        "stop" => include_bytes!("../icons/menu/stop.png"),
        "lan" => include_bytes!("../icons/menu/lan.png"),
        "donate" => include_bytes!("../icons/menu/donate.png"),
        "quit" => include_bytes!("../icons/menu/quit.png"),
        _ => unreachable!("unknown menu icon: {name}"),
    };
    tauri::image::Image::from_bytes(bytes).expect("bundled menu icon")
}

/// 建立托盘并挂菜单。关闭主窗口/设置窗口永不退出进程，服务照常运行。
pub fn build_tray(app: &AppHandle) -> tauri::Result<TrayMenu> {
    let locale = crate::i18n::current(app);
    let settings = IconMenuItem::with_id(
        app,
        "settings",
        tr(locale, "tray.settings", &[]),
        true,
        Some(menu_icon("settings")),
        None::<&str>,
    )?;
    let show = IconMenuItem::with_id(
        app,
        "show",
        tr(locale, "tray.show_main", &[]),
        true,
        Some(menu_icon("show")),
        None::<&str>,
    )?;
    let browser = IconMenuItem::with_id(
        app,
        "browser",
        tr(locale, "tray.open_browser", &[]),
        true,
        Some(menu_icon("browser")),
        None::<&str>,
    )?;
    let github = IconMenuItem::with_id(
        app,
        "github",
        "GitHub Star",
        true,
        Some(menu_icon("github")),
        None::<&str>,
    )?;
    let start = IconMenuItem::with_id(
        app,
        "start",
        tr(locale, "tray.start_service", &[]),
        true,
        Some(menu_icon("start")),
        None::<&str>,
    )?;
    let restart = IconMenuItem::with_id(
        app,
        "restart",
        tr(locale, "tray.restart_service", &[]),
        true,
        Some(menu_icon("restart")),
        None::<&str>,
    )?;
    let stop = IconMenuItem::with_id(
        app,
        "stop",
        tr(locale, "tray.stop_service", &[]),
        true,
        Some(menu_icon("stop")),
        None::<&str>,
    )?;
    let quit = IconMenuItem::with_id(
        app,
        "quit",
        tr(locale, "tray.quit", &[]),
        true,
        Some(menu_icon("quit")),
        None::<&str>,
    )?;
    // 局域网代理服务控制：点击自动打开设置窗口并切到「局域网代理服务控制」面板。
    let lan = IconMenuItem::with_id(
        app,
        "lan",
        tr(locale, "tray.lan_service", &[]),
        true,
        Some(menu_icon("lan")),
        None::<&str>,
    )?;
    // 打赏支持作者：点击自动打开设置窗口并切到「打赏支持作者」面板。
    let donate = IconMenuItem::with_id(
        app,
        "donate",
        tr(locale, "tray.donate", &[]),
        true,
        Some(menu_icon("donate")),
        None::<&str>,
    )?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let sep3 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[
            &settings, &show, &browser, &sep1, &start, &restart, &stop, &lan, &sep2, &donate,
            &github, &sep3, &quit,
        ],
    )?;

    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/128x128.png"))
        .expect("bundled tray icon");

    TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .menu(&menu)
        .tooltip("DeepSeekHarness")
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "settings" => open_settings(app),
            "lan" => open_settings_panel(app, "lan"),
            "donate" => open_settings_panel(app, "donate"),
            "show" => show_main_window(app),
            "browser" => crate::open_url(&format!("http://127.0.0.1:{}", crate::service::DSH_PORT)),
            "github" => crate::open_url("https://github.com/bingoogolapple/bga-dsh-client"),
            // 托盘操作：立即打开设置窗口并选中「dsh 服务控制」面板，
            // 用户直接看到操作过程状态与日志，无需等待操作结果。
            "start" => {
                open_settings_panel(app, "service");
                app.state::<crate::AppState>().sm.start(app);
            }
            "restart" => {
                open_settings_panel(app, "service");
                app.state::<crate::AppState>().sm.restart(app);
            }
            "stop" => {
                open_settings_panel(app, "service");
                app.state::<crate::AppState>().sm.stop(app);
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    Ok(TrayMenu {
        settings,
        lan,
        donate,
        show,
        browser,
        start,
        restart,
        stop,
        quit,
    })
}

/// 显示并聚焦主窗口（配合单实例重复启动聚焦）。
pub fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        if w.is_minimized().unwrap_or(false) {
            let _ = w.unminimize();
        }
        let _ = w.set_focus();
    }
}

/// 打开（或聚焦）设置窗口（保留当前面板）。
pub fn open_settings(app: &AppHandle) {
    crate::telemetry::capture_event("settings_panel_opened", None);
    open_settings_panel(app, "");
}

/// 打开（或聚焦）设置窗口并切换到指定面板（`panel` 为空串时不切换，保留当前状态）。
/// - 窗口已存在：show + focus 后 eval 调用前端 `window.__openPanel(panel)` 切换；
/// - 窗口不存在：以 `settings.html?panel=<panel>` 创建，前端启动时解析 URL 切换。
pub fn open_settings_panel(app: &AppHandle, panel: &str) {
    let locale = crate::i18n::current(app);
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
        let _ = w.set_title(&tr(locale, "win.settings_title", &[]));
        if !panel.is_empty() {
            // 窗口已加载完成：直接调用前端暴露的面板切换入口。
            // （刚创建尚未加载完成的极短竞态下静默跳过，服务操作下一终态会再次触发。）
            let _ = w.eval(format!(
                "window.__openPanel && window.__openPanel({panel:?})"
            ));
        }
        return;
    }
    let url = if panel.is_empty() {
        "settings.html".to_string()
    } else {
        // 新建窗口：URL 携带目标面板，前端启动时解析并切换。
        format!("settings.html?panel={panel}")
    };
    if let Ok(w) = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App(url.into()))
        .title(tr(locale, "win.settings_title", &[]))
        .inner_size(1080.0, 760.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .build()
    {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// 按当前语言刷新托盘菜单文案与设置窗口标题（语言切换时由 i18n watcher 调用）。
pub fn apply_locale(app: &AppHandle) {
    let locale = crate::i18n::current(app);
    if let Some(menu) = app.state::<AppState>().tray.lock().unwrap().clone() {
        menu.set_labels(locale);
    }
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.set_title(&tr(locale, "win.settings_title", &[]));
    }
}

/// 按服务状态刷新菜单可用性（需在主线程调用）：
/// - 运行中且为本应用启动：启动禁用，停止/重启启用；
/// - 运行中是外部服务（非本应用启动）：启动/停止/重启全部禁用；
/// - 启动中：全部禁用；
/// - 其余（未运行/已停止/失败）：启动启用，停止/重启禁用。
pub fn refresh_menu(app: &AppHandle, info: &ServiceInfo) {
    let menu = app.state::<AppState>().tray.lock().unwrap().clone();
    let Some(menu) = menu else {
        return;
    };
    let running = info.state == ServiceState::Running;
    let starting = info.state == ServiceState::Starting;
    // 停止/重启只对本应用启动的服务开放；外部服务不允许管理。
    let manageable = running && info.mine && !starting;
    let start = !running && !starting;
    let _ = menu.start.set_enabled(start);
    let _ = menu.restart.set_enabled(manageable);
    let _ = menu.stop.set_enabled(manageable);
}

/// 从状态管理器取最新信息后刷新菜单（便于 setup 等无 info 的场景直接调用）。
pub fn refresh_menu_now(app: &AppHandle) {
    let info = app.state::<AppState>().sm.info(app);
    refresh_menu(app, &info);
}

/// 供 service 模块使用：把 TrayMenu 存入 AppState。
pub fn store_menu(app: &AppHandle, menu: TrayMenu) {
    *app.state::<AppState>().tray.lock().unwrap() = Some(Arc::new(menu));
}
