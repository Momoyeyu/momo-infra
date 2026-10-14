//! exec_ssh 执行器（issue #51）：rsync 推 job bundle → ssh 远端 launch →
//! 周期 rsync 拉回日志/checkpoint → 完成时产物登记回 `${MOMO_MODELS_DIR}`。
//!
//! 与 exec_local 共用同一 job store / 状态机 / `momoctl jobs exec` helper：
//! helper 仍是状态机唯一写入方，远端侧不跑任何 momo 组件——远端目录里只有
//! 普通文件与一个 `sh run.sh` 监督进程（记录 `job.pid`/`payload.pid`/`exit.code`）。
//!
//! 远端 process identity = remote_id(远端 pid) + `ps args` 含 `<remote_dir>/run.sh`
//! 标记（pid 复用时 args 不含 job 目录，不算同一进程）。断线进 `recovering`：
//! 按 remote_id / run.sh 路径标记重查，确认同一进程存活 → running 续传；
//! 确认消亡才收口——未确认前绝不重复 launch（momo-infra.md 口径）。
//!
//! env 白名单：submit 解析 `--env KEY[=V]` / API env map 写进 bundle 的
//! `env.sh`；`*_API_KEY`、含 `_TOKEN/_SECRET/_PASSWORD/_CREDENTIAL/_PRIVATE_KEY`
//! 的键一律丢弃（借来的机器不推敏感物，borrowed-etiquette.md）。
//!
//! 4090D 纪律：`--lease-hours` 换算释放时刻落 `release_ms`；距释放不足
//! `artifact_lead`（默认 1h）时预拉 artifacts 快照到本地 bundle。
//!
//! 平台：ssh/rsync 依赖 macOS/Linux；Windows 直接报 unsupported。
//!
//! 面向用户输出英文；源码注释中文。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::jobs::store::{self, JobStore};
use crate::jobs::{runner, JobKind, JobRecord, JobStatus, SshJobSpec};
use crate::manifest::{LockEntry, LockFile, ModelsLock};
use crate::output::AppError;

/// 远端工作根缺省：远端 `~` 由远端 shell 展开。
const DEFAULT_REMOTE_DIR: &str = "~/.cache/momo/remote-jobs";

/// bundle 里 run.sh 的身份标记：probe/find 靠它确认「同一进程」。
const RUN_SH: &str = "run.sh";

/// exec_ssh 节奏参数（`exec` 用 `from_env`；测试用小间隔注入）。
#[derive(Debug, Clone)]
pub struct SshCfg {
    /// running 态轮询间隔（probe + run.log 拉回）。
    pub poll: Duration,
    /// recovering 态重探间隔。
    pub recover_poll: Duration,
    /// checkpoint 拉回周期（远端训练脚本按 2h 落盘；拉回是增量 rsync）。
    pub ckpt_poll: Duration,
    /// 距 release_ms 不足此时长 → 预拉 artifacts 快照（4090D ≥1h 纪律）。
    pub artifact_lead: Duration,
    /// 产物登记根（`MOMO_MODELS_DIR`）。
    pub models_dir: PathBuf,
}

impl SshCfg {
    pub fn from_env() -> Self {
        Self {
            poll: Duration::from_secs(30),
            recover_poll: Duration::from_secs(15),
            ckpt_poll: Duration::from_secs(300),
            artifact_lead: Duration::from_secs(3600),
            models_dir: crate::env_cfg::EnvCfg.models_dir(),
        }
    }
}

/// submit 侧输入（CLI flags / REST body 收敛成的同一形状）。
/// `env` 的 value 为 None 时从提交进程环境取值（CLI `--env KEY` 语义）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SshSubmit {
    /// ssh Host 别名或 hostname（`4090D`/`mac-studio`）。
    pub host: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub key: Option<PathBuf>,
    #[serde(default)]
    pub remote_dir: Option<String>,
    /// 推送 env 白名单：value 显式给值；null = 取提交方进程 env。
    #[serde(default)]
    pub env: BTreeMap<String, Option<String>>,
    /// 随 bundle 上行的数据文件/目录（拷进 `payload/data/`）。
    #[serde(default)]
    pub data: Vec<PathBuf>,
    /// 成功收尾时登记进 `${MOMO_MODELS_DIR}` 的产物名（相对路径）。
    #[serde(default)]
    pub artifact_name: Option<String>,
    /// 远端机器剩余租约（小时；换算成 release_ms 落盘）。
    #[serde(default)]
    pub lease_hours: Option<f64>,
}

// ---------- 传输抽象 ----------

/// 传输层失败（连不上/超时/命令不可执行/远端返回非预期）。
/// monitor 里区分用它：probe Err → recovering；pull Err → warn 继续。
#[derive(Debug)]
pub struct Link(pub String);

impl std::fmt::Display for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 远端进程探活结论（probe 只在链路通时给出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Alive,
    Gone,
}

/// ssh/rsync 传输抽象：SystemTransport 走真实外部命令；
/// 测试注入 fake（无真实远端，见文件尾 tests）。
pub trait Transport {
    /// rsync push：`local` 目录内容 → `remote_dir`（远端目录须先存在/由实现创建）。
    fn push(&self, spec: &SshJobSpec, remote_dir: &str, local: &Path) -> Result<(), Link>;
    /// rsync pull：`remote_path` → `local`（目录语义同 rsync：路径以 `/` 结尾取内容）。
    /// 远端路径缺失 → `Ok(false)`。
    fn pull(&self, spec: &SshJobSpec, remote_path: &str, local: &Path) -> Result<bool, Link>;
    /// 远端 detached launch `run.sh`：返回远端 pid（remote_id）。
    fn launch(&self, spec: &SshJobSpec, remote_dir: &str) -> Result<u32, Link>;
    /// 按 pid + run.sh 路径标记确认同一进程存活。
    fn probe(&self, spec: &SshJobSpec, remote_dir: &str, pid: u32) -> Result<Probe, Link>;
    /// 无 remote_id 时按 run.sh 路径标记找回 pid（launch 结果未确认的兜底）。
    fn find(&self, spec: &SshJobSpec, remote_dir: &str) -> Result<Option<u32>, Link>;
    /// 读远端 job 目录内小文本文件（exit.code）；不存在 → `Ok(None)`。
    fn read(&self, spec: &SshJobSpec, remote_dir: &str, name: &str)
        -> Result<Option<String>, Link>;
    /// TERM→KILL 远端 run.sh 与 payload 及其直接子进程；返回是否已确认停止。
    fn stop(&self, spec: &SshJobSpec, remote_dir: &str, pid: u32) -> Result<bool, Link>;
}

/// 真实 ssh/rsync 实现（macOS/Linux）。远端只依赖 POSIX sh/nohup/ps/pkill/pgrep/cat。
pub struct SystemTransport;

/// ssh 连接级失败的出口码（OpenSSH 约定 255）。
const SSH_DOWN_CODE: i32 = 255;
/// 远端 stop 的 TERM→KILL 间隔（秒）。
const STOP_GRACE_S: u64 = 5;

