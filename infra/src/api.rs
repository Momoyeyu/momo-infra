//! REST `/api` 与 CLI `--json` 共用的契约层（issue #46）：
//! `dto` 集中定义出入参 serde 类型；cmds 各子命令与 web handler
//! 只做参数适配，不各自另写结构，保证两侧同 schema 同状态。

pub mod dto;
