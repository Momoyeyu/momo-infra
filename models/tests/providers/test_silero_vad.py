"""silero VAD 测试：mock sherpa_onnx 驱动状态机；真模型用例标 slow，无权重自动 skip。"""

import asyncio
import importlib.util
import sys
import types
from pathlib import Path

import pytest

from momo_models.config import Settings
from momo_models.providers.base import SPEECH_END, SPEECH_START, VadEvent, VADProvider
from momo_models.providers.vad.silero import (
    _MANIFEST_ENTRY,
    _MODEL_FILE,
    _MODEL_SUBDIR,
    SileroVadError,
    SileroVadProvider,
)

# ---------- 假 sherpa_onnx ----------

_FAKE_WS = 512  # 假窗长：16kHz 下 32ms/窗
_FAKE_MIN_SPEECH = 1600  # 0.1s
_FAKE_MIN_SILENCE = 4000  # 0.25s


class _FakeVadModel:
    """is_speech 按脚本逐窗返回；脚本耗尽后恒 False（静音）。"""

    builds: list[dict] = []
    script: list[bool] = []

    def __init__(self, **kwargs) -> None:
        _FakeVadModel.builds.append(kwargs)
        self._calls = 0
        self.resets = 0

    @classmethod
    def reset_state(cls) -> None:
        cls.builds = []
        cls.script = []

    @staticmethod
    def create(config) -> "_FakeVadModel":
        return _FakeVadModel(config=config)

    def window_size(self) -> int:
        return _FAKE_WS

    def min_speech_duration_samples(self) -> int:
        return _FAKE_MIN_SPEECH

    def min_silence_duration_samples(self) -> int:
        return _FAKE_MIN_SILENCE

    def is_speech(self, samples) -> bool:
        i = self._calls
        self._calls += 1
        return _FakeVadModel.script[i] if i < len(_FakeVadModel.script) else False

    def reset(self) -> None:
        self.resets += 1
        self._calls = 0


def _install_fake_sherpa(monkeypatch: pytest.MonkeyPatch) -> None:
    _FakeVadModel.reset_state()
    fake = types.SimpleNamespace(
        VadModel=_FakeVadModel,
        SileroVadModelConfig=lambda **kw: dict(kw),
        VadModelConfig=lambda **kw: dict(kw),
    )
    monkeypatch.setitem(sys.modules, "sherpa_onnx", fake)


def _make_model_dir(root: Path) -> Path:
    d = root / _MODEL_SUBDIR
    d.mkdir(parents=True)
    (d / _MODEL_FILE).write_bytes(b"fake onnx")
    return d


def _patch_manifest(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, sha: str | None = None
) -> None:
    """写一份 MANIFEST 并替换 DEFAULT_MANIFEST（sha 缺省按假文件实算）。"""
    import hashlib

    import momo_models.manifest as manifest_mod

    f = tmp_path / _MODEL_SUBDIR / _MODEL_FILE
    if sha is None:
        sha = hashlib.sha256(f.read_bytes()).hexdigest()
    manifest = tmp_path / "MANIFEST.toml"
    manifest.write_text(
        f"""
[[model]]
name = "{_MANIFEST_ENTRY}"
repo_id = "fake/repo"
revision = "master"
dest = "{_MODEL_SUBDIR.as_posix()}"
files = [
  {{ path = "{_MODEL_FILE}", sha256 = "{sha}" }},
]
""",
        encoding="utf-8",
    )
    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", manifest)


