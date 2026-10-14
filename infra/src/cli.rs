//! 签名对齐 momo-docs `02-repos/momo-infra.md`「子命令清单」。

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::jobs::{JobExecutor, JobKind};

#[derive(Debug, Parser)]
#[command(
    name = "momoctl",
    version,
)]
pub struct Cli {
    /// Emit a single JSON object (versioned schema, for CI/scripts)
    #[arg(long, global = true)]
    pub json: bool,

    /// .env file path (auto-discovers ./.env by default)
    #[arg(long, global = true, value_name = "PATH")]
    pub env_file: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Environment check matrix (system/toolchain/python/network/env/models/endpoints)
    Doctor(DoctorArgs),
    /// Model download + sha256 verification (manifest from momo-models MANIFEST.toml)
    Pull(PullArgs),
    /// Local service orchestration: up/down/status/logs/restart
    Serve(ServeArgs),
    /// Switch between normal and degraded profiles
    Switch(SwitchArgs),
    /// End-to-end latency measurement (p50/p95, target <1.5s to first audio)
    Bench(BenchArgs),
    /// Model GC: keep N-1 generations, reconcile models.lock
    Gc(GcArgs),
    /// Training pipelines: list/run/resume/status (defs in infra/pipelines/*.toml)
    Pipeline(PipelineArgs),
    /// env schema drift check (config.schema.toml vs .env.example / config.md)
    Config(ConfigArgs),
    /// Web console: axum server, embedded SPA + /api route base
    Web(WebArgs),
    /// Job runner: submit/list/status/log/cancel via local exec (state-dir store)
    Jobs(JobsArgs),
}

/// 部署档位（与 MOMO_PROFILE 取值一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Profile {
    Normal,
    Degraded,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Normal => "normal",
            Profile::Degraded => "degraded",
        }
    }
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Validate required env and endpoints for a profile (default: MOMO_PROFILE, else normal)
    #[arg(long, value_enum)]
    pub profile: Option<Profile>,
}

#[derive(Debug, Args)]
pub struct PullArgs {
    /// MANIFEST.toml path (default: momo-models repo models/MANIFEST.toml)
    #[arg(long, value_name = "PATH")]
    pub manifest: Option<PathBuf>,

    /// Process only the named entries (repeatable)
    #[arg(long, value_name = "NAME")]
    pub only: Vec<String>,

    /// Verify only, no download (recompute sha256 per models.lock / MANIFEST)
    #[arg(long)]
    pub check: bool,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    #[command(subcommand)]
    pub action: ServeAction,
}

#[derive(Debug, Subcommand)]
pub enum ServeAction {
    /// Spawn components in profile start order and wait for readiness
    Up(ServeCommon),
    /// SIGTERM→SIGKILL in reverse pidfile order
    Down(ServeCommon),
    /// pid liveness + /healthz probes
    Status(ServeCommon),
    /// Tail component logs (-n lines, -f follow)
    Logs(LogsArgs),
    /// down + up
    Restart(ServeCommon),
}

#[derive(Debug, Args)]
pub struct ServeCommon {
    /// Component profile (default: MOMO_PROFILE, else normal)
    #[arg(long, value_enum)]
    pub profile: Option<Profile>,

    /// Apply only to the named components (repeatable)
    #[arg(long, value_name = "COMPONENT", visible_alias = "component")]
    pub only: Vec<String>,

    /// momod binary path (default: PATH lookup)
    #[arg(long, value_name = "PATH")]
    pub app_bin: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    #[command(flatten)]
    pub common: ServeCommon,

    /// Print the last N lines per component
    #[arg(short = 'n', long = "lines", default_value_t = 50, value_name = "N")]
    pub lines: usize,

    /// Keep following new log output (Ctrl-C to exit)
    #[arg(short = 'f', long)]
    pub follow: bool,
}

#[derive(Debug, Args)]
pub struct SwitchArgs {
    /// Target profile: repoint .env symlink to .env.<to>
    #[arg(long, value_enum)]
    pub to: Profile,

