//! `momoctl gc [--dry-run] [--keep N] [--yes] [--json]`：模型 GC。
//!
//! 分类口径（05-ops/model-fetch.md 不变量）：
//! - `tts/ft-<engine>-<yyyymmdd>/` 按 engine 分组，每 engine 保留最新 keep 代
//!   （默认 2，即 active+rollback 的 N-1 语义）；超出代若未被 models.lock 或
//!   .env 引用列为回收候选；
//! - 基座模型、未登记目录与 `.part` 残留仅当未被 models.lock 引用、也未被
//!   `MOMO_TTS_MODEL_DIR`/`MOMO_TTS_VOICE_REF` 指向时才可回收；
//! - `pull.lock` 存在视为 pull 进行中，全部只报告（--yes 也不删）；
//! - models.lock 指向缺失目录的 stale 条目报告并在 --yes 时摘除。
//!
//! 退出码：0 成功（含 dry-run）；1 删除/IO 失败；2 用法/配置；5 models.lock 损坏。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::api::dto::{Check, CheckStatus, GcReport, Summary};
use crate::cli::GcArgs;
use crate::env_cfg::{self, EnvCfg};
use crate::manifest::{Manifest, ModelsLock};
use crate::output::{AppError, ExitCode};

/// 单个扫描条目的处置结论。
#[derive(Debug)]
enum Decision {
    /// 保留（引用中 / 保留代内 / 元数据）。
    Keep(String),
    /// 回收候选（附原因）。
    Candidate(String),
}

#[derive(Debug)]
struct Item {
    /// 相对 models_dir 的展示路径；stale lock 条目用 `lock:<name>`。
    rel: String,
    /// 磁盘路径；stale lock 条目无路径。
    abs: Option<PathBuf>,
    bytes: u64,
    decision: Decision,
    /// stale lock 条目的 models.lock name（仅 StaleLock 类）。
    lock_name: Option<String>,
}

impl Item {
    fn keep(rel: String, abs: PathBuf, reason: impl Into<String>) -> Self {
        Self {
            rel,
            abs: Some(abs),
            bytes: 0,
            decision: Decision::Keep(reason.into()),
            lock_name: None,
        }
    }

    fn candidate(rel: String, abs: PathBuf, bytes: u64, reason: impl Into<String>) -> Self {
        Self {
            rel,
            abs: Some(abs),
            bytes,
            decision: Decision::Candidate(reason.into()),
            lock_name: None,
        }
    }

    fn stale_lock(name: String, reason: impl Into<String>) -> Self {
        Self {
            rel: format!("lock:{name}"),
            abs: None,
            bytes: 0,
            decision: Decision::Candidate(reason.into()),
            lock_name: Some(name),
        }
    }
}

