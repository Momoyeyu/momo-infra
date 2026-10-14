//! `momoctl pipeline` 编排测试：定义解析、串行执行、重试、断点续跑、
//! register 产物登记、dry-run。执行器为记录型 fake，不跑真实命令/训练。

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use momo_infra::manifest::{Manifest, ModelsLock};
use momo_infra::pipeline::{self, ExecSpec, Executor, PipelineDef, RunOpts, RunStatus, StepStatus};

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("momoctl-pipeline-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 记录调用并按队列返回退出码（队列耗尽默认 0）。
struct FakeExecutor {
    calls: Mutex<Vec<String>>,
    results: Mutex<VecDeque<i32>>,
}

impl FakeExecutor {
    fn new(results: &[i32]) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            results: Mutex::new(results.iter().copied().collect()),
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Executor for FakeExecutor {
    fn run(&self, spec: &ExecSpec, _log: &Path) -> Result<i32, momo_infra::output::AppError> {
        self.calls.lock().unwrap().push(spec.display.clone());
        Ok(self.results.lock().unwrap().pop_front().unwrap_or(0))
    }
}

const TWO_STEP: &str = r#"
name = "demo"
description = "test pipeline"
artifact_dir = "tts/ft-demo-{date}"

[vars.input]
description = "raw input"
required = true

[[step]]
name = "prep"
exec = "local"
workdir = "models"
command = "prep {var:input} {run_dir}/dataset"
artifact = "{run_dir}/dataset"

[[step]]
name = "train"
exec = "ssh"
ssh_host = "{var:train_host}"
workdir = "{var:remote_root}/models"
command = "train --data x --output y"
retries = 1

[[step]]
name = "register"
exec = "register"
artifact = "{artifact_dir}"

[vars.train_host]
default = "bigstrong"

[vars.remote_root]
default = "~/momo"
"#;

fn def(text: &str) -> PipelineDef {
    toml::from_str::<PipelineDef>(text).unwrap_or_else(|e| panic!("def parse: {e}"))
}

fn opts(root: &Path, date: &str, vars: &[(&str, &str)], dry: bool) -> RunOpts {
    let manifest = root.join("MANIFEST.toml");
    if !manifest.exists() {
        fs::write(&manifest, "# test manifest\n").unwrap();
    }
    RunOpts {
        runs_root: root.join("runs"),
        models_dir: root.join("models"),
        repo_root: root.join("repo"),
        manifest_path: manifest,
        date: date.into(),
        dry_run: dry,
        vars: vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
    }
}

#[test]
fn shipped_defs_parse() {
    // 实仓定义：两条 pipeline 可解析、step 序与设计一致。
    for (file, expect) in [
        (
            "pipelines/voice-sft.toml",
            vec![
                "prepare-dataset",
                "upload-dataset",
                "sft-cosyvoice3",
                "fetch-artifact",
                "frozen-gate",
                "register",
            ],
        ),
        (
            "pipelines/persona-qlora.toml",
            vec![
                "build-dataset",
                "baseline-regression",
                "upload-dataset",
                "lora-qwen",
                "fetch-artifact",
                "frozen-gate",
                "register",
            ],
        ),
    ] {
        let d = PipelineDef::load(&PathBuf::from(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        let names: Vec<&str> = d.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, expect, "{file} step order");
        let last = d.steps.last().unwrap();
        assert!(
            matches!(last.exec, pipeline::ExecKind::Register),
            "{file} last step"
        );
    }
}

#[test]
fn run_requires_vars_and_expands() {
    let root = tmpdir("vars");
    let d = def(TWO_STEP);
    let o = opts(&root, "20261101", &[], false);
    let exec = FakeExecutor::new(&[]);
    let err = pipeline::run_pipeline(&d, &o, &exec).unwrap_err();
    assert!(err.to_string().contains("input"), "{err}");

    // register 步需要现成产物目录 + train-manifest.json。
    let artifact = root.join("models/tts/ft-demo-20261101");
    fs::create_dir_all(&artifact).unwrap();
    fs::write(artifact.join("train-manifest.json"), "{}").unwrap();

    let o = opts(&root, "20261101", &[("input", "/raw/voice")], false);
    let run = pipeline::run_pipeline(&d, &o, &exec).unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);
    let calls = exec.calls();
    assert_eq!(
        calls.len(),
        2,
        "register step never reaches executor: {calls:?}"
    );
    assert!(calls[0].contains("prep /raw/voice"), "{}", calls[0]);
    assert!(calls[1].starts_with("ssh bigstrong"), "{}", calls[1]);
}

#[test]
fn run_persists_step_manifests() {
    let root = tmpdir("persist");
    let d = def(TWO_STEP_NO_REG);
    let o = opts(&root, "20261101", &[("input", "/raw")], false);
    let exec = FakeExecutor::new(&[]);
    let run = pipeline::run_pipeline(&d, &o, &exec).unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);

    let run_dir = PathBuf::from(&run.run_dir);
    assert!(run_dir.join("run.json").is_file());
    for name in ["prep", "train"] {
        let p = run_dir.join("steps").join(format!("{name}.json"));
        assert!(p.is_file(), "missing {p:?}");
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["name"], name);
        assert_eq!(v["status"], "succeeded");
        assert!(v["started_at"].is_string());
        assert!(v["finished_at"].is_string());
    }
    // resume 已成功的 run 直接拒绝。
    let err = pipeline::resume_pipeline(&d, &o, &run_dir, &exec).unwrap_err();
    assert!(err.to_string().contains("already succeeded"), "{err}");
}

const TWO_STEP_NO_REG: &str = r#"
name = "demo"
artifact_dir = "tts/ft-demo-{date}"

