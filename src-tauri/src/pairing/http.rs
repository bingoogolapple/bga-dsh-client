//! HTTP 响应构造（302 / 403 / 502 / 503 等门禁响应）。
//!
//! 从 `pairing/mod.rs` 拆出：这些是纯构造函数（不碰 Tauri 状态、不做 IO），
//! 单独成文件后既方便复用，也让 `mod.rs` 只剩编排逻辑。

use bytes::Bytes;
use hyper::header::{HeaderValue, CONTENT_TYPE, LOCATION, SET_COOKIE};
use hyper::{Response, StatusCode};

use crate::i18n::{tr, Locale};

use super::forward::{empty_body, full_body};
use super::rewrite::PAIR_COOKIE;
use super::{HandlerBody, PAIR_TTL};

/// 302 跳回首页（配对成功后不带会话令牌时使用）。
pub(crate) fn redirect_home() -> Response<HandlerBody> {
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
pub(crate) fn redirect_home_with_session(token: &str) -> Response<HandlerBody> {
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

/// 构造一个 HTML 页面响应（用于门禁/错误提示）。
///
/// 注意 `title` 与 `body` 都来自本项目的 i18n 词条（受控常量），不含用户输入，
/// 因此这里直接插进 HTML 是安全的。若将来要在 body 里展示外部数据（如 IP、
/// 错误详情），必须先做 HTML 转义。
pub(crate) fn html_response(status: StatusCode, title: &str, body: &str) -> Response<HandlerBody> {
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

/// 403：未配对 / 会话已过期。
pub(crate) fn denied_response(locale: Locale) -> Response<HandlerBody> {
    html_response(
        StatusCode::FORBIDDEN,
        &tr(locale, "pair.denied_title", &[]),
        &tr(locale, "pair.denied_body", &[]),
    )
}

/// 503：桌面端 DSH 服务未运行。
pub(crate) fn service_down_response() -> Response<HandlerBody> {
    let locale = crate::i18n::global();
    html_response(
        StatusCode::SERVICE_UNAVAILABLE,
        &tr(locale, "pair.down_title", &[]),
        &tr(locale, "pair.down_body", &[]),
    )
}

/// 502：转发上游失败。
pub(crate) fn bad_gateway_response() -> Response<HandlerBody> {
    let locale = crate::i18n::global();
    html_response(
        StatusCode::BAD_GATEWAY,
        &tr(locale, "pair.gw_title", &[]),
        &tr(locale, "pair.gw_body", &[]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 302 响应带 Location: /。
    #[test]
    fn redirect_home_sets_location() {
        let res = redirect_home();
        assert_eq!(res.status(), StatusCode::FOUND);
        assert_eq!(res.headers().get(LOCATION).unwrap(), "/");
    }

    /// 配对成功的 302 必须带上 HttpOnly + SameSite 的会话 Cookie
    /// （HttpOnly 防 JS 窃取，SameSite=Lax 防 CSRF）。
    #[test]
    fn session_redirect_sets_secure_cookie() {
        let res = redirect_home_with_session("deadbeef");
        assert_eq!(res.status(), StatusCode::FOUND);
        let cookie = res
            .headers()
            .get(SET_COOKIE)
            .expect("应下发 Set-Cookie")
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.starts_with(&format!("{PAIR_COOKIE}=deadbeef")));
        assert!(
            cookie.contains("HttpOnly"),
            "会话 Cookie 必须 HttpOnly: {cookie}"
        );
        assert!(
            cookie.contains("SameSite=Lax"),
            "应设 SameSite=Lax: {cookie}"
        );
        assert!(cookie.contains("Path=/"), "Cookie 应对全站生效: {cookie}");
        // Max-Age 应等于配对有效期（30 分钟 = 1800 秒）
        assert!(
            cookie.contains(&format!("Max-Age={}", PAIR_TTL.as_secs())),
            "Max-Age 应为配对有效期秒数: {cookie}"
        );
    }

    /// 非法 Cookie 值（含控制字符）不能让构造 panic：HeaderValue::from_str 失败时
    /// 应静默跳过 Set-Cookie，而不是崩溃或产出畸形响应。
    #[test]
    fn invalid_token_does_not_panic() {
        let res = redirect_home_with_session("bad\ntoken");
        assert_eq!(res.status(), StatusCode::FOUND);
        // 非法值被忽略，不写入 header
        assert!(res.headers().get(SET_COOKIE).is_none());
    }

    /// 各错误页的状态码与 Content-Type 正确。
    #[test]
    fn error_pages_have_right_status_and_content_type() {
        let cases = [
            (denied_response(Locale::Zh).status(), StatusCode::FORBIDDEN),
            (denied_response(Locale::En).status(), StatusCode::FORBIDDEN),
            (
                service_down_response().status(),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (bad_gateway_response().status(), StatusCode::BAD_GATEWAY),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
        let res = denied_response(Locale::Zh);
        assert_eq!(
            res.headers().get(CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
    }
}
