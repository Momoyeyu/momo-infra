"""ABC 契约测试：假 backend 注入，验证签名级契约（momo-models.md「provider 与产物」）。

全 mock，无 torch/GPU/网络依赖。
"""

import asyncio
from collections.abc import AsyncIterator

import pytest

from momo_models.providers.base import (
    EMOTION_TAGS,
    ASRProvider,
    ChatDelta,
    ChatRequest,
    LLMProvider,
    Transcript,
    TTSCaps,
    TTSProvider,
    VoiceRef,
)

# ---------- 假实现 ----------


class FakeTTS(TTSProvider):
    name = "fake"

    def capabilities(self) -> TTSCaps:
        return TTSCaps(streaming=True, languages=("zh", "en"), clone=False, emotion=True)

    async def warmup(self) -> None:
        self.ready = True

    async def synth_stream(
        self, text: str, *, voice: VoiceRef, emotion: str | None
    ) -> AsyncIterator[bytes]:
        yield b"\x00" * 480  # 10ms of 24k s16le mono
        yield b"\x01" * 480

    async def cancel(self, req_id: str) -> None:
        self.cancelled = req_id


class FakeASR(ASRProvider):
    name = "fake-asr"

    async def load(self) -> None:
        self.loaded = True

    async def transcribe(self, pcm: bytes, *, lang: str | None) -> Transcript:
        return Transcript(text="测试", lang=lang or "zh", duration_ms=100)


class FakeLLM(LLMProvider):
    name = "fake-llm"

    async def chat_stream(self, req: ChatRequest) -> AsyncIterator[ChatDelta]:
        yield ChatDelta(text="你")
        yield ChatDelta(text="好", finish_reason="stop")

    async def list_models(self) -> list[str]:
        return ["fake-1"]

    async def aclose(self) -> None:
        self.closed = True


# ---------- TTS 契约 ----------


def test_tts_abstract_enforced() -> None:
    class Incomplete(TTSProvider):
        name = "incomplete"

    with pytest.raises(TypeError):
        Incomplete()  # type: ignore[abstract]


def test_tts_contract() -> None:
    p = FakeTTS()
    caps = p.capabilities()
    assert caps.streaming and "zh" in caps.languages
    asyncio.run(p.warmup())
    assert p.ready

    async def collect() -> bytes:
        chunks = [c async for c in p.synth_stream("你好", voice=VoiceRef(), emotion="happy")]
        return b"".join(chunks)

    assert len(asyncio.run(collect())) == 960
    asyncio.run(p.cancel("r_1"))
    assert p.cancelled == "r_1"


# ---------- ASR 契约 ----------


def test_asr_contract() -> None:
    p = FakeASR()
    asyncio.run(p.load())
    t = asyncio.run(p.transcribe(b"\x00" * 320, lang=None))
    assert isinstance(t, Transcript)
    assert t.text and t.lang and t.duration_ms > 0


# ---------- LLM 契约（eval/bench only）----------


def test_llm_contract() -> None:
    p = FakeLLM()

    async def collect() -> str:
        parts = [d.text async for d in p.chat_stream(ChatRequest(model="m", messages=[]))]
        return "".join(parts)

    assert asyncio.run(collect()) == "你好"
    assert asyncio.run(p.list_models()) == ["fake-1"]
    asyncio.run(p.aclose())
    assert p.closed


# ---------- 协议常量 ----------


def test_emotion_tags_match_protocol() -> None:
    """protocol.md §2：v1 固定 8 值。"""
    assert len(EMOTION_TAGS) == 8
    assert set(EMOTION_TAGS) == {
        "neutral",
        "happy",
        "sad",
        "angry",
        "surprised",
        "thinking",
        "shy",
        "sleepy",
    }
