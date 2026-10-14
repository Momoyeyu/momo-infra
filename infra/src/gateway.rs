//! `momoctl gateway`：统一对外接口（OpenAI-compatible LLM + 云化 ASR/TTS），
//! 对内多 backend 容灾与负载均衡。设计口径见 docs/01-architecture/gateway.md。
//!
//! 路由分层：`asr.paths`/`tts.paths`（默认 `/v1/listen`、`/v1/audio/speech`）
//! 精确路径先注册 → 透明反代；`/v1/{*rest}` 兜底 → llm 池（failover/LB）；
//! `/healthz` `/readyz` `/metrics` 本地自报告。

pub mod config;
pub mod health;
pub mod metrics;
pub mod pool;
pub mod proxy;
pub mod ws;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;

use crate::output::AppError;

use config::{BackendSpec, GatewayConfig};
use pool::{Backend, Pool};

/// gateway 跨 handler 共享状态。
#[derive(Debug)]
pub struct GatewayState {
    pub cfg: GatewayConfig,
    /// 共享 client（连接池复用）；不内置超时，逐请求 `request_timeout_ms` 控制。
    pub client: reqwest::Client,
    pub llm: Pool,
    pub asr: Pool,
    pub tts: Pool,
    pub metrics: metrics::Metrics,
}

impl GatewayState {
    pub fn new(cfg: GatewayConfig) -> Result<Arc<Self>, AppError> {
        let client = reqwest::Client::builder()
            // 反代不透传重定向：客户端自行处理 upstream 3xx。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AppError::Config(format!("build http client: {e}")))?;
        let cooldown = Duration::from_millis(cfg.cooldown_ms);
        let threshold = cfg.cooldown_threshold;
        Ok(Arc::new(Self {
            llm: Pool::new(
                "llm",
                build_backends(&cfg.llm.backends),
                threshold,
                cooldown,
            ),
            asr: Pool::new(
                "asr",
                build_backends(&cfg.asr.upstream),
                threshold,
                cooldown,
            ),
            tts: Pool::new(
                "tts",
                build_backends(&cfg.tts.upstream),
                threshold,
                cooldown,
            ),
            metrics: metrics::Metrics::default(),
            client,
            cfg,
        }))
    }
}

fn build_backends(specs: &[BackendSpec]) -> Vec<Arc<Backend>> {
    specs
        .iter()
        .map(|s| {
            let (name, endpoint, priority, weight, health_path, api_key, path_override) =
                s.normalized();
            Arc::new(Backend {
                name,
                endpoint,
                priority,
                weight,
                health_path,
                api_key,
                path_override,
                health: std::sync::Mutex::new(Default::default()),
            })
        })
        .collect()
}

/// 组装 Router：asr/tts 精确路径先于 `/v1/{*rest}` 兜底注册。
pub fn router(state: Arc<GatewayState>) -> Router {
    let mut app = Router::new()
        .route("/healthz", axum::routing::get(healthz))
        .route("/readyz", axum::routing::get(readyz))
        .route("/metrics", axum::routing::get(metrics_handler));

    for p in state.cfg.asr_paths() {
        app = app.route(&p, any(asr_entry));
    }
    for p in state.cfg.tts_paths() {
        app = app.route(&p, any(tts_entry));
    }
    app.route("/v1", any(llm_entry))
        .route("/v1/", any(llm_entry))
        .route("/v1/{*rest}", any(llm_entry))
        .with_state(state)
}

/// 同 `web::serve` 口径：已 bind 的 listener 上服务至 Ctrl-C。
pub async fn serve(listener: tokio::net::TcpListener, app: Router) -> Result<(), AppError> {
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(AppError::Io)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

// ---------- handlers ----------

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(state): State<Arc<GatewayState>>) -> Response {
    let report = health::ready_report(&state);
    let status = if report.status == "ready" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, axum::Json(report)).into_response()
}

async fn metrics_handler(State(state): State<Arc<GatewayState>>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        state.metrics.render(),
    )
}

/// `/v1/*` → llm 池（`/v1/chat/completions`、`/v1/models` 及其他 OpenAI 端点）。
async fn llm_entry(State(state): State<Arc<GatewayState>>, req: Request) -> Response {
    proxy::forward(&state, &state.llm, req).await
}

/// tts 对外路径 → tts 池透明反代。
async fn tts_entry(State(state): State<Arc<GatewayState>>, req: Request) -> Response {
    proxy::forward(&state, &state.tts, req).await
}

/// asr 对外路径：WS upgrade 走帧中继，普通 HTTP（Deepgram REST 形态）走透明反代。
async fn asr_entry(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    req: Request,
) -> Response {
    if is_ws_upgrade(&headers) {
        let (mut parts, _body) = req.into_parts();
        match axum::extract::ws::WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            Ok(upgrade) => {
                let uri = parts.uri.clone();
                let headers = parts.headers.clone();
                let state = state.clone();
                return upgrade.on_upgrade(move |socket| ws::pump(state, socket, uri, headers));
            }
            Err(rejection) => return rejection.into_response(),
        }
    }
    proxy::forward(&state, &state.asr, req).await
}

fn is_ws_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}
