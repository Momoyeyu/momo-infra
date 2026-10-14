# AGENTS.md

## Git

- conventional commit：`<type>: <what changed>`，`type` in `feat|fix|refactor|docs|chore|test|build`，一行说完一件事。
- 提交信息只写改动本身，不含任何 AI 署名或 trailer。

## 文档先行

- 设计以 docs 目录 为准；实现与文档不一致视为 bug，先改文档再改代码。
- 新增/修改 `MOMO_` env 键必须同步更新仓库根 `config.schema.toml`、本仓 `.env.example` 与 `docs/01-architecture/config.md`。

## 跨平台

- 开发环境 macOS；部署目标 Linux + Windows（Windows 走 WSL2，等同 Linux 处理）。
- 禁止无条件使用 POSIX-only API/路径（os.kill 信号、/tmp、shell 路径分隔）；需要时用 `os.name`/`sys.platform` 分支，不无条件 import POSIX-only 模块（fcntl、termios 等）。
- 路径一律 pathlib.Path/os.path.join 拼接。
