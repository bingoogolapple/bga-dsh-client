//! 局域网扫码配对（hyper 版）：把 loopback 上的 DSH 服务经「一次性配对码网关」暴露给局域网。
//!
//! - 服务本身仍只监听 127.0.0.1:3080（本机 Loopback 访问不受任何影响）；
//! - 本模块在 0.0.0.0:<port> 起一个轻量「配对门禁 + 反向代理」：
//!   - 手机扫码（QR 内容 `http://<lan-ip>:<port>/?pair=<6位码>`）成功后，
//!     该浏览器会话放行 30 分钟；
//!   - 已放行会话的请求转发到 127.0.0.1:3080，**Host 与 Origin 一并改写为
//!     loopback authority**——天然通过 Harness 的 trusted-hosts 栅栏
//!     （Host 须为本地，浏览器带的 Origin 须与 Host 同源）；
//!   - 未配对设备一律 403，桌面端服务没起则返回 503 提示。
//!   - **所有访问（包括 loopback）都需要通过配对码验证**，确保内网穿透场景的安全性。
//!
//! # 设备身份：浏览器会话令牌（Cookie），而不是 IP
//!
//! 局域网直连时对端 IP 是唯一的设备指纹；但经内网穿透隧道（如
//! `ssh -R 80:localhost:<port> nokey@localhost.run`）访问时，外网所有流量都会被
//! 隧道折叠成本机 `127.0.0.1` 的 TCP 连接。若按 IP 放行，第一台设备配对成功后
//! 白名单里记下的 127.0.0.1 会让**任何拿到隧道网址的人**都绕过配对。
//!
//! 因此唯一信任通道是**浏览器会话**：配对成功即签发随机令牌
//! `Set-Cookie: dsh_pair=<token>`，服务端记录 令牌 → 过期时间（另存配对时的来源
//! IP 仅作列表展示，不参与信任判定）；后续请求凭 Cookie 通过门禁。令牌跟着浏览器
//! 走，不跟着 IP 走——局域网与穿透隧道行为完全一致，每台设备各自配对、互不影响；
//! 同一来源（IP）下的不同浏览器也互不通用。
//!
//! 传输层自 v2 起改为 hyper（HTTP/1.1 语义层）：请求解析、keep-alive、分帧、
//! chunked、连接复用全部交给 hyper，不再手写字节级解析；本模块只保留业务逻辑
//! （配对门禁、一次性码轮换、Host/Origin 改写、HTML polyfill 注入、WebSocket
//! 原始隧道）。
//!
//! # 模块划分
//!
//! - `token`：配对码 / 会话令牌的生成与轮换（纯逻辑）；
//! - `net`：局域网 IP 探测（本目录唯一的 unsafe 集中地）；
//! - `qrcode`：二维码 SVG / 位图生成；
//! - `http`：门禁响应（302/403/502/503）构造；
//! - `forward`：上游转发与 HTML polyfill 注入；
//! - `rewrite`：Host/Origin/Cookie 改写；
//! - `tunnel`：WebSocket 原始隧道。
//!
//! 本文件只做编排：状态定义、生命周期（start/stop/restart）、请求分发与命令桥。

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::i18n::tr;
use crate::AppState;

mod forward;
mod http;
mod net;
mod qrcode;
mod rewrite;
mod token;
mod tunnel;
mod upstream;

use forward::{build_client, forward_regular};
use http::{denied_response, redirect_home_with_session};
use net::lan_ipv4;
use qrcode::{qr_rgba, qr_svg};
use rewrite::{extract_pair_cookie, is_upgrade_request, query_has_pair};
use token::{gen_token, rotate_code};
use tunnel::handle_upgrade;

/// 配对有效期：30 分钟。
pub const PAIR_TTL: Duration = Duration::from_secs(30 * 60);
/// 代理绑定的起始端口（从这往后试）。
const BASE_PORT: u16 = 18080;
const UPSTREAM_IP: [u8; 4] = [127, 0, 0, 1];
const UPSTREAM_PORT: u16 = 3080;
/// WebSocket 握手准备阶段的连接/读写超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// 复制二维码 PNG 的放大倍数。
const QR_SCALE: u32 = 8;
/// 复制二维码 PNG 的留白（模块数）。
const QR_MARGIN: u32 = 2;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;
type HandlerBody = http_body_util::combinators::BoxBody<bytes::Bytes, BoxErr>;
type UpstreamClient = hyper_util::client::legacy::Client<
    hyper_util::client::legacy::connect::HttpConnector,
    HandlerBody,
