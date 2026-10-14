//! `/api` REST 与 `momoctl --json` 的契约测试（issue #46）：
//! - 公开端点断言响应 JSON schema（字段名/类型/必选性）；
//! - 同一操作两侧对跑：`GET /api/doctor` ↔ `doctor --json`、
//!   `GET /api/serve/status` ↔ `serve status --json`，防 CLI/Web 漂移；
//! - 错误一律 `{schema:1, error:{code,message}}`。
//!
//! HTTP 侧走 `tower::ServiceExt::oneshot` 直连 Router，不起端口。

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use momo_infra::web::{self, WebState};
use serde_json::Value;
use tokio::sync::broadcast;
use tower::ServiceExt;

const CSRF: &str = "test-csrf-token";
const TOKEN: &str = "test-web-token";

fn state(token: Option<&str>) -> Arc<WebState> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "momoctl-contract-state-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    Arc::new(WebState {
        events_tx: broadcast::channel(web::EVENT_CHANNEL_CAP).0,
        token: token.map(Into::into),
        csrf_token: CSRF.into(),
        env_link: dir.join(".env"),
    })
}

/// 构造请求：oneshot 无对端地址，注入 loopback ConnectInfo + 回环 Host
/// 模拟本机客户端（无 token 模式的两个放行条件）。
fn req(method: &str, uri: &str) -> Request<Body> {
    let mut r = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7900")
        .body(Body::empty())
        .unwrap();
    r.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
    ));
    r
}

async fn call(app: axum::Router, r: Request<Body>) -> (StatusCode, Value) {
    let res = app.oneshot(r).await.unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| panic!("response is not JSON: {}", String::from_utf8_lossy(&body)));
    (status, v)
}

