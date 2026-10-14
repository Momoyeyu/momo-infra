//! `/api/jobs` 二进制级回归：
//! - POST /api/jobs 提交 → detached runner 执行 → GET /api/jobs/{id} 终态可查；
//! - GET /api/jobs 列表；POST /api/jobs/{id}/cancel；
//! - GET /api/jobs/{id}/logs?cursor= SSE：backlog → `event: end` 关流；
//! - 参数错 → 400；未知 id → JSON 404。
//!   与 `momoctl jobs` 读写同一个 store（XDG_STATE_HOME 隔离）。

use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn env_setup(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("momoctl-jobsapi-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let state = dir.join("state");
    fs::create_dir_all(&state).unwrap();
    let env_file = dir.join("empty.env");
    fs::write(&env_file, "").unwrap();
    (dir, state, env_file)
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_web(env_file: &std::path::Path, state: &std::path::Path, listen: &str) -> (Child, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_momoctl"));
    let child = cmd
        .args(["web", "--listen", listen, "--env-file"])
        .arg(env_file)
        .env("XDG_STATE_HOME", state)
        .env_remove("MOMO_WEB_TOKEN")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn momoctl web");

    let base = format!("http://{listen}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match reqwest::blocking::get(format!("{base}/api/healthz")) {
            Ok(r) if r.status().is_success() => return (child, base),
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                panic!("momoctl web did not become ready on {listen}");
            }
        }
    }
}

/// GET → (status, json body)。reqwest 未开 json feature，手工解析。
fn get_json(client: &reqwest::blocking::Client, url: &str) -> (u16, serde_json::Value) {
    let r = client.get(url).send().unwrap();
    let code = r.status().as_u16();
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    (code, v)
}

/// POST json body → (status, json body)。mutation 须回带 CSRF（web guard）。
fn post_json(
    client: &reqwest::blocking::Client,
    url: &str,
    body: &serde_json::Value,
    csrf: &str,
) -> (u16, serde_json::Value) {
    let r = client
        .post(url)
        .header("content-type", "application/json")
        .header("x-csrf-token", csrf)
        .body(body.to_string())
        .send()
        .unwrap();
    let code = r.status().as_u16();
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    (code, v)
}

/// 经 `GET /api/csrf` 取当前进程的 CSRF token。
fn fetch_csrf(client: &reqwest::blocking::Client, base: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(
        &client
            .get(format!("{base}/api/csrf"))
            .send()
            .unwrap()
            .text()
            .unwrap(),
    )
    .unwrap();
    v["csrf_token"].as_str().unwrap().to_string()
}

fn wait_terminal(client: &reqwest::blocking::Client, base: &str, id: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, v) = get_json(client, &format!("{base}/api/jobs/{id}"));
        let status = v["job"]["status"].as_str().unwrap_or("");
        if matches!(status, "succeeded" | "failed" | "canceled") {
            return v["job"].clone();
        }
        if Instant::now() >= deadline {
            panic!("job {id} did not reach terminal state: {v}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_status(client: &reqwest::blocking::Client, base: &str, id: &str, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, v) = get_json(client, &format!("{base}/api/jobs/{id}"));
        if v["job"]["status"] == want {
            return;
        }
        if Instant::now() >= deadline {
            panic!("job {id} did not reach {want}: {v}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn api_submit_list_detail_log_and_cancel() {
    let (dir, state, env_file) = env_setup("routes");
    let port = free_port();
    let (mut child, base) = spawn_web(&env_file, &state, &format!("127.0.0.1:{port}"));
    let client = reqwest::blocking::Client::new();
    let exe = env!("CARGO_BIN_EXE_momoctl");
    let csrf = fetch_csrf(&client, &base);

    // 提交：runner detached 执行 `momoctl --version`
    let (code, v) = post_json(
        &client,
        &format!("{base}/api/jobs"),
        &serde_json::json!({
            "kind": "train",
            "desc": "api job",
            "args": [exe, "--version"],
        }),
        &csrf,
    );
    assert_eq!(code, 200);
    assert_eq!(v["schema"], 1);
    let id = v["job"]["id"].as_str().unwrap().to_string();

    // detail → 终态 succeeded
    let job = wait_terminal(&client, &base, &id);
    assert_eq!(job["status"], "succeeded", "{job}");

    // list 含该 job
    let (_, v) = get_json(&client, &format!("{base}/api/jobs"));
    let ids: Vec<&str> = v["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|j| j["id"].as_str())
        .collect();
    assert!(ids.contains(&id.as_str()), "list missing {id}: {v}");

    // SSE 日志：backlog（event: log）→ event: end 关流；terminal 后流会结束
    let r = client
        .get(format!("{base}/api/jobs/{id}/logs?cursor=0"))
        .send()
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    let body = r.text().unwrap();
    assert!(body.contains("event: log"), "sse body: {body}");
    assert!(body.contains("event: end"), "sse body: {body}");
    assert!(body.contains("momoctl 0.1.0"), "sse body: {body}");

    // cancel 终态 job：幂等返回 succeeded
    let (code, v) = post_json(
        &client,
        &format!("{base}/api/jobs/{id}/cancel"),
        &serde_json::json!({}),
        &csrf,
    );
    assert_eq!(code, 200);
    assert_eq!(v["job"]["status"], "succeeded");

    // 400：args 缺失/为空
    let (code, v) = post_json(
        &client,
        &format!("{base}/api/jobs"),
        &serde_json::json!({"args": []}),
        &csrf,
    );
    assert_eq!(code, 400);
    assert_eq!(v["error"]["code"], "bad_request");
    // 404：未知 job
    let (code, v) = get_json(&client, &format!("{base}/api/jobs/01JNOPE"));
    assert_eq!(code, 404);
    assert_eq!(v["error"]["code"], "not_found");

    child.kill().unwrap();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn api_cancel_running_job() {
    let (dir, state, env_file) = env_setup("apx");
    let port = free_port();
    let (mut child, base) = spawn_web(&env_file, &state, &format!("127.0.0.1:{port}"));
    let client = reqwest::blocking::Client::new();
    let exe = env!("CARGO_BIN_EXE_momoctl");
    let csrf = fetch_csrf(&client, &base);

    // sleeper payload：`momoctl serve logs -f` 无限跟随
    let (_, v) = post_json(
        &client,
        &format!("{base}/api/jobs"),
        &serde_json::json!({
            "desc": "api sleeper",
            "args": [exe, "serve", "logs", "-f"],
        }),
        &csrf,
    );
    let id = v["job"]["id"].as_str().unwrap().to_string();

    wait_status(&client, &base, &id, "running");

    let (_, v) = post_json(
        &client,
        &format!("{base}/api/jobs/{id}/cancel"),
        &serde_json::json!({}),
        &csrf,
    );
    assert_eq!(v["job"]["status"], "canceled", "{v}");

    child.kill().unwrap();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}
