//! `momoctl serve <up|down|status|logs|restart>`：按 profile 组件表 spawn/停止/探测，
//! pidfile + 日志落盘；external 组件只探活不拉起（momo-infra.md service 操作流程）。

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use crate::api::dto::{
    Check, CheckStatus, ComponentInfo, LogBatch, LogEntry, ServeComponentsReport, ServeLogsReport,
    ServeReport, Summary,
};
use crate::cli::{LogsArgs, Profile, ServeAction, ServeArgs, ServeCommon};
use crate::env_cfg::{self, EnvCfg};
use crate::net;
use crate::output::{not_implemented, AppError, ExitCode};
use crate::serve::proc;
use crate::serve::profile::{Component, ServeProfile};

/// bounded readiness 上限（momo-infra.md：bounded readiness 通过才算 active）。
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const READY_POLL: Duration = Duration::from_millis(200);
/// down 时 SIGTERM 后的宽限期，超时升级 SIGKILL。
const TERM_GRACE: Duration = Duration::from_secs(10);
/// serve logs -f 轮询间隔。
const FOLLOW_POLL: Duration = Duration::from_millis(300);

pub fn run(args: &ServeArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    if let ServeAction::Logs(largs) = &args.action {
        let profile = EnvCfg.profile(largs.common.profile);
        let doc = ServeProfile::load_builtin(profile.as_str())?;
        return logs(&doc, largs, profile.as_str(), ctx);
    }
    let (action, common) = split(&args.action);
    let profile = EnvCfg.profile(common.profile);
    let doc = ServeProfile::load_builtin(profile.as_str())?;
    match action {
        "up" => up(&doc, common, profile.as_str(), ctx),
        "down" => down(&doc, common, profile.as_str(), ctx),
        "status" => status(&doc, common, profile.as_str(), ctx),
        "restart" => restart(&doc, common, profile.as_str(), ctx),
        _ => Ok(not_implemented(&format!("serve {action}"), ctx.json)),
    }
}

/// `--only` 过滤组件序列；未命中 → usage 错误（exit 2）。
fn select_components<'d>(
    ordered: Vec<&'d Component>,
    common: &ServeCommon,
    profile: &str,
) -> Result<Vec<&'d Component>, AppError> {
    let v: Vec<&Component> = ordered
        .into_iter()
        .filter(|c| common.only.is_empty() || common.only.iter().any(|n| n == &c.name))
        .collect();
    if v.is_empty() {
        return Err(AppError::Config(format!(
            "profile '{profile}': no component matches --only {:?}",
            common.only
        )));
    }
    Ok(v)
}

/// `serve down`：stop_order 逆序停 owned 组件；幂等 no-op。
fn down(
    doc: &ServeProfile,
    common: &ServeCommon,
    profile: &str,
    ctx: &super::Ctx,
) -> Result<i32, AppError> {
    let (checks, code) = down_inner(doc, common, profile)?;
    render(
        &ServeReport {
            schema: 1,
            command: "serve down".into(),
            profile: profile.into(),
            summary: Summary::from_checks(&checks),
            checks,
        },
        ctx,
    )?;
    Ok(code)
}

pub(crate) fn down_inner(
    doc: &ServeProfile,
    common: &ServeCommon,
    profile: &str,
) -> Result<(Vec<Check>, i32), AppError> {
    let selected = select_components(doc.stop_order(), common, profile)?;
    let mut checks = Vec::new();
    let mut failed = false;

    for c in selected {
        // external/attached provider 不属 owned：绝不关闭（deployment.md process ownership）。
        if c.external {
            checks.push(Check::ok(
                c.name.clone(),
                "external provider not owned; skipped",
            ));
            continue;
        }
        match proc::read_pidfile(&c.name) {
            Err(_) => checks.push(Check::ok(c.name.clone(), "stopped")),
            // 组口径判活：wrapper 死后孙进程（uv run 的 python 本体）残留仍算活，
            // 走停止分支收掉（#149），不会被误判成 stale pidfile。
            Ok(pid) if !proc::component_alive(&c.name, pid) => {
                let _ = proc::remove_pidfile(&c.name);
                checks.push(Check::ok(
                    c.name.clone(),
                    format!("stale pidfile removed (pid {pid} not running)"),
                ));
            }
            Ok(pid) => {
                if proc::stop_component(&c.name, pid, TERM_GRACE) {
                    let _ = proc::remove_pidfile(&c.name);
                    checks.push(Check::ok(c.name.clone(), format!("stopped pid {pid}")));
                } else {
                    failed = true;
                    checks.push(Check::fail(
                        c.name.clone(),
                        format!("pid {pid} or its process group did not exit after TERM+KILL"),
                        "inspect processes manually, then retry momoctl serve down",
                    ));
                }
            }
        }
    }

    // 全量 down 全部停妥 → 清 active_profile（部分停/--only 保留）。
    if !failed && common.only.is_empty() {
        let _ = std::fs::remove_file(proc::active_profile_file());
    }
    Ok((
        checks,
        if failed {
            ExitCode::Runtime as i32
        } else {
            ExitCode::Ok as i32
        },
    ))
}

