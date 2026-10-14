//! `/api/jobs` handler 集：REST（submit/list/detail/cancel/promote/metrics）
//! 与 SSE 日志 tail。handler 只做 HTTP↔core 适配：业务全部走
//! `jobs::store`/`jobs::runner`/`pipeline`/`env_file`，JSON 顶层恒 `schema:1`，
//! 与 CLI `--json` 同口径。
//!
//! 训练页（#42）扩展：
//! - `POST /api/jobs` 支持 `{pipeline, set, dry_run}` 快捷提交——服务端把
//!   pipeline 定义展开为 `<momoctl> pipeline run --name <p> --set k=v ...`
//!   argv（与 `args` 互斥）；`executor` 非 `local` → 422（exec_ssh 属 #51）；
//! - `POST /api/jobs/{id}/promote`：`succeeded` + 产物可解析（pipeline run
//!   或显式 `artifact_dir`，须含 train-manifest.json）→ 声明的 activate_env
//!   写回 `.env.<active>`（.bak + 原子写，rollback 保留）→ `promoted`；
//! - `GET /api/jobs/{id}/metrics`：关联 pipeline run 的 step metrics +
//!   `train-manifest.json` 原文（loss 曲线数据源；无则字段为 null）。

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio_stream::wrappers::ReceiverStream;

use super::api::error;
use super::WebState;
use crate::api::dto;
use crate::env_file;
use crate::jobs::runner;
use crate::jobs::store::{self, JobStore};
use crate::jobs::JobStatus;
use crate::pipeline::{self, RunState};

/// `/api/jobs` 子路由：挂到 api.rs 的 `nest("/jobs", ..)`。
/// 需要 `WebState`（promote 的 env 写回要定位 `.env.<profile>`）。
pub fn router() -> Router<Arc<WebState>> {
    Router::new()
        .route("/", get(list_jobs).post(submit_job))
        .route("/{id}", get(get_job))
        .route("/{id}/cancel", post(cancel_job))
        .route("/{id}/recover", post(recover_job))
        .route("/{id}/promote", post(promote_job))
        .route("/{id}/metrics", get(job_metrics))
        .route("/{id}/logs", get(job_logs))
}

/// rescan（#54）：读侧 reconcile，把非终态 job 按 process identity 收口；
/// 失败只 warn 不挡请求（读侧降级，list/load 自己再报 IO 错误）。
fn reconcile(store: &JobStore) {
    match store.reconcile() {
        Ok(notes) => {
            for n in notes {
                eprintln!("warn: jobs rescan: {n}");
            }
        }
        Err(e) => eprintln!("warn: jobs rescan failed: {e}"),
    }
}

/// 统一 422 字段级错误（与 api.rs config 写回同口径）。
fn validation_failed(fields: BTreeMap<String, String>) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(dto::ApiError::with_fields(
            "validation_failed",
            "job submission failed validation",
            fields,
        )),
    )
        .into_response()
}

