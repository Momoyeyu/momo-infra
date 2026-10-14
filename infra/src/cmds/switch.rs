//! `momoctl switch --to <normal|degraded> [--restart]`。
//!
//! ① `.env.<to>` 不存在时按内置 `.env.example` + config.md §3 降级注释段生成；
//! ② 切换前校验：REQUIRED_KEYS 存在、`config.schema.toml` 取值域
//!   （enum/bool/int/url）、`ServeProfile::load_builtin` 可解析——任一失败
//!   exit 2 且 `.env` 不动；
//! ③ `.env` 符号链接重指 `.env.<to>`（已是普通文件先改名备份），写
//!   `active_profile`（serve 读），输出 env diff（敏感键打码）；
//! ④ `--restart` 时 serve down（旧档组件表）→ serve up（新档），
//!   bounded readiness 通过才算完成。
//!
//! 退出码：0 完成；2 校验/配置失败；1 链接/IO/serve 运行时失败。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::api::dto::{Check, CheckStatus, Summary, SwitchReport};
use crate::cli::{ServeCommon, SwitchArgs};
use crate::config_schema::{self, Schema};
use crate::env_cfg;
use crate::env_file;
use crate::output::{AppError, ExitCode};
use crate::serve::proc;
use crate::serve::profile::ServeProfile;

/// 生成 `.env.<to>` 的模板：infra 侧 .env.example 编译期内嵌。
const ENV_EXAMPLE: &str = include_str!("../../.env.example");

pub fn run(args: &SwitchArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let to = args.to.as_str();
    let env_link = env_file::resolve_env_path(ctx.env_file.as_deref());
    let env_dir = env_link
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let link_name = env_link
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ".env".into());
    let target_name = format!("{link_name}.{to}");
    let target_path = env_dir.join(&target_name);
    let schema = config_schema::find_root(&env_dir)
        .and_then(|root| Schema::load(&root.join("config.schema.toml")).ok());

    let mut checks = Vec::new();

    // ---- ① 目标档 env 生成 ----
    if target_path.is_file() {
        checks.push(Check::ok(
            "env.generate",
            format!("{} exists", target_path.display()),
        ));
    } else {
        let text = generate_env(to);
        fs::write(&target_path, text).map_err(|e| {
            AppError::Io(std::io::Error::new(
                e.kind(),
                format!("write {}: {e}", target_path.display()),
            ))
        })?;
        checks.push(Check::ok(
            "env.generate",
            format!("created {} from .env.example", target_path.display()),
        ));
    }

    // ---- ② 切换前校验（文件自身口径，不混当前进程 env）----
    let new_map = env_file::parse_env_file(&target_path)?;
    checks.extend(validate_target(&new_map, to, schema.as_ref()));

    // 旧 env 快照（diff 用）：跟随现有 symlink / 普通文件。
    let old_map =
        env_file::read_link_target(&env_link).and_then(|p| env_file::parse_env_file(&p).ok());
    let old_profile = old_map
        .as_ref()
        .and_then(|m| m.get("MOMO_PROFILE").cloned())
        .or_else(|| {
            fs::read_to_string(proc::active_profile_file())
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|p| p == "normal" || p == "degraded")
        .unwrap_or_else(|| "normal".into());

    let failed = checks.iter().any(|c| c.status == CheckStatus::Fail);
    let mut code = if failed {
        ExitCode::Usage as i32
    } else {
        ExitCode::Ok as i32
    };

    let mut diff = Vec::new();
    if !failed {
        // ---- ③ 重指符号链接 + active_profile + diff ----
        match repoint_link(&env_link, &target_name) {
            Ok(LinkOutcome::Already) => checks.push(Check::ok(
                "link",
                format!("{} already → {target_name}", env_link.display()),
            )),
            Ok(LinkOutcome::Linked) => checks.push(Check::ok(
                "link",
                format!("{} → {target_name}", env_link.display()),
            )),
            Ok(LinkOutcome::BackedUp(bak)) => checks.push(Check::ok(
                "link",
                format!(
                    "regular file moved to {}; {} → {target_name}",
                    bak.display(),
                    env_link.display()
                ),
            )),
            Err(e) => {
                checks.push(Check::fail(
                    "link",
                    format!("repoint {}: {e}", env_link.display()),
                    "check permissions; on Windows symlink needs developer mode or admin",
                ));
                code = ExitCode::Runtime as i32;
            }
        }
        if let Some(parent) = proc::active_profile_file().parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::write(proc::active_profile_file(), format!("{to}\n")) {
            Ok(()) => checks.push(Check::ok(
                "active_profile",
                format!("wrote {}", proc::active_profile_file().display()),
            )),
            Err(e) => {
                checks.push(Check::fail(
                    "active_profile",
                    format!("write failed: {e}"),
                    "check state dir permissions",
                ));
                code = ExitCode::Runtime as i32;
            }
        }

        let sensitive = env_file::sensitive_keys(schema.as_ref());
        let old = old_map.unwrap_or_default();
        diff = env_file::env_diff(&old, &new_map, &sensitive);

        // ---- ④ --restart：新 env 生效后 serve down → up ----
        if args.restart && code == 0 {
            reload_process_env(&env_link, old);
            let common = ServeCommon {
                profile: None,
                only: vec![],
                app_bin: None,
            };
            let old_doc = ServeProfile::load_builtin(&old_profile)?;
            let (down_checks, down_code) =
                crate::cmds::serve::down_inner(&old_doc, &common, &old_profile)?;
            let new_doc = ServeProfile::load_builtin(to)?;
            let (up_checks, up_code) = crate::cmds::serve::up_inner(&new_doc, &common, to)?;
            checks.extend(down_checks);
            checks.extend(up_checks);
            code = if up_code != 0 { up_code } else { down_code };
        }
    }

    let report = SwitchReport {
        schema: 1,
        command: format!("switch --to {to}"),
        to: to.into(),
        env_file: env_link.display().to_string(),
        target: target_path.display().to_string(),
        summary: Summary::from_checks(&checks),
        checks,
        env_diff: diff,
    };
    render(&report, ctx)?;
    Ok(code)
}

