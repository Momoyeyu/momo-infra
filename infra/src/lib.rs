//! momo-infra：`momoctl` 的实现库。
//!
//! 设计口径以 momo-docs `02-repos/momo-infra.md` 为准；
//! env 全集见 `01-architecture/config.md`，模型清单见 `05-ops/model-fetch.md`。

pub mod api;
pub mod cli;
pub mod cmds;
pub mod config_schema;
pub mod env_cfg;
pub mod env_file;
pub mod jobs;
pub mod manifest;
pub mod models;
pub mod net;
pub mod output;
pub mod pipeline;
pub mod serve;
pub mod web;

use cli::{Cli, Command};
use output::AppError;

/// cli parse 之后的主流程：加载 env → dispatch → 退出码。
pub fn run(cli: Cli) -> i32 {
    if let Err(e) = env_cfg::load_dotenv(cli.env_file.as_deref()) {
        eprintln!("error: {e}");
        return e.exit_code();
    }
    let ctx = cmds::Ctx {
        json: cli.json,
        env_file: cli.env_file.clone(),
    };
    let result = match &cli.command {
        Command::Doctor(args) => cmds::doctor::run(args, &ctx),
        Command::Pull(args) => cmds::pull::run(args, &ctx),
        Command::Serve(args) => cmds::serve::run(args, &ctx),
        Command::Switch(args) => cmds::switch::run(args, &ctx),
        Command::Bench(args) => cmds::bench::run(args, &ctx),
        Command::Gc(args) => cmds::gc::run(args, &ctx),
        Command::Pipeline(args) => cmds::pipeline::run(args, &ctx),
        Command::Config(args) => cmds::config::run(args, &ctx),
        Command::Web(args) => cmds::web::run(args, &ctx),
        Command::Jobs(args) => cmds::jobs::run(args, &ctx),
    };
    finish(result)
}

fn finish(result: Result<i32, AppError>) -> i32 {
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            e.exit_code()
        }
    }
}
