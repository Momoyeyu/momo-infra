//! `serve logs` 回归：组件 tail、-n/--only 过滤、未知组件 exit 2、无日志文件 exit 0。

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn env_setup(tag: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("momoctl-logs-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    // serve 日志落在 ${XDG_STATE_HOME}/momo/logs/
    fs::create_dir_all(dir.join("momo/logs")).unwrap();
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
fn serve_logs_tails_component_log() {
    let (dir, env_file) = env_setup("tail");
    fs::write(
        dir.join("momo/logs/momod.log"),
        "line1\nline2\nline3\nline4\nline5\n",
    )
    .unwrap();
    let out = momoctl(
        &dir,
        &env_file,
        &["serve", "logs", "--only", "momod", "-n", "2"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("line4"));
    assert!(stdout.contains("line5"));
    assert!(
        !stdout.contains("line2"),
        "-n 2 must not print earlier lines"
    );
}

#[test]
fn serve_logs_component_alias_and_json() {
    let (dir, env_file) = env_setup("json");
    fs::write(dir.join("momo/logs/momo-svc-asr.log"), "asr-a\nasr-b\n").unwrap();
    let out = momoctl(
        &dir,
        &env_file,
        &[
            "--json",
            "serve",
            "logs",
            "--component",
            "momo-svc-asr",
            "-n",
            "5",
        ],
    );
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], 1);
    assert_eq!(v["command"], "serve logs");
    let logs = v["logs"].as_array().unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["component"], "momo-svc-asr");
    assert_eq!(logs[0]["lines"], serde_json::json!(["asr-a", "asr-b"]));
}

#[test]
fn serve_logs_missing_log_file_is_noop() {
    let (dir, env_file) = env_setup("missing");
    let out = momoctl(&dir, &env_file, &["serve", "logs", "--only", "momod"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("no log file"));
}

#[test]
fn serve_logs_unknown_component_is_usage_error() {
    let (dir, env_file) = env_setup("unknown");
    let out = momoctl(&dir, &env_file, &["serve", "logs", "--only", "nope"]);
    assert_eq!(out.status.code(), Some(2));
}
