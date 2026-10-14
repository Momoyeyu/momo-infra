//! `momoctl web` 二进制级回归：
//! - 启动守卫：`--listen` 非 loopback 且无 `MOMO_WEB_TOKEN` → exit 2；
//!   不可解析地址 → exit 2。
//! - 认证链：token 已设 → `/api` 全请求须 `Authorization: Bearer`，缺失/错误 → 401；
//!   token 未设 → 仅 loopback peer + 回环 Host（Host 伪造 → 403）。
//! - CSRF：state-changing 方法须 `X-CSRF-Token` 回带 `GET /api/csrf` 签发值，
//!   缺失/错误 → 403；GET/HEAD/OPTIONS 只读放行。
//! - 基座路由：`/api/healthz|readyz` 200 + `schema:1`；`/api/*` 未命中 JSON 404；
//!   占位路由 501；错误一律 `{schema:1, error:{code,message}}`；
//!   SPA fallback 对未知非 api 路径回 index.html；`/api/events` 为 SSE 流。

use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn env_setup(tag: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("momoctl-web-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let env_file = dir.join("empty.env");
    fs::write(&env_file, "").unwrap();
    (dir, env_file)
}

/// 取一个当前空闲的本地端口（先 bind :0 再释放；竞态窗口在测试可接受范围内）。
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn momoctl(env_file: &std::path::Path, args: &[&str], token: Option<&str>) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_momoctl"));
    cmd.args(args)
        .args(["--env-file"])
        .arg(env_file)
        // 隔离宿主 env：token 存在与否必须只受测试控制
        .env_remove("MOMO_WEB_TOKEN");
    if let Some(t) = token {
        cmd.env("MOMO_WEB_TOKEN", t);
    }
    cmd
}

/// 起 server 并轮询 healthz 直到就绪（超时 panic）。
/// token 模式下探活请求同样带 Bearer（认证链覆盖 /api 全部路由）。
fn spawn_web(
    env_file: &std::path::Path,
    listen: &str,
    json: bool,
    token: Option<&str>,
) -> (Child, String) {
    let mut args: Vec<String> = vec!["web".into(), "--listen".into(), listen.into()];
    if json {
        args.insert(0, "--json".into());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut child = momoctl(env_file, &arg_refs, token)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn momoctl web");

    let base = format!("http://{listen}");
    let client = reqwest::blocking::Client::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut req = client.get(format!("{base}/api/healthz"));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        match req.send() {
            Ok(r) if r.status().is_success() => return (child, base),
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                panic!("momoctl web did not become ready on {listen}");
            }
        }
    }
}

/// 断言统一错误体 `{schema:1, error:{code,message}}`。
fn assert_error(v: &serde_json::Value, code: &str) {
    assert_eq!(v["schema"], 1, "{v}");
    assert_eq!(v["error"]["code"], code, "{v}");
    assert!(v["error"]["message"].is_string(), "{v}");
}

/// 经 `GET /api/csrf` 取回当前进程的 CSRF token。
fn fetch_csrf(client: &reqwest::blocking::Client, base: &str, token: Option<&str>) -> String {
    let mut req = client.get(format!("{base}/api/csrf"));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let r = req.send().unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_eq!(v["schema"], 1);
    v["csrf_token"]
        .as_str()
        .expect("csrf_token must be a string")
        .to_string()
}

#[test]
fn web_non_loopback_without_token_refuses_start() {
    let (_dir, env_file) = env_setup("guard");
    let out = momoctl(
        &env_file,
        &["web", "--listen", &format!("0.0.0.0:{}", free_port())],
        None,
    )
    .output()
    .expect("spawn momoctl");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("MOMO_WEB_TOKEN"), "stderr: {stderr}");
}

