//! `GET|PUT /api/config` + `POST /api/config/rollback` 行为测试（issue #56）：
//! - GET 掩码：敏感键只回 `…`/前 4 位 + `…`，整个响应绝不回显明文 secret；
//! - PUT：未知键/非 `MOMO_` 前缀/值域越界 → 422 `error.fields` 字段级归因且不动盘；
//!   校验通过 → `.env.<profile>.bak` 备份 + tmp+rename 原子写 + 掩码 diff + restart hint；
//! - rollback：从最新 `.bak` 原子恢复返回 diff；无 `.bak` → 404；文件缺失 → PUT 404；
//! - 写路径仅 `.env` 同目录 `<link>.<profile>`（profile 白名单，无穿越面）；
//! - mutation 需 `X-CSRF-Token`（guard 层已验，顺带覆盖无 CSRF → 403）。
//!
//! HTTP 侧走 `tower::ServiceExt::oneshot` 直连 Router，不起端口。

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use momo_infra::web::{self, WebState};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tower::ServiceExt;

const CSRF: &str = "test-csrf-token";

fn test_dir(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "momoctl-cfgapi-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn state(env_link: &Path) -> Arc<WebState> {
    Arc::new(WebState {
        events_tx: broadcast::channel(web::EVENT_CHANNEL_CAP).0,
        token: None,
        csrf_token: CSRF.into(),
        env_link: env_link.to_path_buf(),
    })
}

/// 标准夹具：`.env` 链接名 + `.env.normal` / `.env.degraded` 目标档文件。
fn env_state(dir: &Path) -> Arc<WebState> {
    fs::write(dir.join(".env"), "MOMO_PROFILE=normal\n").unwrap();
    fs::write(
        dir.join(".env.normal"),
        concat!(
            "# header comment\n",
            "MOMO_PROFILE=normal\n",
            "MOMO_TTS_PROVIDER=kokoro   # 后端\n",
            "MOMO_BRAIN_PORT=7820\n",
            "MOMO_LLM_API_KEY=sk-aaaabbbb\n",
            "HF_ENDPOINT=https://hf-mirror.com\n"
        ),
    )
    .unwrap();
    fs::write(dir.join(".env.degraded"), "MOMO_PROFILE=degraded\n").unwrap();
    state(&dir.join(".env"))
}

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

fn req_json(method: &str, uri: &str, body: Value) -> Request<Body> {
    let mut r = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7900")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-csrf-token", CSRF)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
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

fn assert_error(v: &Value, code: &str) {
    assert_eq!(v["schema"], 1, "{v}");
    assert_eq!(v["error"]["code"], code, "{v}");
    assert!(v["error"]["message"].is_string(), "{v}");
}

fn entry(v: &Value, key: &str) -> Value {
    v["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .find(|e| e["key"] == key)
        .unwrap_or_else(|| panic!("entry {key} missing in {v}"))
        .clone()
}

/// GET：掩码口径 + set/unset + schema 外文件键可见。
#[tokio::test]
async fn config_get_masks_sensitive() {
    let dir = test_dir("get");
    let app = web::router(env_state(&dir));

    let (status, v) = call(app, req("GET", "/api/config?profile=normal")).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["schema"], 1);
    assert_eq!(v["profile"], "normal");
    assert_eq!(v["file_exists"], true);
    assert!(v["active_profile"].is_string());
    assert!(v["env_file"].as_str().unwrap().ends_with(".env.normal"));

    // 敏感键：打码值，绝不回显明文
    let k = entry(&v, "MOMO_LLM_API_KEY");
    assert_eq!(k["sensitive"], true);
    assert_eq!(k["set"], true);
    assert_eq!(k["value"], "sk-a…");
    // 非敏感：明文
    let tts = entry(&v, "MOMO_TTS_PROVIDER");
    assert_eq!(tts["sensitive"], false);
    assert_eq!(tts["value"], "kokoro");
    // schema 内但文件未设置：set=false、value=null
    let voice = entry(&v, "MOMO_TTS_VOICE");
    assert_eq!(voice["set"], false);
    assert!(voice["value"].is_null());
    // schema 外文件键（passthrough）：仍可见（页面只读展示，PUT 拒收）
    assert_eq!(entry(&v, "HF_ENDPOINT")["value"], "https://hf-mirror.com");
    // 整个响应不含明文 secret
    assert!(!v.to_string().contains("sk-aaaabbbb"), "{v}");
}

