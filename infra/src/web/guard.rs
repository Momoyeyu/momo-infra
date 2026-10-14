//! `momoctl web` 守卫：启动期 + 请求级两段。
//! - 启动守卫 `check_listen`：`--listen` 绑出 loopback 时强制 `MOMO_WEB_TOKEN`
//!   （exit 2 usage error，config.md §2.4 口径）。
//! - 请求级中间件 `require_auth`（挂 `/api` 子树），两段：
//!   1. token——已设 → 全请求须 `Authorization: Bearer`，缺失/不匹配 → 401；
//!      未设 → 仅 loopback peer 且 Host 回环放行（Host 检查防 DNS rebinding：
//!      攻击者域名解析到 127.0.0.1 时 Host 非回环，拒），否则 403。
//!   2. CSRF——state-changing 方法（非 GET/HEAD/OPTIONS）额外校验
//!      `X-CSRF-Token` 头回带 `GET /api/csrf` 签发的进程级 token，且 Origin
//!      （若携带）authority 须等于 Host；失败 403。
//!
//! 认证失败一律 `{schema:1, error:{code,message}}`（api::dto::ApiError）。

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use axum::extract::connect_info::ConnectInfo;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use super::{api, WebState};
use crate::output::AppError;

/// CSRF token 回带头：mutation 请求必须携带且与 `GET /api/csrf` 签发值一致。
pub const CSRF_HEADER: &str = "x-csrf-token";

/// 校验 `--listen` 地址与 token 配置：
/// - 地址不可解析 / 解析为空 → Config（exit 2）；
/// - 任一解析结果非 loopback 且 token 未设 → Config（exit 2）；
/// - 全 loopback，或已配 token → 放行。
///
/// `localhost` 经系统解析（IPv4+IPv6 须全部 loopback 才视为本机绑定），
/// 防止只绑 v6 ::1 之外地址这类半开放配置蒙混过关。
pub fn check_listen(listen: &str, token_configured: bool) -> Result<(), AppError> {
    let addrs: Vec<_> = listen
        .to_socket_addrs()
        .map_err(|e| AppError::Config(format!("invalid --listen '{listen}': {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(AppError::Config(format!(
            "invalid --listen '{listen}': resolves to no address"
        )));
    }
    if !addrs.iter().all(|a| a.ip().is_loopback()) && !token_configured {
        return Err(AppError::Config(
            "--listen binds beyond loopback but MOMO_WEB_TOKEN is not set; \
             refusing to start (set MOMO_WEB_TOKEN or bind a loopback address)"
                .into(),
        ));
    }
    Ok(())
}

// ---------- 请求级认证链（/api 子树中间件） ----------

/// `/api` 子树认证中间件：token → CSRF 两段（momo-infra.md Web 控制台口径）。
/// SPA（非 /api 路径）不经过本中间件。
pub async fn require_auth(
    State(state): State<Arc<WebState>>,
    req: Request,
    next: Next,
) -> Response {
    // ① 身份：token 已设则 Bearer 必验（任意来源）；未设则仅本机。
    if let Some(expected) = &state.token {
        let ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(bearer_token)
            .is_some_and(|t| ct_eq(t, expected));
        if !ok {
            return api::error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or invalid token; send Authorization: Bearer <MOMO_WEB_TOKEN>",
            );
        }
    } else {
        let peer_loopback = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            // 无 ConnectInfo（如 tower oneshot 直连）按非回环处理：fail closed。
            .is_some_and(|c| c.0.ip().is_loopback());
        if !peer_loopback || !host_is_loopback(req.headers()) {
            return api::error(
                StatusCode::FORBIDDEN,
                "forbidden",
                "MOMO_WEB_TOKEN unset: /api is limited to loopback clients",
            );
        }
    }

    // ② CSRF：只读方法（GET/HEAD/OPTIONS，含 SSE）放行；
    // state-changing 方法须 Origin 一致（若携带）+ CSRF token 回带。
    if !read_only(req.method()) {
        if !origin_ok(req.headers()) {
            return api::error(
                StatusCode::FORBIDDEN,
                "forbidden",
                "Origin authority does not match Host",
            );
        }
        let ok = req
            .headers()
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| ct_eq(t, &state.csrf_token));
        if !ok {
            return api::error(
                StatusCode::FORBIDDEN,
                "csrf",
                "missing or invalid CSRF token; GET /api/csrf then echo X-CSRF-Token",
            );
        }
    }
    next.run(req).await
}

/// GET/HEAD/OPTIONS 为只读（SSE 走 GET）：免 CSRF；其余一律按 state-changing 处理。
fn read_only(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// 解析 `Authorization: Bearer <token>`；scheme 大小写不敏感（RFC 7235）。
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, rest) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// 常量时间比较（token 比对防时序侧信道）：长度公开，内容逐字节 xor。
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Host 头回环判定：缺 Host 或非回环主机一律判否（fail closed）。
/// 无 token 模式必走此检查，挡 DNS rebinding——攻击域名 A 记录指 127.0.0.1
/// 时浏览器同源放行，但 Host 是该域名而非回环。
fn host_is_loopback(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    host_str_is_loopback(host)
}

