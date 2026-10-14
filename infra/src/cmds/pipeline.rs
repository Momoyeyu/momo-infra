//! `momoctl pipeline list|run|resume|status`：训练 pipeline 编排
//! （`infra/pipelines/*.toml` + `crate::pipeline` core；口径见
//! momo-docs `02-repos/momo-infra.md`「训练 pipeline 编排」）。
//!
//! 退出码：0 成功；1 step 执行失败；2 用法/配置（缺 --set、run 不存在）。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::api::dto::{PipelineInfo, PipelineListReport, PipelineRunReport};
use crate::cli::{
    PipelineAction, PipelineArgs, PipelineDirArgs, PipelineResumeArgs, PipelineRunArgs,
    PipelineStatusArgs,
};
use crate::env_cfg::EnvCfg;
use crate::manifest;
use crate::output::{AppError, ExitCode};
use crate::pipeline::{self, DryRunExecutor, RunOpts, RunState, RunStatus, ShellExecutor};

pub fn run(args: &PipelineArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    match &args.action {
        PipelineAction::List(a) => list(a, ctx),
        PipelineAction::Run(a) => run_new(a, ctx),
        PipelineAction::Resume(a) => resume(a, ctx),
        PipelineAction::Status(a) => status(a, ctx),
    }
}

/// `--dir` > 自动探测；都找不到时给出候选清单（exit 2）。
fn pipelines_dir(flag: Option<&PathBuf>) -> Result<PathBuf, AppError> {
    if let Some(d) = flag {
        if d.is_dir() {
            return Ok(d.clone());
        }
        return Err(AppError::Config(format!(
            "pipelines dir not found: {}",
            d.display()
        )));
    }
    pipeline::default_pipelines_dir().ok_or_else(|| {
        AppError::Config(
            "pipelines dir not found (expected infra/pipelines/; pass --dir PATH)".into(),
        )
    })
}

