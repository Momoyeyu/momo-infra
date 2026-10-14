//! `/api/*` 路由树基座：本文件是唯一注册点。
//! 已实装：`healthz`/`readyz`/`events`/`csrf`/`doctor`/`serve status`/
//! `serve components`/`models`/`config`（只读）、`serve {up|down|restart}`/
//! `models/pull`/`config`(PUT)/`config/rollback`（POST，config 写回走
//! env_file 校验→.bak→原子写）、`serve logs/{name}/stream`（SSE）、
//! `pipelines`（训练页 wizard 定义清单）与 `jobs`
//! （REST + SSE 日志 + promote/metrics）；其余子路由为 501 占位——真实
//! handler 由后续 issue 挂接。handler 只做 HTTP↔core 适配，业务复用
//! `cmds`/`serve`/`models`/`manifest`/`env_cfg`/`env_file`/`pipeline` 现有
//! core 函数；出入参一律 `api::dto` 结构化类型（不手写 `json!`），
//! 与 CLI `--json` 同 schema；错误统一 `{schema:1, error:{code,message}}`。

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};
use tokio_stream::{Stream, StreamExt};

use super::WebState;
use crate::api::dto;
use crate::cli::Profile;
use crate::env_file;
use crate::output::AppError;

/// PUT/POST body 上限（env 写回 diff 很小，256 KiB 足够防御滥用）。
const BODY_LIMIT: usize = 256 * 1024;

/// env 写回/回滚响应的生效提示（config.md §5：配置变更 = 改 env + 重启）。
const RESTART_HINT: &str = "env changes take effect after `momoctl serve restart`";

/// `/api` 子树。挂到外层 `nest("/api", ..)`；子树内未命中走 JSON 404，
/// 与 SPA fallback（index.html）分流，互不误判。
pub fn router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/events", get(events))
        .route("/csrf", get(csrf))
        // 实装的只读端点用 any + 方法门控：非 GET/HEAD 一律 JSON 405
        // （保证 /api 错误形态唯一，不落框架默认空 405）。
        .route("/doctor", any(doctor))
        .route("/serve/status", any(serve_status))
        .route("/serve/components", any(serve_components))
        // 实装的只读端点同样 any + 方法门控：非 GET → JSON 405 而非框架空 405。
        .route("/serve/logs/{name}/stream", any(serve_log_stream))
        // 写端点：POST /api/serve/{up|down|restart}（CSRF 经守卫中间件）；
        // 未知 action 由 handler 给 JSON 404（{action} 与 {*rest} 路由冲突，
        // /serve 下不再挂通配占位——未命中路径走 fallback JSON 404）。
        .route("/serve/{action}", any(serve_action))
        // 占位路由：any 接全方法（后续写操作经 CSRF 中间件后挂真实 handler）。
        .route("/doctor/{*rest}", any(placeholder))
        .route("/serve", any(placeholder))
        .route("/models", any(models))
        // 写端点：POST /api/models/pull {name}（CSRF 经守卫中间件）；
        // 与 /models/{*rest} 不冲突——静态段优先于通配。
        .route("/models/pull", any(models_pull))
        .route("/models/{*rest}", any(placeholder))
        // jobs 已挂真实 handler（web/jobs.rs：REST + SSE 日志 + promote/metrics）。
        .nest("/jobs", super::jobs::router())
        // 训练页 wizard 数据源：pipeline 定义清单（复用 pipeline::discover core）。
        .route("/pipelines", any(pipelines))
        .route("/pipelines/{*rest}", any(placeholder))
        // 配置页：GET 读当前值（敏感掩码）；PUT 校验→.bak→原子写回；POST rollback 恢复 .bak。
        .route("/config", any(config))
        .route("/config/rollback", any(config_rollback))
        // 渲染 schema 单源下发（issue #81）：前端按 keys[].type/component 出控件与分组。
        .route("/config/schema", any(config_schema))
        .route("/config/{*rest}", any(placeholder))
        .fallback(not_found)
        .with_state(state)
}