pub fn run(args: &GcArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let models_dir = EnvCfg.models_dir();
    let lock_path = models_dir.join("models.lock");
    let mut lock = ModelsLock::load(&lock_path)?;
    let manifest = crate::manifest::default_manifest_path().and_then(|p| Manifest::load(&p).ok());
    let pull_active = models_dir.join("pull.lock").exists();

    let mut items = scan(
        &models_dir,
        args.keep,
        &lock,
        manifest.as_ref(),
        &env_refs(),
    );
    collect_stale_locks(&mut items, &lock, manifest.as_ref(), &models_dir);

    // pull 进行中：全部只报告，.part 与未登记半成品都不能动。
    let execute = args.yes && !args.dry_run && !pull_active;
    let mut deleted_bytes = 0u64;
    let mut checks = Vec::new();

    if pull_active && args.yes && !args.dry_run {
        checks.push(Check::warn(
            "pull.lock",
            "pull in progress; deletion skipped for all candidates",
            "wait for momoctl pull to finish or remove a stale pull.lock manually",
        ));
    }

    // 稳定输出顺序：按展示路径排序。
    items.sort_by(|a, b| a.rel.cmp(&b.rel));
    let mut candidates = 0usize;
    let mut candidate_bytes = 0u64;

    for item in &items {
        match &item.decision {
            Decision::Keep(reason) => {
                checks.push(Check::ok(item.rel.clone(), reason.clone()));
            }
            Decision::Candidate(reason) => {
                candidates += 1;
                candidate_bytes += item.bytes;
                match (&item.abs, execute) {
                    (Some(abs), true) => match remove_path(abs) {
                        Ok(()) => {
                            deleted_bytes += item.bytes;
                            checks.push(Check::ok(
                                item.rel.clone(),
                                format!("deleted ({reason}); {} bytes reclaimed", item.bytes),
                            ));
                        }
                        Err(e) => checks.push(Check::fail(
                            item.rel.clone(),
                            format!("delete failed: {e}"),
                            "check permissions, then remove the path manually",
                        )),
                    },
                    (None, true) => {
                        // stale lock 条目：无磁盘对象，从 models.lock 摘除。
                        checks.push(Check::ok(
                            item.rel.clone(),
                            format!("{reason}; entry removed from models.lock"),
                        ));
                    }
                    _ => checks.push(Check::warn(
                        item.rel.clone(),
                        format!("{reason}; {} bytes reclaimable", item.bytes),
                        "re-run `momoctl gc --yes` to delete",
                    )),
                }
            }
        }
    }

    if execute {
        // 摘除 stale lock 条目并落盘（联动 models.lock）。
        let stale: Vec<&str> = items
            .iter()
            .filter_map(|i| i.lock_name.as_deref())
            .collect();
        if !stale.is_empty() {
            lock.retain(|e| !stale.contains(&e.name.as_str()));
            lock.save(&lock_path)?;
        }
    }

    let report = GcReport {
        schema: 1,
        command: "gc".into(),
        models_dir: models_dir.display().to_string(),
        mode: if execute { "delete" } else { "dry-run" }.into(),
        keep: args.keep,
        candidates,
        candidate_bytes,
        deleted_bytes,
        summary: Summary::from_checks(&checks),
        checks,
    };
    render(&report, ctx)?;

    Ok(if report.summary.fail > 0 {
        ExitCode::Runtime as i32
    } else {
        ExitCode::Ok as i32
    })
}

/// `.env` 引用集合：`MOMO_TTS_MODEL_DIR` / `MOMO_TTS_VOICE_REF`（active 指针）。
/// 相对路径按 cwd 绝对化后再比较。
fn env_refs() -> Vec<PathBuf> {
    ["MOMO_TTS_MODEL_DIR", "MOMO_TTS_VOICE_REF"]
        .iter()
        .filter_map(|k| EnvCfg.get(k))
        .map(|v| absolutize(&env_cfg::expand_tilde(&v)))
        .collect()
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}

/// models.lock 各条目对应的落盘目录：有 MANIFEST 时按 name 取 `dest`，
/// 否则回退 `repo` 的 repo_id（默认镜像布局）。
fn lock_dirs(
    lock: &ModelsLock,
    manifest: Option<&Manifest>,
    models_dir: &Path,
) -> Vec<(String, PathBuf)> {
    lock.entries
        .iter()
        .map(|e| {
            let dest = manifest
                .and_then(|m| m.models.iter().find(|m| m.name == e.name))
                .map(|m| m.dest())
                .unwrap_or_else(|| PathBuf::from(e.repo_id()));
            (e.name.clone(), models_dir.join(dest))
        })
        .collect()
}

/// models.lock 内登记的文件绝对路径集合（.part 引用来判定）。
fn lock_files(lock: &ModelsLock, manifest: Option<&Manifest>, models_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (name, dir) in lock_dirs(lock, manifest, models_dir) {
        if let Some(e) = lock.entries.iter().find(|e| e.name == name) {
            out.extend(e.files.iter().map(|f| dir.join(&f.path)));
        }
    }
    out
}

/// 路径是否被 models.lock 引用：`P == 条目目录`、`P` 是条目目录的祖先
/// （顶层 owner 目录覆盖其下 repo），或 `P` 在条目目录之内。
fn lock_referenced(path: &Path, dirs: &[PathBuf], files: &[PathBuf]) -> bool {
    dirs.iter()
        .any(|d| path == d || d.starts_with(path) || path.starts_with(d))
        || files.iter().any(|f| path == f)
}

/// `.env` 引用：互为祖先即保护（voice ref 文件保护所在目录，model dir
/// 保护其自身与其下路径）。
fn env_referenced(path: &Path, refs: &[PathBuf]) -> bool {
    refs.iter()
        .any(|r| path == r || r.starts_with(path) || path.starts_with(r))
}

