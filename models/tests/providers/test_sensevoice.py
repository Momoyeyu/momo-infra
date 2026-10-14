"""sensevoice provider 测试：sherpa-onnx 全 mock + 真模型集成（env 门控 skip）。

- 单测：sys.modules 注入假 sherpa_onnx；MANIFEST 也用临时清单替换，
  不依赖重库/权重。
- 集成：仅当 $MOMO_MODELS_DIR 下模型文件存在且 sherpa_onnx 可 import 时运行
  （标 slow；CI 无权重自动 skip）。
"""

import asyncio
import importlib.util
import sys
import types
from pathlib import Path

import pytest

from momo_models.config import Settings
from momo_models.providers.asr.sensevoice import (
    SenseVoiceASRProvider,
    SenseVoiceError,
    _normalize_lang,
    _parse_result,
    _strip_tag,
)
from momo_models.providers.base import Transcript

# 与仓库 MANIFEST（models/models/MANIFEST.toml）的 sense-voice-small-int8 条目对齐。
_MODEL_SUBDIR = Path("sherpa-onnx") / "sense-voice-zh-en-ja-ko-yue-2024-07-17"
_ENTRY = "sense-voice-small-int8"

# ---------- 假 sherpa_onnx ----------


class _FakeResult:
    def __init__(
        self,
        text: str,
        lang: str = "<|zh|>",
        emotion: str = "<|HAPPY|>",
        event: str = "<|Speech|>",
    ) -> None:
        self.text = text
        self.lang = lang
        self.emotion = emotion
        self.event = event


class _FakeStream:
    def __init__(self, result: _FakeResult) -> None:
        self._result = result
        self.rate = None
        self.samples = None

    def accept_waveform(self, rate: int, samples) -> None:
        self.rate = rate
        self.samples = list(samples)

    @property
    def result(self) -> _FakeResult:
        return self._result


class _FakeRecognizer:
    """记录 from_sense_voice 参数；decode_stream 返回预置结果。"""

    builds: list[dict] = []
    streams: list[_FakeStream] = []
    next_text = "你好，世界。"

    def __init__(self, **kwargs) -> None:
        _FakeRecognizer.builds.append(kwargs)

    @classmethod
    def reset(cls) -> None:
        cls.builds = []
        cls.streams = []
        cls.next_text = "你好，世界。"

    @staticmethod
    def from_sense_voice(**kwargs) -> "_FakeRecognizer":
        return _FakeRecognizer(**kwargs)

    def create_stream(self) -> _FakeStream:
        stream = _FakeStream(_FakeResult(self.next_text))
        self.streams.append(stream)
        return stream

    def decode_stream(self, stream: _FakeStream) -> None:
        stream.decoded = True


def _install_fake_sherpa(monkeypatch: pytest.MonkeyPatch) -> None:
    _FakeRecognizer.reset()
    fake = types.SimpleNamespace(OfflineRecognizer=_FakeRecognizer)
    monkeypatch.setitem(sys.modules, "sherpa_onnx", fake)


def _make_model_dir(root: Path, model_name: str = "model.int8.onnx") -> Path:
    d = root / _MODEL_SUBDIR
    d.mkdir(parents=True)
    (d / model_name).write_bytes(b"fake onnx")
    (d / "tokens.txt").write_text("x 0\n", encoding="utf-8")
    return d


def _patch_manifest(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, model_name: str = "model.int8.onnx"
) -> None:
    """写一份 sha256 与假文件一致的 MANIFEST 并替换 DEFAULT_MANIFEST。

    MANIFEST 里 sense-voice 条目已填真实 checksum，故单测的假权重也必须
    走完整校验路径，而不是依赖 TODO 占位跳过。
    """
    import hashlib

    import momo_models.manifest as manifest_mod

    d = tmp_path / _MODEL_SUBDIR

    def _sha(p: Path) -> str:
        return hashlib.sha256(p.read_bytes()).hexdigest()

    manifest = tmp_path / "MANIFEST.toml"
    manifest.write_text(
        f"""
[[model]]
name = "{_ENTRY}"
repo_id = "fake/repo"
revision = "main"
dest = "{_MODEL_SUBDIR.as_posix()}"
files = [
  {{ path = "{model_name}", sha256 = "{_sha(d / model_name)}" }},
  {{ path = "tokens.txt", sha256 = "{_sha(d / "tokens.txt")}" }},
]
""",
        encoding="utf-8",
    )
    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", manifest)


