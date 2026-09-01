//! 配对网关请求管线的单元测试。
//!
//! 这些用例原本内联在 `pairing/mod.rs` 的 `#[cfg(test)] mod tests` 里，
//! 覆盖 rewrite / forward / tunnel 三个子模块的纯函数与转发行为。
//! 二次拆分时 `mod.rs` 被重写，本文件从 git 历史中**原样恢复**这些用例，
//! 避免丢失既有覆盖；仅在 import 上适配了拆分后的模块位置。
//!
//! 底部的 `probe_*` 用例是 `#[ignore]` 的手动诊断测试（需要真实 dsh 跑在
//! 127.0.0.1:3080），CI 不会执行。

use super::forward::forward_regular;
use super::rewrite::{
    extract_pair_cookie, inject_html_polyfills, is_upgrade_request, query_has_pair,
    rewrite_connection_bundle, rewrite_loopback, strip_hop_by_hop, strip_pair_cookie, POLYFILL,
};
use super::tunnel::{
    build_raw_request_head, build_upgrade_response, find_head_end, head_starts_101,
};
use super::upstream::{exchange, inject_auth_cookie, pick_auth_cookie};
use super::*;
// 以下符号在 pairing 二次拆分后移入了各自子模块，需显式引入
// （`super::*` 已带不到它们）。
use super::http::redirect_home_with_session;
use super::token::{gen_token, TOKEN_LEN};
// mod.rs 自身不再直接导入这些（拆分后只在子模块里用），测试里需要显式引入
use super::forward::full_body;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use std::io::{Read, Write};
use hyper::header::{
    HeaderMap, CONNECTION, CONTENT_LENGTH, COOKIE, HOST, ORIGIN, TRANSFER_ENCODING, UPGRADE,
};
use hyper::header::{HeaderValue, CONTENT_TYPE, LOCATION, SET_COOKIE};
use hyper::header::{ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, ETAG};
use std::sync::Mutex;
use hyper::Method;
use hyper::StatusCode;
use rewrite::PAIR_COOKIE;

#[test]
fn rewrite_connection_bundle_turns_is_loopback_true() {
    // dsh 0.1.2-alpha.3 的实际文本：判定表达式前面还多了一段 ownsHost。
    let sample = b"const handle = {\n\t\t\tisLoopback: transport?.ownsHost === true || pageLocation === void 0 || isLoopbackHostname(pageLocation.hostname),\n\t\t\tgeneration: {}\n\t\t};";
    let rewritten = rewrite_connection_bundle(sample).expect("pattern must match");
    let text = String::from_utf8(rewritten).unwrap();
    assert!(text.contains("isLoopback: transport?.ownsHost === true || true,"));
    assert!(!text.contains("isLoopbackHostname(pageLocation.hostname)"));
}