/// `GET /api[/]`：路由索引，便于探测已注册面。
/// 外层对 `/api/` 显式挂本 handler（nest 只覆盖 `/api` 与 `/api/{*rest}`，
/// 恰好 `/api/` 会落到 SPA fallback，须显式分流）。
pub(crate) async fn index() -> Json<dto::IndexReport> {
    Json(dto::IndexReport {
        schema: dto::SCHEMA_VERSION,
        status: "ok",
        routes: vec![
            "GET /api/healthz",
            "GET /api/readyz",
            "GET /api/events",
            "GET /api/csrf",
            "GET /api/doctor",
            "GET /api/serve/status",
            "GET /api/serve/components",
            "POST /api/serve/{up|down|restart}",
            "GET /api/serve/logs/{name}/stream (SSE)",
            "GET /api/models",
            "POST /api/models/pull",
            "/api/models/* (placeholder)",
            "GET /api/jobs",
            "POST /api/jobs",
            "GET /api/jobs/{id}",
            "POST /api/jobs/{id}/cancel",
            "POST /api/jobs/{id}/recover",
            "POST /api/jobs/{id}/promote",
            "GET /api/jobs/{id}/metrics",
            "GET /api/jobs/{id}/logs?cursor= (SSE)",
            "GET /api/pipelines",
            "/api/pipelines/* (placeholder)",
            "GET /api/config",
            "PUT /api/config",
            "POST /api/config/rollback",
            "/api/config/* (placeholder)",
            "GET /api/config/schema",
        ],
    })
}

/// `GET /api/healthz`：进程级存活探针（server 自身，不聚合下游组件）。
async fn healthz() -> Json<dto::HealthReport> {
    Json(dto::HealthReport {
        schema: dto::SCHEMA_VERSION,
        status: "ok",
    })
}

/// `GET /api/readyz`：就绪探针。基座无下游依赖，恒 ok；
/// 后续可在 handler 内聚合组件 readiness（复用 serve 探活）。
async fn readyz() -> Json<dto::HealthReport> {
    Json(dto::HealthReport {
        schema: dto::SCHEMA_VERSION,
        status: "ok",
    })
}

/// `GET /api/events`：SSE 通道基座。broadcast 转发 `WebState::events_tx`，
/// 慢消费者 Lagged 时跳过失真事件、保持连接（jobs/serve 日志流复用此通路）。
async fn events(
    State(state): State<Arc<WebState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.events_tx.subscribe()).filter_map(|msg| match msg {
        Ok(data) => Some(Ok(Event::default().event("message").data(data))),
        Err(_) => None,
    });
    Sse::new(stream)
}

/// `GET /api/csrf`：签发进程级 CSRF token，state-changing 请求以
/// `X-CSRF-Token` 头回带（guard::require_auth 校验）。
/// 响应不带 CORS 允许头——跨源页面读不到该值（SOP），token 不会泄漏给第三方站点。
async fn csrf(State(state): State<Arc<WebState>>) -> Json<dto::CsrfToken> {
    Json(dto::CsrfToken {
        schema: dto::SCHEMA_VERSION,
        csrf_token: state.csrf_token.clone(),
    })
}

/// `GET /api/doctor[?profile=]`：环境检查矩阵。
/// core 与 `momoctl doctor` 同为 `cmds::doctor::collect_report`，
/// 响应即 `DoctorReport`——与 `doctor --json` 逐字节同型（契约测试锁定）。
/// 检查有 fail 仍 200：fail 是检查数据而非 HTTP 错误（对应 CLI exit 3 语义）。
async fn doctor(method: Method, Query(q): Query<dto::ProfileQuery>) -> Response {
    let Some(profile) = parse_profile(&method, q.profile.as_deref()) else {
        return bad_method_or_profile(&method, q.profile.as_deref());
    };
    // collect_report 做进程/网络/文件探测（秒级阻塞），offload 到 blocking 池。
    match tokio::task::spawn_blocking(move || crate::cmds::doctor::collect_report(profile)).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("doctor task failed: {e}"),
        ),
    }
}

/// `GET /api/serve/status[?profile=]`：组件存活/就绪快照。
/// core 与 `momoctl serve status` 同为 `cmds::serve::status_report`（ServeReport）。
/// 恒 200 下发报告；组件 fail 体现在 checks（对应 CLI exit 3 语义）。
async fn serve_status(method: Method, Query(q): Query<dto::ProfileQuery>) -> Response {
    let Some(profile) = parse_profile(&method, q.profile.as_deref()) else {
        return bad_method_or_profile(&method, q.profile.as_deref());
    };
    match tokio::task::spawn_blocking(move || crate::cmds::serve::status_report(profile)).await {
        Ok(Ok((report, _code))) => Json(report).into_response(),
        Ok(Err(e)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("serve status failed: {e}"),
        ),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("serve status task failed: {e}"),
        ),
    }
}