/// `momoctl <args> --json --env-file <empty>`：与 REST 对跑时隔离用户 .env，
/// 并移除会影响 profile/状态的宿主 env。
fn cli_json(args: &[&str]) -> Value {
    let env_file = std::env::temp_dir().join(format!("momoctl-contract-{}", std::process::id()));
    std::fs::write(&env_file, "").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_momoctl"))
        .args(args)
        .args(["--json", "--env-file"])
        .arg(&env_file)
        .env_remove("MOMO_PROFILE")
        .env_remove("MOMO_WEB_TOKEN")
        .output()
        .expect("spawn momoctl");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "momoctl {args:?} did not emit JSON (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// `error` 包络断言：{schema:1, error:{code:<code>, message:string}}。
fn assert_error_envelope(v: &Value, code: &str) {
    assert_eq!(v["schema"], 1, "{v}");
    assert_eq!(v["error"]["code"], code, "{v}");
    assert!(v["error"]["message"].is_string(), "{v}");
}

/// `Check` DTO 形态：{id:string, status:ok|warn|fail, detail:string, hint:存在}。
fn assert_check_shape(c: &Value) {
    assert!(c["id"].is_string(), "check.id: {c}");
    let status = c["status"].as_str().expect("check.status string");
    assert!(
        ["ok", "warn", "fail"].contains(&status),
        "check.status: {c}"
    );
    assert!(c["detail"].is_string(), "check.detail: {c}");
    assert!(c.get("hint").is_some(), "check.hint must be present: {c}");
}

/// `Summary` DTO 形态：{ok,warn,fail: u64}。
fn assert_summary_shape(v: &Value) {
    for k in ["ok", "warn", "fail"] {
        assert!(v["summary"][k].is_u64(), "summary.{k}: {v}");
    }
}

/// `DoctorReport` 形态（CLI/REST 共用断言）。
fn assert_doctor_shape(v: &Value) {
    assert_eq!(v["schema"], 1, "{v}");
    assert!(v["profile"].is_string(), "{v}");
    assert_summary_shape(v);
    let groups = v["groups"].as_array().expect("groups array");
    assert!(!groups.is_empty());
    for g in groups {
        assert!(g["name"].is_string(), "{g}");
        for c in g["checks"].as_array().expect("checks array") {
            assert_check_shape(c);
        }
    }
}

/// `ServeReport` 形态（CLI/REST 共用断言）。
fn assert_serve_shape(v: &Value, command: &str) {
    assert_eq!(v["schema"], 1, "{v}");
    assert_eq!(v["command"], command, "{v}");
    assert!(v["profile"].is_string(), "{v}");
    assert_summary_shape(v);
    for c in v["checks"].as_array().expect("checks array") {
        assert_check_shape(c);
    }
}

/// 对象顶层键集合相等性（字段名一致 = 契约不漂移）。
fn assert_same_keys(a: &Value, b: &Value, path: &str) {
    let ka: Vec<&String> = a.as_object().unwrap().keys().collect();
    let kb: Vec<&String> = b.as_object().unwrap().keys().collect();
    assert_eq!(ka, kb, "key mismatch at {path}");
}

// ---------- 只读端点 schema ----------

#[tokio::test]
async fn healthz_readyz_contract() {
    let app = web::router(state(None));
    for path in ["/api/healthz", "/api/readyz"] {
        let (status, v) = call(app.clone(), req("GET", path)).await;
        assert_eq!(status, 200, "{path}");
        assert_eq!(v["schema"], 1, "{path}");
        assert_eq!(v["status"], "ok", "{path}");
        assert_eq!(
            v.as_object().unwrap().len(),
            2,
            "{path} must have exactly schema+status"
        );
    }
}

#[tokio::test]
async fn index_contract() {
    let app = web::router(state(None));
    for path in ["/api", "/api/"] {
        let (status, v) = call(app.clone(), req("GET", path)).await;
        assert_eq!(status, 200, "{path}");
        assert_eq!(v["schema"], 1, "{path}");
        assert_eq!(v["status"], "ok", "{path}");
        for r in v["routes"].as_array().expect("routes array") {
            assert!(r.is_string(), "{r}");
        }
    }
}

#[tokio::test]
async fn csrf_contract() {
    let app = web::router(state(None));
    let (status, v) = call(app, req("GET", "/api/csrf")).await;
    assert_eq!(status, 200);
    assert_eq!(v["schema"], 1);
    assert_eq!(v["csrf_token"], CSRF);
}

#[tokio::test]
async fn events_contract() {
    // SSE 握手契约：只验状态与 content-type，不消费无界流。
    let app = web::router(state(None));
    let res = app.oneshot(req("GET", "/api/events")).await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
}

// ---------- CLI ↔ REST 同型 ----------

#[tokio::test]
async fn doctor_matches_cli_json() {
    // REST 侧与 CLI 侧读各自进程 env；仅断言 schema 同型与组名集合，
    // 不断言具体检查值（env 差异属正常）。
    let app = web::router(state(None));
    let (status, rest) = call(app, req("GET", "/api/doctor")).await;
    assert_eq!(status, 200);
    assert_doctor_shape(&rest);

    let cli = cli_json(&["doctor"]);
    assert_doctor_shape(&cli);

    assert_same_keys(&rest, &cli, "doctor");
    let rest_groups: Vec<&str> = rest["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    let cli_groups: Vec<&str> = cli["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(rest_groups, cli_groups, "doctor group set must match");
}

#[tokio::test]
async fn serve_status_matches_cli_json() {
    let app = web::router(state(None));
    let (status, rest) = call(app, req("GET", "/api/serve/status")).await;
    assert_eq!(status, 200);
    assert_serve_shape(&rest, "serve status");

    let cli = cli_json(&["serve", "status"]);
    assert_serve_shape(&cli, "serve status");

    assert_same_keys(&rest, &cli, "serve status");
    // 组件表同源：check.id 集合（组件名序列）两侧必须一致。
    let ids = |v: &Value| {
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&rest), ids(&cli), "component set must match");
}

#[tokio::test]
async fn doctor_profile_query() {
    let app = web::router(state(None));
    let (status, v) = call(app.clone(), req("GET", "/api/doctor?profile=degraded")).await;
    assert_eq!(status, 200);
    assert_eq!(v["profile"], "degraded");

    let (status, v) = call(app, req("GET", "/api/doctor?profile=bogus")).await;
    assert_eq!(status, 400);
    assert_error_envelope(&v, "bad_request");
}

// ---------- 错误包络与认证链 ----------

#[tokio::test]
async fn error_envelope_contract() {
    let app = web::router(state(None));

    // 未命中 → 404 not_found
    let (status, v) = call(app.clone(), req("GET", "/api/nope")).await;
    assert_eq!(status, 404);
    assert_error_envelope(&v, "not_found");

    // 占位路由（GET 只读）→ 501 not_implemented
    let (status, v) = call(app.clone(), req("GET", "/api/models/_placeholder")).await;
    assert_eq!(status, 501);
    assert_error_envelope(&v, "not_implemented");

    // 占位路由 mutation：CSRF 通过 → 501（先 403 → 见下）
    let (status, v) = call(app.clone(), req("POST", "/api/models/_placeholder")).await;
    assert_eq!(status, 403);
    assert_error_envelope(&v, "csrf");

    let mut r = req("POST", "/api/models/_placeholder");
    r.headers_mut()
        .insert("x-csrf-token", CSRF.parse().unwrap());
    let (status, v) = call(app.clone(), r).await;
    assert_eq!(status, 501);
    assert_error_envelope(&v, "not_implemented");

    // 实装端点的错误方法 → 405 method_not_allowed
    let mut r = req("POST", "/api/doctor");
    r.headers_mut()
        .insert("x-csrf-token", CSRF.parse().unwrap());
    let (status, v) = call(app.clone(), r).await;
    assert_eq!(status, 405);
    assert_error_envelope(&v, "method_not_allowed");

    // 非 loopback peer + token 未设 → 403 forbidden（ConnectInfo 换非回环地址）
    let mut r = req("GET", "/api/healthz");
    r.extensions_mut().insert(ConnectInfo(
        "192.0.2.1:40000".parse::<SocketAddr>().unwrap(),
    ));
    let (status, v) = call(app.clone(), r).await;
    assert_eq!(status, 403);
    assert_error_envelope(&v, "forbidden");

    // Host 伪造（rebinding 形态）→ 403
    let mut r = req("GET", "/api/healthz");
    r.headers_mut()
        .insert(header::HOST, "evil.example".parse().unwrap());
    let (status, v) = call(app, r).await;
    assert_eq!(status, 403);
    assert_error_envelope(&v, "forbidden");
}

#[tokio::test]
async fn token_mode_contract() {
    let app = web::router(state(Some(TOKEN)));

    // 缺 / 错 Bearer → 401 unauthorized
    for bearer in [None, Some("wrong")] {
        let mut r = req("GET", "/api/healthz");
        if let Some(t) = bearer {
            r.headers_mut().insert(
                header::AUTHORIZATION,
                format!("Bearer {t}").parse().unwrap(),
            );
        }
        let (status, v) = call(app.clone(), r).await;
        assert_eq!(status, 401);
        assert_error_envelope(&v, "unauthorized");
    }

    // 正确 Bearer → 放行
    let mut r = req("GET", "/api/healthz");
    r.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {TOKEN}").parse().unwrap(),
    );
    let (status, _) = call(app.clone(), r).await;
    assert_eq!(status, 200);

    // Bearer + CSRF 双要件：缺一 → 401/403
    let mut r = req("POST", "/api/models/_placeholder");
    r.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {TOKEN}").parse().unwrap(),
    );
    let (status, v) = call(app.clone(), r).await;
    assert_eq!(status, 403);
    assert_error_envelope(&v, "csrf");

    let mut r = req("POST", "/api/models/_placeholder");
    r.headers_mut()
        .insert("x-csrf-token", CSRF.parse().unwrap());
    let (status, v) = call(app.clone(), r).await;
    assert_eq!(status, 401);
    assert_error_envelope(&v, "unauthorized");

    let mut r = req("POST", "/api/models/_placeholder");
    r.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {TOKEN}").parse().unwrap(),
    );
    r.headers_mut()
        .insert("x-csrf-token", CSRF.parse().unwrap());
    let (status, v) = call(app, r).await;
    assert_eq!(status, 501);
    assert_error_envelope(&v, "not_implemented");
}

