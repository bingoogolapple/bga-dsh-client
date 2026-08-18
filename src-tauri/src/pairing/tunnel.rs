//! 配对网关的 WebSocket 隧道：升级握手准备、原始请求头构造、
//! 101 响应构造、升级后的双向字节流转发。

use super::forward::empty_body;
use super::*;

use hyper::header::{HeaderMap, HeaderValue};
use hyper::upgrade::Upgraded;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

struct PreparedUpgrade {
    upstream_io: tokio::net::TcpStream,
    /// 101 响应头之后的预读字节（可能是响应 body 或 WS 协议帧），隧道建立后先转发。
    extra: Vec<u8>,
    /// upstream 的 101 响应头原文，用于给手机构造 101 响应（含 Sec-WebSocket-Accept）。
    resp_head: Vec<u8>,
    is_101: bool,
}

/// 处理 WebSocket 升级请求。顺序很重要：
/// 1. 先连 upstream、转发改写后的握手请求、读响应头——确认 upstream 同意升级；
/// 2. 只有拿到 upstream 的 101 后才给手机回 101（浏览器会校验 Sec-WebSocket-Accept，
///    该值必须以手机原始 Sec-WebSocket-Key 计算，原样转发 upstream 响应最稳妥）；
/// 3. 升级握手由 hyper 完成（on()），之后在独立任务里做双向原始拷贝。
pub(crate) async fn handle_upgrade(
    mut req: Request<Incoming>,
    upstream: SocketAddr,
) -> Response<HandlerBody> {
    let method = req.method().clone();
    let uri_path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    let mut headers = req.headers().clone();
    // upgrade 请求保留 Connection/Upgrade/Sec-WebSocket-* 头，只改 Host/Origin，
    // 并剥离网关自己的配对会话 Cookie（不把网关身份泄露给上游）。
    rewrite_loopback(&mut headers);
    super::rewrite::strip_pair_cookie(&mut headers);

    let prepared = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        prepare_upgrade_handshake(method, &uri_path, &headers, upstream),
    )
    .await
    {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(())) | Err(_) => return service_down_response(),
    };
    if !prepared.is_101 {
        drop(prepared);
        return bad_gateway_response();
    }

    // hyper 完成升级后才 resolve on()：必须 spawn，不能先等待再返回 101 响应。
    let resp_head = prepared.resp_head.clone();
    tokio::spawn(async move {
        let client_io = match hyper::upgrade::on(&mut req).await {
            Ok(io) => io,
            Err(_) => return,
        };
        tunnel_after_upgrade(client_io, prepared).await;
    });

    build_upgrade_response(&resp_head)
}

async fn prepare_upgrade_handshake(
    method: Method,
    uri_path: &str,
    headers: &HeaderMap,
    upstream: SocketAddr,
) -> Result<PreparedUpgrade, ()> {
    let mut upstream_io = tokio::net::TcpStream::connect(upstream)
        .await
        .map_err(|_| ())?;
    let head = build_raw_request_head(&method, uri_path, headers);
    let _ = upstream_io.set_nodelay(true);
    use tokio::io::AsyncWriteExt;
    if upstream_io.write_all(&head).await.is_err() {
        return Err(());
    }
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut tmp = [0u8; 4096];
    loop {
        use tokio::io::AsyncReadExt;
        match upstream_io.read(&mut tmp).await {
            Ok(0) => return Err(()),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(head_end) = find_head_end(&buf) {
                    let resp_head = buf[..head_end].to_vec();
                    let extra = buf[head_end..].to_vec();
                    let is_101 = head_starts_101(&resp_head);
                    return Ok(PreparedUpgrade {
                        upstream_io,
                        extra,
                        resp_head,
                        is_101,
                    });
                }
                if buf.len() > 64 * 1024 {
                    return Err(());
                }
            }
            Err(_) => return Err(()),
        }
    }
}

pub(crate) fn build_raw_request_head(
    method: &Method,
    uri_path: &str,
    headers: &HeaderMap,
) -> Vec<u8> {
    let mut head = format!("{method} {uri_path} HTTP/1.1\r\n").into_bytes();
    for (name, value) in headers {
        head.extend_from_slice(name.as_str().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    head
}

/// 找请求/响应头结束位置（\r\n\r\n 或 \n\n），返回其后的偏移。
pub(crate) fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2))
}

pub(crate) fn head_starts_101(head: &[u8]) -> bool {
    head.starts_with(b"HTTP/1.1 101") || head.starts_with(b"HTTP/1.0 101")
}

/// 从 upstream 的原始响应头重建 101 响应：保留 Sec-WebSocket-Accept 等头。
pub(crate) fn build_upgrade_response(head: &[u8]) -> Response<HandlerBody> {
    let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    let text = String::from_utf8_lossy(head);
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.trim().as_bytes()),
                HeaderValue::from_str(value.trim()),
            ) {
                builder = builder.header(name, value);
            }
        }
    }
    match builder.body(empty_body()) {
        Ok(res) => res,
        Err(_) => bad_gateway_response(),
    }
}

/// 升级完成后：把预读字节补发给手机，再双向原始拷贝（WS 帧流不做任何改写）。
async fn tunnel_after_upgrade(client_io: Upgraded, prepared: PreparedUpgrade) {
    let mut client_io = TokioIo::new(client_io);
    let mut upstream_io = prepared.upstream_io;
    use tokio::io::AsyncWriteExt;
    if !prepared.extra.is_empty() && client_io.write_all(&prepared.extra).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await;
}

// ---------------------------------------------------------------------------
// 响应构造与页面
// ---------------------------------------------------------------------------
