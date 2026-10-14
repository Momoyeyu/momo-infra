//! `momoctl jobs` 二进制级回归：
//! - submit → detached runner → succeeded，stdout/stderr 增量捕获到日志；
//! - spawn 失败 → failed；running job cancel → canceled；
//! - `--json` 输出顶层 `schema:1`；未知 id → exit 2。
//!   状态目录用 `XDG_STATE_HOME` 隔离到临时目录（约定见 momo-infra.md）。

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(20);

fn env_setup(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("momoctl-jobs-{tag}-{}", std::process::id()));
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

fn momoctl_exe() -> String {
    env!("CARGO_BIN_EXE_momoctl").to_string()
}

/// `momoctl` 调用限时：`output()` 无超时，且进程退出后等 pipe EOF 也可能
/// 无限阻塞（detached runner 继承 pipe 写端，windows CI 实证）——必须把
/// wait+read 整体放进 worker 线程限时。超限按 pid 杀树并点名 argv。
fn momoctl_output(
    state: &std::path::Path,
    env_file: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    let child = momoctl(state, env_file, args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn momoctl");
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(out) => out.expect("read momoctl output"),
        Err(_) => {
            momo_infra::serve::proc::stop_pid(pid, Duration::from_secs(2));
            panic!("momoctl {args:?} did not exit within 60s");
        }
    }
}

/// `momoctl --json jobs <action>` → 解析后的 JSON 值。
/// `--json` 必须放最前：submit 的 `--` 之后的参数都算 payload argv。
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