/// `GET /api/jobs`：全量列表（created_at 升序）。
async fn list_jobs() -> Response {
    let store = JobStore::open_default();
    reconcile(&store);
    match store.list() {
        Ok((jobs, _warnings)) => Json(dto::JobListReport {
            schema: dto::SCHEMA_VERSION,
            status: "ok",
            jobs,
        })
        .into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// `GET /api/jobs/{id}`：单条 record；未知 id → JSON 404。
async fn get_job(AxumPath(id): AxumPath<String>) -> Response {
    let store = JobStore::open_default();
    reconcile(&store);
    match store.load(&id) {
        Ok(Some(rec)) => Json(dto::JobReport {
            schema: dto::SCHEMA_VERSION,
            status: "ok",
            job: rec,
            note: None,
        })
        .into_response(),
        Ok(None) => not_found(&id),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// `POST /api/jobs`：建档 + spawn detached runner（同 `jobs submit`）。
/// `executor:"ssh"` 时 `ssh` 对象必填且 `ssh.host` 非空；
/// local 执行器带 `ssh` 字段 → 400（与 CLI `--executor` 同口径）。
/// 两种形态：原始 `{args}`，或 `{pipeline, set, dry_run}` 快捷提交——后者由
/// 服务端展开 argv（current_exe 保证 runner 用同一 binary）。
async fn submit_job(Json(body): Json<dto::JobSubmitRequest>) -> Response {
    // pipeline 快捷提交只能走 local 执行器——pipeline 定义自带 ssh step，
    // 本地 momoctl 驱动远端；把整段 argv 塞进 exec_ssh 会把编排器搬走。
    if body.pipeline.is_some()
        && !matches!(body.executor, None | Some(crate::jobs::JobExecutor::Local))
    {
        let mut fields = BTreeMap::new();
        fields.insert(
            "executor".to_string(),
            "pipeline jobs must run on the local executor (pipeline steps manage their own ssh targets)".to_string(),
        );
        return validation_failed(fields);
    }
    let (args, default_desc) = match build_pipeline_argv(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    if args.is_empty() || args[0].trim().is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "body.args must be a non-empty argv array (or use {pipeline, set})",
        );
    }
    let executor = body.executor.unwrap_or(crate::jobs::JobExecutor::Local);
    let ssh = match executor {
        crate::jobs::JobExecutor::Local => {
            if body.ssh.is_some() {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "body.ssh requires executor \"ssh\"",
                );
            }
            None
        }
        crate::jobs::JobExecutor::Ssh => match body.ssh {
            Some(s) if !s.host.trim().is_empty() => Some(s),
            _ => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "executor \"ssh\" requires body.ssh.host",
                )
            }
        },
    };
    let store = JobStore::open_default();
    let desc = body.desc.unwrap_or(default_desc);
    match runner::submit(
        &store,
        body.kind.unwrap_or(crate::jobs::JobKind::Train),
        args,
        desc,
        ssh,
    ) {
        Ok((rec, warns)) => Json(dto::JobReport {
            schema: dto::SCHEMA_VERSION,
            status: "ok",
            job: rec,
            note: if warns.is_empty() {
                None
            } else {
                Some(warns.join("; "))
            },
        })
        .into_response(),
        Err(crate::output::AppError::Config(m)) => error(StatusCode::BAD_REQUEST, "bad_request", m),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// `{pipeline,set,dry_run}` → argv；`args` 形态原样透传。
/// 校验失败 → 422/400 响应（Err 分支已是完整 Response）。
fn build_pipeline_argv(
    body: &dto::JobSubmitRequest,
) -> Result<(Vec<String>, String), Box<Response>> {
    let Some(name) = body.pipeline.as_deref().map(str::trim) else {
        return Ok((body.args.clone(), body.args.join(" ")));
    };
    if !body.args.is_empty() {
        let mut fields = BTreeMap::new();
        fields.insert(
            "args".to_string(),
            "'args' is mutually exclusive with 'pipeline'".to_string(),
        );
        return Err(Box::new(validation_failed(fields)));
    }
    if name.is_empty() {
        let mut fields = BTreeMap::new();
        fields.insert("pipeline".to_string(), "must not be empty".to_string());
        return Err(Box::new(validation_failed(fields)));
    }
    let dir = pipeline::default_pipelines_dir().ok_or_else(|| {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "pipelines dir not found (expected infra/pipelines/)",
        ))
    })?;
    let (_, def) = pipeline::load_def_by_name(&dir, name).map_err(|_| {
        let mut fields = BTreeMap::new();
        fields.insert(
            "pipeline".to_string(),
            format!("pipeline '{name}' not found in {}", dir.display()),
        );
        Box::new(validation_failed(fields))
    })?;
    // required var（无 default 且未 --set）→ 422 逐字段归因。
    let mut fields = BTreeMap::new();
    for (k, v) in &def.vars {
        if v.required && v.default.is_none() && !body.set.contains_key(k) {
            fields.insert(
                format!("set.{k}"),
                format!("required var: {}", v.description),
            );
        }
    }
    if !fields.is_empty() {
        return Err(Box::new(validation_failed(fields)));
    }
    let exe = std::env::current_exe().map_err(|e| {
        Box::new(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("resolve current exe failed: {e}"),
        ))
    })?;
    let mut args = vec![
        exe.display().to_string(),
        "pipeline".into(),
        "run".into(),
        "--name".into(),
        name.to_string(),
    ];
    if body.dry_run {
        args.push("--dry-run".into());
    }
    // BTreeMap 保序：预览行与 argv 一致。
    for (k, v) in &body.set {
        args.push("--set".into());
        args.push(format!("{k}={v}"));
    }
    Ok((args, format!("pipeline run {name}")))
}

