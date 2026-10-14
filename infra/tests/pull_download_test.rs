//! `momoctl pull` 下载路径集成测试（wiremock 本地 HTTP 服）：
//! 新鲜下载 / `.part` 断点续传 / sha256 不匹配 / pull.lock 并发互斥 /
//! 已落位跳过 / `source = "url"` 与 file.url 直链分发。
//! HF_ENDPOINT 指向 mock server，不触网。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// sha256("momo-weights-v1")
const GOOD_SHA: &str = "e8a2f62e4cbebe2e13d23447ceff12b610fe3c31eb7fcebe0d36c5e95ae3c88f";
const CONTENT: &[u8] = b"momo-weights-v1";

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("momoctl-pull-dl-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn manifest_body(sha: &str) -> String {
    format!(
        r#"[[model]]
name = "m"
repo_id = "a/b"
revision = "main"
files = [ {{ path = "w.bin", sha256 = "{sha}", size = 15 }} ]
"#
    )
}

fn run_pull(
    manifest: &Path,
    models_dir: &Path,
    endpoint: &str,
    extra: &[&str],
) -> std::process::Output {
    let env_file = models_dir.parent().unwrap().join("empty.env");
    fs::write(&env_file, "").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_momoctl"));
    cmd.args(["pull", "--manifest"])
        .arg(manifest)
        .args(["--env-file"])
        .arg(&env_file)
        .args(extra)
        .env("MOMO_MODELS_DIR", models_dir)
        .env("HF_ENDPOINT", endpoint)
        // 隔离真实环境里的代理设置，保证请求直达 mock server
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("NO_PROXY");
    cmd.output().expect("spawn momoctl")
}

#[tokio::test]
async fn fresh_download_registers_models_lock() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a/b/resolve/main/w.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(CONTENT))
        .mount(&server)
        .await;

    let root = tmpdir("fresh");
    let manifest = root.join("MANIFEST.toml");
    fs::write(&manifest, manifest_body(GOOD_SHA)).unwrap();
    let models = root.join("models");

    let out = run_pull(&manifest, &models, &server.uri(), &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(models.join("a/b/w.bin")).unwrap(), CONTENT);
    assert!(
        !models.join("a/b/w.bin.part").exists(),
        ".part renamed away"
    );
    assert!(!models.join("pull.lock").exists(), "lock released");

    let lock = fs::read_to_string(models.join("models.lock")).unwrap();
    assert!(lock.contains("a/b@main"), "lock: {lock}");
    assert!(lock.contains(GOOD_SHA), "lock: {lock}");
}

#[tokio::test]
async fn resumes_from_part_with_range_header() {
    let server = MockServer::start().await;
    // 只接受 Range 续传请求；无 Range 的 GET 会 404 → fail。
    Mock::given(method("GET"))
        .and(path("/a/b/resolve/main/w.bin"))
        .and(header("Range", "bytes=5-"))
        .respond_with(ResponseTemplate::new(206).set_body_bytes(&CONTENT[5..]))
        .mount(&server)
        .await;

    let root = tmpdir("resume");
    let manifest = root.join("MANIFEST.toml");
    fs::write(&manifest, manifest_body(GOOD_SHA)).unwrap();
    let models = root.join("models");
    let dest = models.join("a/b");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("w.bin.part"), &CONTENT[..5]).unwrap();

    let out = run_pull(&manifest, &models, &server.uri(), &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(dest.join("w.bin")).unwrap(), CONTENT);
}

#[tokio::test]
async fn sha256_mismatch_exits_5_and_keeps_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a/b/resolve/main/w.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"corrupted-bytes!"))
        .mount(&server)
        .await;

    let root = tmpdir("badsha");
    let manifest = root.join("MANIFEST.toml");
    fs::write(&manifest, manifest_body(GOOD_SHA)).unwrap();
    let models = root.join("models");

    let out = run_pull(&manifest, &models, &server.uri(), &[]);
    assert_eq!(
        out.status.code(),
        Some(5),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!models.join("a/b/w.bin").exists());
    assert!(!models.join("a/b/w.bin.part").exists(), "bad .part removed");
    assert!(
        !models.join("models.lock").exists(),
        "unverified entry not registered"
    );
}

