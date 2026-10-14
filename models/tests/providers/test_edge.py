"""edge provider 单测：fake edge_tts 注入 sys.modules，全 mock 无网络。

真实合成冒烟见 `test_real_edge_synthesis`（slow + 网络不可达自动 skip）。
"""

import asyncio
import sys
import types

import pytest

from momo_models.config import Settings
from momo_models.providers.base import VoiceRef
from momo_models.providers.tts import edge


def _fake_edge_tts(chunks: list[dict]) -> types.ModuleType:
    """最小 edge_tts 替身：Communicate.stream() 按脚本产出 chunk。"""
    mod = types.ModuleType("edge_tts")
    mod.last_call = {}

    class Communicate:
        def __init__(self, text, voice="default", **kw):
            mod.last_call = {"text": text, "voice": voice, **kw}

        async def stream(self):
            for c in chunks:
                yield c

    mod.Communicate = Communicate
    return mod


def _fake_decode(buf: bytes) -> bytes:
    """假解码：每 mp3 字节 → 2 个 s16le samples（4B），模拟前缀解码单调增长。"""
    return b"P" * (len(buf) * 4)


def _collect(p: edge.EdgeTTSProvider, text: str = "你好") -> list[bytes]:
    async def run() -> list[bytes]:
        return [c async for c in p.synth_stream(text, voice=VoiceRef(), emotion=None)]

    return asyncio.run(run())


def test_synth_stream_pcm_deltas(monkeypatch: pytest.MonkeyPatch) -> None:
    """增量解码 + watermark：chunk 流式产出，拼接后等于全量解码。"""
    mod = _fake_edge_tts([{"type": "audio", "data": b"\xff" * 1000} for _ in range(3)])
    monkeypatch.setitem(sys.modules, "edge_tts", mod)
    monkeypatch.setattr(edge, "mp3_to_pcm", _fake_decode)

    p = edge.EdgeTTSProvider(Settings())
    chunks = _collect(p)

    assert b"".join(chunks) == _fake_decode(b"\xff" * 3000)
    assert len(chunks) >= 2  # 流式：不是一次吐完
    assert all(len(c) % 2 == 0 for c in chunks)  # s16le 对齐
    assert mod.last_call == {
        "text": "你好",
        "voice": "zh-CN-XiaoxiaoNeural",
        "rate": "+0%",
        "volume": "+0%",
        "pitch": "+0Hz",
    }