/// `POST /api/jobs/{id}/cancel`：canceling + kill；终态幂等。
async fn cancel_job(AxumPath(id): AxumPath<String>) -> Response {
    let store = JobStore::open_default();
    reconcile(&store);
    match runner::cancel(&store, &id) {
        Ok((rec, note)) => Json(dto::JobReport {
            schema: dto::SCHEMA_VERSION,
            status: "ok",
            job: rec,
            note: Some(note.to_string()),
        })
        .into_response(),
        Err(crate::output::AppError::Config(m)) if m.contains("not found") => not_found(&id),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// `POST /api/jobs/{id}/recover`：recovering ssh job 重挂 runner（同 `jobs recover`）。
async fn recover_job(AxumPath(id): AxumPath<String>) -> Response {
    let store = JobStore::open_default();
    reconcile(&store);
    match runner::recover(&store, &id) {
        Ok((rec, note)) => Json(dto::JobReport {
            schema: dto::SCHEMA_VERSION,
            status: "ok",
            job: rec,
            note: Some(note.to_string()),
        })
        .into_response(),
        Err(crate::output::AppError::Config(m)) if m.contains("not found") => not_found(&id),
        Err(crate::output::AppError::Config(m)) => error(StatusCode::BAD_REQUEST, "bad_request", m),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}
/// `POST /api/jobs/{id}/promote {env_key?, artifact_dir?}`（momo-infra.md
/// 「训练 lifecycle」：succeeded + frozen gate 通过 + 显式动作 → promoted）。
/// 产物解析：显式 `artifact_dir` > 关联 pipeline run 的 artifact_dir；
/// gate 证据 = 目录在盘 + `train-manifest.json` 存在。激活 =
/// `activate_env`（pipeline 声明或显式 `env_key`）写回 `.env.<active>`
/// （`.bak` 备份 + tmp/rename 原子替换——rollback 目标保留）；
/// 无激活键时只做状态迁移并在 note 说明。终态幂等。
async fn promote_job(
    State(state): State<Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    body: Bytes,
) -> Response {
    let req: dto::JobPromoteRequest = if body.is_empty() {
        dto::JobPromoteRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!("invalid body: {e}"),
                )
            }
        }
    };
    let store = JobStore::open_default();
    // 先 load：未知 id 直接 404，不为不存在的 job 跑 reconcile。
    let mut rec = match store.load(&id) {
        Ok(Some(r)) => r,
        Ok(None) => return not_found(&id),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    if rec.status == JobStatus::Promoted {
        return Json(dto::JobReport {
            schema: dto::SCHEMA_VERSION,
            status: "ok",
            job: rec,
            note: Some("already promoted".into()),
        })
        .into_response();
    }
    reconcile(&store);
    if let Ok(Some(fresh)) = store.load(&id) {
        rec = fresh;
    }
    if rec.status == JobStatus::Promoted {
        return Json(dto::JobReport {
            schema: dto::SCHEMA_VERSION,
            status: "ok",
            job: rec,
            note: Some("already promoted".into()),
        })
        .into_response();
    }
    if rec.status != JobStatus::Succeeded {
        return error(
            StatusCode::CONFLICT,
            "conflict",
            format!(
                "job {} is {}; only succeeded jobs can be promoted",
                rec.id,
                rec.status.as_str()
            ),
        );
    }

    // 解析产物 + 激活键：显式入参 > pipeline run/def 推导。
    let mut artifact_dir = req.artifact_dir.clone();
    let mut env_key = req.env_key.clone();
    if let Some((pname, _date)) = pipeline_invocation(&rec.args) {
        if let Ok((_, def)) = pipeline::default_pipelines_dir()
            .ok_or(())
            .and_then(|d| pipeline::load_def_by_name(&d, &pname).map_err(|_| ()))
        {
            if env_key.is_none() {
                env_key = def.activate_env.clone();
            }
        }
        if let Some((run_id, run)) = find_run_for_job(&pname, &rec.id) {
            if run.status != pipeline::RunStatus::Succeeded {
                return error(
                    StatusCode::CONFLICT,
                    "conflict",
                    format!(
                        "pipeline run {run_id} is {}; frozen gate / register not passed",
                        run.status.as_str()
                    ),
                );
            }
            if artifact_dir.is_none() {
                artifact_dir = Some(run.artifact_dir.clone());
            }
        }
    }
    let Some(artifact_dir) = artifact_dir else {
        return error(
            StatusCode::CONFLICT,
            "conflict",
            format!(
                "cannot resolve artifact for job {}; pass \"artifact_dir\" explicitly",
                rec.id
            ),
        );
    };
    let artifact = PathBuf::from(&artifact_dir);
    if !artifact.is_dir() {
        return error(
            StatusCode::CONFLICT,
            "conflict",
            format!("artifact dir missing: {artifact_dir}"),
        );
    }
    if !artifact.join("train-manifest.json").is_file() {
        return error(
            StatusCode::CONFLICT,
            "conflict",
            format!("train-manifest.json missing in {artifact_dir} (frozen gate evidence)"),
        );
    }

    // 激活：env 键 + 产物目录齐了才写 `.env.<active>`（原子替换 + .bak）。
    let mut activation = String::new();
    if let Some(key) = env_key {
        match activate_env(&state, &key, &artifact_dir) {
            Ok(desc) => activation = desc,
            Err(resp) => return *resp,
        }
    }

    let detail = if activation.is_empty() {
        format!("promoted: artifact {artifact_dir} registered (no activation env)")
    } else {
        format!("promoted: {activation}")
    };
    if let Err(e) = store.transition(&mut rec, JobStatus::Promoted, Some(detail.clone())) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string());
    }
    Json(dto::JobReport {
        schema: dto::SCHEMA_VERSION,
        status: "ok",
        job: rec,
        note: Some(format!(
            "{detail}; rollback: restore the .env.<profile>.bak or POST /api/config/rollback"
        )),
    })
    .into_response()
}