/// `ft-<engine>-<yyyymmdd>`：engine 可含 `-`，日期为末尾 8 位数字。
fn parse_ft(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("ft-")?;
    let (engine, date) = rest.rsplit_once('-')?;
    if engine.is_empty() || !is_yyyymmdd(date) {
        return None;
    }
    Some((engine.to_string(), date.to_string()))
}

fn is_yyyymmdd(s: &str) -> bool {
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let month: u32 = s[4..6].parse().unwrap_or(0);
    let day: u32 = s[6..8].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// models_dir 元数据文件：永不参与回收。
fn is_metadata_file(name: &str) -> bool {
    matches!(name, "models.lock" | "models.lock.tmp" | "pull.lock")
}

/// 递归收集 `dir` 下全部常规文件（path, size）；不跟随符号链接目录。
fn walk_files(dir: &Path, out: &mut Vec<(PathBuf, u64)>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        let path = ent.path();
        let Ok(md) = ent.metadata() else {
            continue;
        };
        if md.is_dir() {
            walk_files(&path, out);
        } else if md.is_file() {
            out.push((path, md.len()));
        }
    }
}

fn dir_size(files: &[(PathBuf, u64)], dir: &Path) -> u64 {
    files
        .iter()
        .filter(|(p, _)| p.starts_with(dir))
        .map(|(_, s)| *s)
        .sum()
}

/// 扫描分类。`files` 为 models_dir 下全部文件清单（一次遍历，多处复用）。
fn scan(
    models_dir: &Path,
    keep: u32,
    lock: &ModelsLock,
    manifest: Option<&Manifest>,
    refs: &[PathBuf],
) -> Vec<Item> {
    let ref_dirs: Vec<PathBuf> = lock_dirs(lock, manifest, models_dir)
        .into_iter()
        .map(|(_, d)| d)
        .collect();
    let ref_files = lock_files(lock, manifest, models_dir);

    let mut files = Vec::new();
    if models_dir.is_dir() {
        walk_files(models_dir, &mut files);
    }
    let mut items = Vec::new();
    let mut candidate_dirs: Vec<PathBuf> = Vec::new();

    // ---- 顶层条目 ----
    let mut ft_groups: std::collections::BTreeMap<String, Vec<(String, PathBuf)>> =
        std::collections::BTreeMap::new();
    if let Ok(rd) = fs::read_dir(models_dir) {
        for ent in rd.flatten() {
            let path = ent.path();
            let name = ent.file_name().to_string_lossy().to_string();
            if ent.file_type().is_ok_and(|t| t.is_dir()) {
                if name == "tts" {
                    scan_tts(
                        &path,
                        models_dir,
                        &files,
                        &ref_dirs,
                        &ref_files,
                        refs,
                        &mut ft_groups,
                        &mut items,
                        &mut candidate_dirs,
                    );
                } else {
                    items.push(classify_dir(
                        &path,
                        name,
                        &files,
                        &ref_dirs,
                        &ref_files,
                        refs,
                        &mut candidate_dirs,
                    ));
                }
            } else if !is_metadata_file(&name) {
                if let Some(item) = classify_file(
                    &path,
                    name,
                    ent.metadata().map(|m| m.len()).unwrap_or(0),
                    &ref_dirs,
                    &ref_files,
                    refs,
                ) {
                    items.push(item);
                }
            }
        }
    }

    // ---- ft 代际：每 engine 保留最新 keep 代 ----
    for (engine, mut gens) in ft_groups {
        // 日期倒序，同日按名称倒序（确定性）。
        gens.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        for (idx, (_date, path)) in gens.into_iter().enumerate() {
            let rel = rel_display(models_dir, &path);
            if (idx as u32) < keep {
                items.push(Item::keep(
                    rel,
                    path,
                    format!("ft-{engine}: within latest {keep} generation(s)"),
                ));
            } else if is_protected(&path, &ref_dirs, &ref_files, refs) {
                items.push(Item::keep(
                    rel,
                    path,
                    format!("ft-{engine}: beyond keep but referenced by models.lock/.env"),
                ));
            } else {
                let bytes = dir_size(&files, &path);
                candidate_dirs.push(path.clone());
                items.push(Item::candidate(
                    rel,
                    path,
                    bytes,
                    format!("ft-{engine}: generation beyond keep={keep}"),
                ));
            }
        }
    }

    // ---- .part 残留（递归；父目录已是候选则不重复上报）----
    for (path, size) in &files {
        if path.extension().is_none_or(|e| e != "part") {
            continue;
        }
        if candidate_dirs.iter().any(|d| path.starts_with(d)) {
            continue;
        }
        let rel = rel_display(models_dir, path);
        if env_referenced(path, refs) || ref_files.iter().any(|f| f == path) {
            items.push(Item::keep(
                rel,
                path.clone(),
                "referenced by models.lock/.env",
            ));
        } else {
            items.push(Item::candidate(
                rel,
                path.clone(),
                *size,
                "stale .part download residue",
            ));
        }
    }

    items
}

