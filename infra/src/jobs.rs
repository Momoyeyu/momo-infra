//! jobs 子系统：job 描述与状态机（本模块）、store（`jobs/store.rs`）、
//! exec_local 执行器（`jobs/runner.rs`）、exec_ssh 执行器（`jobs/ssh.rs`）。
//!
//! 状态机口径锁定 momo-infra.md「训练 lifecycle 与恢复」：
//! `queued → preparing → running → succeeded/failed`，
//! `canceling/canceled` 独立终态不写成 failed；
//! `recovering` 归 exec_ssh（断线重查，#51），`promoted` 属 promotion issue。
//!
//! 面向用户输出英文；源码注释中文。

pub mod runner;
pub mod ssh;
pub mod store;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// job 来源域（JobRecord.kind）。`exec_local` 是执行器不是 kind。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Pull,
    Serve,
    Train,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Pull => "pull",
            JobKind::Serve => "serve",
            JobKind::Train => "train",
        }
    }
}

/// 执行器（JobRecord.executor）。serde 小写；旧 meta 无此字段 → `local`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum JobExecutor {
    // exec_local：本地子进程 runner（`jobs/runner.rs`）。
    #[default]
    #[value(help = "local child process runner")]
    Local,
    // exec_ssh：rsync 推 bundle + ssh 远端执行（`jobs/ssh.rs`）。
    #[value(help = "remote execution over ssh + rsync")]
    Ssh,
}

impl JobExecutor {
    pub fn as_str(self) -> &'static str {
        match self {
            JobExecutor::Local => "local",
            JobExecutor::Ssh => "ssh",
        }
    }
}

/// exec_ssh 的远端连接规格（JobRecord.ssh；submit 时校验后落盘，
/// helper 重启/接管后凭它重建远端命令——host 别名、user、key 不依赖本次会话）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshJobSpec {
    /// ssh 目标：Host 别名（`4090D`/`mac-studio`）或 hostname；认证走 `~/.ssh/config`。
    pub host: String,
    /// 显式登录用户；缺省交给 ssh config / 本机用户。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// 显式 identity 文件；缺省交给 ssh config。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<PathBuf>,
    /// 远端工作根（job 目录 = `<remote_dir>/<id>`），远端 shell 语义（`~` 可展开）。
    pub remote_dir: String,
    /// 推入 env.sh 的白名单键名（只存名不存值——值在 bundle 里，不进 meta/API）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_keys: Vec<String>,
    /// 成功收尾时登记到 `${MOMO_MODELS_DIR}` 的产物名（相对路径）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_name: Option<String>,
    /// 远端机器预计释放时刻（epoch ms；4090D 的 24h 纪律，`--lease-hours` 换算）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_ms: Option<u64>,
}

/// job 状态（store 与 API 的序列化值即小写名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Preparing,
    Running,
    Canceling,
    Recovering,
    Succeeded,
    Failed,
    Canceled,
    Promoted,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Preparing => "preparing",
            JobStatus::Running => "running",
            JobStatus::Canceling => "canceling",
            JobStatus::Recovering => "recovering",
            JobStatus::Succeeded => "succeeded",
            JobStatus::Failed => "failed",
            JobStatus::Canceled => "canceled",
            JobStatus::Promoted => "promoted",
        }
    }

    /// 终态：succeeded/failed/canceled/promoted（canceled 独立终态，不并入 failed）。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Succeeded | JobStatus::Failed | JobStatus::Canceled | JobStatus::Promoted
        )
    }

    /// 合法迁移表。momo-infra.md 状态图：
    /// - queued → preparing（launch）/ canceled（launch 前取消）；
    ///   canceling 为带 runner 的取消路径，queued/preparing → canceling 同样合法；
    /// - preparing → running（spawn 成功+process identity 已记）/ failed / canceling / canceled /
    ///   recovering（exec_ssh launch 结果未确认——命令可能已到达远端，须重查不得重发，#51）；
    /// - running → canceling / recovering / succeeded / failed；
    /// - canceling → canceled（确认停止）/ failed / recovering / running（rescan 确认存活）；
    /// - recovering → running / failed / canceled（SSH runner 的重连收口，#51）；
    /// - failed → preparing（retry from checkpoint，预留给 retry 命令）；
    /// - succeeded → promoted（frozen gate + 显式 promote，预留给 promotion issue）。
    pub fn can_transition(self, to: JobStatus) -> bool {
        use JobStatus::*;
        match self {
            Queued => matches!(to, Preparing | Canceling | Canceled | Failed),
            Preparing => matches!(to, Running | Failed | Canceling | Canceled | Recovering),
            Running => matches!(to, Canceling | Recovering | Succeeded | Failed),
            Canceling => matches!(to, Canceled | Failed | Running | Recovering),
            Recovering => matches!(to, Running | Failed | Canceled),
            Failed => matches!(to, Preparing),
            Succeeded => matches!(to, Promoted),
            Canceled | Promoted => false,
        }
    }
}

/// meta/<id>.json 全量记录（#54 JobRecord 口径）。
/// `created_at`/`updated_at` 为 unix epoch 毫秒；`log_path` 为绝对路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub kind: JobKind,
    pub desc: String,
    /// payload argv（exec_local：args[0] 为本地可执行程序；
    /// exec_ssh：打包进 run.sh 远端执行）。
    pub args: Vec<String>,
    /// 执行器；旧 meta 无字段 → `local`。
    #[serde(default)]
    pub executor: JobExecutor,
    /// exec_ssh 远端规格（本地 job 恒为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshJobSpec>,
    pub status: JobStatus,
    pub created_at: u64,
    pub updated_at: u64,
    /// SSH runner 的远端 job id（#51；本地 job 恒为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
    /// payload 子进程 pid（process identity）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// exec_local runner（`momoctl jobs exec` helper）pid。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_pid: Option<u32>,
    pub log_path: String,
    /// payload 退出码；signal 终止为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    /// failed 的原因（spawn/exit/interrupted 等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// cancel 意图标记：状态被 reconcile 翻成 recovering 后 canceling 会丢，
    /// flag 持久保存「用户要停它」，helper 重连后据此收口（exec_ssh）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub cancel_requested: bool,
}

/// `skip_serializing_if` 用的 bool 判空（serde 需要路径引用）。
fn is_false(v: &bool) -> bool {
    !*v
}

/// unix epoch 毫秒（record 时间戳与 jobs.jsonl ts 共用）。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 状态机：关键迁移合法、明显非法迁移被拒。
    #[test]
    fn transition_table_matches_doc() {
        use JobStatus::*;
        assert!(Queued.can_transition(Preparing));
        assert!(Queued.can_transition(Canceled));
        assert!(Preparing.can_transition(Running));
        assert!(Running.can_transition(Succeeded));
        assert!(Running.can_transition(Canceling));
        assert!(Canceling.can_transition(Canceled));
        assert!(Failed.can_transition(Preparing));
        assert!(Succeeded.can_transition(Promoted));
        // canceled 是独立终态；running 不得直达 canceled
        assert!(!Running.can_transition(Canceled));
        assert!(!Canceled.can_transition(Failed));
        assert!(!Succeeded.can_transition(Failed));
    }

    /// 终态集合与文档一致。
    #[test]
    fn terminal_states() {
        use JobStatus::*;
        for s in [Succeeded, Failed, Canceled, Promoted] {
            assert!(s.is_terminal(), "{s:?}");
        }
        for s in [Queued, Preparing, Running, Canceling, Recovering] {
            assert!(!s.is_terminal(), "{s:?}");
        }
    }
}
