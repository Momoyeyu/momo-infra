//! `momoctl config check`：schema ↔ .env.example ↔ config.md 漂移检测；退出码 0/3/2。

use std::path::{Path, PathBuf};

use crate::api::dto::{ConfigCheckReport, ConfigCheckSummary};
use crate::cli::{ConfigAction, ConfigArgs, ConfigCheckArgs};
use crate::config_schema::{self, SourceReport};
use crate::output::{AppError, ExitCode};

pub fn run(args: &ConfigArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    match &args.action {
        ConfigAction::Check(a) => check(a, ctx),
    }
}

fn check(args: &ConfigCheckArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let root = match &args.root {
        Some(p) => p.clone(),
        None => repo_root()?,
    };
    let sources = config_schema::check_repo(&root)?;
    let drift_count: usize = sources.iter().map(|s| s.drifts.len()).sum();
    let skipped_count = sources.iter().filter(|s| s.skipped).count();

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report_json(&root, &sources, drift_count, skipped_count))
                .unwrap()
        );
    } else {
        render_human(&sources, drift_count, skipped_count);
    }

    Ok(if drift_count == 0 {
        ExitCode::Ok as i32
    } else {
        ExitCode::CheckFailed as i32
    })
}

/// 未给 --root：从 CWD 向上定位 `config.schema.toml`。
fn repo_root() -> Result<PathBuf, AppError> {
    let cwd = std::env::current_dir()?;
    config_schema::find_root(&cwd).ok_or_else(|| {
        AppError::Config(format!(
            "config.schema.toml not found above {}; pass --root",
            cwd.display()
        ))
    })
}

fn report_json(
    root: &Path,
    sources: &[SourceReport],
    drift_count: usize,
    skipped_count: usize,
) -> ConfigCheckReport {
    ConfigCheckReport {
        schema: crate::api::dto::SCHEMA_VERSION,
        root: root.display().to_string(),
        sources: sources.to_vec(),
        summary: ConfigCheckSummary {
            sources: sources.len(),
            skipped: skipped_count,
            drift: drift_count,
        },
    }
}

fn render_human(sources: &[SourceReport], drift_count: usize, skipped_count: usize) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    for s in sources {
        if s.skipped {
            let _ = writeln!(out, "  skip  {} (absent under root)", s.file);
        } else if s.ok() {
            let _ = writeln!(out, "  ok    {}", s.file);
        } else {
            let _ = writeln!(out, "  drift {}", s.file);
            for d in &s.drifts {
                match &d.key {
                    Some(k) => {
                        let _ = writeln!(out, "    [{:<9}] {k}: {}", d.kind, d.detail);
                    }
                    None => {
                        let _ = writeln!(out, "    [{:<9}] {}", d.kind, d.detail);
                    }
                }
            }
        }
    }
    let _ = writeln!(
        out,
        "summary: {} sources ({} skipped), {} drift item(s)",
        sources.len(),
        skipped_count,
        drift_count
    );
}