def _patch_manifest_bare(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    name: str = _ENTRY,
    dest: Path = _MODEL_SUBDIR,
) -> None:
    """只登记条目名/dest（无 files），供条目解析与“文件缺失”用例隔离真实仓库清单。"""
    import momo_models.manifest as manifest_mod

    manifest = tmp_path / "MANIFEST.toml"
    manifest.write_text(
        f"""
[[model]]
name = "{name}"
repo_id = "fake/repo"
revision = "main"
dest = "{dest.as_posix()}"
""",
        encoding="utf-8",
    )
    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", manifest)


def _prepare(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    model_name: str = "model.int8.onnx",
) -> None:
    """假权重 + 匹配 sha256 的 manifest + 假 sherpa_onnx。"""
    _make_model_dir(tmp_path, model_name)
    _patch_manifest(monkeypatch, tmp_path, model_name)
    _install_fake_sherpa(monkeypatch)


def _settings(tmp_path: Path, **kw) -> Settings:
    kw.setdefault("asr_model", _ENTRY)
    return Settings(models_dir=tmp_path, **kw)


def _pcm(seconds: float = 1.0) -> bytes:
    return b"\x01\x00" * int(16000 * seconds)


# ---------- load ----------


def test_load_missing_files(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _patch_manifest_bare(monkeypatch, tmp_path)
    p = SenseVoiceASRProvider(_settings(tmp_path))
    with pytest.raises(SenseVoiceError, match="model files missing"):
        asyncio.run(p.load())


def test_load_missing_tokens_only(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _patch_manifest_bare(monkeypatch, tmp_path)
    d = tmp_path / _MODEL_SUBDIR
    d.mkdir(parents=True)
    (d / "model.int8.onnx").write_bytes(b"fake")
    p = SenseVoiceASRProvider(_settings(tmp_path))
    with pytest.raises(SenseVoiceError, match="tokens.txt"):
        asyncio.run(p.load())


def test_load_unknown_model_lists_manifest_entries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """MOMO_ASR_MODEL 不在 MANIFEST 中 → 报错并列出可选条目名。"""
    _patch_manifest_bare(monkeypatch, tmp_path)
    p = SenseVoiceASRProvider(_settings(tmp_path, asr_model="sensevoice-small"))
    with pytest.raises(SenseVoiceError, match="unknown MOMO_ASR_MODEL='sensevoice-small'") as ei:
        asyncio.run(p.load())
    assert _ENTRY in str(ei.value)


def test_load_manifest_absent(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """无 MANIFEST 无法解析模型目录 → 报错而不是回退到硬编码路径。"""
    import momo_models.manifest as manifest_mod

    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", tmp_path / "absent.toml")
    p = SenseVoiceASRProvider(_settings(tmp_path))
    with pytest.raises(SenseVoiceError, match="MANIFEST.toml not found"):
        asyncio.run(p.load())


def test_model_name_selects_entry_dest(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """MOMO_ASR_MODEL 命中的条目 dest 决定实际加载的模型目录（可切换）。"""
    import momo_models.manifest as manifest_mod

    alt_subdir = Path("sherpa-onnx") / "sense-voice-alt"
    entries = ((_ENTRY, _MODEL_SUBDIR), ("sense-voice-alt", alt_subdir))
    for _, subdir in entries:
        d = tmp_path / subdir
        d.mkdir(parents=True)
        (d / "model.int8.onnx").write_bytes(f"fake onnx {subdir.name}".encode())
        (d / "tokens.txt").write_text("x 0\n", encoding="utf-8")

    manifest = tmp_path / "MANIFEST.toml"
    manifest.write_text(
        "\n".join(
            f"""
[[model]]
name = "{name}"
repo_id = "fake/repo"
revision = "main"
dest = "{subdir.as_posix()}"
files = [
  {{ path = "model.int8.onnx", sha256 = "todo" }},
  {{ path = "tokens.txt", sha256 = "todo" }},
]
"""
            for name, subdir in entries
        ),
        encoding="utf-8",
    )
    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", manifest)
    _install_fake_sherpa(monkeypatch)

    for name, subdir in entries:
        _FakeRecognizer.reset()
        p = SenseVoiceASRProvider(_settings(tmp_path, asr_model=name))
        asyncio.run(p.load())
        loaded = Path(_FakeRecognizer.builds[0]["model"])
        assert loaded.parent == tmp_path / subdir


def test_load_prefers_int8_and_builds_auto(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare(tmp_path, monkeypatch)
    p = SenseVoiceASRProvider(_settings(tmp_path, asr_threads=2))
    asyncio.run(p.load())
    assert _FakeRecognizer.builds[0]["language"] == "auto"
    assert _FakeRecognizer.builds[0]["num_threads"] == 2
    assert _FakeRecognizer.builds[0]["provider"] == "cpu"
    assert _FakeRecognizer.builds[0]["model"].endswith("model.int8.onnx")


def test_load_falls_back_to_full_model(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare(tmp_path, monkeypatch, model_name="model.onnx")
    p = SenseVoiceASRProvider(_settings(tmp_path))
    asyncio.run(p.load())
    assert _FakeRecognizer.builds[0]["model"].endswith("model.onnx")


def test_load_sha256_mismatch(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """MANIFEST checksum 不过 → load 抛错（readyz 拒绝就绪口径）。"""
    _make_model_dir(tmp_path)
    import hashlib

    import momo_models.manifest as manifest_mod

    d = tmp_path / _MODEL_SUBDIR
    bad_sha = hashlib.sha256(b"other").hexdigest()
    ok_sha = hashlib.sha256((d / "tokens.txt").read_bytes()).hexdigest()
    manifest = tmp_path / "MANIFEST.toml"
    manifest.write_text(
        f"""
[[model]]
name = "{_ENTRY}"
repo_id = "fake/repo"
revision = "main"
dest = "{_MODEL_SUBDIR.as_posix()}"
files = [
  {{ path = "model.int8.onnx", sha256 = "{bad_sha}" }},
  {{ path = "tokens.txt", sha256 = "{ok_sha}" }},
]
""",
        encoding="utf-8",
    )
    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", manifest)
    _install_fake_sherpa(monkeypatch)
    p = SenseVoiceASRProvider(_settings(tmp_path))
    with pytest.raises(SenseVoiceError, match="sha256"):
        asyncio.run(p.load())


def test_load_without_sherpa_raises(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """未装 sherpa_onnx（且 sys.modules 无注入）→ SenseVoiceError 提示装 extra。"""
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    monkeypatch.delitem(sys.modules, "sherpa_onnx", raising=False)
    p = SenseVoiceASRProvider(_settings(tmp_path))
    if importlib.util.find_spec("sherpa_onnx") is not None:
        pytest.skip("sherpa-onnx installed here; cannot cover the missing-dependency branch")
    with pytest.raises(SenseVoiceError, match="sherpa-onnx"):
        asyncio.run(p.load())


# ---------- transcribe ----------


def test_transcribe_before_load(tmp_path: Path) -> None:
    p = SenseVoiceASRProvider(_settings(tmp_path))
    with pytest.raises(SenseVoiceError, match="not loaded"):
        asyncio.run(p.transcribe(_pcm(), lang=None))


def test_transcribe_happy_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare(tmp_path, monkeypatch)
    p = SenseVoiceASRProvider(_settings(tmp_path))
    asyncio.run(p.load())
    t = asyncio.run(p.transcribe(_pcm(2.0), lang=None))
    assert isinstance(t, Transcript)
    assert t.text == "你好，世界。"
    assert t.lang == "zh"
    assert t.speech_emotion == "happy"
    assert t.events == ("speech",)
    assert t.duration_ms == 2000
    # 波形以 16kHz 送入
    assert _FakeRecognizer.streams[-1].rate == 16000
    assert len(_FakeRecognizer.streams[-1].samples) == 32000


def test_transcribe_lang_hint_builds_dedicated_recognizer(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _prepare(tmp_path, monkeypatch)
    p = SenseVoiceASRProvider(_settings(tmp_path))
    asyncio.run(p.load())
    asyncio.run(p.transcribe(_pcm(), lang="en"))
    asyncio.run(p.transcribe(_pcm(), lang="EN"))  # 归一化命中缓存，不重复建
    langs = [b["language"] for b in _FakeRecognizer.builds]
    assert langs == ["auto", "en"]


def test_transcribe_invalid_lang(tmp_path: Path) -> None:
    p = SenseVoiceASRProvider(_settings(tmp_path))
    with pytest.raises(ValueError, match="unsupported asr lang"):
        asyncio.run(p.transcribe(_pcm(), lang="fr"))


def test_transcribe_empty_and_odd_pcm(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare(tmp_path, monkeypatch)
    p = SenseVoiceASRProvider(_settings(tmp_path))
    asyncio.run(p.load())
    t = asyncio.run(p.transcribe(b"", lang=None))
    assert t.text == "" and t.duration_ms == 0 and t.lang == "unknown"
    # 奇数字节尾截断不炸
    t = asyncio.run(p.transcribe(_pcm() + b"\x00", lang="zh"))
    assert t.duration_ms == 1000


# ---------- 结果解析 ----------


def test_strip_tag() -> None:
    assert _strip_tag("<|HAPPY|>") == "happy"
    assert _strip_tag("zh") == "zh"
    assert _strip_tag("") == ""


def test_normalize_lang() -> None:
    assert _normalize_lang(None) == "auto"
    assert _normalize_lang("") == "auto"
    assert _normalize_lang(" EN ") == "en"
    with pytest.raises(ValueError):
        _normalize_lang("fr")


def test_parse_result_attrs() -> None:
    r = _FakeResult("你好。", "<|zh|>", "<|SAD|>", "<|Cry|>")
    text, lang, emotion, events = _parse_result(r, "auto")
    assert (text, lang, emotion, events) == ("你好。", "zh", "sad", ("cry",))


def test_parse_result_tags_inside_text_fallback() -> None:
    """旧版 sherpa 未拆元标记：从 text 中兜底解析并返回净文本。"""
    r = types.SimpleNamespace(
        text="<|yue|><|ANGRY|><|Laughter|><|woitn|>讲咩啊。",
        lang="",
        emotion="",
        event="",
    )
    text, lang, emotion, events = _parse_result(r, "auto")
    assert text == "讲咩啊。"
    assert lang == "yue"
    assert emotion == "angry"
    assert events == ("laughter",)


def test_parse_result_lang_falls_back_to_hint() -> None:
    r = types.SimpleNamespace(text="...", lang="", emotion="", event="")
    _, lang, _, _ = _parse_result(r, "en")
    assert lang == "en"
    _, lang, _, _ = _parse_result(r, "auto")
    assert lang == "unknown"


# ---------- 真模型集成（env 门控）----------

_REAL_DIR = Settings().models_dir.expanduser() / _MODEL_SUBDIR
_MODEL_PRESENT = (
    all((_REAL_DIR / f).is_file() for f in ("model.int8.onnx", "tokens.txt"))
    and importlib.util.find_spec("sherpa_onnx") is not None
)


@pytest.mark.slow
@pytest.mark.skipif(
    not _MODEL_PRESENT,
    reason="sensevoice weights / sherpa-onnx not available (uv sync --extra asr + momoctl pull)",
)
def test_transcribe_real_model() -> None:
    """真模型冒烟：zh/en 各跑一段真实 wav，打印 RTF 供人工核对 0.040 基线。"""
    import time

    import numpy as np

    from momo_models.audio import wav_unwrap

    p = SenseVoiceASRProvider(Settings())
    asyncio.run(p.load())
    for name, expect_lang in (("zh", "zh"), ("en", "en")):
        wav = _REAL_DIR / "test_wavs" / f"{name}.wav"
        if not wav.is_file():
            continue
        pcm, rate, channels = wav_unwrap(wav.read_bytes())
        assert rate == 16000 and channels == 1
        t0 = time.perf_counter()
        t = asyncio.run(p.transcribe(pcm, lang=None))
        rtf = (time.perf_counter() - t0) / (t.duration_ms / 1000)
        print(
            f"\n[{name}] {t.duration_ms}ms -> RTF={rtf:.3f} "
            f"lang={t.lang} emotion={t.speech_emotion} events={t.events}\n"
            f"  text={t.text}"
        )
        assert t.lang == expect_lang
        assert t.text.strip()
        assert np.isfinite(rtf)
