//! `momoctl gateway [--listen ADDR] [--config PATH]`：统一对外 gateway 入口。
//! 配置解析顺序见 gateway.md §2：`--config` > `MOMO_GATEWAY_CONFIG` > `./gateway.toml`；
//! `--listen` > `MOMO_GATEWAY_LISTEN` > 配置 `listen` > `127.0.0.1:7830`。
//! 启动：加载配置 → bind → 起后台健康探测 → axum serve（Ctrl-C 优雅退出）。

use std::path::PathBuf;

use crate::cli::GatewayArgs;
use crate::env_cfg::{expand_tilde, EnvCfg};
use crate::gateway::{self, config::GatewayConfig, GatewayState};
use crate::output::{AppError, ExitCode};

pub fn run(args: &GatewayArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let env = EnvCfg;
    let cfg_path = args
        .config
        .clone()
        .or_else(|| env.get("MOMO_GATEWAY_CONFIG").map(|v| expand_tilde(&v)))
        .unwrap_or_else(|| PathBuf::from("gateway.toml"));
    let cfg = GatewayConfig::load(&cfg_path)?;
    let listen = args
        .listen
        .clone()
        .or_else(|| env.get("MOMO_GATEWAY_LISTEN"))
        .or_else(|| cfg.listen.clone())
        .unwrap_or_else(|| gateway::config::DEFAULT_LISTEN.to_string());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(&listen, cfg, &cfg_path, ctx))
}

async fn serve(
    listen: &str,
    cfg: GatewayConfig,
    cfg_path: &std::path::Path,
    ctx: &super::Ctx,
) -> Result<i32, AppError> {
    let state = GatewayState::new(cfg)?;
    // 先 bind 再打印：拿到真实 local_addr（--listen :0 时含实际端口）。
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let local = listener.local_addr()?;
    let app = gateway::router(state.clone());
    gateway::health::spawn_probes(&state);

    if ctx.json {
        let report = serde_json::json!({
            "schema": crate::api::dto::SCHEMA_VERSION,
            "command": "gateway",
            "listen": local.to_string(),
            "url": format!("http://{local}/"),
            "config": cfg_path.display().to_string(),
            "upstreams": {
                "llm": state.llm.backends.len(),
                "asr": state.asr.backends.len(),
                "tts": state.tts.backends.len(),
            }
        });
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        println!("momoctl gateway listening on http://{local}/");
        println!(
            "config: {} (llm={}, asr={}, tts={} upstreams)",
            cfg_path.display(),
            state.llm.backends.len(),
            state.asr.backends.len(),
            state.tts.backends.len()
        );
    }

    gateway::serve(listener, app).await?;
    Ok(ExitCode::Ok as i32)
}