>;

/// 配对网关状态（AppState 内，监听任务与命令共用）。
pub struct Pairing {
    /// 代理是否运行中。
    running: bool,
    /// 启动失败原因（端口全被占用等）。
    error: Option<String>,
    code: String,
    /// QR 完整内容（含配对码），供展示 / 复制 / 生成 PNG。
    url: String,
    port: u16,
    lan_ip: Option<Ipv4Addr>,
    qr_svg: String,
    /// 停止信号（serve 任务轮询）。
    stop: Arc<AtomicBool>,
    /// 上一次 serve 任务的退出信号接收端：任务结束（含错误退出）时 drop 对应 Sender，
    /// 此后 recv 返回 Err。重绑端口前先收它，避免旧监听尚未释放导致端口漂移。
    done: Option<std::sync::mpsc::Receiver<()>>,
    /// 已配对浏览器会话：令牌 → 会话信息。配对成功即签发 Cookie，身份跟着
    /// 浏览器走、不跟着 IP 走——局域网与内网穿透隧道（localhost.run 等）行为一致，
    /// 隧道里每台设备各自配对，互不影响。
    sessions: HashMap<String, Session>,
    /// 网关代持的上游（dsh）会话 cookie，以及换它时用的启动令牌——令牌一变
    /// 就作废重换（详见 `upstream` 模块文档）。
    upstream_cookie: Option<String>,
    upstream_cookie_token: Option<String>,
}

/// 一个已配对浏览器会话。
pub struct Session {
    /// 过期时间（墙钟：`Instant` 在系统休眠期间不计时，作为 TTL 会被无限拉长）。
    expires: SystemTime,
    /// 配对时的来源 IP（**仅展示用**，不参与信任判定；经 localhost.run 等
    /// 隧道访问时恒为 127.0.0.1，这正是不能按 IP 信任的原因）。
    peer: IpAddr,
}

/// 下发给前端的信息。
#[derive(Serialize, Clone)]
pub struct PairingInfo {
    running: bool,
    ip: String,
    port: u16,
    code: String,
    qr: String,
    /// 已配对浏览器会话列表（展示用）：来源 IP + 剩余分钟。
    sessions: Vec<SessionInfo>,
    service_up: bool,
}

/// 无需配对的公开元数据：PWA 清单与站点图标。
///
/// 浏览器取 manifest 时按规范**不带凭据**（不会发 cookie），所以配对 cookie 发不
/// 出去，这种请求永远会被门禁拦下。而这些是纯静态元数据（应用名、图标、主题色），
/// 既不含会话也不含业务数据——门禁真正保护的是 `/api` 与页面内容。放行它们，
/// 手机才能「添加到主屏幕」，顺便消掉这条无谓的 403。
const PUBLIC_METADATA: [&str; 2] = ["/manifest.webmanifest", "/favicon.ico"];

/// 是否属于无需配对的公开元数据。
fn is_public_metadata(path: &str) -> bool {
    PUBLIC_METADATA.contains(&path)
}

/// 单个已配对会话的展示信息。
#[derive(Serialize, Clone)]
pub struct SessionInfo {
    /// 配对时的来源 IP。
    ip: String,
    /// 剩余有效分钟（向上取整）。
    minutes_left: u64,
}

impl Pairing {
    pub fn new() -> Self {
        Self {
            running: false,
            error: None,
            code: token::gen_code(),
            url: String::new(),
            port: 0,
            lan_ip: None,
            qr_svg: String::new(),
            stop: Arc::new(AtomicBool::new(true)),
            done: None,
            sessions: HashMap::new(),
            upstream_cookie: None,
            upstream_cookie_token: None,
        }
    }
}

impl Default for Pairing {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 代理服务日志：追加写 pairing.log + emit pairing-log 事件（设置页实时展示）。
// 与 service.log 同级目录、同一 tail 语义；事件与文件双通道。
// ---------------------------------------------------------------------------

pub(crate) fn log_path(app: &AppHandle) -> PathBuf {
    crate::service::files_dir(app).join("pairing.log")
}

pub(crate) fn push_log(app: &AppHandle, line: impl AsRef<str>) {
    let line = crate::service::now_ts() + " " + line.as_ref();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(app))
    {
        let _ = writeln!(f, "{line}");
    }
    let _ = app.emit("pairing-log", serde_json::json!({ "line": line }));
}

pub(crate) fn read_log_tail(app: &AppHandle, limit: usize) -> Vec<String> {
    crate::service::read_tail(&log_path(app), limit)
}