/// `GET /api/serve/components[?profile=]`：内置 profile 组件表。
/// 渲染组件页表格（order/external/local_endpoint_only 标记）；
/// 只解析内嵌 toml，无 IO，可直接同步返回。
async fn serve_components(method: Method, Query(q): Query<dto::ProfileQuery>) -> Response {
    let Some(profile) = parse_profile(&method, q.profile.as_deref()) else {
        return bad_method_or_profile(&method, q.profile.as_deref());
    };
    match crate::cmds::serve::components_report(profile) {
        Ok(report) => Json(report).into_response(),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("serve components failed: {e}"),
        ),
    }
}

/// `POST /api/serve/{up|down|restart}[?profile=]`：组件控制（CSRF 已过守卫）。
/// body `{only:[..]?}`（空 body = 全组件）；与 `momoctl serve <action>` 同 core
/// （`cmds::serve::action_report`），响应 `ServeReport`——逐项 Check 回显，
/// fail 是数据非 HTTP 错误，恒 200；`only` 未命中组件等用法错误 → 400。
async fn serve_action(
    method: Method,
    AxumPath(action): AxumPath<String>,
    Query(q): Query<dto::ProfileQuery>,
    body: Bytes,
) -> Response {
    if method != Method::POST {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("{method} is not allowed; use POST"),
        );
    }
    if !matches!(action.as_str(), "up" | "down" | "restart") {
        return error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no route /api/serve/{action}"),
        );
    }
    let Some(profile) = parse_profile_param(q.profile.as_deref()) else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!(
                "invalid profile '{}'; expected normal|degraded",
                q.profile.as_deref().unwrap_or("")
            ),
        );
    };
    let req: dto::ServeActionRequest = if body.is_empty() {
        dto::ServeActionRequest { only: Vec::new() }
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!("invalid body: {e}"),
                )
            }
        }
    };
    let action_name = action.clone();
    match tokio::task::spawn_blocking(move || {
        crate::cmds::serve::action_report(&action, profile, req.only)
    })
    .await
    {
        Ok(Ok((report, _code))) => Json(report).into_response(),
        Ok(Err(AppError::Config(m))) => error(StatusCode::BAD_REQUEST, "bad_request", m),
        Ok(Err(e)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("serve {action_name} failed: {e}"),
        ),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("serve {action_name} task failed: {e}"),
        ),
    }
}

/// `GET /api/serve/logs/{name}/stream[?profile=&cursor=]`：组件日志 SSE tail。
/// 无 cursor 时先发有界 backlog（尾部 `LOG_BACKLOG_LINES` 行），之后按字节
/// offset 轮询推送；每帧 `id:` = 已消费字节 offset，`data:` = `LogBatch` JSON。
/// 断线后客户端带最后 cursor（或 EventSource 的 Last-Event-ID 头）重连，
/// 从该 offset 续推，不重放无限 backlog。`?profile=` 仅决定组件名校验口径。
async fn serve_log_stream(
    method: Method,
    State(_state): State<Arc<WebState>>,
    AxumPath(name): AxumPath<String>,
    Query(q): Query<dto::LogStreamQuery>,
    headers: HeaderMap,
) -> Response {
    if !matches!(method, Method::GET | Method::HEAD) {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("{method} is not allowed on this read-only endpoint"),
        );
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("invalid component name '{name}'"),
        );
    }
    let Some(profile) = parse_profile_param(q.profile.as_deref()) else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!(
                "invalid profile '{}'; expected normal|degraded",
                q.profile.as_deref().unwrap_or("")
            ),
        );
    };
    match crate::cmds::serve::components_report(profile) {
        Ok(report) if report.components.iter().any(|c| c.name == name) => {}
        Ok(_) => {
            return error(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no component '{name}' in this profile"),
            )
        }
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                format!("serve components failed: {e}"),
            )
        }
    }

    // 初始 offset：?cursor= > Last-Event-ID > 文件末尾（只发 backlog+新增）。
    let header_cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let cursor = q.cursor.or(header_cursor);

    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(LOG_STREAM_CAP);
    let path = crate::serve::proc::log_dir().join(format!("{name}.log"));
    // 文件 tail 是阻塞 IO + 定间隔轮询：每连接一条 OS thread 而非占 blocking 池。
    std::thread::spawn(move || tail_log_file(name, path, cursor, tx));
    Sse::new(ReceiverStream::new(rx).map(Ok::<Event, Infallible>))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `?cursor=` 缺省时的 backlog 行数（有界，防初始重放全量日志）。
