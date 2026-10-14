# momo-infra

运维工具集 momoctl：doctor / pull / serve / switch / bench / gc / config / web / jobs。不承载业务，不在数据通路上。

目标平台：Linux + Windows（开发环境 macOS；Windows 支持逐步实装中）。

设计文档：`docs/02-repos/momo-infra.md`（CLI 契约 canonical：子命令清单 / 退出码 / flag / JSON schema）——仓内 [相对路径](../docs/02-repos/momo-infra.md)，站外 [GitHub Pages](https://momoyeyu.github.io/momo/guide/infra-console)。

## 用法

```bash
cargo build --release          # 产物 target/release/momoctl

momoctl doctor                 # 环境检查矩阵（system/toolchain/python/network/env/models/endpoints）
momoctl doctor --json          # 脚本消费：{schema:1, profile, groups:[{name,checks:[{id,status,detail,hint}]}], summary}
momoctl pull [--manifest PATH] [--only NAME] [--check]   # 按 momo-models 的 MANIFEST.toml 下载 / 校验
momoctl serve up|down|status|logs|restart [--profile normal|degraded] [--only COMPONENT] [--app-bin PATH]
momoctl serve logs [-n N] [-f]  # tail 组件日志（-n 行数，-f 跟随）
momoctl switch --to degraded [--restart]
momoctl bench [--turns N] [--strict]
momoctl gc [--dry-run] [--keep N] [--yes]
momoctl config check [--root PATH]   # env schema 漂移检查
momoctl web [--listen ADDR]          # Web 控制台（默认 127.0.0.1:7900；非 loopback 需 MOMO_WEB_TOKEN）
momoctl jobs submit [--kind train|pull|serve] [--desc TEXT] -- CMD [ARGS...]
momoctl jobs list | jobs status ID | jobs log ID [-n N] [-f] | jobs cancel ID
```

全局 flag：`--json`（单一 JSON 对象，schema 版本化）、`--env-file PATH`。无 `-v/-vv`；日志级别由 `MOMO_LOG_LEVEL` 控制（内置默认 `info`）。

## 退出码

摘要（canonical 表见 [momo-infra.md「退出码」](https://momoyeyu.github.io/momo/guide/infra-console#退出码)）：

| code | 含义 |
|---|---|
| 0 | 成功（doctor 全 ok / bench 达标） |
| 1 | 运行时错误（含未实现 stub） |
| 2 | 用法/配置错误（缺 env 等） |
| 3 | 检查未通过（doctor 有 fail / bench --strict 超标） |
| 4 | 网络不可达 |
| 5 | 校验失败（sha256 不匹配、manifest 损坏） |

子命令与全部 flag 见 [momo-infra.md「子命令清单」](https://momoyeyu.github.io/momo/guide/infra-console#子命令清单)；`doctor --json` schema 见 [「doctor JSON schema」](https://momoyeyu.github.io/momo/guide/infra-console#doctor-json-schema)。

## 布局

- `profiles/normal.toml` / `degraded.toml`：serve 组件表（组件、启动顺序、env 覆盖）
- `src/cmds/`：子命令；`src/serve/`：进程管理与 profile 解析；`src/jobs/`：job 状态机 / store（`jobs.jsonl` + `meta/` + `logs/`）/ exec_local runner；`src/web/`：axum server（api 路由树 / include_dir SPA / 启动守卫）；`web/`：内嵌 SPA 源
- 状态目录固定 `${XDG_STATE_HOME:-~/.local/state}/momo/`（pid/、logs/、active_profile；口径与各平台落点见 [momo-infra.md「状态目录与进程管理」](../docs/02-repos/momo-infra.md#状态目录与进程管理)）；探活/终止走系统调用，不依赖外部 kill/ps/tasklist

## 开发

```bash
cargo fmt && cargo clippy && cargo test
```
