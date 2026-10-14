"""edge-tts（P0 过渡）：实测首包 ~1937ms，依赖外网，仅作过渡。

`edge-tts` pip 包（`tts` extra）；回 `audio-24khz-48kbitrate-mono-mp3`，
经 soundfile 解码统一到契约口径 PCM s16le 24kHz mono。
"""

import asyncio
import io
import time
from collections.abc import AsyncIterator

from momo_models.audio import DOWNLINK_SAMPLE_RATE, numpy_to_s16le_bytes, resample
from momo_models.config import Settings
from momo_models.logging import get_logger
from momo_models.providers.base import TTSCaps, TTSProvider, VoiceRef
from momo_models.providers.tts.emotion import map_emotion

log = get_logger(__name__)

# MP3（24kHz MPEG-2 Layer III）每 granule 576 samples；截断前缀解码的末尾
# 若干帧可能不完整或依赖 bit reservoir，流式产出时保留余量，收尾时全量吐出。
_DECODE_HOLD_BACK_SAMPLES = 1152

# 各语言默认音色；`MOMO_TTS_VOICE` 可覆盖为任意 edge voice 名。
_DEFAULT_VOICES = {
    "zh": "zh-CN-XiaoxiaoNeural",
    "en": "en-US-AriaNeural",
    "ja": "ja-JP-NanamiNeural",
}
_FALLBACK_VOICE = _DEFAULT_VOICES["zh"]

_DEPS_HINT = "edge provider dependencies not installed: `uv sync --extra tts`"


def mp3_to_pcm(mp3: bytes) -> bytes:
    """整段/前缀 MP3 → PCM s16le 24kHz mono（契约口径）。

    截断输入只解出已完整的帧，样本数随输入单调增长；调用方以 watermark
    控制增量产出。数据不足一帧时由 libsndfile 抛错，调用方捕获后等下一块。
    """
    import soundfile as sf  # 懒导入：未装 tts extra 时本模块仍可 import

    data, sr = sf.read(io.BytesIO(mp3), dtype="float32", always_2d=True)
    mono = data.mean(axis=1)
    mono = resample(mono, sr, DOWNLINK_SAMPLE_RATE)
    return numpy_to_s16le_bytes(mono)


def _resolve_voice(settings: Settings, voice: VoiceRef) -> str:
    if settings.tts_voice:
        return settings.tts_voice
    lang = voice.lang if voice.lang != "auto" else settings.tts_lang
    return _DEFAULT_VOICES.get(lang, _FALLBACK_VOICE)


class EdgeTTSProvider(TTSProvider):
    name = "edge"

    def __init__(self, settings: Settings) -> None:
        self._settings = settings
        self._cancel_events: set[asyncio.Event] = set()

    def capabilities(self) -> TTSCaps:
        return TTSCaps(
            streaming=True,
            languages=("zh", "en", "ja"),
            clone=False,
            emotion=True,  # map_emotion → rate/pitch/volume（#33 消费侧）
        )

    async def warmup(self) -> None:
        """仅校验依赖就绪；不做网络探测（连接失败在合成时暴露并报错）。"""
        try:
            import edge_tts  # noqa: F401
            import soundfile  # noqa: F401
        except ImportError as e:
            raise RuntimeError(_DEPS_HINT) from e

    async def synth_stream(
        self, text: str, *, voice: VoiceRef, emotion: str | None
    ) -> AsyncIterator[bytes]:
        """MP3 流增量解码 → PCM s16le 24kHz mono chunk。

        前缀解码复杂度 O(n²)，句子级长度可接受；emotion 经 map_emotion
        映射为 edge-tts 的 rate/pitch/volume（参数表见 providers/tts/emotion.py）。
        """
        import edge_tts

        text = text.strip()
        if not text:
            return
        params = map_emotion(emotion, self.name, self.capabilities())
        cancel = asyncio.Event()
        self._cancel_events.add(cancel)
        mp3 = bytearray()
        emitted = 0  # 已产出的 samples 数
        t0 = time.perf_counter()
        try:
            communicate = edge_tts.Communicate(
                text,
                voice=_resolve_voice(self._settings, voice),
                rate=params.get("rate", "+0%"),
                volume=params.get("volume", "+0%"),
                pitch=params.get("pitch", "+0Hz"),
            )
            async for chunk in communicate.stream():
                if cancel.is_set():
                    break
                if chunk.get("type") != "audio":
                    continue
                if not mp3:
                    log.info(
                        "edge.first_audio",
                        ms=round((time.perf_counter() - t0) * 1000),
                    )
                mp3 += chunk["data"]
                pcm = self._try_decode(bytes(mp3))
                if pcm is None:
                    continue
                emit_upto = len(pcm) // 2 - _DECODE_HOLD_BACK_SAMPLES
                if emit_upto > emitted:
                    yield pcm[emitted * 2 : emit_upto * 2]
                    emitted = emit_upto
            if not cancel.is_set():
                # 收尾：全量吐出 hold-back 余量
                pcm = self._try_decode(bytes(mp3))
                if pcm is not None and len(pcm) > emitted * 2:
                    yield pcm[emitted * 2 :]
                    emitted = len(pcm) // 2
            log.info(
                "edge.synth_done",
                cancelled=cancel.is_set(),
                samples=emitted,
                ms=round((time.perf_counter() - t0) * 1000),
            )
        finally:
            self._cancel_events.discard(cancel)

    async def cancel(self, req_id: str) -> None:
        """尽力取消：置位全部在途流（P0 单回合在飞）。200ms 内于 chunk 边界停产出。"""
        for ev in self._cancel_events:
            ev.set()

    @staticmethod
    def _try_decode(mp3: bytes) -> bytes | None:
        """前缀解码失败（数据不足）时返回 None，等下一块再试。"""
        try:
            return mp3_to_pcm(mp3)
        except Exception:
            return None