/// `serve status` core 报告：`GET /api/serve/status` 与 CLI 共用同一 `ServeReport`。
/// 读 pidfile + 探活（文件 IO/网络探测），web 侧须 `spawn_blocking` 调用。
/// 返回 `(report, 退出码口径)`——REST 恒 200 下发报告，code 仅保留语义。
pub fn status_report(profile_arg: Option<Profile>) -> Result<(ServeReport, i32), AppError> {
    let profile = EnvCfg.profile(profile_arg);
    let doc = ServeProfile::load_builtin(profile.as_str())?;
    let common = ServeCommon {
        profile: profile_arg,
        only: Vec::new(),
        app_bin: None,
    };
    let (checks, code) = status_inner(&doc, &common, profile.as_str())?;
    Ok((
        ServeReport {
            schema: 1,
            command: "serve status".into(),
            profile: profile.as_str().into(),
            summary: Summary::from_checks(&checks),
            checks,
        },
        code,
    ))
}

/// `serve status`：running(pid+uptime)/stopped/stale pidfile；external 报探活。
fn status(
    doc: &ServeProfile,
    common: &ServeCommon,
    profile: &str,
    ctx: &super::Ctx,
) -> Result<i32, AppError> {
    let (checks, code) = status_inner(doc, common, profile)?;
    render(
        &ServeReport {
            schema: 1,
            command: "serve status".into(),
            profile: profile.into(),
            summary: Summary::from_checks(&checks),
            checks,
        },
        ctx,
    )?;
    Ok(code)
}

fn status_inner(
    doc: &ServeProfile,
    common: &ServeCommon,
    profile: &str,
) -> Result<(Vec<Check>, i32), AppError> {
    let selected = select_components(doc.start_order(), common, profile)?;
    let mut checks = Vec::new();

    for c in selected {
        if c.external {
            match probe_external(c) {
                Ok(d) => checks.push(Check::ok(c.name.clone(), format!("external {d}"))),
                Err(d) => checks.push(Check::warn(
                    c.name.clone(),
                    format!("external {d}"),
                    "external provider unreachable; not owned, serve does not start it",
                )),
            }
            continue;
        }
        match proc::read_pidfile(&c.name) {
            Err(_) => checks.push(Check::ok(c.name.clone(), "stopped")),
            Ok(pid) if !proc::component_alive(&c.name, pid) => checks.push(Check::fail(
                c.name.clone(),
                format!("pidfile stale: pid {pid} not running"),
                "run momoctl serve down to clean stale pidfiles",
            )),
            // wrapper 已死但孙进程残留（组/后代链 仍有成员）：不算 running，是泄漏态。
            Ok(pid) if !proc::is_alive(pid) => checks.push(Check::warn(
                c.name.clone(),
                format!("pid {pid} exited but process group still active"),
                "run momoctl serve down to stop leftover processes",
            )),
            Ok(pid) => {
                let up = proc::pidfile_started(&c.name)
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| format!(", up {}", fmt_uptime(d)))
                    .unwrap_or_default();
                match &c.ready {
                    Some(url) if !net::probe_get(url).ready() => checks.push(Check::warn(
                        c.name.clone(),
                        format!("running pid {pid}{up}; not ready: {url}"),
                        "component not ready; check its log under logs/",
                    )),
                    _ => checks.push(Check::ok(c.name.clone(), format!("running pid {pid}{up}"))),
                }
            }
        }
    }

    let code = if checks.iter().any(|c| c.status == CheckStatus::Fail) {
        ExitCode::CheckFailed as i32
    } else {
        ExitCode::Ok as i32
    };
    Ok((checks, code))
}

/// `GET /api/serve/components` core：内置 profile 组件表（order 升序）。
/// 只解析内嵌 toml，无 IO/网络探测，web 侧可同步调用。
pub fn components_report(profile_arg: Option<Profile>) -> Result<ServeComponentsReport, AppError> {
    let profile = EnvCfg.profile(profile_arg);
    let doc = ServeProfile::load_builtin(profile.as_str())?;
    Ok(ServeComponentsReport {
        schema: crate::api::dto::SCHEMA_VERSION,
        profile: profile.as_str().into(),
        components: doc
            .start_order()
            .into_iter()
            .map(|c| ComponentInfo {
                name: c.name.clone(),
                order: c.order,
                external: c.external,
                local_endpoint_only: c.local_endpoint_only,
            })
            .collect(),
    })
}

