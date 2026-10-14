//! HTTP 透明反代 + failover：对池内候选按 `attempt_order` 顺序重试。
//! 失败判定见 gateway.md §3.2：仅连接错误/超时/5xx 触发 failover，
//! 4xx 直接透传；已发出响应头的流式响应不重试。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;

use super::pool::{Backend, Pool};
use super::GatewayState;

/// 逐跳头（rfc9110 §7.6.1）不透传；另剥 host/content-length 交由 reqwest 重建。
const HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

fn is_hop_header(name: &header::HeaderName) -> bool {
    HOP_HEADERS.iter().any(|h| name.as_str() == *h)
}

/// 转发目标 URL：`endpoint` + 原始 path+query；`path_override` 非空时替换 path（query 透传）。
pub fn upstream_url(backend: &Backend, path_and_query: &str) -> String {
    let pq = match &backend.path_override {
        Some(p) => match path_and_query.split_once('?') {
            Some((_, q)) => format!("{p}?{q}"),
            None => p.clone(),
        },
        None => path_and_query.to_string(),
    };
    if pq.starts_with('/') {
        format!("{}{}", backend.endpoint, pq)
    } else {
        format!("{}/{}", backend.endpoint, pq)
    }
}

/// 客户端请求头 → 上游请求头：剥逐跳头，backend.api_key 覆盖 Authorization。
fn outbound_headers(incoming: &HeaderMap, backend: &Backend) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in incoming.iter() {
        if !is_hop_header(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some(key) = &backend.api_key {
        // 允许配置裸 token 或完整 "Bearer xxx"。
        let value = if key.contains(' ') {
            key.clone()
        } else {
            format!("Bearer {key}")
        };
        if let Ok(v) = HeaderValue::from_str(&value) {
            out.insert(header::AUTHORIZATION, v);
        }
    }
    out
}

/// 核心转发：按池的尝试序列逐个 backend 重试，返回首个非 5xx 响应。
/// 全部失败 → 502 JSON（OpenAI error 形态）。
pub async fn forward(state: &Arc<GatewayState>, pool: &Pool, req: Request) -> Response {
    state.metrics.inc_requests(pool.kind);

    let (parts, body) = req.into_parts();
    // 重试需要可重放 body：整读入内存（chat/tts 请求体都很小）。
    let body_bytes = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": {"message": format!("cannot read request body: {e}"), "type": "gateway_error"}
                })),
            )
                .into_response();
        }
    };
    let path_q = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/")
        .to_string();

    let mut attempts = pool.attempt_order();
    attempts.truncate(state.cfg.max_attempts);
    if attempts.is_empty() {
        return gateway_error(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("no {} upstreams configured", pool.kind),
        );
    }

    let timeout = Duration::from_millis(state.cfg.request_timeout_ms);
    let last = attempts.len() - 1;
    let mut last_err = String::from("no attempt made");
    for (i, backend) in attempts.iter().enumerate() {
        state.metrics.inc_backend_attempt(&backend.name);
        let url = upstream_url(backend, &path_q);
        let out = state
            .client
            .request(parts.method.clone(), &url)
            .headers(outbound_headers(&parts.headers, backend))
            .body(body_bytes.clone());
        match tokio::time::timeout(timeout, out.send()).await {
            // 2xx/3xx/4xx：透传（4xx 是客户端错误，不 failover）。
            Ok(Ok(resp)) if !resp.status().is_server_error() => {
                backend.on_success();
                return to_axum(resp);
            }
            Ok(Ok(resp)) => {
                last_err = format!("upstream {} returned {}", backend.name, resp.status());
                backend.on_failure(pool.cooldown_threshold, pool.cooldown, &last_err);
                state.metrics.inc_upstream_failures(pool.kind);
            }
            Ok(Err(e)) => {
                last_err = format!("upstream {} error: {e}", backend.name);
                backend.on_failure(pool.cooldown_threshold, pool.cooldown, &last_err);
                state.metrics.inc_upstream_failures(pool.kind);
            }
            Err(_) => {
                last_err = format!(
                    "upstream {} timed out after {}ms",
                    backend.name, state.cfg.request_timeout_ms
                );
                backend.on_failure(pool.cooldown_threshold, pool.cooldown, &last_err);
                state.metrics.inc_upstream_failures(pool.kind);
            }
        }
        if i < last {
            state.metrics.inc_failovers(pool.kind);
        }
    }

    gateway_error(
        StatusCode::BAD_GATEWAY,
        &format!("all {} upstreams failed: {last_err}", pool.kind),
    )
}

/// reqwest 流式响应 → axum 响应：status + 非逐跳头 + `bytes_stream` 原样转 Body。
fn to_axum(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let headers = resp.headers().clone();
    let mut out = Response::new(Body::from_stream(resp.bytes_stream()));
    *out.status_mut() = status;
    let h = out.headers_mut();
    for (k, v) in headers.iter() {
        if !is_hop_header(k) {
            h.append(k.clone(), v.clone());
        }
    }
    out
}

/// OpenAI 形态错误体。
pub fn gateway_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": {"message": message, "type": "gateway_error"}
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(endpoint: &str, path_override: Option<&str>) -> Backend {
        Backend {
            name: "b".into(),
            endpoint: endpoint.into(),
            priority: 0,
            weight: 1,
            health_path: "/healthz".into(),
            api_key: None,
            path_override: path_override.map(|s| s.to_string()),
            health: Default::default(),
        }
    }

    #[test]
    fn url_joins_endpoint_and_query() {
        let b = backend("http://up:1", None);
        assert_eq!(
            upstream_url(&b, "/v1/chat/completions?x=1"),
            "http://up:1/v1/chat/completions?x=1"
        );
    }

    #[test]
    fn path_override_replaces_path_keeps_query() {
        let b = backend("http://up:1", Some("/ws"));
        assert_eq!(upstream_url(&b, "/v1/listen?m=x"), "http://up:1/ws?m=x");
        assert_eq!(upstream_url(&b, "/v1/listen"), "http://up:1/ws");
    }
}
