//! `momoctl web` 的 axum server 基座：`/api/*` 路由树 + include_dir 内嵌 SPA。
//! `/api` 子树挂请求级认证链（`guard::require_auth`：Bearer token + CSRF，
//! 口径见 momo-infra.md Web 控制台节）；SPA 静态资产不经认证层。

pub mod api;
pub mod guard;
pub mod jobs;
pub mod spa;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::middleware;
use axum::Router;
use tokio::sync::broadcast;

use crate::output::AppError;

/// `/api/events` broadcast 通道容量：超出后慢消费者收到 Lagged 并被跳过。
/// 后续 jobs/serve 日志流复用同一通道。
pub const EVENT_CHANNEL_CAP: usize = 256;

/// web server 跨 handler 共享状态。
#[derive(Debug)]
pub struct WebState {
    /// SSE 事件源：jobs/serve 日志等事件统一经此 broadcast 下发。
    pub events_tx: broadcast::Sender<String>,
    /// `MOMO_WEB_TOKEN`：Some → `/api` 全请求须 `Authorization: Bearer`；
    /// None → 仅 loopback peer + 回环 Host 放行。
    /// 敏感值不回显、不落日志（config.md §4）。
    pub token: Option<String>,
    /// 进程级随机 CSRF token：`GET /api/csrf` 签发，
    /// state-changing 请求须 `X-CSRF-Token` 回带（guard::require_auth）。
    pub csrf_token: String,
    /// `.env` 链接路径（启动时按 `--env-file` > dotenvy > `./.env` 解析）；
    /// 配置页读写的 `.env.<profile>` 在其同目录（env_file::profile_env_path）。
    pub env_link: PathBuf,
}

/// 组装完整 Router：`/api` 子树集中注册并统一叠认证层，其余路径走 SPA fallback。
pub fn router(state: Arc<WebState>) -> Router {
    Router::new()
        // `/api/`（带尾斜杠）不匹配 nest 的 `/api/{*rest}`，会落 SPA fallback；
        // 显式挂到 api 索引并叠同一认证层，保证 api 命名空间不回 HTML 也不绕开守卫。
        .route(
            "/api/",
            axum::routing::get(api::index).route_layer(middleware::from_fn_with_state(
                state.clone(),
                guard::require_auth,
            )),
        )
        .nest(
            "/api",
            api::router(state.clone())
                .layer(middleware::from_fn_with_state(state, guard::require_auth)),
        )
        .fallback(spa::fallback)
}

/// 在已 bind 好的 listener 上服务至 Ctrl-C。
/// listener 由调用方先 bind：CLI 可在启动日志里打印真实 local_addr（含 :0 随机端口）。
/// `into_make_service_with_connect_info` 让认证层拿到对端 IP（loopback 判定）。
pub async fn serve(listener: tokio::net::TcpListener, app: Router) -> Result<(), AppError> {
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(AppError::Io)
}

async fn shutdown_signal() {
    // Ctrl-C 两平台均有；注册失败（如嵌入式无信号支持）时静默——进程仍可被外部终止。
    let _ = tokio::signal::ctrl_c().await;
}
