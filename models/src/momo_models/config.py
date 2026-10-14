"""MOMO_* 环境变量读取（pydantic-settings）；只声明 models 服务侧读取的键。

key 全集与默认值以 momo-docs/01-architecture/config.md §2.3 为准；改动需同步
`.env.example` 与 config.md。
"""

from functools import lru_cache
from pathlib import Path

from pydantic_settings import BaseSettings, SettingsConfigDict

DEFAULT_MODELS_DIR = Path("~/.cache/momo/models")


class Settings(BaseSettings):
    """momo-models 服务侧配置。env 前缀一律 `MOMO_`。"""

    model_config = SettingsConfigDict(
        env_prefix="MOMO_",
        env_file=".env",
        env_file_encoding="utf-8",
        extra="ignore",
    )

    # 通用（config.md §2.1）
    env: str = "dev"
    log_level: str = "info"
    log_format: str = "pretty"  # pretty | json
    models_dir: Path = DEFAULT_MODELS_DIR

    # ASR（momo-svc-asr）
    asr_listen: str = "127.0.0.1:7810"
    asr_provider: str = "sensevoice"  # sensevoice；云端实现经 registry 注册同级切换
    asr_model: str = "sense-voice-small-int8"  # MANIFEST 条目名（dest 即模型目录）
    asr_threads: int = 4
    vad_provider: str = "silero"  # silero
    vad_silence_ms: int = 250

    # TTS（momo-svc-tts）
    tts_listen: str = "127.0.0.1:7811"
    tts_provider: str = "kokoro"  # edge | kokoro | f5 | cosyvoice3 | sovits
    tts_device: str = "cuda"  # cuda | cpu | mps
    tts_voice: str = ""  # 音色名（各 provider 自行解释；空=后端默认）
    tts_voice_ref: str = ""  # 克隆用参考音频路径
    tts_model_dir: str = ""  # SFT 克隆产物目录（灰度切换/回滚）
    tts_lang: str = "auto"  # zh | en | ja | auto
    cosyvoice3_home: str = ""  # FunAudioLLM/CosyVoice 源码 checkout（含 Matcha-TTS 子模块）
    cosyvoice3_python: str = ""  # cosyvoice3 隔离 venv/conda 解释器路径

    # embed（momo-svc-embed，P3）
    embed_listen: str = "127.0.0.1:7812"


@lru_cache
def get_settings() -> Settings:
    """进程级单例；测试需覆盖 env 时调用 `get_settings.cache_clear()`。"""
    return Settings()


def parse_listen(listen: str) -> tuple[str, int]:
    """`host:port` → (host, port)。缺省 host 视为 127.0.0.1。"""
    host, sep, port = listen.rpartition(":")
    if not sep:
        return "127.0.0.1", int(listen)
    return host or "127.0.0.1", int(port)
