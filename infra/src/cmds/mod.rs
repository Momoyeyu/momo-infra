//! 子命令实现。`Ctx` 为 dispatch 时注入的全局上下文。

pub mod bench;
pub mod config;
pub mod doctor;
pub mod gc;
pub mod jobs;
pub mod pipeline;
pub mod pull;
pub mod serve;
pub mod switch;
pub mod web;

#[derive(Debug, Clone)]
pub struct Ctx {
    pub json: bool,
    /// 显式 `--env-file`（switch 重指符号链接时定位 .env）。
    pub env_file: Option<std::path::PathBuf>,
}
