//! 训练 pipeline 定义与编排（issue #41；momo-docs `02-repos/momo-infra.md`
//! 「训练 pipeline 编排」）。infra 只编排不训练：step 串行执行
//! `exec = local | ssh | register`，命令只声明名称/执行方式/产物路径/重试策略，
//! 训练实体仍是 momo-models `training/`、`eval/` 脚本。
//!
//! run 状态目录：`state_dir()/pipeline-runs/<pipeline>/<run_id>/`
//! （run_id = ULID，字典序即时间序）。`run.json` 聚合状态 + `steps/<step>.json`
//! per-step artifact manifest（step/artifact/metrics/timestamp），tmp+rename 原子写。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::env_cfg;
use crate::manifest::{sha256_file, LockEntry, LockFile, Manifest, ModelsLock};
use crate::output::AppError;
use crate::serve::proc::state_dir;

// ---------- 定义层：infra/pipelines/*.toml ----------

/// pipeline 定义（`infra/pipelines/<name>.toml`）。
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// 产物落点（相对 `MOMO_MODELS_DIR`，支持 `{date}` 占位）。
    pub artifact_dir: String,
    /// 产物激活 env 键（可选）：promote 通过把该键写回 `.env.<profile>`
    /// 指向 artifact_dir 完成激活（voice-clone.md「只改 env」灰度口径；
    /// `.bak` 即回滚目标）。未声明 → promote 只迁移状态不碰 env。
    #[serde(default)]
    pub activate_env: Option<String>,
    #[serde(default)]
    pub vars: BTreeMap<String, VarDef>,
    #[serde(rename = "step", default)]
    pub steps: Vec<StepDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VarDef {
    #[serde(default)]
    pub description: String,
    /// `--set` 未注入时 run 直接拒绝（exit 2）。
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExecKind {
    #[default]
    Local,
    Ssh,
    /// 内建产物登记步（无 command）：train-manifest 校验 + MANIFEST 条目 + models.lock。
    Register,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StepDef {
    pub name: String,
    #[serde(default)]
    pub exec: ExecKind,
    /// `exec = "ssh"` 必填；支持 `{var:}` 占位。
    #[serde(default)]
    pub ssh_host: Option<String>,
    /// local：相对 repo 根（绝对路径原样用）；ssh：远端路径字面量。
    #[serde(default)]
    pub workdir: Option<String>,
    /// exec=register 时为空。
    #[serde(default)]
    pub command: Option<String>,
    /// step 产物路径（可选）：per-step manifest 记录 + `<dir>/metrics.json` 拾取。
    #[serde(default)]
    pub artifact: Option<String>,
    /// 失败额外重试次数（默认 0）。
    #[serde(default)]
    pub retries: u32,
}

impl PipelineDef {
    pub fn load(path: &Path) -> Result<Self, AppError> {
        let text = fs::read_to_string(path)
            .map_err(|e| AppError::Config(format!("read {}: {e}", path.display())))?;
        let def: PipelineDef = toml::from_str(&text)
            .map_err(|e| AppError::Config(format!("parse {}: {e}", path.display())))?;
        def.validate()?;
        Ok(def)
    }

    fn validate(&self) -> Result<(), AppError> {
        if self.name.trim().is_empty() {
            return Err(AppError::Config("pipeline with empty name".into()));
        }
        if self.artifact_dir.trim().is_empty() {
            return Err(AppError::Config(format!(
                "pipeline '{}': empty artifact_dir",
                self.name
            )));
        }
        if self.steps.is_empty() {
            return Err(AppError::Config(format!(
                "pipeline '{}': no steps",
                self.name
            )));
        }
        for s in &self.steps {
            if s.name.trim().is_empty() {
                return Err(AppError::Config(format!(
                    "pipeline '{}': step with empty name",
                    self.name
                )));
            }
            match s.exec {
                ExecKind::Register => {}
                ExecKind::Local | ExecKind::Ssh => {
                    if s.command.as_deref().is_none_or(|c| c.trim().is_empty()) {
                        return Err(AppError::Config(format!(
                            "pipeline '{}': step '{}': exec {:?} requires command",
                            self.name, s.name, s.exec
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// pipeline 定义目录探测候选（cwd 相对）：infra/ 内、repo 根、infra/ 兄弟目录。
const PIPELINES_CANDIDATES: &[&str] = &[
    "infra/pipelines",
    "pipelines",
    "../infra/pipelines",
    "../momo-infra/pipelines",
];

pub fn default_pipelines_dir() -> Option<PathBuf> {
    PIPELINES_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}

/// 定义目录下全部 pipeline（按文件名排序）。
pub fn discover(dir: &Path) -> Result<Vec<(PathBuf, PipelineDef)>, AppError> {
    let mut out = Vec::new();
    let rd = fs::read_dir(dir)
        .map_err(|e| AppError::Config(format!("read pipelines dir {}: {e}", dir.display())))?;
    for ent in rd.flatten() {
        let path = ent.path();
        if path.extension().is_some_and(|e| e == "toml") {
            let def = PipelineDef::load(&path)?;
            out.push((path, def));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

// ---------- run 状态（持久化 JSON） ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::Running => "running",
            StepStatus::Succeeded => "succeeded",
            StepStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
        }
    }
}

/// per-step artifact manifest：`steps/<name>.json` 与 run.json 内嵌同构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepState {
    pub name: String,
    pub status: StepStatus,
    #[serde(default)]
    pub attempts: u32,
    /// 展开后的执行描述（本地命令 / ssh 行 / register 标记）。
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub artifact: Option<String>,
    #[serde(default)]
    pub metrics: serde_json::Value,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub pipeline: String,
    pub run_id: String,
    pub status: RunStatus,
    pub date: String,
    pub artifact_dir: String,
    pub run_dir: String,
    pub dry_run: bool,
    pub started_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    pub steps: Vec<StepState>,
}

/// 原子 JSON 写（tmp + rename；Windows rename 不覆盖先删）。
fn save_json(path: &Path, value: &impl Serialize) -> Result<(), AppError> {
    let tmp = path.with_extension("tmp");
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| AppError::Config(format!("serialize {}: {e}", path.display())))?;
    fs::write(&tmp, text)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load_run(run_dir: &Path) -> Result<RunState, AppError> {
    let path = run_dir.join("run.json");
    let text = fs::read_to_string(&path)
        .map_err(|e| AppError::Config(format!("read {}: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| AppError::Config(format!("parse {}: {e}", path.display())))
}

/// pipeline 的全部 run（按 run_id 升序 = 时间序）。
pub fn list_runs(runs_root: &Path, pipeline: &str) -> Vec<PathBuf> {
    let dir = runs_root.join(pipeline);
    let mut out: Vec<PathBuf> = fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.join("run.json").is_file())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

// ---------- 占位符展开 ----------

/// `{name}` / `{var:name}` 展开；未知占位直接报 Config 错（拼写失败好过静默跑错命令）。
fn expand(
    text: &str,
    vars: &BTreeMap<String, String>,
    scope: &BTreeMap<String, String>,
) -> Result<String, AppError> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(AppError::Config(format!(
                "unclosed placeholder in {text:?}"
            )));
        };
        let key = &after[..close];
        let val = if let Some(name) = key.strip_prefix("var:") {
            vars.get(name).cloned().ok_or_else(|| {
                AppError::Config(format!(
                    "pipeline var '{name}' not set (use --set {name}=...)"
                ))
            })?
        } else {
            scope.get(key).cloned().ok_or_else(|| {
                AppError::Config(format!("unknown placeholder {{{key}}} in {text:?}"))
            })?
        };
        out.push_str(&val);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

// ---------- 执行抽象 ----------

/// 已展开的 step 执行请求（register 不经此路径）。
#[derive(Debug)]
pub struct ExecSpec {
    pub kind: ExecKind,
    /// 人类可读完整描述（ssh 含 host，本地含 workdir）。
    pub display: String,
    /// 本地 shell 命令 + 工作目录（绝对）。
    pub local: Option<(PathBuf, String)>,
    /// ssh：host + 远端 bash -lc 载荷（已拼 cd）。
    pub ssh: Option<(String, String)>,
}

/// step 执行器；返回进程退出码。真实实现落 stdout/stderr 到 log 文件。
pub trait Executor {
    fn run(&self, spec: &ExecSpec, log: &Path) -> Result<i32, AppError>;
}

/// exec_local：平台 shell；exec_ssh：`ssh <host> bash -lc '<cmd>'`（非交互）。
pub struct ShellExecutor;

impl Executor for ShellExecutor {
    fn run(&self, spec: &ExecSpec, log: &Path) -> Result<i32, AppError> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| {
                AppError::Io(std::io::Error::new(
                    e.kind(),
                    format!("{}: {e}", log.display()),
                ))
            })?;
        let stderr = file.try_clone()?;
        let mut cmd = if let Some((wd, command)) = &spec.local {
            let mut c = shell_command(command);
            c.current_dir(wd);
            c
        } else if let Some((host, payload)) = &spec.ssh {
            let mut c = Command::new("ssh");
            // ssh 把 host 后的参数以空格拼接交远端 shell：bash -lc '<quoted>'
            c.arg(host)
                .arg(format!("bash -lc {}", shell_quote(payload)));
            c
        } else {
            return Err(AppError::Config(
                "exec spec with neither local nor ssh".into(),
            ));
        };
        let status = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(stderr))
            .status()
            .map_err(|e| AppError::Config(format!("spawn {}: {e}", spec.display)))?;
        Ok(status.code().unwrap_or(-1))
    }
}

/// `--dry-run`：不执行，记录计划命令并返回 0。
pub struct DryRunExecutor;

impl Executor for DryRunExecutor {
    fn run(&self, _spec: &ExecSpec, _log: &Path) -> Result<i32, AppError> {
        Ok(0)
    }
}

#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(command);
    c
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut c = Command::new("cmd");
    c.arg("/C").arg(command);
    c
}

/// 单引号包裹（`'` → `'\''`）：远端 `bash -lc` 载荷安全字面量化。
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------- runner ----------

/// 一次 run/resume 的全部外部依赖（测试注入隔离目录，不碰 env）。
pub struct RunOpts {
    /// `${XDG_STATE_HOME:-~/.local/state}/momo/pipeline-runs`
    pub runs_root: PathBuf,
    /// `MOMO_MODELS_DIR`
    pub models_dir: PathBuf,
    /// repo 根（pipeline 定义目录的两级父目录 / `--dir` 显式给时同上解析）
    pub repo_root: PathBuf,
    /// register step 追加条目的 MANIFEST.toml
    pub manifest_path: PathBuf,
    /// 代际日期 yyyymmdd（`--date` 覆盖，缺省当日 UTC）
    pub date: String,
    pub dry_run: bool,
    /// `--set KEY=VALUE`
    pub vars: BTreeMap<String, String>,
}

pub fn runs_root() -> PathBuf {
    state_dir().join("pipeline-runs")
}

/// 当日 UTC `yyyymmdd`。
pub fn today_yyyymmdd() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d, _, _, _) = unix_to_civil(secs as i64);
    format!("{y:04}{m:02}{d:02}")
}

fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = unix_to_civil(secs as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Unix 秒 → UTC 民用日期（Howard Hinnant 算法，不引 chrono）。
fn unix_to_civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32, h as u32, mi as u32, s as u32)
}

/// 已合并变量 + 占位符 scope 的一次 run 展开视图。
struct Resolved {
    vars: BTreeMap<String, String>,
    scope: BTreeMap<String, String>,
}

impl Resolved {
    fn text(&self, s: &str) -> Result<String, AppError> {
        expand(s, &self.vars, &self.scope)
    }
}

fn resolve(
    def: &PipelineDef,
    opts: &RunOpts,
    run_id: &str,
    run_dir: &Path,
) -> Result<Resolved, AppError> {
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    for (name, v) in &def.vars {
        if let Some(d) = &v.default {
            vars.insert(name.clone(), d.clone());
        }
    }
    for (k, v) in &opts.vars {
        vars.insert(k.clone(), v.clone());
    }
    let missing: Vec<&str> = def
        .vars
        .iter()
        .filter(|(name, v)| v.required && !vars.contains_key(*name))
        .map(|(name, _)| name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(AppError::Config(format!(
            "missing required var(s) for pipeline '{}': {}; pass --set <name>=<value>",
            def.name,
            missing.join(", ")
        )));
    }
    let artifact_rel = expand(
        &def.artifact_dir,
        &vars,
        &BTreeMap::from([("date".into(), opts.date.clone())]),
    )?;
    let artifact_dir = opts.models_dir.join(&artifact_rel);
    let scope = BTreeMap::from([
        ("date".into(), opts.date.clone()),
        ("run_id".into(), run_id.to_string()),
        ("run_dir".into(), run_dir.display().to_string()),
        ("models_dir".into(), opts.models_dir.display().to_string()),
        ("artifact_dir".into(), artifact_dir.display().to_string()),
        ("artifact_rel".into(), artifact_rel.clone()),
        ("repo_root".into(), opts.repo_root.display().to_string()),
    ]);
    Ok(Resolved { vars, scope })
}

fn step_spec(
    def: &PipelineDef,
    step: &StepDef,
    resolved: &Resolved,
    opts: &RunOpts,
) -> Result<ExecSpec, AppError> {
    match step.exec {
        ExecKind::Register => Ok(ExecSpec {
            kind: ExecKind::Register,
            display: format!("register {}", resolved.scope["artifact_dir"]),
            local: None,
            ssh: None,
        }),
        ExecKind::Local => {
            let command = resolved.text(step.command.as_deref().unwrap_or_default())?;
            let wd = match &step.workdir {
                Some(w) => {
                    let w = resolved.text(w)?;
                    let p = PathBuf::from(&w);
                    if p.is_absolute() {
                        p
                    } else {
                        opts.repo_root.join(p)
                    }
                }
                None => opts.repo_root.clone(),
            };
            Ok(ExecSpec {
                kind: ExecKind::Local,
                display: format!("(cd {} && {command})", wd.display()),
                local: Some((wd, command)),
                ssh: None,
            })
        }
        ExecKind::Ssh => {
            let host_raw = step.ssh_host.as_deref().ok_or_else(|| {
                AppError::Config(format!(
                    "pipeline '{}': step '{}': exec ssh requires ssh_host",
                    def.name, step.name
                ))
            })?;
            let host = resolved.text(host_raw)?;
            let command = resolved.text(step.command.as_deref().unwrap_or_default())?;
            let payload = match &step.workdir {
                Some(w) => format!("cd {} && {command}", resolved.text(w)?),
                None => command,
            };
            Ok(ExecSpec {
                kind: ExecKind::Ssh,
                display: format!("ssh {host} bash -lc {}", shell_quote(&payload)),
                local: None,
                ssh: Some((host, payload)),
            })
        }
    }
}

fn write_state(run: &RunState, persist: bool) -> Result<(), AppError> {
    if !persist {
        return Ok(());
    }
    save_json(&PathBuf::from(&run.run_dir).join("run.json"), run)
}

fn write_step(run_dir: &Path, state: &StepState, persist: bool) -> Result<(), AppError> {
    if !persist {
        return Ok(());
    }
    let dir = run_dir.join("steps");
    fs::create_dir_all(&dir)?;
    save_json(&dir.join(format!("{}.json", state.name)), state)
}

/// 新 run：全部 step 从 pending 起跑。
pub fn run_pipeline(
    def: &PipelineDef,
    opts: &RunOpts,
    exec: &dyn Executor,
) -> Result<RunState, AppError> {
    let run_id = ulid::Ulid::new().to_string().to_lowercase();
    let run_dir = opts.runs_root.join(&def.name).join(&run_id);
    let resolved = resolve(def, opts, &run_id, &run_dir)?;
    let artifact_dir = resolved.scope["artifact_dir"].clone();
    let persist = !opts.dry_run;
    if persist {
        fs::create_dir_all(run_dir.join("logs"))?;
    }
    let steps: Vec<StepState> = def
        .steps
        .iter()
        .map(|s| {
            let spec = step_spec(def, s, &resolved, opts)?;
            Ok(StepState {
                name: s.name.clone(),
                status: StepStatus::Pending,
                attempts: 0,
                command: Some(spec.display),
                artifact: s
                    .artifact
                    .as_deref()
                    .map(|a| resolved.text(a))
                    .transpose()?,
                metrics: serde_json::json!({}),
                started_at: None,
                finished_at: None,
                error: None,
            })
        })
        .collect::<Result<_, AppError>>()?;
    let mut run = RunState {
        pipeline: def.name.clone(),
        run_id,
        status: RunStatus::Running,
        date: opts.date.clone(),
        artifact_dir,
        run_dir: run_dir.display().to_string(),
        dry_run: opts.dry_run,
        started_at: now_iso(),
        finished_at: None,
        vars: resolved.vars.clone(),
        steps,
    };
    write_state(&run, persist)?;
    drive(&mut run, def, opts, exec, &resolved, persist)
}

/// 续跑：跳过 succeeded，首个 failed/pending（含崩溃留 running）起步。
pub fn resume_pipeline(
    def: &PipelineDef,
    opts: &RunOpts,
    run_dir: &Path,
    exec: &dyn Executor,
) -> Result<RunState, AppError> {
    let mut run = load_run(run_dir)?;
    if run.pipeline != def.name {
        return Err(AppError::Config(format!(
            "run {} belongs to pipeline '{}', not '{}'",
            run.run_id, run.pipeline, def.name
        )));
    }
    if run.status == RunStatus::Succeeded {
        return Err(AppError::Config(format!(
            "run {} already succeeded; nothing to resume",
            run.run_id
        )));
    }
    // 续跑沿用首跑的代际日期与 --set 变量（存储值为准，新 --set 可覆盖同名项）。
    let mut vars = run.vars.clone();
    vars.extend(opts.vars.iter().map(|(k, v)| (k.clone(), v.clone())));
    let effective = RunOpts {
        runs_root: opts.runs_root.clone(),
        models_dir: opts.models_dir.clone(),
        repo_root: opts.repo_root.clone(),
        manifest_path: opts.manifest_path.clone(),
        date: run.date.clone(),
        dry_run: opts.dry_run,
        vars,
    };
    let resolved = resolve(def, &effective, &run.run_id, run_dir)?;
    let persist = !opts.dry_run && !run.dry_run;
    // 中途崩溃留下的 running step 视为 failed，续跑重跑该步。
    for s in &mut run.steps {
        if s.status == StepStatus::Running {
            s.status = StepStatus::Failed;
        }
    }
    run.status = RunStatus::Running;
    run.dry_run = false;
    write_state(&run, persist)?;
    drive(&mut run, def, &effective, exec, &resolved, persist)
}

/// 串行驱动：首个非 succeeded step 起逐段执行，任何一步失败即停。
fn drive(
    run: &mut RunState,
    def: &PipelineDef,
    opts: &RunOpts,
    exec: &dyn Executor,
    resolved: &Resolved,
    persist: bool,
) -> Result<RunState, AppError> {
    let run_dir = PathBuf::from(&run.run_dir);
    for (i, step) in def.steps.iter().enumerate() {
        if run.steps[i].status == StepStatus::Succeeded {
            continue;
        }
        let spec = step_spec(def, step, resolved, opts)?;
        {
            let state = &mut run.steps[i];
            state.status = StepStatus::Running;
            // 重试预算按「本次 drive」计：resume 不累计上次 run 的尝试次数，
            // 否则 failed step 一续跑即耗尽预算直接判负。
            state.attempts = 0;
            state.started_at = Some(now_iso());
            state.error = None;
            write_step(&run_dir, state, persist)?;
        }
        write_state(run, persist)?;

        let max_attempts = 1 + step.retries;
        let mut last_err = String::new();
        let mut ok = false;
        while run.steps[i].attempts < max_attempts && !ok {
            run.steps[i].attempts += 1;
            let attempt = run.steps[i].attempts;
            let log = run_dir
                .join("logs")
                .join(format!("{}-attempt{attempt}.log", step.name));
            let result = if step.exec == ExecKind::Register {
                if opts.dry_run {
                    // dry-run 不碰产物目录/MANIFEST/models.lock，只记录计划。
                    run.steps[i].metrics = serde_json::json!({
                        "dry_run": true,
                        "would_register": resolved.scope["artifact_rel"],
                    });
                    Ok(0)
                } else {
                    register(def, step, resolved, opts).map(|m| {
                        run.steps[i].metrics = m;
                        0
                    })
                }
            } else {
                exec.run(&spec, &log).inspect(|&code| {
                    if code != 0 {
                        last_err = format!("exit {code} (see {})", log.display());
                    }
                })
            };
            match result {
                Ok(0) => ok = true,
                Ok(_) => {}
                Err(e) => last_err = e.to_string(),
            }
        }

        {
            let state = &mut run.steps[i];
            state.finished_at = Some(now_iso());
            if ok {
                state.status = StepStatus::Succeeded;
                if step.exec != ExecKind::Register {
                    state.metrics = step_metrics(state.artifact.as_deref());
                }
            } else {
                state.status = StepStatus::Failed;
                state.error = Some(if last_err.is_empty() {
                    "step failed".into()
                } else {
                    last_err.clone()
                });
            }
            write_step(&run_dir, state, persist)?;
        }
        if !ok {
            run.status = RunStatus::Failed;
        }
        write_state(run, persist)?;
        if !ok {
            run.finished_at = Some(now_iso());
            write_state(run, persist)?;
            return Ok(run.clone());
        }
    }
    run.status = RunStatus::Succeeded;
    run.finished_at = Some(now_iso());
    write_state(run, persist)?;
    Ok(run.clone())
}

/// step 产物 metrics 拾取：`<artifact>/metrics.json` 若为 JSON object 则原样采纳。
fn step_metrics(artifact: Option<&str>) -> serde_json::Value {
    let Some(a) = artifact else {
        return serde_json::json!({});
    };
    let path = Path::new(a).join("metrics.json");
    fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

// ---------- register step：train-manifest 校验 + MANIFEST 条目 + models.lock ----------

/// `train-manifest.json` 期望字段（05-ops/borrowed-etiquette.md：基座版本/数据量/lr/steps/loss）。
const TRAIN_MANIFEST_KEYS: &[&str] = &["base_revision", "data_count", "lr", "steps", "loss"];

fn register(
    def: &PipelineDef,
    _step: &StepDef,
    resolved: &Resolved,
    opts: &RunOpts,
) -> Result<serde_json::Value, AppError> {
    let artifact_dir = PathBuf::from(&resolved.scope["artifact_dir"]);
    let artifact_rel = resolved.scope["artifact_rel"].clone();
    if !artifact_dir.is_dir() {
        return Err(AppError::Config(format!(
            "register: artifact dir missing: {}",
            artifact_dir.display()
        )));
    }
    let train_manifest = artifact_dir.join("train-manifest.json");
    if !train_manifest.is_file() {
        return Err(AppError::Config(format!(
            "register: train-manifest.json missing in {}",
            artifact_dir.display()
        )));
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&train_manifest).map_err(|e| {
            AppError::Io(std::io::Error::new(
                e.kind(),
                format!("{}: {e}", train_manifest.display()),
            ))
        })?)
        .map_err(|e| AppError::Config(format!("register: {}: {e}", train_manifest.display())))?;
    let mut warnings: Vec<String> = Vec::new();
    match parsed.as_object() {
        Some(obj) => {
            for k in TRAIN_MANIFEST_KEYS {
                if !obj.contains_key(*k) {
                    warnings.push(format!("train-manifest.json missing key '{k}'"));
                }
            }
        }
        None => {
            return Err(AppError::Config(
                "register: train-manifest.json is not a JSON object".into(),
            ))
        }
    }

    // 产物逐文件 size+sha256（models.lock / MANIFEST files 同口径）。
    let mut files: Vec<LockFile> = Vec::new();
    collect_lock_files(&artifact_dir, &artifact_dir, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    if files.is_empty() {
        return Err(AppError::Config(format!(
            "register: no files under {}",
            artifact_dir.display()
        )));
    }
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let name = artifact_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| def.name.clone());
    let repo_id = artifact_rel.clone();
    let revision = opts.date.clone();

    // 1) MANIFEST.toml 追加 [[model]]（name 冲突即拒绝，先合并文本再整体验证）。
    let manifest_text = fs::read_to_string(&opts.manifest_path)
        .map_err(|e| AppError::Manifest(format!("read {}: {e}", opts.manifest_path.display())))?;
    if Manifest::parse(&manifest_text)?
        .models
        .iter()
        .any(|m| m.name == name)
    {
        return Err(AppError::Manifest(format!(
            "register: MANIFEST already has entry '{name}' (same-day rerun? pick a new --date)"
        )));
    }
    let mut entry = format!(
        "\n[[model]]\nname = {name:?}\nrepo_id = {repo_id:?}\nrevision = {revision:?}\ndest = {repo_id:?}\nnotes = {:?}\nfiles = [\n",
        format!("{} artifact (pipeline {} run {})", artifact_rel, def.name, resolved.scope["run_id"]),
    );
    for f in &files {
        entry.push_str(&format!(
            "  {{ path = {:?}, sha256 = {:?}, size = {} }},\n",
            f.path, f.sha256, f.size
        ));
    }
    entry.push_str("]\n");
    let merged = format!("{manifest_text}{entry}");
    Manifest::parse(&merged).map_err(|e| {
        AppError::Manifest(format!("register: generated MANIFEST entry invalid: {e}"))
    })?;
    atomic_write(&opts.manifest_path, &merged)?;

    // 2) models.lock upsert（repo = <dest>@<revision>，同 pull 写入格式）。
    let lock_path = opts.models_dir.join("models.lock");
    let mut lock = ModelsLock::load(&lock_path)?;
    lock.upsert(LockEntry {
        repo: format!("{repo_id}@{revision}"),
        name: name.clone(),
        files,
    });
    lock.save(&lock_path)?;

    Ok(serde_json::json!({
        "entry": name,
        "dest": artifact_rel,
        "files": lock.entries.iter().find(|e| e.name == name).map(|e| e.files.len()).unwrap_or(0),
        "bytes": total_bytes,
        "manifest": opts.manifest_path.display().to_string(),
        "warnings": warnings,
    }))
}

fn collect_lock_files(root: &Path, dir: &Path, out: &mut Vec<LockFile>) -> Result<(), AppError> {
    for ent in fs::read_dir(dir)?.flatten() {
        let path = ent.path();
        let md = ent.metadata()?;
        if md.is_dir() {
            collect_lock_files(root, &path, out)?;
        } else if md.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(LockFile {
                path: rel,
                size: md.len(),
                sha256: sha256_file(&path)?,
            });
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, text: &str) -> Result<(), AppError> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// `EnvCfg` 层便捷入口（cmds 用）：定义目录 → def。
pub fn load_def_by_name(dir: &Path, name: &str) -> Result<(PathBuf, PipelineDef), AppError> {
    let defs = discover(dir)?;
    defs.into_iter()
        .find(|(_, d)| d.name == name)
        .ok_or_else(|| {
            AppError::Config(format!(
                "pipeline '{name}' not found in {} (try `momoctl pipeline list`)",
                dir.display()
            ))
        })
}

/// repo 根 = pipelines 目录的两级父目录（infra/pipelines → repo 根约定）。
/// 相对路径先按 cwd 绝对化，否则 `pipelines` 候选会把 repo 根算成 `.`。
pub fn repo_root_for(pipelines_dir: &Path) -> PathBuf {
    let abs = if pipelines_dir.is_absolute() {
        pipelines_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(pipelines_dir)
    };
    abs.parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| env_cfg::expand_tilde("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date() {
        // 2026-01-01T00:00:00Z = 1767225600 unix；2026-11-08T13:50:22Z = 1794145822 unix
        assert_eq!(unix_to_civil(1_767_225_600), (2026, 1, 1, 0, 0, 0));
        assert_eq!(unix_to_civil(1_794_145_822), (2026, 11, 8, 13, 50, 22));
        assert_eq!(unix_to_civil(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn shell_quote_roundtrip() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    #[test]
    fn expand_unknown_key_rejected() {
        let e = expand("echo {nope}", &BTreeMap::new(), &BTreeMap::new()).unwrap_err();
        assert!(e.to_string().contains("unknown placeholder"), "{e}");
    }
}
