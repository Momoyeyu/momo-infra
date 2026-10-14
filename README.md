<p align="center">
  <strong>English</strong> · <a href="./README_ZH.md">简体中文</a>
</p>

# momo-infra

[![ci-infra](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-infra.yml/badge.svg)](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-infra.yml)
[![ci-models](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-models.yml/badge.svg)](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-models.yml)
[![ci-config](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-config.yml/badge.svg)](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-config.yml)
[![ci-site](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-site.yml/badge.svg)](https://github.com/Momoyeyu/momo-infra/actions/workflows/ci-site.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE.md)
[![docs](https://img.shields.io/badge/docs-momoyeyu.github.io%2Fmomo--infra-blue)](https://momoyeyu.github.io/momo-infra/)

Renting a model API gets you a demo in an afternoon. Owning the stack — a voice that is yours, a persona that is yours, an endpoint that survives a backend dying mid-sentence — takes a training pipeline, a serving layer, and a lot of glue. momo-infra is that glue: the self-owned AI infrastructure behind [momo](https://github.com/Momoyeyu/momo), a desktop companion app that consumes everything here through standard provider protocols.

It is not itself a product. The app at the end holds three endpoint variables; everything between those endpoints and the GPUs lives in this repo.

## What it gives you

- **One endpoint in front of many models.** `momoctl gateway` sits between callers and a pool of LLM backends — a vLLM box, an MLX laptop, a cloud API as last resort — plus your ASR/TTS services. Callers see a single OpenAI-compatible surface (`/v1/chat/completions`, `/v1/models`) and cloud-shaped voice endpoints (`WS /v1/listen` in Deepgram's shape, `POST /v1/audio/speech` in OpenAI's). Failover and load balancing happen inside; a backend dying, a new one joining, or traffic being reweighted are all config changes the caller never sees.
- **Your own models, served like a cloud service.** Self-trained or downloaded weights ship behind the same protocols third-party providers use — point a standard provider config at the gateway and it just works, no vendor-specific client.
- **Training that leaves your laptop.** `momoctl jobs submit --executor ssh` bundles a job and pushes it to a remote machine over SSH — rsync the bundle, reattach if the connection drops, never launch twice. `momoctl pipeline run` chains steps with resume-from-checkpoint, so a 24-hour borrowed GPU window is a normal target, not a gamble.
- **Weights you can verify.** Every model in [models/models/MANIFEST.toml](models/models/MANIFEST.toml) is pinned by revision and sha256 with a structured license; `momoctl pull` downloads and verifies, and a service refuses readyz until the checksums pass.
- **One place config lives.** Every environment difference is a `MOMO_*` variable; `config.schema.toml` is the machine-readable schema, and `momoctl config check` fails CI if schema, `.env.example`, and the docs drift apart.

## Get started

```bash
cd infra && cargo build --release          # → infra/target/release/momoctl
./target/release/momoctl doctor            # environment self-check
./target/release/momoctl pull              # download + verify model weights

cd ../models
uv sync --extra torch-mac --extra asr --extra tts   # CUDA boxes: --extra torch-cu124
uv run momo-svc-asr      # 127.0.0.1:7810 — WS /v1/listen
uv run momo-svc-tts      # 127.0.0.1:7811 — POST /v1/audio/speech

cp ../infra/gateway.example.toml gateway.toml       # point it at your backends
../infra/target/release/momoctl gateway             # 127.0.0.1:7830
```

Point any OpenAI-compatible client at the gateway and it works; the voice endpoints match cloud provider shapes, so a provider config that talks to a vendor talks to your stack too.

## Where the line is

- **Inference engines are a solved problem.** LLM serving rides on vLLM / mlx_lm rather than a hand-rolled runtime. The self-built surface is exactly two things: the gateway and `momoctl`.
- **The ops plane never touches conversation data.** momoctl starts, stops, probes, and keeps artifacts — it does not read turns or carry audio/text.
- **Every upstream keeps its own license.** The MIT license below covers only the code written here; model weights follow their upstream terms, declared per entry in the manifest.

## Layout

| Dir | What lives there |
|---|---|
| `infra/` | `momoctl` CLI + web console + jobs/pipeline runner + gateway (Rust) |
| `models/` | ASR / TTS / embed services and training entities (Python, uv) |
| `docs/` | Design docs — the single source of truth |
| `site/` | VitePress site rendering `docs/` via include |
| `evals/` | Eval sets (voice blind-listening testset) |
| `config.schema.toml` | Machine-readable schema for all `MOMO_*` env vars |

Design comes first here: `docs/` is canonical, and code that disagrees with it is a bug. Start at [docs/README.md](docs/README.md); the same pages render at [momoyeyu.github.io/momo-infra](https://momoyeyu.github.io/momo-infra/).

## Acknowledgements

- [vLLM](https://github.com/vllm-project/vllm) — LLM serving backend
- [mlx-lm](https://github.com/ml-explore/mlx-lm) — LLM serving on Apple Silicon
- [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) — on-device runtime behind ASR and VAD
- [Fun-CosyVoice3-0.5B](https://huggingface.co/FunAudioLLM/Fun-CosyVoice3-0.5B-2512) — primary TTS target (Apache-2.0)
- [Kokoro-82M](https://huggingface.co/hexgrad/Kokoro-82M) — fallback TTS (Apache-2.0)
- [F5-TTS](https://huggingface.co/SWivid/F5-TTS) — zero-shot voice cloning (weights CC-BY-NC-4.0)
- [vocos-mel-24khz](https://huggingface.co/charactr/vocos-mel-24khz) — vocoder for F5-TTS (MIT)
- [SenseVoice Small](https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17) — ASR baseline (FunASR Model License v1.1)
- [silero-vad](https://github.com/snakers4/silero-vad) — voice activity detection (MIT)
- The borrowed machines that hosted the training runs — treated as losable at any time, per [docs/04-ops/borrowed-etiquette.md](docs/04-ops/borrowed-etiquette.md)

## License

Code is released under the [MIT License](LICENSE.md). Model weights are **not** covered — each follows its own upstream license, declared per entry in [models/models/MANIFEST.toml](models/models/MANIFEST.toml).
