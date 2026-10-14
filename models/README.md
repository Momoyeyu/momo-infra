# momo-models

模型服务与训练：momo-svc-asr / momo-svc-tts / momo-svc-embed，声线克隆与人格微调、评测。

目标平台：Linux + Windows（Windows 经 WSL2，等同 Linux；开发环境 macOS）。

设计文档：<https://github.com/Momoyeyu/docs 目录>（口径：`02-repos/momo-models.md`，协议：`01-architecture/protocol.md`，env：`01-architecture/config.md`）

## 开发

Python ≥3.12，[uv](https://docs.astral.sh/uv/) 管理。

```bash
# 仅核心依赖（骨架/CI 口径；不含 torch 与模型后端）
uv sync

# 起服务（默认 127.0.0.1:7811，MOMO_TTS_LISTEN 覆盖）
uv run momo-svc-tts     # asr: momo-svc-asr :7810 / embed: momo-svc-embed :7812

curl -s localhost:7811/healthz   # {"status":"ok","role":"tts"}
curl -s localhost:7811/readyz    # 模型未加载前 503

# 测试 / lint（CI 同口径）
uv run pytest -m "not gpu and not slow"
uv run ruff check && uv run ruff format --check
```

### 按需装重依赖（extras）

```bash
# mac-studio（无 CUDA，PyPI 默认轮即 MPS/CPU 版）
uv sync --extra torch-mac --extra tts

# laptop（linux + CUDA，走 pytorch 官方 cu124 index）
uv sync --extra torch-cu124 --extra tts

# 其余 extras：asr / embed / llm-eval
uv sync --extra asr
```

- `torch`/`torchaudio` 版本按文档红线锁 `2.6.*` 档；cosyvoice3 / sovits 不发 PyPI，按官方源码栈部署——cosyvoice3 为隔离 venv 子进程接入，需另设 `MOMO_COSYVOICE3_HOME`（源码 checkout）与 `MOMO_COSYVOICE3_PYTHON`（该 venv 解释器）。
- 模型权重在 `$MOMO_MODELS_DIR`（默认 `~/.cache/momo/models`），清单口径见 `models/MANIFEST.toml`；laptop 需 `HF_ENDPOINT=https://hf-mirror.com`（见 `.env.example`）。
- 任何引入 fugashi/MeCab 依赖的 backend 一律拒绝（依赖红线）。
