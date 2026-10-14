//! 输出渲染：human 表格（着色）+ 退出码枚举 + 错误类型。
//! 报告/检查项 DTO 集中在 `crate::api::dto`（`--json` 与 REST `/api` 同型）；
//! 本模块不再定义 JSON 结构。

use std::io::{IsTerminal, Write};

use owo_colors::OwoColorize;

use crate::api::dto::{CheckStatus, CommandStatus, DoctorReport};

/// 退出码约定（momo-docs `02-repos/momo-infra.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// 成功（doctor 全 ok / bench 达标）
    Ok = 0,
    /// 运行时错误（IO、进程管理失败、未实现 stub）
    Runtime = 1,
    /// 用法/配置错误（clap 解析之外：缺 env、env-file 不存在等）
    Usage = 2,
    /// 检查未通过（doctor 有 fail / bench --strict 超标）
    CheckFailed = 3,
    /// 网络不可达（下载、端点连接失败）
    Network = 4,
    /// 校验失败（sha256 不匹配、manifest 损坏）
    Verify = 5,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("config: {0}")]
    Config(String),
    #[error("manifest: {0}")]
    Manifest(String),
    #[error("network: {0}")]
    Net(String),
}

impl AppError {
    pub fn exit_code(&self) -> i32 {
        match self {
            AppError::Io(_) => ExitCode::Runtime as i32,
            AppError::Config(_) => ExitCode::Usage as i32,
            AppError::Manifest(_) => ExitCode::Verify as i32,
            AppError::Net(_) => ExitCode::Network as i32,
        }
    }
}

/// 渲染 human 表格：`status` 着色（ok 绿 / warn 黄 / fail 红），hint 次行缩进。
pub fn render_doctor_human(report: &DoctorReport, w: &mut impl Write) -> std::io::Result<()> {
    let color = std::io::stdout().is_terminal();
    for group in &report.groups {
        writeln!(w, "== {} ==", group.name)?;
        let id_width = group.checks.iter().map(|c| c.id.len()).max().unwrap_or(0);
        for check in &group.checks {
            let label = check.status.label();
            let status = styled_status(check.status, color);
            // ANSI 转义不计入对齐宽度：手动补齐到 4 字符右对齐
            let pad = " ".repeat(4usize.saturating_sub(label.len()));
            writeln!(
                w,
                "  {pad}{status}  {:id_width$}  {}",
                check.id, check.detail
            )?;
            if let Some(hint) = &check.hint {
                writeln!(w, "  {pad}      {:id_width$}  hint: {hint}", "")?;
            }
        }
        writeln!(w)?;
    }
    writeln!(
        w,
        "summary: {} ok, {} warn, {} fail (profile={})",
        report.summary.ok, report.summary.warn, report.summary.fail, report.profile
    )
}

fn styled_status(status: CheckStatus, color: bool) -> String {
    if !color {
        return status.label().to_string();
    }
    match status {
        CheckStatus::Ok => status.label().green().to_string(),
        CheckStatus::Warn => status.label().yellow().to_string(),
        CheckStatus::Fail => status.label().red().to_string(),
    }
}

/// 未实现子命令的统一出口：human 走 stderr，--json 走 stdout；退出码 1。
pub fn not_implemented(command: &str, json: bool) -> i32 {
    if json {
        let body = CommandStatus {
            schema: crate::api::dto::SCHEMA_VERSION,
            command: command.into(),
            status: "not_implemented".into(),
        };
        println!("{}", serde_json::to_string_pretty(&body).unwrap());
    } else {
        eprintln!("momoctl {command}: not implemented yet");
    }
    ExitCode::Runtime as i32
}
