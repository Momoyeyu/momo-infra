//! `momoctl jobs <submit|list|status|log|cancel>`：job runner 的 CLI 面。
//! `jobs exec --id` 是 submit spawn 的内部 helper（hidden），不是公开操作。
//! 读写同一个 job store（state dir 约定），与 `/api/jobs` 同口径。
//! 退出码：0 成功；1 运行时；2 用法/配置（未知 job id、空命令等）。

use std::io::Write;
use std::thread;
use std::time::Duration;

use serde::Serialize;

use crate::cli::{JobsAction, JobsArgs, JobsLogArgs, JobsSubmitArgs};
use crate::jobs::ssh::SshSubmit;
use crate::jobs::store::{self, JobStore};
use crate::jobs::{runner, JobExecutor, JobRecord};
use crate::output::{AppError, ExitCode};

/// `jobs log -f` 轮询间隔（同 serve logs 口径）。
const FOLLOW_POLL: Duration = Duration::from_millis(300);

#[derive(Debug, Serialize)]
struct JobsReport {
    schema: u8,
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    job: Option<JobRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jobs: Option<Vec<JobRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

pub fn run(args: &JobsArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let store = JobStore::open_default();
    match &args.action {
        JobsAction::Submit(a) => {
            let ssh = build_ssh_submit(a)?;
            let desc = a.desc.clone().unwrap_or_else(|| a.cmd.join(" "));
            let (rec, warns) = runner::submit(&store, a.kind, a.cmd.clone(), desc, ssh)?;
            warn_stderr(&warns);
            emit_job("jobs submit", &rec, None, ctx)?;
            Ok(ExitCode::Ok as i32)
        }
        JobsAction::Exec(a) => runner::exec(&store, &a.id),
        JobsAction::Recover(a) => {
            reconcile(&store)?;
            let (rec, note) = runner::recover(&store, &a.id)?;
            emit_job("jobs recover", &rec, Some(note), ctx)?;
            Ok(ExitCode::Ok as i32)
        }
        JobsAction::List => {
            reconcile(&store)?;
            let (jobs, warnings) = store.list()?;
            warn_stderr(&warnings);
            if ctx.json {
                let report = JobsReport {
                    schema: 1,
                    command: "jobs list".into(),
                    job: None,
                    jobs: Some(jobs),
                    note: None,
                };
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                render_list(&jobs);
            }
            Ok(ExitCode::Ok as i32)
        }
        JobsAction::Status(a) => {
            reconcile(&store)?;
            let rec = must_load(&store, &a.id)?;
            emit_job("jobs status", &rec, None, ctx)?;
            Ok(ExitCode::Ok as i32)
        }
        JobsAction::Cancel(a) => {
            reconcile(&store)?;
            let (rec, note) = runner::cancel(&store, &a.id)?;
            emit_job("jobs cancel", &rec, Some(note.as_str()), ctx)?;
            Ok(ExitCode::Ok as i32)
        }
        JobsAction::Log(a) => log(&store, a, ctx),
    }
}

/// `jobs submit` flags → `Option<SshSubmit>`。
/// `--executor ssh` 必须有 `--host`；local 执行器拒绝任何 ssh flag。
fn build_ssh_submit(a: &JobsSubmitArgs) -> Result<Option<SshSubmit>, AppError> {
    let ssh_flags_used = a.host.is_some()
        || a.user.is_some()
        || a.key.is_some()
        || a.remote_dir.is_some()
        || !a.env.is_empty()
        || !a.data.is_empty()
        || a.artifact_name.is_some()
        || a.lease_hours.is_some();
    match a.executor {
        JobExecutor::Local => {
            if ssh_flags_used {
                return Err(AppError::Config(
                    "ssh flags require `--executor ssh`".into(),
                ));
            }
            Ok(None)
        }
        JobExecutor::Ssh => {
            let Some(host) = a.host.clone() else {
                return Err(AppError::Config(
                    "--executor ssh requires --host (ssh alias or hostname)".into(),
                ));
            };
            let mut env = std::collections::BTreeMap::new();
            for e in &a.env {
                // KEY=VALUE 显式值；裸 KEY = 取本地 env（value None）
                match e.split_once('=') {
                    Some((k, v)) => env.insert(k.to_string(), Some(v.to_string())),
                    None => env.insert(e.clone(), None),
                };
            }
            Ok(Some(SshSubmit {
                host,
                user: a.user.clone(),
                key: a.key.clone(),
                remote_dir: a.remote_dir.clone(),
                env,
                data: a.data.clone(),
                artifact_name: a.artifact_name.clone(),
                lease_hours: a.lease_hours,
            }))
        }
    }
}

fn must_load(store: &JobStore, id: &str) -> Result<JobRecord, AppError> {
    store
        .load(id)?
        .ok_or_else(|| AppError::Config(format!("job {id} not found")))
}

fn emit_job(
    command: &str,
    rec: &JobRecord,
    note: Option<&str>,
    ctx: &super::Ctx,
) -> Result<(), AppError> {
    if ctx.json {
        let report = JobsReport {
            schema: 1,
            command: command.into(),
            job: Some(rec.clone()),
            jobs: None,
            note: note.map(str::to_string),
        };
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        let mut w = std::io::stdout().lock();
        if let Some(n) = note {
            writeln!(w, "{n}")?;
        }
        render_record(&mut w, rec)?;
    }
    Ok(())
}

/// `jobs log`：tail `logs/<id>.log` 最后 N 行；
/// `-f` 跟随新增直到 job 进终态且日志排空。
fn log(store: &JobStore, args: &JobsLogArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    reconcile(store)?;
    let rec = must_load(store, &args.id)?;
    let path = store.log_path(&args.id)?;

    let lines = if path.is_file() {
        store::read_tail(&path, args.lines)?
    } else {
        Vec::new()
    };

    if ctx.json {
        let report = serde_json::json!({
            "schema": 1,
            "command": "jobs log",
            "job": rec.id,
            "path": if path.is_file() { Some(path.display().to_string()) } else { None },
            "lines": lines,
        });
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        let mut w = std::io::stdout().lock();
        if path.is_file() {
            writeln!(w, "== {} ({}) ==", rec.id, path.display())?;
        } else {
            writeln!(w, "== {} (no log file) ==", rec.id)?;
        }
        for line in &lines {
            writeln!(w, "{line}")?;
        }
    }

    if args.follow {
        follow(&store.clone(), args, &path, ctx)?;
    }
    Ok(ExitCode::Ok as i32)
}

/// `-f` 跟随：从当前文件末尾增量读，打到 stdout；job 终态且排空后退出。
/// json 模式每批一行 NDJSON `{"lines":[...]}`，收尾一行 `{"done":true,"status":...}`。
fn follow(
    store: &JobStore,
    args: &JobsLogArgs,
    path: &std::path::Path,
    ctx: &super::Ctx,
) -> Result<(), AppError> {
    let mut offset = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    loop {
        let (cur, chunk) = store::read_chunk(path, offset)?;
        offset = cur;
        if !chunk.is_empty() {
            if ctx.json {
                let lines: Vec<&str> = chunk.lines().collect();
                println!("{}", serde_json::json!({"lines": lines}));
            } else {
                let mut w = std::io::stdout().lock();
                write!(w, "{chunk}")?;
                w.flush()?;
            }
        }
        let terminal = store
            .load(&args.id)?
            .is_some_and(|r| r.status.is_terminal());
        if terminal && chunk.is_empty() {
            if ctx.json {
                let status = store
                    .load(&args.id)?
                    .map(|r| r.status.as_str().to_string())
                    .unwrap_or_else(|| "unknown".into());
                println!("{}", serde_json::json!({"done": true, "status": status}));
            }
            return Ok(());
        }
        thread::sleep(FOLLOW_POLL);
    }
}

fn render_list(jobs: &[JobRecord]) {
    let mut w = std::io::stdout().lock();
    if jobs.is_empty() {
        let _ = writeln!(w, "== jobs ==\n  (none)");
        return;
    }
    let _ = writeln!(w, "== jobs ==");
    for r in jobs {
        let exit = r
            .exit
            .map(|c| format!("exit={c}"))
            .unwrap_or_else(|| "-".into());
        let _ = writeln!(
            w,
            "  {}  {:<5}  {:<10}  {:<7}  {:>6}  {}",
            r.id,
            r.kind.as_str(),
            r.status.as_str(),
            exit,
            fmt_age(r.created_at),
            r.desc
        );
    }
}

fn render_record(w: &mut impl Write, r: &JobRecord) -> Result<(), AppError> {
    writeln!(w, "job {}", r.id)?;
    writeln!(w, "  status   {}", r.status.as_str())?;
    writeln!(w, "  kind     {}", r.kind.as_str())?;
    writeln!(w, "  executor {}", r.executor.as_str())?;
    if !r.desc.is_empty() {
        writeln!(w, "  desc     {}", r.desc)?;
    }
    writeln!(w, "  args     {}", r.args.join(" "))?;
    writeln!(w, "  created  {} ago", fmt_age(r.created_at))?;
    if let Some(spec) = &r.ssh {
        let host = match &spec.user {
            Some(u) => format!("{u}@{}", spec.host),
            None => spec.host.clone(),
        };
        writeln!(w, "  remote   {host}:{}", spec.remote_dir)?;
        if let Some(ms) = spec.release_ms {
            let left_s = ms.saturating_sub(crate::jobs::now_ms()) / 1000;
            writeln!(w, "  release  in {}m", left_s / 60)?;
        }
    }
    if let Some(rid) = &r.remote_id {
        writeln!(w, "  remote id {rid}")?;
    }
    if let Some(pid) = r.pid {
        writeln!(w, "  pid      {pid}")?;
    }
    if let Some(rp) = r.runner_pid {
        writeln!(w, "  runner   pid {rp}")?;
    }
    writeln!(w, "  log      {}", r.log_path)?;
    if let Some(c) = r.exit {
        writeln!(w, "  exit     {c}")?;
    }
    if let Some(e) = &r.error {
        writeln!(w, "  error    {e}")?;
    }
    Ok(())
}

/// epoch 毫秒 → 紧凑「多久前」（`12s`/`3m`/`2h`/`1d`）。
fn fmt_age(created_ms: u64) -> String {
    let now = crate::jobs::now_ms();
    let s = now.saturating_sub(created_ms) / 1000;
    if s >= 86400 {
        format!("{}d", s / 86400)
    } else if s >= 3600 {
        format!("{}h", s / 3600)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// rescan（#54）：读侧 reconcile——回放索引对齐 meta，按 process identity
/// 收口非终态 job；修正/警告打到 stderr（stdout 保持干净 JSON/table）。
fn reconcile(store: &JobStore) -> Result<(), AppError> {
    let notes = store.reconcile()?;
    warn_stderr(&notes);
    Ok(())
}

fn warn_stderr(warnings: &[String]) {
    for w in warnings {
        eprintln!("warn: {w}");
    }
}
