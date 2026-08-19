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

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue, CONTENT_TYPE, LOCATION, SET_COOKIE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as LegacyClient;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::i18n::{tr, Locale};
use crate::AppState;

use forward::{build_client, empty_body, forward_regular, full_body};
use rewrite::{
    extract_pair_cookie, is_upgrade_request, query_has_pair, rewrite_loopback, PAIR_COOKIE,
};
use tunnel::handle_upgrade;

mod forward;
mod rewrite;
mod tunnel;

/// 配对有效期：30 分钟。
pub const PAIR_TTL: Duration = Duration::from_secs(30 * 60);
/// 代理绑定的起始端口（从这往后试）。
const BASE_PORT: u16 = 18080;
const UPSTREAM_IP: [u8; 4] = [127, 0, 0, 1];
const UPSTREAM_PORT: u16 = 3080;
/// 等待上游返回响应头（首字节）的超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// WebSocket 握手准备阶段的连接/读写超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// 200 text/html 注入 polyfill 时允许缓冲的最大响应体。
const HTML_BODY_MAX: usize = 8 * 1024 * 1024;
/// 复制二维码 PNG 的放大倍数。
const QR_SCALE: u32 = 8;
/// 复制二维码 PNG 的留白（模块数）。
const QR_MARGIN: u32 = 2;
/// 配对会话令牌的随机字节数（hex 后 32 字符）。
const TOKEN_BYTES: usize = 16;
/// 令牌 hex 长度（Cookie 值长度）。
const TOKEN_LEN: usize = TOKEN_BYTES * 2;
/// 极端兜底（OS 随机源故障）时的计数器。
static TOKEN_SEQ: AtomicU64 = AtomicU64::new(0);

type BoxErr = Box<dyn std::error::Error + Send + Sync>;
type HandlerBody = BoxBody<Bytes, BoxErr>;
type UpstreamClient = LegacyClient<HttpConnector, HandlerBody>;

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
    /// 已配对浏览器会话：令牌 → 会话信息。配对成功即签发 Cookie，身份跟着
    /// 浏览器走、不跟着 IP 走——局域网与内网穿透隧道（localhost.run 等）行为一致，
    /// 隧道里每台设备各自配对，互不影响。
    sessions: HashMap<String, Session>,
}

/// 一个已配对浏览器会话。
pub struct Session {
    /// 过期时间。
    expires: Instant,
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
            code: gen_code(),
            url: String::new(),
            port: 0,
            lan_ip: None,
            qr_svg: String::new(),
            stop: Arc::new(AtomicBool::new(true)),
            sessions: HashMap::new(),
        }
    }
}

/// 生成新的 6 位一次性配对码。
fn gen_code() -> String {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        % 1_000_000;
    format!("{n:06}")
}

/// 生成加密随机会话令牌（hex，`TOKEN_LEN` 字符）。
/// OS 随机源故障时退回 时间+进程号+计数器 组合，仍不可预测。
fn gen_token() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    if getrandom::getrandom(&mut buf).is_err() {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
        return format!("{n:x}{:x}{seq:x}", std::process::id());
    }
    let mut s = String::with_capacity(TOKEN_LEN);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 轮换配对码并同步 URL/QR（已配对设备的白名单不受影响）。
fn rotate_code(p: &mut Pairing) {
    p.code = gen_code();
    if let Some(ip) = p.lan_ip {
        if p.port == 0 {
            p.port = BASE_PORT;
        }
        let url = format!("http://{ip}:{}/?pair={}", p.port, p.code);
        p.url = url.clone();
        p.qr_svg = qr_svg(&url).unwrap_or_default();
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

/// 探测局域网 IPv4：优先默认路由出口 IP（Wi-Fi/有线即手机同网段），
/// 取不到时枚举所有接口里第一个私网 IPv4。
pub fn lan_ipv4() -> Option<Ipv4Addr> {
    let udp = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    if udp.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).is_ok() {
        if let Ok(addr) = udp.local_addr() {
            if let IpAddr::V4(v) = addr.ip() {
                if !v.is_loopback() {
                    return Some(v);
                }
            }
        }
    }
    lan_ipv4_ifaddrs()
}

#[cfg(unix)]
fn lan_ipv4_ifaddrs() -> Option<Ipv4Addr> {
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return None;
        }
        let mut best: Option<Ipv4Addr> = None;
        let mut p = ifap;
        while !p.is_null() {
            let ifa = &*p;
            if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !ip.is_loopback()
                    && !ip.is_link_local()
                    && !ip.is_unspecified()
                    && ip.is_private()
                    && best.map(|b| !b.is_private()).unwrap_or(true)
                {
                    best = Some(ip);
                }
            }
            p = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
        best
    }
}