/// 目标档生成：模板逐行复制，`KEY=` 行命中覆盖表则替换值并保留行尾注释；
/// 模板缺失的覆盖键追加到文件尾。
fn generate_env(to: &str) -> String {
    let overrides = profile_overrides(to);
    let mut used: BTreeSet<&str> = BTreeSet::new();
    let mut out = String::new();
    for line in ENV_EXAMPLE.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let replaced = overrides.iter().find_map(|(k, v)| {
            if trimmed.starts_with(&format!("{k}=")) {
                used.insert(k);
                let comment = line
                    .find('#')
                    .map(|i| format!("  {}", &line[i..]))
                    .unwrap_or_default();
                Some(format!("{k}={v}{comment}"))
            } else {
                None
            }
        });
        out.push_str(replaced.as_deref().unwrap_or(line));
        out.push('\n');
    }
    let missing: Vec<_> = overrides
        .iter()
        .filter(|(k, _)| !used.contains(k))
        .collect();
    if !missing.is_empty() {
        out.push_str("\n# profile overrides\n");
        for (k, v) in missing {
            out.push_str(&format!("{k}={v}\n"));
        }
    }
    out
}

/// 目标档相对模板的覆盖行（config.md §3「降级档参考」段；degraded 默认
/// loopback 本地脑，DeepSeek API 为备选改 host 即可；API key 留空占位，
/// 本地脑无需配置）。
fn profile_overrides(to: &str) -> Vec<(&'static str, &'static str)> {
    match to {
        "degraded" => vec![
            ("MOMO_PROFILE", "degraded"),
            ("MOMO_LLM_ENDPOINT", "http://127.0.0.1:7820/v1"),
            ("MOMO_LLM_MODEL", "qwen3-8b-local"),
            ("MOMO_LLM_API_KEY", ""),
            ("MOMO_TTS_PROVIDER", "kokoro"),
        ],
        _ => vec![("MOMO_PROFILE", "normal")],
    }
}

