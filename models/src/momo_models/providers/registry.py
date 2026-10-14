"""名称 → provider 类映射；env 选择（`MOMO_{TTS,ASR,VAD}_PROVIDER`）。

LLM 无 registry：编排器直连 `MOMO_LLM_ENDPOINT`，不经本层。
"""

from momo_models.config import Settings
from momo_models.providers.asr.sensevoice import SenseVoiceASRProvider
from momo_models.providers.base import ASRProvider, TTSProvider, VADProvider
from momo_models.providers.tts.cosyvoice3 import CosyVoice3Provider
from momo_models.providers.tts.edge import EdgeTTSProvider
from momo_models.providers.tts.f5 import F5TTSProvider
from momo_models.providers.tts.kokoro import KokoroTTSProvider
from momo_models.providers.tts.sovits import SoVITSProvider
from momo_models.providers.vad.silero import SileroVadProvider

TTS_PROVIDERS: dict[str, type[TTSProvider]] = {
    "edge": EdgeTTSProvider,
    "kokoro": KokoroTTSProvider,
    "f5": F5TTSProvider,
    "cosyvoice3": CosyVoice3Provider,
    "sovits": SoVITSProvider,
}


def tts_provider_names() -> list[str]:
    return sorted(TTS_PROVIDERS)


def get_tts_provider_class(name: str) -> type[TTSProvider]:
    try:
        return TTS_PROVIDERS[name]
    except KeyError:
        raise ValueError(
            f"unknown MOMO_TTS_PROVIDER={name!r}; valid: {tts_provider_names()}"
        ) from None


def create_tts_provider(settings: Settings) -> TTSProvider:
    """按 `settings.tts_provider`（MOMO_TTS_PROVIDER）实例化后端。"""
    cls = get_tts_provider_class(settings.tts_provider)
    return cls(settings)


ASR_PROVIDERS: dict[str, type[ASRProvider]] = {
    "sensevoice": SenseVoiceASRProvider,
}


def asr_provider_names() -> list[str]:
    return sorted(ASR_PROVIDERS)


def get_asr_provider_class(name: str) -> type[ASRProvider]:
    try:
        return ASR_PROVIDERS[name]
    except KeyError:
        raise ValueError(
            f"unknown MOMO_ASR_PROVIDER={name!r}; valid: {asr_provider_names()}"
        ) from None


def create_asr_provider(settings: Settings) -> ASRProvider:
    """按 `settings.asr_provider`（MOMO_ASR_PROVIDER）实例化后端。"""
    cls = get_asr_provider_class(settings.asr_provider)
    return cls(settings)


VAD_PROVIDERS: dict[str, type[VADProvider]] = {
    "silero": SileroVadProvider,
}


def vad_provider_names() -> list[str]:
    return sorted(VAD_PROVIDERS)


def get_vad_provider_class(name: str) -> type[VADProvider]:
    try:
        return VAD_PROVIDERS[name]
    except KeyError:
        raise ValueError(
            f"unknown MOMO_VAD_PROVIDER={name!r}; valid: {vad_provider_names()}"
        ) from None


def create_vad_provider(settings: Settings) -> VADProvider:
    """按 `settings.vad_provider`（MOMO_VAD_PROVIDER）实例化后端。"""
    cls = get_vad_provider_class(settings.vad_provider)
    return cls(settings)