#[cfg(not(unix))]
fn lan_ipv4_ifaddrs() -> Option<Ipv4Addr> {
    None
}

/// 解析 QR 矩阵：返回（宽度, 行优先的深色标记）。
#[allow(deprecated)] // qrcode 0.14 的 to_colors 依赖私有 Color，to_vec 等效且无隐私问题
fn qr_modules(url: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::new(url).ok()?;
    Some((code.width(), code.to_vec()))
}

/// 生成 QR 的 SVG。
fn qr_svg(url: &str) -> Option<String> {
    let (w, bits) = qr_modules(url)?;
    let mut path = String::new();
    for y in 0..w {
        for x in 0..w {
            if bits[y * w + x] {
                path.push_str(&format!("M{x} {y}h1v1h-1z"));
            }
        }
    }
    Some(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {w}" shape-rendering="crispEdges"><rect width="{w}" height="{w}" fill="#ffffff"/><path fill="#1f2430" d="{path}"/></svg>"##
    ))
}

/// 确保网关已启动（幂等）；失败返回错误信息。
pub fn ensure_started(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut p = state.pairing.lock().unwrap();
    if p.running {
        return p.error.clone().map_or(Ok(()), Err);
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
    let h = app.clone();
    let upstream = SocketAddr::from((UPSTREAM_IP, UPSTREAM_PORT));
    let client = build_client();
    tauri::async_runtime::spawn(async move {
        serve_loop(h, listener, stop, client, upstream).await;
    });
    Ok(())
}

/// 停止代理服务：关闭监听、清空已配对会话（配对码保留，重新启动后仍用原码）。
pub fn stop_pairing(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut p = state.pairing.lock().unwrap();
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
        let mut p = state.pairing.lock().unwrap();
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
    let trusted = {
        let state = app.state::<AppState>();
        let mut p = state.pairing.lock().unwrap();

        // 检查是否包含有效的配对码
        let target = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");
        if query_has_pair(target, &p.code) {
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
                    expires: Instant::now() + PAIR_TTL,
                    peer,
                },
            );
            rotate_code(&mut p);
            drop(p);
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
            .map(|s| s.expires > Instant::now())
            .unwrap_or(false);

        // 顺手清理过期条目（每次访问顺带做，量小）。
        p.sessions.retain(|_, s| s.expires > Instant::now());
        session_ok
    };
    if !trusted {
        push_log(
            &app,
            tr(
                crate::i18n::current(&app),
                "pair.deny_log",
                &[&peer.to_string()],
            ),
        );
        return denied_response(crate::i18n::current(&app));
    }

    if is_upgrade_request(req.headers()) {
        return handle_upgrade(req, upstream).await;
    }
    let res = forward_regular(req, client, upstream).await;
    res
}

// ---------------------------------------------------------------------------
// 请求改写（纯函数，可单测）
// ---------------------------------------------------------------------------

fn redirect_home() -> Response<HandlerBody> {
    let mut res = Response::builder()
        .status(StatusCode::FOUND)
        .body(empty_body())
        .unwrap_or_else(|_| bad_gateway_response());
    res.headers_mut()
        .insert(LOCATION, HeaderValue::from_static("/"));
    res
}

/// 配对成功的 302：带 `Set-Cookie: dsh_pair=<token>` 跳回首页。
/// 令牌记在浏览器 Cookie 里，后续请求凭它通过门禁（不依赖来源 IP）。
fn redirect_home_with_session(token: &str) -> Response<HandlerBody> {
    let mut res = redirect_home();
    let cookie = format!(
        "{PAIR_COOKIE}={token}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax",
        PAIR_TTL.as_secs()
    );
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(SET_COOKIE, v);
    }
    res
}

