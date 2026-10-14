//! 训练页（/train，#42）二进制级回归：
//! - GET /api/pipelines：实仓定义清单（vars/activate_env 随包下发）；
//! - POST /api/jobs `{pipeline, set, dry_run}` 快捷提交 → detached runner 执行
//!   `momoctl pipeline run --dry-run` → succeeded；
//! - GET /api/jobs/{id}/metrics：手工补 pipeline run + train-manifest.json →
//!   loss 曲线数据源；
//! - POST /api/jobs/{id}/promote：succeeded + gate 证据 → activate_env 写回
//!   `.env.normal`（.bak 保留）→ promoted；重复 promote 幂等；
//! - running job promote → 409；ssh executor → 422 字段级拒绝。
//!   store/env/产物全部落在临时目录（XDG_STATE_HOME 隔离）。

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// (base dir, state dir, env link, profile env file)
fn env_setup(tag: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("momoctl-trainapi-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let state = dir.join("state");
    let models = dir.join("models");
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&models).unwrap();
    let env_file = dir.join("empty.env");
    fs::write(&env_file, "").unwrap();
    // promote 写回目标：`.env.<active>`（缺省 normal）。
    let profile_env = dir.join("empty.env.normal");
    fs::write(&profile_env, "MOMO_TTS_MODEL_DIR=\n").unwrap();
    (dir, state, env_file, profile_env)
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_web(env_file: &Path, state: &Path, models: &Path, listen: &str) -> (Child, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_momoctl"));
    let child = cmd
        .args(["web", "--listen", listen, "--env-file"])
        .arg(env_file)
        .env("XDG_STATE_HOME", state)
        .env("MOMO_MODELS_DIR", models)
        .env_remove("MOMO_WEB_TOKEN")
        .env_remove("MOMO_PROFILE")
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

fn get_json(client: &reqwest::blocking::Client, url: &str) -> (u16, serde_json::Value) {
    let r = client.get(url).send().unwrap();
    let code = r.status().as_u16();
    let v: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    (code, v)
}

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

fn wait_status(client: &reqwest::blocking::Client, base: &str, id: &str, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, v) = get_json(client, &format!("{base}/api/jobs/{id}"));
        if v["job"]["status"] == want {
            return;
        }
        if matches!(v["job"]["status"].as_str(), Some("failed" | "canceled")) && want != "failed" {
            panic!("job {id} reached {}: {v}", v["job"]["status"]);
        }
        if Instant::now() >= deadline {
            panic!("job {id} did not reach {want}: {v}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 手工补一个 succeeded pipeline run（run.json + artifact/train-manifest.json）。
/// run 目录名用 job id：ULID 字典序命中 `find_run_for_job` 的 `>= job id` 分支。
fn craft_run(state: &Path, models: &Path, job_id: &str) -> PathBuf {
    let artifact = models.join("tts/ft-cosyvoice3-20260101");
    fs::create_dir_all(&artifact).unwrap();
    fs::write(
        artifact.join("train-manifest.json"),
        serde_json::json!({
            "base_revision": "abc123",
            "data_count": 42,
            "loss": [0.9, 0.5, 0.3, 0.2],
        })
        .to_string(),
    )
    .unwrap();
    let run_dir = state.join("momo/pipeline-runs/voice-sft").join(job_id);
    fs::create_dir_all(&run_dir).unwrap();
    fs::write(
        run_dir.join("run.json"),
        serde_json::json!({
            "pipeline": "voice-sft",
            "run_id": job_id,
            "status": "succeeded",
            "date": "20260101",
            "artifact_dir": artifact.display().to_string(),
            "run_dir": run_dir.display().to_string(),
            "dry_run": false,
            "started_at": "2026-01-01T00:00:00Z",
            "steps": [{
                "name": "sft_cosyvoice3",
                "status": "succeeded",
                "metrics": {"loss": [0.9, 0.5, 0.3, 0.2]},
            }],
        })
        .to_string(),
    )
    .unwrap();
    artifact
}

#[test]
fn api_train_flow_pipelines_submit_metrics_promote() {
    let (dir, state, env_file, profile_env) = env_setup("flow");
    let models = dir.join("models");
    let port = free_port();
    let (mut child, base) = spawn_web(&env_file, &state, &models, &format!("127.0.0.1:{port}"));
    let client = reqwest::blocking::Client::new();
    let csrf = fetch_csrf(&client, &base);

    // ---- GET /api/pipelines：wizard 数据源 ----
    let (code, v) = get_json(&client, &format!("{base}/api/pipelines"));
    assert_eq!(code, 200);
    assert_eq!(v["schema"], 1);
    let pipes = v["pipelines"].as_array().unwrap();
    let voice = pipes
        .iter()
        .find(|p| p["name"] == "voice-sft")
        .expect("voice-sft present");
    assert_eq!(voice["activate_env"], "MOMO_TTS_MODEL_DIR", "{voice}");
    let vars = voice["vars"].as_array().unwrap();
    assert!(
        vars.iter()
            .any(|x| x["name"] == "input" && x["required"] == true),
        "voice-sft input var: {vars:?}"
    );

    // ---- 校验：pipeline+ssh executor → 422 字段级；required var 缺项 → 422 ----
    let (code, v) = post_json(
        &client,
        &format!("{base}/api/jobs"),
        &serde_json::json!({"pipeline": "persona-qlora", "executor": "ssh", "ssh": {"host": "bigstrong"}}),
        &csrf,
    );
    assert_eq!(code, 422, "{v}");
    assert!(v["error"]["fields"]["executor"].is_string(), "{v}");

    let (code, v) = post_json(
        &client,
        &format!("{base}/api/jobs"),
        &serde_json::json!({"pipeline": "voice-sft"}),
        &csrf,
    );
    assert_eq!(code, 422, "{v}");
    assert!(v["error"]["fields"]["set.input"].is_string(), "{v}");

    // ---- pipeline 快捷提交（dry-run，不真跑训练）----
    let (code, v) = post_json(
        &client,
        &format!("{base}/api/jobs"),
        &serde_json::json!({
            "kind": "train",
            "pipeline": "voice-sft",
            "set": {"input": "/tmp/voice-raw"},
            "executor": "local",
            "dry_run": true,
        }),
        &csrf,
    );
    assert_eq!(code, 200, "{v}");
    let id = v["job"]["id"].as_str().unwrap().to_string();
    wait_status(&client, &base, &id, "succeeded");

    // ---- running job promote → 409（先建 sleeper）----
    let (_, v) = post_json(
        &client,
        &format!("{base}/api/jobs"),
        &serde_json::json!({
            "desc": "api sleeper",
            "args": [env!("CARGO_BIN_EXE_momoctl"), "serve", "logs", "-f"],
        }),
        &csrf,
    );
    let sleeper = v["job"]["id"].as_str().unwrap().to_string();
    wait_status(&client, &base, &sleeper, "running");
    let (code, v) = post_json(
        &client,
        &format!("{base}/api/jobs/{sleeper}/promote"),
        &serde_json::json!({}),
        &csrf,
    );
    assert_eq!(code, 409, "{v}");
    assert_eq!(v["error"]["code"], "conflict", "{v}");
    post_json(
        &client,
        &format!("{base}/api/jobs/{sleeper}/cancel"),
        &serde_json::json!({}),
        &csrf,
    );

    // ---- 手工补 run + 产物，metrics → manifest 原文 ----
    let artifact = craft_run(&state, &models, &id);
    let (code, v) = get_json(&client, &format!("{base}/api/jobs/{id}/metrics"));
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["run_id"], id, "{v}");
    assert_eq!(v["manifest"]["loss"].as_array().unwrap().len(), 4, "{v}");
    assert!(
        v["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "sft_cosyvoice3"),
        "{v}"
    );

    // ---- promote：activate_env 写回 .env.normal + .bak 保留 ----
    let (code, v) = post_json(
        &client,
        &format!("{base}/api/jobs/{id}/promote"),
        &serde_json::json!({}),
        &csrf,
    );
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["job"]["status"], "promoted", "{v}");
    let env_text = fs::read_to_string(&profile_env).unwrap();
    assert!(
        env_text.contains(&format!("MOMO_TTS_MODEL_DIR={}", artifact.display())),
        "env write-back: {env_text}"
    );
    assert!(
        profile_env.with_file_name("empty.env.normal.bak").is_file(),
        ".bak missing"
    );

    // 幂等：重复 promote → 200 already promoted
    let (code, v) = post_json(
        &client,
        &format!("{base}/api/jobs/{id}/promote"),
        &serde_json::json!({}),
        &csrf,
    );
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["job"]["status"], "promoted", "{v}");
    assert_eq!(v["note"], "already promoted", "{v}");

    // ---- SSE 日志：dry-run 输出应含展开后的 pipeline step 行 ----
    let r = client
        .get(format!("{base}/api/jobs/{id}/logs?cursor=0"))
        .send()
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().unwrap();
    assert!(body.contains("event: log"), "sse: {body}");
    assert!(body.contains("event: end"), "sse: {body}");
    assert!(body.contains("voice-sft"), "sse: {body}");

    child.kill().unwrap();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn api_promote_missing_gate_evidence() {
    let (dir, state, env_file, _profile_env) = env_setup("gate");
    let models = dir.join("models");
    let port = free_port();
    let (mut child, base) = spawn_web(&env_file, &state, &models, &format!("127.0.0.1:{port}"));
    let client = reqwest::blocking::Client::new();
    let csrf = fetch_csrf(&client, &base);

    // 普通 args job（非 pipeline）：succeeded 但无法解析产物 → 409。
    let (_, v) = post_json(
        &client,
        &format!("{base}/api/jobs"),
        &serde_json::json!({
            "desc": "plain job",
            "args": [env!("CARGO_BIN_EXE_momoctl"), "--version"],
        }),
        &csrf,
    );
    let id = v["job"]["id"].as_str().unwrap().to_string();
    wait_status(&client, &base, &id, "succeeded");

    let (code, v) = post_json(
        &client,
        &format!("{base}/api/jobs/{id}/promote"),
        &serde_json::json!({}),
        &csrf,
    );
    assert_eq!(code, 409, "{v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("artifact"),
        "{v}"
    );

    // 显式 artifact_dir 但缺 train-manifest.json → 409（frozen gate 证据缺失）。
    let empty = dir.join("empty-artifact");
    fs::create_dir_all(&empty).unwrap();
    let (code, v) = post_json(
        &client,
        &format!("{base}/api/jobs/{id}/promote"),
        &serde_json::json!({"artifact_dir": empty.display().to_string()}),
        &csrf,
    );
    assert_eq!(code, 409, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("train-manifest.json"),
        "{v}"
    );

    // 显式 artifact_dir + manifest + env_key 覆盖 → promoted。
    fs::write(empty.join("train-manifest.json"), r#"{"loss":[0.5]}"#).unwrap();
    let (code, v) = post_json(
        &client,
        &format!("{base}/api/jobs/{id}/promote"),
        &serde_json::json!({
            "artifact_dir": empty.display().to_string(),
            "env_key": "MOMO_TTS_MODEL_DIR",
        }),
        &csrf,
    );
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["job"]["status"], "promoted", "{v}");

    child.kill().unwrap();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}