/// promote 的 env 激活写回：schema 校验 → .bak → 原子替换 → 返回描述串。
/// 写目标与配置页同一 `.env.<active>`（env_file 白名单路径，无穿越面）。
fn activate_env(state: &WebState, key: &str, artifact_dir: &str) -> Result<String, Box<Response>> {
    if !key.starts_with("MOMO_") {
        let mut fields = BTreeMap::new();
        fields.insert("env_key".to_string(), "key must start with MOMO_".into());
        return Err(Box::new(validation_failed(fields)));
    }
    let env_dir = state.env_link.parent().unwrap_or(Path::new("."));
    let schema = env_file::load_schema(env_dir).map_err(|e| {
        Box::new(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("config schema load failed: {e}"),
        ))
    })?;
    match schema.get(key) {
        None => {
            let mut fields = BTreeMap::new();
            fields.insert(
                "env_key".to_string(),
                "unknown key (not in config.schema.toml)".into(),
            );
            return Err(Box::new(validation_failed(fields)));
        }
        Some(spec) => {
            if let Err(msg) = env_file::validate_value(spec, artifact_dir) {
                let mut fields = BTreeMap::new();
                fields.insert("env_key".to_string(), msg);
                return Err(Box::new(validation_failed(fields)));
            }
        }
    }
    let profile = env_file::active_profile();
    let env_path = env_file::profile_env_path(&state.env_link, &profile).map_err(|e| {
        Box::new(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            e.to_string(),
        ))
    })?;
    if !env_path.is_file() {
        return Err(Box::new(error(
            StatusCode::CONFLICT,
            "conflict",
            format!(
                "{} not found; run `momoctl switch --to {profile}` to create it",
                env_path.display()
            ),
        )));
    }
    let text = std::fs::read_to_string(&env_path).map_err(|e| {
        Box::new(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("read {}: {e}", env_path.display()),
        ))
    })?;
    let bak = env_file::backup_file(&env_path).map_err(|e| {
        Box::new(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            e.to_string(),
        ))
    })?;
    let new_text = env_file::apply_changes(
        &text,
        &BTreeMap::from([(key.to_string(), artifact_dir.to_string())]),
    );
    env_file::atomic_write(&env_path, &new_text).map_err(|e| {
        Box::new(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            e.to_string(),
        ))
    })?;
    Ok(format!(
        "{key} -> {artifact_dir} in {} (backup {})",
        env_path.display(),
        bak.display()
    ))
}

