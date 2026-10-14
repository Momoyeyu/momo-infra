"""f5 provider 单测：f5_tts/torch/soundfile 全 mock，无 GPU/权重/网络。

真实拉取与加载走 `momoctl pull --only f5-tts-v1 --only vocos-mel-24khz`
+ warmup（需 torch/f5-tts 权重，标 gpu/slow）。
"""

import asyncio
import hashlib
import sys
import types
from pathlib import Path

import numpy as np
import pytest

from momo_models.audio import DOWNLINK_SAMPLE_RATE
from momo_models.config import Settings
from momo_models.providers.base import Transcript, VoiceRef
from momo_models.providers.tts import f5
from momo_models.providers.tts.f5 import F5Error, F5TTSProvider

_F5_DEST = Path("SWivid") / "F5-TTS"
_VOCOS_DEST = Path("charactr") / "vocos-mel-24khz"
_REF_NAME = "ref_calm.wav"


# ---------- 假重依赖 ----------


class _FakeF5TTS:
    """f5_tts.api.F5TTS 替身：记录构造参数，infer 返回预置波形。"""

    builds: list[dict] = []
    infer_calls: list[dict] = []
    next_wav: np.ndarray | None = None
    next_sr: int = DOWNLINK_SAMPLE_RATE

    def __init__(self, **kwargs) -> None:
        _FakeF5TTS.builds.append(kwargs)

    @classmethod
    def reset(cls) -> None:
        cls.builds = []
        cls.infer_calls = []
        cls.next_wav = _sine(seconds=1.5)
        cls.next_sr = DOWNLINK_SAMPLE_RATE

    def infer(self, **kwargs):
        _FakeF5TTS.infer_calls.append(kwargs)
        return self.next_wav, self.next_sr, None


def _sine(seconds: float = 1.5, sr: int = DOWNLINK_SAMPLE_RATE) -> np.ndarray:
    t = np.linspace(0, seconds, int(sr * seconds), endpoint=False)
    return (0.3 * np.sin(2 * np.pi * 440 * t)).astype(np.float32)


def _fake_torch(cuda: bool = False, mps: bool = False) -> types.SimpleNamespace:
    return types.SimpleNamespace(
        cuda=types.SimpleNamespace(is_available=lambda: cuda),
        backends=types.SimpleNamespace(mps=types.SimpleNamespace(is_available=lambda: mps)),
    )


def _fake_soundfile(
    sr: int = 24000, channels: int = 1, frames: int = 48000, fail_read: bool = False
) -> types.ModuleType:
    """最小 soundfile 替身：read 回零矩阵，write/info 记录参数。"""
    mod = types.ModuleType("soundfile")
    calls: dict = {"writes": []}

    def read(path, dtype="float32", always_2d=True):
        if fail_read:
            raise RuntimeError("not an audio file")
        return np.zeros((frames, channels), dtype=np.float32), sr

    def write(path, data, samplerate, subtype=None):
        calls["writes"].append({"path": path, "sr": samplerate, "n": len(data), "subtype": subtype})

    mod.read = read
    mod.write = write
    mod.calls = calls
    return mod


def _install_fake_deps(
    monkeypatch: pytest.MonkeyPatch, *, cuda: bool = False, mps: bool = False, **sf_kw
) -> types.ModuleType:
    """注入假 f5_tts.api / torch / soundfile；返回假 soundfile 供断言。"""
    _FakeF5TTS.reset()
    monkeypatch.setitem(sys.modules, "f5_tts", types.ModuleType("f5_tts"))
    api = types.ModuleType("f5_tts.api")
    api.F5TTS = _FakeF5TTS
    monkeypatch.setitem(sys.modules, "f5_tts.api", api)
    monkeypatch.setitem(sys.modules, "torch", _fake_torch(cuda=cuda, mps=mps))
    sf = _fake_soundfile(**sf_kw)
    monkeypatch.setitem(sys.modules, "soundfile", sf)
    return sf


# ---------- 假模型目录与清单 ----------


def _make_model_dirs(root: Path) -> None:
    f5d = root / _F5_DEST / "F5TTS_v1_Base"
    f5d.mkdir(parents=True)
    (f5d / "model_1250000.safetensors").write_bytes(b"fake f5 ckpt")
    (f5d / "vocab.txt").write_text("x\n", encoding="utf-8")
    vd = root / _VOCOS_DEST
    vd.mkdir(parents=True)
    (vd / "config.yaml").write_text("vocos: cfg\n", encoding="utf-8")
    (vd / "pytorch_model.bin").write_bytes(b"fake vocos")


