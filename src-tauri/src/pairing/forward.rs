//! 配对网关的普通 HTTP 转发：hyper legacy client、转发请求构造、
//! 响应体收集与新头构造（含 HTML polyfill 注入时机判定）。

use super::*;

use super::rewrite::{
    inject_html_polyfills, is_framing_header, rewrite_connection_bundle, rewrite_loopback,
    strip_hop_by_hop, strip_pair_cookie,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{HeaderMap, CONTENT_TYPE};
use hyper::{Method, Request, Response, StatusCode, Uri, Version};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as LegacyClient;
use hyper_util::rt::TokioExecutor;

pub(crate) fn build_client() -> UpstreamClient {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(Duration::from_secs(3)));
    LegacyClient::builder(TokioExecutor::new()).build(connector)
}

/// 网关注入循环：接受连接，每连接跑一个 hyper HTTP/1.1 服务。
fn build_forward_request(
    method: Method,
    uri_path: &str,
    version: Version,
    mut headers: HeaderMap,
    body: Incoming,
    upstream: SocketAddr,
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
    let is_connection_bundle = status == StatusCode::OK
        && uri_path.contains("/dsh-client-connection/client.js")
        && content_type.contains("javascript");

    // 剥离 framing 头后由 hyper server 重算（注入/改写路径 body 大小会变化；
    // 流式路径避免上游 Connection 头与 hyper 分帧冲突）。
    let mut resp = Response::builder().status(status);
    for (name, value) in response.headers() {
        if !is_framing_header(name) {
            resp = resp.header(name, value);
        }
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
        let bytes = match collect_limited(response.into_body(), HTML_BODY_MAX).await {
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
