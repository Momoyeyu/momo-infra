"""kokoro provider 单测：fake torch/kokoro 注入 sys.modules，全 mock 无重依赖。

真实合成冒烟见 `test_real_kokoro_synthesis`（需 `uv sync --extra tts` +
torch 平台档 + `momoctl pull --only kokoro-82m`，slow + 权重门控 skip）。
"""

import asyncio
import hashlib
import importlib.util
import sys
import types
from pathlib import Path

import numpy as np
import pytest

from momo_models.config import Settings
from momo_models.providers.base import VoiceRef
from momo_models.providers.tts import kokoro
from momo_models.providers.tts.kokoro import KokoroError, KokoroTTSProvider

# 与仓库 MANIFEST（models/models/MANIFEST.toml）的 kokoro-82m 条目对齐。
_MODEL_SUBDIR = Path("hexgrad") / "Kokoro-82M"
_ENTRY = "kokoro-82m"
_MODEL_FILES = (
    "kokoro-v1_0.pth",
    "config.json",
    "voices/af_heart.pt",
    "voices/zf_xiaoxiao.pt",
    "voices/jf_alpha.pt",
)

# ---------- 假 torch / kokoro ----------


def _fake_torch(*, cuda: bool = False, mps: bool = False) -> types.ModuleType:
    mod = types.ModuleType("torch")
    mod.cuda = types.SimpleNamespace(is_available=lambda: cuda)
    mod.backends = types.SimpleNamespace(mps=types.SimpleNamespace(is_available=lambda: mps))
    return mod


def _fake_kokoro(chunks: list) -> types.ModuleType:
    """最小 kokoro 替身：KModel/KPipeline 记录构造与调用参数。"""
    mod = types.ModuleType("kokoro")
    mod.kmodel_calls = []
    mod.pipeline_calls = []
    mod.synth_calls = []

    class KModel:
        def __init__(self, repo_id=None, config=None, model=None, **_kw):
            mod.kmodel_calls.append({"repo_id": repo_id, "config": config, "model": model})

        def to(self, device):
            mod.device = device
            return self

        def eval(self):
            return self

    class KPipeline:
        def __init__(self, lang_code, repo_id=None, model=None, **_kw):
            mod.pipeline_calls.append({"lang_code": lang_code, "repo_id": repo_id, "model": model})

        def __call__(self, text, voice=None, speed=1, **_kw):
            mod.synth_calls.append({"text": text, "voice": voice, "speed": speed})
            # 真实 kokoro 产 KPipeline.Result（.audio）；裸 ndarray 包装成同形对象
            for c in chunks:
                if hasattr(c, "audio") or isinstance(c, tuple):
                    yield c
                else:
                    yield types.SimpleNamespace(audio=c)

    mod.KModel = KModel
    mod.KPipeline = KPipeline
    return mod


def _install_fakes(
    monkeypatch: pytest.MonkeyPatch, chunks: list, *, cuda=False, mps=False
) -> types.ModuleType:
    monkeypatch.setitem(sys.modules, "torch", _fake_torch(cuda=cuda, mps=mps))
    mod = _fake_kokoro(chunks)
    monkeypatch.setitem(sys.modules, "kokoro", mod)
    return mod


# ---------- 假模型目录 + MANIFEST ----------


def _make_model_dir(root: Path) -> Path:
    d = root / _MODEL_SUBDIR
    for rel in _MODEL_FILES:
        p = d / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(f"fake {rel}".encode())
    return d