fn list(args: &PipelineDirArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let dir = pipelines_dir(args.dir.as_ref())?;
    let defs = pipeline::discover(&dir)?;
    let infos: Vec<PipelineInfo> = defs
        .iter()
        .map(|(path, d)| PipelineInfo {
            name: d.name.clone(),
            description: d.description.clone(),
            artifact_dir: d.artifact_dir.clone(),
            activate_env: d.activate_env.clone(),
            steps: d.steps.iter().map(|s| s.name.clone()).collect(),
            vars: d
                .vars
                .iter()
                .map(|(name, v)| crate::api::dto::PipelineVarInfo {
                    name: name.clone(),
                    description: v.description.clone(),
                    required: v.required,
                    default: v.default.clone(),
                })
                .collect(),
            path: path.display().to_string(),
        })
        .collect();
    if ctx.json {
        let report = PipelineListReport {
            schema: crate::api::dto::SCHEMA_VERSION,
            command: "pipeline list".into(),
            dir: dir.display().to_string(),
            pipelines: infos,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        let out = std::io::stdout();
        let mut w = out.lock();
        writeln!(w, "pipelines dir: {}", dir.display())?;
        for i in &infos {
            writeln!(w, "  {}  ({})", i.name, i.steps.join(" -> "))?;
            if !i.description.is_empty() {
                writeln!(w, "      {}", i.description)?;
            }
        }
    }
    Ok(ExitCode::Ok as i32)
}

fn parse_sets(sets: &[String]) -> Result<BTreeMap<String, String>, AppError> {
    let mut out = BTreeMap::new();
    for s in sets {
        let (k, v) = s
            .split_once('=')
            .ok_or_else(|| AppError::Config(format!("--set expects KEY=VALUE, got {s:?}")))?;
        if k.trim().is_empty() {
            return Err(AppError::Config(format!("--set with empty key: {s:?}")));
        }
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}

fn make_opts(
    dir: &Path,
    manifest_flag: Option<&PathBuf>,
    date: Option<&String>,
    dry_run: bool,
    vars: BTreeMap<String, String>,
) -> Result<RunOpts, AppError> {
    let manifest_path = match manifest_flag {
        Some(p) => p.clone(),
        None => manifest::default_manifest_path().ok_or_else(|| {
            AppError::Config(format!(
                "MANIFEST.toml not found; tried {:?} (pass --manifest PATH)",
                manifest::manifest_candidates()
            ))
        })?,
    };
    let date = match date {
        Some(d) => {
            if d.len() != 8 || !d.bytes().all(|b| b.is_ascii_digit()) {
                return Err(AppError::Config(format!(
                    "--date expects yyyymmdd, got {d:?}"
                )));
            }
            d.clone()
        }
        None => pipeline::today_yyyymmdd(),
    };
    Ok(RunOpts {
        runs_root: pipeline::runs_root(),
        models_dir: EnvCfg.models_dir(),
        repo_root: pipeline::repo_root_for(dir),
        manifest_path,
        date,
        dry_run,
        vars,
    })
}

fn run_new(args: &PipelineRunArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let dir = pipelines_dir(args.dir.as_ref())?;
    let (_, def) = pipeline::load_def_by_name(&dir, &args.name)?;
    let opts = make_opts(
        &dir,
        args.manifest.as_ref(),
        args.date.as_ref(),
        args.dry_run,
        parse_sets(&args.set)?,
    )?;
    let exec = if args.dry_run {
        &DryRunExecutor as &dyn pipeline::Executor
    } else {
        &ShellExecutor
    };
    let run = pipeline::run_pipeline(&def, &opts, exec)?;
    render_run(&run, "pipeline run", ctx)?;
    Ok(if run.status == RunStatus::Succeeded {
        ExitCode::Ok as i32
    } else {
        ExitCode::Runtime as i32
    })
}

fn resume(args: &PipelineResumeArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let dir = pipelines_dir(args.dir.as_ref())?;
    let (_, def) = pipeline::load_def_by_name(&dir, &args.name)?;
    let opts = make_opts(
        &dir,
        args.manifest.as_ref(),
        None,
        false,
        parse_sets(&args.set)?,
    )?;
    let run_dir = pick_run_dir(&opts.runs_root, &def.name, args.run_id.as_deref(), false)?;
    let run = pipeline::resume_pipeline(&def, &opts, &run_dir, &ShellExecutor)?;
    render_run(&run, "pipeline resume", ctx)?;
    Ok(if run.status == RunStatus::Succeeded {
        ExitCode::Ok as i32
    } else {
        ExitCode::Runtime as i32
    })
}

fn status(args: &PipelineStatusArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let dir = pipelines_dir(args.dir.as_ref())?;
    let (_, def) = pipeline::load_def_by_name(&dir, &args.name)?;
    let run_dir = pick_run_dir(
        &pipeline::runs_root(),
        &def.name,
        args.run_id.as_deref(),
        true,
    )?;
    let run = pipeline::load_run(&run_dir)?;
    render_run(&run, "pipeline status", ctx)?;
    Ok(ExitCode::Ok as i32)
}

/// `--run-id` 显式 → 校验存在；缺省取字典序最大（ULID = 时间序）。
fn pick_run_dir(
    runs_root: &Path,
    pipeline: &str,
    run_id: Option<&str>,
    for_status: bool,
) -> Result<PathBuf, AppError> {
    let base = runs_root.join(pipeline);
    if let Some(id) = run_id {
        let dir = base.join(id);
        if dir.join("run.json").is_file() {
            return Ok(dir);
        }
        return Err(AppError::Config(format!(
            "run '{id}' not found for pipeline '{pipeline}'"
        )));
    }
    let runs = pipeline::list_runs(runs_root, pipeline);
    let pick = if for_status {
        runs.last()
    } else {
        // resume：取最近一个未成功 run。
        runs.iter().rev().find(|d| {
            pipeline::load_run(d)
                .map(|r| r.status != RunStatus::Succeeded)
                .unwrap_or(false)
        })
    };
    pick.cloned().ok_or_else(|| {
        AppError::Config(format!(
            "no resumable run for pipeline '{pipeline}' under {}",
            base.display()
        ))
    })
}

fn render_run(run: &RunState, command: &str, ctx: &super::Ctx) -> Result<(), AppError> {
    if ctx.json {
        let report = PipelineRunReport {
            schema: crate::api::dto::SCHEMA_VERSION,
            command: command.into(),
            run: run.clone(),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        return Ok(());
    }
    let out = std::io::stdout();
    let mut w = out.lock();
    writeln!(
        w,
        "pipeline {} run {} — {}{}",
        run.pipeline,
        run.run_id,
        run.status.as_str(),
        if run.dry_run { " (dry-run)" } else { "" }
    )?;
    writeln!(w, "  artifact: {}", run.artifact_dir)?;
    writeln!(w, "  run dir:  {}", run.run_dir)?;
    for s in &run.steps {
        writeln!(
            w,
            "  {:<10} {:<20} attempts={}{}{}",
            s.status.as_str(),
            s.name,
            s.attempts,
            s.artifact
                .as_deref()
                .map(|a| format!("  artifact={a}"))
                .unwrap_or_default(),
            s.finished_at
                .as_deref()
                .map(|t| format!("  finished={t}"))
                .unwrap_or_default(),
        )?;
        if run.dry_run {
            if let Some(c) = &s.command {
                writeln!(w, "      $ {c}")?;
            }
        }
        if let Some(e) = &s.error {
            writeln!(w, "      error: {e}")?;
        }
    }
    Ok(())
}
