//! 配对网关使用的纯函数改写工具：配对码解析、WebSocket 升级识别、
//! Host/Origin 重写、hop-by-hop 头剥离、HTML polyfill 与 isLoopback 改写、
//! 配对会话 Cookie 的提取与剥离。

use hyper::header::{
    HeaderMap, HeaderName, HeaderValue, CONNECTION, CONTENT_LENGTH, COOKIE, HOST, ORIGIN,
    TRANSFER_ENCODING, UPGRADE,
};

/// 配对会话 Cookie 名：配对成功后网关签发，浏览器凭它通过门禁。
/// 身份跟着 Cookie 走而不是 IP——内网穿透隧道（localhost.run 等）会把所有
/// 流量折叠成本机 127.0.0.1，按 IP 放行会让任意拿到隧道网址的人绕过配对。
pub(crate) const PAIR_COOKIE: &str = "dsh_pair";

/// 从请求头提取配对会话令牌（Cookie 名大小写不敏感）。
pub(crate) fn extract_pair_cookie(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case(PAIR_COOKIE)
            .then(|| v.trim().to_string())
    })
}

/// 转发给上游前剥离网关自己的配对会话 Cookie（网关身份不泄露给上游；
/// 上游是 127.0.0.1:3080 的 Harness，不认识也不该看到这个 Cookie）。
pub(crate) fn strip_pair_cookie(headers: &mut HeaderMap) {
    let Some(cookie) = headers.get(COOKIE).and_then(|v| v.to_str().ok()) else {
        return;
    };
    let mut changed = false;
    let kept: Vec<&str> = cookie
        .split(';')
        .map(str::trim)
        .filter(|part| {
            let is_ours = part
                .split_once('=')
                .map(|(k, _)| k.trim().eq_ignore_ascii_case(PAIR_COOKIE))
                .unwrap_or(false);
            if is_ours {
                changed = true;
            }
            !is_ours
        })
        .collect();
    if !changed {
        return; // 未命中网关 Cookie，原样保留
    }
    if kept.is_empty() {
        headers.remove(COOKIE);
    } else if let Ok(v) = HeaderValue::from_str(&kept.join("; ")) {
        headers.insert(COOKIE, v);
    }
}

pub(crate) fn query_has_pair(target: &str, code: &str) -> bool {
    let Some(query) = target.split_once('?').map(|(_, q)| q) else {
        return false;
    };
    query.split('&').any(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        k == "pair" && v == code
    })
}

/// 是否为 WebSocket（或任意 Connection: upgrade）升级请求。
pub(crate) fn is_upgrade_request(headers: &HeaderMap) -> bool {
    let connection_upgrade = headers.get_all(CONNECTION).iter().any(|value| {
        value
            .to_str()
            .unwrap_or("")
            .to_ascii_lowercase()
            .split(',')
            .any(|part| part.trim() == "upgrade")
    });
    headers.contains_key(UPGRADE) && connection_upgrade
}

/// 把 Host/Origin 改写为 loopback authority（通过 trusted-hosts 栅栏：
/// 栅栏要求 Host 为 loopback/可信，且浏览器带的 Origin.host === Host.host；
/// POST/JSON 与 WS 握手一定带 Origin，只改 Host 会 403）。
pub(crate) fn rewrite_loopback(headers: &mut HeaderMap) {
    headers.insert(HOST, HeaderValue::from_static("127.0.0.1:3080"));
    if headers.contains_key(ORIGIN) {
        headers.insert(ORIGIN, HeaderValue::from_static("http://127.0.0.1:3080"));
    }
}

/// 剥离 hop-by-hop 头：连接级语义由 hyper 管理，残留的 Connection/Transfer-Encoding
/// 会与 hyper 的分帧冲突。upgrade 请求不走此函数（Upgrade/Connection 必须保留）。
pub(crate) fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

pub(crate) fn is_framing_header(name: &HeaderName) -> bool {
    name == CONTENT_LENGTH
        || name == TRANSFER_ENCODING
        || name == CONNECTION
        || name == "keep-alive"
}

/// `dsh-client-connection` 客户端 bundle 里 `connection.isLoopback` 的判定表达式
/// （页面 URL hostname 是否 loopback）。网关把它替换为恒 true，让「经网关访问的
/// 页面」（手机/局域网）获得与桌面本机一致的主机语义：内测声明确认落盘、设置域
/// 编辑器持久化、设置文档可用、宿主侧打开产物文件可用。
///
/// 该替换只作用于经网关转发的 bundle 副本；桌面直连 127.0.0.1:3080 不受影响。
const IS_LOOPBACK_EVAL: &str =
    "isLoopback: pageLocation === void 0 || isLoopbackHostname(pageLocation.hostname)";
const IS_LOOPBACK_TRUE: &str = "isLoopback: true";

/// 改写 connection bundle：命中目标表达式则返回改写后的字节；未命中返回 None，
/// 由调用方决定回退与告警（升级导致 bundle 形态变化时静默降级）。
pub(crate) fn rewrite_connection_bundle(bytes: &[u8]) -> Option<Vec<u8>> {
    let source = std::str::from_utf8(bytes).ok()?;
    if !source.contains(IS_LOOPBACK_EVAL) {
        return None;
    }
    Some(
        source
            .replace(IS_LOOPBACK_EVAL, IS_LOOPBACK_TRUE)
            .into_bytes(),
    )
}

// ---------------------------------------------------------------------------
// 普通请求转发（hyper client）
// ---------------------------------------------------------------------------

pub(crate) const POLYFILL: &str = r#"<script>(function(){try{if(globalThis.crypto && !globalThis.crypto.randomUUID && globalThis.crypto.getRandomValues){var b = new Uint8Array(16);globalThis.crypto.randomUUID = function(){crypto.getRandomValues(b);b[6] = (b[6] & 15) | 64;b[8] = (b[8] & 63) | 128;var h = Array.from(b, function(x){ return x.toString(16).padStart(2, "0"); }).join("");return h.slice(0, 8) + "-" + h.slice(8, 12) + "-" + h.slice(12, 16) + "-" + h.slice(16, 20) + "-" + h.slice(20);};}}catch(e){}})();</script>"#;

/// 把 polyfill 插入 HTML：优先 `</head>` 前，其次 `<html …>` 之后，否则最前。
pub(crate) fn inject_html_polyfills(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + POLYFILL.len());
    if let Some(i) = find_case_insensitive(body, b"</head") {
        out.extend_from_slice(&body[..i]);
        out.extend_from_slice(POLYFILL.as_bytes());
        out.extend_from_slice(&body[i..]);
        return out;
    }
    if let Some(i) = find_case_insensitive(body, b"<html") {
        if let Some(gt) = find_bytes(&body[i..], b">") {
            let at = i + gt + 1;
            out.extend_from_slice(&body[..at]);
            out.extend_from_slice(POLYFILL.as_bytes());
            out.extend_from_slice(&body[at..]);
            return out;
        }
    }
    out.extend_from_slice(POLYFILL.as_bytes());
    out.extend_from_slice(body);
    out
}

/// 找第一个匹配子串的偏移（大小写不敏感）。
fn find_case_insensitive(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}
