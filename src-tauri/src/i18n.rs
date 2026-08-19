//! 多语言支持：语言偏好来自 DeepSeek Harness 配置文件
//! `$DSH_HOME/settings.yaml`（默认 `~/.dsh/settings.yaml`）的 `locale.preference`
//! （`zh` / `en`；缺省时按 Harness 语义回退中文）。
//!
//! 设计要点：
//! - 语言解析不引入 YAML 依赖：settings.yaml 由 js-yaml 生成，`locale:` / `preference:`
//!   是顶层与 2 空格缩进的块标量，用轻量行扫描即可稳定提取（支持 `.yml` / `.json` 兜底）；
//! - 全部用户可见文案集中在 msg() 目录，按 key 取当前语言的译文，`{0}` 占位符做参数替换；
//! - 文件 watcher 轮询配置 mtime，语言变更时刷新托盘菜单、设置窗口标题并广播
//!   `locale-changed` 事件给前端（前端据此重渲染）。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

/// 支持的语言。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Locale {
    #[default]
    Zh,
    En,
}

impl Locale {
    pub fn as_str(self) -> &'static str {
        match self {
            Locale::Zh => "zh",
            Locale::En => "en",
        }
    }

    /// 解析 preference 值：主标签匹配，`zh` 前缀 → 中文，`en` 前缀 → 英文，其余回退中文。
    fn parse(value: &str) -> Locale {
        let tag = value.trim().split(['-', '_']).next().unwrap_or("");
        match tag {
            "en" => Locale::En,
            _ => Locale::Zh,
        }
    }
}

/// Harness 主目录：`$DSH_HOME` 环境变量，缺省 `~/.dsh`。
pub fn dsh_home() -> PathBuf {
    if let Ok(home) = std::env::var("DSH_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home);
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".dsh")
}

/// Harness 设置文档路径：优先 `settings.yaml`，其次 `.yml` / `.json`。
fn settings_path() -> Option<PathBuf> {
    let dir = dsh_home();
    for name in ["settings.yaml", "settings.yml", "settings.json"] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 从 YAML 块标量中提取 `locale:\n  preference: <value>`。
/// 只关心顶层 `locale:` 键下缩进块里的 `preference:` 值，容忍引号与行尾注释。
fn locale_from_yaml(content: &str) -> Option<String> {
    let mut in_locale = false;
    for line in content.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            if let Some(rest) = line.trim_start().strip_prefix("locale:") {
                // 行内形式 `locale: { preference: zh }`
                if rest.contains("preference") {
                    return scalar_of_inline(rest);
                }
                in_locale = true;
            } else {
                in_locale = false;
            }
            continue;
        }
        if in_locale {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("preference:") {
                return Some(unquote(rest));
            }
        }
    }
    None
}

/// 从 `locale: { preference: zh }` 这类行内对象中提取 preference 值。
fn scalar_of_inline(rest: &str) -> Option<String> {
    let after = rest.split("preference:").nth(1)?;
    let value = after.split(['}', ',']).next().unwrap_or("").trim();
    (!value.is_empty()).then(|| unquote(value))
}

