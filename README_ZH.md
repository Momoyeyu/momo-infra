<p align="center">
  <a href="./README.md">English</a> · <strong>简体中文</strong>
</p>

# momo-infra

[![ci-infra](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-infra.yml/badge.svg)](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-infra.yml)
[![ci-models](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-models.yml/badge.svg)](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-models.yml)
[![ci-config](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-config.yml/badge.svg)](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-config.yml)
[![ci-site](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-site.yml/badge.svg)](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-site.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE.md)
[![docs](https://img.shields.io/badge/docs-momoyeyu.github.io%2Fmomo--infra-blue)](https://momoyeyu.github.io/momo-infra/)

租一个模型 API，一个下午就能跑出 demo。但要把栈握在自己手里——声线是自己的、人格是自己的、某个 backend 挂了话说到一半也不断——需要的是一条训练 pipeline、一层 serving，和大量胶水。momo-infra 就是那层胶水：自有 AI 基础设施，服务于终端产品 [momo](https://github.com/Momoyeyu/momo)（一个桌面伴侣 app，经标准 provider 协议消费本仓的一切能力）。

它本身不是产品。终端的 app 只持有三个端点变量；端点与 GPU 之间的一切都在这个仓里。

## 你能得到什么

- **一个端点，背后任意多模型。** `momoctl gateway` 挡在调用方与 LLM backend 池之间——一台 vLLM 机器、一台跑 MLX 的笔记本、一个兜底云 API——外加你的 ASR/TTS 服务。调用方只看到单一 OpenAI-compatible 接口（`/v1/chat/completions`、`/v1/models`）和云服务形态的语音端点（Deepgram 形态的 `WS /v1/listen`、OpenAI 形态的 `POST /v1/audio/speech`）。容灾与负载均衡全在内部发生：backend 挂了、新机器加入、流量权重调整，都只是改配置，调用方无感知。
- **自己的模型，以云服务形态放出去。** 自训或下载的权重走和第三方 provider 相同的协议对外——把一份标准 provider 配置指向 gateway 就能直接用，不需要厂商专属客户端。
- **训练可以离开你的笔记本。** `momoctl jobs submit --executor ssh` 把 job 打包推到 SSH 远端执行——rsync 传 bundle、断线重挂、绝不重复 launch。`momoctl pipeline run` 串行 step 加断点续跑，借来的 24 小时 GPU 窗口是常规目标，不是赌博。
- **权重可校验。** [models/models/MANIFEST.toml](models/models/MANIFEST.toml) 里每个模型都钉死 revision 与 sha256、带结构化 license；`momoctl pull` 下载并校验，校验不过的服务 readyz 拒绝就绪。
- **配置只有一处真相。** 一切环境差异走 `MOMO_*` env；`config.schema.toml` 是机器可读权威 schema，`momoctl config check` 在 CI 里拦住 schema ↔ `.env.example` ↔ 文档的三方漂移。

## 上手

```bash
cd infra && cargo build --release          # 产物 → infra/target/release/momoctl
./target/release/momoctl doctor            # 环境自检
./target/release/momoctl pull              # 下载 + 校验模型权重

cd ../models
uv sync --extra torch-mac --extra asr --extra tts   # CUDA 机器用 --extra torch-cu124
uv run momo-svc-asr      # 127.0.0.1:7810 —— WS /v1/listen
uv run momo-svc-tts      # 127.0.0.1:7811 —— POST /v1/audio/speech

cp ../infra/gateway.example.toml gateway.toml       # 按实际 backend 改
../infra/target/release/momoctl gateway             # 127.0.0.1:7830
```

把任意 OpenAI-compatible 客户端指向 gateway 即可；语音端点对齐云服务形态，能跟厂商通话的 provider 配置同样能跟你的栈通话。

## 边界在哪

- **推理引擎是已被解决的问题。** LLM serving 骑在 vLLM / mlx_lm 上，不手搓运行时。自研面恰好只有两处：gateway 与 `momoctl`。
- **运维面不碰对话数据。** momoctl 只做启停、探测、产物管理——不读 turn 内容、不转发音频/文本。
- **上游各自保有自己的许可。** 下方 MIT 只覆盖本仓代码；模型权重按各自上游条款使用，逐条声明在 manifest 里。

## 目录

| 目录 | 内容 |
|---|---|
| `infra/` | `momoctl` CLI + Web 控制台 + jobs/pipeline 编排 + gateway（Rust） |
| `models/` | ASR / TTS / embed 服务与训练实体（Python，uv） |
| `docs/` | 设计文档——单一来源 |
| `site/` | VitePress 文档站，经 include 呈现 `docs/` |
| `evals/` | 评测集（声线盲听 testset） |
| `config.schema.toml` | 全部 `MOMO_*` env 的机器可读 schema |

这里文档先行：`docs/` 是 canonical，实现与文档不一致视为 bug。入口 [docs/README.md](docs/README.md)；同一份内容渲染在 [momoyeyu.github.io/momo-infra](https://momoyeyu.github.io/momo-infra/)。

## 致谢

- [vLLM](https://github.com/vllm-project/vllm) —— LLM serving 底座
- [mlx-lm](https://github.com/ml-explore/mlx-lm) —— Apple Silicon 上的 LLM serving
- [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) —— ASR 与 VAD 背后的端侧推理运行时
- [Fun-CosyVoice3-0.5B](https://huggingface.co/FunAudioLLM/Fun-CosyVoice3-0.5B-2512) —— TTS 首选目标（Apache-2.0）
- [Kokoro-82M](https://huggingface.co/hexgrad/Kokoro-82M) —— TTS 兜底（Apache-2.0）
- [F5-TTS](https://huggingface.co/SWivid/F5-TTS) —— 零样本声线克隆（权重 CC-BY-NC-4.0）
- [vocos-mel-24khz](https://huggingface.co/charactr/vocos-mel-24khz) —— F5-TTS 的声码器（MIT）
- [SenseVoice Small](https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17) —— ASR 基线（FunASR Model License v1.1）
- [silero-vad](https://github.com/snakers4/silero-vad) —— 语音活动检测（MIT）
- 承载了历次训练的借来的机器——按「随时会失去」对待，规矩见 [docs/04-ops/borrowed-etiquette.md](docs/04-ops/borrowed-etiquette.md)

## License

代码以 [MIT License](LICENSE.md) 发布；**模型权重不适用**——分别按各自上游许可条款使用，逐条声明见 [models/models/MANIFEST.toml](models/models/MANIFEST.toml) 的 `license` 字段。