/// GET 参数与文件缺失行为；POST /api/config → 405（带 CSRF 穿过守卫）。
#[tokio::test]
async fn config_get_profile_validation_and_405() {
    let dir = test_dir("gate");
    let app = web::router(env_state(&dir));

    // 非法 profile → 400 bad_request
    let (status, v) = call(app.clone(), req("GET", "/api/config?profile=bogus")).await;
    assert_eq!(status, 400);
    assert_error(&v, "bad_request");

    // 缺省 profile 解析到某合法档（active_profile/MOMO_PROFILE/normal）
    let (status, v) = call(app.clone(), req("GET", "/api/config")).await;
    assert_eq!(status, 200, "{v}");
    assert!(["normal", "degraded"].contains(&v["profile"].as_str().unwrap()));

    // 无 CSRF 的 mutation 在 guard 层 403；带 CSRF 的 POST /config → 405（GET|PUT only）
    let (status, v) = call(app.clone(), req("POST", "/api/config")).await;
    assert_eq!(status, 403);
    assert_error(&v, "csrf");

    let mut r = req("POST", "/api/config");
    r.headers_mut()
        .insert("x-csrf-token", CSRF.parse().unwrap());
    let (status, v) = call(app.clone(), r).await;
    assert_eq!(status, 405);
    assert_error(&v, "method_not_allowed");

    // 文件缺失（删掉 .env.degraded）→ GET 仍 200，file_exists=false，entries 全 unset
    fs::remove_file(dir.join(".env.degraded")).unwrap();
    let (status, v) = call(app, req("GET", "/api/config?profile=degraded")).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["file_exists"], false);
    assert!(v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["set"] == false && e["value"].is_null()));
}

/// PUT：校验通过 → .bak + 原子写 + 掩码 diff + restart hint；GET 回读一致。
#[tokio::test]
async fn config_put_writes_backup_and_masked_diff() {
    let dir = test_dir("put");
    let app = web::router(env_state(&dir));

    let put = req_json(
        "PUT",
        "/api/config?profile=normal",
        json!({"changes": {
            "MOMO_TTS_PROVIDER": "edge",
            "MOMO_LLM_API_KEY": "sk-newsecret999",
            "MOMO_ENV": "prod"          // 文件中未出现的键 → 追加
        }}),
    );
    let (status, v) = call(app.clone(), put).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["schema"], 1);
    assert_eq!(v["profile"], "normal");
    assert!(v["backup"].as_str().unwrap().ends_with(".env.normal.bak"));
    assert!(v["restart_hint"]
        .as_str()
        .unwrap()
        .contains("serve restart"));

    // 盘上读回：命中行换值（行尾注释保留）、新键追加
    let text = fs::read_to_string(dir.join(".env.normal")).unwrap();
    assert!(text.contains("MOMO_TTS_PROVIDER=edge   # 后端\n"), "{text}");
    assert!(
        text.contains("MOMO_LLM_API_KEY=sk-newsecret999\n"),
        "{text}"
    );
    assert!(text.contains("MOMO_ENV=prod\n"), "{text}");
    assert!(text.contains("# header comment\n"), "{text}");
    // .bak 保留写前值
    let bak = fs::read_to_string(dir.join(".env.normal.bak")).unwrap();
    assert!(bak.contains("MOMO_TTS_PROVIDER=kokoro"), "{bak}");
    // 无 tmp 残留
    assert!(!fs::read_dir(&dir).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".tmp")));

    // diff 掩码：敏感 from/to 均打码，响应整体不含明文 secret
    let diff = v["diff"].as_array().unwrap();
    let d = diff
        .iter()
        .find(|d| d["key"] == "MOMO_LLM_API_KEY")
        .expect("LLM_API_KEY diff entry");
    assert_eq!(d["from"], "sk-a…");
    assert_eq!(d["to"], "sk-n…");
    let d = diff
        .iter()
        .find(|d| d["key"] == "MOMO_TTS_PROVIDER")
        .unwrap();
    assert_eq!(d["from"], "kokoro");
    assert_eq!(d["to"], "edge");
    assert!(!v.to_string().contains("sk-newsecret999"), "{v}");

    // GET 回读：新值生效、掩码一致
    let (status, v2) = call(app, req("GET", "/api/config?profile=normal")).await;
    assert_eq!(status, 200);
    assert_eq!(entry(&v2, "MOMO_TTS_PROVIDER")["value"], "edge");
    assert_eq!(entry(&v2, "MOMO_LLM_API_KEY")["value"], "sk-n…");
}