/// `POST /api/serve/{up|down|restart}` core：与 CLI 同一 inner 语义
/// （幂等 spawn + bounded readiness / 逆序 SIGTERM→SIGKILL / down+up）。
/// 秒级阻塞（readiness 最长 15s/组件），web 侧须 `spawn_blocking` 调用。
/// `app_bin` 无 REST 对应参数，固定 None（momod 走 PATH 查找）。
pub fn action_report(
    action: &str,
    profile_arg: Option<Profile>,
    only: Vec<String>,
) -> Result<(ServeReport, i32), AppError> {
    let profile = EnvCfg.profile(profile_arg);
    let doc = ServeProfile::load_builtin(profile.as_str())?;
    let common = ServeCommon {
        profile: profile_arg,
        only,
        app_bin: None,
    };
    let (checks, code) = match action {
        "up" => up_inner(&doc, &common, profile.as_str())?,
        "down" => down_inner(&doc, &common, profile.as_str())?,
        "restart" => {
            let (mut checks, down_code) = down_inner(&doc, &common, profile.as_str())?;
            let (up_checks, up_code) = up_inner(&doc, &common, profile.as_str())?;
            checks.extend(up_checks);
            (checks, if up_code != 0 { up_code } else { down_code })
        }
        // 调用方（web handler）已收敛 action 集合；到这里属内部误用。
        other => return Err(AppError::Config(format!("unknown serve action '{other}'"))),
    };
    Ok((
        ServeReport {
            schema: crate::api::dto::SCHEMA_VERSION,
            command: format!("serve {action}"),
            profile: profile.as_str().into(),
            summary: Summary::from_checks(&checks),
            checks,
        },
        code,
    ))
}

/// `serve restart` = down + up；合并报告，退出码取 up 优先。
fn restart(
    doc: &ServeProfile,
    common: &ServeCommon,
    profile: &str,
    ctx: &super::Ctx,
) -> Result<i32, AppError> {
    let (down_checks, down_code) = down_inner(doc, common, profile)?;
    let (up_checks, up_code) = up_inner(doc, common, profile)?;
    let mut checks = down_checks;
    checks.extend(up_checks);
    render(
        &ServeReport {
            schema: 1,
            command: "serve restart".into(),
            profile: profile.into(),
            summary: Summary::from_checks(&checks),
            checks,
        },
        ctx,
    )?;
    Ok(if up_code != 0 { up_code } else { down_code })
}

/// `serve logs`：每组件 tail `logs/<name>.log` 最后 N 行；-f 轮询追加内容。
fn logs(
    doc: &ServeProfile,
    args: &LogsArgs,
    profile: &str,
    ctx: &super::Ctx,
) -> Result<i32, AppError> {
    let selected = select_components(doc.start_order(), &args.common, profile)?;

    let mut entries = Vec::new();
    // follow 监听列表（含尚无日志文件的组件，出现后从头跟随）。
    let mut watched: Vec<(String, PathBuf)> = Vec::new();
    for c in selected {
        let path = proc::log_dir().join(format!("{}.log", c.name));
        watched.push((c.name.clone(), path.clone()));
        if path.is_file() {
            let lines = read_last_lines(&path, args.lines)?;
            entries.push(LogEntry {
                component: c.name.clone(),
                path: Some(path.display().to_string()),
                lines,
            });
        } else {
            entries.push(LogEntry {
                component: c.name.clone(),
                path: None,
                lines: Vec::new(),
            });
        }
    }

    if ctx.json {
        let report = ServeLogsReport {
            schema: crate::api::dto::SCHEMA_VERSION,
            command: "serve logs".into(),
            profile: profile.into(),
            logs: entries,
        };
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        let mut w = std::io::stdout().lock();
        for e in &entries {
            match &e.path {
                Some(p) => writeln!(w, "== {} ({p}) ==", e.component)?,
                None => writeln!(w, "== {} (no log file) ==", e.component)?,
            }
            for line in &e.lines {
                writeln!(w, "{line}")?;
            }
        }
    }

    if args.follow {
        follow_logs(&watched, ctx)?;
    }
    Ok(ExitCode::Ok as i32)
}

/// 读文件最后 n 行：先只读末尾 1 MiB 窗口，不够再全量重读。
pub(crate) fn read_last_lines(path: &Path, n: usize) -> Result<Vec<String>, AppError> {
    const WINDOW: u64 = 1 << 20;
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(WINDOW);
    let mut lines = tail_lines(&mut f, start, n)?;
    if start > 0 && lines.len() < n {
        lines = tail_lines(&mut f, 0, n)?;
    }
    Ok(lines)
}

fn tail_lines(f: &mut std::fs::File, start: u64, n: usize) -> Result<Vec<String>, AppError> {
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|s| s.trim_end_matches('\r').to_string())
        .collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if start > 0 && !lines.is_empty() {
        // 窗口首行可能是半行，丢弃。
        lines.remove(0);
    }
    Ok(lines
        .into_iter()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect())
}

