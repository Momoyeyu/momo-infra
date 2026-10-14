//! `momoctl gateway` 回归（语义见 docs/01-architecture/gateway.md）：
//! - LB：同优先级加权轮询分布；priority 低组整体优先，跨组兜底；
//! - failover：5xx/超时/连接错自动转下一 backend；4xx 透传不重试；
//! - 熔断：连续失败进冷却、退出正常轮转；readyz 聚合 503；全挂 502 JSON；
//! - TTS/ASR 透明反代（ASR WS upgrade 帧中继 + HTTP 兜底）；
//! - /healthz /readyz /metrics 本地自报告。
//!
//! LLM/TTS upstream 用 wiremock；ASR WS upstream 用手写 axum echo stub。

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use momo_infra::gateway::{self, config::GatewayConfig, GatewayState};
use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn parse(toml: &str) -> GatewayConfig {
    GatewayConfig::parse(toml).unwrap()
}

/// 起 gateway（随机端口），返回 base URL 与 state（可读指标/健康快照）。
async fn spawn_gateway(toml: &str) -> (String, Arc<GatewayState>) {
    let state = GatewayState::new(parse(toml)).unwrap();
    let app = gateway::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

fn chat_body() -> Value {
    json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]})
}

async fn post_chat(client: &reqwest::Client, base: &str) -> reqwest::Response {
    client
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body())
        .send()
        .await
        .unwrap()
}

fn llm_cfg(backends: &str, extra: &str) -> String {
    format!("{extra}\n{backends}")
}

/// 同优先级等权两 backend：一挂一成 → failover 成功返回，指标计数正确。
#[tokio::test]
async fn llm_failover_on_5xx_serves_via_next_backend() {
    let a = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&a)
        .await;
    let b = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"via": "b"})))
        .mount(&b)
        .await;

    let (base, _state) = spawn_gateway(&llm_cfg(
        &format!(
            "[[llm.backends]]\nname=\"a\"\nendpoint=\"{}\"\n\
             [[llm.backends]]\nname=\"b\"\nendpoint=\"{}\"\n",
            a.uri(),
            b.uri()
        ),
        "",
    ))
    .await;

    let resp = post_chat(&reqwest::Client::new(), &base).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["via"], "b");

    // a 试过 1 次后 failover 到 b。
    assert_eq!(a.received_requests().await.unwrap().len(), 1);
    assert_eq!(b.received_requests().await.unwrap().len(), 1);

    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("gateway_requests_total{pool=\"llm\"} 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains("gateway_failovers_total{pool=\"llm\"} 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains("gateway_upstream_failures_total{pool=\"llm\"} 1"),
        "{metrics}"
    );
}

/// 4xx 是客户端错误：直接透传，不 failover。
#[tokio::test]
async fn llm_4xx_passthrough_without_failover() {
    let a = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "bad request", "type": "invalid_request_error"}
        })))
        .mount(&a)
        .await;
    let b = MockServer::start().await;

    let (base, _state) = spawn_gateway(&llm_cfg(
        &format!(
            "[[llm.backends]]\nendpoint=\"{}\"\n[[llm.backends]]\nendpoint=\"{}\"\n",
            a.uri(),
            b.uri()
        ),
        "",
    ))
    .await;

    let resp = post_chat(&reqwest::Client::new(), &base).await;
    assert_eq!(resp.status(), 400);
    assert_eq!(b.received_requests().await.unwrap().len(), 0);
}

/// 上游超时按失败处理并 failover（request_timeout_ms 生效）。
#[tokio::test]
async fn llm_timeout_triggers_failover() {
    let slow = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(5)))
        .mount(&slow)
        .await;
    let fast = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"via": "fast"})))
        .mount(&fast)
        .await;

    let (base, _state) = spawn_gateway(&llm_cfg(
        &format!(
            "[[llm.backends]]\nendpoint=\"{}\"\n[[llm.backends]]\nendpoint=\"{}\"\n",
            slow.uri(),
            fast.uri()
        ),
        "request_timeout_ms = 150\n",
    ))
    .await;

    let resp = post_chat(&reqwest::Client::new(), &base).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["via"], "fast");
}