    /// Automatically serve down && serve up after switching
    #[arg(long)]
    pub restart: bool,
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    /// Number of turns to measure (p50/p95)
    #[arg(long, default_value_t = 5, value_name = "N")]
    pub turns: u32,

    /// Exit with code 3 when over budget (>=1.5s to first audio)
    #[arg(long)]
    pub strict: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: ConfigAction,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Check the three .env.example files and config.md against config.schema.toml
    Check(ConfigCheckArgs),
}

#[derive(Debug, Args)]
pub struct ConfigCheckArgs {
    /// Repo root (default: locate config.schema.toml upward from CWD)
    #[arg(long, value_name = "PATH")]
    pub root: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct WebArgs {
    /// Listen address (default 127.0.0.1:7900);
    /// non-loopback requires MOMO_WEB_TOKEN, else refuse to start (exit 2)
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:7900")]
    pub listen: String,
}

#[derive(Debug, Args)]
pub struct JobsArgs {
    #[command(subcommand)]
    pub action: JobsAction,
}

#[derive(Debug, Subcommand)]
pub enum JobsAction {
    /// Submit a job (local runner by default; `--executor ssh` for a remote host)
    Submit(Box<JobsSubmitArgs>),
    /// List all jobs (oldest first)
    List,
    /// Show one job record
    Status(JobIdArgs),
    /// Tail a job's captured stdout/stderr log
    Log(JobsLogArgs),
    /// Cancel a queued/running job (local: SIGTERM→SIGKILL; ssh: remote TERM→KILL)
    Cancel(JobIdArgs),
    /// Reattach a runner to a recovering ssh job (re-query by remote job id)
    Recover(JobIdArgs),
    /// Internal runner spawned by `jobs submit`; not a user action
    #[command(hide = true)]
    Exec(JobIdArgs),
}

#[derive(Debug, Args)]
pub struct JobsSubmitArgs {
    /// Job kind domain
    #[arg(long, value_enum, default_value_t = JobKind::Train)]
    pub kind: JobKind,

    /// Executor: local child process or remote ssh host
    #[arg(long, value_enum, default_value_t = JobExecutor::Local)]
    pub executor: JobExecutor,

    /// Human-readable description (default: joined argv)
    #[arg(long, value_name = "TEXT")]
    pub desc: Option<String>,

    // ----- exec_ssh options (require --executor ssh) -----
    /// SSH target: Host alias from ~/.ssh/config (e.g. 4090D, mac-studio) or hostname
    #[arg(long, value_name = "HOST")]
    pub host: Option<String>,

    /// SSH login user (default: ssh config / current user)
    #[arg(long, value_name = "USER")]
    pub user: Option<String>,

    /// SSH identity file (default: ssh config)
    #[arg(long, value_name = "PATH")]
    pub key: Option<PathBuf>,

    /// Remote working root; the job dir is <remote-dir>/<job-id>
    /// (default: ~/.cache/momo/remote-jobs on the remote host)
    #[arg(long, value_name = "PATH")]
    pub remote_dir: Option<String>,

    /// Env whitelist entry pushed to the remote job: KEY (= local env) or KEY=VALUE.
    /// Sensitive keys (*_API_KEY/*_TOKEN/*_SECRET/…) are always dropped.
    #[arg(long = "env", value_name = "KEY[=VALUE]")]
    pub env: Vec<String>,

    /// Extra file/dir shipped inside the job bundle under data/ (repeatable)
    #[arg(long = "data", value_name = "PATH")]
    pub data: Vec<PathBuf>,

    /// Register remote artifacts/ into $MOMO_MODELS_DIR under this name on success
    #[arg(long, value_name = "NAME")]
    pub artifact_name: Option<String>,

    /// Hours until the remote machine is released (e.g. 24 on 4090D);
    /// artifacts are prefetched >=1h before the deadline
    #[arg(long, value_name = "HOURS")]
    pub lease_hours: Option<f64>,