// ---------------------------------------------------------------------------
// 生命周期：启动 / 停止 / 重启
// ---------------------------------------------------------------------------

/// 确保网关已启动（幂等）；失败返回错误信息。
pub fn ensure_started(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut p = crate::state::lock(&state.pairing);
    if p.running {
        return p.error.clone().map_or(Ok(()), Err);
    }
    // 等上一个 serve 任务退出并释放端口：restart（或 stop→start）里旧实例仍
    // 在异步收尾时若不等待，bind_free 会跳开旧端口，导致每次重启端口 +1 漂移。
    if let Some(rx) = p.done.take() {
        drop(p);
        let _ = rx.recv_timeout(std::time::Duration::from_secs(3));
        p = crate::state::lock(&state.pairing);
    }
    // 探测局域网 IP（决定 URL/QR 用什么地址广播）。
    let ip = lan_ipv4().ok_or_else(|| {
        let locale = crate::i18n::current(app);
        push_log(app, tr(locale, "pair.no_ip_log", &[]));
        tr(locale, "pair.no_ip", &[])
    })?;
    p.lan_ip = Some(ip);

    let listener = match bind_free(BASE_PORT) {
        Ok((l, port)) => {
            p.port = port;
            l
        }
        Err(e) => {
            let locale = crate::i18n::current(app);
            p.error = Some(tr(
                locale,
                "pair.port_busy",
                &[
                    &BASE_PORT.to_string(),
                    &(BASE_PORT + 30).to_string(),
                    &e.to_string(),
                ],
            ));
            push_log(
                app,
                tr(
                    locale,
                    "pair.port_busy_log",
                    &[&BASE_PORT.to_string(), &(BASE_PORT + 30).to_string()],
                ),
            );
            return Err(p.error.clone().unwrap());
        }
    };
    let url = format!("http://{ip}:{}/?pair={}", p.port, p.code);
    p.url = url.clone();
    let locale = crate::i18n::current(app);
    p.qr_svg = qr_svg(&url).ok_or_else(|| tr(locale, "pair.qr_fail", &[]))?;
    p.stop = Arc::new(AtomicBool::new(false));
    p.running = true;
    p.error = None;
    push_log(
        app,
        tr(
            locale,
            "pair.start_log",
            &[&ip.to_string(), &p.port.to_string(), &p.code],
        ),
    );
    let stop = p.stop.clone();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    p.done = Some(rx);
    let h = app.clone();
    let upstream = SocketAddr::from((UPSTREAM_IP, UPSTREAM_PORT));
    let client = build_client();
    tauri::async_runtime::spawn(async move {
        // tx 随任务结束 drop → 等待方的 recv 解除，可确认端口已释放。
        let _done_tx = tx;
        serve_loop(h, listener, stop, client, upstream).await;
    });
    // 预热上游会话：手机上第一次访问就不必等这次交换。失败不记日志，
    // 配对成功时还会再试一次（那时才值得提示用户）。
    let warm = app.clone();
    std::thread::spawn(move || {
        if upstream::ensure_cookie(&warm, upstream).is_some() {
            push_log(
                &warm,
                tr(crate::i18n::current(&warm), "pair.upstream_ok_log", &[]),
            );
        }
    });
    Ok(())
}

/// 停止代理服务：关闭监听、清空已配对会话（配对码保留，重新启动后仍用原码）。
pub fn stop_pairing(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut p = crate::state::lock(&state.pairing);
    if p.running {
        p.running = false;
        p.stop.store(true, Ordering::SeqCst);
        p.sessions.clear();
        push_log(app, tr(crate::i18n::current(app), "pair.stop_log", &[]));
    }
}

/// 重启代理服务：停止当前实例（保留配对码与已配对会话），再重新拉起监听。
/// 与「停止 + 启动」的区别：白名单不清除，已配对设备无需重新扫码。
pub fn restart(app: &AppHandle) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        let mut p = crate::state::lock(&state.pairing);
        if p.running {
            p.running = false;
            p.stop.store(true, Ordering::SeqCst);
        }
    }
    push_log(app, tr(crate::i18n::current(app), "pair.restart_log", &[]));
    ensure_started(app)
}

fn bind_free(from: u16) -> std::io::Result<(std::net::TcpListener, u16)> {
    for port in from..=from + 30 {
        if let Ok(l) = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)) {
            // tokio::net::TcpListener::from_std 要求 socket 已非阻塞
            // （tokio >= 1.53 会在阻塞 socket 上 panic）。
            l.set_nonblocking(true)?;
            return Ok((l, port));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        "all ports busy",
    ))
}