def _patch_manifest(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, *, bad_ckpt_sha: bool = False
) -> None:
    """写一份 sha256 与假文件一致的 MANIFEST 并替换 DEFAULT_MANIFEST。"""
    import momo_models.manifest as manifest_mod

    def sha(p: Path) -> str:
        return hashlib.sha256(p.read_bytes()).hexdigest()

    f5d = tmp_path / _F5_DEST / "F5TTS_v1_Base"
    vd = tmp_path / _VOCOS_DEST
    ckpt_sha = sha(f5d / "model_1250000.safetensors")
    if bad_ckpt_sha:
        ckpt_sha = hashlib.sha256(b"other").hexdigest()
    manifest = tmp_path / "MANIFEST.toml"
    manifest.write_text(
        f"""
[[model]]
name = "{f5._F5_ENTRY}"
repo_id = "SWivid/F5-TTS"
revision = "main"
dest = "{_F5_DEST.as_posix()}"
files = [
  {{ path = "F5TTS_v1_Base/model_1250000.safetensors", sha256 = "{ckpt_sha}" }},
  {{ path = "F5TTS_v1_Base/vocab.txt", sha256 = "{sha(f5d / "vocab.txt")}" }},
]

[[model]]
name = "{f5._VOCOS_ENTRY}"
repo_id = "charactr/vocos-mel-24khz"
revision = "main"
dest = "{_VOCOS_DEST.as_posix()}"
files = [
  {{ path = "config.yaml", sha256 = "{sha(vd / "config.yaml")}" }},
  {{ path = "pytorch_model.bin", sha256 = "{sha(vd / "pytorch_model.bin")}" }},
]
""",
        encoding="utf-8",
    )
    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", manifest)