    /// Payload command and args (place after `--`)
    #[arg(
        required = true,
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "CMD..."
    )]
    pub cmd: Vec<String>,
}

#[derive(Debug, Args)]
pub struct JobIdArgs {
    /// Job id (ULID)
    pub id: String,
}

#[derive(Debug, Args)]
pub struct JobsLogArgs {
    /// Job id (ULID)
    pub id: String,

    /// Print the last N lines
    #[arg(short = 'n', long = "lines", default_value_t = 50, value_name = "N")]
    pub lines: usize,

    /// Follow appended output until the job reaches a terminal state
    #[arg(short = 'f', long)]
    pub follow: bool,
}

#[derive(Debug, Args)]
pub struct GcArgs {
    /// Report only, do not delete (default; deletion requires --yes)
    #[arg(long)]
    pub dry_run: bool,

    /// Fine-tune artifact generations to keep (default 2, i.e. newest two of N-1)
    #[arg(long, default_value_t = 2, value_name = "N")]
    pub keep: u32,

    /// Confirm deletion (without --yes this is equivalent to --dry-run)
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct PipelineArgs {
    #[command(subcommand)]
    pub action: PipelineAction,
}

#[derive(Debug, Subcommand)]
pub enum PipelineAction {
    /// List pipeline definitions (--dir or auto-detected infra/pipelines/)
    List(PipelineDirArgs),
    /// Start a new run: serial steps, per-step manifest, artifact registration
    Run(PipelineRunArgs),
    /// Resume a run from its first non-succeeded step (default: latest run)
    Resume(PipelineResumeArgs),
    /// Show a run's step table (default: latest run)
    Status(PipelineStatusArgs),
}

#[derive(Debug, Args)]
pub struct PipelineDirArgs {
    /// Pipeline definitions directory (default: auto-detect infra/pipelines/)
    #[arg(long, value_name = "PATH")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct PipelineRunArgs {
    /// Pipeline name (infra/pipelines/<name>.toml)
    #[arg(long, value_name = "NAME")]
    pub name: String,

    /// Inject a [vars] value, repeatable (--set input=/path/to/raw)
    #[arg(long, value_name = "KEY=VALUE")]
    pub set: Vec<String>,

    /// Expand the plan without executing commands or registering artifacts
    #[arg(long)]
    pub dry_run: bool,

    /// Artifact generation date yyyymmdd (default: today UTC)
    #[arg(long, value_name = "YYYYMMDD")]
    pub date: Option<String>,

    /// MANIFEST.toml the register step appends to (default: auto-detect)
    #[arg(long, value_name = "PATH")]
    pub manifest: Option<PathBuf>,

    /// Pipeline definitions directory (default: auto-detect infra/pipelines/)
    #[arg(long, value_name = "PATH")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct PipelineResumeArgs {
    /// Pipeline name
    #[arg(long, value_name = "NAME")]
    pub name: String,

    /// Run id to resume (default: latest run of the pipeline)
    #[arg(long, value_name = "ID")]
    pub run_id: Option<String>,

    /// Inject/override a [vars] value, repeatable
    #[arg(long, value_name = "KEY=VALUE")]
    pub set: Vec<String>,

    /// MANIFEST.toml the register step appends to (default: auto-detect)
    #[arg(long, value_name = "PATH")]
    pub manifest: Option<PathBuf>,

    /// Pipeline definitions directory (default: auto-detect infra/pipelines/)
    #[arg(long, value_name = "PATH")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct PipelineStatusArgs {
    /// Pipeline name
    #[arg(long, value_name = "NAME")]
    pub name: String,

    /// Run id (default: latest run of the pipeline)
    #[arg(long, value_name = "ID")]
    pub run_id: Option<String>,

    /// Pipeline definitions directory (default: auto-detect infra/pipelines/)
    #[arg(long, value_name = "PATH")]
    pub dir: Option<PathBuf>,
}