// ---------- serve 端点（#55） ----------

/// 带 CSRF 头的 POST 构造。
fn post(uri: &str, body: Option<&str>) -> Request<Body> {
    let mut r = req("POST", uri);
    r.headers_mut()
        .insert("x-csrf-token", CSRF.parse().unwrap());
    if let Some(b) = body {
        r.headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        *r.body_mut() = Body::from(b.to_string());
    }
    r
}

#[tokio::test]
async fn serve_components_contract() {
    let app = web::router(state(None));

    let (status, v) = call(
        app.clone(),
        req("GET", "/api/serve/components?profile=normal"),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(v["schema"], 1, "{v}");
    assert_eq!(v["profile"], "normal", "{v}");
    let comps = v["components"].as_array().expect("components array");
    assert!(!comps.is_empty());
    let mut orders = Vec::new();
    for c in comps {
        assert!(c["name"].is_string(), "{c}");
        assert!(c["order"].is_u64(), "{c}");
        assert!(c["external"].is_boolean(), "{c}");
        assert!(c["local_endpoint_only"].is_boolean(), "{c}");
        orders.push(c["order"].as_u64().unwrap());
    }
    let mut sorted = orders.clone();
    sorted.sort_unstable();
    assert_eq!(orders, sorted, "components must be in start order");
    // 两个内置 profile 都含 external brain（探活不拉起）。
    assert!(comps.iter().any(|c| c["external"] == true), "{v}");

    // 只读端点：POST → 405
    let (status, v) = call(app, post("/api/serve/components", None)).await;
    assert_eq!(status, 405);
    assert_error_envelope(&v, "method_not_allowed");
}

#[tokio::test]
async fn serve_action_contract() {
    let app = web::router(state(None));

    // 写端点：GET → 405
    let (status, v) = call(app.clone(), req("GET", "/api/serve/up")).await;
    assert_eq!(status, 405);
    assert_error_envelope(&v, "method_not_allowed");

    // 未知 action → 404
    let (status, v) = call(app.clone(), post("/api/serve/nope", None)).await;
    assert_eq!(status, 404);
    assert_error_envelope(&v, "not_found");

    // 非法 body → 400
    let (status, v) = call(app.clone(), post("/api/serve/up", Some("{oops"))).await;
    assert_eq!(status, 400);
    assert_error_envelope(&v, "bad_request");

    // only 未命中组件 → 400（不触发任何 spawn/信号副作用）
    let (status, v) = call(
        app.clone(),
        post("/api/serve/up", Some(r#"{"only":["__missing__"]}"#)),
    )
    .await;
    assert_eq!(status, 400);
    assert_error_envelope(&v, "bad_request");

    // 合法 action：down external 组件是安全的 no-op（探活语义，不发信号），
    // 返回 ServeReport 同 schema（fail 是数据不是 HTTP 错误）。
    let (status, v) = call(
        app.clone(),
        post("/api/serve/down", Some(r#"{"only":["brain"]}"#)),
    )
    .await;
    assert_eq!(status, 200);
    assert_serve_shape(&v, "serve down");

    // up external：只探活不拉起（不 spawn 任何进程），仍回 ServeReport。
    let (status, v) = call(
        app.clone(),
        post("/api/serve/up", Some(r#"{"only":["brain"]}"#)),
    )
    .await;
    assert_eq!(status, 200);
    assert_serve_shape(&v, "serve up");
}

#[tokio::test]
async fn serve_log_stream_contract() {
    let app = web::router(state(None));

    // 组件名非法字符 → 400
    let (status, v) = call(app.clone(), req("GET", "/api/serve/logs/bad%20name/stream")).await;
    assert_eq!(status, 400);
    assert_error_envelope(&v, "bad_request");

    // 组件不在 profile → 404
    let (status, v) = call(
        app.clone(),
        req("GET", "/api/serve/logs/__missing__/stream?profile=normal"),
    )
    .await;
    assert_eq!(status, 404);
    assert_error_envelope(&v, "not_found");

    // 非 GET → 405（JSON 错误形态，非框架空 405）
    let (status, v) = call(app.clone(), post("/api/serve/logs/brain/stream", None)).await;
    assert_eq!(status, 405);
    assert_error_envelope(&v, "method_not_allowed");

    // 合法组件 → SSE 握手（流本身无界，只验头；response drop 后 tail 线程退出）
    let res = app
        .oneshot(req("GET", "/api/serve/logs/brain/stream?profile=normal"))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
}

// ---------- models 端点（#52） ----------

#[tokio::test]
async fn models_contract() {
    let app = web::router(state(None));

    let (status, v) = call(app.clone(), req("GET", "/api/models")).await;
    assert_eq!(status, 200);
    assert_eq!(v["schema"], 1, "{v}");
    // manifest 可定位与否取决于 cwd；两种合法形态都断言关键字段存在。
    assert!(v.get("manifest").is_some(), "{v}");
    assert!(v["models_dir"].is_string(), "{v}");
    assert!(v["models"].is_array(), "{v}");
    assert!(v["warn"].is_array(), "{v}");
    for m in v["models"].as_array().unwrap() {
        assert!(m["name"].is_string(), "{m}");
        assert!(
            ["missing", "partial", "verified"].contains(&m["state"].as_str().unwrap()),
            "model.state: {m}"
        );
        assert!(m["active"].is_boolean(), "{m}");
        for f in m["files"].as_array().expect("files array") {
            assert!(f["path"].is_string(), "{f}");
            assert!(
                ["missing", "unverified", "verified"].contains(&f["state"].as_str().unwrap()),
                "file.state: {f}"
            );
        }
    }
    // 激活选择快照：键固定、值 string|null
    let a = &v["active"];
    for k in [
        "asr_provider",
        "asr_model",
        "tts_provider",
        "tts_voice",
        "tts_lang",
        "brain_endpoint",
        "brain_model",
    ] {
        let val = &a[k];
        assert!(val.is_string() || val.is_null(), "active.{k}: {a}");
    }

    // 只读端点：POST → 405
    let (status, v) = call(app, post("/api/models", None)).await;
    assert_eq!(status, 405);
    assert_error_envelope(&v, "method_not_allowed");
}

#[tokio::test]
async fn models_pull_contract() {
    let app = web::router(state(None));

    // 写端点：GET → 405
    let (status, v) = call(app.clone(), req("GET", "/api/models/pull")).await;
    assert_eq!(status, 405);
    assert_error_envelope(&v, "method_not_allowed");

    // 缺 name / 非法 body → 400
    for body in [Some("{}"), Some("{oops"), Some(r#"{"name":"  "}"#), None] {
        let (status, v) = call(app.clone(), post("/api/models/pull", body)).await;
        assert_eq!(status, 400, "body={body:?}");
        assert_error_envelope(&v, "bad_request");
    }

    // 未知名称 → 400（manifest 缺失也归同一错误面，不触发下载）
    let (status, v) = call(
        app.clone(),
        post("/api/models/pull", Some(r#"{"name":"__missing__"}"#)),
    )
    .await;
    assert_eq!(status, 400);
    assert_error_envelope(&v, "bad_request");

    // 无 CSRF → 403（守卫先于 handler）
    let (status, v) = call(app, req("POST", "/api/models/pull")).await;
    assert_eq!(status, 403);
    assert_error_envelope(&v, "csrf");
}

// ---------- 训练页端点（#42） ----------

/// `GET /api/pipelines`：定义清单契约（wizard 数据源）。
/// 测试 cwd = infra/ → `pipelines` 候选命中实仓定义目录。
#[tokio::test]
async fn pipelines_contract() {
    let app = web::router(state(None));

    let (status, v) = call(app.clone(), req("GET", "/api/pipelines")).await;
    assert_eq!(status, 200);
    assert_eq!(v["schema"], 1, "{v}");
    assert_eq!(v["command"], "pipeline list", "{v}");
    assert!(v["dir"].is_string(), "{v}");
    let pipes = v["pipelines"].as_array().expect("pipelines array");
    assert!(pipes.len() >= 2, "{v}");
    let voice = pipes
        .iter()
        .find(|p| p["name"] == "voice-sft")
        .expect("voice-sft present");
    assert_eq!(voice["activate_env"], "MOMO_TTS_MODEL_DIR", "{voice}");
    assert!(voice["artifact_dir"].is_string(), "{voice}");
    assert!(voice["steps"].as_array().unwrap().len() >= 2, "{voice}");
    // vars：wizard 表单依据；voice-sft 的 input 为 required。
    let vars = voice["vars"].as_array().expect("vars array");
    let input = vars
        .iter()
        .find(|x| x["name"] == "input")
        .expect("input var");
    assert_eq!(input["required"], true, "{input}");

    // 只读端点：POST → 405
    let (status, v) = call(app, post("/api/pipelines", None)).await;
    assert_eq!(status, 405);
    assert_error_envelope(&v, "method_not_allowed");
}

/// `POST /api/jobs` pipeline 快捷提交的校验面（均在 store 建档前拒绝，无副作用）：
/// pipeline+ssh executor → 422；args×pipeline 互斥 → 422；未知 pipeline → 422；
/// required var 缺项 → 422 逐字段归因；空 args → 400。
#[tokio::test]
async fn jobs_submit_pipeline_validation() {
    let app = web::router(state(None));

    // pipeline 形态只能走 local 执行器（pipeline step 自带 ssh 目标）→ 422。
    let (status, v) = call(
        app.clone(),
        post(
            "/api/jobs",
            Some(r#"{"pipeline":"persona-qlora","executor":"ssh","ssh":{"host":"bigstrong"}}"#),
        ),
    )
    .await;
    assert_eq!(status, 422, "{v}");
    assert_error_envelope(&v, "validation_failed");
    assert!(v["error"]["fields"]["executor"].is_string(), "{v}");

    // args 与 pipeline 互斥 → 422
    let (status, v) = call(
        app.clone(),
        post(
            "/api/jobs",
            Some(r#"{"pipeline":"voice-sft","args":["echo","hi"]}"#),
        ),
    )
    .await;
    assert_eq!(status, 422, "{v}");
    assert!(v["error"]["fields"]["args"].is_string(), "{v}");

    // 未知 pipeline → 422
    let (status, v) = call(
        app.clone(),
        post("/api/jobs", Some(r#"{"pipeline":"__missing__"}"#)),
    )
    .await;
    assert_eq!(status, 422, "{v}");
    assert!(v["error"]["fields"]["pipeline"].is_string(), "{v}");

    // voice-sft 缺 required var input → 422 fields["set.input"]
    let (status, v) = call(
        app.clone(),
        post("/api/jobs", Some(r#"{"pipeline":"voice-sft"}"#)),
    )
    .await;
    assert_eq!(status, 422, "{v}");
    assert!(v["error"]["fields"]["set.input"].is_string(), "{v}");

    // 空 args → 400（原形态校验不变）
    let (status, v) = call(app.clone(), post("/api/jobs", Some(r#"{"args":[]}"#))).await;
    assert_eq!(status, 400);
    assert_error_envelope(&v, "bad_request");

    // 无 CSRF → 403（守卫先于 handler）
    let (status, v) = call(app, req("POST", "/api/jobs")).await;
    assert_eq!(status, 403);
    assert_error_envelope(&v, "csrf");
}

/// promote / metrics 的未知 id → JSON 404（load 先行，不跑 reconcile 写路径）。
#[tokio::test]
async fn jobs_promote_metrics_unknown_id() {
    let app = web::router(state(None));

    let (status, v) = call(app.clone(), post("/api/jobs/01JNOPE42/promote", Some("{}"))).await;
    assert_eq!(status, 404, "{v}");
    assert_error_envelope(&v, "not_found");

    let (status, v) = call(app.clone(), req("GET", "/api/jobs/01JNOPE42/metrics")).await;
    assert_eq!(status, 404, "{v}");
    assert_error_envelope(&v, "not_found");

    // promote 为写端点：GET → 405（axum 方法门控先于 handler，空 body
    // ——与 `/{id}/cancel` 等 post-only 子路由同口径，不走 JSON 错误包络）。
    let res = app
        .oneshot(req("GET", "/api/jobs/01JNOPE42/promote"))
        .await
        .unwrap();
    assert_eq!(res.status(), 405);
}