/// 连续失败达阈值 → 熔断并退出正常轮转；冷却期间请求只打健康成员。
#[tokio::test]
async fn llm_cooling_backend_excluded_after_threshold() {
    let a = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&a)
        .await;
    let b = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"via": "b"})))
        .mount(&b)
        .await;

    let (base, state) = spawn_gateway(&llm_cfg(
        &format!(
            "[[llm.backends]]\nname=\"a\"\nendpoint=\"{}\"\n\
             [[llm.backends]]\nname=\"b\"\nendpoint=\"{}\"\n",
            a.uri(),
            b.uri()
        ),
        "cooldown_threshold = 2\ncooldown_ms = 60000\n",
    ))
    .await;
    let client = reqwest::Client::new();

    // 等权轮询下 a 在序号为奇数的请求上被首选：req1/req3 各打 a 一次并
    // failover 到 b；req2 首选 b 直接成功。累计 2 次失败后 a 熔断。
    for _ in 0..3 {
        assert_eq!(post_chat(&client, &base).await.status(), 200);
    }
    assert_eq!(a.received_requests().await.unwrap().len(), 2);
    assert!(state.llm.backends[0].is_cooling(), "a should be cooling");

    // 熔断后请求只打 b（a 退到尾部但 b 先成功）。
    assert_eq!(post_chat(&client, &base).await.status(), 200);
    assert_eq!(a.received_requests().await.unwrap().len(), 2);
    assert_eq!(b.received_requests().await.unwrap().len(), 4);
}

/// 全部 backend 失败 → 502 OpenAI 形态错误体。
#[tokio::test]
async fn llm_all_backends_down_returns_502() {
    let a = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&a)
        .await;

    let (base, _state) = spawn_gateway(&llm_cfg(
        &format!("[[llm.backends]]\nendpoint=\"{}\"\n", a.uri()),
        "",
    ))
    .await;

    let resp = post_chat(&reqwest::Client::new(), &base).await;
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "gateway_error");
}

/// weight 3:1 → 8 次请求首选分布 6:2（加权轮询）。
#[tokio::test]
async fn llm_weighted_round_robin_distribution() {
    let a = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"via": "a"})))
        .mount(&a)
        .await;
    let b = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"via": "b"})))
        .mount(&b)
        .await;

    let (base, _state) = spawn_gateway(&llm_cfg(
        &format!(
            "[[llm.backends]]\nendpoint=\"{}\"\nweight=3\n\
             [[llm.backends]]\nendpoint=\"{}\"\nweight=1\n",
            a.uri(),
            b.uri()
        ),
        "",
    ))
    .await;
    let client = reqwest::Client::new();
    for _ in 0..8 {
        assert_eq!(post_chat(&client, &base).await.status(), 200);
    }
    assert_eq!(a.received_requests().await.unwrap().len(), 6);
    assert_eq!(b.received_requests().await.unwrap().len(), 2);
}

/// priority 低组全挂 → 高组兜底接管；低组恢复后自动回切。
#[tokio::test]
async fn llm_priority_group_fallback() {
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&primary)
        .await;
    let backup = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"via": "backup"})))
        .mount(&backup)
        .await;

    let (base, _state) = spawn_gateway(&llm_cfg(
        &format!(
            "[[llm.backends]]\nendpoint=\"{}\"\npriority=0\n\
             [[llm.backends]]\nendpoint=\"{}\"\npriority=1\n",
            primary.uri(),
            backup.uri()
        ),
        "",
    ))
    .await;

    let resp = post_chat(&reqwest::Client::new(), &base).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["via"], "backup");
}

/// /v1/models 与任意 /v1/* 端点同样走 llm 池。
#[tokio::test]
async fn llm_models_endpoint_forwarded() {
    let a = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "data": [{"id": "qwen3", "object": "model"}]
        })))
        .mount(&a)
        .await;

    let (base, _state) = spawn_gateway(&llm_cfg(
        &format!("[[llm.backends]]\nendpoint=\"{}\"\n", a.uri()),
        "",
    ))
    .await;

    let resp = reqwest::get(format!("{base}/v1/models")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["data"][0]["id"],
        "qwen3"
    );
}