const LOG_BACKLOG_LINES: usize = 50;
/// SSE 推送 channel 容量（行批，不是字节）。
const LOG_STREAM_CAP: usize = 32;
/// 轮询间隔（与 CLI `serve logs -f` 同口径 300ms）。
const LOG_POLL: std::time::Duration = std::time::Duration::from_millis(300);

/// SSE tail 线程：先发 backlog（无 cursor 时），随后按 offset 轮询追加内容。
/// 每帧 `id` = 消费后 offset；客户端断开（channel 关闭）即退出。
fn tail_log_file(
    name: String,
    path: std::path::PathBuf,
    cursor: Option<u64>,
    tx: tokio::sync::mpsc::Sender<Event>,
) {
    let mut offset: u64 = match cursor {
        Some(c) => c,
        None => {
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            // 无 cursor：有界 backlog（最后 N 行），之后从文件末尾续推。
            if len > 0 {
                if let Ok(lines) = crate::cmds::serve::read_last_lines(&path, LOG_BACKLOG_LINES) {
                    if !lines.is_empty() && !send_log_batch(&tx, &name, lines, len) {
                        return;
                    }
                }
            }
            len
        }
    };
    loop {
        if tx.is_closed() {
            return;
        }
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if len < offset {
            offset = 0; // 日志截断/轮转：从头读
        }
        if len > offset {
            if let Ok(mut f) = std::fs::File::open(&path) {
                if f.seek(SeekFrom::Start(offset)).is_ok() {
                    let mut buf = Vec::new();
                    if f.read_to_end(&mut buf).is_ok() {
                        offset = len;
                        let text = String::from_utf8_lossy(&buf);
                        let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
                        if !lines.is_empty() && !send_log_batch(&tx, &name, lines, offset) {
                            return;
                        }
                    }
                }
            }
        }
        std::thread::sleep(LOG_POLL);
    }
}

/// 组一帧 `event:log` + `id:<offset>` + `data:{component,lines}` 发送；
/// 客户端断开返回 false（调用方退出线程）。
fn send_log_batch(
    tx: &tokio::sync::mpsc::Sender<Event>,
    name: &str,
    lines: Vec<String>,
    offset: u64,
) -> bool {
    let batch = dto::LogBatch {
        component: name,
        lines: lines.iter().map(String::as_str).collect(),
    };
    let data = serde_json::to_string(&batch).unwrap_or_else(|_| "{}".into());
    tx.blocking_send(
        Event::default()
            .event("log")
            .id(offset.to_string())
            .data(data),
    )
    .is_ok()
}

/// `GET /api/models`：MANIFEST + models.lock merge 清单 + 激活选择（只读）。
/// core 为 `models::collect_report`；MANIFEST 缺失返回 200 + `manifest:null`
/// + warn（区分「未部署清单」与故障），其余 core 错误 → 500。
async fn models(method: Method) -> Response {
    if !matches!(method, Method::GET | Method::HEAD) {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("{method} is not allowed on this read-only endpoint"),
        );
    }
    match tokio::task::spawn_blocking(crate::models::collect_report).await {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(e)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("models report failed: {e}"),
        ),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("models report task failed: {e}"),
        ),
    }
}

/// `GET /api/pipelines`：pipeline 定义清单（训练页 wizard 数据源）。
/// core 与 `momoctl pipeline list` 同为 `pipeline::discover` +
/// `default_pipelines_dir`；定义目录未找到 → 200 + 空清单 + `dir:""`
/// （「未部署定义」是数据不是故障，同 `GET /api/models` 的 manifest:null 口径）。
async fn pipelines(method: Method) -> Response {
    if !matches!(method, Method::GET | Method::HEAD) {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("{method} is not allowed on this read-only endpoint"),
        );
    }
    match tokio::task::spawn_blocking(|| -> Result<dto::PipelineListReport, AppError> {
        let Some(dir) = crate::pipeline::default_pipelines_dir() else {
            return Ok(dto::PipelineListReport {
                schema: dto::SCHEMA_VERSION,
                command: "pipeline list".into(),
                dir: String::new(),
                pipelines: Vec::new(),
            });
        };
        let defs = crate::pipeline::discover(&dir)?;
        let pipelines = defs
            .iter()
            .map(|(path, d)| dto::PipelineInfo {
                name: d.name.clone(),
                description: d.description.clone(),
                artifact_dir: d.artifact_dir.clone(),
                activate_env: d.activate_env.clone(),
                steps: d.steps.iter().map(|s| s.name.clone()).collect(),
                vars: d
                    .vars
                    .iter()
                    .map(|(name, v)| dto::PipelineVarInfo {
                        name: name.clone(),
                        description: v.description.clone(),
                        required: v.required,
                        default: v.default.clone(),
                    })
                    .collect(),
                path: path.display().to_string(),
            })
            .collect();
        Ok(dto::PipelineListReport {
            schema: dto::SCHEMA_VERSION,
            command: "pipeline list".into(),
            dir: dir.display().to_string(),
            pipelines,
        })
    })
    .await
    {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(e)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("pipelines discover failed: {e}"),
        ),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("pipelines task failed: {e}"),
        ),
    }
}

