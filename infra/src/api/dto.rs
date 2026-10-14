//! 统一 DTO：CLI `--json` 输出与 REST `/api` 响应共用同一套 serde 类型
//! （issue #46：cmds 各报告类型迁入，web handler 只做 HTTP↔core 适配）。
//! 顶层恒 `"schema": 1`；错误统一 `{schema, error:{code,message}}` 形态
//! （tests/api_contract_test.rs 锁定字段名/类型/必选性）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config_schema::SourceReport;
use crate::jobs::ssh::SshSubmit;
use crate::jobs::{JobExecutor, JobKind, JobRecord};

/// 全部报告/错误顶层的 schema 版本号。
pub const SCHEMA_VERSION: u8 = 1;

// ---------- 检查矩阵基元（doctor/serve/pull/bench/gc/switch 共用） ----------

/// 检查项状态：`ok`/`warn`/`fail`（serde 小写）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

impl CheckStatus {
    /// human 渲染标签（output::render_* 用）。
    pub(crate) fn label(self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
        }
    }
}

/// 单项检查。`hint` 恒出现在 JSON（schema 稳定），可为 null。
#[derive(Debug, Serialize)]
pub struct Check {
    pub id: String,
    pub status: CheckStatus,
    pub detail: String,
    pub hint: Option<String>,
}

impl Check {
    pub fn ok(id: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: CheckStatus::Ok,
            detail: detail.into(),
            hint: None,
        }
    }

    pub fn warn(id: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: CheckStatus::Warn,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    pub fn fail(id: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: CheckStatus::Fail,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
}

/// doctor 分组（system/toolchain/python/network/env/models/endpoints）。
#[derive(Debug, Serialize)]
pub struct CheckGroup {
    pub name: String,
    pub checks: Vec<Check>,
}

impl CheckGroup {
    pub fn new(name: impl Into<String>, checks: Vec<Check>) -> Self {
        Self {
            name: name.into(),
            checks,
        }
    }
}

/// ok/warn/fail 计数汇总。
#[derive(Debug, Serialize)]
pub struct Summary {
    pub ok: usize,
    pub warn: usize,
    pub fail: usize,
}

impl Summary {
    /// 从检查项列表计数（cmds 各命令与 DoctorReport 共用口径）。
    pub fn from_checks<'a>(checks: impl IntoIterator<Item = &'a Check>) -> Self {
        let mut s = Self {
            ok: 0,
            warn: 0,
            fail: 0,
        };
        for c in checks {
            match c.status {
                CheckStatus::Ok => s.ok += 1,
                CheckStatus::Warn => s.warn += 1,
                CheckStatus::Fail => s.fail += 1,
            }
        }
        s
    }
}

// ---------- 子命令报告（CLI `--json` 顶层对象；REST 复用同型） ----------

/// `momoctl doctor --json` / `GET /api/doctor`：
/// `{schema, profile, groups:[{name, checks:[{id,status,detail,hint}]}], summary}`。
#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub schema: u8,
    pub profile: String,
    pub groups: Vec<CheckGroup>,
    pub summary: Summary,
}

impl DoctorReport {
    pub fn new(profile: &str, groups: Vec<CheckGroup>) -> Self {
        let summary = Summary::from_checks(groups.iter().flat_map(|g| g.checks.iter()));
        Self {
            schema: SCHEMA_VERSION,
            profile: profile.to_string(),
            groups,
            summary,
        }
    }

    pub fn has_fail(&self) -> bool {
        self.summary.fail > 0
    }
}

/// `momoctl serve <up|down|status|restart> --json` / `GET /api/serve/status`。
#[derive(Debug, Serialize)]
pub struct ServeReport {
    pub schema: u8,
    pub command: String,
    pub profile: String,
    pub checks: Vec<Check>,
    pub summary: Summary,
}

