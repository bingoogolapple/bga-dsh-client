//! 配对网关的普通 HTTP 转发：hyper legacy client、转发请求构造、
//! 响应体收集与新头构造（含 HTML polyfill 注入时机判定）。

use super::*;

use super::http::{bad_gateway_response, service_down_response};
use super::rewrite::{
    inject_html_polyfills, is_connection_bundle_response, is_framing_header,
    rewrite_connection_bundle, rewrite_loopback, strip_hop_by_hop, strip_pair_cookie,
};
use super::upstream::inject_auth_cookie;

/// 等待上游返回响应头（首字节）的超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// 200 text/html 注入 polyfill 时允许缓冲的最大响应体。
const HTML_BODY_MAX: usize = 8 * 1024 * 1024;
/// combo 响应是所有插件脚本的合并体，改写的缓冲上限比单个 HTML 宽一些
/// （超上限会退回 502，宁可多留些余量）。
const BUNDLE_BODY_MAX: usize = 32 * 1024 * 1024;
/// 连接池里空闲连接的存活上限，必须小于上游（Node）的 keepAliveTimeout（默认 5s）。
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{
    HeaderMap, HeaderValue, ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, ETAG,
};
use hyper::{Method, Request, Response, StatusCode, Uri, Version};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as LegacyClient;
use hyper_util::rt::TokioExecutor;

pub(crate) fn build_client() -> UpstreamClient {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(Duration::from_secs(3)));
    LegacyClient::builder(TokioExecutor::new())
        // 上游是 Node，其 keepAliveTimeout 默认只有 5 秒，远小于 hyper 池默认的
        // 90 秒空闲上限：池里缓存的连接会先被上游关掉，而 hyper 仍认为它可用，
        // 复用时就拿到一个已关闭的连接，表现为成片、随机的 503（页面加载并发建
        // 好几个连接，坏连接要连着失败几次才被耗尽）。把池的空闲上限压到它之下，
        // 宁可多握几次手——局域网里这点开销远小于一次失败。
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .build(connector)
}

/// 网关注入循环：接受连接，每连接跑一个 hyper HTTP/1.1 服务。
fn build_forward_request(
    method: Method,
    uri_path: &str,
    version: Version,
    mut headers: HeaderMap,
    body: Incoming,
    upstream: SocketAddr,
    cookie: Option<&str>,
) -> Result<Request<HandlerBody>, ()> {
    // 绝对形式 URI 指向 loopback：hyper_util 连接器据此连接目标端口，
    // 同时 Host 头由 rewrite_loopback 设定为 127.0.0.1:3080。
    let absolute: Uri = format!("http://{}:{}{uri_path}", upstream.ip(), upstream.port())
        .parse()
        .map_err(|_| ())?;
    strip_hop_by_hop(&mut headers);
    // 网关自己的配对会话 Cookie 不转发给上游（上游不需要也不该看到）。
    strip_pair_cookie(&mut headers);
    rewrite_loopback(&mut headers);
    // 网关代持的上游会话 Cookie：dsh 0.1.2+ 的 /api 认证（`upstream` 模块）。
    if let Some(cookie) = cookie {
        inject_auth_cookie(&mut headers, cookie);
    }
    // 只接受未压缩的响应：本网关会改写 HTML（polyfill）与 connection bundle，
    // 上游一旦压缩，改写后的 body 就与 Content-Encoding 对不上，浏览器会报
    // `ERR_CONTENT_DECODING_FAILED`。必须显式写 identity——按 RFC 7231，请求里
    // 没有 Accept-Encoding 反而表示「任何编码都可以接受」。
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    let body: HandlerBody = body.map_err(|e| Box::new(e) as BoxErr).boxed();
    let mut builder = Request::builder()
        .method(method)
        .uri(absolute)
        .version(version);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    builder.body(body).map_err(|_| ())
}