/// `-f` 跟随：轮询各日志文件 offset 之后的新增内容并输出。
/// json 模式下每批输出一行 NDJSON `{"component","lines"}`。Ctrl-C 退出。
fn follow_logs(watched: &[(String, PathBuf)], ctx: &super::Ctx) -> Result<(), AppError> {
    let multi = watched.len() > 1;
    let mut offsets: HashMap<PathBuf, u64> = watched
        .iter()
        .map(|(_, p)| {
            let len = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            (p.clone(), len)
        })
        .collect();
    loop {
        for (name, path) in watched {
            let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let offset = offsets.entry(path.clone()).or_insert(0);
            if len < *offset {
                *offset = 0; // 日志被截断/轮转：从头读
            }
            if len <= *offset {
                continue;
            }
            let mut f = std::fs::File::open(path)?;
            f.seek(SeekFrom::Start(*offset))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            *offset = len;
            let text = String::from_utf8_lossy(&buf);
            let lines: Vec<&str> = text.lines().collect();
            if lines.is_empty() {
                continue;
            }
            if ctx.json {
                let body = LogBatch {
                    component: name,
                    lines,
                };
                println!("{}", serde_json::to_string(&body).unwrap());
            } else {
                let mut w = std::io::stdout().lock();
                for line in lines {
                    if multi {
                        writeln!(w, "{name}: {line}")?;
                    } else {
                        writeln!(w, "{line}")?;
                    }
                }
            }
        }
        thread::sleep(FOLLOW_POLL);
    }
}

/// `serve up`：按 start_order 逐个拉起；任一 fail → 停止本次 owned children。
fn up(
    doc: &ServeProfile,
    common: &ServeCommon,
    profile: &str,
    ctx: &super::Ctx,
) -> Result<i32, AppError> {
    let (checks, code) = up_inner(doc, common, profile)?;
    render(
        &ServeReport {
            schema: 1,
            command: "serve up".into(),
            profile: profile.into(),
            summary: Summary::from_checks(&checks),
            checks,
        },
        ctx,
    )?;
    Ok(code)
}

pub(crate) fn up_inner(
    doc: &ServeProfile,
    common: &ServeCommon,
    profile: &str,
) -> Result<(Vec<Check>, i32), AppError> {
    let llm = EnvCfg.llm_endpoint();
    let selected = select_components(doc.start_order(), common, profile)?;

    let mut checks: Vec<Check> = Vec::new();
    let mut owned: Vec<(String, Child)> = Vec::new();
    let mut aborted = false;

    for c in selected {
        if aborted {
            break;
        }
        // external / 条件跳过：探活或标记。
        if !doc.should_spawn(c, llm.as_deref()) {
            if c.external {
                match probe_external(c) {
                    Ok(url) => checks.push(Check::ok(
                        c.name.clone(),
                        format!("external probe ok: {url}"),
                    )),
                    Err(detail) => {
                        checks.push(Check::fail(
                            c.name.clone(),
                            detail,
                            "external provider unreachable; check endpoint/network then retry serve up",
                        ));
                        aborted = true;
                    }
                }
            } else {
                checks.push(Check::warn(
                    c.name.clone(),
                    "skipped: MOMO_LLM_ENDPOINT is not loopback",
                    "local_endpoint_only components spawn only under a local brain profile",
                ));
            }
            continue;
        }

        // 幂等：pidfile 指向的活进程不再拉起（momo-infra.md：同 identity 运行 → no-op）。
        if let Ok(pid) = proc::read_pidfile(&c.name) {
            if proc::is_alive(pid) {
                checks.push(Check::ok(
                    c.name.clone(),
                    format!("already running (pid {pid})"),
                ));
                continue;
            }
            // wrapper 已死但组/后代链 仍有成员（历史泄漏的孙进程持端口）→
            // 先收干净再拉起，否则新 spawn 撞端口再起不来（#149）。
            if proc::component_alive(&c.name, pid) {
                if proc::stop_component(&c.name, pid, TERM_GRACE) {
                    checks.push(Check::warn(
                        c.name.clone(),
                        format!("reaped leftover process group of dead pid {pid}"),
                        "leftover processes from a previous run were stopped before respawn",
                    ));
                } else {
                    checks.push(Check::fail(
                        c.name.clone(),
                        format!("leftover process group of dead pid {pid} did not exit"),
                        "inspect processes manually, then retry momoctl serve up",
                    ));
                    aborted = true;
                    continue;
                }
            }
            let _ = proc::remove_pidfile(&c.name);
        }

        match spawn_component(c, common) {
            Ok((child, log_path)) => {
                let pid = child.id();
                proc::write_pidfile(&c.name, pid)?;
                owned.push((c.name.clone(), child));
                let check = match &c.ready {
                    Some(url) => {
                        let (_, child) = owned.last_mut().unwrap();
                        let (ready, exited) = wait_ready(url, child);
                        if ready {
                            Check::ok(
                                c.name.clone(),
                                format!(
                                    "spawned pid {pid}, ready: {url}, log {}",
                                    log_path.display()
                                ),
                            )
                        } else {
                            let why = if exited {
                                "exited before ready"
                            } else {
                                "readiness timeout"
                            };
                            Check::fail(
                                c.name.clone(),
                                format!("spawned pid {pid} but {why}: {url}"),
                                format!("see {}", log_path.display()),
                            )
                        }
                    }
                    None => Check::ok(
                        c.name.clone(),
                        format!("spawned pid {pid}, log {}", log_path.display()),
                    ),
                };
                if check.status == CheckStatus::Fail {
                    aborted = true;
                }
                checks.push(check);
            }
            Err(e) => {
                checks.push(Check::fail(
                    c.name.clone(),
                    format!("spawn failed: {e}"),
                    "check cmd/cwd; for momod use --app-bin to point at the binary",
                ));
                aborted = true;
            }
        }
    }

    // readiness/spawn 失败 → 停掉本次 owned children，attached providers 保留。
    // kill_component 按进程组/后代链 整树强杀：wrapper（uv run）已退出时也能
    // 收掉残留的孙进程（#149），child.kill() 对这种情况是 no-op。
    if aborted {
        for (name, mut child) in owned.drain(..) {
            let _ = proc::kill_component(&name, child.id());
            let _ = child.wait();
            let _ = proc::remove_pidfile(&name);
        }
    } else {
        // 标记 active（switch/serve status 读）。
        if let Some(parent) = proc::active_profile_file().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(proc::active_profile_file(), format!("{profile}\n"));
    }

    Ok((
        checks,
        if aborted {
            ExitCode::Runtime as i32
        } else {
            ExitCode::Ok as i32
        },
    ))
}