/// `GET /api/serve/components` 单组件条目（内置 profile 组件表）。
#[derive(Debug, Serialize)]
pub struct ComponentInfo {
    pub name: String,
    pub order: u32,
    /// 外部端点：只探活不拉起。
    pub external: bool,
    /// 仅当 MOMO_LLM_ENDPOINT 指 loopback 时才拉起。
    pub local_endpoint_only: bool,
}

/// `GET /api/serve/components`：组件表（order 升序）+ profile。
#[derive(Debug, Serialize)]
pub struct ServeComponentsReport {
    pub schema: u8,
    pub profile: String,
    pub components: Vec<ComponentInfo>,
}

/// `POST /api/serve/{up|down|restart}` 请求体；`only` 为空 = 全组件。
#[derive(Debug, Deserialize)]
pub struct ServeActionRequest {
    #[serde(default)]
    pub only: Vec<String>,
}

/// `serve logs` 单组件条目。
#[derive(Debug, Serialize)]
pub struct LogEntry {
    pub component: String,
    /// 日志文件路径；尚无日志文件时为 null。
    pub path: Option<String>,
    pub lines: Vec<String>,
}

/// `momoctl serve logs --json` 顶层对象。
#[derive(Debug, Serialize)]
pub struct ServeLogsReport {
    pub schema: u8,
    pub command: String,
    pub profile: String,
    pub logs: Vec<LogEntry>,
}

/// `serve logs -f --json` 的 NDJSON 行（每批一条）。
#[derive(Debug, Serialize)]
pub struct LogBatch<'a> {
    pub component: &'a str,
    pub lines: Vec<&'a str>,
}

/// `GET /api/models` 单文件状态（MANIFEST + models.lock merge）。
#[derive(Debug, Serialize)]
pub struct ModelFileStatus {
    /// MANIFEST 声明的相对路径。
    pub path: String,
    /// `missing`（不在盘）/ `unverified`（在盘但未过 lock/manifest 校验）/
    /// `verified`（在盘且 lock 记录 size 一致）。
    pub state: &'static str,
    /// MANIFEST 声明的期望字节数（未声明为 null）。
    pub expected_size: Option<u64>,
    /// 磁盘实际字节数（文件缺失为 null）。
    pub actual_size: Option<u64>,
    /// MANIFEST sha256 为占位 `TODO`（无法内容校验）。
    pub sha256_placeholder: bool,
}

/// `GET /api/models` 单模型条目。
#[derive(Debug, Serialize)]
pub struct ModelStatus {
    pub name: String,
    pub repo_id: String,
    pub revision: String,
    pub dest: String,
    pub notes: Option<String>,
    /// `missing`（无文件在盘）/ `partial`（部分在盘或有未验证项）/
    /// `verified`（全部文件在盘且校验一致）。
    pub state: &'static str,
    /// 当前激活选择是否引用本条目（MOMO_ASR_MODEL / MOMO_LLM_MODEL /
    /// MOMO_TTS_VOICE 任一命中 name）。
    pub active: bool,
    /// 人工核对项：占位 sha256、lock 与 MANIFEST 不一致等。
    pub warn: Vec<String>,
    pub files: Vec<ModelFileStatus>,
}

/// 激活选择快照（只读；写回走 #56 `/api/config` 通路，未实装）。
/// 对应 env：ASR=`MOMO_ASR_PROVIDER`/`MOMO_ASR_MODEL`，
/// 声线=`MOMO_TTS_PROVIDER`/`MOMO_TTS_VOICE`/`MOMO_TTS_LANG`，
/// 大脑=`MOMO_LLM_ENDPOINT`/`MOMO_LLM_MODEL`。未设置为 null。
#[derive(Debug, Serialize)]
pub struct ActiveSelection {
    pub asr_provider: Option<String>,
    pub asr_model: Option<String>,
    pub tts_provider: Option<String>,
    pub tts_voice: Option<String>,
    pub tts_lang: Option<String>,
    pub brain_endpoint: Option<String>,
    pub brain_model: Option<String>,
}