[vars.input]
required = true

[[step]]
name = "prep"
exec = "local"
command = "prep {var:input}"
artifact = "{run_dir}/dataset"

[[step]]
name = "train"
exec = "ssh"
ssh_host = "bigstrong"
command = "train"
"#;

#[test]
fn failure_stops_and_resume_skips_succeeded() {
    let root = tmpdir("resume");
    let d = def(TWO_STEP_NO_REG);
    let o = opts(&root, "20261101", &[("input", "/raw")], false);

    // 首跑：prep ok、train exit 3（retries=0 → 1 次尝试）。
    let exec = FakeExecutor::new(&[0, 3]);
    let run = pipeline::run_pipeline(&d, &o, &exec).unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.steps[0].status, StepStatus::Succeeded);
    assert_eq!(run.steps[1].status, StepStatus::Failed);
    assert!(run.steps[1].error.as_deref().unwrap().contains("exit 3"));

    // 续跑：只重跑 train，prep 不重放。
    let exec2 = FakeExecutor::new(&[0]);
    let run2 = pipeline::resume_pipeline(&d, &o, &PathBuf::from(&run.run_dir), &exec2).unwrap();
    assert_eq!(run2.status, RunStatus::Succeeded);
    let calls = exec2.calls();
    assert_eq!(
        calls.len(),
        1,
        "resume must skip succeeded steps: {calls:?}"
    );
    assert!(calls[0].contains("ssh"), "{}", calls[0]);
}

#[test]
fn retry_counts_attempts() {
    let root = tmpdir("retry");
    let d = def(r#"
name = "demo"
artifact_dir = "tts/ft-demo-{date}"
[[step]]
name = "flaky"
exec = "local"
command = "flaky"
retries = 2
"#);
    let o = opts(&root, "20261101", &[], false);
    // 失败两次、第三次成功：retries=2 → max_attempts=3。
    let exec = FakeExecutor::new(&[1, 1, 0]);
    let run = pipeline::run_pipeline(&d, &o, &exec).unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(run.steps[0].attempts, 3);
    assert_eq!(exec.calls().len(), 3);
}

#[test]
fn register_appends_manifest_and_lock() {
    let root = tmpdir("register");
    let d = def(r#"
name = "demo"
artifact_dir = "tts/ft-demo-{date}"
[[step]]
name = "register"
exec = "register"
artifact = "{artifact_dir}"
"#);
    // 产物目录 + train-manifest.json 就位（训练脚本产出物，本测试手工放）。
    let artifact = root.join("models/tts/ft-demo-20261101");
    fs::create_dir_all(artifact.join("ckpt")).unwrap();
    fs::write(
        artifact.join("train-manifest.json"),
        r#"{"base_revision":"main","data_count":120,"lr":0.0001,"steps":800,"loss":0.42}"#,
    )
    .unwrap();
    fs::write(artifact.join("ckpt/weights.bin"), b"weights").unwrap();
    fs::write(artifact.join("adapter.json"), b"{}").unwrap();

    let o = opts(&root, "20261101", &[], false);
    let exec = FakeExecutor::new(&[]);
    let run = pipeline::run_pipeline(&d, &o, &exec).unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);

    // MANIFEST 条目：name/dest/files 全量 hash，pull --check 同口径。
    let text = fs::read_to_string(root.join("MANIFEST.toml")).unwrap();
    let m = Manifest::parse(&text).unwrap();
    let entry = m
        .models
        .iter()
        .find(|e| e.name == "ft-demo-20261101")
        .expect("manifest entry");
    assert_eq!(entry.dest(), PathBuf::from("tts/ft-demo-20261101"));
    assert_eq!(entry.files.len(), 3, "all artifact files registered");

    // models.lock 条目：<dest>@<date> + 文件 size/sha256。
    let lock = ModelsLock::load(&root.join("models/models.lock")).unwrap();
    let le = lock
        .entries
        .iter()
        .find(|e| e.name == "ft-demo-20261101")
        .expect("lock entry");
    assert_eq!(le.repo, "tts/ft-demo-20261101@20261101");
    assert!(le
        .files
        .iter()
        .any(|f| f.path == "ckpt/weights.bin" && f.size == 7));

    // 同名二次登记拒绝（同日 rerun 防护）：run 记录为 failed 而非 panic。
    let run2 = pipeline::run_pipeline(&d, &o, &exec).unwrap();
    assert_eq!(run2.status, RunStatus::Failed);
    assert!(run2.steps[0]
        .error
        .as_deref()
        .unwrap()
        .contains("already has entry"));
}

#[test]
fn register_requires_train_manifest() {
    let root = tmpdir("regreq");
    let d = def(r#"
name = "demo"
artifact_dir = "tts/ft-demo-{date}"
[[step]]
name = "register"
exec = "register"
"#);
    fs::create_dir_all(root.join("models/tts/ft-demo-20261101")).unwrap();
    let o = opts(&root, "20261101", &[], false);
    let exec = FakeExecutor::new(&[]);
    let run = pipeline::run_pipeline(&d, &o, &exec).unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.steps[0]
        .error
        .as_deref()
        .unwrap()
        .contains("train-manifest.json"));
}

#[test]
fn dry_run_persists_nothing() {
    let root = tmpdir("dry");
    let d = def(TWO_STEP_NO_REG);
    let o = opts(&root, "20261101", &[("input", "/raw")], true);
    let exec = FakeExecutor::new(&[]);
    let run = pipeline::run_pipeline(&d, &o, &exec).unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);
    assert!(run.dry_run);
    assert!(
        !root.join("runs").exists(),
        "dry-run must not persist state"
    );
}
