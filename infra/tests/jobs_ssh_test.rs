//! `momoctl jobs --executor ssh` 二进制级回归：
//! - flag 校验（缺 --host / local 执行器带 ssh flag → exit 2）；
//! - `jobs recover` 对非 ssh job → exit 2；
//! - 真 ssh 冒烟：仅在 `MOMO_TEST_SSH_HOST` 设置时跑（手动验证用，
//!   CI 默认跳过——本机需能 `ssh <host>` 非交互直连）。

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(30);

fn env_setup(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("momoctl-jobsssh-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let state = dir.join("state");
    fs::create_dir_all(&state).unwrap();
    let env_file = dir.join("empty.env");
    fs::write(&env_file, "").unwrap();
    (dir, state, env_file)
}

fn momoctl(state: &std::path::Path, env_file: &std::path::Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_momoctl"));
    cmd.args(args)
        .args(["--env-file"])
        .arg(env_file)
        .env("XDG_STATE_HOME", state);
    cmd
}

/// `momoctl` 调用限时：`output()` 无超时，CLI 内部挂起会拖死整个测试
/// 二进制。超限杀进程并带 argv 点名挂起的调用方。
fn momoctl_output(
    state: &std::path::Path,
    env_file: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    let mut child = momoctl(state, env_file, args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn momoctl");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(_status) = child.try_wait().expect("try_wait momoctl") {
            return child.wait_with_output().expect("read momoctl output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("momoctl {args:?} did not exit within 60s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn jobs_json(
    state: &std::path::Path,
    env_file: &std::path::Path,
    args: &[&str],
) -> serde_json::Value {
    let mut argv: Vec<&str> = vec!["--json"];
    argv.extend_from_slice(args);
    let out = momoctl_output(state, env_file, &argv);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "momoctl {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("bad json from {args:?}: {e}\n{stdout}"))
}

/// `--executor ssh` 缺 `--host` → 用法错误 exit 2。
#[test]
fn ssh_submit_requires_host() {
    let (dir, state, env_file) = env_setup("nohost");
    let out = momoctl_output(
        &state,
        &env_file,
        &["jobs", "submit", "--executor", "ssh", "--", "echo", "hi"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--host"));
    let _ = fs::remove_dir_all(&dir);
}

/// local 执行器携带 ssh flag → 用法错误 exit 2。
#[test]
fn ssh_flags_rejected_for_local_executor() {
    let (dir, state, env_file) = env_setup("localflag");
    let out = momoctl_output(
        &state,
        &env_file,
        &["jobs", "submit", "--host", "4090D", "--", "echo", "hi"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--executor ssh"));
    let _ = fs::remove_dir_all(&dir);
}

/// `jobs recover` 对 exec_local job → exit 2（只对 ssh job 有意义）。
#[test]
fn recover_rejects_local_job() {
    let (dir, state, env_file) = env_setup("recvlocal");
    let exe = env!("CARGO_BIN_EXE_momoctl");
    let v = jobs_json(
        &state,
        &env_file,
        &["jobs", "submit", "--", exe, "--version"],
    );
    let id = v["job"]["id"].as_str().unwrap().to_string();
    let out = momoctl_output(&state, &env_file, &["jobs", "recover", &id]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not an ssh job"));
    let _ = fs::remove_dir_all(&dir);
}

/// 真 ssh 冒烟（手动）：`MOMO_TEST_SSH_HOST=<alias>` 设置才跑。
/// 跑通 = bundle 推上去、远端 echo 日志拉回本地 store、终态 succeeded。
/// 例：`MOMO_TEST_SSH_HOST=localhost cargo test --test jobs_ssh_test real_ssh -- --ignored`
#[test]
#[ignore = "manual smoke: requires a reachable ssh host"]
fn real_ssh_submit_runs_to_success() {
    let Ok(host) = std::env::var("MOMO_TEST_SSH_HOST") else {
        return;
    };
    let (dir, state, env_file) = env_setup("realssh");
    let v = jobs_json(
        &state,
        &env_file,
        &[
            "jobs",
            "submit",
            "--executor",
            "ssh",
            "--host",
            &host,
            "--",
            "echo",
            "momo-ssh-smoke-ok",
        ],
    );
    let id = v["job"]["id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + DEADLINE;
    let job = loop {
        let v = jobs_json(&state, &env_file, &["jobs", "status", &id]);
        let job = &v["job"];
        if matches!(
            job["status"].as_str().unwrap_or(""),
            "succeeded" | "failed" | "canceled"
        ) {
            break job.clone();
        }
        assert!(Instant::now() < deadline, "ssh job stuck: {job}");
        std::thread::sleep(Duration::from_secs(1));
    };
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(job["executor"], "ssh");
    assert!(job["remote_id"].is_string(), "remote job id recorded");
    let log = jobs_json(&state, &env_file, &["jobs", "log", &id]);
    let text = log["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("momo-ssh-smoke-ok"), "log: {text}");
    let _ = fs::remove_dir_all(&dir);
}