/// `POST /api/models/pull`：拉取单个 MANIFEST 条目（`{name}`）。
/// 与 `momoctl pull --only <name>` 同 core（`cmds::pull::pull_report`），
/// spawn_blocking 直跑等结果——逐文件进度流待 jobs runner（#54）。
/// 响应 `PullReport`（fail 是数据非 HTTP 错误，恒 200）；
/// body/name 非法、MANIFEST/条目缺失 → 400。
async fn models_pull(method: Method, body: Bytes) -> Response {
    if method != Method::POST {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("{method} is not allowed; use POST"),
        );
    }
    let req: dto::ModelsPullRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("invalid body: {e}"),
            )
        }
    };
    if req.name.trim().is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "field 'name' is required",
        );
    }
    match tokio::task::spawn_blocking(move || crate::cmds::pull::pull_report(None, vec![req.name]))
        .await
    {
        Ok(Ok((report, _code))) => Json(report).into_response(),
        Ok(Err(AppError::Config(m))) => error(StatusCode::BAD_REQUEST, "bad_request", m),
        Ok(Err(e)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("pull failed: {e}"),
        ),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("pull task failed: {e}"),
        ),
    }
}
// ---------- 配置页：/api/config（issue #56） ----------
//
// 读写对象恒为 `.env` 同目录的 `.env.<profile>`（profile 白名单，
// env_file::profile_env_path 构造，无路径穿越面）。GET/日志/diff
// 对敏感键只回掩码（前 4 位 + `…`），绝不回显明文（config.md §4）。

/// `GET|PUT /api/config` 方法门控；其余方法 → 405 JSON。
/// 注意：`&Request` 不得跨 `.await` 持有（Body 是 UnsyncBoxBody，!Sync），
/// 入口先把 method/query 提成 owned 值再分派。
async fn config(State(state): State<Arc<WebState>>, req: Request) -> Response {
    let q_profile = query_profile(req.uri());
    match *req.method() {
        Method::GET | Method::HEAD => config_get(&state, q_profile).await,
        Method::PUT => config_put(&state, req, q_profile).await,
        ref m => error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("{m} is not allowed on /api/config (GET|PUT only)"),
        ),
    }
}

/// `GET /api/config[?profile=]`：返回 `{profile, active_profile, env_file,
/// file_exists, entries:[{key, value, sensitive, set}]}`，value 已过掩码。
async fn config_get(state: &Arc<WebState>, q_profile: Option<String>) -> Response {
    let profile = match resolve_profile(q_profile.as_deref()) {
        Ok(p) => p,
        Err(msg) => return error(StatusCode::BAD_REQUEST, "bad_request", msg),
    };
    let env_link = state.env_link.clone();
    match tokio::task::spawn_blocking(move || build_config_report(&env_link, &profile)).await {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(resp)) => *resp,
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("config read task failed: {e}"),
        ),
    }
}