/// PUT 值域/键名校验：全部非法 → 422 字段级归因，文件与 .bak 均未动。
#[tokio::test]
async fn config_put_validation_errors_do_not_write() {
    let dir = test_dir("badput");
    let app = web::router(env_state(&dir));
    let before = fs::read_to_string(dir.join(".env.normal")).unwrap();

    let put = req_json(
        "PUT",
        "/api/config",
        json!({"profile": "normal", "changes": {
            "MOMO_TTS_PROVIDER": "not-a-provider",  // enum 越界
            "MOMO_BRAIN_PORT": "abc",               // int 非数字
            "MOMO_ASR_ENDPOINT": "no-scheme",       // url 无 scheme
            "MOMO_NOPE": "x",                       // schema 未知键
            "HF_ENDPOINT": "https://x",             // 非 MOMO_ 前缀
            "OTHER": "y"                            // 非 MOMO_ 前缀
        }}),
    );
    let (status, v) = call(app.clone(), put).await;
    assert_eq!(status, 422, "{v}");
    assert_error(&v, "validation_failed");
    let fields = &v["error"]["fields"];
    for k in [
        "MOMO_TTS_PROVIDER",
        "MOMO_BRAIN_PORT",
        "MOMO_ASR_ENDPOINT",
        "MOMO_NOPE",
        "HF_ENDPOINT",
        "OTHER",
    ] {
        assert!(fields[k].is_string(), "field {k}: {v}");
    }
    // 未动盘：文件原样、无 .bak
    assert_eq!(fs::read_to_string(dir.join(".env.normal")).unwrap(), before);
    assert!(!dir.join(".env.normal.bak").exists());

    // profile 非法 → 422 fields.profile
    let put = req_json(
        "PUT",
        "/api/config",
        json!({"profile": "prod", "changes": {"MOMO_ENV": "dev"}}),
    );
    let (status, v) = call(app.clone(), put).await;
    assert_eq!(status, 422, "{v}");
    assert_error(&v, "validation_failed");
    assert!(v["error"]["fields"]["profile"].is_string(), "{v}");

    // changes 为空 → 400
    let put = req_json("PUT", "/api/config", json!({"changes": {}}));
    let (status, v) = call(app, put).await;
    assert_eq!(status, 400, "{v}");
    assert_error(&v, "bad_request");
}

/// PUT 目标文件缺失 → 404（先 `momoctl switch --to <p>` 生成）。
#[tokio::test]
async fn config_put_missing_file_404() {
    let dir = test_dir("nofile");
    fs::write(dir.join(".env"), "MOMO_PROFILE=normal\n").unwrap();
    let app = web::router(state(&dir.join(".env")));

    let put = req_json(
        "PUT",
        "/api/config?profile=normal",
        json!({"changes": {"MOMO_TTS_PROVIDER": "edge"}}),
    );
    let (status, v) = call(app, put).await;
    assert_eq!(status, 404, "{v}");
    assert_error(&v, "not_found");
    assert!(!dir.join(".env.normal").exists());
}