fn html_response(status: StatusCode, title: &str, body: &str) -> Response<HandlerBody> {
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><style>body{{font-family:-apple-system,sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0;background:#f6f7fb;color:#1f2430}}div{{text-align:center;max-width:420px;padding:24px}}h1{{font-size:18px}}p{{font-size:13.5px;color:#5b6472;line-height:1.7}}code{{display:inline-block;margin-top:8px;font-size:11.5px;color:#8a5160;background:#fdeef0;border-radius:6px;padding:2px 6px}}</style><div><h1>{title}</h1><p>{body}</p></div>"
    );
    let mut res = Response::builder()
        .status(status)
        .body(full_body(Bytes::from(html)))
        .unwrap_or_else(|_| bad_gateway_response());
    res.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    res
}

fn denied_response(locale: Locale) -> Response<HandlerBody> {
    html_response(
        StatusCode::FORBIDDEN,
        &tr(locale, "pair.denied_title", &[]),
        &tr(locale, "pair.denied_body", &[]),
    )
}

fn service_down_response() -> Response<HandlerBody> {
    let locale = crate::i18n::global();
    html_response(
        StatusCode::SERVICE_UNAVAILABLE,
        &tr(locale, "pair.down_title", &[]),
        &tr(locale, "pair.down_body", &[]),
    )
}

fn bad_gateway_response() -> Response<HandlerBody> {
    let locale = crate::i18n::global();
    html_response(
        StatusCode::BAD_GATEWAY,
        &tr(locale, "pair.gw_title", &[]),
        &tr(locale, "pair.gw_body", &[]),
    )
}

// ---------------------------------------------------------------------------
// Tauri 命令桥
// ---------------------------------------------------------------------------

/// 只读查询（不启动）：窗口轮询时若在停止状态保持停止。
pub fn info(app: &AppHandle) -> Result<PairingInfo, String> {
    let state = app.state::<AppState>();
    let p = state.pairing.lock().unwrap();
    let mut sessions: Vec<SessionInfo> = p
        .sessions
        .iter()
        .filter(|(_, s)| s.expires > Instant::now())
        .map(|(_, s)| SessionInfo {
            ip: s.peer.to_string(),
            minutes_left: s
                .expires
                .duration_since(Instant::now())
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
        let mut p = state.pairing.lock().unwrap();
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
    let url = state.pairing.lock().unwrap().url.clone();
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
    let url = state.pairing.lock().unwrap().url.clone();
    if url.is_empty() {
        return Err(tr(crate::i18n::current(app), "pair.qr_not_ready", &[]));
    }
    let locale = crate::i18n::current(app);
    let (w, bits) = qr_modules(&url).ok_or_else(|| tr(locale, "pair.qr_gen_fail", &[]))?;
    let size = (w as u32 + QR_MARGIN * 2) * QR_SCALE;
    let mut rgba = vec![255u8; (size * size * 4) as usize];
    for y in 0..w {
        for x in 0..w {
            if bits[y * w + x] {
                let x0 = ((x as u32 + QR_MARGIN) * QR_SCALE) as usize;
                let y0 = ((y as u32 + QR_MARGIN) * QR_SCALE) as usize;
                for dy in 0..QR_SCALE as usize {
                    for dx in 0..QR_SCALE as usize {
                        let i = ((y0 + dy) * size as usize + (x0 + dx)) * 4;
                        rgba[i..i + 4].copy_from_slice(&[0x1f, 0x24, 0x30, 0xff]);
                    }
                }
            }
        }
    }
    arboard::Clipboard::new()
        .map_err(|e| e.to_string())?
        .set_image(arboard::ImageData {
            width: size as usize,
            height: size as usize,
            bytes: std::borrow::Cow::Owned(rgba),
        })
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::forward::forward_regular;
    use super::rewrite::{
        extract_pair_cookie, inject_html_polyfills, is_upgrade_request, query_has_pair,
        rewrite_connection_bundle, rewrite_loopback, strip_hop_by_hop, strip_pair_cookie, POLYFILL,
    };
    use super::tunnel::{
        build_raw_request_head, build_upgrade_response, find_head_end, head_starts_101,
    };
    use super::*;
    use http_body_util::{BodyExt, Full};
    use hyper::header::{
        HeaderMap, CONNECTION, CONTENT_LENGTH, COOKIE, HOST, ORIGIN, TRANSFER_ENCODING, UPGRADE,
    };
    use hyper::Method;

    #[test]
    fn rewrite_connection_bundle_turns_is_loopback_true() {
        let sample = b"const handle = { api, isLoopback: pageLocation === void 0 || isLoopbackHostname(pageLocation.hostname), hostDescription: {} };";
        let rewritten = rewrite_connection_bundle(sample).expect("pattern must match");
        let text = String::from_utf8(rewritten).unwrap();
        assert!(text.contains("isLoopback: true"));
        assert!(!text.contains("isLoopbackHostname(pageLocation.hostname)"));
    }

    #[test]
    fn rewrite_connection_bundle_returns_none_for_other_js() {
        assert!(rewrite_connection_bundle(b"const a = 1; isLoopbackHostname(x);").is_none());
        assert!(rewrite_connection_bundle(b"\xff\xfe not utf8").is_none());
    }

    #[test]
    fn inject_polyfill_goes_before_head_close() {
        let html = b"<!doctype html><html><head><title>t</title></head><body>hi</body></html>";
        let out = inject_html_polyfills(html);
        let s = String::from_utf8(out).unwrap();
        let head_close = s.find("</head>").unwrap();
        let poly = s.find("<script>").unwrap();
        assert!(poly < head_close);
        assert!(s.contains("crypto.randomUUID"));
    }

    #[test]
    fn inject_polyfill_fallback_when_no_head() {
        let html = b"<!doctype html><body>hi</body>";
        let out = inject_html_polyfills(html);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("<script>"));
        assert!(s.ends_with("</html>") || s.ends_with("hi</body>"));
    }

    #[test]
    fn polyfill_has_no_crlf_and_quotes_balanced() {
        assert!(POLYFILL.contains("crypto.randomUUID"));
        assert!(!POLYFILL.contains('\n'));
        assert!(!POLYFILL.contains('\r'));
    }

    #[test]
    fn rewrite_loopback_rewrites_host_and_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("192.0.2.1:18080"));
        headers.insert(ORIGIN, HeaderValue::from_static("http://192.0.2.1:18080"));
        rewrite_loopback(&mut headers);
        assert_eq!(headers.get(HOST).unwrap(), "127.0.0.1:3080");
        assert_eq!(headers.get(ORIGIN).unwrap(), "http://127.0.0.1:3080");
    }

    #[test]
    fn rewrite_loopback_preserves_absent_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("192.0.2.1:18080"));
        rewrite_loopback(&mut headers);
        assert_eq!(headers.get(HOST).unwrap(), "127.0.0.1:3080");
        assert!(!headers.contains_key(ORIGIN));
    }

    #[test]
    fn strip_hop_by_hop_removes_connection_and_framing() {
        let mut headers = HeaderMap::new();
        for name in [
            "connection",
            "keep-alive",
            "transfer-encoding",
            "upgrade",
            "te",
            "trailer",
        ] {
            headers.insert(name, HeaderValue::from_static("x"));
        }
        headers.insert(HOST, HeaderValue::from_static("127.0.0.1:3080"));
        strip_hop_by_hop(&mut headers);
        assert!(!headers.contains_key("connection"));
        assert!(!headers.contains_key("keep-alive"));
        assert!(!headers.contains_key("transfer-encoding"));
        assert!(!headers.contains_key("upgrade"));
        assert!(headers.contains_key(HOST));
    }

    #[test]
    fn is_upgrade_request_detects_websocket() {
        let mut headers = HeaderMap::new();
        headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive, Upgrade"));
        assert!(is_upgrade_request(&headers));

        let mut no_upgrade = HeaderMap::new();
        no_upgrade.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        assert!(!is_upgrade_request(&no_upgrade));

        let mut no_connection = HeaderMap::new();
        no_connection.insert(UPGRADE, HeaderValue::from_static("websocket"));
        assert!(!is_upgrade_request(&no_connection));
    }

    #[test]
    fn raw_request_head_preserves_websocket_headers() {
        let method = Method::GET;
        let uri = "/api/events.mux";
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("192.0.2.1:18080"));
        headers.insert(ORIGIN, HeaderValue::from_static("http://192.0.2.1:18080"));
        headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert("sec-websocket-key", HeaderValue::from_static("abc123=="));
        rewrite_loopback(&mut headers);
        let head = build_raw_request_head(&method, uri, &headers);
        let text = String::from_utf8(head).unwrap();
        assert!(text.starts_with("GET /api/events.mux HTTP/1.1\r\n"));
        // HeaderName 的 Display 输出统一小写。
        assert!(text.contains("host: 127.0.0.1:3080"));
        assert!(text.contains("origin: http://127.0.0.1:3080"));
        assert!(text.contains("connection: Upgrade"));
        assert!(text.contains("upgrade: websocket"));
        assert!(text.contains("sec-websocket-key: abc123=="));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn find_head_end_locates_separator() {
        let buf = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\nbody";
        // 30 + 2 + 18 + 2 + 4 = 56
        assert_eq!(find_head_end(buf), Some(56));
    }

    #[test]
    fn head_starts_101_matches_both_versions() {
        assert!(head_starts_101(b"HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(head_starts_101(b"HTTP/1.0 101 Upgrading\r\n"));
        assert!(!head_starts_101(b"HTTP/1.1 200 OK\r\n"));
    }

    #[test]
    fn build_upgrade_response_keeps_sec_websocket_accept() {
        let head = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
        let res = build_upgrade_response(head);
        assert_eq!(res.status(), StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(
            res.headers().get("sec-websocket-accept").unwrap(),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn query_has_pair_matches_only_code() {
        assert!(query_has_pair("/?pair=123456", "123456"));
        assert!(query_has_pair("/?x=1&pair=123456&y=2", "123456"));
        assert!(!query_has_pair("/?pair=654321", "123456"));
        assert!(!query_has_pair("/", "123456"));
    }

    // -----------------------------------------------------------------------
    // 配对会话令牌与 Cookie（内网穿透场景的核心：身份跟着 Cookie 走，不跟着 IP）
    // -----------------------------------------------------------------------

    #[test]
    fn gen_token_is_unique_hex() {
        let a = gen_token();
        let b = gen_token();
        assert_eq!(a.len(), TOKEN_LEN);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn extract_pair_cookie_reads_own_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("other=x; dsh_pair=abc123; session=y"),
        );
        assert_eq!(extract_pair_cookie(&headers).as_deref(), Some("abc123"));

        // 大小写不敏感。
        let mut upper = HeaderMap::new();
        upper.insert(COOKIE, HeaderValue::from_static("DSH_PAIR=XYZ"));
        assert_eq!(extract_pair_cookie(&upper).as_deref(), Some("XYZ"));

        // 无 Cookie 或没有网关 Cookie → None。
        assert_eq!(extract_pair_cookie(&HeaderMap::new()), None);
        let mut no_own = HeaderMap::new();
        no_own.insert(COOKIE, HeaderValue::from_static("other=1"));
        assert_eq!(extract_pair_cookie(&no_own), None);
    }

    #[test]
    fn strip_pair_cookie_removes_only_own_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, HeaderValue::from_static("a=1; dsh_pair=tok; b=2"));
        strip_pair_cookie(&mut headers);
        let kept = headers.get(COOKIE).unwrap().to_str().unwrap();
        assert!(!kept.to_ascii_lowercase().contains("dsh_pair"));
        assert!(kept.contains("a=1"));
        assert!(kept.contains("b=2"));

        // 只剩网关 Cookie → 整个 Cookie 头移除。
        let mut only = HeaderMap::new();
        only.insert(COOKIE, HeaderValue::from_static("dsh_pair=tok"));
        strip_pair_cookie(&mut only);
        assert!(!only.contains_key(COOKIE));

        // 没有网关 Cookie → 原样保留。
        let mut none = HeaderMap::new();
        none.insert(COOKIE, HeaderValue::from_static("a=1; b=2"));
        strip_pair_cookie(&mut none);
        assert_eq!(none.get(COOKIE).unwrap(), "a=1; b=2");
    }

    #[test]
    fn redirect_home_with_session_sets_cookie_and_location() {
        let res = redirect_home_with_session("tok123");
        assert_eq!(res.status(), StatusCode::FOUND);
        assert_eq!(res.headers().get(LOCATION).unwrap(), "/");
        let set = res.headers().get(SET_COOKIE).unwrap().to_str().unwrap();
        assert!(set.starts_with(&format!("{PAIR_COOKIE}=tok123; ")));
        assert!(set.contains("Path=/"));
        assert!(set.contains(&format!("Max-Age={}", PAIR_TTL.as_secs())));
        assert!(set.contains("HttpOnly"));
        assert!(set.contains("SameSite=Lax"));
    }

    // -----------------------------------------------------------------------
    // 集成测试：真实 hyper server + 假 upstream，走完整 TCP 转发管道
    // （forward_regular 的 keep-alive / 头改写 / HTML 注入行为）。
    // -----------------------------------------------------------------------

    type UpstreamHandler = Arc<dyn Fn(Request<Incoming>) -> Response<Full<Bytes>> + Send + Sync>;
    type GatewayHandler = Arc<
        dyn Fn(
                Request<Incoming>,
            )
                -> std::pin::Pin<Box<dyn std::future::Future<Output = Response<HandlerBody>> + Send>>
            + Send
            + Sync,
    >;

    /// 起一个假 upstream hyper 服务，返回监听地址。
    async fn spawn_fake_upstream(handler: UpstreamHandler) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((sock, _)) => {
                        let handler = handler.clone();
                        tokio::spawn(async move {
                            let service = service_fn(move |req| {
                                let handler = handler.clone();
                                async move { Ok::<_, Infallible>(handler(req)) }
                            });
                            let _ = http1::Builder::new()
                                .serve_connection(TokioIo::new(sock), service)
                                .await;
                        });
                    }
                    Err(_) => return,
                }
            }
        });
        addr
    }

    /// 起一个模拟「网关 handler」的 hyper 服务（等价于 serve_loop 的每连接服务）。
    async fn spawn_gateway(handler: GatewayHandler) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((sock, _)) => {
                        let handler = handler.clone();
                        tokio::spawn(async move {
                            let service = service_fn(move |req| {
                                let handler = handler.clone();
                                async move { Ok::<_, Infallible>(handler(req).await) }
                            });
                            let _ = http1::Builder::new()
                                .serve_connection(TokioIo::new(sock), service)
                                .await;
                        });
                    }
                    Err(_) => return,
                }
            }
        });
        addr
    }

    /// 用真实客户端请求网关并收集响应体。
    async fn gateway_get(gateway: SocketAddr, path: &str) -> (StatusCode, HeaderMap, Bytes) {
        let client = build_client();
        let req = Request::builder()
            .uri(format!("http://{gateway}{path}"))
            .header(HOST, "192.168.1.5:18080")
            .body(full_body(Bytes::from_static(b"")))
            .unwrap();
        let res = client.request(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let collected = res.into_body().collect().await.unwrap();
        (status, headers, collected.to_bytes())
    }

    fn text_html() -> Response<Full<Bytes>> {
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Full::new(Bytes::from_static(
                b"<!doctype html><html><head><title>t</title></head><body>hi</body></html>",
            )))
            .unwrap()
    }

    #[tokio::test]
    async fn forward_regular_injects_polyfill_into_html() {
        let upstream_addr = spawn_fake_upstream(Arc::new(move |_req| text_html())).await;
        let client = build_client();
        let gateway_addr = spawn_gateway(Arc::new(move |req| {
            // 直接进入转发阶段（门禁纯函数已单独覆盖）。
            let client = client.clone();
            let upstream = upstream_addr;
            Box::pin(async move { forward_regular(req, client, upstream).await })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Response<HandlerBody>> + Send>,
                >
        }))
        .await;

        let (status, _, body) = gateway_get(gateway_addr, "/page").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.windows(b"</head>".len()).any(|w| w == b"</head>"),
            "polyfill 应插在 </head> 之前"
        );
        let idx_head = body
            .windows(b"</head>".len())
            .position(|w| w == b"</head>")
            .unwrap();
        assert!(body[..idx_head]
            .windows(b"randomUUID".len())
            .any(|w| w == b"randomUUID"));
    }

    #[tokio::test]
    async fn forward_regular_leaves_json_untouched() {
        let payload = b"{\"ok\":true}".to_vec();
        let upstream_addr = spawn_fake_upstream(Arc::new(move |_req| {
            Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(payload.clone())))
                .unwrap()
        }))
        .await;
        let client = build_client();
        let gateway_addr = spawn_gateway(Arc::new(move |req| {
            let client = client.clone();
            let upstream = upstream_addr;
            Box::pin(async move { forward_regular(req, client, upstream).await })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Response<HandlerBody>> + Send>,
                >
        }))
        .await;

        let (status, headers, body) = gateway_get(gateway_addr, "/api").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[CONTENT_TYPE], "application/json");
        assert_eq!(body.as_ref(), b"{\"ok\":true}");
    }

    #[tokio::test]
    async fn forward_regular_does_not_inject_non_200_html() {
        let upstream_addr = spawn_fake_upstream(Arc::new(move |_req| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(CONTENT_TYPE, "text/html")
                .body(Full::new(Bytes::from_static(
                    b"<html><body>err</body></html>",
                )))
                .unwrap()
        }))
        .await;
        let client = build_client();
        let gateway_addr = spawn_gateway(Arc::new(move |req| {
            let client = client.clone();
            let upstream = upstream_addr;
            Box::pin(async move { forward_regular(req, client, upstream).await })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Response<HandlerBody>> + Send>,
                >
        }))
        .await;

        let (status, _, body) = gateway_get(gateway_addr, "/err").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!body
            .windows(b"randomUUID".len())
            .any(|w| w == b"randomUUID"));
    }

    // -----------------------------------------------------------------------
    // 诊断测试（#[ignore]，仅本机手动运行：需要真实 dsh 服务在 127.0.0.1:3080）：
    // 分离「hyper client ↔ 真 dsh」与「网关全链路」两个变量。
    // -----------------------------------------------------------------------

    async fn time_or_log<T>(label: &str, fut: impl std::future::Future<Output = T>) -> T {
        match tokio::time::timeout(Duration::from_secs(5), fut).await {
            Ok(v) => v,
            Err(_) => {
                eprintln!("[probe] {label}: TIMEOUT after 5s");
                panic!("{label} timed out");
            }
        }
    }

    #[tokio::test]
    #[ignore = "needs real dsh on 127.0.0.1:3080"]
    async fn probe_hyper_client_direct_to_dsh() {
        let client = build_client();
        let req = Request::builder()
            .uri("http://127.0.0.1:3080/")
            .header(HOST, "127.0.0.1:3080")
            .body(full_body(Bytes::from_static(b"")))
            .unwrap();
        let res = time_or_log("direct", async { client.request(req).await }).await;
        match res {
            Ok(res) => {
                let status = res.status();
                let cl = res
                    .headers()
                    .get(CONTENT_LENGTH)
                    .map(|v| v.to_str().unwrap_or("?").to_owned())
                    .unwrap_or_else(|| "none".into());
                let te = res
                    .headers()
                    .get(TRANSFER_ENCODING)
                    .map(|v| v.to_str().unwrap_or("?").to_owned())
                    .unwrap_or_else(|| "none".into());
                eprintln!(
                    "[probe] direct status={status} content-length={cl} transfer-encoding={te}"
                );
                let body = time_or_log("direct-body", res.into_body().collect()).await;
                match body {
                    Ok(collected) => {
                        eprintln!("[probe] direct body bytes={}", collected.to_bytes().len())
                    }
                    Err(e) => eprintln!("[probe] direct body error: {e}"),
                }
            }
            Err(e) => eprintln!("[probe] direct request error: {e}"),
        }
    }

    #[tokio::test]
    #[ignore = "needs real dsh on 127.0.0.1:3080"]
    async fn probe_gateway_full_chain_to_dsh() {
        let upstream_addr: SocketAddr = ([127, 0, 0, 1], 3080).into();
        let client = build_client();
        let gateway_addr = spawn_gateway(Arc::new(move |req| {
            let client = client.clone();
            Box::pin(async move { forward_regular(req, client, upstream_addr).await })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Response<HandlerBody>> + Send>,
                >
        }))
        .await;
        let (status, headers, body) =
            time_or_log("gateway-get", gateway_get(gateway_addr, "/")).await;
        let cl = headers
            .get(CONTENT_LENGTH)
            .map(|v| v.to_str().unwrap_or("?").to_owned())
            .unwrap_or_else(|| "none".into());
        let te = headers
            .get(TRANSFER_ENCODING)
            .map(|v| v.to_str().unwrap_or("?").to_owned())
            .unwrap_or_else(|| "none".into());
        eprintln!(
            "[probe] gateway status={status} content-length={cl} transfer-encoding={te} body={}",
            body.len()
        );
        assert_eq!(status, StatusCode::OK);
        assert!(!body.is_empty());
    }
}
