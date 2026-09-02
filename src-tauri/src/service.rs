//! DSH 服务生命周期管理：
//! - 探测 127.0.0.1:3080 是否已有服务（外部服务直接复用，不做任何停止操作）；
//! - 按自动判定的方式启动子进程（普通版 npx / 内置版内置 Node.js）；子进程 stdout/stderr
//!   追加写入日志文件，由 tailer 线程轮询转发给前端（放生后服务仍可安全运行，
//!   不受管道 SIGPIPE 影响）；
//! - 用 `service.pid` 留存本应用启动过的服务 PID，跨启动可继续「接管」管理；
//! - 停止/重启按进程组整棵结束；退出应用时按设置决定 停止 或 放生（detach）。

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::i18n::tr;
use crate::AppState;

pub const DSH_PORT: u16 = 3080;
/// npx 首次运行需要下载包，给足时间。
const START_TIMEOUT: Duration = Duration::from_secs(180);

/// 内置运行时在 Resources 中的目录名（与 scripts/bundle-runtime.mjs 的输出一致）。
const RUNTIME_DIR: &str = "runtime";

/// 拉起方式（完全自动，不来自用户配置）：
/// - 普通版（无内置 runtime）→ `Npx`
/// - 内置版（应用自带 runtime）→ `Builtin`
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LaunchMethod {
    Npx,
    Builtin,
}

impl LaunchMethod {
    /// 用于展示给用户的命令描述。
    fn display(self, locale: crate::i18n::Locale) -> String {
        match self {
            Self::Npx => "npx --yes @deepseek-ai/dsh web".into(),
            Self::Builtin => tr(locale, "mth.builtin", &[]),
        }
    }
}

/// 内置 Node.js 运行时根目录：<Resources>/runtime。
/// 非内置版打包没有该目录，返回 None（此时自动走 npx 拉起）。
pub fn runtime_root(handle: &AppHandle) -> Option<PathBuf> {
    let dir = handle.path().resource_dir().ok()?.join(RUNTIME_DIR);
    dir.is_dir().then_some(dir)
}

/// 内置运行时的 Node 可执行文件与 dsh 入口。
/// 布局：nd/bin/node（darwin/linux）或 nd/node.exe（win）；rt/node_modules/@deepseek-ai/dsh/lib/bin.js
pub(crate) fn runtime_entry(runtime: &Path) -> Option<(PathBuf, PathBuf)> {
    let node = if cfg!(windows) {
        runtime.join("nd").join("node.exe")
    } else {
        runtime.join("nd").join("bin").join("node")
    };
    let bin_js = runtime
        .join("rt")
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");
    (node.is_file() && bin_js.is_file()).then_some((node, bin_js))
}

/// 服务状态（序列化为小写字符串，直接提供给前端）。
#[derive(Serialize, Clone, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    #[default]
    None,
    Starting,
    Running,
    Stopped,
    Error,
}

/// 通过 `service-status` 事件与 `query_status` 命令下发给前端的状态。
#[derive(Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub struct ServiceInfo {
    pub state: ServiceState,
    /// 是否由本应用启动（含上次退出放生、本次接管的服务；外部服务只复用、不管理）。
    pub mine: bool,
    pub pid: Option<u32>,
    /// 当前设置对应的拉起命令描述。
    pub method: String,
    pub detail: String,
}

pub struct ServiceManager {
    /// 本应用启动、正在被本进程管理的子进程。
    child: Mutex<Option<Child>>,
    /// 上次退出时放生、本次启动接管的孤儿服务 PID（无 Child 句柄，只能按组杀）。
    orphan: Mutex<Option<u32>>,
    starting: AtomicBool,
    /// 本应用最近一次启动失败标记（向前端暴露 error 状态，从而展示失败日志）。
    failed: AtomicBool,
    detail: Mutex<String>,
    /// 服务生命周期操作互斥锁：start/stop/restart 整体串行化，
    /// 避免快速连续操作并发交错（二次 start 覆盖 child 泄漏进程等竞态）。
    lifecycle: Mutex<()>,
}

impl ServiceManager {
    pub fn new() -> Self {
        Self {
            child: Mutex::new(None),
            orphan: Mutex::new(None),
            starting: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            detail: Mutex::new(String::new()),
            lifecycle: Mutex::new(()),
        }
    }