/// rollback：写后回滚恢复原内容（掩码 diff）；无 .bak → 404；重复回滚幂等。
#[tokio::test]
async fn config_rollback_restores_backup() {
    let dir = test_dir("rb");
    let app = web::router(env_state(&dir));

    // 无 .bak → 404
    let (status, v) = call(
        app.clone(),
        req_json("POST", "/api/config/rollback", json!({"profile": "normal"})),
    )
    .await;
    assert_eq!(status, 404, "{v}");
    assert_error(&v, "not_found");

    // PUT 制造 .bak
    let put = req_json(
        "PUT",
        "/api/config?profile=normal",
        json!({"changes": {"MOMO_TTS_PROVIDER": "edge", "MOMO_LLM_API_KEY": "sk-rollback999"}}),
    );
    let (status, _) = call(app.clone(), put).await;
    assert_eq!(status, 200);

    // rollback → 恢复原内容，diff 为 edge→kokoro 的掩码还原
    let (status, v) = call(
        app.clone(),
        req_json("POST", "/api/config/rollback", json!({"profile": "normal"})),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert!(v["backup"].as_str().unwrap().ends_with(".env.normal.bak"));
    let text = fs::read_to_string(dir.join(".env.normal")).unwrap();
    assert!(text.contains("MOMO_TTS_PROVIDER=kokoro"), "{text}");
    assert!(text.contains("MOMO_LLM_API_KEY=sk-aaaabbbb"), "{text}");
    // .bak 保留（可重复回滚）；第二次回滚 diff 为空仍 200
    assert!(dir.join(".env.normal.bak").is_file());
    let (status, v) = call(
        app.clone(),
        req_json("POST", "/api/config/rollback?profile=normal", json!({})),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["diff"].as_array().unwrap().len(), 0, "{v}");

    // GET /api/config/rollback → 405
    let (status, v) = call(app, req("GET", "/api/config/rollback")).await;
    assert_eq!(status, 405);
    assert_error(&v, "method_not_allowed");
}

/// GET /api/config/schema（issue #81）：config.schema.toml 单源直出——
/// 键集合与盘内 schema 一致；type 对外映射 `string`→`str`；component 取值域校验；
/// 非 GET → 405。
#[tokio::test]
async fn config_schema_endpoint_single_source() {
    let dir = test_dir("schema");
    let app = web::router(env_state(&dir));

    let (status, v) = call(app.clone(), req("GET", "/api/config/schema")).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["schema"], 1);
    let keys = v["keys"].as_array().expect("keys array");
    assert!(!keys.is_empty());

    // 单源：与仓库根 schema 文件键集合一致（服务端无第二份键表）
    let schema =
        momo_infra::config_schema::Schema::parse(include_str!("../../config.schema.toml")).unwrap();
    assert_eq!(keys.len(), schema.key.len(), "{v}");
    let by_name: std::collections::BTreeMap<&str, &Value> = keys
        .iter()
        .map(|k| (k["name"].as_str().unwrap(), k))
        .collect();
    for spec in &schema.key {
        let k = by_name
            .get(spec.name.as_str())
            .unwrap_or_else(|| panic!("key {} missing in {v}", spec.name));
        // 契约字段齐：name/type/default/sensitive/description/component（values 仅 enum 下发）
        for f in [
            "name",
            "type",
            "default",
            "sensitive",
            "description",
            "component",
        ] {
            assert!(
                k.get(f).is_some(),
                "key {} field {f} missing: {k}",
                spec.name
            );
        }
        assert_eq!(k["component"], spec.component, "key {}", spec.name);
        assert_eq!(k["sensitive"], spec.sensitive, "key {}", spec.name);
        let want_type = if spec.kind == "string" {
            "str"
        } else {
            spec.kind.as_str()
        };
        assert_eq!(k["type"], want_type, "key {}", spec.name);
    }

    // 枚举下发 values；敏感键 sensitive=true 且无明文泄漏（schema 只含元数据）
    let tts = by_name["MOMO_TTS_PROVIDER"];
    assert_eq!(tts["type"], "enum");
    assert!(tts["values"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x == "kokoro"));
    assert_eq!(by_name["MOMO_LLM_API_KEY"]["sensitive"], true);
    assert_eq!(by_name["MOMO_LLM_API_KEY"]["type"], "str");

    // PUT /api/config/schema → 405（带 CSRF 穿过守卫层）
    let (status, v) = call(app, req_json("PUT", "/api/config/schema", json!({"x": 1}))).await;
    assert_eq!(status, 405);
    assert_error(&v, "method_not_allowed");
}