/// 拉起单个组件：展开 args → 叠 env（含 HF_ENDPOINT 默认注入）→ spawn。
/// stdout/stderr 重定向到 `logs/<name>.log`；返回 Child 与日志路径。
fn spawn_component(
    c: &Component,
    common: &ServeCommon,
) -> Result<(Child, std::path::PathBuf), AppError> {
    let program = if c.name == "momod" {
        common
            .app_bin
            .clone()
            .map(|p| p.display().to_string())
            .or_else(|| c.cmd.clone())
    } else {
        c.cmd.clone()
    }
    .ok_or_else(|| AppError::Config(format!("component '{}': no cmd", c.name)))?;

    let (log_path, out, err) = proc::log_stdio(&c.name)?;

    let mut cmd = Command::new(&program);
    cmd.args(ServeProfile::expand_args(c))
        .stdout(out)
        .stderr(err)
        // 子进程 detach：stdin 置空，momoctl 退出后组件继续运行。
        .stdin(std::process::Stdio::null());

    // config.md §2.5：infra 负责向 Python 服务注入 HF_ENDPOINT（默认 hf-mirror）。
    if std::env::var_os("HF_ENDPOINT").is_none() && !c.env.contains_key("HF_ENDPOINT") {
        cmd.env("HF_ENDPOINT", "https://hf-mirror.com");
    }
    for (k, v) in &c.env {
        cmd.env(k, v);
    }
    if let Some(cwd) = &c.cwd {
        let dir = env_cfg::expand_tilde(cwd);
        if !dir.is_dir() {
            return Err(AppError::Config(format!(
                "component '{}': cwd {} not found",
                c.name,
                dir.display()
            )));
        }
        cmd.current_dir(dir);
    }

    // 进程组隔离（#149）：unix 自成进程组（pgid=pid，pidfile 即组 id）；
    // windows 后代归属靠 Toolhelp ppid 快照追溯。uv run 的孙进程随之同生共死。
    proc::prepare_spawn(&mut cmd);
    match cmd.spawn() {
        Ok(child) => Ok((child, log_path)),
        Err(e) => Err(AppError::Io(std::io::Error::new(
            e.kind(),
            format!("{program}: {e}"),
        ))),
    }
}

/// external 组件探活：`probe_env` 端点 + `probe_path` 拼 URL，GET 探 2xx。
fn probe_external(c: &Component) -> Result<String, String> {
    let base = c
        .probe_env
        .as_deref()
        .and_then(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .map(|ep| ep.trim_end_matches('/').to_string());
    let url = match (base, &c.probe_path, &c.ready) {
        (Some(ep), Some(path), _) => format!("{ep}{path}"),
        (Some(ep), None, _) => ep,
        (None, _, Some(ready)) => ready.clone(),
        (None, _, None) => {
            return Err(format!(
                "{} unset",
                c.probe_env.as_deref().unwrap_or("endpoint env")
            ))
        }
    };
    let p = net::probe_get(&url);
    if p.ready() {
        Ok(format!("{url} → {}", p.describe()))
    } else {
        Err(format!("{url} → {}", p.describe()))
    }
}

/// bounded readiness：轮询 GET 至 2xx；子进程提前退出立即返回。
/// 返回 (ready, exited)。
fn wait_ready(url: &str, child: &mut Child) -> (bool, bool) {
    let start = Instant::now();
    loop {
        if net::probe_get(url).ready() {
            return (true, false);
        }
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return (false, true),
            Ok(None) => {}
        }
        if start.elapsed() >= READY_TIMEOUT {
            return (false, false);
        }
        thread::sleep(READY_POLL);
    }
}

fn render(report: &ServeReport, ctx: &super::Ctx) -> Result<(), AppError> {
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        return Ok(());
    }
    let mut w = std::io::stdout().lock();
    writeln!(w, "== {} (profile={}) ==", report.command, report.profile)?;
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
    )?;
    Ok(())
}