/// `GET /api/jobs/{id}/metrics`：loss 曲线数据源。
/// 关联规则：args 解析 `pipeline run --name <p>` → 该 pipeline 首个
/// run_id >= job id 的 run（ULID 字典序≈时间序；找不到取最新）。
/// 无 pipeline 关联时兜底 `<log 所在目录>/train-manifest.json`。
async fn job_metrics(AxumPath(id): AxumPath<String>) -> Response {
    let store = JobStore::open_default();
    let rec = match store.load(&id) {
        Ok(Some(r)) => r,
        Ok(None) => return not_found(&id),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    let mut report = dto::JobMetricsReport {
        schema: dto::SCHEMA_VERSION,
        status: "ok",
        job: rec.id.clone(),
        run_id: None,
        artifact_dir: None,
        manifest: None,
        steps: Vec::new(),
    };
    if let Some((pname, _)) = pipeline_invocation(&rec.args) {
        if let Some((run_id, run)) = find_run_for_job(&pname, &rec.id) {
            report.run_id = Some(run_id);
            report.artifact_dir = Some(run.artifact_dir.clone());
            report.manifest = read_train_manifest(Path::new(&run.artifact_dir));
            report.steps = run
                .steps
                .iter()
                .map(|s| dto::JobStepMetrics {
                    name: s.name.clone(),
                    status: s.status.as_str().to_string(),
                    metrics: s.metrics.clone(),
                })
                .collect();
        }
    }
    if report.manifest.is_none() {
        // 非 pipeline job：logs/<id>.log 同目录的 train-manifest.json 兜底。
        if let Some(dir) = Path::new(&rec.log_path).parent() {
            if let Some(m) = read_train_manifest(dir) {
                report.manifest = Some(m);
            }
        }
    }
    Json(report).into_response()
}

/// 解析 args 中的 pipeline 调用：`[..., "pipeline", "run", "--name", <p>, ...]`。
/// `--name=X` / `--date=X` 等号形态同样识别；不匹配返回 None。
fn pipeline_invocation(args: &[String]) -> Option<(String, Option<String>)> {
    let mut saw_pipeline = false;
    let mut saw_run = false;
    let mut name = None;
    let mut date = None;
    for (i, a) in args.iter().enumerate() {
        if a == "pipeline" {
            saw_pipeline = true;
        } else if saw_pipeline && a == "run" {
            saw_run = true;
        } else if let Some(v) = a.strip_prefix("--name=") {
            name = Some(v.to_string());
        } else if a == "--name" {
            name = args.get(i + 1).cloned();
        } else if let Some(v) = a.strip_prefix("--date=") {
            date = Some(v.to_string());
        } else if a == "--date" {
            date = args.get(i + 1).cloned();
        }
    }
    if saw_pipeline && saw_run {
        name.map(|n| (n, date))
    } else {
        None
    }
}

/// job → pipeline run：run_id 字典序 >= job id 的最小 run；更早的全不匹配
/// 时取最新一个（job 先于 run 建档的正常时序 / 手工补 run 的兜底）。
fn find_run_for_job(pipeline_name: &str, job_id: &str) -> Option<(String, RunState)> {
    let runs = pipeline::list_runs(&pipeline::runs_root(), pipeline_name);
    let jid = job_id.to_lowercase();
    let pick = runs
        .iter()
        .find(|d| {
            d.file_name()
                .map(|n| n.to_string_lossy().as_ref() >= jid.as_str())
                .unwrap_or(false)
        })
        .or_else(|| runs.last())?;
    let run = pipeline::load_run(pick).ok()?;
    let run_id = pick
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| run.run_id.clone());
    Some((run_id, run))
}

