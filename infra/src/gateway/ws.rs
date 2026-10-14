//! ASR WS 透明反代：客户端 upgrade → gateway 代握上游 WS → 双向帧中继。
//! 上游选择复用 pool 的 attempt_order（连接失败即 failover 到下一候选）。
//! 帧只做 axum↔tungstenite 类型映射，不解析 payload。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::{HeaderMap, Uri};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite;

use super::pool::Backend;
use super::GatewayState;

/// 转发给上游 WS 握手的头：鉴权/cookie/子协议透传，其余由 tungstenite 重建。
const FORWARD_HEADERS: &[&str] = &["authorization", "cookie", "sec-websocket-protocol"];

/// endpoint（ws/wss/http/https）→ ws scheme 的 base。
fn ws_base(endpoint: &str) -> String {
    if let Some(rest) = endpoint.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if let Some(rest) = endpoint.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        endpoint.to_string()
    }
}

/// 上游 WS URL：endpoint + 原始 path+query（`path_override` 时替换 path）。
fn ws_url(backend: &Backend, uri: &Uri) -> String {
    let pq = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let path_q = match &backend.path_override {
        Some(p) => match pq.split_once('?') {
            Some((_, q)) => format!("{p}?{q}"),
            None => p.clone(),
        },
        None => pq.to_string(),
    };
    format!("{}{}", ws_base(&backend.endpoint), path_q)
}

/// 构造上游握手请求：`IntoClientRequest` 生成标准握手头（Host/Upgrade/
/// Sec-WebSocket-Key/Version，gateway 是独立客户端身份），再叠加透传
/// 鉴权/cookie/子协议头。
fn upstream_request(url: &str, incoming: &HeaderMap) -> http::Request<()> {
    use tungstenite::client::IntoClientRequest;
    let mut req = url
        .into_client_request()
        .unwrap_or_else(|_| http::Request::new(()));
    for (k, v) in incoming.iter() {
        if FORWARD_HEADERS.iter().any(|h| k.as_str() == *h) {
            req.headers_mut().insert(k.clone(), v.clone());
        }
    }
    req
}

/// 客户端 socket 已建立后的入口：按池选上游、代握、双向中继。
/// 所有候选均连接失败 → 以 1011 close 帧告知客户端。
pub async fn pump(state: Arc<GatewayState>, mut client: WebSocket, uri: Uri, headers: HeaderMap) {
    state.metrics.inc_ws_upgrades("asr");

    let mut attempts = state.asr.attempt_order();
    attempts.truncate(state.cfg.max_attempts);
    if attempts.is_empty() {
        let _ = close_with(&mut client, 1011, "no asr upstreams configured").await;
        return;
    }

    let timeout = Duration::from_millis(state.cfg.request_timeout_ms);
    let last = attempts.len() - 1;
    for (i, backend) in attempts.iter().enumerate() {
        state.metrics.inc_backend_attempt(&backend.name);
        let url = ws_url(backend, &uri);
        let req = upstream_request(&url, &headers);
        match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(req)).await {
            Ok(Ok((upstream, _resp))) => {
                backend.on_success();
                relay(client, upstream).await;
                return;
            }
            Ok(Err(e)) => {
                let err = format!("ws connect to {} failed: {e}", backend.name);
                eprintln!("warn: gateway asr: {err}");
                backend.on_failure(state.asr.cooldown_threshold, state.asr.cooldown, &err);
                state.metrics.inc_upstream_failures("asr");
            }
            Err(_) => {
                let err = format!("ws connect to {} timed out", backend.name);
                eprintln!("warn: gateway asr: {err}");
                backend.on_failure(state.asr.cooldown_threshold, state.asr.cooldown, &err);
                state.metrics.inc_upstream_failures("asr");
            }
        }
        if i < last {
            state.metrics.inc_failovers("asr");
        }
    }
    let _ = close_with(&mut client, 1011, "all asr upstreams unavailable").await;
}

async fn close_with(client: &mut WebSocket, code: u16, reason: &str) -> Result<(), axum::Error> {
    client
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await
}