async fn serve_loop(
    app: AppHandle,
    listener: std::net::TcpListener,
    stop: Arc<AtomicBool>,
    client: UpstreamClient,
    upstream: SocketAddr,
) {
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("DeepSeekHarness: LAN listener setup failed: {e}");
            return;
        }
    };
    // 100ms 轮询 stop，与旧同步实现的可停止语义一致。
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match tokio::time::timeout(Duration::from_millis(100), listener.accept()).await {
            Ok(Ok((sock, addr))) => {
                let peer = addr.ip();
                let app = app.clone();
                let client = client.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let app = app.clone();
                        let client = client.clone();
                        async move {
                            let res: Response<HandlerBody> =
                                handle_request(req, peer, app, client, upstream).await;
                            Ok::<_, Infallible>(res)
                        }
                    });
                    let conn = http1::Builder::new().serve_connection(TokioIo::new(sock), service);
                    // with_upgrades():没有它,带 Connection: upgrade 的请求的
                    // hyper::upgrade::on() future 永远不会完成。
                    let _ = conn.with_upgrades().await;
                });
            }
            Ok(Err(_)) => return,
            Err(_) => continue,
        }
    }
}

/// 单请求处理：配对/信任门禁 → WebSocket 隧道或普通转发。
async fn handle_request(
    req: Request<Incoming>,
    peer: IpAddr,
    app: AppHandle,
    client: UpstreamClient,
    upstream: SocketAddr,
) -> Response<HandlerBody> {
    // 配对/信任门禁：
    // - 所有设备（包括 loopback）都需要通过配对码验证或已放行；
    // - ?pair=<code> 命中即签发浏览器会话令牌（一次性——立即作废旧码换新码），
    //   302 + Set-Cookie 跳回首页；
    // - 校验、签发、轮换在同一把锁内完成：并发访问时同一码最多只可能命中一次。
    // 请求路径（查询串可能带配对码，落日志时只取路径部分）。
    let target = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    let path = target.split('?').next().unwrap_or("/").to_owned();
    let trusted = is_public_metadata(&path) || {
        let state = app.state::<AppState>();
        let mut p = crate::state::lock(&state.pairing);

        // 检查是否包含有效的配对码
        if query_has_pair(&target, &p.code) {
            // 签发会话令牌（防碰撞重试），Set-Cookie 随 302 返回浏览器；
            // 白名单从此按 令牌 记，不再按 IP 记（peer 仅作展示元数据）。
            let token = loop {
                let t = gen_token();
                if !p.sessions.contains_key(&t) {
                    break t;
                }
            };
            p.sessions.insert(
                token.clone(),
                Session {
                    expires: SystemTime::now() + PAIR_TTL,
                    peer,
                },
            );
            rotate_code(&mut p);
            drop(p);
            // 配对成功就顺手把上游会话换好：手机 302 回首页时不会撞上 401。
            if upstream::ensure_cookie(&app, upstream).is_none() {
                push_log(
                    &app,
                    tr(crate::i18n::current(&app), "pair.upstream_fail_log", &[]),
                );
            }
            push_log(
                &app,
                tr(
                    crate::i18n::current(&app),
                    "pair.pair_ok_log",
                    &[&peer.to_string()],
                ),
            );
            return redirect_home_with_session(&token);
        }

        // 会话 Cookie（浏览器配对）：令牌跟着浏览器走、不跟着 IP 走。
        // loopback 来源（localhost.run 等隧道把外网流量折叠成 127.0.0.1）与
        // 局域网直连走同一通道——封死「一台设备配对、全员免检」。
        let session_ok = extract_pair_cookie(req.headers())
            .as_deref()
            .and_then(|t| p.sessions.get(t))
            .map(|s| s.expires > SystemTime::now())
            .unwrap_or(false);

        // 顺手清理过期条目（每次访问顺带做，量小）。
        p.sessions.retain(|_, s| s.expires > SystemTime::now());
        session_ok
    };
    if !trusted {
        // 带上路径与 UA：只看 IP 分不清是「旧配对码被复用」「不带凭据的浏览器请求
        // （manifest 之类）」还是「服务端回调」——三者性质完全不同。
        let ua: String = req
            .headers()
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .chars()
            .take(60)
            .collect();
        push_log(
            &app,
            tr(
                crate::i18n::current(&app),
                "pair.deny_log",
                &[&peer.to_string(), &path, &ua],
            ),
        );
        return denied_response(crate::i18n::current(&app));
    }

    // 插件脚本改写未命中会静默降级（局域网端退回「非本机」语义：内测声明反复弹、
    // 设置不落盘），攒着由这里节流上报——否则只能从页面行为反推。
    if rewrite::take_rewrite_warning() {
        push_log(
            &app,
            tr(crate::i18n::current(&app), "pair.rewrite_missed_log", &[]),
        );
    }

    // dsh 0.1.2+ 的 /api 认证：手机浏览器没有上游的会话 cookie，由网关代持并
    // 注入（缓存命中时开销可忽略）。
    let cookie = upstream::ensure_cookie(&app, upstream);

    if is_upgrade_request(req.headers()) {
        return handle_upgrade(req, upstream, cookie.as_deref()).await;
    }
    let res = forward_regular(req, client, upstream, cookie.as_deref()).await;
    if res.status() == StatusCode::UNAUTHORIZED {
        // 代持的会话失效了（cookie 过期 / 上游凭据记录被删）：作废缓存，
        // 让下一个请求重新交换。当前请求已消费掉 body，无法重放。
        upstream::invalidate(&app);
    }
    res
}