impl SystemTransport {
    /// `ssh` 公共参数：非交互、有界超时、死链路探测。
    fn ssh_args(spec: &SshJobSpec) -> Vec<String> {
        let mut v = vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "ServerAliveInterval=15".into(),
            "-o".into(),
            "ServerAliveCountMax=2".into(),
        ];
        if let Some(k) = &spec.key {
            v.push("-i".into());
            v.push(k.display().to_string());
        }
        if let Some(u) = &spec.user {
            v.push("-l".into());
            v.push(u.clone());
        }
        v
    }

    /// `ssh <opts> <host> <cmd>`：255 → Link（连接级失败）；
    /// 其余按远端命令出口码返回 (code, stdout)。
    fn run(spec: &SshJobSpec, cmd: &str) -> Result<(i32, String), Link> {
        let out = std::process::Command::new("ssh")
            .args(Self::ssh_args(spec))
            .arg(&spec.host)
            .arg(cmd)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| Link(format!("ssh spawn failed: {e}")))?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        match out.status.code() {
            Some(SSH_DOWN_CODE) => Err(Link(format!(
                "ssh {} unreachable: {}",
                spec.host,
                stderr.trim()
            ))),
            Some(c) => Ok((c, String::from_utf8_lossy(&out.stdout).into_owned())),
            None => Err(Link(format!("ssh {} terminated by signal", spec.host))),
        }
    }

    /// 快速连通性检查（pull 失败后区分「链路断」与「远端路径缺失」）。
    fn link_up(spec: &SshJobSpec) -> bool {
        std::process::Command::new("ssh")
            .args(Self::ssh_args(spec))
            .arg(&spec.host)
            .arg("true")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// `rsync -az -e "ssh <opts>"`；失败再探链路：通 → 远端侧错误（非 Link）。
    fn rsync(spec: &SshJobSpec, src: &str, dst: &str) -> Result<Result<(), String>, Link> {
        let e_arg = format!(
            "ssh {}",
            Self::ssh_args(spec)
                .iter()
                .map(|a| shq(a))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let out = std::process::Command::new("rsync")
            .args(["-az", "--timeout=120", "-e", &e_arg, src, dst])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| Link(format!("rsync spawn failed: {e}")))?;
        if out.status.success() {
            return Ok(Ok(()));
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if out.status.code() == Some(SSH_DOWN_CODE) || !Self::link_up(spec) {
            return Err(Link(format!("rsync link down: {stderr}")));
        }
        Ok(Err(stderr))
    }

    /// rsync 风格的远端引用 `host:path`。
    fn remote_ref(spec: &SshJobSpec, path: &str) -> String {
        match &spec.user {
            Some(u) => format!("{u}@{}:{}", spec.host, path),
            None => format!("{}:{}", spec.host, path),
        }
    }
}

impl Transport for SystemTransport {
    fn push(&self, spec: &SshJobSpec, remote_dir: &str, local: &Path) -> Result<(), Link> {
        // macOS 自带 rsync 无 --mkpath：先 ssh mkdir 再推。
        let (code, _) = Self::run(spec, &format!("mkdir -p {}", shq(remote_dir)))?;
        if code != 0 {
            return Err(Link(format!("mkdir {remote_dir} failed on remote")));
        }
        let src = format!("{}/", local.display());
        match Self::rsync(
            spec,
            &src,
            &Self::remote_ref(spec, &format!("{remote_dir}/")),
        )? {
            Ok(()) => Ok(()),
            Err(e) => Err(Link(format!("bundle push failed: {e}"))),
        }
    }

    fn pull(&self, spec: &SshJobSpec, remote_path: &str, local: &Path) -> Result<bool, Link> {
        if let Some(parent) = local.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match Self::rsync(
            spec,
            &Self::remote_ref(spec, remote_path),
            &local.display().to_string(),
        )? {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    fn launch(&self, spec: &SshJobSpec, remote_dir: &str) -> Result<u32, Link> {
        // 单条简单命令 backgrounded：$! 即 run.sh 自身的 pid；
        // 三向重定向全离开 tty，ssh 会话可正常退出。
        let run = shq(&format!("{remote_dir}/{RUN_SH}"));
        let log = shq(&format!("{remote_dir}/run.log"));
        let cmd = format!("nohup sh {run} > {log} 2>&1 < /dev/null & echo $!");
        let (code, stdout) = Self::run(spec, &cmd)?;
        if code != 0 {
            return Err(Link(format!("remote launch failed (code {code})")));
        }
        stdout.trim().parse::<u32>().map_err(|_| {
            Link(format!(
                "remote launch returned no pid: '{}'",
                stdout.trim()
            ))
        })
    }

    fn probe(&self, spec: &SshJobSpec, remote_dir: &str, pid: u32) -> Result<Probe, Link> {
        // 同一进程确认：pid 活着 且 argv 含本 job 的 run.sh 路径标记
        // （pid 被复用时 args 不含 job 目录 → Gone）。
        let cmd = format!("kill -0 {pid} 2>/dev/null && ps -o args= -p {pid} 2>/dev/null");
        let (code, stdout) = Self::run(spec, &cmd)?;
        if code != 0 {
            return Ok(Probe::Gone);
        }
        let marker = format!("{remote_dir}/{RUN_SH}");
        Ok(if stdout.contains(&marker) {
            Probe::Alive
        } else {
            Probe::Gone
        })
    }

    fn find(&self, spec: &SshJobSpec, remote_dir: &str) -> Result<Option<u32>, Link> {
        let cmd = format!(
            "pgrep -f {} | head -n 1",
            shq(&format!("{remote_dir}/{RUN_SH}"))
        );
        let (code, stdout) = Self::run(spec, &cmd)?;
        if code != 0 {
            return Ok(None); // pgrep 无命中 = exit 1
        }
        Ok(stdout.trim().parse::<u32>().ok())
    }

    fn read(
        &self,
        spec: &SshJobSpec,
        remote_dir: &str,
        name: &str,
    ) -> Result<Option<String>, Link> {
        let (code, stdout) = Self::run(
            spec,
            &format!("cat {}", shq(&format!("{remote_dir}/{name}"))),
        )?;
        Ok(if code == 0 { Some(stdout) } else { None })
    }

    fn stop(&self, spec: &SshJobSpec, remote_dir: &str, pid: u32) -> Result<bool, Link> {
        // TERM（payload + 其子进程 + run.sh）→ grace → KILL 同集合。
        // 出口码：run.sh 确认消亡 → 0。
        let d = shq(remote_dir);
        let cmd = format!(
            "p={pid}; pp=$(cat {d}/payload.pid 2>/dev/null); \
             kill -TERM \"$pp\" 2>/dev/null; pkill -TERM -P \"$pp\" 2>/dev/null; \
             kill -TERM \"$p\" 2>/dev/null; pkill -TERM -P \"$p\" 2>/dev/null; \
             sleep {STOP_GRACE_S}; \
             pkill -KILL -P \"$pp\" 2>/dev/null; kill -KILL \"$pp\" 2>/dev/null; \
             pkill -KILL -P \"$p\" 2>/dev/null; kill -KILL \"$p\" 2>/dev/null; \
             ! kill -0 \"$p\" 2>/dev/null",
            pid = pid,
            d = d
        );
        let (code, _) = Self::run(spec, &cmd)?;
        Ok(code == 0)
    }
}

// ---------- submit：校验 + bundle staging + 建档 ----------

/// `momoctl jobs submit --executor ssh ...` 的 core：
/// 校验 spec → 建档 → 写 bundle（run.sh/env.sh/data）→ spawn helper。
/// 返回 (record, warns)：敏感/未解析 env 键的提示给 CLI/API 回显。
pub fn submit(
    store: &JobStore,
    kind: JobKind,
    args: Vec<String>,
    desc: String,
    sub: SshSubmit,
) -> Result<(JobRecord, Vec<String>), AppError> {
    ensure_supported()?;
    if args.is_empty() || args[0].trim().is_empty() {
        return Err(AppError::Config(
            "jobs submit: empty command (expected `-- CMD [ARGS...]`)".into(),
        ));
    }
    let mut warns = Vec::new();
    if !valid_name(&sub.host) {
        return Err(AppError::Config(format!(
            "invalid ssh host '{}' (expect alias/hostname: [A-Za-z0-9._-]+)",
            sub.host
        )));
    }
    if let Some(u) = &sub.user {
        if !valid_name(u) {
            return Err(AppError::Config(format!("invalid ssh user '{u}'")));
        }
    }
    if let Some(k) = &sub.key {
        let k = crate::env_cfg::expand_tilde(&k.display().to_string());
        if !k.is_file() {
            return Err(AppError::Config(format!(
                "ssh identity file not found: {}",
                k.display()
            )));
        }
    }
    let remote_dir = sub
        .remote_dir
        .unwrap_or_else(|| DEFAULT_REMOTE_DIR.into())
        .trim()
        .to_string();
    if remote_dir.is_empty() || remote_dir.bytes().any(|b| b == b'\n' || b == b'\r') {
        return Err(AppError::Config(format!(
            "invalid remote dir '{remote_dir}'"
        )));
    }
    let artifact_name = match &sub.artifact_name {
        Some(n) => Some(valid_artifact_name(n)?),
        None => None,
    };
    let release_ms = match sub.lease_hours {
        Some(h) if h.is_nan() || h <= 0.0 => {
            return Err(AppError::Config(format!(
                "--lease-hours must be positive (got {h})"
            )))
        }
        Some(h) => Some(crate::jobs::now_ms() + (h * 3_600_000.0) as u64),
        None => None,
    };
    let (env, w) = resolve_env(&sub.env);
    warns.extend(w);

    let mut rec = store.create(kind, args, desc)?;
    rec.executor = crate::jobs::JobExecutor::Ssh;
    rec.ssh = Some(SshJobSpec {
        host: sub.host.clone(),
        user: sub.user.clone(),
        key: sub.key.clone(),
        remote_dir,
        env_keys: env.keys().cloned().collect(),
        artifact_name,
        release_ms,
    });
    store.save(&rec)?;

    let bundle = store.bundle_dir(&rec.id)?;
    if let Err(e) = stage_bundle(&bundle, &rec.args, &env, &sub.data) {
        let detail = format!("bundle staging failed: {e}");
        runner_log(&rec, &detail);
        let mut r = rec.clone();
        let _ = store.transition(&mut r, JobStatus::Failed, Some(detail));
        return Err(e);
    }
    if let Err(e) = runner::spawn_helper(store, &rec.id) {
        let detail = format!("runner spawn failed: {e}");
        let mut r = rec.clone();
        let _ = store.transition(&mut r, JobStatus::Failed, Some(detail));
        return Err(AppError::Io(e));
    }
    Ok((rec, warns))
}

/// ssh/rsync 平台门：Windows 无可靠 OpenSSH+rsync 组合，直接拒绝。
fn ensure_supported() -> Result<(), AppError> {
    #[cfg(windows)]
    {
        Err(AppError::Config(
            "exec_ssh requires ssh/rsync; not supported on Windows".into(),
        ))
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// host/user 字符集：alias/hostname/login 常规字符。
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// artifact_name：相对路径、无 `..`/根/空段（写入 `${MOMO_MODELS_DIR}` 之下）。
fn valid_artifact_name(n: &str) -> Result<String, AppError> {
    let p = Path::new(n);
    let ok = !n.trim().is_empty()
        && !p.is_absolute()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    if ok {
        Ok(n.trim().to_string())
    } else {
        Err(AppError::Config(format!(
            "invalid artifact name '{n}' (expect a relative path)"
        )))
    }
}

/// env 白名单敏感键判据：按名过滤——借来的机器不推任何疑似凭据。
fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    const NEEDLES: &[&str] = &[
        "_SECRET",
        "_TOKEN",
        "_PASSWORD",
        "_CREDENTIAL",
        "_PRIVATE_KEY",
    ];
    NEEDLES.iter().any(|n| k.contains(n))
        || k.ends_with("_API_KEY")
        || k.ends_with("_ACCESS_KEY")
        || k.ends_with("_SESSION_KEY")
}

/// 解析 env 白名单：value None → 提交进程 env；键名非法/敏感/未设置 → 丢弃并 warn。
fn resolve_env(
    input: &BTreeMap<String, Option<String>>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut out = BTreeMap::new();
    let mut warns = Vec::new();
    for (k, v) in input {
        if !valid_env_key(k) {
            warns.push(format!("env '{k}': invalid key name, dropped"));
            continue;
        }
        if is_sensitive_key(k) {
            warns.push(format!("env '{k}': sensitive key, never pushed (dropped)"));
            continue;
        }
        match v.clone().or_else(|| std::env::var(k).ok()) {
            Some(val) if !val.is_empty() => {
                out.insert(k.clone(), val);
            }
            _ => warns.push(format!("env '{k}': unset, skipped")),
        }
    }
    (out, warns)
}

fn valid_env_key(k: &str) -> bool {
    let mut b = k.bytes();
    match b.next() {
        Some(c) if c.is_ascii_alphabetic() || c == b'_' => {}
        _ => return false,
    }
    b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// POSIX shell 单引号包裹（`'` → `'"'"'`：关闭引号 + 双引号内字面量 + 重开）。
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// bundle staging：`bundle_root/payload/` 为推上远端的全部内容：
/// `run.sh`（监督脚本，写 job.pid/payload.pid/exit.code）、`env.sh`（白名单）、
/// `data/`（显式附带数据）。
fn stage_bundle(
    bundle_root: &Path,
    args: &[String],
    env: &BTreeMap<String, String>,
    data: &[PathBuf],
) -> Result<(), AppError> {
    let payload = bundle_root.join("payload");
    fs::create_dir_all(&payload)?;

    // run.sh：监督进程——记 pid、起 payload、记退出码。
    // 不用 exec：监督 shell 必须活到 payload 结束（probe 认的是它）。
    let argv = args.iter().map(|a| shq(a)).collect::<Vec<_>>().join(" ");
    let run_sh = concat!(
        "#!/bin/sh\n",
        "# exec_ssh payload supervisor（momoctl 生成；勿手改）\n",
        "cd \"$(dirname \"$0\")\" || exit 1\n",
        "echo $$ > job.pid\n",
        "if [ -f ./env.sh ]; then . ./env.sh; fi\n",
    )
    .to_string()
        + &argv
        + " &\ncpid=$!\necho \"$cpid\" > payload.pid\nwait \"$cpid\"\ncode=$?\n\
           echo \"$code\" > exit.code\nexit \"$code\"\n";
    fs::write(payload.join(RUN_SH), run_sh)?;

    let mut env_sh = String::from("#!/bin/sh\n# exec_ssh env whitelist（敏感键已过滤）\n");
    for (k, v) in env {
        env_sh.push_str(&format!("export {k}={}\n", shq(v)));
    }
    fs::write(payload.join("env.sh"), env_sh)?;

    for d in data {
        let name = d
            .file_name()
            .ok_or_else(|| AppError::Config(format!("bad --data path {}", d.display())))?;
        let dst = payload.join("data").join(name);
        if d.is_dir() {
            copy_dir_all(d, &dst)?;
        } else if d.is_file() {
            fs::create_dir_all(dst.parent().unwrap())?;
            fs::copy(d, &dst)?;
        } else {
            return Err(AppError::Config(format!(
                "--data path not found: {}",
                d.display()
            )));
        }
    }
    Ok(())
}

/// 递归目录复制（bundle staging / prefetch 快照注册回退用）。
fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), AppError> {
    fs::create_dir_all(dst)?;
    for ent in fs::read_dir(src)? {
        let ent = ent?;
        let to = dst.join(ent.file_name());
        if ent.file_type()?.is_dir() {
            copy_dir_all(&ent.path(), &to)?;
        } else {
            fs::copy(ent.path(), &to)?;
        }
    }
    Ok(())
}

// ---------- helper 主流程：claim → launch → monitor ----------

/// helper 入口（`momoctl jobs exec <id>` 按 executor 分发到这里）。
/// queued → launch；recovering/canceling → 认领后重查（绝不重复 launch）。
pub fn exec(store: &JobStore, id: &str) -> Result<i32, AppError> {
    exec_with(store, id, &SystemTransport, &SshCfg::from_env())
}

pub(crate) fn exec_with(
    store: &JobStore,
    id: &str,
    t: &dyn Transport,
    cfg: &SshCfg,
) -> Result<i32, AppError> {
    ensure_supported()?;
    let mut rec = match store.load(id)? {
        Some(r) => r,
        None => return Err(AppError::Config(format!("job {id} not found"))),
    };
    match rec.status {
        JobStatus::Queued | JobStatus::Recovering | JobStatus::Canceling => {}
        _ => return Ok(crate::output::ExitCode::Ok as i32),
    }
    if !runner::claim(store, &mut rec)? {
        return Ok(crate::output::ExitCode::Ok as i32); // 被其他 helper 抢走
    }

    if rec.status == JobStatus::Preparing && !launch(store, &mut rec, t)? {
        // launch 路径（刚 claim 的 queued）：已收口，直接退出。
        return Ok(crate::output::ExitCode::Ok as i32);
    }
    monitor(store, id, t, cfg)
}

/// launch 阶段：cancel 复查 → push bundle → 远端 launch → 记 remote_id。
/// `Ok(true)` 进 monitor；`Ok(false)` 已收口直接退出。
fn launch(store: &JobStore, rec: &mut JobRecord, t: &dyn Transport) -> Result<bool, AppError> {
    let Some(spec) = rec.ssh.clone() else {
        let detail = "ssh job missing spec".to_string();
        runner_log(rec, &detail);
        store.transition(rec, JobStatus::Failed, Some(detail))?;
        return Ok(false);
    };
    let bundle = store.bundle_dir(&rec.id)?.join("payload");
    let rdir = remote_dir(&spec, &rec.id);

    // launch 前复查 cancel（submit→spawn 窗口）。
    match store.load(&rec.id)? {
        Some(f) if f.status.is_terminal() => return Ok(false),
        Some(f) if f.status == JobStatus::Canceling || f.cancel_requested => {
            store.transition(rec, JobStatus::Canceling, Some("cancel requested".into()))?;
            store.transition(
                rec,
                JobStatus::Canceled,
                Some("canceled before launch".into()),
            )?;
            runner_log(rec, "canceled before launch");
            return Ok(false);
        }
        _ => {}
    }

    if let Err(e) = t.push(&spec, &rdir, &bundle) {
        let detail = format!("bundle push failed: {e}");
        runner_log(rec, &detail);
        store.transition(rec, JobStatus::Failed, Some(detail))?;
        return Ok(false);
    }
    runner_log(rec, &format!("bundle pushed to {}:{rdir}", spec.host));

    match t.launch(&spec, &rdir) {
        Ok(pid) => {
            rec.remote_id = Some(pid.to_string());
            store.transition(rec, JobStatus::Running, None)?;
            runner_log(rec, &format!("remote pid {pid} on {} ({rdir})", spec.host));
        }
        Err(e) => {
            // launch 结果未确认（命令可能已到达）：绝不重试，
            // 进 recovering 由 monitor 按 run.sh 标记重查（NEVER re-launch）。
            runner_log(rec, &format!("remote launch unconfirmed: {e}"));
            store.transition(
                rec,
                JobStatus::Recovering,
                Some("launch result unconfirmed".into()),
            )?;
        }
    }
    Ok(true)
}

/// 监控主循环：probe → 日志/ckpt 拉回 / stop / recovering → Gone 时收口。
fn monitor(store: &JobStore, id: &str, t: &dyn Transport, cfg: &SshCfg) -> Result<i32, AppError> {
    let bundle = store.bundle_dir(id)?;
    let staging = bundle.join("remote-run.log");
    let offset_path = bundle.join("pull.offset");
    let mut log_off = read_offset(&offset_path);
    // 首轮即拉 ckpt（reattach 场景可能已经落了几个 checkpoint）。
    let mut last_ckpt = Instant::now()
        .checked_sub(cfg.ckpt_poll)
        .unwrap_or_else(Instant::now);

    loop {
        let Some(mut fresh) = store.load(id)? else {
            return Ok(crate::output::ExitCode::Ok as i32); // meta 消失：退出
        };
        if fresh.status.is_terminal() {
            return Ok(crate::output::ExitCode::Ok as i32);
        }
        let Some(spec) = fresh.ssh.clone() else {
            return Err(AppError::Config(format!("job {id}: ssh spec missing")));
        };
        let rdir = remote_dir(&spec, id);
        let want_cancel = fresh.cancel_requested || fresh.status == JobStatus::Canceling;
        let remote_pid: Option<u32> = fresh.remote_id.as_deref().and_then(|s| s.parse().ok());

        let probe = match remote_pid {
            Some(p) => t.probe(&spec, &rdir, p),
            None => match t.find(&spec, &rdir) {
                // 找回 remote_id（launch 未确认但实际起来了）：认领后继续监控。
                Ok(Some(p)) => {
                    fresh.remote_id = Some(p.to_string());
                    let _ = store.save(&fresh);
                    runner_log(&fresh, &format!("re-identified remote pid {p}"));
                    Ok(Probe::Alive)
                }
                Ok(None) => Ok(Probe::Gone),
                Err(e) => Err(e),
            },
        };

        match probe {
            Ok(Probe::Alive) => {
                if want_cancel {
                    // find() 回填的 remote_id 在 fresh 上；取最新解析值。
                    if let Some(p) = fresh.remote_id.as_deref().and_then(|s| s.parse().ok()) {
                        let _ = t.stop(&spec, &rdir, p);
                    }
                } else {
                    if fresh.status == JobStatus::Recovering {
                        store.transition(
                            &mut fresh,
                            JobStatus::Running,
                            Some("ssh reconnected; process identity confirmed".into()),
                        )?;
                        runner_log(&fresh, "recovered: remote process still alive");
                    }
                    log_off = pull_log(t, &spec, &rdir, &staging, &fresh.log_path, log_off);
                    save_offset(&offset_path, log_off);
                    if last_ckpt.elapsed() >= cfg.ckpt_poll {
                        pull_ckpt(t, &spec, &rdir, &bundle, &fresh);
                        last_ckpt = Instant::now();
                    }
                    deadline_check(t, &spec, &rdir, &bundle, &fresh, cfg);
                }
                std::thread::sleep(cfg.poll);
            }
            Ok(Probe::Gone) => {
                return conclude(store, id, t, cfg, want_cancel, &staging, log_off);
            }
            Err(link) => {
                match fresh.status {
                    JobStatus::Running | JobStatus::Canceling => {
                        store.transition(
                            &mut fresh,
                            JobStatus::Recovering,
                            Some(format!("ssh link lost: {link}")),
                        )?;
                        runner_log(&fresh, &format!("ssh link lost: {link}"));
                    }
                    _ => {}
                }
                std::thread::sleep(cfg.recover_poll);
            }
        }
    }
}

/// 收口：终拉日志/ckpt → 读 exit.code → 按语义落终态 + 产物登记。
/// 只在「链路通、确认远端进程消亡」后调用——链路不通永不走到这（不重复 launch）。
fn conclude(
    store: &JobStore,
    id: &str,
    t: &dyn Transport,
    cfg: &SshCfg,
    want_cancel: bool,
    staging: &Path,
    log_off: u64,
) -> Result<i32, AppError> {
    let Some(mut rec) = store.load(id)? else {
        return Ok(crate::output::ExitCode::Ok as i32);
    };
    let Some(spec) = rec.ssh.clone() else {
        return Err(AppError::Config(format!("job {id}: ssh spec missing")));
    };
    let rdir = remote_dir(&spec, id);
    let bundle = store.bundle_dir(id)?;

    // 终拉：尽力而为，失败只 warn。
    let off = pull_log(t, &spec, &rdir, staging, &rec.log_path, log_off);
    save_offset(&bundle.join("pull.offset"), off);
    pull_ckpt(t, &spec, &rdir, &bundle, &rec);

    let code = t
        .read(&spec, &rdir, "exit.code")
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<i32>().ok());
    rec.exit = code;

    if want_cancel {
        // 用户已要停：确认消亡 → canceled（独立终态）。
        if rec.status == JobStatus::Running {
            store.transition(
                &mut rec,
                JobStatus::Canceling,
                Some("cancel requested".into()),
            )?;
        }
        if rec.status == JobStatus::Canceled {
            store.save(&rec)?;
        } else {
            store.transition(
                &mut rec,
                JobStatus::Canceled,
                Some("remote process confirmed stopped".into()),
            )?;
        }
        return Ok(crate::output::ExitCode::Ok as i32);
    }

    match code {
        Some(0) => {
            collect_artifacts(&rec, &spec, &rdir, &bundle, t, cfg);
            land_succeeded(store, &mut rec)?;
        }
        Some(c) => {
            let detail = format!("remote exited with code {c}");
            runner_log(&rec, &detail);
            store.transition(&mut rec, JobStatus::Failed, Some(detail))?;
        }
        None => {
            let detail = "remote process exited without status".to_string();
            runner_log(&rec, &detail);
            store.transition(&mut rec, JobStatus::Failed, Some(detail))?;
        }
    }
    Ok(crate::output::ExitCode::Ok as i32)
}

/// recovering → succeeded 的合法路径：先回 running（同一进程确认）再落终态。
fn land_succeeded(store: &JobStore, rec: &mut JobRecord) -> Result<(), AppError> {
    if rec.status == JobStatus::Recovering {
        store.transition(
            rec,
            JobStatus::Running,
            Some("remote process re-identified".into()),
        )?;
    }
    store.transition(rec, JobStatus::Succeeded, None)
}

/// 产物回收 + 登记：远端 `artifacts/` → `${MOMO_MODELS_DIR}/<name>`；
/// 拉不到时回退 deadline 预拉快照；有文件才写 models.lock。
fn collect_artifacts(
    rec: &JobRecord,
    spec: &SshJobSpec,
    rdir: &str,
    bundle: &Path,
    t: &dyn Transport,
    cfg: &SshCfg,
) {
    let Some(name) = &spec.artifact_name else {
        return;
    };
    let dest = cfg.models_dir.join(name);
    let pulled = t
        .pull(spec, &format!("{rdir}/artifacts/"), &dest)
        .unwrap_or(false);
    if !pulled {
        // 链路刚好又断 / artifacts 缺失：回退到 deadline 预拉快照。
        let prefetch = bundle.join("artifacts-prefetch");
        if prefetch.is_dir() {
            if let Err(e) = copy_dir_all(&prefetch, &dest) {
                runner_log(
                    rec,
                    &format!("warn: artifact prefetch fallback failed: {e}"),
                );
            }
        }
    }
    match register_artifacts(&cfg.models_dir, name, &rec.id) {
        Ok(0) => runner_log(rec, "warn: no artifact files pulled"),
        Ok(n) => runner_log(
            rec,
            &format!(
                "registered {n} artifact files in models.lock ({}/{name})",
                cfg.models_dir.display()
            ),
        ),
        Err(e) => runner_log(rec, &format!("warn: artifact registration failed: {e}")),
    }
}

/// models.lock 登记：`local/<name>@<job_id>` 条目，逐文件 size+sha256。
fn register_artifacts(models_dir: &Path, name: &str, job_id: &str) -> Result<usize, AppError> {
    let dest = models_dir.join(name);
    let mut files = Vec::new();
    collect_files(&dest, &dest, &mut files)?;
    if files.is_empty() {
        return Ok(0);
    }
    let lock_path = models_dir.join("models.lock");
    let mut lock = ModelsLock::load(&lock_path)?;
    lock.upsert(LockEntry {
        repo: format!("local/{name}@{job_id}"),
        name: name.to_string(),
        files,
    });
    lock.save(&lock_path)?;
    Ok(lock
        .entries
        .iter()
        .find(|e| e.name == name)
        .map(|e| e.files.len())
        .unwrap_or(0))
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<LockFile>) -> Result<(), AppError> {
    if !dir.is_dir() {
        return Ok(());
    }
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let path = ent.path();
        if ent.file_type()?.is_dir() {
            collect_files(root, &path, out)?;
        } else if ent.file_type()?.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push(LockFile {
                path: rel,
                size: ent.metadata()?.len(),
                sha256: crate::manifest::sha256_file(&path)?,
            });
        }
    }
    Ok(())
}

// ---------- 周期拉回与 4090D 纪律 ----------

/// run.log 增量拉回：staging 镜像文件 → 追加到 `logs/<id>.log`（store/SSE 同文件）。
fn pull_log(
    t: &dyn Transport,
    spec: &SshJobSpec,
    rdir: &str,
    staging: &Path,
    log_path: &str,
    offset: u64,
) -> u64 {
    match t.pull(spec, &format!("{rdir}/run.log"), staging) {
        Ok(true) => match store::read_chunk(staging, offset) {
            Ok((cur, chunk)) => {
                if !chunk.is_empty() {
                    if let Ok(mut f) = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(log_path)
                    {
                        let _ = f.write_all(chunk.as_bytes());
                    }
                }
                cur
            }
            Err(_) => offset,
        },
        _ => offset,
    }
}

/// checkpoint 拉回：`rdir/ckpt/` → `bundles/<id>/ckpt/`（本地快照，防丢）。
fn pull_ckpt(t: &dyn Transport, spec: &SshJobSpec, rdir: &str, bundle: &Path, rec: &JobRecord) {
    let dst = bundle.join("ckpt");
    match t.pull(spec, &format!("{rdir}/ckpt/"), &dst) {
        Ok(true) => {}
        Ok(false) => {} // 远端还没 ckpt：正常
        Err(e) => runner_log(rec, &format!("warn: checkpoint pull failed: {e}")),
    }
}

/// 4090D 纪律：距 release_ms 不足 artifact_lead → 预拉 artifacts 快照到
/// `bundles/<id>/artifacts-prefetch/`（机器被回收前把产物攥在手里）；
/// 每轮 warn 一次提示剩余时间（marker 文件防重复快照）。
fn deadline_check(
    t: &dyn Transport,
    spec: &SshJobSpec,
    rdir: &str,
    bundle: &Path,
    rec: &JobRecord,
    cfg: &SshCfg,
) {
    let Some(release_ms) = spec.release_ms else {
        return;
    };
    let now = crate::jobs::now_ms();
    let lead_ms = cfg.artifact_lead.as_millis() as u64;
    if now.saturating_add(lead_ms) < release_ms {
        return;
    }
    let warned = bundle.join("deadline-warned");
    if !warned.exists() {
        let remain_m = release_ms.saturating_sub(now) / 60_000;
        runner_log(
            rec,
            &format!("release deadline in {remain_m}m; prefetching artifacts"),
        );
        let _ = fs::write(&warned, b"\n");
    }
    // 远端 artifacts 可能还没生成：拉到为止（marker 只在成功后写）。
    let prefetch = bundle.join("artifacts-prefetch");
    let marker = bundle.join("artifacts-prefetch.done");
    if !marker.exists() {
        match t.pull(spec, &format!("{rdir}/artifacts/"), &prefetch) {
            Ok(true) => {
                let _ = fs::write(&marker, b"done\n");
                runner_log(
                    rec,
                    "artifact snapshot prefetched ahead of release deadline",
                );
            }
            Ok(false) => {}
            Err(e) => runner_log(rec, &format!("warn: artifact prefetch failed: {e}")),
        }
    }
}

// ---------- cancel ----------

/// exec_ssh 的 cancel（runner::cancel 按 executor 分发）：
/// - 终态幂等；
/// - recovering：链路当前不可确认进程 → 只落 `cancel_requested`，helper 重连后收口；
/// - 其余非终态 → `canceling` + flag → 远端 TERM→KILL → 确认消亡即落 canceled，
///   未确认留 canceling（helper 每轮重试 stop）。
pub fn cancel(store: &JobStore, id: &str) -> Result<(JobRecord, String), AppError> {
    cancel_with(store, id, &SystemTransport)
}

pub(crate) fn cancel_with(
    store: &JobStore,
    id: &str,
    t: &dyn Transport,
) -> Result<(JobRecord, String), AppError> {
    let mut rec = store
        .load(id)?
        .ok_or_else(|| AppError::Config(format!("job {id} not found")))?;
    if rec.status.is_terminal() {
        return Ok((rec, "already terminal".into()));
    }

    if rec.status == JobStatus::Recovering {
        // 无法确认进程身份/链路：只记意图，绝不盲杀；helper 重连后收口。
        rec.cancel_requested = true;
        store.save(&rec)?;
        return Ok((
            rec,
            "cancel requested; remote stop pending ssh reconnect".into(),
        ));
    }

    rec.cancel_requested = true;
    if rec.status != JobStatus::Canceling {
        store.transition(
            &mut rec,
            JobStatus::Canceling,
            Some("cancel requested".into()),
        )?;
    } else {
        store.save(&rec)?;
    }

    let note = match (rec.ssh.clone(), rec.remote_id.as_deref()) {
        (Some(spec), Some(pid_s)) => match pid_s.parse::<u32>() {
            Ok(pid) => {
                let rdir = remote_dir(&spec, id);
                match t.stop(&spec, &rdir, pid) {
                    Ok(true) => "remote process confirmed stopped".to_string(),
                    Ok(false) => "remote stop sent; process still terminating".to_string(),
                    Err(e) => format!("remote unreachable ({e}); cancel pending"),
                }
            }
            Err(_) => "cancel requested".to_string(),
        },
        _ => "cancel requested".to_string(),
    };

    // 确认停止且仍 canceling → 当场落 canceled（与 helper 的 monitor 殊途同归）。
    if let Some(mut fresh) = store.load(id)? {
        let gone = match (fresh.ssh.clone(), fresh.remote_id.as_deref()) {
            (Some(spec), Some(pid_s)) => match pid_s.parse::<u32>() {
                Ok(pid) => matches!(t.probe(&spec, &remote_dir(&spec, id), pid), Ok(Probe::Gone)),
                Err(_) => true,
            },
            _ => false,
        };
        if fresh.status == JobStatus::Canceling && gone {
            store.transition(
                &mut fresh,
                JobStatus::Canceled,
                Some("remote process confirmed stopped".into()),
            )?;
        }
        return Ok((fresh, note));
    }
    Ok((rec, note))
}

// ---------- 小工具 ----------

/// 远端 job 目录：`<remote_dir>/<id>`（远端恒 unix 路径，按 `/` 拼接）。
pub fn remote_dir(spec: &SshJobSpec, id: &str) -> String {
    format!("{}/{}", spec.remote_dir.trim_end_matches('/'), id)
}

/// runner 诊断行（与 runner.rs 同口径：同文件 `[runner]` 前缀）。
fn runner_log(rec: &JobRecord, line: &str) {
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&rec.log_path)
    {
        let _ = writeln!(f, "[runner] {line}");
    }
}