enum LinkOutcome {
    /// 已指向目标。
    Already,
    /// 新建/重指完成。
    Linked,
    /// 原普通文件已备份到该路径后建链。
    BackedUp(PathBuf),
}

/// `.env` → `.env.<to>`：先在同目录建临时 symlink 再 rename 覆盖（同目录 rename
/// 原子替换，切换窗口不存在）。普通文件先改名 `<name>.bak`（已存在则报错）；
/// 目标用相对文件名，目录可整体搬迁。
fn repoint_link(link: &Path, target_name: &str) -> Result<LinkOutcome, AppError> {
    // 幂等短路：已指向目标则不动。
    if let Ok(md) = fs::symlink_metadata(link) {
        if md.file_type().is_symlink()
            && fs::read_link(link).ok().as_deref() == Some(Path::new(target_name))
        {
            return Ok(LinkOutcome::Already);
        }
    }

    // 临时 symlink：建在同目录，保证 rename 不跨文件系统。
    let tmp = link.with_file_name(format!(
        ".{}.momoctl-{}.tmp",
        link.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| ".env".into()),
        std::process::id()
    ));
    make_symlink(target_name, &tmp).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;

    let result = finish_repoint(link, &tmp);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// 临时链已就位后的收尾：旧普通文件先备份，再原子 rename 覆盖 `.env`。
fn finish_repoint(link: &Path, tmp: &Path) -> Result<LinkOutcome, AppError> {
    match fs::symlink_metadata(link) {
        Ok(md) if md.file_type().is_symlink() => {
            fs::rename(tmp, link)?;
            Ok(LinkOutcome::Linked)
        }
        Ok(md) => {
            if md.is_dir() {
                return Err(AppError::Config(format!(
                    "{} is a directory; expected env file or symlink",
                    link.display()
                )));
            }
            let bak = link.with_file_name(format!(
                "{}.bak",
                link.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| ".env".into())
            ));
            if bak.exists() {
                return Err(AppError::Config(format!(
                    "{} exists; refusing to overwrite backup — resolve manually",
                    bak.display()
                )));
            }
            fs::rename(link, &bak)?;
            if let Err(e) = fs::rename(tmp, link) {
                // 回滚：恢复普通文件，不留半个 .env 缺口。
                let _ = fs::rename(&bak, link);
                return Err(e.into());
            }
            Ok(LinkOutcome::BackedUp(bak))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::rename(tmp, link)?;
            Ok(LinkOutcome::Linked)
        }
        Err(e) => Err(AppError::Io(e)),
    }
}

#[cfg(unix)]
fn make_symlink(target: &str, link: &Path) -> Result<(), AppError> {
    std::os::unix::fs::symlink(target, link).map_err(AppError::Io)
}

#[cfg(windows)]
fn make_symlink(target: &str, link: &Path) -> Result<(), AppError> {
    std::os::windows::fs::symlink_file(target, link).map_err(AppError::Io)
}