// ---------------------------------------------------------------------------
// Tauri 命令桥
// ---------------------------------------------------------------------------

/// 只读查询（不启动）：窗口轮询时若在停止状态保持停止。
pub fn info(app: &AppHandle) -> Result<PairingInfo, String> {
    let state = app.state::<AppState>();
    let p = crate::state::lock(&state.pairing);
    let mut sessions: Vec<SessionInfo> = p
        .sessions
        .iter()
        .filter(|(_, s)| s.expires > SystemTime::now())
        .map(|(_, s)| SessionInfo {
            ip: s.peer.to_string(),
            minutes_left: s
                .expires
                .duration_since(SystemTime::now())
                .unwrap_or_default()
                .as_secs()
                .div_ceil(60),
        })
        .collect();
    sessions.sort_by(|a, b| a.ip.cmp(&b.ip));
    Ok(PairingInfo {
        running: p.running,
        ip: p.lan_ip.unwrap_or(Ipv4Addr::LOCALHOST).to_string(),
        port: p.port,
        code: p.code.clone(),
        qr: p.qr_svg.clone(),
        sessions,
        service_up: crate::service::ServiceManager::is_up(),
    })
}

/// 重新生成配对码并清空已配对会话。
pub fn regen(app: &AppHandle) -> Result<PairingInfo, String> {
    let new_code;
    {
        let state = app.state::<AppState>();
        let mut p = crate::state::lock(&state.pairing);
        rotate_code(&mut p);
        new_code = p.code.clone();
        p.sessions.clear();
    }
    push_log(
        app,
        tr(crate::i18n::current(app), "pair.regen_log", &[&new_code]),
    );
    info(app)
}

/// 复制完整访问链接（含配对码）到剪贴板。
pub fn copy_url(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let url = crate::state::lock(&state.pairing).url.clone();
    if url.is_empty() {
        return Err(tr(crate::i18n::current(app), "pair.url_not_ready", &[]));
    }
    arboard::Clipboard::new()
        .map_err(|e| e.to_string())?
        .set_text(url)
        .map_err(|e| e.to_string())
}

/// 复制二维码图片（PNG）到剪贴板：直接从 QR 矩阵渲染 RGBA，无临时文件。
pub fn copy_qr_image(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let url = crate::state::lock(&state.pairing).url.clone();
    if url.is_empty() {
        return Err(tr(crate::i18n::current(app), "pair.qr_not_ready", &[]));
    }
    let locale = crate::i18n::current(app);
    // 用 qrcode::qr_rgba 统一渲染（与 test 用例共用同一份实现，避免两处逻辑漂移）
    let (size, _, rgba) = qr_rgba(&url).ok_or_else(|| tr(locale, "pair.qr_gen_fail", &[]))?;
    arboard::Clipboard::new()
        .map_err(|e| e.to_string())?
        .set_image(arboard::ImageData {
            width: size as usize,
            height: size as usize,
            bytes: std::borrow::Cow::Owned(rgba),
        })
        .map_err(|e| e.to_string())
}

// 测试集中在 tests.rs（原 mod.rs 内联的那批用例在二次拆分时整体迁出，
// 以免重写 mod.rs 时丢失覆盖），故这里只声明模块。
#[cfg(test)]
mod tests;