/// `PUT /api/config {profile?, changes:{key:value}}`：
/// 先校验（键须 `MOMO_` 前缀且存在于 config.schema.toml；值按 schema 值域复核），
/// 非法 → 422 字段级归因、不写盘；通过 → `.bak` 备份 → tmp+rename 原子写 → 掩码 diff。
async fn config_put(state: &Arc<WebState>, req: Request, q_profile: Option<String>) -> Response {
    let body = match axum::body::to_bytes(req.into_body(), BODY_LIMIT).await {
        Ok(b) => b,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("cannot read request body: {e}"),
            )
        }
    };
    let put: dto::ConfigPutRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("invalid JSON body: {e}"),
            )
        }
    };
    if put.changes.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "changes must not be empty",
        );
    }
    let profile = match resolve_profile(put.profile.as_deref().or(q_profile.as_deref())) {
        Ok(p) => p,
        Err(msg) => {
            let mut fields = BTreeMap::new();
            fields.insert("profile".to_string(), msg);
            return fields_error(fields);
        }
    };
    // 键存在性 + schema 值域复核（与前端即时校验同一份 schema 单源）。
    let env_dir = env_dir(&state.env_link);
    let schema = match env_file::load_schema(env_dir) {
        Ok(s) => s,
        Err(e) => return *internal(e),
    };
    let mut fields = BTreeMap::new();
    for (k, v) in &put.changes {
        if !k.starts_with("MOMO_") {
            fields.insert(k.clone(), "key must start with MOMO_".to_string());
            continue;
        }
        match schema.get(k) {
            None => {
                fields.insert(k.clone(), "unknown key (not in config.schema.toml)".into());
            }
            Some(spec) => {
                if let Err(msg) = env_file::validate_value(spec, v) {
                    fields.insert(k.clone(), msg);
                }
            }
        }
    }
    if !fields.is_empty() {
        return fields_error(fields);
    }

    let sensitive = env_file::sensitive_keys(Some(&schema));
    let env_link = state.env_link.clone();
    let changes = put.changes;
    match tokio::task::spawn_blocking(move || {
        apply_config_changes(&env_link, &profile, &changes, &sensitive)
    })
    .await
    {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(resp)) => *resp,
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("config write task failed: {e}"),
        ),
    }
}

/// `POST /api/config/rollback {profile?}`：从 `.env.<profile>.bak` 原子恢复，
/// 返回掩码 diff；无备份 → 404。GET 等非 POST → 405。
async fn config_rollback(State(state): State<Arc<WebState>>, req: Request) -> Response {
    if *req.method() != Method::POST {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!(
                "{} is not allowed on /api/config/rollback (POST only)",
                req.method()
            ),
        );
    }
    let q_profile = query_profile(req.uri());
    let body = match axum::body::to_bytes(req.into_body(), BODY_LIMIT).await {
        Ok(b) => b,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("cannot read request body: {e}"),
            )
        }
    };
    let rb: dto::ConfigRollbackRequest = if body.is_empty() {
        dto::ConfigRollbackRequest { profile: None }
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!("invalid JSON body: {e}"),
                )
            }
        }
    };
    let profile = match resolve_profile(rb.profile.as_deref().or(q_profile.as_deref())) {
        Ok(p) => p,
        Err(msg) => {
            let mut fields = BTreeMap::new();
            fields.insert("profile".to_string(), msg);
            return fields_error(fields);
        }
    };
    let env_link = state.env_link.clone();
    match tokio::task::spawn_blocking(move || apply_config_rollback(&env_link, &profile)).await {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(resp)) => *resp,
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("config rollback task failed: {e}"),
        ),
    }
}

/// `GET /api/config/schema`（issue #81）：`{schema:1, keys:[{name,type,default,
/// values?,sensitive,description,component}]}`——config.schema.toml 单源直出，
/// 服务端不维护第二份键表。`type` 对外输出 `str`（schema 内部仍用 `string`）。
/// GET/HEAD only；其余方法 → 405 JSON。
async fn config_schema(State(state): State<Arc<WebState>>, req: Request) -> Response {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!(
                "{} is not allowed on /api/config/schema (GET only)",
                req.method()
            ),
        );
    }
    let env_link = state.env_link.clone();
    match tokio::task::spawn_blocking(move || env_file::load_schema(env_dir(&env_link))).await {
        Ok(Ok(schema)) => {
            let keys = schema
                .key
                .iter()
                .map(|k| dto::ConfigKeySchema {
                    name: k.name.clone(),
                    // schema 文件口径 `string` → API 口径 `str`（issue #81 契约）。
                    kind: if k.kind == "string" { "str" } else { &k.kind }.to_string(),
                    default: k.default.clone(),
                    values: k.values.clone(),
                    sensitive: k.sensitive,
                    description: k.description.clone(),
                    component: k.component.clone(),
                })
                .collect();
            Json(dto::ConfigSchemaReport {
                schema: dto::SCHEMA_VERSION,
                keys,
            })
            .into_response()
        }
        Ok(Err(e)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("config schema load failed: {e}"),
        ),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("config schema task failed: {e}"),
        ),
    }
}

