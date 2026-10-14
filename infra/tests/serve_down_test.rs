//! `momoctl serve down/status/restart` 二进制级回归：全停稳态幂等 no-op；
//! `--only` 未命中 → 2；status --json 可解析。内置 profile 依赖真实
//! uv/momod，故只覆盖「全停」。

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn env_setup(tag: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("momoctl-down-{tag}-{}", std::process::id()));
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
        .env("XDG_STATE_HOME", dir)
        .env("MOMO_LLM_ENDPOINT", "http://127.0.0.1:1")
        .output()
        .expect("spawn momoctl")
}

#[test]
fn serve_down_on_stopped_system_is_noop() {
    let (dir, env_file) = env_setup("noop");
    let out = momoctl(&dir, &env_file, &["serve", "down"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // 再次 down 同样幂等
    let out = momoctl(&dir, &env_file, &["serve", "down"]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn serve_status_on_stopped_system() {
    let (dir, env_file) = env_setup("status");
    let out = momoctl(&dir, &env_file, &["--json", "serve", "status"]);
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], 1);
    assert_eq!(v["command"], "serve status");
    assert_eq!(v["profile"], "normal");
    // 全部组件 stopped / external warn，无 fail
    assert_eq!(v["summary"]["fail"], 0);
    let checks = v["checks"].as_array().unwrap();
    let momod = checks.iter().find(|c| c["id"] == "momod").unwrap();
    assert_eq!(momod["status"], "ok");
    assert!(momod["detail"].as_str().unwrap().contains("stopped"));
}

#[test]
fn serve_down_only_miss_is_usage_error() {
    let (dir, env_file) = env_setup("only");
    let out = momoctl(&dir, &env_file, &["serve", "down", "--only", "nope"]);
    assert_eq!(out.status.code(), Some(2));
    let out = momoctl(&dir, &env_file, &["serve", "status", "--only", "nope"]);
    assert_eq!(out.status.code(), Some(2));
}