    /// 探测 127.0.0.1:3080 上是否有监听。
    pub fn is_up() -> bool {
        std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], DSH_PORT)),
            Duration::from_millis(400),
        )
        .is_ok()
    }

    /// 当前服务信息（供命令与事件共用）。
    pub fn info(&self, handle: &AppHandle) -> ServiceInfo {
        let starting = self.starting.load(Ordering::SeqCst);
        // 用 state::lock：锁中毒时恢复数据继续运行，避免因后台线程 panic 导致
        // 用户点击任何菜单都崩溃（详见 state 模块文档）。
        let child = crate::state::lock(&self.child);
        let orphan = *crate::state::lock(&self.orphan);
        let mine = starting || child.is_some() || orphan.is_some();
        let state = if starting {
            ServiceState::Starting
        } else if child.is_some() || Self::is_up() {
            ServiceState::Running
        } else if self.failed.load(Ordering::SeqCst) {
            ServiceState::Error
        } else {
            ServiceState::None
        };
        // 拉起方式展示：用户选定版本 > 自动判定（内置/npx）。
        let pinned = crate::state::settings(handle).dsh_version.clone();
        let method = if let Some(ref ver) = pinned {
            ver.clone()
        } else if runtime_root(handle).is_some() {
            LaunchMethod::Builtin.display(crate::i18n::current(handle))
        } else {
            LaunchMethod::Npx.display(crate::i18n::current(handle))
        };
        ServiceInfo {
            state,
            mine,
            pid: child.as_ref().map(|c| c.id()).or(orphan),
            method,
            detail: crate::state::lock(&self.detail).clone(),
        }
    }

    fn set_detail(&self, handle: &AppHandle, text: String) {
        use std::io::Write;
        let line = format!("{} {text}", now_ts());
        // 同 push_log：只落盘，实时广播由日志尾随线程统一发出。
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(files_dir(handle).join("service.log"))
        {
            let _ = writeln!(f, "{line}");
        }
        *crate::state::lock(&self.detail) = text;
    }

    fn push_log(&self, handle: &AppHandle, line: String) {
        use std::io::Write;
        let line = format!("{} {line}", now_ts());
        // 只落盘，不 emit：实时广播统一由 start_log_tailer 读文件后发出——
        // 若在此也 emit，tailer 又会读到刚写回的行再广播（重复行），
        // 更不能在 tailer 里 push_log（写回自己读到的行→时间戳前缀雪球叠加的死循环）。
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(files_dir(handle).join("service.log"))
        {
            let _ = writeln!(f, "{line}");
        }
    }

    fn emit_status(&self, handle: &AppHandle) {
        let info = self.info(handle);
        let _ = handle.emit("service-status", &info);
        // 状态变更同步刷新托盘菜单可用性（切到主线程改，避免 macOS 菜单线程问题）。
        let h = handle.clone();
        let info_for_menu = info;
        let _ = handle.run_on_main_thread(move || {
            crate::tray::refresh_menu(&h, &info_for_menu);
        });
    }

    /// 终态收尾：写详情、广播状态。
    fn finish(&self, handle: &AppHandle, detail: String) {
        self.set_detail(handle, detail);
        self.emit_status(handle);
    }

    /// 启动服务（异步）。先做快速检查，真正的工作在线程里做。
    pub fn start(&self, handle: &AppHandle) {
        let h = handle.clone();
        std::thread::spawn(move || {
            let state = h.state::<AppState>();
            // 生命周期操作串行化：防止快速连续 start/stop/restart 并发交错。
            let _guard = crate::state::lock(&state.sm.lifecycle);
            state.sm.start_inner(&h);
        });
    }

    fn start_inner(&self, handle: &AppHandle) {
        if self.starting.load(Ordering::SeqCst) {
            self.finish(
                handle,
                tr(crate::i18n::current(handle), "svc.starting_wait", &[]),
            );
            return;
        }
        if crate::state::lock(&self.child).is_some() {
            self.finish(
                handle,
                tr(crate::i18n::current(handle), "svc.already_running", &[]),
            );
            return;
        }
        // 上次退出放生的服务仍在运行：接管，继续管理。
        if let Some(pid) = *crate::state::lock(&self.orphan) {
            if process_alive(pid) {
                self.finish(
                    handle,
                    tr(
                        crate::i18n::current(handle),
                        "svc.orphan_reuse",
                        &[&pid.to_string()],
                    ),
                );
                return;
            }
            *crate::state::lock(&self.orphan) = None;
            self.clear_pid(handle);
        }
        if Self::is_up() {
            self.finish(
                handle,
                tr(crate::i18n::current(handle), "svc.external_running", &[]),
            );
            return;
        }

        // 令牌每个 dsh 进程一变：先作废上一次的，否则 `dsh_launch_url` 会命中
        // 缓存、把**已死进程**的令牌拼进 URL（只剩旧 cookie 能兜底，兜不住就 401）。
        // 清空内存和磁盘旧值；该命令会等到本次启动的新令牌出现，超时才
        // 退回裸地址，从而兼容没有 token 的旧版 dsh。
        forget_launch_token(handle);

        self.starting.store(true, Ordering::SeqCst);
        self.failed.store(false, Ordering::SeqCst);
        self.set_detail(
            handle,
            tr(crate::i18n::current(handle), "svc.starting", &[]),
        );
        self.emit_status(handle);
        let start_time = Instant::now();

        let runtime = runtime_root(handle);

        // 用户选定版本：直接用 ~/.dsh/dsh-versions/<ver>/node_modules/@deepseek-ai/dsh/lib/bin.js
        let pinned = crate::state::settings(handle).dsh_version.clone();
        let (shell_cmd, cwd, launch_method_display) = if let Some(ref ver) = pinned {
            let bin_js = crate::dsh::dsh_versions_dir(handle)
                .join(ver)
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib")
                .join("bin.js");
            // 也检查 builtin 路径
            let bin_js = if bin_js.exists() {
                bin_js
            } else if let Some(rt) = runtime.as_deref() {
                let (_, builtin_bin_js) = runtime_entry(rt).unwrap_or_default();
                // 只有当 builtin 版本匹配时才用
                if builtin_bin_js.exists()
                    && crate::dsh::builtin_dsh_version(handle).as_deref() == Some(ver.as_str())
                {
                    builtin_bin_js
                } else {
                    bin_js // 不存在，后续 spawn_shell 会报错
                }
            } else {
                bin_js
            };
            let node = runtime.as_deref().and_then(runtime_entry).map(|(n, _)| n);
            let node_str = node
                .map(|n| n.display().to_string())
                .unwrap_or_else(|| "node".into());
            #[cfg(not(windows))]
            let cmd = {
                let path_prefix = shell_path_prefix();
                format!(
                    "{path_prefix}exec \"{node}\" \"{bin_js}\" web --no-open",
                    path_prefix = path_prefix,
                    node = node_str,
                    bin_js = bin_js.display()
                )
            };
            #[cfg(windows)]
            let cmd = format!(
                "\"{node}\" \"{bin_js}\" web --no-open",
                node = node_str,
                bin_js = bin_js.display()
            );
            let display = ver.to_string();
            (cmd, None, display)
        } else {
            // 拉起方式完全自动：内置版（应用自带 runtime）用内置 Node.js，普通版用 npx。
            let launch_method = if runtime.is_some() {
                LaunchMethod::Builtin
            } else {
                LaunchMethod::Npx
            };
            // 先确保 npx 独立缓存目录存在（绕过用户 ~/.npm 的权限/损坏问题）。
            let npm_cache = files_dir(handle).join("npm-cache");
            let _ = std::fs::create_dir_all(&npm_cache);
            let (cmd, cwd) = build_command(launch_method, runtime.as_deref(), Some(&npm_cache));
            let display = launch_method.display(crate::i18n::current(handle));
            (cmd, cwd, display)
        };
        let log_path = files_dir(handle).join("service.log");

        let child = match spawn_shell(&shell_cmd, cwd.as_deref(), &log_path) {
            Ok(c) => c,
            Err(e) => {
                self.starting.store(false, Ordering::SeqCst);
                self.failed.store(true, Ordering::SeqCst);
                self.finish(
                    handle,
                    tr(
                        crate::i18n::current(handle),
                        "svc.spawn_failed",
                        &[&e.to_string()],
                    ),
                );
                return;
            }
        };
        let pid = child.id();
        self.push_log(handle, format!("$ {shell_cmd}"));

        *crate::state::lock(&self.child) = Some(child);

        // 等待端口就绪（或进程退出 / 超时）。
        let deadline = Instant::now() + START_TIMEOUT;
        let mut exited = false;
        loop {
            {
                let mut guard = crate::state::lock(&self.child);
                if let Some(c) = guard.as_mut() {
                    match c.try_wait() {
                        Ok(Some(_)) => {
                            exited = true;
                            break;
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            }
            if Self::is_up() {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        if Self::is_up() {
            let elapsed_ms = start_time.elapsed().as_millis() as u64;
            self.starting.store(false, Ordering::SeqCst);
            self.write_pid(handle, pid);
            self.finish(handle, tr(crate::i18n::current(handle), "svc.ready", &[]));
            self.spawn_exit_watcher(handle, pid);
            crate::telemetry::capture_event(
                "service_started",
                Some(serde_json::json!({
                    "method": launch_method_display,
                    "startup_ms": elapsed_ms,
                })),
            );
        } else {
            self.starting.store(false, Ordering::SeqCst);
            self.failed.store(true, Ordering::SeqCst);
            if let Some(mut c) = crate::state::lock(&self.child).take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            self.clear_pid(handle);
            let locale = crate::i18n::current(handle);
            // 进程提前退出但端口仍被占用：大概率是 EADDRINUSE（残留/外部服务占着 3080），
            // 给出明确提示而不是笼统的「提前退出」，避免用户误以为只是启动失败。
            let reason = if exited && Self::is_up() {
                tr(locale, "svc.fail_port_busy", &[])
            } else if exited {
                tr(locale, "svc.fail_exited", &[])
            } else {
                tr(locale, "svc.fail_timeout", &[])
            };
            let detail = tr(locale, "svc.fail_detail", &[&reason]);
            self.finish(handle, detail.clone());
            crate::telemetry::capture_error(&detail, Some("service_start_failed"));
        }
    }

    /// 在后台观察子进程：若是本应用启动的且已退出，把状态重置为已停止并清理 pid 记录。
    /// 注意：npx 壳进程退出 ≠ dsh 服务退出——npx 拉起 dsh 后自身退出、dsh 成孤儿继续跑。
    /// 因此子进程退出时先查端口：仍在线则视为「放生孤儿」，转为接管对象（保留可停止/重启），
    /// 而非误判为外部服务。
    fn spawn_exit_watcher(&self, handle: &AppHandle, my_pid: u32) {
        let h = handle.clone();
        std::thread::spawn(move || loop {
            {
                let state = h.state::<AppState>();
                let sm = &state.sm;
                let mut guard = crate::state::lock(&sm.child);
                if guard.as_ref().map(|c| c.id()) != Some(my_pid) {
                    return; // 已被 stop() 接管清理
                }
                match guard.as_mut().unwrap().try_wait() {
                    Ok(Some(_)) => {
                        guard.take();
                        if Self::is_up() {
                            // dsh 服务仍在线：npx 壳退出了，服务变成孤儿。用端口反查真实
                            // 服务 PID 记入 orphan，本次会话内仍可停止/重启，下次启动走接管分支。
                            if let Some(real) = port_listener_pid() {
                                *crate::state::lock(&sm.orphan) = Some(real);
                                sm.write_pid(&h, real);
                                sm.set_detail(
                                    &h,
                                    tr(
                                        crate::i18n::current(&h),
                                        "svc.orphan_release",
                                        &[&real.to_string()],
                                    ),
                                );
                                sm.emit_status(&h);
                                return;
                            }
                        }
                        sm.clear_pid(&h);
                        sm.set_detail(&h, tr(crate::i18n::current(&h), "svc.exited", &[]));
                        sm.emit_status(&h);
                        return;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        guard.take();
                        sm.clear_pid(&h);
                        return;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        });
    }

    /// 停止服务（异步）：仅停止本应用启动的服务（含接管的孤儿）；外部服务不操作。
    pub fn stop(&self, handle: &AppHandle) {
        let h = handle.clone();
        std::thread::spawn(move || {
            let state = h.state::<AppState>();
            let _guard = crate::state::lock(&state.sm.lifecycle);
            state.sm.stop_inner(&h);
        });
    }

    fn stop_inner(&self, handle: &AppHandle) {
        // 进程令牌随服务下线作废：新进程会打印新令牌，旧的留着没有意义。
        forget_launch_token(handle);
        self.starting.store(false, Ordering::SeqCst);
        self.failed.store(false, Ordering::SeqCst);
        let mine_child = crate::state::lock(&self.child).take();
        let mine_orphan = crate::state::lock(&self.orphan).take();
        if let Some(mut child) = mine_child {
            self.set_detail(
                handle,
                tr(crate::i18n::current(handle), "svc.stopping", &[]),
            );
            self.emit_status(handle);
            kill_tree(&mut child);
            self.wait_port_free();
            self.clear_pid(handle);
            self.finish(handle, tr(crate::i18n::current(handle), "svc.stopped", &[]));
            crate::telemetry::capture_event("service_stopped", None);
        } else if let Some(pid) = mine_orphan {
            self.set_detail(
                handle,
                tr(crate::i18n::current(handle), "svc.stopping", &[]),
            );
            self.emit_status(handle);
            kill_group(pid);
            self.wait_port_free();
            self.clear_pid(handle);
            self.finish(handle, tr(crate::i18n::current(handle), "svc.stopped", &[]));
            crate::telemetry::capture_event("service_stopped", None);
        } else if Self::is_up() {
            self.finish(
                handle,
                tr(crate::i18n::current(handle), "svc.external_no_stop", &[]),
            );
        } else {
            self.finish(
                handle,
                tr(crate::i18n::current(handle), "svc.none_running", &[]),
            );
        }
    }

    /// 停止后等待端口释放，避免紧接着的重启抢占失败。
    fn wait_port_free(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Self::is_up() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// 重启服务（停止本应用启动的进程后再启动）。
    pub fn restart(&self, handle: &AppHandle) {
        let h = handle.clone();
        // The stop→start sequence can finish before the main window's
        // polling interval observes a non-running state. Notify it explicitly
        // so a token-less legacy dsh URL is reloaded after version switching.
        let _ = handle.emit("service-restarting", ());
        std::thread::spawn(move || {
            let state = h.state::<AppState>();
            // 同一把锁内串行 stop→start，避免中间态被并发操作打断。
            let _guard = crate::state::lock(&state.sm.lifecycle);
            state.sm.stop_inner(&h);
            state.sm.start_inner(&h);
        });
    }

    /// 退出应用且设置「停止服务」：杀掉本应用启动的所有进程（含孤儿），清理 pid 记录。
    pub fn shutdown(&self, handle: &AppHandle) {
        self.starting.store(false, Ordering::SeqCst);
        self.failed.store(false, Ordering::SeqCst);
        if let Some(mut child) = crate::state::lock(&self.child).take() {
            kill_tree(&mut child);
        }
        if let Some(pid) = crate::state::lock(&self.orphan).take() {
            kill_group(pid);
        }
        self.clear_pid(handle);
    }

    /// 退出应用且设置「不停止服务」：放弃子进程句柄但保留 pid 记录，服务继续运行，
    /// 下次启动自动接管。子进程输出已指向日志文件，不会因本进程退出而 SIGPIPE。
    pub fn detach(&self) {
        self.starting.store(false, Ordering::SeqCst);
        self.failed.store(false, Ordering::SeqCst);
        let _ = crate::state::lock(&self.child).take();
    }

    // ---- pid 记录（决定服务是否属于本应用、能否停止/接管） ----

    fn pid_path(&self, handle: &AppHandle) -> PathBuf {
        files_dir(handle).join("service.pid")
    }

    fn write_pid(&self, handle: &AppHandle, pid: u32) {
        let _ = std::fs::write(self.pid_path(handle), pid.to_string());
    }

    fn clear_pid(&self, handle: &AppHandle) {
        let _ = std::fs::remove_file(self.pid_path(handle));
    }

    fn read_pid(&self, handle: &AppHandle) -> Option<u32> {
        std::fs::read_to_string(self.pid_path(handle))
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }
}

/// 应用启动时的自动探测 + 自动拉起/接管。
pub fn auto_boot(handle: &AppHandle) {
    let h = handle.clone();
    std::thread::spawn(move || {
        // 稍等片刻，让主窗口的监听器先挂上。
        std::thread::sleep(Duration::from_millis(400));
        let state = h.state::<AppState>();
        let sm = &state.sm;
        let detection;
        if ServiceManager::is_up() {
            match sm.read_pid(&h).filter(|&pid| process_alive(pid)) {
                Some(pid) => {
                    *crate::state::lock(&sm.orphan) = Some(pid);
                    sm.set_detail(
                        &h,
                        tr(
                            crate::i18n::current(&h),
                            "svc.orphan_takeover",
                            &[&pid.to_string()],
                        ),
                    );
                    detection = "orphan_takeover";
                }
                None => {
                    sm.set_detail(&h, tr(crate::i18n::current(&h), "svc.external_reuse", &[]));
                    detection = "external_reuse";
                }
            }
        } else {
            detection = "fresh_start";
            sm.start(&h);
        }
        crate::telemetry::capture_event(
            "runtime_detection_result",
            Some(serde_json::json!({ "detection": detection })),
        );
        sm.emit_status(&h);
    });
}

/// 心跳：外部服务上下线时也能及时通知前端。
pub fn start_heartbeat(handle: &AppHandle) {
    let h = handle.clone();
    std::thread::spawn(move || {
        let mut last_up: Option<bool> = None;
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let state = h.state::<AppState>();
            let sm = &state.sm;
            if crate::state::lock(&sm.child).is_some() {
                continue; // 本应用启动的进程由 exit watcher 管理
            }
            let up = ServiceManager::is_up();
            // 放生/接管的孤儿服务已退出（进程死或端口已释放）时清理孤儿 PID 记录，
            // 避免前端一直显示「本应用管理 + 陈旧 PID」，也避免下次启动被误判为接管。
            let orphan_gone = match *crate::state::lock(&sm.orphan) {
                Some(pid) => !up || !process_alive(pid),
                None => false,
            };
            if orphan_gone {
                *crate::state::lock(&sm.orphan) = None;
                sm.clear_pid(&h);
            }
            if last_up != Some(up) {
                last_up = Some(up);
                // 服务下线：进程令牌随之作废（新进程会打印新令牌），不留残余凭据。
                if !up {
                    forget_launch_token(&h);
                }
                sm.emit_status(&h);
            }
        }
    });
}

/// 日志文件尾随线程：服务输出追加到 <app_config_dir>/service.log，此线程每 300ms
/// 读取新增行并广播给前端（替代原先的管道方案，放生后服务不会因 SIGPIPE 死掉）。
pub fn start_log_tailer(handle: &AppHandle) {
    let h = handle.clone();
    std::thread::spawn(move || {
        let path = files_dir(&h).join("service.log");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut file = match OpenOptions::new()
            .read(true)
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(_) => return,
        };
        // 只读本次会话新增内容，跳过历史日志。
        if let Ok(len) = file.metadata().map(|m| m.len()) {
            let _ = file.seek(SeekFrom::Start(len));
        }
        let mut carry = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match file.read(&mut buf) {
                // 令牌脱敏会重写整个日志（变短）：把偏移拉回来，别停在旧位置上。
                Ok(0) => resync_log_offset(&mut file),
                Ok(n) => {
                    let mut chunk = std::mem::take(&mut carry);
                    chunk.extend_from_slice(&buf[..n]);
                    let mut start = 0;
                    for i in 0..chunk.len() {
                        if chunk[i] == b'\n' {
                            let line = String::from_utf8_lossy(&chunk[start..i])
                                .trim_end_matches('\r')
                                .to_string();
                            if !line.is_empty() {
                                // 新版 dsh 的启动行藏着进程令牌：留档（另存到
                                // 0600 的令牌文件），再把日志里那行抹掉，最后打码广播。
                                if let Some(token) = parse_launch_token(&line) {
                                    store_launch_token(&h, token);
                                    scrub_launch_tokens(&h);
                                    resync_log_offset(&mut file);
                                }
                                // 唯一广播出口：读到的任何新行（子进程输出 / 应用 push_log 行）
                                // 都从这里 emit，绝不回写文件（回写会形成读→写→读反馈环）。
                                let _ = h.emit(
                                    "service-log",
                                    &serde_json::json!({ "line": redact_token(&line) }),
                                );
                            }
                            start = i + 1;
                        }
                    }
                    carry = chunk[start..].to_vec();
                }
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    });
}

/// 本地时间戳 `[YYYY-MM-DD HH:MM:SS]` 前缀，供日志行写入与实时事件统一使用。
///
/// 用 `time` crate 的 `OffsetDateTime::now_local()`，替代原先手写的
/// `libc::localtime_r` / `localtime_s` 双分支 unsafe 实现：
/// - 去掉 2 处 unsafe（OS 本地时区换算交给成熟库）；
/// - 抹平 unix / windows 的 API 差异（`localtime_r` vs `localtime_s`），单份实现跨平台；
/// - 拿不到本地时区（极少数环境）时回退 UTC，绝不 panic——日志时间戳缺失不影响主流程。
pub(crate) fn now_ts() -> String {
    use time::OffsetDateTime;
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let (h, m, s) = now.to_hms();
    let (y, mo, d) = (now.year(), u8::from(now.month()), now.day());
    format!("[{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}]")
}

/// 日志滚动：应用启动时检查，超过 `max` 字节的文件轮转为 `.log.1`（原 `.1` 顺延为 `.2`，
/// 最多保留两份旧档）；service.log 与 pairing.log 共用。无定时任务，时点=启动时。
pub(crate) fn rotate_logs(app: &AppHandle, max: u64) {
    for name in ["service.log", "pairing.log"] {
        let dir = files_dir(app);
        let path = dir.join(name);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() < max {
            continue;
        }
        let _ = std::fs::remove_file(dir.join(format!("{name}.2")));
        let _ = std::fs::rename(dir.join(format!("{name}.1")), dir.join(format!("{name}.2")));
        let _ = std::fs::rename(&path, dir.join(format!("{name}.1")));
    }
}

/// 服务日志/pid 文件目录（service.log / pairing.log / pid 文件所在）。
/// 注意与 settings.json 目录（~/.dsh/bga-dsh-client/）不同；
/// 这里走系统标准 app_config_dir（macOS: ~/Library/Application Support/cn.bingoogolapple.dsh/，
/// Windows: %APPDATA%\Roaming\cn.bingoogolapple.dsh\，Linux: ~/.config/cn.bingoogolapple.dsh/）。
pub(crate) fn files_dir(handle: &AppHandle) -> PathBuf {
    handle
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
}

/// 读取日志文件尾部最多 `limit` 行（供失败页兜底展示历史日志，不依赖实时事件）。
/// 只 seek 到文件尾部读最多 512KB——大日志（子进程 stdout 可能数 MB~数 GB）
/// 不会整文件读入，避免打开设置页卡顿。
pub fn read_log_tail(handle: &AppHandle, limit: usize) -> Vec<String> {
    read_tail(&files_dir(handle).join("service.log"), limit)
        .into_iter()
        // 失败态会把这批行原样显示在窗口里，令牌不能跟着出去。
        .map(|line| redact_token(&line))
        .collect()
}

/// 通用尾部读取：max 512KB，去首行残片，按行取尾部 `limit` 行。
pub(crate) fn read_tail(path: &Path, limit: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL_BYTES: u64 = 512 * 1024;
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if f.take(TAIL_BYTES).read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let mut text = String::from_utf8_lossy(&buf);
    // 被截断时去掉首行残片（半截行不展示）
    if start > 0 {
        if let Some(pos) = text.find('\n') {
            let rest = text[pos + 1..].to_string();
            text = rest.into();
        }
    }
    if limit == 0 {
        return Vec::new();
    }
    text.lines()
        .rev()
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(String::from)
        .collect()
}

// ---------------------------------------------------------------------------
// 启动令牌：新版 dsh 的浏览器接口用它换会话 cookie
// ---------------------------------------------------------------------------
//
// dsh 0.1.2 起，Web 接口的 index 与 /api 都要求浏览器会话：启动时打印一行
// `dsh web: http://127.0.0.1:3080/?token=<43 字符>`，首次访问用该令牌换一枚
// 绑定 authority 的签名 cookie（默认 30 天有效，跨 dsh 重启仍有效，但令牌
// 本身每次进程启动都会变）——详见 dsh 的
// `packages/client/connection/src/browser-auth.ts`。
//
// 本模块只做三件事：从日志里认出令牌、把它存起来、以及在任何可能外泄的地方
// 打码（日志窗口 / 遥测 / 剪贴板）。是否拼接由 `launch_url` 决定。

/// 启动行的固定前缀（`printUrl` 默认开启，web profile 的 patch 里写死为 true）。
const LAUNCH_LINE_PREFIX: &str = "dsh web: ";
/// 令牌是 32 字节 base64url（43 字符）；低于此长度的一律视为无关内容。
const TOKEN_MIN_LEN: usize = 32;

/// 从一行日志里提取启动令牌；不是启动行或格式不符时返回 `None`。
///
/// 只认启动行的第一个 URL：`(LAN: …)` 那半行带的是同一个令牌，不该重复解析。
pub(crate) fn parse_launch_token(line: &str) -> Option<String> {
    let rest = line.trim_end().strip_prefix(LAUNCH_LINE_PREFIX)?;
    let url = rest.split_whitespace().next()?;
    let (_, value) = url.split_once("?token=")?;
    let token: String = value
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (token.len() >= TOKEN_MIN_LEN).then_some(token)
}

/// 把行内所有 `token=<令牌>` 打码。短值（非令牌）保持原样，避免误伤日志内容。
pub(crate) fn redact_token(line: &str) -> String {
    const NEEDLE: &str = "token=";
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(NEEDLE) {
        let (head, tail) = rest.split_at(at + NEEDLE.len());
        out.push_str(head);
        // 令牌只含 ASCII，字符数即字节数。
        let len = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .count();
        if len == 0 {
            // 形如 `token=` 的空值：原样收尾，否则下面的 split_at 会死循环。
            out.push_str(tail);
            return out;
        }
        let (value, tail) = tail.split_at(len);
        if len >= TOKEN_MIN_LEN {
            out.push_str("***");
        } else {
            out.push_str(value);
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// 令牌的落盘位置。
///
/// 日志里那行会被打码（进程凭据不该以明文长期留在磁盘上），所以令牌另存于此：
/// 应用重启后 dsh 可能还是同一个进程，令牌依然有效，没了它网关就换不了会话。
/// 权限仅当前用户（与 dsh 自己的 `.credentials.yaml` 同级保护）。
fn launch_token_path(app: &AppHandle) -> PathBuf {
    files_dir(app).join("launch-token")
}

/// 记住最新令牌（每次 dsh 进程启动都会变，直接覆盖即可）。
pub(crate) fn store_launch_token(app: &AppHandle, token: String) {
    persist_launch_token(app, &token);
    *crate::state::lock(&app.state::<AppState>().dsh_token) = Some(token);
}

/// 写入令牌文件；Unix 下收紧到 0600。
fn persist_launch_token(app: &AppHandle, token: &str) {
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(launch_token_path(app)) {
        use std::io::Write;
        let _ = f.write_all(token.as_bytes());
    }
}

/// 读回上次记录的令牌（应用重启后 dsh 若未重启，它仍然有效）。
fn read_persisted_token(app: &AppHandle) -> Option<String> {
    let raw = std::fs::read_to_string(launch_token_path(app)).ok()?;
    let token = raw.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// 从历史日志回填令牌：尾随线程只处理本次会话的新增行，而服务可能是上次
/// 放生的孤儿、或在应用启动前就已经在跑——那行启动输出早就被跳过了。
///
/// 优先读令牌文件；没有（旧版本遗留的日志）才去扫日志。
pub(crate) fn prime_launch_token(app: &AppHandle) {
    let token = read_persisted_token(app).or_else(|| {
        // 扫描整个尾部窗口（512KB）而不是最后几行：服务可能已经跑了很久，那行
        // 启动输出早被后续的会话日志刷出了小窗口，漏掉它网关就一直拿不到令牌。
        read_tail(&files_dir(app).join("service.log"), usize::MAX)
            .iter()
            .rev()
            .find_map(|line| parse_launch_token(line))
    });
    if let Some(token) = token {
        store_launch_token(app, token);
    }
}

/// 把日志里已经落盘的令牌打成星号。
///
/// 令牌是进程凭据，明文摊在日志里不合适；它本身已另存在 `launch-token`（0600），
/// 所以打码不影响使用。**原地重写**而不是 rename：子进程以追加模式持有该文件
/// 的句柄，换 inode 会让它继续写旧文件。尾随线程会把读偏移拉回来（见
/// `resync_log_offset`），所以在它运行期间调用也安全。
pub(crate) fn scrub_launch_tokens(app: &AppHandle) {
    let path = files_dir(app).join("service.log");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    if !text.contains("token=") {
        return;
    }
    let scrubbed = redact_token(&text);
    if scrubbed == text {
        return;
    }
    let _ = std::fs::write(&path, scrubbed.as_bytes());
}

/// 服务已下线：进程令牌随之作废（下一个进程会打印新令牌），内存与磁盘都不留残余。
pub(crate) fn forget_launch_token(app: &AppHandle) {
    *crate::state::lock(&app.state::<AppState>().dsh_token) = None;
    let _ = std::fs::remove_file(launch_token_path(app));
}

/// 文件被重写变短后把读偏移拉回有效范围：否则偏移停在旧位置上，之后追加的
/// 日志永远读不到（`read` 一直返回 0）。
fn resync_log_offset(file: &mut std::fs::File) {
    use std::io::Seek;
    let Ok(pos) = file.stream_position() else {
        return;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return;
    };
    if pos > len {
        let _ = file.seek(SeekFrom::Start(len));
    }
}

/// 浏览器要打开的地址：有令牌就带上，dsh 会 303 回干净 `/` 并下发 cookie，
/// 令牌随即从地址栏消失；没有令牌（旧版 dsh、外部服务、日志已轮转）就用裸
/// 地址——此前换过的 cookie 仍在有效期内时一样能进。
pub(crate) fn launch_url(token: Option<&str>) -> String {
    // The Vite dev window uses localhost and dsh's dev flow expects the
    // numeric loopback authority. Production uses tauri.localhost, where the
    // hostname form is required for the SameSite=Strict auth cookie.
    let host = if cfg!(debug_assertions) {
        "127.0.0.1"
    } else {
        "localhost"
    };
    let base = format!("http://{host}:{DSH_PORT}");
    match token {
        Some(t) if !t.is_empty() => format!("{base}/?token={t}"),
        _ => base,
    }
}

/// Whether a pinned dsh version predates launch-token support.
pub(crate) fn is_legacy_without_launch_token(version: &str) -> bool {
    version.starts_with("0.1.0") || version.starts_with("0.1.1")
}

/// 从 Dock 启动的应用 PATH 往往只有系统目录，npx/dsh/pnpm 都找不到。
/// 这里枚举常见的 Node/包管理器 bin 目录，拼成显式 PATH 前缀。
#[cfg(not(windows))]
pub(crate) fn path_dirs() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = Vec::new();
    // nvm：~/.nvm/versions/node/<v*>/bin（可能有多个版本，全部纳入）
    if let Ok(rd) = std::fs::read_dir(PathBuf::from(&home).join(".nvm/versions/node")) {
        for e in rd.flatten() {
            let bin = e.path().join("bin");
            if bin.is_dir() {
                dirs.push(bin);
            }
        }
    }
    // volta / fnm / mise / asdf / npm 全局 / brew / 用户本地
    for p in [
        ".volta/bin",
        ".fnm",
        ".local/share/mise/shims",
        ".asdf/shims",
        ".npm-global/bin",
        ".local/bin",
    ] {
        dirs.push(PathBuf::from(&home).join(p));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    // 系统工具（sh/sed/dirname/uname…）：显式兜底 + 继承父进程 PATH。
    // 继承的 PATH 必须按 ':' 拆分成段再过滤，整体作为一个路径会被 is_dir 过滤掉，
    // 导致 /usr/bin、/bin 丢失（npx 里 npm 会 spawn sh，pnpm 脚本要用 sed 等）。
    dirs.push(PathBuf::from("/usr/bin"));
    dirs.push(PathBuf::from("/bin"));
    dirs.push(PathBuf::from("/usr/sbin"));
    dirs.push(PathBuf::from("/sbin"));
    if let Ok(cur) = std::env::var("PATH") {
        for seg in cur.split(':') {
            if !seg.is_empty() {
                dirs.push(PathBuf::from(seg));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    dirs.into_iter()
        .filter(|d| d.is_dir() && seen.insert(d.to_string_lossy().into_owned()))
        .collect()
}

/// 组装显式 PATH 前缀；`extra` 为内置运行时目录（捆绑 node bin + dsh/pnpm 的 .bin）。
#[cfg(not(windows))]
pub(crate) fn shell_path_prefix() -> String {
    let entries = path_dirs()
        .into_iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    format!("export PATH=\"{entries}\" 2>/dev/null; ")
}

/// 按自动判定的方式构造 shell 命令（返回命令与工作目录，后者恒为 None）。
/// - `Npx`：`npx --yes @deepseek-ai/dsh web --no-open`
///   （普通版，依赖系统 node/npx；不带版本号 → npx 解析 npm `latest` 标签，
///   metadata 有缓存秒回；新版本发布后首次启动才会下载，之后复用 npm 缓存秒启。
///   绝不加 `--prefer-online`——那会强制每次重新下载整个依赖树，重启卡死）
/// - `Builtin`：用内置 Node.js 直跑 dsh（内置版，离线可用）
///
/// 统一追加 `--no-open`：DSH 服务已内嵌到本客户端 WebView，禁止 dsh 启动时
/// 再自动打开系统浏览器。
///
/// `runtime` 为内置 Node.js 运行时根目录（None 表示非内置版或目录缺失）；
/// `npm_cache` 为 npx 使用的独立缓存目录——npx 不再读写用户 ~/.npm，
/// 绕开历史遗留的 root 属主/损坏缓存导致的 EACCES/EEXIST 问题。
fn build_command(
    method: LaunchMethod,
    runtime: Option<&Path>,
    npm_cache: Option<&Path>,
) -> (String, Option<String>) {
    #[cfg(windows)]
    {
        // Windows 走 cmd：用 set 语法注入 npm 缓存目录（值含分隔符与可能的空格，带引号）
        let npm_prefix = match npm_cache {
            Some(p) => format!("set \"npm_config_cache={}\" && ", p.display()),
            None => String::new(),
        };
        let npx_dsh = "npx --yes @deepseek-ai/dsh web --no-open";
        match method {
            LaunchMethod::Npx => (format!("{npm_prefix}{npx_dsh}"), None),
            LaunchMethod::Builtin => match runtime.and_then(runtime_entry) {
                Some((node, bin_js)) => (
                    format!(
                        "\"{}\" \"{}\" web --no-open",
                        node.display(),
                        bin_js.display()
                    ),
                    None,
                ),
                // 内置版打包缺失运行时（异常）：回退 npx，让日志暴露原因。
                None => (format!("{npm_prefix}{npx_dsh}"), None),
            },
        }
    }
    #[cfg(not(windows))]
    {
        let path_prefix = shell_path_prefix();
        // 注入独立 npm 缓存：npx 不碰用户 ~/.npm，避免旧版 npm 遗留的权限损坏问题
        let npm_prefix = match npm_cache {
            Some(p) => format!(
                "export npm_config_cache={} 2>/dev/null; ",
                shell_quote(p.to_string_lossy().as_ref())
            ),
            None => String::new(),
        };
        let npx_dsh = "exec npx --yes @deepseek-ai/dsh web --no-open";
        match method {
            LaunchMethod::Npx => (format!("{path_prefix}{npm_prefix}{npx_dsh}"), None),
            LaunchMethod::Builtin => match runtime.and_then(runtime_entry) {
                Some((node, bin_js)) => {
                    // 内置 PATH：捆绑 node bin + dsh-runtime/.bin 置前，dsh plugin 的 pnpm / npx 能找到
                    let extra = [
                        node.parent().unwrap_or(Path::new("")).to_path_buf(),
                        bin_js
                            .parent()
                            .and_then(|p| p.parent())
                            .and_then(|p| p.parent())
                            .and_then(|p| p.parent())
                            .unwrap_or_else(|| Path::new(""))
                            .join(".bin"),
                    ];
                    let mut dirs: Vec<PathBuf> = extra.into_iter().collect();
                    dirs.extend(path_dirs());
                    let mut seen = std::collections::HashSet::new();
                    let entries = dirs
                        .into_iter()
                        .filter(|d| d.is_dir() && seen.insert(d.to_string_lossy().into_owned()))
                        .map(|d| d.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(":");
                    (
                        format!(
                            "export PATH=\"{entries}\" 2>/dev/null; exec \"{}\" \"{}\" web --no-open",
                            node.display(),
                            bin_js.display()
                        ),
                        None,
                    )
                }
                // 内置版打包缺失运行时（异常）：回退 npx，让日志暴露原因。
                None => (format!("{path_prefix}{npm_prefix}{npx_dsh}"), None),
            },
        }
    }
}

#[cfg(not(windows))]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 启动 shell 子进程；stdout/stderr 追加写入日志文件（放生安全）。
#[cfg(unix)]
fn spawn_shell(cmd: &str, cwd: Option<&str>, log_path: &std::path::Path) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt;
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let err = out.try_clone()?;
    let mut c = Command::new("sh");
    c.arg("-lc").arg(cmd);
    c.stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    // 独立进程组，便于整树停止。
    c.process_group(0);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    c.spawn()
}

#[cfg(windows)]
fn spawn_shell(cmd: &str, cwd: Option<&str>, log_path: &std::path::Path) -> std::io::Result<Child> {
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let err = out.try_clone()?;
    let mut c = Command::new("cmd");
    c.arg("/C").arg(cmd);
    c.stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    c.spawn()
}

/// 判断 PID 对应的进程是否存活。
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let r = unsafe { libc::kill(pid as i32, 0) };
    if r == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}")])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
        Err(_) => false,
    }
}

/// 结束目标进程：优先按进程组整体结束（SIGTERM → 宽限 → SIGKILL）；
/// 若该进程不是进程组组长（npx 拉起的 dsh 孤儿属于 shell 的组，`kill(-pid)` 会
/// 因无此进程组而报 ESRCH），则退化为单进程 SIGTERM → SIGKILL，确保能停掉。
#[cfg(unix)]
fn _kill_group_or_single(pid: u32, group: bool) {
    let p = pid as i32;
    let target = if group { -p } else { p };
    unsafe {
        let _ = libc::kill(target, libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(1500));
    unsafe {
        let _ = libc::kill(target, libc::SIGKILL);
    }
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    // 先尝试进程组：若 ESRCH（无此进程组 / 不是组长），进程组信号无效。
    unsafe {
        if libc::kill(-(pid as i32), 0) == 0 {
            _kill_group_or_single(pid, true);
            return;
        }
    }
    _kill_group_or_single(pid, false);
}

/// 查 127.0.0.1:DSH_PORT 上监听进程的真实 PID（npx 壳退出后 dsh 成为孤儿，
/// 用端口反查才能拿到真正的服务进程，供接管/停止使用）。
#[cfg(unix)]
fn port_listener_pid() -> Option<u32> {
    let out = Command::new("lsof")
        .args(["-nP", "-iTCP:3080", "-sTCP:LISTEN", "-t"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next()?.trim().parse::<u32>().ok()
}

#[cfg(windows)]
fn port_listener_pid() -> Option<u32> {
    let out = Command::new("netstat").args(["-ano"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // 行形如 "  TCP   127.0.0.1:3080   0.0.0.0:0   LISTENING   12345"
    text.lines()
        .find(|l| l.contains(":3080") && l.contains("LISTENING"))
        .and_then(|l| l.split_whitespace().last()?.parse::<u32>().ok())
}

#[cfg(windows)]
fn kill_group(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status();
}

/// 结束子进程整棵树并回收。
fn kill_tree(child: &mut Child) {
    kill_group(child.id());
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 普通版（Npx）：命令包含 npx @deepseek-ai/dsh web --no-open（不自动弹浏览器），
    /// 且注入独立 npm 缓存目录。
    #[test]
    fn build_command_npx_injects_cache() {
        let (cmd, cwd) = build_command(LaunchMethod::Npx, None, Some(Path::new("/tmp/npm-cache")));
        assert!(cmd.contains("npx --yes @deepseek-ai/dsh web --no-open"));
        // 严禁 --prefer-online：会强制每次重新下载整个依赖树，导致重启卡死
        assert!(!cmd.contains("prefer-online"), "cmd: {cmd}");
        assert!(cmd.contains("npm_config_cache"));
        assert!(cmd.contains("/tmp/npm-cache"));
        assert_eq!(cwd, None);
    }

    /// 内置版（Builtin）+ runtime 存在：直接使用捆绑 node 直跑 dsh，而非 npx。
    #[test]
    fn build_command_builtin_uses_runtime_entry() {
        // 构造一个模拟的 runtime 布局：nd/bin/node（类 unix）或 nd/node.exe（win）
        // + rt/node_modules/@deepseek-ai/dsh/lib/bin.js
        let fake = std::env::temp_dir().join("dsh-fake-runtime-test");
        let node = if cfg!(windows) {
            fake.join("nd").join("node.exe")
        } else {
            fake.join("nd").join("bin").join("node")
        };
        let bin_js = fake
            .join("rt")
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js");
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::fs::create_dir_all(bin_js.parent().unwrap()).unwrap();
        std::fs::write(&node, "").unwrap();
        std::fs::write(&bin_js, "").unwrap();

        let (cmd, cwd) = build_command(LaunchMethod::Builtin, Some(&fake), None);
        assert!(cmd.contains(node.to_str().unwrap()), "cmd: {cmd}");
        assert!(cmd.contains(bin_js.to_str().unwrap()), "cmd: {cmd}");
        assert!(cmd.contains("web --no-open"), "cmd: {cmd}");
        // 不再走 npx
        assert!(!cmd.contains("npx"), "cmd: {cmd}");
        assert_eq!(cwd, None);
        let _ = std::fs::remove_dir_all(&fake);
    }

    /// 内置版 + runtime 缺失（异常/开发目录）：安全回退 npx，让日志暴露原因。
    #[test]
    fn build_command_builtin_falls_back_to_npx() {
        let (cmd, _cwd) = build_command(
            LaunchMethod::Builtin,
            None,
            Some(Path::new("/tmp/npm-cache")),
        );
        assert!(
            cmd.contains("npx --yes @deepseek-ai/dsh web --no-open"),
            "cmd: {cmd}"
        );
    }

    /// 启动行的令牌能被认出来；LAN 那半行带的是同一个令牌，只按第一个 URL 解析。
    #[test]
    fn parse_launch_token_reads_first_url() {
        let token = "62lso3kIv99w0wrfKhnuuOxArUpgoSCI9WwuIw62xuY";
        let line = format!("dsh web: http://127.0.0.1:3080/?token={token}");
        assert_eq!(parse_launch_token(&line).as_deref(), Some(token));
        // 带 LAN 地址时不把括号里的内容也吞进来。
        let with_lan =
            format!("dsh web: http://127.0.0.1:3080/?token={token} (LAN: http://10.0.0.2:3080/?token={token})");
        assert_eq!(parse_launch_token(&with_lan).as_deref(), Some(token));
    }

    /// 非启动行（含旧版 dsh 的裸地址输出）不产生令牌。
    #[test]
    fn parse_launch_token_ignores_other_lines() {
        assert_eq!(parse_launch_token("dsh web: http://127.0.0.1:3080/"), None);
        assert_eq!(parse_launch_token("[2026-09-01 17:17:39] 服务已就绪"), None);
        assert_eq!(parse_launch_token("web-app: listening on 3080"), None);
    }

    /// 令牌长度不足（被截断/畸形）时宁可不要，也不要拿半个令牌去拼 URL。
    #[test]
    fn parse_launch_token_rejects_short_value() {
        let line = "dsh web: http://127.0.0.1:3080/?token=short";
        assert_eq!(parse_launch_token(line), None);
    }

    /// 打码：令牌变成 `***`，其余内容一字不动。
    #[test]
    fn redact_token_masks_only_the_token() {
        let token = "62lso3kIv99w0wrfKhnuuOxArUpgoSCI9WwuIw62xuY";
        let line = format!("dsh web: http://127.0.0.1:3080/?token={token} (LAN: http://10.0.0.2:3080/?token={token})");
        let redacted = redact_token(&line);
        assert!(!redacted.contains(token), "令牌必须被打码: {redacted}");
        assert_eq!(
            redacted,
            "dsh web: http://127.0.0.1:3080/?token=*** (LAN: http://10.0.0.2:3080/?token=***)"
        );
        // 无关行原样返回。
        assert_eq!(redact_token("服务已就绪"), "服务已就绪");
    }

    /// `token=` 后面没有值（畸形/被截断的行）不能让打码逻辑死循环。
    #[test]
    fn redact_token_terminates_on_empty_value() {
        assert_eq!(
            redact_token("dsh web: http://x/?token="),
            "dsh web: http://x/?token="
        );
        assert_eq!(redact_token("token="), "token=");
    }

    /// 有令牌拼带令牌的地址，没有（旧版 dsh / 外部服务）就退回裸地址。
    #[test]
    fn launch_url_appends_token_only_when_present() {
        assert_eq!(
            launch_url(Some("abc")),
            format!("http://127.0.0.1:{DSH_PORT}/?token=abc")
        );
        assert_eq!(launch_url(None), format!("http://127.0.0.1:{DSH_PORT}"));
        // 空串等同于没有令牌，不能拼出 `?token=` 这种畸形地址。
        assert_eq!(launch_url(Some("")), format!("http://127.0.0.1:{DSH_PORT}"));
    }

    #[test]
    fn launch_token_support_starts_at_012() {
        assert!(is_legacy_without_launch_token("0.1.0"));
        assert!(is_legacy_without_launch_token("0.1.1-rc.2"));
        assert!(!is_legacy_without_launch_token("0.1.2-alpha.1"));
        assert!(!is_legacy_without_launch_token("0.1.2"));
    }
}
