//! `momoctl pull [--manifest PATH] [--only NAME] [--check]`：模型下载 + sha256 校验。
//! `.part` + Range 续传 → size+sha256 复算 → rename → `models.lock`；`pull.lock`
//! 原子占位防并发。`--check` 只校验不下载（占位 "TODO"/缺失按 fail）。
//! 下载源按条目 `source` 分发：`hf`（默认）拼 `<HF_ENDPOINT>/<repo>/resolve/
//! <rev>/<path>`，`url` 逐文件取 `files[].url` 直链（GitHub raw 等非 HF 源）。
//! 退出码：2 usage/config、4 网络、5 校验（02-repos/momo-infra.md）。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::RANGE;

use crate::api::dto::{Check, CheckStatus, PullReport, Summary};
use crate::cli::PullArgs;
use crate::env_cfg::EnvCfg;
use crate::manifest::{
    is_placeholder_sha, sha256_file, FileEntry, LockEntry, LockFile, Manifest, ModelEntry,
    ModelsLock,
};
use crate::output::{AppError, ExitCode};

/// 下载连接超时；总时长不设限（权重文件 GB 级）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// HF 镜像默认（config.md §2.5：laptop huggingface.co 不通）。
const DEFAULT_HF_ENDPOINT: &str = "https://hf-mirror.com";

pub fn run(args: &PullArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    if args.check {
        return check(args, ctx);
    }
    pull(args, ctx)
}

fn pull(args: &PullArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let (report, code) = pull_report(args.manifest.clone(), args.only.clone())?;
    render(&report, ctx)?;
    Ok(code)
}

/// `POST /api/models/pull` 与 CLI `pull` 共用 core：`.part` 续传 + 校验 +
/// `models.lock` 登记，返回 `PullReport` + CLI 退出码口径。
/// GB 级权重长时间阻塞且带 stderr 进度行，web 侧须 `spawn_blocking` 调用；
/// 逐文件进度流待 jobs runner（#54）挂接，当前为直跑等结果。
pub fn pull_report(
    manifest: Option<PathBuf>,
    only: Vec<String>,
) -> Result<(PullReport, i32), AppError> {
    let manifest_path = locate_manifest(manifest.as_deref())?;
    let manifest = Manifest::load(&manifest_path)?;
    let models_dir = EnvCfg.models_dir();
    let selected = select(&manifest, &only)?;

    // 并发保护：pull.lock 占位失败即拒跑（05-ops/model-fetch.md）。
    let _guard = PullGuard::acquire(&models_dir)?;
    let endpoint = hf_endpoint();
    let client = Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(io::Error::other)?;

    let lock_path = models_dir.join("models.lock");
    let mut lock = ModelsLock::load(&lock_path)?;
    let mut checks = Vec::new();
    let mut saw_net = false;
    let mut saw_verify = false;

    for m in selected {
        if m.files.is_empty() {
            checks.push(Check::warn(
                format!("{}/", m.name),
                "entry declares no files",
                "see MANIFEST.toml entry schema in momo-models.md",
            ));
            continue;
        }
        let mut lock_files = Vec::new();
        let mut entry_failed = false;
        for f in &m.files {
            let r = fetch_file(&client, &endpoint, m, f, &models_dir);
            if let Some(lf) = r.lock_file {
                lock_files.push(lf);
            }
            match r.fail {
                Some(FailKind::Net) => {
                    saw_net = true;
                    entry_failed = true;
                }
                Some(FailKind::Verify) => {
                    saw_verify = true;
                    entry_failed = true;
                }
                None => {}
            }
            checks.push(r.check);
        }
        // 整体校验通过才登记 models.lock（不把半成品注册为 ready）。
        if !entry_failed {
            lock.upsert(LockEntry {
                repo: format!("{}@{}", m.repo_id, m.revision),
                name: m.name.clone(),
                files: lock_files,
            });
            lock.save(&lock_path)?;
        }
    }

    let code = if saw_verify {
        ExitCode::Verify
    } else if saw_net {
        ExitCode::Network
    } else {
        ExitCode::Ok
    };
    let report = PullReport {
        schema: 1,
        command: "pull".into(),
        manifest: manifest_path.display().to_string(),
        models_dir: models_dir.display().to_string(),
        endpoint: Some(endpoint),
        summary: Summary::from_checks(&checks),
        checks,
    };
    Ok((report, code as i32))
}