// ---------- 配置页 core（blocking 池执行的文件 IO） ----------

/// `.env` 所在目录（`.env.<profile>` 与 schema 查找都以它为锚）。
fn env_dir(env_link: &Path) -> &Path {
    env_link.parent().unwrap_or(Path::new("."))
}

/// `?profile=` 提取（解析失败按缺省处理）。
fn query_profile(uri: &axum::http::Uri) -> Option<String> {
    Query::<dto::ProfileQuery>::try_from_uri(uri)
        .ok()
        .and_then(|q| q.0.profile)
}

/// profile 解析：显式值限 `normal|degraded`（大小写归一）；
/// 缺省按 `active_profile` 状态文件 > MOMO_PROFILE > normal。
fn resolve_profile(param: Option<&str>) -> Result<String, String> {
    match param.map(|p| p.to_ascii_lowercase()) {
        Some(p) if env_file::PROFILES.contains(&p.as_str()) => Ok(p),
        Some(p) => Err(format!("invalid profile '{p}'; expected normal|degraded")),
        None => Ok(env_file::active_profile()),
    }
}

/// GET core：`.env.<profile>` → masked entries。文件缺失时 entries 全 unset
/// （`file_exists:false` 供前端提示；PUT 对缺失文件 404）。
/// 错误侧 Box<Response>（result_large_err：Response >128B）。
fn build_config_report(env_link: &Path, profile: &str) -> Result<dto::ConfigReport, Box<Response>> {
    let path = env_file::profile_env_path(env_link, profile).map_err(internal)?;
    let file_exists = path.is_file();
    let map = if file_exists {
        env_file::parse_env_file(&path).map_err(internal)?
    } else {
        BTreeMap::new()
    };
    let schema = env_file::load_schema(env_dir(env_link)).map_err(internal)?;
    let sensitive = env_file::sensitive_keys(Some(&schema));
    let mut entries = Vec::with_capacity(schema.key.len() + map.len());
    // schema 键序在前（页面按 schema 渲染），文件内 schema 外键
    // （passthrough/游离键）按字典序列尾——只读可见，PUT 拒收非 MOMO_ 键。
    for k in &schema.key {
        entries.push(config_entry(&k.name, &map, &sensitive));
    }
    for k in map.keys().filter(|k| schema.get(k).is_none()) {
        entries.push(config_entry(k, &map, &sensitive));
    }
    Ok(dto::ConfigReport {
        schema: dto::SCHEMA_VERSION,
        profile: profile.to_string(),
        active_profile: env_file::active_profile(),
        env_file: path.display().to_string(),
        file_exists,
        entries,
    })
}

/// 单条 entry：非空取值掩码（敏感）或原值；缺失/空 → set=false、value=null。
fn config_entry(
    key: &str,
    map: &BTreeMap<String, String>,
    sensitive: &std::collections::BTreeSet<String>,
) -> dto::ConfigEntry {
    let raw = map.get(key).filter(|v| !v.is_empty());
    let hide = sensitive.contains(key) || env_file::is_sensitive_name(key);
    dto::ConfigEntry {
        key: key.to_string(),
        value: raw.map(|v| if hide { env_file::mask(v) } else { v.clone() }),
        sensitive: hide,
        set: raw.is_some(),
    }
}

/// PUT core：备份 → 保序重写 → 原子替换 → 掩码 diff。
/// 校验已在 handler 完成；此处只做文件 IO，任何一步失败 active 不变。
fn apply_config_changes(
    env_link: &Path,
    profile: &str,
    changes: &BTreeMap<String, String>,
    sensitive: &std::collections::BTreeSet<String>,
) -> Result<dto::ConfigWriteReport, Box<Response>> {
    let path = env_file::profile_env_path(env_link, profile).map_err(internal)?;
    if !path.is_file() {
        return Err(Box::new(error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!(
                "{} not found; run `momoctl switch --to {profile}` to create it",
                path.display()
            ),
        )));
    }
    let old = env_file::parse_env_file(&path).map_err(internal)?;
    let text = std::fs::read_to_string(&path)
        .map_err(AppError::from)
        .map_err(internal)?;
    let bak = env_file::backup_file(&path).map_err(internal)?;
    let new_text = env_file::apply_changes(&text, changes);
    env_file::atomic_write(&path, &new_text).map_err(internal)?;
    let new = env_file::parse_env_file(&path).map_err(internal)?;
    Ok(dto::ConfigWriteReport {
        schema: dto::SCHEMA_VERSION,
        profile: profile.to_string(),
        env_file: path.display().to_string(),
        backup: bak.display().to_string(),
        diff: env_file::env_diff(&old, &new, sensitive),
        restart_hint: RESTART_HINT.to_string(),
    })
}