/// 轮询 `jobs status` 直到终态或超时；返回最终 job JSON。
fn wait_terminal(
    state: &std::path::Path,
    env_file: &std::path::Path,
    id: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let v = jobs_json(state, env_file, &["jobs", "status", id]);
        let status = v["job"]["status"].as_str().unwrap_or("");
        if matches!(status, "succeeded" | "failed" | "canceled" | "promoted") {
            return v["job"].clone();
        }
        if Instant::now() >= deadline {
            panic!("job {id} did not reach a terminal state: {v}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 轮询直到 job 记录出现 pid（running）或终态。
fn wait_running_pid(
    state: &std::path::Path,
    env_file: &std::path::Path,
    id: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let v = jobs_json(state, env_file, &["jobs", "status", id]);
        let job = &v["job"];
        if job["pid"].is_u64()
            || ["succeeded", "failed", "canceled"].contains(&job["status"].as_str().unwrap_or(""))
        {
            if let Some(pid) = job["pid"].as_u64() {
                // sleeper 是 `serve logs -f`：启动时向 runner 的 stdout 管写头部。
                // 若此后立刻杀 runner，首写撞 EPIPE 会让 payload 退出——给一拍
                // 让启动输出先排空，并确认 payload 健在（区分自退与后杀）。
                std::thread::sleep(Duration::from_millis(600));
                assert!(
                    momo_infra::serve::proc::is_alive(pid as u32),
                    "payload {pid} exited on its own before running was observed"
                );
            }
            return job.clone();
        }
        if Instant::now() >= deadline {
            panic!("job {id} did not reach running: {v}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 平台无关 sleeper payload：`momoctl serve logs -f` 打印 tail 后无限轮询。
fn sleeper_args(state: &std::path::Path) -> Vec<String> {
    let _ = state;
    vec![momoctl_exe(), "serve".into(), "logs".into(), "-f".into()]
}

#[test]
fn submit_runs_to_success_and_captures_log() {
    let (dir, state, env_file) = env_setup("ok");
    let exe = momoctl_exe();
    let v = jobs_json(
        &state,
        &env_file,
        &[
            "jobs",
            "submit",
            "--desc",
            "test-job",
            "--",
            &exe,
            "--version",
        ],
    );
    assert_eq!(v["schema"], 1);
    assert_eq!(v["job"]["status"], "queued");
    let id = v["job"]["id"].as_str().unwrap().to_string();

    let job = wait_terminal(&state, &env_file, &id);
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(job["exit"], 0);
    assert!(job["pid"].is_u64(), "process identity recorded: {job}");
    assert!(job["runner_pid"].is_u64(), "runner pid recorded: {job}");

    // 日志捕获 payload stdout + runner 诊断行
    let log = jobs_json(&state, &env_file, &["jobs", "log", &id]);
    let lines = log["lines"].as_array().unwrap();
    let text = lines
        .iter()
        .map(|l| l.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("momoctl 0.1.0"), "log lines: {text}");
    assert!(text.contains("[runner] spawned"), "log lines: {text}");

    // jobs.jsonl 索引按序记录迁移
    let jsonl = fs::read_to_string(state.join("momo/jobs/jobs.jsonl")).unwrap();
    for needle in [
        "\"status\":\"queued\"",
        "\"status\":\"running\"",
        "\"status\":\"succeeded\"",
    ] {
        assert!(jsonl.contains(needle), "index lacks {needle}: {jsonl}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn submit_bad_program_fails() {
    let (dir, state, env_file) = env_setup("badspawn");
    let v = jobs_json(
        &state,
        &env_file,
        &["jobs", "submit", "--", "definitely-no-such-bin-momo-42"],
    );
    let id = v["job"]["id"].as_str().unwrap().to_string();
    let job = wait_terminal(&state, &env_file, &id);
    assert_eq!(job["status"], "failed", "{job}");
    assert!(
        job["error"].as_str().unwrap_or("").contains("spawn failed"),
        "{job}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cancel_running_job() {
    let (dir, state, env_file) = env_setup("cancel");
    let sleeper = sleeper_args(&state);
    let refs: Vec<&str> = sleeper.iter().map(String::as_str).collect();
    let mut argv = vec!["jobs", "submit", "--desc", "sleeper", "--"];
    argv.extend(refs);
    let v = jobs_json(&state, &env_file, &argv);
    let id = v["job"]["id"].as_str().unwrap().to_string();

    let running = wait_running_pid(&state, &env_file, &id);
    assert_eq!(running["status"], "running", "{running}");
    let pid = running["pid"].as_u64().unwrap() as u32;

    let v = jobs_json(&state, &env_file, &["jobs", "cancel", &id]);
    assert_eq!(v["job"]["status"], "canceled", "{v}");
    assert!(
        !momo_infra::serve::proc::is_alive(pid),
        "payload pid {pid} must be stopped"
    );

    // cancel 是独立终态，不写成 failed；再 cancel 幂等
    let job = wait_terminal(&state, &env_file, &id);
    assert_eq!(job["status"], "canceled", "{job}");
    let v = jobs_json(&state, &env_file, &["jobs", "cancel", &id]);
    assert_eq!(v["job"]["status"], "canceled");
    let _ = fs::remove_dir_all(&dir);
}

/// #54 rescan：杀掉 detached runner 后，下一次读侧调用（jobs list）触发
/// reconcile——payload 还活着 → 保持 running（adopt，不重复 launch）；
/// 再杀 payload → failed(interrupted: runner restart)。
#[test]
fn rescan_recovers_running_job_after_runner_death() {
    use momo_infra::serve::proc;

    let (dir, state, env_file) = env_setup("rescan");
    let sleeper = sleeper_args(&state);
    let refs: Vec<&str> = sleeper.iter().map(String::as_str).collect();
    let mut argv = vec!["jobs", "submit", "--desc", "sleeper", "--"];
    argv.extend(refs);
    let v = jobs_json(&state, &env_file, &argv);
    let id = v["job"]["id"].as_str().unwrap().to_string();

    let running = wait_running_pid(&state, &env_file, &id);
    let pid = running["pid"].as_u64().unwrap() as u32;
    let runner_pid = running["runner_pid"].as_u64().unwrap() as u32;

    // 杀 runner 不动 payload：list 触发 rescan → payload 探活 → 仍 running
    proc::stop_pid(runner_pid, Duration::from_secs(2));
    let deadline = Instant::now() + DEADLINE;
    while proc::is_alive(runner_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let v = jobs_json(&state, &env_file, &["jobs", "list"]);
    let job = v["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"].as_str() == Some(&id))
        .expect("job in list");
    assert_eq!(job["status"], "running", "payload alive → adopted: {job}");
    assert!(proc::is_alive(pid), "payload must still run");

    // 再杀 payload：rescan 确认双亡 → failed(interrupted)
    proc::stop_pid(pid, Duration::from_secs(2));
    let deadline = Instant::now() + DEADLINE;
    loop {
        let v = jobs_json(&state, &env_file, &["jobs", "status", &id]);
        let job = &v["job"];
        if job["status"] == "failed" {
            assert!(
                job["error"].as_str().unwrap_or("").contains("interrupted"),
                "{job}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "job {id} not marked failed: {job}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn unknown_job_id_is_usage_error() {
    let (dir, state, env_file) = env_setup("noid");
    let out = momoctl_output(
        &state,
        &env_file,
        &["jobs", "status", "01JAAAAAAAAAAAAAAAAAAAAAAA"],
    );
    assert_eq!(out.status.code(), Some(2));
    let _ = fs::remove_dir_all(&dir);
}