/// uptime 紧凑格式：`90s` → `1m30s`，`3700s` → `1h1m`。
fn fmt_uptime(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

fn split(action: &ServeAction) -> (&'static str, &ServeCommon) {
    match action {
        ServeAction::Up(c) => ("up", c),
        ServeAction::Down(c) => ("down", c),
        ServeAction::Status(c) => ("status", c),
        ServeAction::Logs(l) => ("logs", &l.common),
        ServeAction::Restart(c) => ("restart", c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// env 变量（XDG_STATE_HOME/MOMO_*）进程级共享：测试串行化。
    static LOCK: Mutex<()> = Mutex::new(());

    fn test_ctx() -> super::super::Ctx {
        super::super::Ctx {
            json: false,
            env_file: None,
        }
    }

    fn common() -> ServeCommon {
        ServeCommon {
            profile: None,
            only: vec![],
            app_bin: None,
        }
    }

    /// 隔离状态目录 + 返回其路径（XDG_STATE_HOME 约定）。
    fn state_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("momo-serve-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::env::set_var("XDG_STATE_HOME", &dir);
        dir.join("momo")
    }

    /// 组装测试 profile：`[profile]` 头 + `[[component]]` 片段序列。
    fn profile(components: &str) -> String {
        format!("[profile]\nname = \"t\"\n\n{components}")
    }

    /// 可 spawn 的 stub 组件：re-exec 当前测试二进制跑 `sleeper_entry`。
    /// 平台无关——不依赖 python3/py（windows runner 无 python3 别名）。
    /// TOML basic string 里 windows 路径反斜杠需转义。
    fn stub_component() -> String {
        let exe = std::env::current_exe().unwrap();
        format!(
            r#"[[component]]
name = "stub"
order = 10
cmd = "{}"
args = ["--exact", "cmds::serve::tests::sleeper_entry"]
env = {{ MOMO_TEST_SLEEPER = "1" }}
"#,
            exe.display().to_string().replace('\\', "\\\\")
        )
    }

    /// sleeper 入口：被 `stub_component` re-exec 时睡到被 kill；
    /// `cargo test` 正常跑到时无标记 env → 立即通过。
    #[test]
    fn sleeper_entry() {
        if std::env::var_os("MOMO_TEST_SLEEPER").is_none() {
            return;
        }
        thread::sleep(Duration::from_secs(600));
    }

    /// wrapper stub（复刻 `uv run` 包壳形态，#149）：re-exec 当前测试二进制跑
    /// `wrapper_entry`——它再 spawn 一个 sleeper 孙进程并把孙 pid 写进
    /// marker 文件。mode=`sleep` 睡到被 kill；mode=`exit` 随即退出
    /// （readiness 前退出的 abort 场景）。
    fn wrapper_component(marker: &std::path::Path, mode: &str, ready: Option<&str>) -> String {
        let exe = std::env::current_exe().unwrap();
        let ready = ready
            .map(|u| format!("ready = \"{u}\"\n"))
            .unwrap_or_default();
        format!(
            r#"[[component]]
name = "stub"
order = 10
cmd = "{}"
args = ["--exact", "cmds::serve::tests::wrapper_entry"]
{ready}env = {{ MOMO_TEST_WRAPPER = "{mode}", MOMO_TEST_GC_PID = "{}" }}
"#,
            exe.display().to_string().replace('\\', "\\\\"),
            marker.display().to_string().replace('\\', "\\\\"),
        )
    }

    /// wrapper 入口：spawn sleeper 孙进程（孙 pid 写 `MOMO_TEST_GC_PID`
    /// marker），再按 `MOMO_TEST_WRAPPER` 模式 sleep 或退出；
    /// 无标记 env → 立即通过。
    // zombie_processes：孙进程就是要被"父死/组杀"的孤儿，绝不 wait——
    // wait 了就不是测试目标了。
    #[allow(clippy::zombie_processes)]
    #[test]
    fn wrapper_entry() {
        let mode = std::env::var("MOMO_TEST_WRAPPER").unwrap_or_default();
        if mode.is_empty() {
            return;
        }
        // 孙进程归属靠 unix 进程组继承 / windows ppid 追溯，无生时序要求。
        let exe = std::env::current_exe().unwrap();
        let gc = Command::new(exe)
            .args(["--exact", "cmds::serve::tests::sleeper_entry"])
            .env("MOMO_TEST_SLEEPER", "1")
            .spawn()
            .unwrap();
        if let Ok(path) = std::env::var("MOMO_TEST_GC_PID") {
            let _ = fs::write(path, gc.id().to_string());
        }
        if mode == "exit" {
            return;
        }
        thread::sleep(Duration::from_secs(600));
    }

    /// 轮询 marker 文件读孙进程 pid（bounded 10s）。
    fn wait_reported_pid(marker: &std::path::Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = fs::read_to_string(marker) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    return pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "wrapper did not report grandchild pid"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// 等 pid 死透（bounded 10s；孤儿 reparent 后收尸有延迟，poll 兜住）。
    fn wait_pid_dead(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while proc::is_alive(pid) {
            assert!(Instant::now() < deadline, "pid {pid} still alive");
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn up_spawns_pidfile_and_log() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = state_dir("spawn");
        let doc = ServeProfile::parse(&profile(&stub_component())).unwrap();
        let code = up(&doc, &common(), "t", &test_ctx()).unwrap();
        assert_eq!(code, 0);

        let pid = proc::read_pidfile("stub").unwrap();
        assert!(proc::is_alive(pid), "spawned process must be alive");
        assert!(state.join("logs/stub.log").exists());
        assert!(state.join("active_profile").exists());

        // 幂等：再次 up → already running，不重复拉起
        let code = up(&doc, &common(), "t", &test_ctx()).unwrap();
        assert_eq!(code, 0);
        assert_eq!(proc::read_pidfile("stub").unwrap(), pid);

        assert!(proc::kill_pid(pid));
        let _ = proc::remove_pidfile("stub");
        std::env::remove_var("XDG_STATE_HOME");
        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    #[test]
    fn failed_external_probe_stops_owned_children() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = state_dir("cleanup");
        // stub 先拉起，随后的 external 探活指向无监听端口 → fail → 清理 stub。
        std::env::set_var("MOMO_TEST_EP", "http://127.0.0.1:1");
        let toml = profile(&format!(
            r#"{stub}

[[component]]
name = "brain"
order = 20
external = true
probe_env = "MOMO_TEST_EP"
probe_path = "/models"
"#,
            stub = stub_component()
        ));
        let doc = ServeProfile::parse(&toml).unwrap();
        let code = up(&doc, &common(), "t", &test_ctx()).unwrap();
        assert_eq!(code, ExitCode::Runtime as i32);

        // 本次 spawn 的 stub 已被回收：pidfile 删除、进程终止。
        assert!(proc::read_pidfile("stub").is_err());
        std::env::remove_var("MOMO_TEST_EP");
        std::env::remove_var("XDG_STATE_HOME");
        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    #[test]
    fn only_unknown_component_is_usage_error() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let doc = ServeProfile::parse(&profile(&stub_component())).unwrap();
        let c = ServeCommon {
            profile: None,
            only: vec!["nope".into()],
            app_bin: None,
        };
        assert!(up(&doc, &c, "t", &test_ctx()).is_err());
    }

    #[test]
    fn local_endpoint_only_skipped_off_loopback() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = state_dir("skip");
        std::env::set_var("MOMO_LLM_ENDPOINT", "https://api.deepseek.com/v1");
        let toml = profile(&format!(
            r#"[[component]]
name = "llama-server"
order = 5
local_endpoint_only = true
cmd = "llama-server-nonexistent-bin"

{stub}"#,
            stub = stub_component()
        ));
        let doc = ServeProfile::parse(&toml).unwrap();
        let code = up(&doc, &common(), "t", &test_ctx()).unwrap();
        assert_eq!(code, 0);
        // llama-server 跳过未拉起，stub 正常 spawn
        assert!(proc::read_pidfile("llama-server").is_err());
        let pid = proc::read_pidfile("stub").unwrap();
        assert!(proc::kill_pid(pid));
        std::env::remove_var("MOMO_LLM_ENDPOINT");
        std::env::remove_var("XDG_STATE_HOME");
        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    #[test]
    fn down_is_idempotent_noop() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = state_dir("down-noop");
        let doc = ServeProfile::parse(&profile(&stub_component())).unwrap();
        // 未运行即 down：no-op 成功，再次 down 同样成功。
        assert_eq!(down(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        assert_eq!(down(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        std::env::remove_var("XDG_STATE_HOME");
        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    #[test]
    fn down_stops_running_component_and_cleans_state() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = state_dir("down-stop");
        let doc = ServeProfile::parse(&profile(&stub_component())).unwrap();
        assert_eq!(up(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        let pid = proc::read_pidfile("stub").unwrap();

        assert_eq!(down(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        assert!(!proc::is_alive(pid), "killed process must be dead");
        assert!(proc::read_pidfile("stub").is_err(), "pidfile removed");
        assert!(
            !state.join("active_profile").exists(),
            "full down clears active_profile"
        );
        std::env::remove_var("XDG_STATE_HOME");
        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    #[test]
    fn down_removes_stale_pidfile() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = state_dir("down-stale");
        let doc = ServeProfile::parse(&profile(&stub_component())).unwrap();
        // 写一个不可能存活的 pid → down 应清掉 pidfile 且成功。
        proc::write_pidfile("stub", 4_000_000).unwrap();
        assert_eq!(down(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        assert!(proc::read_pidfile("stub").is_err());
        std::env::remove_var("XDG_STATE_HOME");
        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    #[test]
    fn status_reports_stopped_running_and_stale() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = state_dir("status");
        let doc = ServeProfile::parse(&profile(&stub_component())).unwrap();

        // 未运行：stopped → exit 0
        assert_eq!(status(&doc, &common(), "t", &test_ctx()).unwrap(), 0);

        // 运行中：running → exit 0
        assert_eq!(up(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        assert_eq!(status(&doc, &common(), "t", &test_ctx()).unwrap(), 0);

        // pidfile 指向死进程：stale → exit 3
        let pid = proc::read_pidfile("stub").unwrap();
        assert!(proc::stop_pid(pid, Duration::from_secs(5)));
        assert_eq!(
            status(&doc, &common(), "t", &test_ctx()).unwrap(),
            ExitCode::CheckFailed as i32
        );

        // down 清理后回到 stopped
        assert_eq!(down(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        assert_eq!(status(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        std::env::remove_var("XDG_STATE_HOME");
        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    #[test]
    fn restart_replaces_running_component() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = state_dir("restart");
        let doc = ServeProfile::parse(&profile(&stub_component())).unwrap();
        assert_eq!(up(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        let old_pid = proc::read_pidfile("stub").unwrap();

        assert_eq!(restart(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        let new_pid = proc::read_pidfile("stub").unwrap();
        assert_ne!(old_pid, new_pid, "restart spawns fresh process");
        assert!(proc::is_alive(new_pid));

        assert_eq!(down(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        std::env::remove_var("XDG_STATE_HOME");
        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    /// #149 回归：只杀 pidfile 记的 wrapper（uv run）孙进程成孤儿持端口，
    /// 但仍在 spawn 时的进程组/后代链 内；`serve down` 按组收口连孤儿一起停。
    #[test]
    fn down_reaps_orphaned_grandchild() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = state_dir("group-orphan");
        let marker = std::env::temp_dir().join(format!("momo-gc-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let doc =
            ServeProfile::parse(&profile(&wrapper_component(&marker, "sleep", None))).unwrap();
        assert_eq!(up(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        let pid = proc::read_pidfile("stub").unwrap();
        let gc = wait_reported_pid(&marker);

        // 只杀 wrapper（旧 down 行为）：孙进程泄漏，但组/后代链 仍能定位它。
        assert!(proc::kill_pid(pid));
        wait_pid_dead(pid);
        assert!(proc::is_alive(gc), "orphaned grandchild must still run");
        assert!(
            proc::component_alive("stub", pid),
            "process group/descendants still active after wrapper exit"
        );

        assert_eq!(down(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        wait_pid_dead(gc);
        assert!(!proc::component_alive("stub", pid));
        assert!(proc::read_pidfile("stub").is_err());

        std::env::remove_var("XDG_STATE_HOME");
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    /// #149 回归：wrapper 在 readiness 前退出 → abort 清理须连孤儿孙进程
    /// 一起杀（旧行为 `child.kill()` 对已退出 wrapper 是 no-op，孙进程泄漏）。
    #[test]
    fn abort_reaps_group_after_wrapper_exit() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = state_dir("group-abort");
        let marker = std::env::temp_dir().join(format!("momo-gc-abort-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        // ready 探针指向无监听端口；wrapper 提前退出 → exited before ready → abort。
        let toml = profile(&wrapper_component(
            &marker,
            "exit",
            Some("http://127.0.0.1:1/readyz"),
        ));
        let doc = ServeProfile::parse(&toml).unwrap();
        assert_eq!(
            up(&doc, &common(), "t", &test_ctx()).unwrap(),
            ExitCode::Runtime as i32
        );

        let gc = wait_reported_pid(&marker);
        wait_pid_dead(gc);
        assert!(
            proc::read_pidfile("stub").is_err(),
            "pidfile removed on abort"
        );

        std::env::remove_var("XDG_STATE_HOME");
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    /// #149 回归：pidfile 指向死 wrapper 但进程组/后代链 残留（历史泄漏的孤儿
    /// 持端口）→ 下次 `serve up` 先收孤儿再拉起，而不是 stale 一删了之。
    #[test]
    fn up_reaps_orphan_group_before_respawn() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = state_dir("group-reap");
        let marker = std::env::temp_dir().join(format!("momo-gc-reap-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let doc =
            ServeProfile::parse(&profile(&wrapper_component(&marker, "sleep", None))).unwrap();
        assert_eq!(up(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        let pid = proc::read_pidfile("stub").unwrap();
        let gc = wait_reported_pid(&marker);

        // 模拟旧版泄漏：只杀 wrapper，孤儿孙进程留在组内。
        assert!(proc::kill_pid(pid));
        wait_pid_dead(pid);
        assert!(proc::is_alive(gc));

        // 再次 up：先收孤儿再拉起新实例。
        assert_eq!(up(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        wait_pid_dead(gc);
        let new_pid = proc::read_pidfile("stub").unwrap();
        assert_ne!(new_pid, pid, "respawn writes fresh pidfile");
        assert!(proc::is_alive(new_pid));

        assert_eq!(down(&doc, &common(), "t", &test_ctx()).unwrap(), 0);
        std::env::remove_var("XDG_STATE_HOME");
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    #[test]
    fn read_last_lines_tails_file() {
        let dir = std::env::temp_dir().join(format!("momo-logs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.log");
        fs::write(&p, "l1\nl2\nl3\nl4\n").unwrap();
        assert_eq!(read_last_lines(&p, 2).unwrap(), vec!["l3", "l4"]);
        // n 超过行数 → 全量返回
        assert_eq!(
            read_last_lines(&p, 99).unwrap(),
            vec!["l1", "l2", "l3", "l4"]
        );
        // 无尾换行
        fs::write(&p, "a\nb").unwrap();
        assert_eq!(read_last_lines(&p, 1).unwrap(), vec!["b"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