/// 早期 dsh 没有 `transport?.ownsHost === true ||` 这一段，同样要能改写——
/// 匹配串取的是旧串的子串，两个版本都命中。
#[test]
fn rewrite_connection_bundle_supports_older_wording() {
    let sample = b"const handle = { api, isLoopback: pageLocation === void 0 || isLoopbackHostname(pageLocation.hostname), hostDescription: {} };";
    let text = String::from_utf8(rewrite_connection_bundle(sample).unwrap()).unwrap();
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
        Box::pin(async move { forward_regular(req, client, upstream, None).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Response<HandlerBody>> + Send>>
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
        Box::pin(async move { forward_regular(req, client, upstream, None).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Response<HandlerBody>> + Send>>
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
        Box::pin(async move { forward_regular(req, client, upstream, None).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Response<HandlerBody>> + Send>>
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
            eprintln!("[probe] direct status={status} content-length={cl} transfer-encoding={te}");
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
        Box::pin(async move { forward_regular(req, client, upstream_addr, None).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Response<HandlerBody>> + Send>>
    }))
    .await;
    let (status, headers, body) = time_or_log("gateway-get", gateway_get(gateway_addr, "/")).await;
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

/// 新建的网关处于停止态、端口为 0、会话为空。
#[test]
fn fresh_pairing_is_stopped() {
    let p = Pairing::new();
    assert!(!p.running);
    assert_eq!(p.port, 0);
    assert!(p.sessions.is_empty());
    assert!(p.error.is_none());
    // 上游会话尚未代持。
    assert!(p.upstream_cookie.is_none());
    assert!(p.upstream_cookie_token.is_none());
    assert_eq!(p.code.len(), 6);
    assert!(p.code.chars().all(|c| c.is_ascii_digit()));
}

// -----------------------------------------------------------------------
// 上游会话代持：dsh 0.1.2+ 要求 /api 带会话 cookie，由网关用启动令牌换取代持
// -----------------------------------------------------------------------

/// 只认 dsh 的会话 cookie，且只保留 `name=value`（属性一概丢掉）。
#[test]
fn pick_auth_cookie_keeps_only_the_pair() {
    let raw = "dsh-auth-abc123=v1.payload.sig; Path=/; HttpOnly; SameSite=Strict";
    assert_eq!(
        pick_auth_cookie(raw).as_deref(),
        Some("dsh-auth-abc123=v1.payload.sig")
    );
    // 网关自己的 dsh_pair 不能被当成上游会话。
    assert_eq!(pick_auth_cookie("dsh_pair=deadbeef; Path=/; HttpOnly"), None);
    // 空值（注销型 cookie）不是有效会话。
    assert_eq!(pick_auth_cookie("dsh-auth-abc=; Path=/"), None);
}

/// 注入时保留浏览器自己的 cookie，替换掉同前缀的旧值。
#[test]
fn inject_auth_cookie_merges_without_clobbering_others() {
    let mut headers = HeaderMap::new();
    headers.insert(
        COOKIE,
        HeaderValue::from_static("theme=dark; dsh-auth-old=stale"),
    );
    inject_auth_cookie(&mut headers, "dsh-auth-new=fresh");
    let cookie = headers.get(COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(cookie.contains("theme=dark"), "不能吃掉浏览器自己的: {cookie}");
    assert!(
        !cookie.contains("dsh-auth-old=stale"),
        "同前缀的旧值必须被替换: {cookie}"
    );
    assert!(cookie.ends_with("dsh-auth-new=fresh"), "cookie: {cookie}");
}

/// 请求本来没有 Cookie 头时也能注入。
#[test]
fn inject_auth_cookie_works_without_existing_cookie() {
    let mut headers = HeaderMap::new();
    inject_auth_cookie(&mut headers, "dsh-auth-new=fresh");
    assert_eq!(headers.get(COOKIE).unwrap(), "dsh-auth-new=fresh");
}

/// 令牌交换：网关必须带令牌、以 loopback authority 去换，并解析回 cookie。
#[test]
fn exchange_trades_token_for_session_cookie() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 2048];
        let n = sock.read(&mut buf).unwrap();
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(req.starts_with("GET /?token=tok-abc HTTP/1.1\r\n"), "req: {req}");
        assert!(
            req.to_ascii_lowercase()
                .contains(&format!("host: {addr}")),
            "Host 必须是 loopback authority（上游据此算 cookie 名）: {req}"
        );
        sock.write_all(
            b"HTTP/1.1 303 See Other\r\nlocation: /\r\nset-cookie: dsh-auth-xyz=v1.p.s; Path=/; HttpOnly; SameSite=Strict\r\ncontent-length: 0\r\n\r\n",
        )
        .unwrap();
    });
    let cookie = exchange(addr, "tok-abc");
    server.join().unwrap();
    assert_eq!(cookie.as_deref(), Some("dsh-auth-xyz=v1.p.s"));
}

/// 上游不接受令牌（401，无 Set-Cookie）时不能凭空造出一枚 cookie。
#[test]
fn exchange_returns_none_when_upstream_refuses() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 2048];
        let _ = sock.read(&mut buf).unwrap();
        sock.write_all(
            b"HTTP/1.1 401 Unauthorized\r\ncontent-type: text/plain\r\ncontent-length: 0\r\n\r\n",
        )
        .unwrap();
    });
    let cookie = exchange(addr, "wrong-token");
    server.join().unwrap();
    assert_eq!(cookie, None);
}

/// 上游没启动（端口无监听）是常态，不能 panic。
#[test]
fn exchange_returns_none_when_upstream_is_down() {
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = dead.local_addr().unwrap();
    drop(dead);
    assert_eq!(exchange(addr, "tok"), None);
}

