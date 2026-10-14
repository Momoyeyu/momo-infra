# AGENTS.md

## Git

- conventional commit：`<type>: <what changed>`，`type` in `feat|fix|refactor|docs|chore|test|build`，一行说完一件事。
- 提交信息只写改动本身，不含任何 AI 署名或 trailer。

## 文档先行

- 设计以 docs 目录 为准；实现与文档不一致视为 bug，先改文档再改代码。
- 新增/修改 `MOMO_` env 键必须同步更新仓库根 `config.schema.toml`、本仓 `.env.example` 与 `docs/01-architecture/config.md`。

## 跨平台

- 开发环境 macOS；部署目标 Linux + Windows（momo-models 的 Windows 走 WSL2）。
- 禁止无条件使用 unix-only API/路径（kill -0、/tmp、shell 路径分隔）；需要时用 `cfg(unix)`/`cfg(windows)` 或 `sys.platform` 分支。
- TLS 一律 rustls（禁 openssl 依赖）；路径一律 PathBuf/path.join 拼接。
