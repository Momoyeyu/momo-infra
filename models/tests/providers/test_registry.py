"""registry 映射与 stub provider 行为（全 mock，不 import 重依赖）。"""

import asyncio

import pytest

from momo_models.config import Settings
from momo_models.providers.base import ASRProvider, TTSProvider, VoiceRef
from momo_models.providers.registry import (
    TTS_PROVIDERS,
    create_asr_provider,
    create_tts_provider,
    get_tts_provider_class,
    tts_provider_names,
)
from momo_models.providers.tts.cosyvoice3 import CosyVoice3Provider
from momo_models.providers.tts.edge import EdgeTTSProvider
from momo_models.providers.tts.f5 import F5TTSProvider
from momo_models.providers.tts.kokoro import KokoroTTSProvider
from momo_models.providers.tts.sovits import SoVITSProvider


def test_tts_registry_covers_doc_names() -> None:
    """MOMO_TTS_PROVIDER 取值集合 = edge|kokoro|f5|cosyvoice3|sovits（config.md）。"""
    assert tts_provider_names() == ["cosyvoice3", "edge", "f5", "kokoro", "sovits"]
    assert get_tts_provider_class("edge") is EdgeTTSProvider
    assert get_tts_provider_class("kokoro") is KokoroTTSProvider
    assert get_tts_provider_class("f5") is F5TTSProvider
    assert get_tts_provider_class("cosyvoice3") is CosyVoice3Provider
    assert get_tts_provider_class("sovits") is SoVITSProvider


def test_tts_registry_unknown_name() -> None:
    with pytest.raises(ValueError, match="MOMO_TTS_PROVIDER"):
        get_tts_provider_class("nope")


def test_create_tts_provider_from_env(monkeypatch: pytest.MonkeyPatch) -> None:
    """MOMO_TTS_PROVIDER env → provider 类（Settings 走 env 读取）。"""
    monkeypatch.setenv("MOMO_TTS_PROVIDER", "edge")
    settings = Settings()
    p = create_tts_provider(settings)
    assert isinstance(p, EdgeTTSProvider)
    assert isinstance(p, TTSProvider)
    assert p.name == "edge"


def test_all_tts_stubs_satisfy_contract() -> None:
    """每个 stub 都能实例化、报能力集、在未实现处抛 NotImplementedError。"""
    settings = Settings()
    for name, cls in TTS_PROVIDERS.items():
        p = cls(settings)
        caps = p.capabilities()
        assert caps.languages
        if name in ("edge", "f5", "kokoro", "cosyvoice3"):
            continue  # 已实装，行为见各自 test_<name>.py
        with pytest.raises(NotImplementedError):
            asyncio.run(p.warmup())

        async def first_chunk(provider=p):
            async for c in provider.synth_stream("x", voice=VoiceRef(), emotion=None):
                return c
            return None

        with pytest.raises(NotImplementedError):
            asyncio.run(first_chunk())
        with pytest.raises(NotImplementedError):
            asyncio.run(p.cancel("r_x"))


def test_asr_provider_selected_by_env(tmp_path) -> None:
    """MOMO_ASR_PROVIDER 选 provider；本地 sherpa 与未来云端实现同级注册。"""
    settings = Settings(
        asr_provider="sensevoice",
        asr_model="sense-voice-small-int8",
        asr_threads=4,
        models_dir=tmp_path,
    )
    p = create_asr_provider(settings)
    assert isinstance(p, ASRProvider)
    assert p.name == "sensevoice"
    assert p.model == "sense-voice-small-int8"
    with pytest.raises(ValueError, match="MOMO_ASR_PROVIDER"):
        create_asr_provider(Settings(asr_provider="not-a-provider", models_dir=tmp_path))
    # 实装后 load 不再是 stub：模型文件缺失时抛 SenseVoiceError（readyz 拒绝就绪）。
    from momo_models.providers.asr.sensevoice import SenseVoiceError

    with pytest.raises(SenseVoiceError, match="model files missing"):
        asyncio.run(p.load())
