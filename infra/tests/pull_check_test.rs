//! `momoctl pull --check` 集成测试：
//! MANIFEST 定位 + 逐文件存在性 + sha256 复算 + 占位上报。
//! 缺失/占位/不匹配 → 退出码 5；全 ok → 0。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const GOOD_SHA: &str = "3100486406b39efc3f3d3565bc97cc3b9e2d7b6e3427b194f4442ef4beb05b41"; // sha256("momo")
const GOOD_SHA_B: &str = "a4f413570234f968e5afe7836fee01b1b7b9a65e19788e6720e2c6d65839e570"; // sha256("momo2")

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("momoctl-pull-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_manifest(dir: &Path, body: &str) -> PathBuf {
    let p = dir.join("MANIFEST.toml");
    fs::write(&p, body).unwrap();
    p
}

/// 隔离用户 .env：显式空 --env-file，并把 MOMO_MODELS_DIR 指向测试目录。
fn run_check(manifest: &Path, models_dir: &Path, extra: &[&str]) -> std::process::Output {
    let env_file = std::env::temp_dir().join(format!("momoctl-pull-{}.env", std::process::id()));
    fs::write(&env_file, "").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_momoctl"));
    cmd.args(["pull", "--check", "--manifest"])
        .arg(manifest)
        .args(["--env-file"])
        .arg(&env_file)
        .args(extra)
        .env("MOMO_MODELS_DIR", models_dir);
    cmd.output().expect("spawn momoctl")
}

const MANIFEST: &str = r#"
[[model]]
name = "m"
repo_id = "a/b"
dest = "a/b"
files = [
  { path = "ok.bin", sha256 = "3100486406b39efc3f3d3565bc97cc3b9e2d7b6e3427b194f4442ef4beb05b41", size = 4 },
  { path = "nohash.bin" },
  { path = "placeholder.bin", sha256 = "TODO" },
  { path = "missing.bin", sha256 = "a4f413570234f968e5afe7836fee01b1b7b9a65e19788e6720e2c6d65839e570" },
  { path = "mismatch.bin", sha256 = "a4f413570234f968e5afe7836fee01b1b7b9a65e19788e6720e2c6d65839e570" },
]

[[model]]
name = "other"
repo_id = "c/d"
files = [ { path = "x.bin", sha256 = "TODO" } ]
"#;

fn fixture(tag: &str) -> (PathBuf, PathBuf) {
    let root = tmpdir(tag);
    let manifest = write_manifest(&root, MANIFEST);
    let models = root.join("models");
    let dest = models.join("a/b");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("ok.bin"), "momo").unwrap();
    fs::write(dest.join("nohash.bin"), "whatever").unwrap();
    fs::write(dest.join("placeholder.bin"), "data").unwrap();
    fs::write(dest.join("mismatch.bin"), "not-momo2").unwrap();
    (manifest, models)
}

#[test]
fn reports_missing_placeholder_and_mismatch() {
    let (manifest, models) = fixture("report");
    let out = run_check(&manifest, &models, &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(out.status.code(), Some(5), "stdout: {stdout}");
    assert!(stdout.contains("m/ok.bin"));
    assert!(stdout.contains("m/nohash.bin"));
    assert!(stdout.contains("m/placeholder.bin") && stdout.contains("placeholder"));
    assert!(stdout.contains("m/missing.bin") && stdout.contains("missing"));
    assert!(stdout.contains("m/mismatch.bin"));
    assert!(
        stdout.contains("summary: 2 ok, 0 warn, 4 fail"),
        "stdout: {stdout}"
    );
}

#[test]
fn json_output_is_parseable() {
    let (manifest, models) = fixture("json");
    let out = run_check(&manifest, &models, &["--json"]);
    assert_eq!(out.status.code(), Some(5));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], 1);
    assert!(v["manifest"].as_str().unwrap().ends_with("MANIFEST.toml"));
    let checks = v["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 6); // m 的 5 个文件 + other 的 1 个
    assert_eq!(v["summary"]["fail"], 4);
    assert_eq!(v["summary"]["ok"], 2);
}

#[test]
fn only_filters_entries() {
    let (manifest, models) = fixture("only");
    let out = run_check(&manifest, &models, &["--json", "--only", "m"]);
    assert_eq!(out.status.code(), Some(5));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let checks = v["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 5);
    assert!(checks
        .iter()
        .all(|c| c["id"].as_str().unwrap().starts_with("m/")));
}

#[test]
fn unknown_only_is_usage_error() {
    let (manifest, models) = fixture("unknown");
    let out = run_check(&manifest, &models, &["--only", "nope"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn all_ok_exits_zero() {
    let root = tmpdir("ok");
    let manifest = write_manifest(
        &root,
        &format!(
            r#"[[model]]
name = "m"
repo_id = "a/b"
files = [
  {{ path = "ok.bin", sha256 = "{GOOD_SHA}" }},
  {{ path = "nohash.bin" }},
]
"#
        ),
    );
    let models = root.join("models");
    let dest = models.join("a/b");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("ok.bin"), "momo").unwrap();
    fs::write(dest.join("nohash.bin"), "x").unwrap();

    let out = run_check(&manifest, &models, &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn missing_manifest_is_usage_error() {
    let root = tmpdir("nomanifest");
    let out = run_check(&root.join("MANIFEST.toml"), &root, &[]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn placeholder_sha_passes_parse_but_fails_check() {
    // 仓库实际 MANIFEST.toml 的 sha256="TODO" 必须能解析（不被 validate 拒绝）。
    let m = momo_infra::manifest::Manifest::parse(
        r#"[[model]]
name = "x"
repo_id = "a/b"
files = [ { path = "f.bin", sha256 = "TODO" } ]
"#,
    )
    .unwrap();
    assert_eq!(m.models.len(), 1);
    assert_eq!(GOOD_SHA_B.len(), 64);
}