def test_synth_stream_emotion_maps_to_edge_params(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """#33：emotion tag 经 map_emotion → Communicate 的 rate/pitch/volume。"""
    mod = _fake_edge_tts([{"type": "audio", "data": b"\xff" * 1000}])
    monkeypatch.setitem(sys.modules, "edge_tts", mod)
    monkeypatch.setattr(edge, "mp3_to_pcm", _fake_decode)
    p = edge.EdgeTTSProvider(Settings())

    async def run(emotion):
        return [c async for c in p.synth_stream("你好", voice=VoiceRef(), emotion=emotion)]

    asyncio.run(run("happy"))
    assert mod.last_call["rate"] == "+8%"
    assert mod.last_call["pitch"] == "+4Hz"
    assert mod.last_call["volume"] == "+0%"
    asyncio.run(run(None))  # None → neutral 兜底
    assert mod.last_call["rate"] == "+0%"
    asyncio.run(run("nope"))  # 未知 tag → neutral
    assert mod.last_call["rate"] == "+0%"


def test_synth_stream_skips_non_audio_and_short_audio(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """metadata chunk 跳过；总时长不足 hold-back 时收尾全量吐出。"""
    mod = _fake_edge_tts(
        [
            {"type": "SentenceBoundary", "offset": 0, "duration": 0, "text": "你"},
            {"type": "audio", "data": b"\xff" * 100},
        ]
    )
    monkeypatch.setitem(sys.modules, "edge_tts", mod)
    monkeypatch.setattr(edge, "mp3_to_pcm", _fake_decode)

    chunks = _collect(edge.EdgeTTSProvider(Settings()))
    assert chunks == [_fake_decode(b"\xff" * 100)]


def test_synth_stream_empty_text(monkeypatch: pytest.MonkeyPatch) -> None:
    mod = _fake_edge_tts([{"type": "audio", "data": b"x" * 1000}])
    monkeypatch.setitem(sys.modules, "edge_tts", mod)
    p = edge.EdgeTTSProvider(Settings())
    assert _collect(p, text="  ") == []
    assert mod.last_call == {}  # 空文本不发起请求


def test_cancel_stops_stream(monkeypatch: pytest.MonkeyPatch) -> None:
    """cancel 后在下一 chunk 边界停止产出（协议 §8 尽力取消）。"""
    mod = _fake_edge_tts([{"type": "audio", "data": b"\xff" * 1000} for _ in range(5)])
    monkeypatch.setitem(sys.modules, "edge_tts", mod)
    monkeypatch.setattr(edge, "mp3_to_pcm", _fake_decode)
    p = edge.EdgeTTSProvider(Settings())

    async def run() -> tuple[bytes, list[bytes]]:
        it = p.synth_stream("你好", voice=VoiceRef(), emotion=None)
        first = await it.__anext__()
        await p.cancel("r_1")
        rest = [c async for c in it]
        return first, rest

    first, rest = asyncio.run(run())
    assert first
    assert rest == []


def test_resolve_voice() -> None:
    """优先级：MOMO_TTS_VOICE > VoiceRef.lang > MOMO_TTS_LANG > zh 兜底。"""
    assert (
        edge._resolve_voice(Settings(tts_voice="en-US-JennyNeural"), VoiceRef(lang="ja"))
        == "en-US-JennyNeural"
    )
    assert edge._resolve_voice(Settings(), VoiceRef(lang="ja")) == "ja-JP-NanamiNeural"
    assert edge._resolve_voice(Settings(tts_lang="en"), VoiceRef()) == "en-US-AriaNeural"
    assert edge._resolve_voice(Settings(), VoiceRef()) == "zh-CN-XiaoxiaoNeural"


def test_warmup_ok(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setitem(sys.modules, "edge_tts", _fake_edge_tts([]))
    monkeypatch.setitem(sys.modules, "soundfile", types.ModuleType("soundfile"))
    asyncio.run(edge.EdgeTTSProvider(Settings()).warmup())


def test_warmup_missing_deps(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setitem(sys.modules, "edge_tts", None)
    with pytest.raises(RuntimeError, match="extra tts"):
        asyncio.run(edge.EdgeTTSProvider(Settings()).warmup())


def test_try_decode_garbage_returns_none() -> None:
    pytest.importorskip("soundfile")
    assert edge.EdgeTTSProvider._try_decode(b"not mp3 at all") is None


def _make_mp3(seconds: float = 1.0) -> bytes:
    """soundfile 本地编码一段 24kHz 正弦为 MP3（libsndfile 需支持 MP3 写）。"""
    sf = pytest.importorskip("soundfile")
    import io

    import numpy as np

    sr = 24000
    t = np.linspace(0, seconds, int(sr * seconds), endpoint=False)
    sig = (0.3 * np.sin(2 * np.pi * 440 * t)).astype(np.float32)
    buf = io.BytesIO()
    try:
        sf.write(buf, sig, sr, format="MP3")
    except Exception as e:
        pytest.skip(f"libsndfile cannot encode MP3: {e}")
    return buf.getvalue()


def test_mp3_to_pcm_roundtrip() -> None:
    """真实解码路径：soundfile 写 mp3 → mp3_to_pcm → s16le 24k。"""
    pcm = edge.mp3_to_pcm(_make_mp3())
    assert len(pcm) % 2 == 0 and len(pcm) > 0
    # MP3 编码 delay/padding 下样本数允许 ±10%
    assert abs(len(pcm) // 2 - 24000) < 2400


def test_synth_stream_real_mp3_decode(monkeypatch: pytest.MonkeyPatch) -> None:
    """集成：fake edge_tts 推真实 MP3 分片 → 真实解码 → PCM 流式产出。"""
    mp3 = _make_mp3()
    step = 500
    chunks = [{"type": "audio", "data": mp3[i : i + step]} for i in range(0, len(mp3), step)]
    monkeypatch.setitem(sys.modules, "edge_tts", _fake_edge_tts(chunks))

    out = _collect(edge.EdgeTTSProvider(Settings()))
    total = b"".join(out)
    assert len(out) > 1  # 流式多 chunk
    assert abs(len(total) // 2 - 24000) < 2400


@pytest.mark.slow
def test_real_edge_synthesis() -> None:
    """真实 edge-tts 合成冒烟：需要外网 + `uv sync --extra tts`。"""
    pytest.importorskip("edge_tts")
    pytest.importorskip("soundfile")
    p = edge.EdgeTTSProvider(Settings())
    try:
        chunks = _collect(p, "你好，我是默默。")
    except Exception as e:
        pytest.skip(f"edge-tts network unreachable: {e!r}")
    assert len(chunks) > 1
    assert sum(map(len, chunks)) > 4800  # >100ms 的 24k s16le
