# 推理模型获取、激活与回滚

> 历史事实：2026-10-10 laptop 访问 huggingface.co 不通，当时必须使用 `HF_ENDPOINT=https://hf-mirror.com`。这不是对所有未来环境的性能保证，每次以 probe 为准。

权重存放在 `${MOMO_MODELS_DIR}`，仓库只保存 manifest；Runtime Core、模型与数据各自遵循上游许可。

## fetch lifecycle

```mermaid
flowchart TD
    REQUEST["选择 manifest 中的模型/revision"] --> LICENSE{"许可与用途允许？"}
    LICENSE -->|"否/不明确"| BLOCK["阻止下载激活；补授权信息"]
    LICENSE -->|"是"| FETCH["下载到临时目录；支持续传"]
    FETCH --> CHECK{"size + checksum + revision 通过？"}
    CHECK -->|"否"| BAD["隔离/删除临时产物；retry"]
    CHECK -->|"是"| REGISTER["写 models.lock/本地 registry"]
    REGISTER --> WARM["候选 provider warmup + probe"]
    WARM --> READY{"readiness 通过？"}
    READY -->|"否"| KEEP["保持 active 版本；查看日志/retry"]
    READY -->|"是"| ACTIVATE["下一 turn 前原子 activate"]
    ACTIVATE --> OBSERVE{"运行健康？"}
    OBSERVE -->|"否"| ROLLBACK["回滚上一已校验版本"]
    OBSERVE -->|"是"| GC["保留 rollback 版本后再 GC"]
```

## 不变量

- 活动 turn 存在时先显式 cancel，不在回合中换模型。
- checksum/revision/readiness 任一失败都不能覆盖 active。
- `.part`、未登记目录和旧版本只有在无 active/rollback 引用后才能 GC。
- provider 运行时不在线拉权重；下载与 turn 数据路径分离。
- `models.lock` 不记录密钥，下载凭据由独立 secret store 提供。
- `pull` 期间以 `${MOMO_MODELS_DIR}/pull.lock` 占位互斥（create_new 原子）；崩溃残留需手动删除后重试。

## 操作检查

- [x] manifest 包含 repo/revision/files/checksum/destination，及结构化 `license`（`spdx`/`source`，非 SPDX 许可另带 `name`）；非 HF 上游用 `source = "url"` + `files[].url` 直链（如 silero-vad 走 GitHub raw）。
- [ ] 下载失败可续传或干净重试，不把半文件注册为 ready。
- [ ] warmup/probe 失败 active 不变。
- [ ] promotion 后有明确 rollback 目标。
- [ ] 实际大小/性能只有测量后才以“实测 + 日期”回写。