/// 单文件取回结果：check 上报 + models.lock 条目 + 失败类别（决定退出码）。
struct FileFetch {
    check: Check,
    lock_file: Option<LockFile>,
    fail: Option<FailKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailKind {
    /// 网络/HTTP 层失败 → 退出码 4。
    Net,
    /// 下载完成但 size/sha256 不符 → 退出码 5。
    Verify,
}

impl FileFetch {
    fn ok(check: Check, lock_file: LockFile) -> Self {
        Self {
            check,
            lock_file: Some(lock_file),
            fail: None,
        }
    }

    fn fail(check: Check, kind: FailKind) -> Self {
        Self {
            check,
            lock_file: None,
            fail: Some(kind),
        }
    }
}

/// 下载单个文件到 `<dest>.part`（断点续传），校验后 rename 到 `<dest>`。
fn fetch_file(
    client: &Client,
    endpoint: &str,
    m: &ModelEntry,
    f: &FileEntry,
    models_dir: &Path,
) -> FileFetch {
    let id = format!("{}/{}", m.name, f.path);
    let dest = m.dest_dir(models_dir).join(&f.path);
    let url = file_url(endpoint, m, f);

    // 幂等：已落位且校验通过 → 跳过下载。
    if dest.is_file() {
        match verify_local(&dest, f) {
            Ok(sha) => {
                let size = fs::metadata(&dest).map(|md| md.len()).unwrap_or(0);
                return FileFetch::ok(
                    Check::ok(id, format!("already present, sha256 {}", &sha[..12])),
                    LockFile {
                        path: f.path.clone(),
                        size,
                        sha256: sha,
                    },
                );
            }
            Err(why) => {
                eprintln!("pull: {id}: existing file invalid ({why}); re-downloading");
            }
        }
    }

    let part = part_path(&dest);
    if let Some(dir) = part.parent() {
        if let Err(e) = fs::create_dir_all(dir) {
            return FileFetch::fail(
                Check::fail(
                    id,
                    format!("mkdir {}: {e}", dir.display()),
                    "check directory permissions",
                ),
                FailKind::Net,
            );
        }
    }
    let resume = fs::metadata(&part).map(|md| md.len()).unwrap_or(0);
    eprintln!(
        "pull: {id}: GET {url}{}",
        if resume > 0 {
            format!(" (resume from {resume} bytes)")
        } else {
            String::new()
        }
    );

    match download(client, &url, &part, resume) {
        Ok(done) => eprintln!("pull: {id}: {done} bytes in .part"),
        Err(detail) => {
            let hint = if f.url.is_some() {
                "check the file url is reachable (direct-url source ignores HF_ENDPOINT); .part kept for resume"
            } else {
                "if network is unreachable set HF_ENDPOINT to a mirror and retry; .part kept for resume"
            };
            return FileFetch::fail(
                Check::fail(id, format!("download failed: {detail}"), hint),
                FailKind::Net,
            );
        }
    }

    // 校验 size + sha256；失败删除 .part（坏字节不能续传复用，也不落位）。
    let sha = match verify_local(&part, f) {
        Ok(sha) => sha,
        Err(why) => {
            let _ = fs::remove_file(&part);
            return FileFetch::fail(
                Check::fail(
                    id,
                    format!("verify failed: {why}"),
                    "delete leftovers and re-run pull",
                ),
                FailKind::Verify,
            );
        }
    };
    let size = fs::metadata(&part).map(|md| md.len()).unwrap_or(0);

    if dest.exists() {
        let _ = fs::remove_file(&dest);
    }
    if let Err(e) = fs::rename(&part, &dest) {
        return FileFetch::fail(
            Check::fail(
                id,
                format!("rename to {}: {e}", dest.display()),
                "check directory permissions",
            ),
            FailKind::Net,
        );
    }

    let check = match &f.sha256 {
        Some(h) if is_placeholder_sha(h) => Check::warn(
            id,
            format!("downloaded {size} bytes; sha256 placeholder, recorded actual"),
            "backfill the actual sha256 in MANIFEST.toml to enable verification",
        ),
        _ => Check::ok(
            id,
            format!("downloaded {size} bytes, sha256 {}", &sha[..12]),
        ),
    };
    FileFetch::ok(
        check,
        LockFile {
            path: f.path.clone(),
            size,
            sha256: sha,
        },
    )
}

/// 单文件下载 URL：`files[].url` 存在 → 直链原样 GET（`source = "url"` 条目
/// 或 hf 条目的单文件覆盖，均不经 `HF_ENDPOINT`）；否则按 HF resolve 拼
/// `<endpoint>/<repo_id>/resolve/<revision>/<path>`。
fn file_url(endpoint: &str, m: &ModelEntry, f: &FileEntry) -> String {
    match &f.url {
        Some(u) => u.clone(),
        None => format!("{endpoint}/{}/resolve/{}/{}", m.repo_id, m.revision, f.path),
    }
}

/// `.part` 路径：`<dest>` 同级追加 `.part` 后缀（`w.bin` → `w.bin.part`）。
fn part_path(dest: &Path) -> PathBuf {
    dest.with_file_name(format!(
        "{}.part",
        dest.file_name().unwrap_or_default().to_string_lossy()
    ))
}

/// 本地下载态校验：size 声明 + sha256 声明（占位跳过内容比对）。
/// 返回实际 sha256；Err 为可读原因。
fn verify_local(path: &Path, f: &FileEntry) -> Result<String, String> {
    if let Some(expected) = f.size {
        let actual = fs::metadata(path).map_err(|e| format!("stat: {e}"))?.len();
        if actual != expected {
            return Err(format!("size {actual} != expected {expected}"));
        }
    }
    let actual_sha = sha256_file(path).map_err(|e| format!("sha256 read: {e}"))?;
    match &f.sha256 {
        Some(expected) if is_placeholder_sha(expected) => Ok(actual_sha),
        Some(expected) if actual_sha.eq_ignore_ascii_case(expected) => Ok(actual_sha),
        Some(expected) => Err(format!(
            "sha256 {} != expected {}",
            &actual_sha[..12],
            &expected[..12.min(expected.len())]
        )),
        None => Ok(actual_sha),
    }
}

/// GET 下载：`.part` 已有 `resume` 字节时带 `Range: bytes={resume}-`。
/// 200 → 服务端不支持续传，截断重写；206 → 追加；416 → 已完整。
fn download(client: &Client, url: &str, part: &Path, resume: u64) -> Result<u64, String> {
    let mut req = client.get(url);
    if resume > 0 {
        req = req.header(RANGE, format!("bytes={resume}-"));
    }
    let resp = req.send().map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.as_u16() == 416 {
        return Ok(resume);
    }
    let append = match status.as_u16() {
        s if (200..300).contains(&s) => status.as_u16() == 206,
        _ => return Err(format!("HTTP {}", status.as_u16())),
    };
    let mut opts = OpenOptions::new();
    opts.create(true).write(true);
    if append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    let mut file = opts.open(part).map_err(|e| format!("open .part: {e}"))?;
    let mut resp = resp;
    io::copy(&mut resp, &mut file).map_err(|e| format!("write .part: {e}"))?;
    fs::metadata(part)
        .map_err(|e| e.to_string())
        .map(|md| md.len())
}

/// 并发 pull 互斥：`pull.lock` 用 `create_new` 原子占位；Drop 删除。
/// 进程崩溃残留时提示手动删除（不自动判 stale，避免误删活进程锁）。
struct PullGuard {
    path: PathBuf,
}

impl PullGuard {
    fn acquire(models_dir: &Path) -> Result<Self, AppError> {
        fs::create_dir_all(models_dir)?;
        let path = models_dir.join("pull.lock");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                let _ = writeln!(f, "pid {}", std::process::id());
                Ok(Self { path })
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                Err(AppError::Io(io::Error::new(
                    e.kind(),
                    format!(
                        "another pull holds {}; remove it manually if stale",
                        path.display()
                    ),
                )))
            }
            Err(e) => Err(AppError::Io(e)),
        }
    }
}