/// 去掉标量首尾引号与行尾注释（` # 注释`）；先剥注释再剥引号。
fn unquote(raw: &str) -> String {
    let mut value = raw.trim().to_string();
    if let Some(idx) = value.find('#') {
        value = value[..idx].trim().to_string();
    }
    let trimmed = value.trim();
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

/// 解析 settings.json（`{"locale":{"preference":"en"}}`）。
fn locale_from_json(content: &str) -> Option<String> {
    let root: serde_json::Value = serde_json::from_str(content).ok()?;
    root.get("locale")?
        .get("preference")?
        .as_str()
        .map(String::from)
}

/// 从 Harness 配置文件解析当前语言；文件缺失/解析失败时回退中文。
pub fn resolve() -> Locale {
    let Some(path) = settings_path() else {
        return Locale::default();
    };
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let is_yaml = path.extension().is_some_and(|e| {
        let e = e.to_string_lossy();
        e == "yaml" || e == "yml"
    });
    let value = if is_yaml {
        locale_from_yaml(&content)
    } else {
        locale_from_json(&content)
    };
    value.map(|v| Locale::parse(&v)).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 消息目录：全部用户可见文案的 key → {zh, en}。
// 占位符 `{0}`、`{1}` 由 tr() 按参数顺序替换。
// ---------------------------------------------------------------------------

pub fn msg(locale: Locale, key: &str) -> &str {
    match (locale, key) {
        // 系统托盘
        (_, "tray.settings") => if_en(locale, "Settings", "客户端设置"),
        (_, "tray.show_main") => if_en(locale, "Show Main Window", "显示主界面"),
        (_, "tray.open_browser") => if_en(locale, "Open DSH Service in Browser", "浏览器中打开 dsh 服务页面"),
        (_, "tray.start_service") => if_en(locale, "Start dsh Service", "启动 dsh 服务"),
        (_, "tray.restart_service") => if_en(locale, "Restart dsh Service", "重启 dsh 服务"),
        (_, "tray.stop_service") => if_en(locale, "Stop dsh Service", "停止 dsh 服务"),
        (_, "tray.quit") => if_en(locale, "Quit App", "退出应用"),
        (_, "tray.lan_service") => if_en(locale, "LAN Proxy Service Control", "局域网代理服务控制"),
        (_, "tray.donate") => if_en(locale, "Support the Author", "打赏支持作者"),
        (_, "win.settings_title") => if_en(locale, "Settings - DeepSeekHarness", "客户端设置 - DeepSeekHarness"),
        (_, "pick.dir_title") => if_en(locale, "Select DeepSeekHarness Source Directory", "选择 DeepSeekHarness 源码目录"),

        // 版本信息
        (_, "ver.not_installed") => if_en(locale, "not installed", "未安装"),

        // 服务状态详情（写日志 + 前端 status）：
        (_, "svc.starting_wait") => if_en(locale, "Service is starting, please wait…", "服务正在启动中，请稍候…"),
        (_, "svc.already_running") => if_en(locale, "Service already started and managed by this app", "服务已由本应用启动并在运行中"),
        (_, "svc.orphan_reuse") => if_en(locale, "Taking over the service left from last exit (PID {0}), reusing it directly", "接管上次退出时保留的服务（PID {0}），直接复用"),
        (_, "svc.external_running") => if_en(locale, "Service already running on 127.0.0.1:3080 (started externally); reusing it, this app will not stop it", "检测到 127.0.0.1:3080 已有服务在运行（外部启动），直接复用，本应用不负责停止它"),
        (_, "svc.starting") => if_en(locale, "Starting service…", "正在启动服务…"),
        (_, "svc.builtin_forced") => if_en(locale, "Bundled build: forced to use the built-in Node.js runtime (launch method setting ignored)", "内置版：已强制使用内置 Node.js 拉起服务（设置中的拉起方式已忽略）"),
        (_, "svc.builtin_fallback") => if_en(locale, "No built-in Node.js runtime in this install; falling back to npx", "当前安装不含内置 Node.js 运行时，按设置（内置）回退 npx 拉起"),
        (_, "svc.spawn_failed") => if_en(locale, "Failed to spawn process: {0}", "启动进程失败：{0}"),
        (_, "svc.ready") => if_en(locale, "Service ready", "服务已就绪"),
        (_, "svc.fail_exited") => if_en(locale, ", process exited early", "，子进程提前退出"),
        (_, "svc.fail_timeout") => if_en(locale, " (timeout)", "（超时）"),
        (_, "svc.fail_port_busy") => if_en(
            locale,
            ", port 127.0.0.1:3080 is already in use by another service; stop it first or restart it",
            "，127.0.0.1:3080 端口已被其他服务占用，请先停止占用端口的服务或重启该服务",
        ),
        (_, "svc.fail_detail") => if_en(locale, "Service failed to start{0}, check the log below", "服务启动失败{0}，请查看下方日志"),
        (_, "svc.exited") => if_en(locale, "Service process exited", "服务进程已退出"),
        (_, "svc.stopping") => if_en(locale, "Stopping service…", "正在停止服务…"),
        (_, "svc.stopped") => if_en(locale, "Service stopped", "服务已停止"),
        (_, "svc.external_no_stop") => if_en(locale, "Externally started service detected; this app will not stop it (nor on quit)", "检测到外部启动的服务，本应用不执行停止（退出应用也不会停止它）"),
        (_, "svc.none_running") => if_en(locale, "No service is currently running", "当前没有运行中的服务"),
        (_, "svc.orphan_takeover") => if_en(locale, "Taking over the service left from last exit (PID {0}); can stop/restart from tray", "接管上次退出时保留的服务（PID {0}），可在托盘停止/重启"),
        (_, "svc.external_reuse") => if_en(locale, "Service already running on 127.0.0.1:3080; reusing it (not managed by this app)", "检测到 127.0.0.1:3080 已有服务在运行，直接复用（该服务不由本应用管理）"),

        // 拉起方式描述
        (_, "mth.npx") => "npx --yes @deepseek-ai/dsh web",
        (_, "mth.dsh") => "dsh web",
        (_, "mth.pnpm") => if_en(locale, "pnpm dsh web (dir: {0})", "pnpm dsh web（目录：{0}）"),
        (_, "mth.builtin") => if_en(locale, "Built-in Node.js (bundled dsh, works offline)", "内置 Node.js（应用自带 dsh，离线可用）"),

        // 设置
        (_, "set.unknown_method") => if_en(locale, "Unknown launch method: {0}", "未知的拉起方式：{0}"),
        (_, "set.dir_required") => if_en(locale, "Method 3 requires choosing a directory", "方式三需要选择一个目录"),
        (_, "set.config_dir_missing") => if_en(locale, "Configuration directory is not initialized", "配置目录尚未初始化"),

        // 局域网配对
        (_, "pair.no_ip") => if_en(locale, "No LAN IP found, check network (computer and device must be on the same LAN)", "未找到局域网 IP，请检查网络连接（本机与管理设备需在同一局域网）"),
        (_, "pair.no_ip_log") => if_en(locale, "Startup failed: no LAN IP found (computer and device must be on the same LAN)", "启动失败：未找到局域网 IP（本机与管理设备需在同一局域网）"),
        (_, "pair.port_busy") => if_en(locale, "LAN ports {0}~{1} are all occupied: {2}", "局域网端口（{0}~{1}）均被占用：{2}"),
        (_, "pair.port_busy_log") => if_en(locale, "Startup failed: LAN ports {0}~{1} are all occupied", "启动失败：局域网端口（{0}~{1}）均被占用"),
        (_, "pair.qr_fail") => if_en(locale, "Failed to generate QR code", "QR 生成失败"),
        (_, "pair.start_log") => if_en(locale, "Started LAN proxy service: http://{0}:{1} (one-time code {2})", "启动局域网代理服务：http://{0}:{1}（一次性配对码 {2}）"),
        (_, "pair.stop_log") => if_en(locale, "Stopped LAN proxy service (paired sessions cleared)", "停止局域网代理服务（已清空已配对会话）"),
        (_, "pair.restart_log") => if_en(locale, "Restarted LAN proxy service (code and paired sessions kept)", "重启局域网代理服务（保留配对码与已配对会话）"),
        (_, "pair.pair_ok_log") => if_en(locale, "Device {0} paired by QR (granted 30 minutes, code rotated)", "设备 {0} 扫码配对成功（已放行 30 分钟，配对码已轮换）"),
        (_, "pair.deny_log") => if_en(locale, "Blocked unpaired device {0} (403)", "拒绝未配对设备 {0} 的访问（403）"),
        (_, "pair.regen_log") => if_en(locale, "Regenerated code: {0} (paired sessions cleared)", "重新生成配对码：{0}（已清空已配对会话）"),
        (_, "pair.url_not_ready") => if_en(locale, "Access URL not generated yet, start the LAN proxy service first", "访问链接尚未生成，请先启动局域网代理服务"),
        (_, "pair.qr_not_ready") => if_en(locale, "QR code not generated yet, start the LAN proxy service first", "二维码尚未生成，请先启动局域网代理服务"),
        (_, "pair.qr_gen_fail") => if_en(locale, "Failed to generate QR code", "二维码生成失败"),
        // 手机端错误页
        (_, "pair.denied_title") => if_en(locale, "Access Denied", "访问被拒绝"),
        (_, "pair.denied_body") => if_en(locale, "This device is not paired. Open DeepSeekHarness on your computer, go to the LAN Access window and use the current one-time code to pair (after pairing, this browser stays trusted for 30 minutes; LAN scanning and tunnel access pair per browser, independent of each other).", "该设备尚未配对。请在电脑端打开 DeepSeekHarness 的「局域网访问」窗口，获取当前一次性配对码后访问（配对成功后本浏览器 30 分钟内免确认；局域网扫码与内网穿透隧道均按浏览器分别配对，互不影响）。"),
        (_, "pair.down_title") => if_en(locale, "Service Not Running", "服务未运行"),
        (_, "pair.down_body") => if_en(locale, "The DSH service on your computer is not running or temporarily unreachable. Start it on the computer, then refresh this page.", "桌面端 DSH 服务尚未启动或暂时不可达，请先在电脑上启动服务，再刷新本页。"),
        (_, "pair.gw_title") => if_en(locale, "Gateway Error", "网关错误"),
        (_, "pair.gw_body") => if_en(locale, "Proxy forwarding failed. Refresh and retry; if it persists, check the DSH service status on your computer.", "代理转发失败，请刷新重试；若持续出现，请检查桌面端 DSH 服务状态。"),

        _ => key,
    }
}

#[inline]
fn if_en(locale: Locale, en: &'static str, zh: &'static str) -> &'static str {
    match locale {
        Locale::En => en,
        Locale::Zh => zh,
    }
}

/// 按当前语言渲染 key 对应的文案，`{0}`/`{1}` 按顺序替换。
pub fn tr(locale: Locale, key: &str, args: &[&str]) -> String {
    let template = msg(locale, key);
    let mut out = template.to_string();
    for (i, arg) in args.iter().enumerate() {
        out = out.replace(&format!("{{{i}}}"), arg);
    }
    out
}

// ---------------------------------------------------------------------------
// 当前语言状态 + 文件 watcher
// ---------------------------------------------------------------------------

/// 进程级当前语言（无 AppHandle 的纯函数路径读取，如配对网关的错误页；
/// 由 setup 初始化、start_watcher 更新，与 AppState.locale 保持一致）。
static GLOBAL: Mutex<Option<Locale>> = Mutex::new(None);

/// 读取进程级当前语言（未初始化时回退中文）。
pub fn global() -> Locale {
    GLOBAL.lock().unwrap().unwrap_or_default()
}

/// 读 AppState 中的当前语言。
pub fn current(app: &AppHandle) -> Locale {
    *app.state::<AppState>().locale.lock().unwrap()
}

/// 更新进程级语言（setup 初始化、watcher 变更时调用）。
pub fn store_global(locale: Locale) {
    *GLOBAL.lock().unwrap() = Some(locale);
}

/// 更新 AppState 中的当前语言（watcher 与 setup 共用）。
fn store(app: &AppHandle, locale: Locale) {
    *app.state::<AppState>().locale.lock().unwrap() = locale;
}

/// 配置 mtime（供 watcher 比对；文件不存在返回 None）。
fn settings_mtime() -> Option<SystemTime> {
    settings_path()?.metadata().ok()?.modified().ok()
}

/// 后台轮询 Harness 配置文件：语言变化时刷新托盘/窗口标题并广播 `locale-changed`。
/// 轮询间隔 2s，与心跳线程同风格；文件缺失时持续等待并保持当前语言。
pub fn start_watcher(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last_mtime = settings_mtime();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let mtime = settings_mtime();
            if mtime != last_mtime {
                last_mtime = mtime;
                let resolved = resolve();
                let current = crate::i18n::current(&app);
                if resolved != current {
                    store(&app, resolved);
                    store_global(resolved);
                    crate::tray::apply_locale(&app);
                    let _ = app.emit("locale-changed", resolved.as_str());
                    // 状态详情文案随语言重渲染（下次状态事件自然带新语言）
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yaml_block() {
        let yaml = "locale:\n  preference: en\npet:\n  visible: true\n";
        assert_eq!(locale_from_yaml(yaml).as_deref(), Some("en"));
    }

    #[test]
    fn parses_yaml_zh_with_quotes_and_comment() {
        let yaml = "locale:\n  preference: \"zh\" # 注释\n";
        assert_eq!(locale_from_yaml(yaml).as_deref(), Some("zh"));
    }

    #[test]
    fn parses_yaml_inline_object() {
        let yaml = "locale:\n  preference: zh\n";
        assert_eq!(locale_from_yaml(yaml).as_deref(), Some("zh"));
        let inline = "locale: { preference: 'en' }\n";
        assert_eq!(locale_from_yaml(inline).as_deref(), Some("en"));
    }

    #[test]
    fn ignores_other_top_level_keys() {
        let yaml = "agent-default-model:\n  provider: x\nlocale:\n  preference: en\nui-theme:\n  preference: system\n";
        assert_eq!(locale_from_yaml(yaml).as_deref(), Some("en"));
    }

    #[test]
    fn missing_locale_returns_none() {
        assert_eq!(locale_from_yaml("ui-theme:\n  preference: system\n"), None);
        assert_eq!(locale_from_yaml(""), None);
    }

    #[test]
    fn parses_json_preference() {
        assert_eq!(
            locale_from_json(r#"{"locale":{"preference":"zh"}}"#).as_deref(),
            Some("zh")
        );
        assert_eq!(
            locale_from_json(r#"{"locale":{"preference":"en"}}"#).as_deref(),
            Some("en")
        );
        assert_eq!(locale_from_json("{}"), None);
    }

    #[test]
    fn parses_locale_tags() {
        assert_eq!(Locale::parse("zh"), Locale::Zh);
        assert_eq!(Locale::parse("zh-CN"), Locale::Zh);
        assert_eq!(Locale::parse("zh_Hans"), Locale::Zh);
        assert_eq!(Locale::parse("en"), Locale::En);
        assert_eq!(Locale::parse("en-US"), Locale::En);
        assert_eq!(Locale::parse("fr"), Locale::Zh); // 未知语言回退中文
        assert_eq!(Locale::parse(""), Locale::Zh);
    }

    #[test]
    fn tr_replaces_placeholders() {
        assert_eq!(
            tr(Locale::Zh, "svc.orphan_reuse", &["1234"]),
            "接管上次退出时保留的服务（PID 1234），直接复用"
        );
        assert_eq!(
            tr(Locale::En, "svc.orphan_reuse", &["1234"]),
            "Taking over the service left from last exit (PID 1234), reusing it directly"
        );
        assert_eq!(
            tr(
                Locale::Zh,
                "pair.start_log",
                &["192.168.1.5", "18080", "123456"]
            ),
            "启动局域网代理服务：http://192.168.1.5:18080（一次性配对码 123456）"
        );
    }

    #[test]
    fn unknown_key_falls_back_to_key() {
        assert_eq!(msg(Locale::Zh, "no.such.key"), "no.such.key");
    }

    #[test]
    fn dsh_home_respects_env() {
        std::env::set_var("DSH_HOME", "/tmp/fake-harness-home");
        assert_eq!(dsh_home(), PathBuf::from("/tmp/fake-harness-home"));
        std::env::remove_var("DSH_HOME");
    }
}
