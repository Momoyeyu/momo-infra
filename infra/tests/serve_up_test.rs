//! `momoctl serve up` 二进制级回归：`--only` 未命中 → 2；external brain
//! 探活失败 → 1 且回收已 spawn 组件；`--json` 可解析带 schema。
//! 不依赖真实 uv/momod：探活失败在 spawn 前即中止。

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn env_setup(tag: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("momoctl-serve-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let env_file = dir.join("empty.env");
    fs::write(&env_file, "").unwrap();
    (dir, env_file)
}

fn momoctl(dir: &PathBuf, env_file: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_momoctl"))
        .args(args)
        .args(["--env-file"])
        .arg(env_file)
        // 状态目录隔离 + 大脑端点指向无监听端口（connect refused）
        .env("XDG_STATE_HOME", dir)
        .env("MOMO_LLM_ENDPOINT", "http://127.0.0.1:1")
        .output()
        .expect("spawn momoctl")
}

#[test]
fn serve_up_only_miss_is_usage_error() {
    let (dir, env_file) = env_setup("only");
    let out = momoctl(&dir, &env_file, &["serve", "up", "--only", "nope"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn serve_up_json_schema_and_external_fail() {
    let (dir, env_file) = env_setup("json");
    let out = momoctl(&dir, &env_file, &["--json", "serve", "up"]);
    // normal 档 order=0 是 external brain 探活：127.0.0.1:1 不可达 → fail+abort
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], 1);
    assert_eq!(v["command"], "serve up");
    assert_eq!(v["profile"], "normal");
    assert_eq!(v["summary"]["fail"], 1);
    let checks = v["checks"].as_array().unwrap();
    assert_eq!(checks[0]["id"], "brain");
    assert_eq!(checks[0]["status"], "fail");
}