impl Drop for PullGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// `HF_ENDPOINT`（config.md §2.5 默认 hf-mirror）；末尾斜杠归一。
fn hf_endpoint() -> String {
    std::env::var("HF_ENDPOINT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HF_ENDPOINT.into())
        .trim_end_matches('/')
        .to_string()
}

fn select<'m>(manifest: &'m Manifest, only: &[String]) -> Result<Vec<&'m ModelEntry>, AppError> {
    let selected: Vec<_> = manifest
        .models
        .iter()
        .filter(|m| only.is_empty() || only.iter().any(|o| o == &m.name))
        .collect();
    if selected.is_empty() {
        return Err(AppError::Config(format!(
            "no manifest entry matches --only {:?}",
            only
        )));
    }
    Ok(selected)
}

fn check(args: &PullArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let manifest_path = locate_manifest(args.manifest.as_deref())?;
    let manifest = Manifest::load(&manifest_path)?;
    let models_dir = EnvCfg.models_dir();
    let selected = select(&manifest, &args.only)?;

    let mut checks = Vec::new();
    for m in selected {
        if m.files.is_empty() {
            checks.push(Check::warn(
                format!("{}/", m.name),
                "entry declares no files",
                "see MANIFEST.toml entry schema in momo-models.md",
            ));
        }
        for f in &m.files {
            checks.push(check_file(
                m.dest_dir(&models_dir).join(&f.path),
                &m.name,
                f,
            ));
        }
    }

    let report = PullReport {
        schema: 1,
        command: "pull --check".into(),
        manifest: manifest_path.display().to_string(),
        models_dir: models_dir.display().to_string(),
        endpoint: None,
        summary: Summary::from_checks(&checks),
        checks,
    };
    render(&report, ctx)?;

    Ok(if report.summary.fail > 0 {
        ExitCode::Verify as i32
    } else {
        ExitCode::Ok as i32
    })
}