/// `GET /api/models`：MANIFEST + models.lock merge 清单 + 激活选择。
#[derive(Debug, Serialize)]
pub struct ModelsReport {
    pub schema: u8,
    /// MANIFEST.toml 定位结果；未找到为 null（models 为空 + warn 提示）。
    pub manifest: Option<String>,
    pub models_dir: String,
    pub models: Vec<ModelStatus>,
    pub active: ActiveSelection,
    /// 报告级提示（MANIFEST 缺失等）。
    pub warn: Vec<String>,
}

/// `POST /api/models/pull` 请求体：拉取单个 MANIFEST 条目。
#[derive(Debug, Deserialize)]
pub struct ModelsPullRequest {
    pub name: String,
}

/// `momoctl pull [--check] --json`。
#[derive(Debug, Serialize)]
pub struct PullReport {
    pub schema: u8,
    pub command: String,
    pub manifest: String,
    pub models_dir: String,
    /// 下载源（--check 模式为 null）。
    pub endpoint: Option<String>,
    pub checks: Vec<Check>,
    pub summary: Summary,
}

/// bench 单回合测量。
#[derive(Debug, Serialize)]
pub struct TurnResult {
    pub turn: u32,
    /// 发→`input.accepted`。
    pub accepted_ms: Option<u64>,
    /// 发→首个 `output.sentence`。
    pub first_sentence_ms: Option<u64>,
    /// 发→首个下行 `audio.chunk`（二进制帧）。
    pub first_audio_ms: Option<u64>,
    /// 发→`turn.finished`。
    pub finished_ms: Option<u64>,
    pub outcome: Option<String>,
    /// rejected/timeout/异常说明。
    pub note: Option<String>,
}

/// bench 阶段统计（p50/p95/max）。
#[derive(Debug, Serialize)]
pub struct StageStats {
    pub samples: usize,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
}

/// `momoctl bench --json`。
#[derive(Debug, Serialize)]
pub struct BenchReport {
    pub schema: u8,
    pub command: String,
    pub endpoint: String,
    pub turns: u32,
    pub target_first_audio_ms: u64,
    pub results: Vec<TurnResult>,
    pub stats: BTreeMap<String, StageStats>,
    pub checks: Vec<Check>,
    pub summary: Summary,
}

/// `momoctl gc --json`。
#[derive(Debug, Serialize)]
pub struct GcReport {
    pub schema: u8,
    pub command: String,
    pub models_dir: String,
    /// `dry-run`（默认）或 `delete`（--yes 且 pull 未在运行）。
    pub mode: String,
    pub keep: u32,
    /// 回收候选数与字节数（dry-run 口径为「可回收」，delete 口径为「已删除」）。
    pub candidates: usize,
    pub candidate_bytes: u64,
    pub deleted_bytes: u64,
    pub checks: Vec<Check>,
    pub summary: Summary,
}

