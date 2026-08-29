//! 版本信息探测与缓存（设置页左下角「版本信息」区）。
//!
//! 设计要点：
//! - `get_version_info` / `force_refresh_version_info` **秒回**：有磁盘缓存立即返回，
//!   完整探测放后台线程，结束后 emit `version-refreshed` 由前端刷新；
//! - 完整探测（7 个外部命令 + 1 个 HTTP 探测）在 `std::thread::scope` 内并行跑，
//!   单次最坏 8s（Corepack 的 pnpm shim 冷启动可达数秒），串行会拖到几十秒；
//! - 探测结果落盘 `version-cache.json`，跨启动复用；内存只记「上次探测时间」做节流。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::i18n::{tr, Locale};
use crate::service::DSH_PORT;
use crate::AppState;

/// 一个工具链的三件套版本。
#[derive(Serialize, Deserialize, Clone)]
pub struct ToolVersions {
    pub node: String,
    pub pnpm: String,
    pub dsh: String,
}

/// 前端「版本信息」区的完整数据。
#[derive(Serialize, Deserialize, Clone)]
pub struct VersionInfo {
    /// 内置运行时包（resources/runtime）内的版本。
    pub runtime: ToolVersions,
    /// 系统 PATH 上当前生效的版本。
    pub system: ToolVersions,
    /// 运行中服务（127.0.0.1:DSH_PORT）自报的版本；离线或查询失败为 None。
    pub running: Option<String>,
    /// 3080 端口当前是否有服务在监听（host.describe 查不到版本但服务在线时，
    /// 前端可据此展示「运行中（版本未知）」而非误导性的「未安装」）。
    pub service_up: bool,
}

