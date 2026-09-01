//! 上游（127.0.0.1:3080）浏览器会话的代持。
//!
//! # 为什么需要它
//!
//! dsh 0.1.2 起，Web 接口的 index 与 `/api` 都要求一枚绑定 authority 的会话
//! cookie（`dsh-auth-<hash(authority)>`），它只能由 `GET /?token=<启动令牌>`
//! 换得，默认 30 天有效、跨 dsh 重启仍有效，但**令牌本身每个进程一变**。
//!
//! 手机访问的是网关（`http://<lan-ip>:18080`），浏览器的 cookie 属于 18080 这个
//! 站点：既不会自动带上 dsh 的 cookie，也从未跟 3080 交换过，直接转发必然 401。
//! 而网关已经把 Host/Origin 改写成了 loopback（`rewrite_loopback`），上游按请求
//! Host 算出的 cookie 名恒为 `dsh-auth-<hash("127.0.0.1:3080")>`——所以网关只要
//! **代持**一枚以 loopback authority 换来的 cookie、转发时注入即可，浏览器侧
//! 不需要任何配合，也因此不受 `SameSite` 与第三方 cookie 策略影响。
//!
//! 令牌来自 `service.log` 里那行 `dsh web: …?token=…`（`service::parse_launch_token`
//! 解析后存在 `AppState.dsh_token`）。外部启动的服务 stdout 不进该日志，此时本模块
//! 无能为力，网关只能把上游的 401 透传（日志会写明原因）。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hyper::header::{HeaderMap, HeaderValue, COOKIE};
use tauri::{AppHandle, Manager};

use super::push_log;
use crate::i18n::tr;
use crate::AppState;

/// 一次交换的连接/读写超时（loopback，正常在毫秒级）。
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);
/// 响应头上限：交换只需要读到 `Set-Cookie`，多余的一律不要。
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// dsh 会话 cookie 名前缀（`cookieName()` = `dsh-auth-<b64url(sha256(authority))>`）。
const AUTH_COOKIE_PREFIX: &str = "dsh-auth-";
/// 同一条失败原因的日志节流间隔（秒）：拿不到会话时每个请求都会走一遍，
/// 但不能每个请求都刷一条日志。
const WARN_INTERVAL_SECS: u64 = 60;
/// 上次记失败日志的 Unix 秒（节流用）。
static LAST_WARN: AtomicU64 = AtomicU64::new(0);

/// 节流地记一条失败原因：网关拿不到会话时用户只能靠 pairing.log 判断。
fn warn_throttled(app: &AppHandle, key: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_WARN.load(Ordering::SeqCst);
    if now.saturating_sub(last) < WARN_INTERVAL_SECS {
        return;
    }
    LAST_WARN.store(now, Ordering::SeqCst);
    push_log(app, tr(crate::i18n::current(app), key, &[]));
}

/// 从 `Set-Cookie` 里挑出 dsh 的会话 cookie，只保留 `name=value`（去掉属性）。
pub(crate) fn pick_auth_cookie(set_cookie: &str) -> Option<String> {
    let pair = set_cookie.split(';').next()?.trim();
    let (name, value) = pair.split_once('=')?;
    if !name.trim().starts_with(AUTH_COOKIE_PREFIX) || value.trim().is_empty() {
        return None;
    }
    Some(pair.to_string())
}

/// 把代持的 cookie 并进请求头：保留浏览器自己的 cookie，替换掉同前缀的旧值。
pub(crate) fn inject_auth_cookie(headers: &mut HeaderMap, cookie: &str) {
    let mut parts: Vec<String> = headers
        .get(COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .filter(|part| !part.starts_with(AUTH_COOKIE_PREFIX))
        .map(str::to_string)
        .collect();
    parts.push(cookie.to_string());
    if let Ok(value) = HeaderValue::from_str(&parts.join("; ")) {
        headers.insert(COOKIE, value);
    }
}

/// 响应头结束的位置（`\r\n\r\n` 之后）。
fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| at + 4)
}

/// 用启动令牌向上游换一枚会话 cookie（原始 HTTP/1.1，不跟随 303）。
pub(crate) fn exchange(upstream: SocketAddr, token: &str) -> Option<String> {
    let mut stream = TcpStream::connect_timeout(&upstream, EXCHANGE_TIMEOUT).ok()?;
    let _ = stream.set_read_timeout(Some(EXCHANGE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(EXCHANGE_TIMEOUT));
    let request = format!(
        "GET /?token={token} HTTP/1.1\r\n\
         Host: {upstream}\r\n\
         Connection: close\r\n\
         User-Agent: dsh-lan-gateway\r\n\
         \r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1024];
    loop {
        if head_end(&buf).is_some() || buf.len() >= MAX_HEAD_BYTES {
            break;
        }
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    // 头名大小写不敏感，cookie 值却区分大小写：只把名字那一段拿去比较。
    let head = String::from_utf8_lossy(&buf);
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("set-cookie") {
            continue;
        }
        if let Some(cookie) = pick_auth_cookie(value.trim()) {
            return Some(cookie);
        }
    }
    None
}

/// 取当前代持的会话 cookie；没有、或令牌换过就现换一枚。
pub(crate) fn ensure_cookie(app: &AppHandle, upstream: SocketAddr) -> Option<String> {
    let state = app.state::<AppState>();
    let token = crate::state::lock(&state.dsh_token).clone();
    {
        let p = crate::state::lock(&state.pairing);
        if let Some(cookie) = p.upstream_cookie.clone() {
            // 令牌没换过（或本次压根拿不到令牌）就复用：cookie 默认 30 天有效，
            // 且跨 dsh 重启依然有效——只有它自己过期才会走到下面的重新交换。
            if token.is_none() || p.upstream_cookie_token.as_deref() == token.as_deref() {
                return Some(cookie);
            }
        }
    }
    let Some(token) = token else {
        // 拿不到启动令牌：服务多半由外部启动，其 stdout 不进本应用的日志。
        warn_throttled(app, "pair.upstream_fail_log");
        return None;
    };
    let Some(cookie) = exchange(upstream, &token) else {
        // 有令牌但交换失败：上游不接受它（令牌属于已退出的进程）或服务不可达。
        warn_throttled(app, "pair.upstream_exchange_fail_log");
        return None;
    };
    let mut p = crate::state::lock(&state.pairing);
    p.upstream_cookie = Some(cookie.clone());
    p.upstream_cookie_token = Some(token);
    Some(cookie)
}

/// 作废代持的会话（上游返回 401 时）：下次请求会重新交换。
pub(crate) fn invalidate(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut p = crate::state::lock(&state.pairing);
    p.upstream_cookie = None;
    p.upstream_cookie_token = None;
}