/// `momoctl switch` 的 env 差异行；敏感键为打码值，None = 该侧未设置。
#[derive(Debug, Serialize)]
pub struct DiffEntry {
    pub key: String,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// `momoctl switch --json`。
#[derive(Debug, Serialize)]
pub struct SwitchReport {
    pub schema: u8,
    pub command: String,
    pub to: String,
    pub env_file: String,
    pub target: String,
    pub checks: Vec<Check>,
    pub env_diff: Vec<DiffEntry>,
    pub summary: Summary,
}

/// `momoctl config check --json`。
#[derive(Debug, Serialize)]
pub struct ConfigCheckReport {
    pub schema: u8,
    pub root: String,
    pub sources: Vec<SourceReport>,
    pub summary: ConfigCheckSummary,
}

#[derive(Debug, Serialize)]
pub struct ConfigCheckSummary {
    /// 被检查来源文件数（含 skipped）。
    pub sources: usize,
    /// `root` 下不存在而跳过的来源数（拆仓口径，不计漂移）。
    pub skipped: usize,
    /// 漂移条目总数。
    pub drift: usize,
}

/// `momoctl web --json` 启动对象。
#[derive(Debug, Serialize)]
pub struct WebReport {
    pub schema: u8,
    pub command: String,
    pub listen: String,
    pub url: String,
}

// ---------- pipeline（训练编排；持久化形态即 RunState，--json 与其同构） ----------

/// `pipeline list` 单条定义摘要。
#[derive(Debug, Serialize)]
pub struct PipelineInfo {
    pub name: String,
    pub description: String,
    /// `{date}` 未展开的产物相对路径（MOMO_MODELS_DIR 下）。
    pub artifact_dir: String,
    /// 产物激活 env 键（promote 写回目标；未声明为 null）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activate_env: Option<String>,
    pub steps: Vec<String>,
    /// `[vars]` 定义（web wizard 表单依据：required/default/description）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vars: Vec<PipelineVarInfo>,
    pub path: String,
}

/// `pipeline list` 的 `[vars]` 条目（BTreeMap 展开为有序数组）。
#[derive(Debug, Serialize)]
pub struct PipelineVarInfo {
    pub name: String,
    pub description: String,
    /// `--set` 未注入且无 default 时 run 拒绝（exit 2 / API 422）。
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// `pipeline list --json`。
#[derive(Debug, Serialize)]
pub struct PipelineListReport {
    pub schema: u8,
    pub command: String,
    pub dir: String,
    pub pipelines: Vec<PipelineInfo>,
}

/// `pipeline run|resume|status --json`：run 状态平铺（含 steps 数组）。
#[derive(Debug, Serialize)]
pub struct PipelineRunReport {
    pub schema: u8,
    pub command: String,
    #[serde(flatten)]
    pub run: crate::pipeline::RunState,
}

/// 未实现子命令的统一 JSON（`{schema, command, status:"not_implemented"}`）。
#[derive(Debug, Serialize)]
pub struct CommandStatus {
    pub schema: u8,
    pub command: String,
    pub status: String,
}

// ---------- REST `/api` 出入参 ----------

/// `GET /api[/]`：路由索引。
#[derive(Debug, Serialize)]
pub struct IndexReport {
    pub schema: u8,
    pub status: &'static str,
    pub routes: Vec<&'static str>,
}

/// `GET /api/healthz` / `readyz`：进程级探针。
#[derive(Debug, Serialize)]
pub struct HealthReport {
    pub schema: u8,
    pub status: &'static str,
}

/// `GET /api/csrf` 响应：进程级 CSRF token。
/// state-changing 请求须以 `X-CSRF-Token` 头原样回带（web/guard.rs）。
#[derive(Debug, Serialize)]
pub struct CsrfToken {
    pub schema: u8,
    pub csrf_token: String,
}

/// 读端点公共查询参数：`?profile=normal|degraded`（缺省读 MOMO_PROFILE env）。
#[derive(Debug, Deserialize)]
pub struct ProfileQuery {
    pub profile: Option<String>,
}

// ---------- jobs ----------

/// `POST /api/jobs` 请求体：`{kind?, executor?, ssh?, args, desc?}` 或
/// `{kind?, pipeline, set?, dry_run?, executor?, desc?}`（pipeline 快捷提交：
/// 服务端把 pipeline 定义展开为 `<momoctl> pipeline run --name <p> [--dry-run]
/// --set k=v ...` argv，与 `args` 互斥）。
#[derive(Debug, Deserialize)]
pub struct JobSubmitRequest {
    /// job 类别；缺省 `train`。
    #[serde(default)]
    pub kind: Option<JobKind>,
    /// 执行器；缺省 `local`。`ssh` 时 `ssh` 字段必填（host 至少给出）。
    #[serde(default)]
    pub executor: Option<JobExecutor>,
    /// exec_ssh 远端规格（host/user?/key?/remote_dir?/env/data/artifact_name/lease_hours）。
    /// env 的 value 为 null 时取 web server 进程 env；敏感键恒被丢弃。
    #[serde(default)]
    pub ssh: Option<SshSubmit>,
    /// payload argv（exec_local：本地可执行程序；exec_ssh：打包进 run.sh 远端执行）。
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub desc: Option<String>,
    /// pipeline 名（`infra/pipelines/<name>.toml`）；与 `args` 互斥。
    #[serde(default)]
    pub pipeline: Option<String>,
    /// pipeline `--set KEY=VALUE` 注入（required 缺项 → 422 fields）。
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// pipeline 提交追加 `--dry-run`（只展开计划不执行/登记）。
    #[serde(default)]
    pub dry_run: bool,
}

/// `GET /api/jobs` 列表响应。
#[derive(Debug, Serialize)]
pub struct JobListReport {
    pub schema: u8,
    pub status: &'static str,
    pub jobs: Vec<JobRecord>,
}

/// `POST /api/jobs/{id}/promote` 请求体（可空）：显式激活覆盖。
/// 缺省按 job args 解析 pipeline → def.activate_env + run.artifact_dir。
#[derive(Debug, Default, Deserialize)]
pub struct JobPromoteRequest {
    /// 激活 env 键覆盖（须 schema 已知 `MOMO_*` 键；值域校验同 PUT /api/config）。
    #[serde(default)]
    pub env_key: Option<String>,
    /// 产物目录覆盖（须存在且含 train-manifest.json——frozen gate 通过证据）。
    #[serde(default)]
    pub artifact_dir: Option<String>,
}

/// `GET /api/jobs/{id}/metrics`：单 step 的 metrics 拾取
/// （`<artifact>/metrics.json` / register 步登记结果）。
#[derive(Debug, Serialize)]
pub struct JobStepMetrics {
    pub name: String,
    pub status: String,
    pub metrics: serde_json::Value,
}

/// `GET /api/jobs/{id}/metrics` 响应：关联 pipeline run + train-manifest 原文。
/// 非 pipeline job / 未找到 run 时 `run_id`/`artifact_dir`/`manifest` 为 null，
/// `steps` 为空——200 而非 404（「无曲线可画」是数据不是错误）。
#[derive(Debug, Serialize)]
pub struct JobMetricsReport {
    pub schema: u8,
    pub status: &'static str,
    pub job: String,
    pub run_id: Option<String>,
    pub artifact_dir: Option<String>,
    /// `<artifact_dir>/train-manifest.json` 解析结果（loss 曲线数据源）。
    pub manifest: Option<serde_json::Value>,
    pub steps: Vec<JobStepMetrics>,
}

/// `GET /api/jobs/{id}` / `POST /api/jobs` / `POST /api/jobs/{id}/cancel|promote` 响应。
#[derive(Debug, Serialize)]
pub struct JobReport {
    pub schema: u8,
    pub status: &'static str,
    pub job: JobRecord,
    /// cancel 等操作的附加说明（幂等/降级提示）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `GET /api/jobs/{id}/logs` SSE `event: log` 的 data。
#[derive(Debug, Serialize)]
pub struct JobLogChunk {
    /// 字节偏移（重连续传用）。
    pub cursor: u64,
    pub chunk: String,
}

/// `GET /api/jobs/{id}/logs` SSE `event: end` 的 data（终态排空后关流）。
#[derive(Debug, Serialize)]
pub struct JobLogEnd {
    pub status: String,
}

/// `GET /api/serve/logs/{name}/stream` 查询参数：
/// `profile` 用于组件名校验；`cursor` 为字节偏移（SSE 断线续传，
/// 前端也可走 `Last-Event-ID` 头）。
#[derive(Debug, Deserialize)]
pub struct LogStreamQuery {
    pub profile: Option<String>,
    pub cursor: Option<u64>,
}

// ---------- 配置页（/api/config，issue #56/#81） ----------

/// `GET /api/config` 单条 env 项：value 已过掩码，敏感键绝不回显明文
/// （config.md §4：前 4 位 + `…`，短值全遮）；未设置（缺失或空值）为 null。
#[derive(Debug, Serialize)]
pub struct ConfigEntry {
    pub key: String,
    pub value: Option<String>,
    pub sensitive: bool,
    /// 文件中存在且值非空（与 EnvCfg::get 的 set/unset 口径一致）。
    pub set: bool,
}

/// `GET /api/config[?profile=]`：`.env.<profile>` 当前值视图。
/// `?profile=` 缺省按 `active_profile` 状态文件定位。
#[derive(Debug, Serialize)]
pub struct ConfigReport {
    pub schema: u8,
    /// 本次读取的档位。
    pub profile: String,
    /// `active_profile` 状态文件解析结果（serve/switch 写）。
    pub active_profile: String,
    /// 读取/写回的 `.env.<profile>` 路径。
    pub env_file: String,
    /// 目标文件是否存在（不存在时 entries 全 unset，PUT 会 404）。
    pub file_exists: bool,
    pub entries: Vec<ConfigEntry>,
}

/// `PUT /api/config` 请求体：`changes` 只放要写入的键；
/// 敏感键不回显——未变更的敏感键不要放进 changes（缺省即保持原值）。
#[derive(Debug, Deserialize)]
pub struct ConfigPutRequest {
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub changes: BTreeMap<String, String>,
}

/// `POST /api/config/rollback` 请求体（可整 body 为空）。
#[derive(Debug, Deserialize)]
pub struct ConfigRollbackRequest {
    #[serde(default)]
    pub profile: Option<String>,
}

/// `PUT /api/config` / `POST /api/config/rollback` 响应：掩码 diff + 生效提示。
#[derive(Debug, Serialize)]
pub struct ConfigWriteReport {
    pub schema: u8,
    pub profile: String,
    pub env_file: String,
    /// PUT：写前新建的 `.bak`；rollback：恢复来源的 `.bak`。
    pub backup: String,
    /// 掩码后的 env 差异（from→to；敏感键同 GET 口径打码）。
    pub diff: Vec<DiffEntry>,
    /// 「env 变更经重启生效」提示（config.md §5：配置变更 = 改 env + 重启）。
    pub restart_hint: String,
}

/// `GET /api/config/schema` 单键描述：schema 单源直出，
/// 前端据此渲染控件（type/values/sensitive/default）与分组（component）。
/// `type` 输出对外口径 `str|int|bool|enum|url|path`（schema 文件的 `string`
/// 在 API 边界统一映射为 `str`）。
#[derive(Debug, Serialize)]
pub struct ConfigKeySchema {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub default: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub values: Vec<String>,
    pub sensitive: bool,
    pub description: String,
    /// 分 section：common/app/models/infra（对齐 config.md §2.1–2.4）。
    pub component: String,
}

/// `GET /api/config/schema`：全量键 schema（config.schema.toml 单源）。
#[derive(Debug, Serialize)]
pub struct ConfigSchemaReport {
    pub schema: u8,
    pub keys: Vec<ConfigKeySchema>,
}

/// 统一错误体。
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    /// 机器可读短标识：unauthorized / forbidden / csrf / not_found /
    /// not_implemented / method_not_allowed / bad_request /
    /// validation_failed / internal。
    pub code: String,
    /// 面向用户的一句话说明（英文）。
    pub message: String,
    /// 字段级错误归因（validation_failed 时下发）：字段名 → 错误说明。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<BTreeMap<String, String>>,
}

/// 统一错误响应：`{schema:1, error:{code,message[,fields]}}`。
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub schema: u8,
    pub error: ErrorBody,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            error: ErrorBody {
                code: code.into(),
                message: message.into(),
                fields: None,
            },
        }
    }

    /// 字段级错误（422）：`error.fields` 按键名归因，前端逐字段回显。
    pub fn with_fields(
        code: impl Into<String>,
        message: impl Into<String>,
        fields: BTreeMap<String, String>,
    ) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            error: ErrorBody {
                code: code.into(),
                message: message.into(),
                fields: Some(fields),
            },
        }
    }
}