/// 版本缓存状态：内存里只存「上次完整探测时间」，探测结果落在磁盘
/// version-cache.json，这样设置窗口每次打开都能秒回最近一次结果，
/// 之后由后台线程异步重新探测并 emit 刷新（不阻塞窗口）。
pub struct VersionCache {
    pub last_probe: Option<Instant>,
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

/// 距上次完整探测超过该时长才重新探测（设置窗口高频开关时避免反复跑慢探测）。
const PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// 版本探测超时。放宽到 8s：Corepack 的 pnpm shim 冷启动可达 2.7~4s，
/// 写死 3s 会被误杀成「未安装」。
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// 版本缓存文件路径（files_dir/version-cache.json）。
fn version_cache_path(app: &AppHandle) -> PathBuf {
    crate::service::files_dir(app).join("version-cache.json")
}

/// 读磁盘缓存（首次冷启动 / 文件缺失返回 None）。
fn load_version_cache(app: &AppHandle) -> Option<VersionInfo> {
    let text = std::fs::read_to_string(version_cache_path(app)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 写磁盘缓存。
fn save_version_cache(app: &AppHandle, vi: &VersionInfo) {
    if let Ok(text) = serde_json::to_string_pretty(vi) {
        let _ = std::fs::write(version_cache_path(app), text);
    }
}

/// 重映射缓存里的「未安装」文案到当前语言。
/// 探测时「未安装」按当次语言写死进缓存（version-cache.json 跨启动/跨语言复用），
/// 语言切换或下次以另一语言启动时，读缓存会拿到旧语言文案；这里读时统一归一。
fn relocalize_miss(mut vi: VersionInfo, app: &AppHandle) -> VersionInfo {
    let cur = tr(i18n_current(app), "ver.not_installed", &[]);
    let zh = tr(Locale::Zh, "ver.not_installed", &[]);
    let en = tr(Locale::En, "ver.not_installed", &[]);
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

/// 取当前语言（薄封装，避免本模块到处写 crate::i18n::current）。
fn i18n_current(app: &AppHandle) -> Locale {
    crate::i18n::current(app)
}

/// 无任何缓存时的占位值（"—"表示尚未探测，区别于「未安装」）。
fn placeholder(service_up: bool) -> VersionInfo {
    VersionInfo {
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
        service_up,
    }
}

/// 执行 `prog [args]` 并捕获 stdout 原文（trim 后）；超时或失败返回 None。
/// 不经 shell（Windows 系统命令走 cmd /C，见 sys_version）。
/// `extra_path`：可选的 PATH 覆盖值（仅 Unix 生效），用于在 Dock 启动等短 PATH 场景下定位 node/pnpm/dsh。
///
/// 参数名刻意带下划线前缀：它只在 `#[cfg(not(windows))]` 分支里被消费，Windows 上用不到。
/// 去掉前缀会让 Windows 构建被 clippy 的 `unused_variables` 拦下——CI 跑的是
/// `cargo clippy --all-targets -- -D warnings`，警告即失败。与下方
/// `#[cfg(windows)] fn sys_version(cmd, _extra_path)` 的处理保持一致。
fn run_capture(
    prog: &Path,
    args: &[&OsStr],
    timeout: Duration,
    _extra_path: Option<&str>,
) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

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
        std::thread::sleep(Duration::from_millis(30));
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
#[cfg(not(windows))]
fn sys_version(cmd: &str, extra_path: Option<&str>) -> Option<String> {
    run_capture(
        Path::new(cmd),
        &[OsStr::new("--version")],
        PROBE_TIMEOUT,
        extra_path,
    )
}

#[cfg(windows)]
fn sys_version(cmd: &str, _extra_path: Option<&str>) -> Option<String> {
    run_capture(
        Path::new("cmd"),
        &[OsStr::new("/C"), OsStr::new(cmd), OsStr::new("--version")],
        PROBE_TIMEOUT,
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
            std::thread::sleep(Duration::from_secs(2));
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
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let body = serde_json::json!({
        "type": "client-request",
        "rpcId": "version-probe",
        "method": "host.describe",
        "payload": {},
    });
    let resp: serde_json::Value = client
        .post(format!("http://127.0.0.1:{DSH_PORT}/api/host.describe"))
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
fn npx_cached_dsh_version(app: &AppHandle) -> Option<String> {
    let npx_dir = crate::service::files_dir(app)
        .join("npm-cache")
        .join("_npx");
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
/// 该函数只做探测不写缓存，供后台线程与启动遥测复用。
pub fn probe_versions(app: &AppHandle) -> VersionInfo {
    // 内置运行时入口（node 可执行 + dsh 的 bin.js；pnpm 走 pnpm.cjs，均用内置 node 直跑，跨平台安全）
    let (node_bin, dsh_js, pnpm_cjs) = match crate::service::runtime_root(app) {
        Some(rt) => match crate::service::runtime_entry(&rt) {
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

    // 系统探测：拼入 nvm/pnpm home 等路径（Dock 启动时默认 PATH 极短）
    #[cfg(not(windows))]
    let sys_path = {
        let mut dirs = crate::service::path_dirs();
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
            Some(b) => run_capture(b, &[OsStr::new("--version")], PROBE_TIMEOUT, None),
            None => None,
        });
        let rp = s.spawn(|| match (&node_bin, &pnpm_cjs) {
            (Some(b), Some(p)) => run_capture(
                b,
                &[p.as_os_str(), OsStr::new("--version")],
                PROBE_TIMEOUT,
                None,
            ),
            _ => None,
        });
        let rd = s.spawn(|| match (&node_bin, &dsh_js) {
            (Some(b), Some(d)) => run_capture(
                b,
                &[d.as_os_str(), OsStr::new("--version")],
                PROBE_TIMEOUT,
                None,
            ),
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

    let service_up = crate::service::ServiceManager::is_up();
    // 用户显式选定的版本（dsh_version）：下载版/内置版都在 settings 里标记，
    // 它正是实际拉起的服务版本。
    let pinned = crate::state::settings(app).dsh_version.clone();
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

    let miss = tr(i18n_current(app), "ver.not_installed", &[]).to_string();
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

/// 若距上次探测超过阈值（或 force=true），spawn 后台线程完整探测（不阻塞命令返回）。
fn try_probe_async(app: &AppHandle, force: bool) {
    {
        let state = app.state::<AppState>();
        let mut cache = crate::state::lock(&state.version_cache);
        if !force {
            if let Some(t) = cache.last_probe {
                if t.elapsed() < PROBE_INTERVAL {
                    return;
                }
            }
        }
        cache.last_probe = Some(Instant::now());
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        let vi = probe_versions(&handle);
        let _ = handle.emit("version-refreshed", vi.clone());
        save_version_cache(&handle, &vi);
    });
}

/// 设置页左下角版本信息：**秒回缓存，不阻塞**。
/// - 有磁盘缓存 → 立即返回，后台按节流重新探测；
/// - 无缓存 → 返回占位（"—"），后台探测补上真实值。
#[tauri::command]
pub fn get_version_info(app: AppHandle) -> VersionInfo {
    if let Some(cached) = load_version_cache(&app) {
        try_probe_async(&app, false);
        return relocalize_miss(cached, &app);
    }
    let ph = placeholder(crate::service::ServiceManager::is_up());
    try_probe_async(&app, false);
    ph
}

/// 服务重启后强制重新探测版本（绕过30秒冷却期），前端调用后立即刷新左下角版本显示。
#[tauri::command]
pub fn force_refresh_version_info(app: AppHandle) -> VersionInfo {
    // 用户显式选定版本（dsh_version）即实际拉起的服务版本——同步给出最可能的运行版本，
    // 避免 host.describe 探测稍慢/失败时，左下角先显示缓存里切换前的旧版本。
    let pinned = crate::state::settings(&app).dsh_version.clone();
    let service_up = crate::service::ServiceManager::is_up();

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

    let ph = placeholder(service_up);
    try_probe_async(&app, true);
    ph
}

/// 启动时在后台完整探测一次：写缓存 + emit，并上报启动遥测。
/// 与 try_probe_async 的差别：无条件执行（不节流），且额外上报 app_started。
pub fn startup_probe(app: &AppHandle, app_version: String, service_up: bool) {
    let has_bundled = crate::service::runtime_root(app).is_some();
    let handle = app.clone();
    std::thread::spawn(move || {
        let vi = probe_versions(&handle);
        // 与 try_probe_async 一致：探测完成后广播，使启动期间已打开的
        // 设置窗口也能刷新（否则它拿到的磁盘缓存可能在本次启动内不再更新）。
        let _ = handle.emit("version-refreshed", vi.clone());
        save_version_cache(&handle, &vi);
        let miss = tr(i18n_current(&handle), "ver.not_installed", &[]);
        crate::telemetry::report_app_started(&crate::telemetry::EnvInfo {
            app_version,
            has_bundled_runtime: has_bundled,
            node_version: Some(vi.runtime.node).filter(|v| v.as_str() != miss.as_str()),
            pnpm_version: Some(vi.runtime.pnpm).filter(|v| v.as_str() != miss.as_str()),
            dsh_version: Some(vi.runtime.dsh).filter(|v| v.as_str() != miss.as_str()),
            service_was_up: service_up,
        });
    });
}

/// 标记「本次启动已探测」，避免设置窗口首次打开时 try_probe_async 又重复跑一次完整探测。
pub fn mark_probed(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut cache = crate::state::lock(&state.version_cache);
    cache.last_probe = Some(Instant::now());
}

/// 语义化版本比较：包装 `semver`，与 update.rs 的 `version_gt` 共用同一套解析规则。
///
/// 历史上这里有一份手写实现（`filter_map(parse::<u64>)` 逐段比较），与 update.rs 的
/// `semver::Version::parse` 语义不一致（对含非标准 pre-release 的版本号，前者宽松后者严格）。
/// dsh 版本号形如 `0.1.10-rc.2` 是常态，两份实现并存会导致同一版本号在不同界面排序不同，
/// 故统一收敛到 `semver`：解析失败的版本号排最后（视为最小），保证排序稳定、不 panic。
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    match (
        semver::Version::parse(a.trim_start_matches('v')),
        semver::Version::parse(b.trim_start_matches('v')),
    ) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        // 一边解析失败：可解析的那边更大（失败者排最后）。
        (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
        (Err(_), Ok(_)) => std::cmp::Ordering::Less,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 语义化数值比较：0.1.10 > 0.1.9（字典序会误判 "10" < "9"）。
    #[test]
    fn semver_numeric_not_lexicographic() {
        assert_eq!(
            compare_versions("0.1.10", "0.1.9"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("0.9.0", "0.10.0"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_versions("1.0.0", "1.0.0"),
            std::cmp::Ordering::Equal
        );
    }

    /// pre-release 语义：正式版 > 同名 pre-release；pre-release 之间按 semver 规则
    /// （rc.2 > rc.10 是错的，semver 里 rc.2 < rc.10，与旧手写实现的字典序不同——
    /// 这正是统一实现的原因）。
    #[test]
    fn prerelease_follows_semver_rules() {
        assert_eq!(
            compare_versions("1.0.0", "1.0.0-rc.1"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.0.0-rc.1", "1.0.0"),
            std::cmp::Ordering::Less
        );
        // semver 的 pre-release 数字段按数值比较：rc.10 > rc.2
        assert_eq!(
            compare_versions("0.1.10-rc.10", "0.1.10-rc.2"),
            std::cmp::Ordering::Greater
        );
        // 与 dsh 真实版本号形态一致
        assert_eq!(
            compare_versions("0.1.10-rc.2", "0.1.9"),
            std::cmp::Ordering::Greater
        );
    }

    /// 兼容 v 前缀（GitHub tag 形如 v0.1.0），与 update.rs 的 version_gt 行为一致。
    #[test]
    fn tolerates_v_prefix() {
        assert_eq!(
            compare_versions("v0.1.0", "0.1.0"),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_versions("v0.2.0", "v0.1.0"),
            std::cmp::Ordering::Greater
        );
    }

    /// 非法版本号不 panic，且排序稳定：可解析者大于不可解析者。
    #[test]
    fn invalid_versions_never_panic_and_sort_last() {
        assert_eq!(
            compare_versions("0.1.0", "not-a-version"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("not-a-version", "0.1.0"),
            std::cmp::Ordering::Less
        );
        // 两边都非法：退化为字符串比较，保证全序（sort_by 需要稳定的全序）
        assert_eq!(
            compare_versions("abc", "abd"),
            std::cmp::Ordering::Less,
            "两边都非法时应退化为字符串比较，保证全序"
        );
        // 空串也不 panic
        assert_eq!(compare_versions("", "0.0.1"), std::cmp::Ordering::Less);
    }

    /// 排序与 update.rs 的 version_gt 在**合法版本号**上语义一致。
    ///
    /// 注意两者的失败处理策略有意不同（详见 update::version_gt 注释）：
    /// - `compare_versions` 用于列表排序，必须给出全序，故"解析失败者排最后"；
    /// - `version_gt` 用于更新提示，任一侧非法即保守返回 false（宁可漏报不误报）。
    ///
    /// 本用例只交叉验证两者都合法的情形。
    #[test]
    fn sorting_is_consistent_with_version_gt() {
        let mut versions = vec!["0.1.9", "0.1.10", "0.2.0", "0.1.10-rc.2", "1.0.0"];
        versions.sort_by(|a, b| compare_versions(b, a)); // 倒序
        assert_eq!(
            versions,
            vec!["1.0.0", "0.2.0", "0.1.10", "0.1.10-rc.2", "0.1.9"]
        );
        // 交叉验证：排序中靠前的版本号，version_gt 也应判定"更大"
        for w in versions.windows(2) {
            assert!(
                crate::update::version_gt(w[1], w[0]),
                "排序与 version_gt 冲突: {} vs {}",
                w[0],
                w[1]
            );
        }
    }

    /// 与 update::version_gt 的失败策略差异（有意设计，此处固化防止被"统一"掉）。
    #[test]
    fn failure_policy_differs_from_version_gt_by_design() {
        // 排序：非法版本排最后（保证全序，sort 不会乱序）
        assert_eq!(
            compare_versions("0.1.0", "garbage"),
            std::cmp::Ordering::Greater
        );
        // 更新提示：任一侧非法一律 false（保守，不误报更新）
        assert!(!crate::update::version_gt("garbage", "0.1.0"));
        assert!(!crate::update::version_gt("0.1.0", "garbage"));
        // 两侧都合法时两者一致
        assert!(crate::update::version_gt("0.1.0", "0.2.0"));
        assert_eq!(
            compare_versions("0.2.0", "0.1.0"),
            std::cmp::Ordering::Greater
        );
    }

    /// 版本信息 JSON 往返（磁盘缓存读写的实际形态）。
    #[test]
    fn version_info_roundtrip() {
        let vi = VersionInfo {
            runtime: ToolVersions {
                node: "22.0.0".into(),
                pnpm: "9.0.0".into(),
                dsh: "0.1.10".into(),
            },
            system: ToolVersions {
                node: "20.0.0".into(),
                pnpm: "8.0.0".into(),
                dsh: "未安装".into(),
            },
            running: Some("0.1.10".into()),
            service_up: true,
        };
        let json = serde_json::to_string(&vi).unwrap();
        let back: VersionInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.runtime.node, "22.0.0");
        assert_eq!(back.system.dsh, "未安装");
        assert_eq!(back.running.as_deref(), Some("0.1.10"));
        assert!(back.service_up);
    }

    /// 缺字段的旧缓存：解析失败 → `load_version_cache` 返回 None → 走占位值路径。
    ///
    /// 这是**期望行为**：`VersionInfo` 各字段都是必填（前端按完整结构渲染），
    /// 与其带着半截数据渲染出错，不如整体回退到"—"占位并让后台重新探测。
    #[test]
    fn version_info_missing_fields_falls_back_gracefully() {
        let partial = r#"{"runtime":{"node":"x","pnpm":"y","dsh":"z"}}"#;
        // 缺 system → 反序列化失败（字段必填），这正是我们想要的"整体回退"语义
        assert!(
            serde_json::from_str::<VersionInfo>(partial).is_err(),
            "缺字段的缓存应解析失败，从而触发占位值 + 重新探测"
        );
        // 完整字段可正常解析
        let full = r#"{"runtime":{"node":"a","pnpm":"b","dsh":"c"},"system":{"node":"d","pnpm":"e","dsh":"f"},"running":null,"service_up":false}"#;
        let back: VersionInfo = serde_json::from_str(full).unwrap();
        assert_eq!(back.runtime.node, "a");
        assert_eq!(back.system.dsh, "f");
        assert!(back.running.is_none());
        assert!(!back.service_up);
    }

    /// `running` 字段在旧缓存里可能整个缺失（早期版本可能没有该字段）——
    /// 但当前结构下它是必填的，缺失即整体回退，与上面用例同一语义。
    #[test]
    fn placeholder_has_dash_for_all_versions() {
        let ph = placeholder(true);
        assert_eq!(ph.runtime.node, "—");
        assert_eq!(ph.system.dsh, "—");
        assert!(ph.service_up);
        assert!(ph.running.is_none());
        // 占位值能正常序列化（会被写进缓存再读出）
        let json = serde_json::to_string(&ph).unwrap();
        let back: VersionInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.runtime.node, "—");
    }
}