def _patch_manifest(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """写一份 sha256 与假文件一致的 MANIFEST 并替换 DEFAULT_MANIFEST。"""
    import momo_models.manifest as manifest_mod

    d = tmp_path / _MODEL_SUBDIR
    files = ",\n".join(
        f'  {{ path = "{rel}", sha256 = "{hashlib.sha256((d / rel).read_bytes()).hexdigest()}" }}'
        for rel in _MODEL_FILES
    )
    manifest = tmp_path / "MANIFEST.toml"
    manifest.write_text(
        f"""
[[model]]
name = "{_ENTRY}"
repo_id = "fake/repo"
revision = "main"
dest = "{_MODEL_SUBDIR.as_posix()}"
files = [
{files}
]
""",
        encoding="utf-8",
    )
    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", manifest)


def _patch_manifest_bare(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """只登记条目名/dest（无 files），隔离真实仓库清单。"""
    import momo_models.manifest as manifest_mod

    manifest = tmp_path / "MANIFEST.toml"
    manifest.write_text(
        f"""
[[model]]
name = "{_ENTRY}"
repo_id = "fake/repo"
revision = "main"
dest = "{_MODEL_SUBDIR.as_posix()}"
""",
        encoding="utf-8",
    )
    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", manifest)


def _settings(tmp_path: Path, **kw) -> Settings:
    kw.setdefault("tts_device", "cpu")
    return Settings(models_dir=tmp_path, **kw)


def _float_audio(n: int = 4800) -> np.ndarray:
    """200ms 24kHz float32 伪波形。"""
    t = np.arange(n, dtype=np.float32) / 24000.0
    return 0.2 * np.sin(2 * np.pi * 220 * t)


def _collect(p: KokoroTTSProvider, text: str = "你好", **kw) -> list[bytes]:
    voice = kw.pop("voice", VoiceRef())
    emotion = kw.pop("emotion", None)

    async def run() -> list[bytes]:
        return [c async for c in p.synth_stream(text, voice=voice, emotion=emotion)]

    return asyncio.run(run())


# ---------- warmup ----------


def test_warmup_happy_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    mod = _install_fakes(monkeypatch, [])
    p = KokoroTTSProvider(_settings(tmp_path))
    asyncio.run(p.warmup())
    call = mod.kmodel_calls[0]
    assert call["repo_id"] == "hexgrad/Kokoro-82M"
    assert call["config"].endswith("config.json")
    assert call["model"].endswith("kokoro-v1_0.pth")
    assert mod.device == "cpu"


def test_warmup_missing_files(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _patch_manifest_bare(monkeypatch, tmp_path)
    _install_fakes(monkeypatch, [])
    p = KokoroTTSProvider(_settings(tmp_path))
    with pytest.raises(KokoroError, match="模型文件缺失"):
        asyncio.run(p.warmup())


def test_warmup_sha256_mismatch(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """checksum 不过 → warmup 抛错（readyz 拒绝就绪口径）。"""
    _make_model_dir(tmp_path)
    import momo_models.manifest as manifest_mod

    bad = hashlib.sha256(b"other").hexdigest()
    manifest = tmp_path / "MANIFEST.toml"
    manifest.write_text(
        f"""
[[model]]
name = "{_ENTRY}"
repo_id = "fake/repo"
revision = "main"
dest = "{_MODEL_SUBDIR.as_posix()}"
files = [
  {{ path = "kokoro-v1_0.pth", sha256 = "{bad}" }},
]
""",
        encoding="utf-8",
    )
    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", manifest)
    _install_fakes(monkeypatch, [])
    p = KokoroTTSProvider(_settings(tmp_path))
    with pytest.raises(KokoroError, match="sha256"):
        asyncio.run(p.warmup())


def test_warmup_manifest_absent(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import momo_models.manifest as manifest_mod

    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", tmp_path / "absent.toml")
    p = KokoroTTSProvider(_settings(tmp_path))
    with pytest.raises(KokoroError, match="MANIFEST.toml not found"):
        asyncio.run(p.warmup())


def test_warmup_missing_deps(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    monkeypatch.setitem(sys.modules, "kokoro", None)  # import 必炸
    monkeypatch.setitem(sys.modules, "torch", None)
    p = KokoroTTSProvider(_settings(tmp_path))
    with pytest.raises(KokoroError, match="extra"):
        asyncio.run(p.warmup())


def test_warmup_cuda_unavailable(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """MOMO_TTS_DEVICE=cuda 但无 CUDA → 不就绪（不静默降级）。"""
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    _install_fakes(monkeypatch, [], cuda=False)
    p = KokoroTTSProvider(_settings(tmp_path, tts_device="cuda"))
    with pytest.raises(KokoroError, match="MOMO_TTS_DEVICE"):
        asyncio.run(p.warmup())


def test_warmup_device_selected(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    mod = _install_fakes(monkeypatch, [], cuda=True)
    p = KokoroTTSProvider(_settings(tmp_path, tts_device="cuda"))
    asyncio.run(p.warmup())
    assert mod.device == "cuda"


# ---------- synth_stream ----------


def _warmed(p: KokoroTTSProvider) -> None:
    asyncio.run(p.warmup())


def test_synth_stream_yields_pcm(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    mod = _install_fakes(monkeypatch, [_float_audio(), _float_audio()])
    p = KokoroTTSProvider(_settings(tmp_path))
    _warmed(p)
    chunks = _collect(p)
    assert len(chunks) == 2
    assert all(len(c) % 2 == 0 for c in chunks)  # s16le 对齐
    call = mod.synth_calls[0]
    assert call["voice"].endswith("voices/zf_xiaoxiao.pt")
    assert call["speed"] == 1.0
    assert mod.pipeline_calls[0]["lang_code"] == "z"


def test_synth_stream_before_warmup(tmp_path: Path) -> None:
    p = KokoroTTSProvider(_settings(tmp_path))
    with pytest.raises(KokoroError, match="未 warmup"):
        _collect(p)


def test_synth_stream_empty_text(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    mod = _install_fakes(monkeypatch, [_float_audio()])
    p = KokoroTTSProvider(_settings(tmp_path))
    _warmed(p)
    assert _collect(p, text="  ") == []
    assert mod.synth_calls == []


def test_synth_stream_emotion_maps_to_speed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """#33：emotion tag 经 map_emotion → KPipeline speed。"""
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    mod = _install_fakes(monkeypatch, [_float_audio()])
    p = KokoroTTSProvider(_settings(tmp_path))
    _warmed(p)
    _collect(p, emotion="happy")
    assert mod.synth_calls[0]["speed"] == pytest.approx(1.08)
    _collect(p, emotion=None)
    assert mod.synth_calls[1]["speed"] == 1.0


def test_synth_stream_voice_priority(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """MOMO_TTS_VOICE > lang 映射；音色文件缺失 → KokoroError。"""
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    mod = _install_fakes(monkeypatch, [_float_audio()])
    p = KokoroTTSProvider(_settings(tmp_path, tts_voice="af_heart"))
    _warmed(p)
    _collect(p, voice=VoiceRef(lang="ja"))
    assert mod.synth_calls[0]["voice"].endswith("voices/af_heart.pt")
    assert mod.pipeline_calls[-1]["lang_code"] == "a"  # 音色首字母定 pipeline


def test_synth_stream_lang_maps_voice(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    mod = _install_fakes(monkeypatch, [_float_audio()])
    p = KokoroTTSProvider(_settings(tmp_path))
    _warmed(p)
    _collect(p, voice=VoiceRef(lang="en"))
    assert mod.synth_calls[0]["voice"].endswith("voices/af_heart.pt")
    _collect(p, voice=VoiceRef(lang="ja"))
    assert mod.synth_calls[1]["voice"].endswith("voices/jf_alpha.pt")
    assert mod.pipeline_calls[-1]["lang_code"] == "j"


def test_synth_stream_missing_voice(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    _install_fakes(monkeypatch, [_float_audio()])
    p = KokoroTTSProvider(_settings(tmp_path, tts_voice="no_such"))
    _warmed(p)
    with pytest.raises(KokoroError, match="音色文件缺失"):
        _collect(p)


def test_cancel_stops_stream(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """cancel 后在下一 chunk 边界停止产出（协议 §8 尽力取消）。"""
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    _install_fakes(monkeypatch, [_float_audio() for _ in range(5)])
    p = KokoroTTSProvider(_settings(tmp_path))
    _warmed(p)

    async def run() -> tuple[bytes, list[bytes]]:
        it = p.synth_stream("你好", voice=VoiceRef(), emotion=None)
        first = await it.__anext__()
        await p.cancel("r_1")
        rest = [c async for c in it]
        return first, rest

    first, rest = asyncio.run(run())
    assert first
    assert rest == []


def test_result_audio_tuple_compat() -> None:
    """旧版 kokoro 三元组 (gs, ps, audio) 兼容。"""
    audio = np.zeros(8, dtype=np.float32)
    assert kokoro._result_audio(("g", "p", audio)) is audio
    assert kokoro._result_audio(types.SimpleNamespace(audio=audio)) is audio
    assert kokoro._result_audio(("g", "p", None)) is None


def test_resolve_voice() -> None:
    """优先级：MOMO_TTS_VOICE > VoiceRef.lang > MOMO_TTS_LANG > zh 兜底。"""
    s = Settings
    assert kokoro._resolve_voice(s(tts_voice="af_bella"), VoiceRef(lang="ja")) == "af_bella"
    assert kokoro._resolve_voice(s(), VoiceRef(lang="en")) == "af_heart"
    assert kokoro._resolve_voice(s(), VoiceRef(lang="ja")) == "jf_alpha"
    assert kokoro._resolve_voice(s(tts_lang="en"), VoiceRef()) == "af_heart"
    assert kokoro._resolve_voice(s(), VoiceRef()) == "zf_xiaoxiao"


def test_resolve_lang_code() -> None:
    """pipeline lang_code：音色首字母优先，其次语言映射。"""
    s = Settings
    assert kokoro._resolve_lang_code(s(), VoiceRef(), "af_heart") == "a"
    assert kokoro._resolve_lang_code(s(), VoiceRef(lang="ja"), "no_prefix") == "j"
    assert kokoro._resolve_lang_code(s(), VoiceRef(), "no_prefix") == "z"


# ---------- 真模型冒烟（env 门控）----------

_REAL_DIR = Settings().models_dir.expanduser() / _MODEL_SUBDIR
_MODEL_PRESENT = (
    all((_REAL_DIR / f).is_file() for f in _MODEL_FILES)
    and importlib.util.find_spec("kokoro") is not None
    and importlib.util.find_spec("torch") is not None
)


@pytest.mark.slow
@pytest.mark.skipif(
    not _MODEL_PRESENT,
    reason="kokoro 权重/依赖未就绪（需 uv sync --extra tts + torch 档 + momoctl pull）",
)
def test_real_kokoro_synthesis() -> None:
    """真实 kokoro 合成冒烟：产出非空 PCM，打印首包与总时长供人工核对基线。"""
    import time

    p = KokoroTTSProvider(Settings(tts_device="cpu"))
    asyncio.run(p.warmup())

    async def run() -> tuple[list[bytes], float]:
        t0 = time.perf_counter()
        first_ms = -1.0
        out = []
        async for c in p.synth_stream("你好，我是默默。", voice=VoiceRef(), emotion="happy"):
            if first_ms < 0:
                first_ms = (time.perf_counter() - t0) * 1000
            out.append(c)
        return out, first_ms

    chunks, first_ms = asyncio.run(run())
    total = sum(map(len, chunks))
    print(
        f"\nfirst_audio={first_ms:.0f}ms chunks={len(chunks)} pcm={total}B "
        f"({total / 48000:.2f}s @24k s16le)"
    )
    assert chunks and total > 4800  # >100ms 的 24k s16le