/// rollback core：`.bak` → 原子恢复 active（bak 保留），返回掩码 diff。
fn apply_config_rollback(
    env_link: &Path,
    profile: &str,
) -> Result<dto::ConfigWriteReport, Box<Response>> {
    let path = env_file::profile_env_path(env_link, profile).map_err(internal)?;
    let bak = env_file::backup_path(&path);
    if !bak.is_file() {
        return Err(Box::new(error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no backup {}; nothing to roll back", bak.display()),
        )));
    }
    let old = if path.is_file() {
        env_file::parse_env_file(&path).map_err(internal)?
    } else {
        BTreeMap::new()
    };
    let from = env_file::restore_backup(&path).map_err(internal)?;
    let new = env_file::parse_env_file(&path).map_err(internal)?;
    let schema = env_file::load_schema(env_dir(env_link)).map_err(internal)?;
    let sensitive = env_file::sensitive_keys(Some(&schema));
    Ok(dto::ConfigWriteReport {
        schema: dto::SCHEMA_VERSION,
        profile: profile.to_string(),
        env_file: path.display().to_string(),
        backup: from.display().to_string(),
        diff: env_file::env_diff(&old, &new, &sensitive),
        restart_hint: RESTART_HINT.to_string(),
    })
}

/// 字段级校验错误：422 + `error.fields`（键 → 归因），前端逐字段回显。
fn fields_error(fields: BTreeMap<String, String>) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(dto::ApiError::with_fields(
            "validation_failed",
            "config changes failed validation; no file was modified",
            fields,
        )),
    )
        .into_response()
}

/// core IO/AppError → 500（校验错误在 handler 已拦，走到这里均为运行时失败）。
/// 返回 Box：收窄大 Err 变体（result_large_err：Response >128B）。
fn internal(e: AppError) -> Box<Response> {
    Box::new(error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        e.to_string(),
    ))
}

/// GET/HEAD → Some(profile 解析结果)；其余方法或非法 profile → None
/// （由 `bad_method_or_profile` 产出对应错误响应）。
fn parse_profile(method: &Method, profile: Option<&str>) -> Option<Option<Profile>> {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return None;
    }
    parse_profile_param(profile)
}

/// `?profile=` 值解析：缺省/合法 → Some；非法 → None（调用方给 400）。
fn parse_profile_param(profile: Option<&str>) -> Option<Option<Profile>> {
    match profile.map(|s| s.to_ascii_lowercase()).as_deref() {
        None => Some(None),
        Some("normal") => Some(Some(Profile::Normal)),
        Some("degraded") => Some(Some(Profile::Degraded)),
        Some(_) => None,
    }
}

/// `parse_profile` 失败的错误分流：非只读方法 → 405；非法 profile 值 → 400。
fn bad_method_or_profile(method: &Method, profile: Option<&str>) -> Response {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("{method} is not allowed on this read-only endpoint"),
        );
    }
    error(
        StatusCode::BAD_REQUEST,
        "bad_request",
        format!(
            "invalid profile '{}'; expected normal|degraded",
            profile.unwrap_or("")
        ),
    )
}

/// 统一错误出口：`{schema:1, error:{code,message}}`（dto::ApiError 口径）。
/// guard 中间件与 handler 共用，保证 /api 错误形态唯一。
pub(crate) fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (status, Json(dto::ApiError::new(code, message))).into_response()
}

/// 占位 handler：501 + 错误 DTO。挂接点前任何方法均无 core 副作用。
async fn placeholder(uri: Uri) -> Response {
    error(
        StatusCode::NOT_IMPLEMENTED,
        "not_implemented",
        format!("route {} is not implemented yet", uri.path()),
    )
}

/// `/api` 子树内未命中 → JSON 404（区别于 SPA fallback 的 index.html）。
async fn not_found(uri: Uri) -> Response {
    error(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("no route {}", uri.path()),
    )
}