/// 双向帧中继：任一侧流结束/出错即终止另一侧。
/// axum `WebSocket` 与 tungstenite `WebSocketStream` 都是 Message 的
/// Stream+Sink，只做枚举映射。
async fn relay<S>(client: WebSocket, upstream: S)
where
    S: futures_util::Stream<Item = Result<tungstenite::Message, tungstenite::Error>>
        + futures_util::Sink<tungstenite::Message, Error = tungstenite::Error>
        + Unpin,
{
    let (mut client_sink, mut client_stream) = client.split();
    let (mut up_sink, mut up_stream) = upstream.split();

    let client_to_upstream = async {
        while let Some(msg) = client_stream.next().await {
            match msg {
                Ok(m) => {
                    if up_sink.send(to_tungstenite(m)).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };
    let upstream_to_client = async {
        while let Some(msg) = up_stream.next().await {
            match msg {
                Ok(m) => {
                    if let Some(m) = to_axum(m) {
                        if client_sink.send(m).await.is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    };
    // 任一方向终止即整体结束（对端 socket 随 drop 关闭）。
    tokio::select! {
        _ = client_to_upstream => {},
        _ = upstream_to_client => {},
    }
}

fn to_tungstenite(m: Message) -> tungstenite::Message {
    use tungstenite::Message as T;
    match m {
        // axum Utf8Bytes 与 tungstenite Utf8Bytes 是各自 newtype，经 &str 互转。
        Message::Text(t) => T::Text(t.as_str().into()),
        Message::Binary(b) => T::Binary(b),
        Message::Ping(b) => T::Ping(b),
        Message::Pong(b) => T::Pong(b),
        Message::Close(cf) => T::Close(cf.map(|f| tungstenite::protocol::CloseFrame {
            code: tungstenite::protocol::frame::coding::CloseCode::from(f.code),
            reason: f.reason.as_str().into(),
        })),
    }
}

fn to_axum(m: tungstenite::Message) -> Option<Message> {
    use tungstenite::Message as T;
    match m {
        T::Text(t) => Some(Message::Text(t.as_str().into())),
        T::Binary(b) => Some(Message::Binary(b)),
        T::Ping(b) => Some(Message::Ping(b)),
        T::Pong(b) => Some(Message::Pong(b)),
        T::Close(cf) => Some(Message::Close(cf.map(|f| CloseFrame {
            code: u16::from(f.code),
            reason: f.reason.as_str().into(),
        }))),
        // 低层原始帧不下发（当前 tungstenite 不产生非 Message 流）。
        T::Frame(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;
    use axum::http::HeaderValue;
    use std::sync::Mutex;

    fn backend(endpoint: &str, path_override: Option<&str>) -> Backend {
        Backend {
            name: "b".into(),
            endpoint: endpoint.into(),
            priority: 0,
            weight: 1,
            health_path: "/healthz".into(),
            api_key: None,
            path_override: path_override.map(|s| s.to_string()),
            health: Mutex::new(Default::default()),
        }
    }

    #[test]
    fn http_endpoint_maps_to_ws_scheme() {
        let b = backend("http://127.0.0.1:7810", None);
        let uri: Uri = "http://gw/v1/listen?model=x".parse().unwrap();
        assert_eq!(ws_url(&b, &uri), "ws://127.0.0.1:7810/v1/listen?model=x");
    }

    #[test]
    fn wss_endpoint_keeps_scheme_and_override_rewrites_path() {
        let b = backend("wss://up.example", Some("/socket"));
        let uri: Uri = "http://gw/v1/listen?a=1".parse().unwrap();
        assert_eq!(ws_url(&b, &uri), "wss://up.example/socket?a=1");
    }

    #[test]
    fn upstream_request_forwards_only_allowlisted_headers() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer k"));
        h.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("token.v1"),
        );
        h.insert(
            header::SEC_WEBSOCKET_KEY,
            HeaderValue::from_static("client-key-must-not-forward"),
        );
        let req = upstream_request("ws://up/v1/listen", &h);
        assert_eq!(
            req.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer k"
        );
        assert_eq!(
            req.headers().get(header::SEC_WEBSOCKET_PROTOCOL).unwrap(),
            "token.v1"
        );
        // 客户端的 Sec-WebSocket-Key 不透传；gateway 以独立身份重新生成。
        let key = req.headers().get(header::SEC_WEBSOCKET_KEY).unwrap();
        assert_ne!(key, "client-key-must-not-forward");
    }
}
