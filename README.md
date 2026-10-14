# momo-infra

自有 AI 基础设施：**模型训练 pipeline + serving gateway**。

- LLM 后训练（persona/QLoRA）与 ASR/TTS 全训练 pipeline（声线克隆 zero-shot/SFT），训练 job 由 `momoctl` 编排，覆盖本地与 SSH 远端执行器。
- 推理底座复用 vLLM 等开源实现，不手搓推理引擎/KV cache。
- 自研收敛到统一对外 gateway：对内多模型容灾与负载均衡，对外单一 OpenAI-compatible 接口；ASR/TTS 接口形态对齐主流云服务。

终端产品是 [momo](https://github.com/Momoyeyu/momo)——app 通过标准 provider 接口消费本仓能力，容灾永远在服务侧。

## 文档

`docs/` 是设计的单一来源；[文档站](https://momoyeyu.github.io/momo-infra/)（`site/`）经 VitePress include 呈现同一份内容。阅读顺序见 [docs/README.md](docs/README.md)。

## 目录

| 目录 | 内容 |
|---|---|
| `docs/` | 设计文档（单一来源） |
| `site/` | VitePress 文档站 |