def _prepare(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    _install_fake_sherpa(monkeypatch)


def _settings(tmp_path: Path, **kw) -> Settings:
    return Settings(models_dir=tmp_path, **kw)


def _pcm(samples: int) -> bytes:
    """samples 个 s16le 样本（0x0100 非零便于 PCM 断言）。"""
    return b"\x01\x00" * samples


def _silence(samples: int) -> bytes:
    return b"\x00\x00" * samples


def _load(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, **kw) -> SileroVadProvider:
    _prepare(tmp_path, monkeypatch)
    p = SileroVadProvider(_settings(tmp_path, **kw))
    asyncio.run(p.load())
    return p


# ---------- load ----------


def test_load_missing_model(tmp_path: Path) -> None:
    p = SileroVadProvider(_settings(tmp_path))
    with pytest.raises(SileroVadError, match="model file missing"):
        asyncio.run(p.load())


def test_load_sha256_mismatch(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """MANIFEST checksum 不过 → load 抛错（readyz 拒绝就绪口径）。"""
    import hashlib

    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path, sha=hashlib.sha256(b"other").hexdigest())
    _install_fake_sherpa(monkeypatch)
    p = SileroVadProvider(_settings(tmp_path))
    with pytest.raises(SileroVadError, match="sha256"):
        asyncio.run(p.load())


def test_load_builds_model_with_silence_ms(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """MOMO_VAD_SILENCE_MS → min_silence_duration（秒）传入 sherpa 配置。"""
    _prepare(tmp_path, monkeypatch)
    p = SileroVadProvider(_settings(tmp_path, vad_silence_ms=500))
    asyncio.run(p.load())
    cfg = _FakeVadModel.builds[0]["config"]
    assert cfg["silero_vad"]["min_silence_duration"] == 0.5
    assert cfg["silero_vad"]["max_speech_duration"] == 30.0
    assert cfg["silero_vad"]["model"].endswith(_MODEL_FILE)
    assert cfg["sample_rate"] == 16000
    assert cfg["provider"] == "cpu"


# ---------- feed / 断句 ----------


def test_feed_before_load(tmp_path: Path) -> None:
    p = SileroVadProvider(_settings(tmp_path))
    with pytest.raises(SileroVadError, match="not loaded"):
        list(p.feed(_pcm(512)))


def test_silence_yields_no_events(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    p = _load(tmp_path, monkeypatch)
    assert list(p.feed(_silence(512 * 10))) == []
    assert list(p.flush()) == []


def test_speech_boundaries_and_segment_pcm(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """5 窗静音 + 20 窗 speech + 10 窗静音 → 恰好一对边界事件。

    翻转点回推迟滞：start=(6*512)-1600，end=(26*512)-4000。
    """
    p = _load(tmp_path, monkeypatch)
    _FakeVadModel.script = [False] * 5 + [True] * 20 + [False] * 10
    events = list(p.feed(_pcm(512 * 35)))
    assert [(e.event, e.segment_index) for e in events] == [
        (SPEECH_START, 1),
        (SPEECH_END, 1),
    ]
    start, end = events
    assert start.at_ms == round((6 * 512 - _FAKE_MIN_SPEECH) / 16)
    assert end.at_ms == round((26 * 512 - _FAKE_MIN_SILENCE) / 16)
    # 段 PCM = [seg_start, seg_end) 原始字节（剥掉头尾静音/迟滞）
    seg_samples = (26 * 512 - _FAKE_MIN_SILENCE) - (6 * 512 - _FAKE_MIN_SPEECH)
    assert len(end.pcm) == seg_samples * 2
    assert end.pcm[:4] == b"\x01\x00\x01\x00"


def test_two_bursts_two_segments(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """两次发声 → 两个 segment，index 递增（断句正确）。"""
    p = _load(tmp_path, monkeypatch)
    _FakeVadModel.script = [False] * 2 + [True] * 10 + [False] * 12 + [True] * 10 + [False] * 10
    events = list(p.feed(_pcm(512 * 44)))
    kinds = [(e.event, e.segment_index) for e in events]
    assert kinds == [
        (SPEECH_START, 1),
        (SPEECH_END, 1),
        (SPEECH_START, 2),
        (SPEECH_END, 2),
    ]
    ends = [e for e in events if e.event == SPEECH_END]
    assert all(len(e.pcm) > 0 for e in ends)


def test_flush_closes_open_segment(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """capture 结束时仍在 speech → flush 强制 speech_end 并携带剩余 PCM。"""
    p = _load(tmp_path, monkeypatch)
    _FakeVadModel.script = [False] * 2 + [True] * 10
    events = list(p.feed(_pcm(512 * 12)))
    assert [e.event for e in events] == [SPEECH_START]
    tail = list(p.flush())
    assert [e.event for e in tail] == [SPEECH_END]
    assert tail[0].segment_index == 1
    assert len(tail[0].pcm) > 0


def test_feed_odd_bytes_and_partial_window(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """奇数字节截断 + 不足一窗缓存到下次 feed。"""
    p = _load(tmp_path, monkeypatch)
    _FakeVadModel.script = [True] * 4 + [False] * 12
    # 300 样本 + 奇数尾字节 → 不足一窗，无事件
    assert list(p.feed(_pcm(300) + b"\x00")) == []
    # 再补 212 样本凑满一窗 → 脚本第 0 项 True → speech_start
    events = list(p.feed(_pcm(212)))
    assert [e.event for e in events] == [SPEECH_START]


def test_reset_restarts_stream(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    p = _load(tmp_path, monkeypatch)
    _FakeVadModel.script = [True] * 8 + [False] * 12
    first = list(p.feed(_pcm(512 * 20)))
    assert any(e.event == SPEECH_END for e in first)
    _FakeVadModel.script = [True] * 8 + [False] * 12
    p.reset()
    second = list(p.feed(_pcm(512 * 20)))
    # reset 后 segment_index 归零重数
    assert second[0].segment_index == 1
    assert p._vad.resets == 2  # load() 初始化一次 + 显式 reset 一次


def test_long_silence_buffer_bounded(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """静默期 _buf 被裁剪，长静音下不无界增长。"""
    p = _load(tmp_path, monkeypatch)
    list(p.feed(_silence(16000 * 10)))  # 10s 全静音
    assert len(p._buf) <= (_FAKE_MIN_SPEECH + 2 * _FAKE_WS) * 2


# ---------- registry ----------


def test_vad_registry() -> None:
    from momo_models.providers.registry import (
        VAD_PROVIDERS,
        create_vad_provider,
        get_vad_provider_class,
        vad_provider_names,
    )

    assert vad_provider_names() == ["silero"]
    assert get_vad_provider_class("silero") is SileroVadProvider
    assert VAD_PROVIDERS["silero"] is SileroVadProvider
    with pytest.raises(ValueError, match="MOMO_VAD_PROVIDER"):
        get_vad_provider_class("nope")
    with pytest.raises(ValueError, match="MOMO_VAD_PROVIDER"):
        create_vad_provider(Settings(vad_provider="nope"))
    p = create_vad_provider(Settings(vad_provider="silero"))
    assert isinstance(p, VADProvider)
    assert p.name == "silero"


def test_events_are_frozen_dataclass() -> None:
    e = VadEvent(event=SPEECH_START, at_ms=100, segment_index=1)
    assert e.pcm == b""
    with pytest.raises(AttributeError):
        e.at_ms = 200  # type: ignore[misc]


# ---------- 真模型集成（env 门控）----------

_REAL_MODEL = Settings().models_dir.expanduser() / _MODEL_SUBDIR / _MODEL_FILE
_MODEL_PRESENT = _REAL_MODEL.is_file() and importlib.util.find_spec("sherpa_onnx") is not None


@pytest.mark.slow
@pytest.mark.skipif(
    not _MODEL_PRESENT,
    reason="silero_vad.onnx / sherpa-onnx not available (needs uv sync --extra asr + momoctl pull)",
)
def test_real_model_sentence_segmentation(tmp_path: Path) -> None:
    """真实断句冒烟：静音-语音-静音-语音-静音 → 恰好两段。"""
    import numpy as np

    from momo_models.audio import wav_unwrap

    wav = (
        Settings().models_dir.expanduser()
        / "sherpa-onnx"
        / "sense-voice-zh-en-ja-ko-yue-2024-07-17"
        / "test_wavs"
        / "zh.wav"
    )
    if not wav.is_file():
        pytest.skip("sensevoice test_wavs missing; cannot build a real speech stream")
    pcm, rate, channels = wav_unwrap(wav.read_bytes())
    assert rate == 16000 and channels == 1
    speech = np.frombuffer(pcm, dtype="<i2")
    silence = np.zeros(16000, dtype="<i2")  # 1s
    stream = np.concatenate([silence, speech, silence, speech, silence])
    pcm_s16 = stream.tobytes()

    p = SileroVadProvider(Settings())
    asyncio.run(p.load())
    events: list[VadEvent] = []
    step = 16000 // 10  # 100ms 逐块喂入
    for i in range(0, len(pcm_s16), step * 2):
        events.extend(p.feed(pcm_s16[i : i + step * 2]))
    events.extend(p.flush())

    starts = [e for e in events if e.event == SPEECH_START]
    ends = [e for e in events if e.event == SPEECH_END]
    assert len(starts) == len(ends) == 2
    assert [e.segment_index for e in ends] == [1, 2]
    # 第二段语音起点 ≈ 1s 静音 + 5.6s 语音 + 1s 静音 ≈ 7.6s（±1s 容差）
    assert abs(starts[1].at_ms - 7600) < 1000
    for e in ends:
        assert len(e.pcm) > 16000  # 段长 >0.5s