/// 切换前校验：REQUIRED_KEYS、取值域、MOMO_PROFILE 一致、ServeProfile 可解析、
/// DeepSeek 端点带 key。失败项为 fail check（调用方据此 exit 2 且不动 .env）。
fn validate_target(
    map: &BTreeMap<String, String>,
    to: &str,
    schema: Option<&Schema>,
) -> Vec<Check> {
    let mut checks = Vec::new();

    for key in env_cfg::REQUIRED_KEYS {
        match map.get(*key).filter(|v| !v.trim().is_empty()) {
            Some(_) => checks.push(Check::ok(format!("env.{key}"), "set")),
            None => checks.push(Check::fail(
                format!("env.{key}"),
                "required key missing in target env file",
                format!("set {key} in the target profile file"),
            )),
        }
    }

    let declared = map
        .get("MOMO_PROFILE")
        .map(String::as_str)
        .unwrap_or("normal");
    if declared == to {
        checks.push(Check::ok("env.MOMO_PROFILE", format!("= {to}")));
    } else {
        checks.push(Check::fail(
            "env.MOMO_PROFILE",
            format!("target file declares '{declared}', expected '{to}'"),
            "fix MOMO_PROFILE or switch to the matching profile",
        ));
    }

    match schema {
        Some(s) => checks.extend(domain_checks(map, s)),
        None => checks.push(Check::warn(
            "env.domain",
            "config.schema.toml not found; value domain checks skipped",
            "run from the momo monorepo for full validation",
        )),
    }

    match ServeProfile::load_builtin(to) {
        Ok(doc) => checks.push(Check::ok(
            "profile.components",
            format!("{} component(s) parse ok", doc.components.len()),
        )),
        Err(e) => checks.push(Check::fail(
            "profile.components",
            format!("profile '{to}' failed to parse: {e}"),
            "profiles/*.toml is broken; fix the component table",
        )),
    }

    // 条件必需：LLM 端点指 DeepSeek 时必须配 MOMO_LLM_API_KEY（与 doctor.rs 口径一致）。
    let ep = map
        .get("MOMO_LLM_ENDPOINT")
        .map(String::as_str)
        .unwrap_or("");
    if ep.contains("deepseek.com")
        && map
            .get("MOMO_LLM_API_KEY")
            .is_none_or(|v| v.is_empty() || v.starts_with("your-"))
    {
        checks.push(Check::fail(
            "env.MOMO_LLM_API_KEY",
            "unset but MOMO_LLM_ENDPOINT is api.deepseek.com",
            "degraded DeepSeek profile requires MOMO_LLM_API_KEY in the target file",
        ));
    }
    checks
}

/// schema 取值域校验：enum ∈ values；bool ∈ true/false；int 可解析；
/// url 需带 scheme。path/string 不查取值。
fn domain_checks(map: &BTreeMap<String, String>, schema: &Schema) -> Vec<Check> {
    let mut checks = Vec::new();
    for (k, v) in map {
        let Some(spec) = schema.get(k) else { continue };
        let bad = match spec.kind.as_str() {
            "enum" if !spec.values.iter().any(|x| x == v) => {
                Some(format!("'{v}' not in {:?}", spec.values))
            }
            "bool" if v != "true" && v != "false" => Some(format!("'{v}' not a bool")),
            "int" if v.parse::<i64>().is_err() => Some(format!("'{v}' not an int")),
            "url" if !v.contains("://") => Some(format!("'{v}' missing scheme")),
            _ => None,
        };
        if let Some(why) = bad {
            checks.push(Check::fail(
                format!("env.{k}"),
                format!("invalid value: {why}"),
                "fix the value to match config.schema.toml domain",
            ));
        }
    }
    if checks.is_empty() {
        checks.push(Check::ok("env.domain", "all values within schema domains"));
    }
    checks
}

/// `--restart` 前让进程 env 与新 .env 一致：新文件键覆盖写入；
/// 旧文件有而新文件没有的键移除（不残留旧档配置）。
fn reload_process_env(link: &Path, old: BTreeMap<String, String>) {
    let target = env_file::read_link_target(link).unwrap_or_else(|| link.to_path_buf());
    let Ok(map) = env_file::parse_env_file(&target) else {
        return;
    };
    for k in old.keys().filter(|k| !map.contains_key(*k)) {
        std::env::remove_var(k);
    }
    for (k, v) in map {
        std::env::set_var(k, v);
    }
}