pub(crate) async fn forward_regular(
    req: Request<Incoming>,
    client: UpstreamClient,
    upstream: SocketAddr,
    cookie: Option<&str>,
) -> Response<HandlerBody> {
    let (parts, body) = req.into_parts();
    let uri_path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    let fwd = match build_forward_request(
        parts.method,
        &uri_path,
        parts.version,
        parts.headers,
        body,
        upstream,
        cookie,
    ) {
        Ok(fwd) => fwd,
        Err(()) => return bad_gateway_response(),
    };
    let response = match tokio::time::timeout(REQUEST_TIMEOUT, client.request(fwd)).await {
        Ok(Ok(response)) => response,
        // 上游连不上/请求错误：桌面端服务未运行或不可达。
        Ok(Err(_)) => return service_down_response(),
        Err(_) => return bad_gateway_response(),
    };
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_injectable = status == StatusCode::OK && content_type.contains("text/html");
    // dsh-client-connection 的客户端 bundle：改写 isLoopback 判定（见 rewrite_connection_bundle）。
    let is_connection_bundle =
        status == StatusCode::OK && is_connection_bundle_response(&uri_path, content_type);

    // 剥离 framing 头后由 hyper server 重算（注入/改写路径 body 大小会变化；
    // 流式路径避免上游 Connection 头与 hyper 分帧冲突）。
    // 改写过 body 的响应不能再声称自己是压缩的，也不该沿用原 etag（内容已经变了）——
    // 请求侧已声明只接受 identity，这里是兜底：上游若仍压缩，宁可丢掉这个头。
    let rewritten = is_injectable || is_connection_bundle;
    if rewritten {
        if let Some(encoding) = response.headers().get(CONTENT_ENCODING) {
            if encoding != "identity" {
                // 改写前必须得到明文；删除 Content-Encoding 头不能解压响应，
                // 否则浏览器会把 gzip/br 字节当 HTML/JS 解析。
                return bad_gateway_response();
            }
        }
    }
    let mut resp = Response::builder().status(status);
    for (name, value) in response.headers() {
        if is_framing_header(name) {
            continue;
        }
        if rewritten && (name == CONTENT_ENCODING || name == ETAG || name == CACHE_CONTROL) {
            continue;
        }
        resp = resp.header(name, value);
    }
    if rewritten {
        // 改写过的响应内容已经和原资源不一致：URL 没变、etag 又已剥掉，若再让它
        // 被强缓存，浏览器会一直用改写前的旧副本（表现为手机改好了、电脑还老样子）。
        resp = resp.header(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }

    if is_injectable {
        // 只有 200 text/html 才缓冲整个 body 注入 polyfill，限量防内存爆。
        let bytes = match collect_limited(response.into_body(), HTML_BODY_MAX).await {
            Ok(bytes) => bytes,
            Err(_) => return bad_gateway_response(),
        };
        let injected = inject_html_polyfills(&bytes);
        match resp.body(full_body(Bytes::from(injected))) {
            Ok(res) => res,
            Err(_) => bad_gateway_response(),
        }
    } else if is_connection_bundle {
        let bytes = match collect_limited(response.into_body(), BUNDLE_BODY_MAX).await {
            Ok(bytes) => bytes,
            Err(_) => return bad_gateway_response(),
        };
        match rewrite_connection_bundle(&bytes) {
            Some(rewritten) => match resp.body(full_body(Bytes::from(rewritten))) {
                Ok(res) => res,
                Err(_) => bad_gateway_response(),
            },
            None => match resp.body(full_body(bytes)) {
                Ok(res) => res,
                Err(_) => bad_gateway_response(),
            },
        }
    } else {
        let body: HandlerBody = response
            .into_body()
            .map_err(|e| Box::new(e) as BoxErr)
            .boxed();
        match resp.body(body) {
            Ok(res) => res,
            Err(_) => bad_gateway_response(),
        }
    }
}

/// 流式收集 body，超过上限即失败。
async fn collect_limited(mut body: Incoming, limit: usize) -> Result<Bytes, BoxErr> {
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| Box::new(e) as BoxErr)?;
        if let Some(data) = frame.data_ref() {
            out.extend_from_slice(data);
            if out.len() > limit {
                return Err(format!("response body exceeds {limit} bytes").into());
            }
        }
    }
    Ok(Bytes::from(out))
}

// ---------------------------------------------------------------------------
// WebSocket 升级（原始隧道）
// ---------------------------------------------------------------------------

/// 握手的准备结果：upstream 已同意升级（101），`extra` 是读头部时顺带读到的后续字节。
pub(crate) fn full_body(bytes: Bytes) -> HandlerBody {
    Full::new(bytes)
        .map_err(|never| -> BoxErr { match never {} })
        .boxed()
}

pub(crate) fn empty_body() -> HandlerBody {
    full_body(Bytes::new())
}