#[test]
fn web_invalid_listen_is_usage_error() {
    let (_dir, env_file) = env_setup("badaddr");
    let out = momoctl(&env_file, &["web", "--listen", "not-an-addr"], None)
        .output()
        .expect("spawn momoctl");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn web_base_routes_and_spa_fallback() {
    let (_dir, env_file) = env_setup("routes");
    let port = free_port();
    let (mut child, base) = spawn_web(&env_file, &format!("127.0.0.1:{port}"), false, None);
    let client = reqwest::blocking::Client::new();

    // liveness / readiness：200 + schema:1
    for path in ["/api/healthz", "/api/readyz"] {
        let r = client.get(format!("{base}{path}")).send().unwrap();
        assert_eq!(r.status(), 200, "{path}");
        let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
        assert_eq!(v["schema"], 1, "{path}");
        assert_eq!(v["status"], "ok", "{path}");
    }

    // /api 路由索引：带不带尾斜杠都必须回 JSON，不得落 SPA fallback
    for path in ["/api", "/api/"] {
        let r = client.get(format!("{base}{path}")).send().unwrap();
        assert_eq!(r.status(), 200, "{path}");
        let ct = r.headers()["content-type"].to_str().unwrap().to_string();
        assert!(
            ct.starts_with("application/json"),
            "{path} content-type: {ct}"
        );
        let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
        assert_eq!(v["schema"], 1, "{path}");
        assert!(v["routes"].is_array(), "{path}");
    }

    // 占位路由：GET（只读）→ 501 错误 DTO
    let r = client
        .get(format!("{base}/api/models/_placeholder"))
        .send()
        .unwrap();
    assert_eq!(r.status(), 501);
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_error(&v, "not_implemented");

    // 占位路由 mutation：无 CSRF → 403；回带签发 token → 到达 handler（501）
    let r = client
        .post(format!("{base}/api/models/_placeholder"))
        .send()
        .unwrap();
    assert_eq!(r.status(), 403);
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_error(&v, "csrf");

    let csrf = fetch_csrf(&client, &base, None);
    let r = client
        .post(format!("{base}/api/models/_placeholder"))
        .header("x-csrf-token", &csrf)
        .send()
        .unwrap();
    assert_eq!(r.status(), 501);
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_error(&v, "not_implemented");

    // 实装 mutation：CSRF 通过 → 到达 handler；未命中组件 → 400 bad_request
    let r = client
        .post(format!("{base}/api/serve/up"))
        .header("x-csrf-token", &csrf)
        .header("content-type", "application/json")
        .body(r#"{"only": ["__missing__"]}"#)
        .send()
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_error(&v, "bad_request");

    // 跨站 Origin 的 mutation：Origin authority 与 Host 不符 → 403
    let r = client
        .post(format!("{base}/api/serve/down"))
        .header("x-csrf-token", &csrf)
        .header("Origin", "https://evil.example")
        .send()
        .unwrap();
    assert_eq!(r.status(), 403);
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_error(&v, "forbidden");

    // Host 伪造（DNS rebinding 形态）：token 未设时 Host 非回环 → 403
    let r = client
        .get(format!("{base}/api/healthz"))
        .header("Host", "evil.example")
        .send()
        .unwrap();
    assert_eq!(r.status(), 403);
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_error(&v, "forbidden");

    // /api 未命中 → JSON 404（不落入 SPA fallback）
    let r = client.get(format!("{base}/api/nope")).send().unwrap();
    assert_eq!(r.status(), 404);
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_error(&v, "not_found");

    // SPA：/ 直出 index.html；未知前端路由回退 index.html（不经认证层）
    for path in ["/", "/models/123"] {
        let r = client.get(format!("{base}{path}")).send().unwrap();
        assert_eq!(r.status(), 200, "{path}");
        let ct = r.headers()["content-type"].to_str().unwrap().to_string();
        assert!(ct.starts_with("text/html"), "{path} content-type: {ct}");
        assert!(r.text().unwrap().contains("momo console"), "{path}");
    }

    // 静态资产：正确 content-type
    let r = client
        .get(format!("{base}/assets/style.css"))
        .send()
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-type"].to_str().unwrap(),
        "text/css; charset=utf-8"
    );

    // SSE 基座：event-stream 头；只验握手即断开（流本身无界）
    let r = client.get(format!("{base}/api/events")).send().unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    drop(r);

    child.kill().unwrap();
    let _ = child.wait();
}

#[test]
fn web_token_mode_requires_bearer() {
    let (_dir, env_file) = env_setup("token");
    let port = free_port();
    // token 已设：任意来源（含 loopback）/api 全请求都须 Bearer。
    let (mut child, base) = spawn_web(
        &env_file,
        &format!("127.0.0.1:{port}"),
        false,
        Some("test-token"),
    );
    let client = reqwest::blocking::Client::new();

    // 缺 / 错误 Bearer → 401；正确 → 200（即使 peer 是 loopback 也必验）
    for req in [
        client.get(format!("{base}/api/healthz")),
        client
            .get(format!("{base}/api/healthz"))
            .bearer_auth("wrong"),
    ] {
        let r = req.send().unwrap();
        assert_eq!(r.status(), 401);
        let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
        assert_error(&v, "unauthorized");
    }
    let r = client
        .get(format!("{base}/api/healthz"))
        .bearer_auth("test-token")
        .send()
        .unwrap();
    assert_eq!(r.status(), 200);

    // SPA 不经认证层
    let r = client.get(&base).send().unwrap();
    assert_eq!(r.status(), 200);

    // mutation：Bearer + CSRF 双要件，缺一不可
    let csrf = fetch_csrf(&client, &base, Some("test-token"));
    let r = client
        .post(format!("{base}/api/models/_placeholder"))
        .bearer_auth("test-token")
        .send()
        .unwrap();
    assert_eq!(r.status(), 403);
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_error(&v, "csrf");

    let r = client
        .post(format!("{base}/api/models/_placeholder"))
        .header("x-csrf-token", &csrf)
        .send()
        .unwrap();
    assert_eq!(r.status(), 401);

    let r = client
        .post(format!("{base}/api/models/_placeholder"))
        .bearer_auth("test-token")
        .header("x-csrf-token", &csrf)
        .send()
        .unwrap();
    assert_eq!(r.status(), 501);
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_error(&v, "not_implemented");

    child.kill().unwrap();
    let _ = child.wait();
}

#[test]
fn web_json_mode_prints_schema_object() {
    let (_dir, env_file) = env_setup("json");
    let port = free_port();
    let (mut child, _base) = spawn_web(&env_file, &format!("127.0.0.1:{port}"), true, None);

    // --json 启动对象：{schema, command, listen, url}（pretty 多行，累积到可解析为止）
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut buf = String::new();
    let v: serde_json::Value = loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        buf.push_str(&line);
        if let Ok(v) = serde_json::from_str(&buf) {
            break v;
        }
        assert!(buf.len() < 4096, "startup output is not valid JSON: {buf}");
    };
    assert_eq!(v["schema"], 1);
    assert_eq!(v["command"], "web");
    assert_eq!(v["listen"], format!("127.0.0.1:{port}"));
    assert!(v["url"].as_str().unwrap().starts_with("http://"));

    child.kill().unwrap();
    let _ = child.wait();
}