/// `tts/` 子树：`ft-*` 目录取代际分组；其余目录/文件走同一套
/// 未登记分类规则；tts 本身不参与回收（命名空间目录）。
#[allow(clippy::too_many_arguments)]
fn scan_tts(
    tts_dir: &Path,
    models_dir: &Path,
    files: &[(PathBuf, u64)],
    ref_dirs: &[PathBuf],
    ref_files: &[PathBuf],
    refs: &[PathBuf],
    groups: &mut std::collections::BTreeMap<String, Vec<(String, PathBuf)>>,
    items: &mut Vec<Item>,
    candidate_dirs: &mut Vec<PathBuf>,
) {
    let Ok(rd) = fs::read_dir(tts_dir) else {
        return;
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        let path = ent.path();
        let rel = rel_display(models_dir, &path);
        if ent.file_type().is_ok_and(|t| t.is_dir()) {
            match parse_ft(&name) {
                Some((engine, date)) => {
                    groups.entry(engine).or_default().push((date, path));
                }
                None => items.push(classify_dir(
                    &path,
                    rel,
                    files,
                    ref_dirs,
                    ref_files,
                    refs,
                    candidate_dirs,
                )),
            }
        } else if let Some(item) = classify_file(
            &path,
            rel,
            ent.metadata().map(|m| m.len()).unwrap_or(0),
            ref_dirs,
            ref_files,
            refs,
        ) {
            items.push(item);
        }
    }
}

/// 目录分类：被 models.lock/.env 引用 → 保留；否则未登记候选。
fn classify_dir(
    path: &Path,
    rel: String,
    files: &[(PathBuf, u64)],
    ref_dirs: &[PathBuf],
    ref_files: &[PathBuf],
    refs: &[PathBuf],
    candidate_dirs: &mut Vec<PathBuf>,
) -> Item {
    if is_protected(path, ref_dirs, ref_files, refs) {
        return Item::keep(rel, path.to_path_buf(), "referenced by models.lock/.env");
    }
    let bytes = dir_size(files, path);
    candidate_dirs.push(path.to_path_buf());
    Item::candidate(rel, path.to_path_buf(), bytes, "unregistered directory")
}

/// 散落文件分类：`.part` 跳过（统一走递归残留清单）；被引用 → 保留；
/// 否则未登记候选。返回 None 表示本处不处理。
fn classify_file(
    path: &Path,
    rel: String,
    bytes: u64,
    ref_dirs: &[PathBuf],
    ref_files: &[PathBuf],
    refs: &[PathBuf],
) -> Option<Item> {
    if path.extension().is_some_and(|e| e == "part") {
        return None; // .part 统一由递归清单处理
    }
    if is_protected(path, ref_dirs, ref_files, refs) {
        return Some(Item::keep(
            rel,
            path.to_path_buf(),
            "referenced by models.lock/.env",
        ));
    }
    Some(Item::candidate(
        rel,
        path.to_path_buf(),
        bytes,
        "unregistered file",
    ))
}

/// 综合引用判定（models.lock 目录/文件 + .env 引用）。
fn is_protected(
    path: &Path,
    ref_dirs: &[PathBuf],
    ref_files: &[PathBuf],
    refs: &[PathBuf],
) -> bool {
    lock_referenced(path, ref_dirs, ref_files) || env_referenced(path, refs)
}