def _prepare(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, **dep_kw) -> types.ModuleType:
    """假权重 + 匹配 sha256 的 manifest + 假 f5_tts/torch/soundfile。"""
    _make_model_dirs(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    return _install_fake_deps(monkeypatch, **dep_kw)


def _provider(tmp_path: Path, **kw) -> F5TTSProvider:
    kw.setdefault("models_dir", tmp_path)
    kw.setdefault("tts_device", "cpu")
    return F5TTSProvider(Settings(**kw))


def _ref_file(tmp_path: Path) -> Path:
    p = tmp_path / _REF_NAME
    p.write_bytes(b"fake wav")
    return p


def _collect(p: F5TTSProvider, text: str = "你好", voice: VoiceRef | None = None) -> list[bytes]:
    async def run() -> list[bytes]:
        return [c async for c in p.synth_stream(text, voice=voice or VoiceRef(), emotion=None)]

    return asyncio.run(run())


# ---------- capabilities / warmup ----------


def test_capabilities() -> None:
    caps = F5TTSProvider(Settings()).capabilities()
    assert caps.streaming is False
    assert caps.languages == ("zh", "en")
    assert caps.clone is True
    assert caps.emotion is False


def test_warmup_ok(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """双条目校验通过 → 以本地 ckpt/vocab/vocos 目录构建 F5TTS。"""
    _prepare(tmp_path, monkeypatch, mps=True)
    p = _provider(tmp_path, tts_device="mps")
    asyncio.run(p.warmup())
    build = _FakeF5TTS.builds[0]
    assert build["model"] == "F5TTS_v1_Base"
    assert build["ckpt_file"].endswith("F5TTS_v1_Base/model_1250000.safetensors")
    assert build["vocab_file"].endswith("F5TTS_v1_Base/vocab.txt")
    assert Path(build["vocoder_local_path"]) == tmp_path / _VOCOS_DEST
    assert build["device"] == "mps"


def test_warmup_missing_files(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """权重缺失 → F5Error 列出缺失文件并提示 pull（readyz 拒就绪）。"""
    _make_model_dirs(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    (tmp_path / _VOCOS_DEST / "pytorch_model.bin").unlink()
    _install_fake_deps(monkeypatch)
    p = _provider(tmp_path)
    with pytest.raises(F5Error, match="vocos-mel-24khz.*missing"):
        asyncio.run(p.warmup())


def test_warmup_sha_mismatch(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """MANIFEST checksum 不过 → warmup 抛错。"""
    _make_model_dirs(tmp_path)
    _patch_manifest(monkeypatch, tmp_path, bad_ckpt_sha=True)
    _install_fake_deps(monkeypatch)
    p = _provider(tmp_path)
    with pytest.raises(F5Error, match="sha256 mismatch"):
        asyncio.run(p.warmup())


def test_warmup_manifest_absent(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """无 MANIFEST 无法解析模型目录 → 报错而不是回退硬编码路径。"""
    import momo_models.manifest as manifest_mod

    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", tmp_path / "absent.toml")
    p = _provider(tmp_path)
    with pytest.raises(F5Error, match="MANIFEST.toml not found"):
        asyncio.run(p.warmup())


def test_warmup_missing_deps(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """f5_tts 未安装（sys.modules 置 None 强制 ImportError）→ F5Error 提示 extra。"""
    _prepare(tmp_path, monkeypatch)
    monkeypatch.setitem(sys.modules, "f5_tts.api", None)
    p = _provider(tmp_path)
    with pytest.raises(F5Error, match="extra tts"):
        asyncio.run(p.warmup())


def test_warmup_cuda_unavailable(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """MOMO_TTS_DEVICE=cuda 但 CUDA 不可用 → 明确报错而非运行时崩溃。"""
    _prepare(tmp_path, monkeypatch)  # fake torch cuda.is_available()=False
    p = _provider(tmp_path, tts_device="cuda")
    with pytest.raises(F5Error, match="CUDA is unavailable"):
        asyncio.run(p.warmup())


def test_warmup_entry_absent(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """MANIFEST 无 f5-tts-v1 条目 → 报错并列出可选条目名。"""
    import momo_models.manifest as manifest_mod

    manifest = tmp_path / "MANIFEST.toml"
    manifest.write_text(
        '[[model]]\nname = "other"\nrepo_id = "a/b"\nrevision = "main"\ndest = "a/b"\n',
        encoding="utf-8",
    )
    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", manifest)
    p = _provider(tmp_path)
    with pytest.raises(F5Error, match="no entry 'f5-tts-v1'"):
        asyncio.run(p.warmup())


# ---------- synth_stream ----------


def test_synth_requires_warmup(tmp_path: Path) -> None:
    p = _provider(tmp_path)
    with pytest.raises(F5Error, match="not warmed up"):
        _collect(p)


def test_synth_empty_text(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare(tmp_path, monkeypatch)
    p = _provider(tmp_path)
    asyncio.run(p.warmup())
    assert _collect(p, text="  ") == []
    assert _FakeF5TTS.infer_calls == []


def test_synth_requires_ref_audio(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """克隆必需参考音：无 MOMO_TTS_VOICE_REF / VoiceRef.ref_audio → 拒绝。"""
    _prepare(tmp_path, monkeypatch)
    p = _provider(tmp_path)
    asyncio.run(p.warmup())
    with pytest.raises(F5Error, match="MOMO_TTS_VOICE_REF"):
        _collect(p)
    with pytest.raises(F5Error, match="not found"):
        _collect(p, voice=VoiceRef(ref_audio=tmp_path / "nope.wav"))


def test_synth_happy_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """整段合成 → 2400B/50ms 帧回吐；ref 统一写 24k PCM16 WAV 后喂 infer。"""
    sf = _prepare(tmp_path, monkeypatch)
    p = _provider(tmp_path)
    asyncio.run(p.warmup())
    ref = _ref_file(tmp_path)
    chunks = _collect(p, text="你好，世界", voice=VoiceRef(ref_audio=ref, ref_text="参考文本"))

    assert len(chunks) > 1  # 1.5s → 30 帧
    assert all(len(c) % 2 == 0 for c in chunks)
    assert all(len(c) == 2400 for c in chunks[:-1])
    assert len(b"".join(chunks)) == len(_FakeF5TTS.next_wav) * 2

    call = _FakeF5TTS.infer_calls[0]
    assert call["ref_text"] == "参考文本"
    assert call["gen_text"] == "你好，世界"
    assert call["ref_file"].endswith("ref.wav")
    # 参考音经 soundfile 统一转写为 24kHz PCM16 临时 WAV
    assert sf.calls["writes"][0]["sr"] == DOWNLINK_SAMPLE_RATE
    assert sf.calls["writes"][0]["subtype"] == "PCM_16"


def test_synth_resamples_non_24k_output(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """infer 返回非 24kHz 波形 → 重采样到契约口径。"""
    _prepare(tmp_path, monkeypatch)
    _FakeF5TTS.next_sr = 48000
    _FakeF5TTS.next_wav = _sine(seconds=1.0, sr=48000)
    p = _provider(tmp_path)
    asyncio.run(p.warmup())
    chunks = _collect(p, voice=VoiceRef(ref_audio=_ref_file(tmp_path), ref_text="x"))
    total_samples = len(b"".join(chunks)) // 2
    assert abs(total_samples - 24000) < 240  # 线性插值误差容忍


def test_synth_ref_text_sidecar(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """缺 ref_text → 参考音同名 .txt sidecar 兜底。"""
    _prepare(tmp_path, monkeypatch)
    p = _provider(tmp_path)
    asyncio.run(p.warmup())
    ref = _ref_file(tmp_path)
    ref.with_suffix(".txt").write_text(" sidecar transcript \n", encoding="utf-8")
    _collect(p, voice=VoiceRef(ref_audio=ref))
    assert _FakeF5TTS.infer_calls[0]["ref_text"] == "sidecar transcript"


def test_synth_ref_text_sensevoice_fallback(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """无 ref_text 无 sidecar → SenseVoice 兜底转写，并按 (path, mtime) 缓存。"""

    class _FakeASR:
        calls = 0

        def __init__(self, settings) -> None:
            pass

        async def load(self) -> None:
            pass

        async def transcribe(self, pcm: bytes, *, lang) -> Transcript:
            _FakeASR.calls += 1
            assert len(pcm) % 2 == 0  # s16le 16k 上行口径
            return Transcript(text="auto transcript", lang="zh", duration_ms=100)

    monkeypatch.setattr("momo_models.providers.asr.sensevoice.SenseVoiceASRProvider", _FakeASR)
    _prepare(tmp_path, monkeypatch)
    p = _provider(tmp_path)
    asyncio.run(p.warmup())
    ref = _ref_file(tmp_path)
    _collect(p, voice=VoiceRef(ref_audio=ref))
    _collect(p, text="第二句", voice=VoiceRef(ref_audio=ref))
    assert [c["ref_text"] for c in _FakeF5TTS.infer_calls] == [
        "auto transcript",
        "auto transcript",
    ]
    assert _FakeASR.calls == 1  # 缓存命中，不重复 ASR


def test_synth_ref_text_all_empty(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """三途皆空（无 ref_text/无 sidecar/ASR 转写为空）→ 明确报错。"""

    class _EmptyASR:
        def __init__(self, settings) -> None:
            pass

        async def load(self) -> None:
            pass

        async def transcribe(self, pcm: bytes, *, lang) -> Transcript:
            return Transcript(text="", lang="zh", duration_ms=0)

    monkeypatch.setattr("momo_models.providers.asr.sensevoice.SenseVoiceASRProvider", _EmptyASR)
    _prepare(tmp_path, monkeypatch)
    p = _provider(tmp_path)
    asyncio.run(p.warmup())
    with pytest.raises(F5Error, match="ref_text unavailable"):
        _collect(p, voice=VoiceRef(ref_audio=_ref_file(tmp_path)))


def test_synth_ref_text_asr_unavailable(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """SenseVoice 兜底自身不可用 → F5Error 指给 sidecar/安装路径。"""

    class _BrokenASR:
        def __init__(self, settings) -> None:
            pass

        async def load(self) -> None:
            raise RuntimeError("sherpa-onnx not installed")

    monkeypatch.setattr("momo_models.providers.asr.sensevoice.SenseVoiceASRProvider", _BrokenASR)
    _prepare(tmp_path, monkeypatch)
    p = _provider(tmp_path)
    asyncio.run(p.warmup())
    with pytest.raises(F5Error, match="SenseVoice fallback unavailable"):
        _collect(p, voice=VoiceRef(ref_audio=_ref_file(tmp_path)))


def test_synth_ref_sample_rate_too_low(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """素材规范：参考音 <24kHz → 拒绝（voice-clone.md）。"""
    _prepare(tmp_path, monkeypatch)
    monkeypatch.setitem(sys.modules, "soundfile", _fake_soundfile(sr=16000))
    p = _provider(tmp_path)
    asyncio.run(p.warmup())
    with pytest.raises(F5Error, match="sample rate 16000Hz"):
        _collect(p, voice=VoiceRef(ref_audio=_ref_file(tmp_path), ref_text="x"))


def test_synth_ref_unreadable(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare(tmp_path, monkeypatch)
    monkeypatch.setitem(sys.modules, "soundfile", _fake_soundfile(fail_read=True))
    p = _provider(tmp_path)
    asyncio.run(p.warmup())
    with pytest.raises(F5Error, match="unreadable"):
        _collect(p, voice=VoiceRef(ref_audio=_ref_file(tmp_path), ref_text="x"))


def test_cancel_stops_output(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """cancel 后在下一 chunk 边界停止产出（协议 §8 尽力取消）。"""
    _prepare(tmp_path, monkeypatch)
    p = _provider(tmp_path)
    asyncio.run(p.warmup())

    async def run() -> tuple[bytes, list[bytes]]:
        it = p.synth_stream(
            "你好", voice=VoiceRef(ref_audio=_ref_file(tmp_path), ref_text="x"), emotion=None
        )
        first = await it.__anext__()
        await p.cancel("r_1")
        rest = [c async for c in it]
        return first, rest

    first, rest = asyncio.run(run())
    assert first
    assert rest == []


def test_synth_none_wav_yields_nothing(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """f5 infer 对空 batch 返回 wav=None → 零产出（服务层上报 no audio）。"""
    _prepare(tmp_path, monkeypatch)
    _FakeF5TTS.next_wav = None
    p = _provider(tmp_path)
    asyncio.run(p.warmup())
    assert _collect(p, voice=VoiceRef(ref_audio=_ref_file(tmp_path), ref_text="x")) == []