fn read_offset(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn save_offset(path: &Path, off: u64) {
    let _ = fs::write(path, format!("{off}\n"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::proc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    // ---------- FakeTransport：本地沙盒目录当「远端 FS」，真 spawn `sh run.sh` ----------

    struct FakeTransport {
        /// 模拟远端文件系统根。
        root: PathBuf,
        /// true = ssh 链路断（所有调用 Err(Link)）。
        down: AtomicBool,
        /// launch 调用计数（验证「绝不重复 launch」）。
        launched: AtomicUsize,
        /// pid → 模拟 `ps args=` 输出（含 run.sh 路径标记）。
        procs: Mutex<BTreeMap<u32, String>>,
        stops: AtomicUsize,
    }

    impl FakeTransport {
        fn new(root: PathBuf) -> Self {
            Self {
                root,
                down: AtomicBool::new(false),
                launched: AtomicUsize::new(0),
                procs: Mutex::new(BTreeMap::new()),
                stops: AtomicUsize::new(0),
            }
        }
        /// 远端路径 → 沙盒路径（剥 `~`/`/` 前缀）。
        fn rpath(&self, remote: &str) -> PathBuf {
            self.root
                .join(remote.trim_start_matches('~').trim_start_matches('/'))
        }
        fn up(&self) -> Result<(), Link> {
            if self.down.load(Ordering::SeqCst) {
                Err(Link("link down".into()))
            } else {
                Ok(())
            }
        }
        fn set_down(&self, d: bool) {
            self.down.store(d, Ordering::SeqCst);
        }
    }

    impl Transport for FakeTransport {
        fn push(&self, _s: &SshJobSpec, remote_dir: &str, local: &Path) -> Result<(), Link> {
            self.up()?;
            copy_dir_all(local, &self.rpath(remote_dir)).map_err(|e| Link(e.to_string()))
        }

        fn pull(&self, _s: &SshJobSpec, remote_path: &str, local: &Path) -> Result<bool, Link> {
            self.up()?;
            let contents = remote_path.ends_with('/');
            let src = self.rpath(remote_path.trim_end_matches('/'));
            if !src.exists() {
                return Ok(false);
            }
            if src.is_dir() {
                if contents {
                    copy_dir_all(&src, local).map_err(|e| Link(e.to_string()))?;
                } else {
                    let name = src.file_name().unwrap();
                    copy_dir_all(&src, &local.join(name)).map_err(|e| Link(e.to_string()))?;
                }
            } else {
                let dst = if local.is_dir() {
                    local.join(src.file_name().unwrap())
                } else {
                    local.to_path_buf()
                };
                if let Some(p) = dst.parent() {
                    let _ = fs::create_dir_all(p);
                }
                fs::copy(&src, &dst).map_err(|e| Link(e.to_string()))?;
            }
            Ok(true)
        }

        fn launch(&self, _s: &SshJobSpec, remote_dir: &str) -> Result<u32, Link> {
            self.up()?;
            let dir = self.rpath(remote_dir);
            let script = format!(
                "nohup sh {} > {} 2>&1 </dev/null & echo $!",
                shq(&dir.join(RUN_SH).display().to_string()),
                shq(&dir.join("run.log").display().to_string())
            );
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .output()
                .map_err(|e| Link(e.to_string()))?;
            let pid: u32 = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .map_err(|_| Link("no pid".into()))?;
            self.launched.fetch_add(1, Ordering::SeqCst);
            self.procs
                .lock()
                .unwrap()
                .insert(pid, format!("sh {remote_dir}/{RUN_SH}"));
            Ok(pid)
        }

        fn probe(&self, _s: &SshJobSpec, remote_dir: &str, pid: u32) -> Result<Probe, Link> {
            self.up()?;
            let marker = format!("{remote_dir}/{RUN_SH}");
            let same = self
                .procs
                .lock()
                .unwrap()
                .get(&pid)
                .is_some_and(|m| m.contains(&marker));
            Ok(if same && proc::is_alive(pid) {
                Probe::Alive
            } else {
                Probe::Gone
            })
        }

        fn find(&self, _s: &SshJobSpec, remote_dir: &str) -> Result<Option<u32>, Link> {
            self.up()?;
            let marker = format!("{remote_dir}/{RUN_SH}");
            Ok(self
                .procs
                .lock()
                .unwrap()
                .iter()
                .find(|(p, m)| m.contains(&marker) && proc::is_alive(**p))
                .map(|(p, _)| *p))
        }

        fn read(
            &self,
            _s: &SshJobSpec,
            remote_dir: &str,
            name: &str,
        ) -> Result<Option<String>, Link> {
            self.up()?;
            Ok(fs::read_to_string(self.rpath(remote_dir).join(name)).ok())
        }

        fn stop(&self, _s: &SshJobSpec, remote_dir: &str, pid: u32) -> Result<bool, Link> {
            self.up()?;
            self.stops.fetch_add(1, Ordering::SeqCst);
            let dir = self.rpath(remote_dir);
            let payload = fs::read_to_string(dir.join("payload.pid"))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());
            if let Some(pp) = payload {
                proc::stop_pid(pp, Duration::from_millis(500));
            }
            let dead = proc::stop_pid(pid, Duration::from_millis(500));
            Ok(dead && !proc::is_alive(pid))
        }
    }

    // ---------- 测试基座 ----------

    fn store_and_dirs(tag: &str) -> (JobStore, PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("momo-ssh-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = JobStore::new(dir.join("jobs"));
        let remote = dir.join("remote");
        let models = dir.join("models");
        fs::create_dir_all(&remote).unwrap();
        fs::create_dir_all(&models).unwrap();
        (store, dir, remote, models)
    }

    #[cfg(unix)]
    fn cfg(models: &Path) -> SshCfg {
        SshCfg {
            poll: Duration::from_millis(15),
            recover_poll: Duration::from_millis(15),
            ckpt_poll: Duration::from_millis(30),
            artifact_lead: Duration::from_secs(3600),
            models_dir: models.to_path_buf(),
        }
    }

    fn submit_spec(remote_dir: &str) -> SshSubmit {
        SshSubmit {
            host: "fake-host".into(),
            remote_dir: Some(remote_dir.into()),
            ..Default::default()
        }
    }

    /// 等条件满足或超时（monitor 线程驱动状态用）。
    #[cfg(unix)]
    fn wait_for(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[cfg(unix)]
    fn status_of(store: &JobStore, id: &str) -> JobStatus {
        store.load(id).unwrap().unwrap().status
    }

    // ---------- 用例 ----------

    /// env 白名单：敏感键丢弃 + warn，安全键进 env.sh。
    #[test]
    fn env_whitelist_drops_sensitive_keys() {
        let mut env = BTreeMap::new();
        env.insert("HF_ENDPOINT".into(), Some("https://hf-mirror.com".into()));
        env.insert("MOMO_LLM_API_KEY".into(), Some("sk-secret".into()));
        env.insert("HF_TOKEN".into(), Some("tok".into()));
        env.insert("AWS_SECRET_ACCESS_KEY".into(), Some("x".into()));
        env.insert("9BAD".into(), Some("v".into()));
        env.insert("MISSING_VAR_XYZ".into(), None);
        let (kept, warns) = resolve_env(&env);
        assert_eq!(kept.len(), 1);
        assert!(kept.contains_key("HF_ENDPOINT"));
        for k in ["MOMO_LLM_API_KEY", "HF_TOKEN", "AWS_SECRET_ACCESS_KEY"] {
            assert!(warns.iter().any(|w| w.contains(k)), "{warns:?}");
        }
        assert!(warns.iter().any(|w| w.contains("9BAD")));
        assert!(warns.iter().any(|w| w.contains("MISSING_VAR_XYZ")));
    }

    /// bundle staging：run.sh 含 argv 与 pid/exit.code 记录；env.sh 不含敏感值。
    #[test]
    fn stage_bundle_writes_supervisor_and_env() {
        let (store, dir, _remote, _models) = store_and_dirs("stage");
        let rec = store
            .create(
                JobKind::Train,
                vec!["echo".into(), "hi there".into()],
                String::new(),
            )
            .unwrap();
        let bundle = store.bundle_dir(&rec.id).unwrap();
        let mut env = BTreeMap::new();
        env.insert("EPOCHS".to_string(), "3".to_string());
        stage_bundle(&bundle, &rec.args, &env, &[]).unwrap();

        let run = fs::read_to_string(bundle.join("payload").join(RUN_SH)).unwrap();
        assert!(run.contains("echo $$ > job.pid"), "{run}");
        assert!(run.contains("'echo' 'hi there' &"), "{run}");
        assert!(run.contains("exit.code"), "{run}");
        let env_sh = fs::read_to_string(bundle.join("payload").join("env.sh")).unwrap();
        assert!(env_sh.contains("export EPOCHS='3'"), "{env_sh}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 全链路：submit staging → exec_with(fake) → succeeded；
    /// 日志进 `logs/<id>.log`（验收「ssh job 日志可达」）、
    /// artifacts 拉到 models_dir 且 models.lock 有登记。
    #[test]
    #[cfg(unix)] // exec_with 需 ssh/rsync，Windows 平台门拒绝
    fn exec_success_pulls_logs_and_registers_artifacts() {
        let (store, dir, _remote, models) = store_and_dirs("ok");
        let payload = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo hello-remote; mkdir -p artifacts ckpt; echo w > artifacts/w.bin; \
             echo '{\"job\":\"x\"}' > artifacts/train-manifest.json; \
             echo c > ckpt/step1.bin; echo done-remote"
                .to_string(),
        ];
        let mut sub = submit_spec("rj");
        sub.artifact_name = Some("fine-tunes/test-model".into());
        // 不 spawn helper：直接 stage + 手动建 record 等价物——
        // submit 会 spawn 真 helper，走 store.create + stage_bundle 手动路径。
        let mut rec = store
            .create(JobKind::Train, payload.clone(), "t".into())
            .unwrap();
        rec.executor = crate::jobs::JobExecutor::Ssh;
        rec.ssh = Some(SshJobSpec {
            host: "fake-host".into(),
            user: None,
            key: None,
            remote_dir: "rj".into(),
            env_keys: vec![],
            artifact_name: sub.artifact_name.take(),
            release_ms: None,
        });
        store.save(&rec).unwrap();
        stage_bundle(
            &store.bundle_dir(&rec.id).unwrap(),
            &rec.args,
            &BTreeMap::new(),
            &[],
        )
        .unwrap();

        let fake = FakeTransport::new(dir.join("remote"));
        assert_eq!(exec_with(&store, &rec.id, &fake, &cfg(&models)).unwrap(), 0);

        let done = store.load(&rec.id).unwrap().unwrap();
        assert_eq!(done.status, JobStatus::Succeeded, "{done:?}");
        assert_eq!(done.exit, Some(0));
        assert!(done.remote_id.is_some(), "remote job id recorded");

        // 日志可达：远端 run.log 增量进了 store 的 logs/<id>.log
        let log = fs::read_to_string(&done.log_path).unwrap();
        assert!(log.contains("hello-remote"), "log: {log}");
        assert!(log.contains("done-remote"), "log: {log}");
        assert!(log.contains("[runner] remote pid"), "log: {log}");

        // ckpt 快照拉回 bundle；artifacts 登记进 models_dir + models.lock
        let bundle = store.bundle_dir(&rec.id).unwrap();
        assert!(bundle.join("ckpt/step1.bin").is_file());
        assert!(models.join("fine-tunes/test-model/w.bin").is_file());
        let lock = ModelsLock::load(&models.join("models.lock")).unwrap();
        let entry = lock
            .entries
            .iter()
            .find(|e| e.name == "fine-tunes/test-model")
            .expect("lock entry");
        assert_eq!(entry.files.len(), 2);
        assert!(entry.files.iter().any(|f| f.path == "train-manifest.json"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// payload 非零退出 → failed + exit code。
    #[test]
    #[cfg(unix)] // exec_with 需 ssh/rsync，Windows 平台门拒绝
    fn exec_failed_exit_code() {
        let (store, dir, _remote, models) = store_and_dirs("fail");
        let rec = make_ssh_job(&store, vec!["sh".into(), "-c".into(), "exit 3".into()]);
        stage_bundle(
            &store.bundle_dir(&rec.id).unwrap(),
            &rec.args,
            &BTreeMap::new(),
            &[],
        )
        .unwrap();
        let fake = FakeTransport::new(dir.join("remote"));
        exec_with(&store, &rec.id, &fake, &cfg(&models)).unwrap();
        let done = store.load(&rec.id).unwrap().unwrap();
        assert_eq!(done.status, JobStatus::Failed);
        assert_eq!(done.exit, Some(3));
        assert!(done.error.unwrap().contains("code 3"));
        let _ = fs::remove_dir_all(&dir);
    }

    fn make_ssh_job(store: &JobStore, args: Vec<String>) -> JobRecord {
        let mut rec = store.create(JobKind::Train, args, String::new()).unwrap();
        rec.executor = crate::jobs::JobExecutor::Ssh;
        rec.ssh = Some(SshJobSpec {
            host: "fake-host".into(),
            user: None,
            key: None,
            remote_dir: "rj".into(),
            env_keys: vec![],
            artifact_name: None,
            release_ms: None,
        });
        store.save(&rec).unwrap();
        rec
    }

    /// 断线 → recovering；重连确认同一进程 → running 续传 → succeeded。
    /// 全程 launch 只调一次（NEVER re-launch）。
    #[test]
    #[cfg(unix)] // exec_with 需 ssh/rsync，Windows 平台门拒绝
    fn exec_recovers_after_link_down() {
        let (store, dir, _remote, models) = store_and_dirs("recv");
        let rec = make_ssh_job(
            &store,
            vec![
                "sh".into(),
                "-c".into(),
                "sleep 1; echo after-recovery".into(),
            ],
        );
        stage_bundle(
            &store.bundle_dir(&rec.id).unwrap(),
            &rec.args,
            &BTreeMap::new(),
            &[],
        )
        .unwrap();
        let fake = FakeTransport::new(dir.join("remote"));
        let c = cfg(&models);
        std::thread::scope(|scope| {
            let st = &store;
            let ft = &fake;
            let id = rec.id.clone();
            let handle = scope.spawn(move || exec_with(st, &id, ft, &c));

            assert!(wait_for(Duration::from_secs(10), || {
                status_of(&store, &rec.id) == JobStatus::Running
            }));
            // 链路断 → recovering
            fake.set_down(true);
            assert!(wait_for(Duration::from_secs(10), || {
                status_of(&store, &rec.id) == JobStatus::Recovering
            }));
            assert_eq!(fake.launched.load(Ordering::SeqCst), 1);
            // 恢复 → running → 正常跑完 succeeded
            fake.set_down(false);
            assert!(wait_for(Duration::from_secs(10), || {
                status_of(&store, &rec.id) == JobStatus::Running
            }));
            assert!(wait_for(Duration::from_secs(10), || {
                status_of(&store, &rec.id) == JobStatus::Succeeded
            }));
            handle.join().unwrap().unwrap();
        });
        assert_eq!(fake.launched.load(Ordering::SeqCst), 1);
        let log = fs::read_to_string(store.log_path(&rec.id).unwrap()).unwrap();
        assert!(log.contains("after-recovery"), "log: {log}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 确认进程消亡（无 exit.code）→ failed「exited without status」，不重复 launch。
    #[test]
    #[cfg(unix)] // exec_with 需 ssh/rsync，Windows 平台门拒绝
    fn exec_gone_without_status_fails() {
        let (store, dir, _remote, models) = store_and_dirs("gone");
        let rec = make_ssh_job(&store, vec!["sh".into(), "-c".into(), "sleep 30".into()]);
        stage_bundle(
            &store.bundle_dir(&rec.id).unwrap(),
            &rec.args,
            &BTreeMap::new(),
            &[],
        )
        .unwrap();
        let fake = FakeTransport::new(dir.join("remote"));
        let c = cfg(&models);
        std::thread::scope(|scope| {
            let st = &store;
            let ft = &fake;
            let id = rec.id.clone();
            let handle = scope.spawn(move || exec_with(st, &id, ft, &c));

            assert!(wait_for(Duration::from_secs(10), || {
                status_of(&store, &rec.id) == JobStatus::Running
            }));
            // 直接 SIGKILL run.sh（无 exit.code 的消亡）
            let _pid: u32 = store
                .load(&rec.id)
                .unwrap()
                .unwrap()
                .remote_id
                .unwrap()
                .parse()
                .unwrap();
            #[cfg(unix)]
            unsafe {
                libc::kill(_pid as i32, libc::SIGKILL);
            }
            assert!(wait_for(Duration::from_secs(10), || {
                status_of(&store, &rec.id) == JobStatus::Failed
            }));
            handle.join().unwrap().unwrap();
        });
        let done = store.load(&rec.id).unwrap().unwrap();
        assert!(done.error.unwrap().contains("exited without status"));
        assert_eq!(fake.launched.load(Ordering::SeqCst), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// recovering job 被 `jobs exec` 接管（runner 死后 recover）：认领 →
    /// 确认同一进程 → running → 收口，不重新 launch。
    #[test]
    #[cfg(unix)] // exec_with 需 ssh/rsync，Windows 平台门拒绝
    fn exec_reattach_on_recovering() {
        let (store, dir, _remote, models) = store_and_dirs("attach");
        let rec = make_ssh_job(
            &store,
            vec![
                "sh".into(),
                "-c".into(),
                "sleep 0.2; echo reattached".into(),
            ],
        );
        let bundle = store.bundle_dir(&rec.id).unwrap();
        stage_bundle(&bundle, &rec.args, &BTreeMap::new(), &[]).unwrap();

        // 手动复现「runner 死后」现场：push + launch + 状态 recovering。
        let fake = FakeTransport::new(dir.join("remote"));
        let spec = rec.ssh.clone().unwrap();
        let rdir = remote_dir(&spec, &rec.id);
        fake.push(&spec, &rdir, &bundle.join("payload")).unwrap();
        let pid = fake.launch(&spec, &rdir).unwrap();
        let mut r = store.load(&rec.id).unwrap().unwrap();
        r.remote_id = Some(pid.to_string());
        store
            .transition(&mut r, JobStatus::Preparing, None)
            .unwrap();
        store.transition(&mut r, JobStatus::Running, None).unwrap();
        store
            .transition(&mut r, JobStatus::Recovering, None)
            .unwrap();

        exec_with(&store, &rec.id, &fake, &cfg(&models)).unwrap();
        let done = store.load(&rec.id).unwrap().unwrap();
        assert_eq!(done.status, JobStatus::Succeeded, "{done:?}");
        assert_eq!(fake.launched.load(Ordering::SeqCst), 1); // 只有手动那次
        let log = fs::read_to_string(&done.log_path).unwrap();
        assert!(log.contains("reattached"), "log: {log}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// cancel：canceling + flag → 远端 stop → 确认消亡 → canceled（独立终态）。
    #[test]
    fn cancel_stops_remote_process() {
        let (store, dir, _remote, _models) = store_and_dirs("cancel");
        let rec = make_ssh_job(&store, vec!["sh".into(), "-c".into(), "sleep 60".into()]);
        let bundle = store.bundle_dir(&rec.id).unwrap();
        stage_bundle(&bundle, &rec.args, &BTreeMap::new(), &[]).unwrap();
        let fake = FakeTransport::new(dir.join("remote"));
        let spec = rec.ssh.clone().unwrap();
        let rdir = remote_dir(&spec, &rec.id);
        fake.push(&spec, &rdir, &bundle.join("payload")).unwrap();
        let pid = fake.launch(&spec, &rdir).unwrap();
        let mut r = store.load(&rec.id).unwrap().unwrap();
        r.remote_id = Some(pid.to_string());
        r.runner_pid = Some(std::process::id()); // 伪装 helper 在场不影响 cancel
        store
            .transition(&mut r, JobStatus::Preparing, None)
            .unwrap();
        store.transition(&mut r, JobStatus::Running, None).unwrap();

        let (done, note) = cancel_with(&store, &rec.id, &fake).unwrap();
        assert_eq!(done.status, JobStatus::Canceled, "{note}");
        assert!(!proc::is_alive(pid));
        assert!(done.cancel_requested);
        assert_eq!(fake.launched.load(Ordering::SeqCst), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// recovering + cancel：只落 flag 不盲杀（不确认身份不动远端）。
    #[test]
    fn cancel_on_recovering_only_marks_intent() {
        let (store, dir, _remote, _models) = store_and_dirs("cancelrecv");
        let rec = make_ssh_job(&store, vec!["sh".into(), "-c".into(), "sleep 60".into()]);
        let mut r = store.load(&rec.id).unwrap().unwrap();
        r.remote_id = Some("12345".into());
        store
            .transition(&mut r, JobStatus::Preparing, None)
            .unwrap();
        store.transition(&mut r, JobStatus::Running, None).unwrap();
        store
            .transition(&mut r, JobStatus::Recovering, None)
            .unwrap();

        let fake = FakeTransport::new(dir.join("remote"));
        fake.set_down(true);
        let (done, note) = cancel_with(&store, &rec.id, &fake).unwrap();
        assert_eq!(done.status, JobStatus::Recovering);
        assert!(done.cancel_requested);
        assert!(note.contains("pending ssh reconnect"), "{note}");
        assert_eq!(fake.stops.load(Ordering::SeqCst), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 4090D 纪律：release_ms 已进入预拉窗口 → artifacts 快照落到 bundle。
    #[test]
    #[cfg(unix)] // exec_with 需 ssh/rsync，Windows 平台门拒绝
    fn deadline_prefetches_artifacts() {
        let (store, dir, _remote, models) = store_and_dirs("deadline");
        let rec = make_ssh_job(
            &store,
            vec![
                "sh".into(),
                "-c".into(),
                "sleep 0.4; mkdir -p artifacts; echo a > artifacts/w.bin".into(),
            ],
        );
        let mut r = store.load(&rec.id).unwrap().unwrap();
        r.ssh.as_mut().unwrap().release_ms = Some(crate::jobs::now_ms() + 30 * 60_000); // 30min < 1h lead
        store.save(&r).unwrap();
        let bundle = store.bundle_dir(&rec.id).unwrap();
        stage_bundle(&bundle, &rec.args, &BTreeMap::new(), &[]).unwrap();

        let fake = FakeTransport::new(dir.join("remote"));
        exec_with(&store, &rec.id, &fake, &cfg(&models)).unwrap();
        assert_eq!(status_of(&store, &rec.id), JobStatus::Succeeded);
        // artifacts 在窗口内才出现（0.4s 后）；快照非空 → marker 写明
        let log = fs::read_to_string(store.log_path(&rec.id).unwrap()).unwrap();
        assert!(log.contains("release deadline in"), "log: {log}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// launch 结果未确认（push 后链路断）→ recovering → find 找回 pid 续传。
    #[test]
    #[cfg(unix)] // exec_with 需 ssh/rsync，Windows 平台门拒绝
    fn launch_unconfirmed_goes_recovering_then_finds_pid() {
        let (store, dir, _remote, models) = store_and_dirs("unconf");
        // 进程需活过 monitor 首轮 find()（recover_poll 粒度 + CI 负载）：
        // 瞬时退出的命令可能在重查前消亡，导致 remote_id 永远找不回。
        let rec = make_ssh_job(
            &store,
            vec!["sh".into(), "-c".into(), "echo came-back; sleep 1".into()],
        );
        let bundle = store.bundle_dir(&rec.id).unwrap();
        stage_bundle(&bundle, &rec.args, &BTreeMap::new(), &[]).unwrap();

        // fake：push 放行、launch 报链路断但进程其实起来了——模拟「回复丢了」。
        struct HalfFake {
            inner: FakeTransport,
        }
        impl Transport for HalfFake {
            fn push(&self, s: &SshJobSpec, r: &str, l: &Path) -> Result<(), Link> {
                self.inner.push(s, r, l)
            }
            fn pull(&self, s: &SshJobSpec, r: &str, l: &Path) -> Result<bool, Link> {
                self.inner.pull(s, r, l)
            }
            fn launch(&self, s: &SshJobSpec, r: &str) -> Result<u32, Link> {
                let _pid = self.inner.launch(s, r)?; // 实际起来了
                Err(Link("reply lost".into())) // 但 pid 没回来
            }
            fn probe(&self, s: &SshJobSpec, r: &str, p: u32) -> Result<Probe, Link> {
                self.inner.probe(s, r, p)
            }
            fn find(&self, s: &SshJobSpec, r: &str) -> Result<Option<u32>, Link> {
                self.inner.find(s, r)
            }
            fn read(&self, s: &SshJobSpec, r: &str, n: &str) -> Result<Option<String>, Link> {
                self.inner.read(s, r, n)
            }
            fn stop(&self, s: &SshJobSpec, r: &str, p: u32) -> Result<bool, Link> {
                self.inner.stop(s, r, p)
            }
        }
        let fake = HalfFake {
            inner: FakeTransport::new(dir.join("remote")),
        };
        exec_with(&store, &rec.id, &fake, &cfg(&models)).unwrap();
        let done = store.load(&rec.id).unwrap().unwrap();
        assert_eq!(done.status, JobStatus::Succeeded, "{done:?}");
        assert!(done.remote_id.is_some(), "find() re-identified pid");
        let _ = fs::remove_dir_all(&dir);
    }

    /// submit 校验：坏 host/artifact_name/lease 被拒。
    #[test]
    fn submit_validation() {
        let (store, dir, _remote, _models) = store_and_dirs("valid");
        let bad = |mut s: SshSubmit| {
            s.host = "bad host;rm".into();
            submit(&store, JobKind::Train, vec!["x".into()], String::new(), s).is_err()
        };
        assert!(bad(submit_spec("rj")));
        let mut s = submit_spec("rj");
        s.artifact_name = Some("../escape".into());
        assert!(submit(&store, JobKind::Train, vec!["x".into()], String::new(), s).is_err());
        let mut s = submit_spec("rj");
        s.lease_hours = Some(-1.0);
        assert!(submit(&store, JobKind::Train, vec!["x".into()], String::new(), s).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