/// models.lock 中指向已不存在目录的 stale 条目 → 候选（lock_name 供摘除）。
fn collect_stale_locks(
    items: &mut Vec<Item>,
    lock: &ModelsLock,
    manifest: Option<&Manifest>,
    models_dir: &Path,
) {
    for (name, dir) in lock_dirs(lock, manifest, models_dir) {
        if !dir.exists() {
            items.push(Item::stale_lock(
                name,
                "stale models.lock entry: dir missing",
            ));
        }
    }
}

fn rel_display(models_dir: &Path, path: &Path) -> String {
    path.strip_prefix(models_dir)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// 目录走 remove_dir_all；文件/符号链接走 remove_file。
fn remove_path(path: &Path) -> Result<(), std::io::Error> {
    let md = fs::symlink_metadata(path)?;
    if md.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn render(report: &GcReport, ctx: &super::Ctx) -> Result<(), AppError> {
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        return Ok(());
    }
    let mut w = std::io::stdout().lock();
    writeln!(w, "== gc ({}, keep={}) ==", report.mode, report.keep)?;
    writeln!(w, "models dir: {}", report.models_dir)?;
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
        "candidates: {} ({} bytes){}",
        report.candidates,
        report.candidate_bytes,
        if report.mode == "delete" {
            format!(", deleted {} bytes", report.deleted_bytes)
        } else {
            String::new()
        }
    )?;
    writeln!(
        w,
        "summary: {} ok, {} warn, {} fail",
        report.summary.ok, report.summary.warn, report.summary.fail
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{LockEntry, LockFile};

    fn lock_with(entries: &[(&str, &str)]) -> ModelsLock {
        ModelsLock {
            entries: entries
                .iter()
                .map(|(repo, name)| LockEntry {
                    repo: repo.to_string(),
                    name: name.to_string(),
                    files: vec![LockFile {
                        path: "w.bin".into(),
                        size: 1,
                        sha256: "0".repeat(64),
                    }],
                })
                .collect(),
        }
    }

    #[test]
    fn parse_ft_extracts_engine_and_date() {
        let (e, d) = parse_ft("ft-sovits-20261001").unwrap();
        assert_eq!((e.as_str(), d.as_str()), ("sovits", "20261001"));
        let (e, d) = parse_ft("ft-cosy-voice-20260101").unwrap();
        assert_eq!((e.as_str(), d.as_str()), ("cosy-voice", "20260101"));
        assert!(parse_ft("ft-x-2026").is_none());
        assert!(parse_ft("base-model").is_none());
        assert!(parse_ft("ft--20260101").is_none());
        assert!(parse_ft("ft-x-20261301").is_none());
    }

    #[test]
    fn lock_dirs_fall_back_to_repo_layout() {
        let lock = lock_with(&[("a/b@main", "b-model")]);
        let dirs = lock_dirs(&lock, None, Path::new("/m"));
        assert_eq!(dirs[0].1, Path::new("/m/a/b"));
    }

    #[test]
    fn lock_referenced_covers_ancestor_and_descendant() {
        let dirs = vec![PathBuf::from("/m/owner/repo")];
        let files = vec![PathBuf::from("/m/owner/repo/w.bin")];
        assert!(lock_referenced(Path::new("/m/owner"), &dirs, &files));
        assert!(lock_referenced(Path::new("/m/owner/repo"), &dirs, &files));
        assert!(lock_referenced(
            Path::new("/m/owner/repo/sub"),
            &dirs,
            &files
        ));
        assert!(lock_referenced(
            Path::new("/m/owner/repo/w.bin"),
            &dirs,
            &files
        ));
        assert!(!lock_referenced(Path::new("/m/other"), &dirs, &files));
        assert!(!lock_referenced(Path::new("/m/owner2"), &dirs, &files));
    }

    #[test]
    fn env_referenced_is_prefix_symmetric() {
        let refs = vec![PathBuf::from("/m/tts/ft-x-20260101/voice.wav")];
        assert!(env_referenced(Path::new("/m/tts/ft-x-20260101"), &refs));
        assert!(env_referenced(Path::new("/m/tts"), &refs));
        assert!(!env_referenced(Path::new("/m/tts/ft-x-20260102"), &refs));
    }

    /// 顶层散落文件识别：models.lock/pull.lock 跳过，普通文件成为候选。
    #[test]
    fn metadata_files_skipped() {
        assert!(is_metadata_file("models.lock"));
        assert!(is_metadata_file("pull.lock"));
        assert!(!is_metadata_file("stray.bin"));
    }
}