#[tokio::test]
async fn pull_lock_contention_fails() {
    let server = MockServer::start().await;
    let root = tmpdir("lock");
    let manifest = root.join("MANIFEST.toml");
    fs::write(&manifest, manifest_body(GOOD_SHA)).unwrap();
    let models = root.join("models");
    fs::create_dir_all(&models).unwrap();
    fs::write(models.join("pull.lock"), "pid 1\n").unwrap();

    let out = run_pull(&manifest, &models, &server.uri(), &[]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("pull.lock"), "stderr: {stderr}");
    // 预置的锁文件不被误删
    assert!(models.join("pull.lock").exists());
}

#[tokio::test]
async fn already_present_file_skips_download() {
    let server = MockServer::start().await; // 不 mount：任何请求都会 404
    let root = tmpdir("present");
    let manifest = root.join("MANIFEST.toml");
    fs::write(&manifest, manifest_body(GOOD_SHA)).unwrap();
    let models = root.join("models");
    let dest = models.join("a/b");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("w.bin"), CONTENT).unwrap();

    let out = run_pull(&manifest, &models, &server.uri(), &["--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], 1);
    assert_eq!(v["command"], "pull");
    assert_eq!(v["checks"][0]["status"], "ok");
    assert!(v["checks"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("already present"));
    assert!(models.join("models.lock").exists());
}

/// `source = "url"` 条目：逐文件直链下载，不拼 HF_ENDPOINT resolve URL
/// （HF_ENDPOINT 指到不通地址也应成功）。silero-vad 类 GitHub-only 上游用例。
#[tokio::test]
async fn url_source_downloads_direct_and_ignores_hf_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/raw/silero_vad.onnx"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(CONTENT))
        .mount(&server)
        .await;

    let root = tmpdir("urlsrc");
    let manifest = root.join("MANIFEST.toml");
    fs::write(
        &manifest,
        format!(
            r#"[[model]]
name = "silero-vad"
repo_id = "snakers4/silero-vad"
revision = "master"
source = "url"
dest = "snakers4/silero-vad"
files = [ {{ path = "silero_vad.onnx", url = "{}/raw/silero_vad.onnx", sha256 = "{GOOD_SHA}", size = 15 }} ]
"#,
            server.uri()
        ),
    )
    .unwrap();
    let models = root.join("models");

    // 故意把 HF_ENDPOINT 指到不可达地址：直链源不应触碰它。
    let out = run_pull(&manifest, &models, "http://127.0.0.1:1", &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(models.join("snakers4/silero-vad/silero_vad.onnx")).unwrap(),
        CONTENT
    );
    let lock = fs::read_to_string(models.join("models.lock")).unwrap();
    assert!(lock.contains("snakers4/silero-vad@master"), "lock: {lock}");
}

/// `hf` 条目（缺省 source）下 file.url 单文件覆盖：绕过 resolve 拼接走直链。
#[tokio::test]
async fn hf_entry_per_file_url_overrides_resolve_path() {
    let server = MockServer::start().await;
    // 只挂直链路径；若仍拼 /a/b/resolve/main/w.bin 会命中 404 → fail。
    Mock::given(method("GET"))
        .and(path("/direct/w.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(CONTENT))
        .mount(&server)
        .await;

    let root = tmpdir("fileurl");
    let manifest = root.join("MANIFEST.toml");
    fs::write(
        &manifest,
        format!(
            r#"[[model]]
name = "m"
repo_id = "a/b"
revision = "main"
files = [ {{ path = "w.bin", url = "{}/direct/w.bin", sha256 = "{GOOD_SHA}", size = 15 }} ]
"#,
            server.uri()
        ),
    )
    .unwrap();
    let models = root.join("models");

    let out = run_pull(&manifest, &models, &server.uri(), &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(models.join("a/b/w.bin")).unwrap(), CONTENT);
}
