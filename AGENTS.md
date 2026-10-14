# AGENTS.md

## Git

- conventional commit：`<type>: <what changed>`，`type` in `feat|fix|refactor|docs|chore|test|build`，一行说完一件事。
- 提交信息只写改动本身，不含任何 AI 署名或 trailer。

## Review 纪律

- PR comment 只放结果与证据；禁止把验证动作甩给 reviewer。
- 被资源卡住（API key、算力、账号权限、可达网络）→ 直接在会话中向用户索要，不在 PR/issue comment 里索要或绕开。

## 输出与日志

- 面向用户的输出一律英文：CLI 日志、错误提示、hint、报告文本（测试断言同样适用）。源码注释与 `docs/` 文档保持中文。

## 文档先行

- 设计以 `docs/` 为准；实现与文档不一致视为 bug，先改文档再改代码。
- `docs/` 是单一来源，`site/docs/guide/` 通过 include 消费 canonical docs，不复制正文。

## Site verification

- `cd site && pnpm test`
- `cd site && pnpm build`
