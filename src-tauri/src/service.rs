//! DSH 服务生命周期管理：
//! - 探测 127.0.0.1:3080 是否已有服务（外部服务直接复用，不做任何停止操作）；
//! - 按设置的拉起方式启动子进程；子进程 stdout/stderr 追加写入日志文件，
//!   由 tailer 线程轮询转发给前端（放生后服务仍可安全运行，不受管道 SIGPIPE 影响）；
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
use crate::settings::LaunchMethod;
use crate::AppState;

pub const DSH_PORT: u16 = 3080;
/// npx 首次运行需要下载包，给足时间。
const START_TIMEOUT: Duration = Duration::from_secs(180);

/// 内置运行时在 Resources 中的目录名（与 scripts/bundle-runtime.mjs 的输出一致）。
const RUNTIME_DIR: &str = "runtime";

/// 内置 Node.js 运行时根目录：<Resources>/runtime。
/// 非内置版打包没有该目录，返回 None（前端据此隐藏「内置 Node.js」方式）。
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
}

impl ServiceManager {
    pub fn new() -> Self {
        Self {
            child: Mutex::new(None),
            orphan: Mutex::new(None),
            starting: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            detail: Mutex::new(String::new()),
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
        let child = self.child.lock().unwrap();
        let orphan = *self.orphan.lock().unwrap();
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
        let settings = {
            let state = handle.state::<AppState>();
            let guard = state.settings.lock().unwrap();
            guard.clone()
        };
        // 内置版强制内置：状态展示与实际启动命令保持一致
        let method = if runtime_root(handle).is_some() {
            LaunchMethod::Builtin.display("", crate::i18n::current(handle))
        } else {
            settings
                .launch_method
                .display(&settings.launch_dir, crate::i18n::current(handle))
        };
        ServiceInfo {
            state,
            mine,
            pid: child.as_ref().map(|c| c.id()).or(orphan),
            method,
            detail: self.detail.lock().unwrap().clone(),
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
        *self.detail.lock().unwrap() = text;
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
        if self.child.lock().unwrap().is_some() {
            self.finish(
                handle,
                tr(crate::i18n::current(handle), "svc.already_running", &[]),
            );
            return;
        }
        // 上次退出放生的服务仍在运行：接管，继续管理。
        if let Some(pid) = *self.orphan.lock().unwrap() {
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
            *self.orphan.lock().unwrap() = None;
            self.clear_pid(handle);
        }
        if Self::is_up() {
            self.finish(
                handle,
                tr(crate::i18n::current(handle), "svc.external_running", &[]),
            );
            return;
        }

        self.starting.store(true, Ordering::SeqCst);
        self.failed.store(false, Ordering::SeqCst);
        self.set_detail(
            handle,
            tr(crate::i18n::current(handle), "svc.starting", &[]),
        );
        self.emit_status(handle);
        let start_time = Instant::now();

        let settings = handle.state::<AppState>().settings.lock().unwrap().clone();
        let runtime = runtime_root(handle);
        // 内置版（应用自带 runtime）强制使用内置 Node.js，不读用户设置；
        // 普通版按设置走；设置停在内置方式但 runtime 缺失（异常/开发目录）时回退 npx。
        let launch_method = if runtime.is_some() {
            if settings.launch_method != LaunchMethod::Builtin {
                self.push_log(
                    handle,
                    tr(crate::i18n::current(handle), "svc.builtin_forced", &[]),
                );
            }
            LaunchMethod::Builtin
        } else {
            if settings.launch_method == LaunchMethod::Builtin {
                self.push_log(
                    handle,
                    tr(crate::i18n::current(handle), "svc.builtin_fallback", &[]),
                );
            }
            settings.launch_method
        };
        // 先确保 npx 独立缓存目录存在（绕过用户 ~/.npm 的权限/损坏问题）。
        let npm_cache = files_dir(handle).join("npm-cache");
        let _ = std::fs::create_dir_all(&npm_cache);
        let (shell_cmd, cwd) = build_command(
            &launch_method,
            &settings.launch_dir,
            runtime.as_deref(),
            Some(&npm_cache),
        );
        let launch_method_display =
            launch_method.display(&settings.launch_dir, crate::i18n::current(handle));
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

        *self.child.lock().unwrap() = Some(child);

        // 等待端口就绪（或进程退出 / 超时）。
        let deadline = Instant::now() + START_TIMEOUT;
        let mut exited = false;
        loop {
            {
                let mut guard = self.child.lock().unwrap();
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
            if let Some(mut c) = self.child.lock().unwrap().take() {
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
    fn spawn_exit_watcher(&self, handle: &AppHandle, my_pid: u32) {
        let h = handle.clone();
        std::thread::spawn(move || loop {
            {
                let state = h.state::<AppState>();
                let sm = &state.sm;
                let mut guard = sm.child.lock().unwrap();
                if guard.as_ref().map(|c| c.id()) != Some(my_pid) {
                    return; // 已被 stop() 接管清理
                }
                match guard.as_mut().unwrap().try_wait() {
                    Ok(Some(_)) => {
                        guard.take();
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
            state.sm.stop_inner(&h);
        });
    }

    fn stop_inner(&self, handle: &AppHandle) {
        self.starting.store(false, Ordering::SeqCst);
        self.failed.store(false, Ordering::SeqCst);
        let mine_child = self.child.lock().unwrap().take();
        let mine_orphan = self.orphan.lock().unwrap().take();
        if let Some(mut child) = mine_child {
            self.set_detail(
                handle,
                tr(crate::i18n::current(handle), "svc.stopping", &[]),
            );
            self.emit_status(handle);
            kill_tree(&mut child);
            self.wait_port_free(handle);
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
            self.wait_port_free(handle);
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
    fn wait_port_free(&self, handle: &AppHandle) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Self::is_up() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = handle;
    }

    /// 重启服务（停止本应用启动的进程后再启动）。
    pub fn restart(&self, handle: &AppHandle) {
        let h = handle.clone();
        std::thread::spawn(move || {
            let state = h.state::<AppState>();
            state.sm.stop_inner(&h);
            state.sm.start_inner(&h);
        });
    }

    /// 退出应用且设置「停止服务」：杀掉本应用启动的所有进程（含孤儿），清理 pid 记录。
    pub fn shutdown(&self, handle: &AppHandle) {
        self.starting.store(false, Ordering::SeqCst);
        self.failed.store(false, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            kill_tree(&mut child);
        }
        if let Some(pid) = self.orphan.lock().unwrap().take() {
            kill_group(pid);
        }
        self.clear_pid(handle);
    }

    /// 退出应用且设置「不停止服务」：放弃子进程句柄但保留 pid 记录，服务继续运行，
    /// 下次启动自动接管。子进程输出已指向日志文件，不会因本进程退出而 SIGPIPE。
    pub fn detach(&self) {
        self.starting.store(false, Ordering::SeqCst);
        self.failed.store(false, Ordering::SeqCst);
        let _ = self.child.lock().unwrap().take();
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
                    *sm.orphan.lock().unwrap() = Some(pid);
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
            if sm.child.lock().unwrap().is_some() {
                continue; // 本应用启动的进程由 exit watcher 管理
            }
            let up = ServiceManager::is_up();
            if last_up != Some(up) {
                last_up = Some(up);
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
                Ok(0) => {}
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
                                // 唯一广播出口：读到的任何新行（子进程输出 / 应用 push_log 行）
                                // 原样 emit，绝不回写文件（回写会形成读→写→读反馈环）。
                                let _ = h.emit("service-log", &serde_json::json!({ "line": line }));
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

/// 本地时间戳 `[YYYY-MM-DD HH:MM:SS]` 前缀，供日志行写入与实时事件统一使用
/// （libc localtime，避免 UTC 与本地时区混淆）。
#[cfg(unix)]
pub(crate) fn now_ts() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}]",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

#[cfg(windows)]
pub(crate) fn now_ts() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_s(&mut tm, &t);
        format!(
            "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}]",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
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
    text.lines()
        .rev()
        .take(limit.max(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(String::from)
        .collect()
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
fn shell_path_prefix() -> String {
    let entries = path_dirs()
        .into_iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    format!("export PATH=\"{entries}\" 2>/dev/null; ")
}

/// 按设置构造 shell 命令与工作目录。
/// `runtime` 为内置 Node.js 运行时根目录（None 表示非内置版或目录缺失）；
/// `npm_cache` 为 npx 使用的独立缓存目录——npx 不再读写用户 ~/.npm，
/// 绕开历史遗留的 root 属主/损坏缓存导致的 EACCES/EEXIST 问题。
fn build_command(
    method: &LaunchMethod,
    dir: &str,
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
        match method {
            LaunchMethod::Npx => (format!("{npm_prefix}npx --yes @deepseek-ai/dsh web"), None),
            LaunchMethod::Dsh => ("dsh web".into(), None),
            LaunchMethod::Pnpm => {
                let dir = if dir.trim().is_empty() { "." } else { dir };
                ("pnpm dsh web".to_string(), Some(dir.to_string()))
            }
            LaunchMethod::Builtin => match runtime.and_then(runtime_entry) {
                Some((node, bin_js)) => (
                    format!("\"{}\" \"{}\" web", node.display(), bin_js.display()),
                    None,
                ),
                // 内置版打包缺失运行时（异常）：回退 npx，让日志暴露原因。
                None => (format!("{npm_prefix}npx --yes @deepseek-ai/dsh web"), None),
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
        match method {
            LaunchMethod::Npx => (
                format!("{path_prefix}{npm_prefix}exec npx --yes @deepseek-ai/dsh web"),
                None,
            ),
            LaunchMethod::Dsh => (format!("{path_prefix}exec dsh web"), None),
            LaunchMethod::Pnpm => {
                let dir = if dir.trim().is_empty() { "." } else { dir };
                let quoted = shell_quote(dir);
                (
                    format!("{path_prefix}cd {quoted} && exec pnpm dsh web"),
                    Some(dir.to_string()),
                )
            }
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
                            "export PATH=\"{entries}\" 2>/dev/null; exec \"{}\" \"{}\" web",
                            node.display(),
                            bin_js.display()
                        ),
                        None,
                    )
                }
                // 内置版打包缺失运行时（异常）：回退 npx，让日志暴露原因。
                None => (
                    format!("{path_prefix}{npm_prefix}exec npx --yes @deepseek-ai/dsh web"),
                    None,
                ),
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

/// 按进程组整体结束：SIGTERM → 宽限 → SIGKILL。
#[cfg(unix)]
fn kill_group(pid: u32) {
    let p = pid as i32;
    unsafe {
        let _ = libc::kill(-p, libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(1500));
    unsafe {
        let _ = libc::kill(-p, libc::SIGKILL);
    }
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