fn render(report: &SwitchReport, ctx: &super::Ctx) -> Result<(), AppError> {
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        return Ok(());
    }
    let mut w = std::io::stdout().lock();
    writeln!(w, "== {} ==", report.command)?;
    writeln!(w, "env file: {}", report.env_file)?;
    writeln!(w, "target:   {}", report.target)?;
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
    if !report.env_diff.is_empty() {
        writeln!(w, "env diff:")?;
        for d in &report.env_diff {
            writeln!(
                w,
                "  {}: {} → {}",
                d.key,
                d.from.as_deref().unwrap_or("(unset)"),
                d.to.as_deref().unwrap_or("(unset)")
            )?;
        }
    }
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

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// degraded 生成：MOMO_PROFILE/LLM_ENDPOINT/LLM_MODEL 被覆盖，注释保留。
    #[test]
    fn generate_env_applies_degraded_overrides() {
        let text = generate_env("degraded");
        assert!(text.contains("MOMO_PROFILE=degraded"));
        assert!(text.contains("MOMO_LLM_ENDPOINT=http://127.0.0.1:7820/v1"));
        assert!(text.contains("MOMO_LLM_MODEL=qwen3-8b-local"));
        assert!(!text.contains("MOMO_PROFILE=normal "));
        // 模板注释行保留
        assert!(text.contains("档位切换"));
    }

    /// normal 生成：值与模板一致（MOMO_PROFILE 仍为 normal）。
    #[test]
    fn generate_env_normal_matches_template() {
        let text = generate_env("normal");
        assert!(text.contains("MOMO_PROFILE=normal"));
        assert!(text.contains("MOMO_LLM_ENDPOINT=http://mac-studio:7820/v1"));
    }

    /// REQUIRED_KEYS 缺失 → fail；齐全 + 域名内 → 无 fail。
    #[test]
    fn validate_flags_missing_required() {
        let m = map(&[("MOMO_PROFILE", "degraded")]);
        let checks = validate_target(&m, "degraded", None);
        assert!(checks
            .iter()
            .any(|c| c.id == "env.MOMO_LLM_ENDPOINT" && c.status == CheckStatus::Fail));

        let full = map(&[
            ("MOMO_PROFILE", "degraded"),
            ("MOMO_ORCH_LISTEN", "127.0.0.1:7800"),
            ("MOMO_LLM_ENDPOINT", "http://127.0.0.1:7820/v1"),
            ("MOMO_LLM_MODEL", "qwen3-8b-local"),
            ("MOMO_ASR_ENDPOINT", "ws://127.0.0.1:7810/ws"),
            ("MOMO_TTS_ENDPOINT", "ws://127.0.0.1:7811/ws"),
        ]);
        let checks = validate_target(&full, "degraded", None);
        assert!(
            !checks.iter().any(|c| c.status == CheckStatus::Fail),
            "{checks:?}"
        );
    }

    /// profile 不一致 → fail。
    #[test]
    fn validate_flags_profile_mismatch() {
        let m = map(&[
            ("MOMO_PROFILE", "normal"),
            ("MOMO_ORCH_LISTEN", "x"),
            ("MOMO_LLM_ENDPOINT", "e"),
            ("MOMO_LLM_MODEL", "m"),
            ("MOMO_ASR_ENDPOINT", "a"),
            ("MOMO_TTS_ENDPOINT", "t"),
        ]);
        let checks = validate_target(&m, "degraded", None);
        assert!(checks
            .iter()
            .any(|c| c.id == "env.MOMO_PROFILE" && c.status == CheckStatus::Fail));
    }

    /// unix：symlink 重指 + 普通文件备份；重复执行幂等。
    #[cfg(unix)]
    #[test]
    fn repoint_link_unix() {
        let dir = std::env::temp_dir().join(format!("momoctl-switch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join(".env");
        let t_normal = dir.join(".env.normal");
        let t_degraded = dir.join(".env.degraded");
        fs::write(&t_normal, "MOMO_PROFILE=normal\n").unwrap();
        fs::write(&t_degraded, "MOMO_PROFILE=degraded\n").unwrap();

        // 普通文件 → 备份 + 建链
        fs::write(&link, "custom\n").unwrap();
        match repoint_link(&link, ".env.normal").unwrap() {
            LinkOutcome::BackedUp(bak) => {
                assert_eq!(bak, dir.join(".env.bak"));
                assert_eq!(fs::read_to_string(&bak).unwrap(), "custom\n");
            }
            _ => panic!("expected backup outcome"),
        }
        assert_eq!(fs::read_link(&link).unwrap(), Path::new(".env.normal"));

        // 重指
        match repoint_link(&link, ".env.degraded").unwrap() {
            LinkOutcome::Linked => {}
            _ => panic!("expected linked outcome"),
        }
        assert_eq!(fs::read_link(&link).unwrap(), Path::new(".env.degraded"));

        // 幂等
        match repoint_link(&link, ".env.degraded").unwrap() {
            LinkOutcome::Already => {}
            _ => panic!("expected already outcome"),
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