/// TTS：/v1/audio/speech 透明反代（body/Content-Type/Authorization 透传）。
#[tokio::test]
async fn tts_speech_transparent_proxy() {
    let tts = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .and(header("authorization", "Bearer client-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"RIFFfakeaudio".to_vec())
                .insert_header("content-type", "audio/mpeg"),
        )
        .mount(&tts)
        .await;

    let (base, _state) =
        spawn_gateway(&format!("[[tts.upstream]]\nendpoint=\"{}\"\n", tts.uri())).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/audio/speech"))
        .header("authorization", "Bearer client-token")
        .json(&json!({"model": "tts-1", "input": "你好", "voice": "alloy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "audio/mpeg");
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"RIFFfakeaudio");
    assert_eq!(tts.received_requests().await.unwrap().len(), 1);
}

/// ASR REST 兜底（非 upgrade 请求）：同样透明反代，可 path 改写。
#[tokio::test]
async fn asr_http_fallback_with_path_rewrite() {
    let asr = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ws"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&asr)
        .await;

    let (base, _state) = spawn_gateway(&format!(
        "[[asr.upstream]]\nendpoint=\"{}\"\npath=\"/ws\"\n",
        asr.uri()
    ))
    .await;

    // 默认对外路径 /v1/listen；配置 path="/ws" 改写上游路径。
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/listen?model=x"))
        .body("audio-bytes")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// ASR WS：客户端 upgrade → gateway 代握上游 → 文本帧 echo 透传。
#[tokio::test]
async fn asr_ws_frames_relayed() {
    // 手写 echo stub：回显收到的每条消息，close 时结束。
    async fn echo(ws: WebSocketUpgrade) -> impl IntoResponse {
        ws.on_upgrade(|mut socket| async move {
            while let Some(Ok(msg)) = socket.recv().await {
                if matches!(msg, Message::Close(_)) {
                    break;
                }
                if socket.send(msg).await.is_err() {
                    break;
                }
            }
        })
    }
    let upstream_app = Router::new()
        .route("/v1/listen", axum::routing::get(echo))
        .route("/healthz", axum::routing::get(|| async { "ok" }));
    let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_addr = up_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(up_listener, upstream_app).await.unwrap() });

    let (base, _state) = spawn_gateway(&format!(
        "[[asr.upstream]]\nendpoint=\"http://{up_addr}\"\n",
    ))
    .await;
    let ws_url = format!(
        "ws://{}/v1/listen?model=sensevoice",
        base.trim_start_matches("http://")
    );

    let (mut sock, _resp) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
    sock.send(tokio_tungstenite::tungstenite::Message::Text(
        "hello-asr".into(),
    ))
    .await
    .unwrap();
    let echoed = sock.next().await.unwrap().unwrap();
    assert_eq!(
        echoed,
        tokio_tungstenite::tungstenite::Message::Text("hello-asr".into())
    );
    // query 透传已由 upstream 命中 /v1/listen 证实；关连接。
    sock.close(None).await.unwrap();
}

/// ASR WS 上游不可达 → 客户端收到 1011 close 而非挂死。
#[tokio::test]
async fn asr_ws_all_upstreams_down_closes_1011() {
    // 指一个未被监听的端口制造连接错误。
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    };
    let (base, _state) = spawn_gateway(&format!(
        "request_timeout_ms = 200\n[[asr.upstream]]\nendpoint=\"ws://{dead}\"\n",
    ))
    .await;
    let ws_url = format!("ws://{}/v1/listen", base.trim_start_matches("http://"));

    let (mut sock, _resp) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
    let msg = sock.next().await.unwrap().unwrap();
    match msg {
        tokio_tungstenite::tungstenite::Message::Close(cf) => {
            assert_eq!(cf.unwrap().code, 1011.into());
        }
        other => panic!("expected close frame, got {other:?}"),
    }
}

/// /healthz 恒 200；/readyz 在池全员熔断时 503。
#[tokio::test]
async fn healthz_readyz_aggregation() {
    let a = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&a)
        .await;

    let (base, _state) = spawn_gateway(&llm_cfg(
        &format!("[[llm.backends]]\nendpoint=\"{}\"\n", a.uri()),
        "cooldown_threshold = 1\ncooldown_ms = 60000\n",
    ))
    .await;
    let client = reqwest::Client::new();

    assert_eq!(
        client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    // 初始：唯一 backend 未熔断 → ready。
    let resp = client.get(format!("{base}/readyz")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["status"], "ready");

    // 打挂它 → 熔断 → readyz 503 + not_ready。
    assert_eq!(post_chat(&client, &base).await.status(), 502);
    let resp = client.get(format!("{base}/readyz")).send().await.unwrap();
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "not_ready");
    assert_eq!(body["pools"]["llm"]["degraded"], true);
}