// -----------------------------------------------------------------------
// 压缩与改写：网关改过 body 的响应不能再声称自己是压缩的
// -----------------------------------------------------------------------

/// 网关会往 HTML 里注入 polyfill，所以上游一旦压缩，浏览器就会拿到
/// 「gzip 数据 + 明文脚本」而报 `ERR_CONTENT_DECODING_FAILED`。两道防线：
/// 请求侧显式声明只接受 identity，且改写过的响应一律剥掉 content-encoding。
#[tokio::test]
async fn rewritten_html_never_claims_compression() {
    let seen: Arc<Mutex<Option<HeaderMap>>> = Arc::new(Mutex::new(None));
    let recorder = seen.clone();
    let upstream_addr = spawn_fake_upstream(Arc::new(move |req| {
        *recorder.lock().unwrap() = Some(req.headers().clone());
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "text/html; charset=utf-8")
            .header(CONTENT_ENCODING, "gzip")
            .header(ETAG, "\"abc\"")
            .body(Full::new(Bytes::from_static(
                b"<!doctype html><html><head><title>t</title></head><body>hi</body></html>",
            )))
            .unwrap()
    }))
    .await;
    let client = build_client();
    let gateway_addr = spawn_gateway(Arc::new(move |req| {
        let client = client.clone();
        let upstream = upstream_addr;
        Box::pin(async move { forward_regular(req, client, upstream, None).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Response<HandlerBody>> + Send>>
    }))
    .await;

    let (status, headers, body) = gateway_get(gateway_addr, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers.get(CONTENT_ENCODING).is_none(),
        "改写过的 HTML 不能带 content-encoding: {headers:?}"
    );
    assert!(
        headers.get(ETAG).is_none(),
        "改写过的 HTML 不能沿用原 etag: {headers:?}"
    );
    assert_eq!(
        headers.get(CACHE_CONTROL).and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "改写过的响应不能被缓存（否则一直用改写前的旧副本）: {headers:?}"
    );
    assert!(body.windows(10).any(|w| w == b"randomUUID"));

    let sent = seen.lock().unwrap().clone().expect("上游应收到请求");
    assert_eq!(
        sent.get(ACCEPT_ENCODING).and_then(|v| v.to_str().ok()),
        Some("identity"),
        "必须显式声明 identity（缺这个头表示「任何编码都行」）: {sent:?}"
    );
}

/// 没改写的响应（body 原样转发）可以保留 content-encoding——不能一刀切地剥。
#[tokio::test]
async fn untouched_body_keeps_content_encoding() {
    let upstream_addr = spawn_fake_upstream(Arc::new(move |_req| {
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "application/json")
            .header(CONTENT_ENCODING, "gzip")
            .header(CACHE_CONTROL, "public, max-age=60")
            .body(Full::new(Bytes::from_static(b"compressed-bytes")))
            .unwrap()
    }))
    .await;
    let client = build_client();
    let gateway_addr = spawn_gateway(Arc::new(move |req| {
        let client = client.clone();
        let upstream = upstream_addr;
        Box::pin(async move { forward_regular(req, client, upstream, None).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Response<HandlerBody>> + Send>>
    }))
    .await;

    let (_, headers, body) = gateway_get(gateway_addr, "/api").await;
    assert_eq!(headers.get(CONTENT_ENCODING).unwrap(), "gzip");
    // 没改写的响应不该被插手缓存策略。
    assert_eq!(headers.get(CACHE_CONTROL).unwrap(), "public, max-age=60");
    assert_eq!(body.as_ref(), b"compressed-bytes");
}

/// 端口探测：BASE_PORT 起的 31 个端口里应能绑到一个；即便全部被占也只返回
/// Err 而不 panic。
#[test]
fn bind_free_never_panics() {
    match bind_free(BASE_PORT) {
        Ok((l, port)) => {
            assert!((BASE_PORT..=BASE_PORT + 30).contains(&port));
            // bind_free 内部已 set_nonblocking(true)（tokio from_std 的硬性要求）；
            // 这里再显式设一次并确认调用成功即可（查询非阻塞状态的方法仅 Unix 有）。
            assert!(
                l.set_nonblocking(true).is_ok(),
                "返回的 socket 应可设为非阻塞"
            );
        }
        Err(_) => { /* 端口全忙也是合法结果 */ }
    }
}
