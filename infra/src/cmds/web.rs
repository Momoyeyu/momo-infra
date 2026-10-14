//! `momoctl web [--listen ADDR]`：Web 控制台 server 入口。
//! 流程：启动守卫（非 loopback 且无 `MOMO_WEB_TOKEN` → exit 2）
//! → tokio runtime → axum serve（Ctrl-C 优雅退出）。
//! 启动日志打印监听地址与 token 提示；`--json` 输出 `{schema, command, listen, url}`。

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::api::dto::WebReport;
use crate::cli::WebArgs;
use crate::env_cfg::EnvCfg;
use crate::output::{AppError, ExitCode};
use crate::web::{self, WebState};

pub fn run(args: &WebArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let env = EnvCfg;
    let token = env.web_token();
    // 启动守卫：拒启先于任何 socket 操作。
    web::guard::check_listen(&args.listen, token.is_some())?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(args, token, ctx))
}

async fn serve(args: &WebArgs, token: Option<String>, ctx: &super::Ctx) -> Result<i32, AppError> {
    // 先 bind 再打印：拿到真实 local_addr（--listen :0 时含实际端口）。
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    let local = listener.local_addr()?;
    let url = format!("http://{local}/");

    let state = Arc::new(WebState {
        events_tx: broadcast::channel(web::EVENT_CHANNEL_CAP).0,
        token: token.clone(),
        // 进程级 CSRF token：每次启动重签发；GET /api/csrf 下发给已过认证的客户端。
        csrf_token: ulid::Ulid::new().to_string(),
        // .env 定位与 `momoctl switch` 同口径；`.env.<profile>` 在其同目录。
        env_link: crate::env_file::resolve_env_path(ctx.env_file.as_deref()),
    });
    let app = web::router(state);

    // #54 rescan：对外接 API 前先回放索引 + 按 process identity 收口
    // 非终态 job（runner 死亡 → failed/canceled/recovering）。store 打不开
    // 不拒启——读侧 handler 自己会按 500 报具体 IO 错误。
    match crate::jobs::store::JobStore::open_default().reconcile() {
        Ok(notes) => {
            for n in notes {
                eprintln!("warn: jobs rescan: {n}");
            }
        }
        Err(e) => eprintln!("warn: jobs rescan failed: {e}"),
    }

    if ctx.json {
        let report = WebReport {
            schema: 1,
            command: "web".into(),
            listen: local.to_string(),
            url,
        };
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        println!("momoctl web listening on {url}");
        if token.is_some() {
            // token 值不落日志（config.md §4 敏感键不回显）。
            println!("MOMO_WEB_TOKEN is set; send it as Authorization: Bearer <token>");
        } else {
            println!("MOMO_WEB_TOKEN is not set; /api is limited to loopback clients");
        }
    }

    web::serve(listener, app).await?;
    Ok(ExitCode::Ok as i32)
}