/// 单个文件校验：缺失 / size 不符 / sha256 占位 / sha256 不匹配 → fail。
fn check_file(path: PathBuf, model: &str, f: &FileEntry) -> Check {
    let id = format!("{model}/{}", f.path);
    if !path.is_file() {
        return Check::fail(
            id,
            format!("missing: {}", path.display()),
            format!("run `momoctl pull --only {model}` then re-run --check"),
        );
    }
    if let Some(expected) = f.size {
        match fs::metadata(&path) {
            Ok(md) if md.len() == expected => {}
            Ok(md) => {
                return Check::fail(
                    id,
                    format!("size {} != expected {}", md.len(), expected),
                    "file may be incomplete; pull again",
                )
            }
            Err(e) => {
                return Check::fail(
                    id,
                    format!("stat failed: {e}"),
                    "check file permissions and path",
                )
            }
        }
    }
    match &f.sha256 {
        Some(h) if is_placeholder_sha(h) => Check::fail(
            id,
            "sha256 is placeholder (TODO); cannot verify",
            "MANIFEST sha256 to be backfilled from actual files once pull lands",
        ),
        Some(expected) => match sha256_file(&path) {
            Ok(actual) if actual.eq_ignore_ascii_case(expected) => {
                Check::ok(id, format!("sha256 ok ({})", &actual[..12]))
            }
            Ok(actual) => Check::fail(
                id,
                format!(
                    "sha256 {} != expected {}",
                    &actual[..12],
                    &expected[..12.min(expected.len())]
                ),
                format!("delete {} and pull again", path.display()),
            ),
            Err(e) => Check::fail(
                id,
                format!("sha256 read failed: {e}"),
                "check file readability",
            ),
        },
        None => Check::ok(id, "present (no sha256 declared)"),
    }
}

/// MANIFEST.toml 定位：显式路径优先；否则按常见仓布局探测
/// （候选清单见 manifest::default_manifest_path）。
fn locate_manifest(manifest: Option<&Path>) -> Result<PathBuf, AppError> {
    if let Some(p) = manifest {
        if !p.is_file() {
            return Err(AppError::Config(format!(
                "manifest not found: {}",
                p.display()
            )));
        }
        return Ok(p.to_path_buf());
    }
    crate::manifest::default_manifest_path().ok_or_else(|| {
        AppError::Config(format!(
            "MANIFEST.toml not found (tried {}); use --manifest PATH",
            crate::manifest::manifest_candidates().join(", ")
        ))
    })
}

fn render(report: &PullReport, ctx: &super::Ctx) -> Result<(), AppError> {
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        return Ok(());
    }
    render_human(report, &mut std::io::stdout().lock())?;
    Ok(())
}

fn render_human(report: &PullReport, w: &mut impl Write) -> std::io::Result<()> {
    writeln!(w, "== {} ==", report.command)?;
    writeln!(w, "manifest:   {}", report.manifest)?;
    writeln!(w, "models dir: {}", report.models_dir)?;
    if let Some(ep) = &report.endpoint {
        writeln!(w, "endpoint:   {ep}")?;
    }
    let id_width = report.checks.iter().map(|c| c.id.len()).max().unwrap_or(0);
    for c in &report.checks {
        let label = match c.status {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
        };
        let pad = " ".repeat(4usize.saturating_sub(label.len()));
        writeln!(w, "  {pad}{label}  {:id_width$}  {}", c.id, c.detail)?;
        if let Some(hint) = &c.hint {
            writeln!(w, "  {pad}      {:id_width$}  hint: {hint}", "")?;
        }
    }
    writeln!(
        w,
        "summary: {} ok, {} warn, {} fail",
        report.summary.ok, report.summary.warn, report.summary.fail
    )
}