/// `<dir>/train-manifest.json` 解析（缺失/坏 JSON → None）。
fn read_train_manifest(dir: &Path) -> Option<serde_json::Value> {
    let path = dir.join("train-manifest.json");
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}
/// `GET /api/jobs/{id}/logs?cursor=N`：SSE tail。
/// 每条 `event: log` 携带 `{cursor, chunk}` 且 `id:` 帧 = 消费后字节 offset
/// （与 `serve logs/{name}/stream` 同约定，断线以 ?cursor=/Last-Event-ID 续传）；
/// job 终态且日志排空后发 `event: end`（携带最终 status）并关闭流。
/// cursor 越过文件长度（截断）时从 0 重读。未知 id → JSON 404（非 SSE）。
async fn job_logs(AxumPath(id): AxumPath<String>, Query(q): Query<LogQuery>) -> Response {
    let store = JobStore::open_default();
    reconcile(&store);
    let rec = match store.load(&id) {
        Ok(Some(r)) => r,
        Ok(None) => return not_found(&id),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    let path = std::path::PathBuf::from(&rec.log_path);
    let mut cursor = q.cursor.unwrap_or(0);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(16);
    tokio::spawn(async move {
        loop {
            match store::read_chunk(&path, cursor) {
                Ok((cur, chunk)) => {
                    cursor = cur;
                    if !chunk.is_empty() {
                        let data = serde_json::to_string(&dto::JobLogChunk { cursor: cur, chunk })
                            .expect("job log chunk serialize");
                        if tx
                            .send(Ok(Event::default()
                                .event("log")
                                .id(cur.to_string())
                                .data(data)))
                            .await
                            .is_err()
                        {
                            return; // 客户端断开
                        }
                    }
                }
                Err(e) => {
                    let data =
                        serde_json::to_string(&dto::ApiError::new("internal", e.to_string()))
                            .expect("api error serialize");
                    let _ = tx
                        .send(Ok(Event::default().event("error").data(data)))
                        .await;
                    return;
                }
            }
            let terminal = store
                .load(&id)
                .ok()
                .flatten()
                .is_some_and(|r| r.status.is_terminal());
            if terminal {
                // 终态：本轮增量已发完即可关流。
                let status = store
                    .load(&id)
                    .ok()
                    .flatten()
                    .map(|r| r.status.as_str().to_string())
                    .unwrap_or_else(|| "unknown".into());
                let _ = tx
                    .send(Ok(Event::default().event("end").data(
                        serde_json::to_string(&dto::JobLogEnd { status })
                            .expect("job log end serialize"),
                    )))
                    .await;
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    });
    Sse::new(ReceiverStream::new(rx)).into_response()
}

#[derive(Debug, Deserialize)]
struct LogQuery {
    #[serde(default)]
    cursor: Option<u64>,
}

fn not_found(id: &str) -> Response {
    error(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("job {id} not found"),
    )
}