/// Host 字面值判定：`[::1]:7900` 剥方括号、`localhost:7900` 剥端口、
/// 尾点（`localhost.`）归一；IP 字面值走 `is_loopback`。
fn host_str_is_loopback(host: &str) -> bool {
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((n, _)) => n,
            None => return false,
        }
    } else {
        host.split(':').next().unwrap_or("")
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if name == "localhost" {
        return true;
    }
    name.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// mutation 的 Origin 校验：Origin 存在时其 authority 必须等于 Host
/// （自家 SPA 同源恒成立；跨站表单/跨源 fetch 的 Origin 必不匹配）。
/// 无 Origin（curl/脚本等非浏览器客户端）放行——Bearer/CSRF 已鉴权；
/// Origin 不可解析（如 `null`）判否。
fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    origin_authority(origin).is_some_and(|a| a.eq_ignore_ascii_case(host.trim()))
}

/// 提取 Origin 的 authority（`scheme://` 之后、首个 `/` 之前）。
/// Origin 规范不含 path；无 `://`（如 `null`）时整串当 authority，必不匹配 Host。
fn origin_authority(origin: &str) -> Option<&str> {
    let rest = origin.split_once("://").map(|(_, r)| r).unwrap_or(origin);
    let auth = rest.split('/').next()?;
    if auth.is_empty() {
        None
    } else {
        Some(auth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// loopback 字面量无论有无 token 都放行。
    #[test]
    fn loopback_passes_without_token() {
        assert!(check_listen("127.0.0.1:7900", false).is_ok());
        assert!(check_listen("[::1]:7900", false).is_ok());
    }

    /// 非 loopback 无 token → exit 2；有 token → 放行。
    #[test]
    fn non_loopback_requires_token() {
        let err = check_listen("0.0.0.0:7900", false).unwrap_err();
        assert!(matches!(err, AppError::Config(_)), "{err}");
        assert!(check_listen("0.0.0.0:7900", true).is_ok());
    }

    /// 无法解析的地址 → exit 2 而非 panic。
    #[test]
    fn unresolvable_is_usage_error() {
        assert!(matches!(
            check_listen("not-an-addr", false),
            Err(AppError::Config(_))
        ));
    }

    /// `localhost` 解析结果全 loopback 时按本机绑定放行。
    /// （解析依赖系统 hosts；CI/开发机 localhost 均解析到回环。）
    #[test]
    fn localhost_counts_as_loopback() {
        assert!(check_listen("localhost:7900", false).is_ok());
    }

    /// Bearer scheme 大小写不敏感；空 token / 非 Bearer / 缺空格判 None。
    #[test]
    fn bearer_token_parses_scheme() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer  abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("Bearerabc"), None);
    }

    /// 常量时间比较：等值 true，长度/内容差异 false。
    #[test]
    fn ct_eq_compares_bytes() {
        assert!(ct_eq("tok", "tok"));
        assert!(!ct_eq("tok", "tOk"));
        assert!(!ct_eq("tok", "token"));
    }

    /// Host 回环：字面 IP / localhost / 尾点 / 方括号 v6 均判回环。
    #[test]
    fn host_str_is_loopback_covers_forms() {
        for h in [
            "127.0.0.1:7900",
            "127.0.0.1",
            "localhost:7900",
            "localhost.",
            "LOCALHOST:7900",
            "[::1]:7900",
            "[::1]",
        ] {
            assert!(host_str_is_loopback(h), "{h}");
        }
        // 裸 "::1" 非合法 Host 写法（v6 必须带方括号），按非回环拒。
        for h in ["example.com", "0.0.0.0:7900", "10.0.0.1", "", "::1", "[::1"] {
            assert!(!host_str_is_loopback(h), "{h}");
        }
    }

    /// Origin 校验：authority 等于 Host 放行；无 Origin 放行；不匹配/不可解析判否。
    #[test]
    fn origin_check_matches_host() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "127.0.0.1:7900".parse().unwrap());
        assert!(origin_ok(&h), "no Origin → pass");
        h.insert(header::ORIGIN, "http://127.0.0.1:7900".parse().unwrap());
        assert!(origin_ok(&h));
        h.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!origin_ok(&h));
        h.insert(header::ORIGIN, "null".parse().unwrap());
        assert!(!origin_ok(&h));
    }

    /// 只读方法表：GET/HEAD/OPTIONS 免 CSRF，其余按 mutation 处理。
    #[test]
    fn read_only_method_table() {
        for m in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(read_only(&m), "{m}");
        }
        for m in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            assert!(!read_only(&m), "{m}");
        }
    }
}
