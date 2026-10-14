"""cosyvoice3 provider 单测：官方推理栈不 import——worker 用 fake python 子进程。

worker 协议（JSON 头行 + f32le 负载）由 _FAKE_WORKER 按同一约定实现，
真实 asyncio 子进程跑通管道路径；模型权重/manifest/参考音全部 fake，
无 GPU/网络。真实拉取与加载走 `momoctl pull --only cosyvoice3-0.5b`
+ 部署 MOMO_COSYVOICE3_HOME/PYTHON 后 warmup（标 gpu/slow）。
"""

import asyncio
import hashlib
import json
import sys
import types
from pathlib import Path

import numpy as np
import pytest

from momo_models.config import Settings
from momo_models.providers.base import Transcript, VoiceRef
from momo_models.providers.tts import cosyvoice3
from momo_models.providers.tts.cosyvoice3 import CosyVoice3Error, CosyVoice3Provider

_DEST = Path("FunAudioLLM") / "Fun-CosyVoice3-0.5B-2512"
_REF_NAME = "ref_calm.wav"

# ---------- 假 worker（与 cosyvoice3_worker.py 同一 stdio 协议）----------

_FAKE_WORKER = r"""
import json, os, queue, struct, sys, threading, time

def emit(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()

if os.environ.get("FAKE_WORKER_FAIL"):
    emit({"type": "error", "phase": "load", "message": "fake load failure"})
    raise SystemExit(3)
if os.environ.get("FAKE_WORKER_EXIT"):
    raise SystemExit(2)

emit({"type": "ready", "sample_rate": int(os.environ.get("FAKE_WORKER_SR", "48000"))})

requests = queue.Queue()
cancel = threading.Event()

def reader():
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        req = json.loads(line)
        if req.get("type") == "cancel":
            cancel.set()
        elif req.get("type") == "shutdown":
            requests.put(None)
            return
        else:
            requests.put(req)
    requests.put(None)

threading.Thread(target=reader, daemon=True).start()
logp = os.environ.get("FAKE_WORKER_LOG")

while True:
    req = requests.get()
    if req is None:
        break
    if logp:
        with open(logp, "a") as f:
            f.write(json.dumps(req) + "\n")
    n = int(os.environ.get("FAKE_WORKER_CHUNKS", "3"))
    frames = int(os.environ.get("FAKE_WORKER_FRAMES", "2400"))
    delay = float(os.environ.get("FAKE_WORKER_DELAY", "0"))
    cancelled = False
    for _ in range(n):
        if cancel.is_set():
            cancelled = True
            break
        payload = struct.pack("<f", 0.1) * frames
        emit({"type": "audio", "n": len(payload), "req": req.get("id")})
        sys.stdout.buffer.write(payload)
        sys.stdout.buffer.flush()
        if delay:
            time.sleep(delay)
    emit({"type": "eos", "cancelled": cancelled, "req": req.get("id")})
    cancel.clear()
"""


def _fake_worker(tmp_path: Path) -> Path:
    p = tmp_path / "fake_worker.py"
    p.write_text(_FAKE_WORKER, encoding="utf-8")
    return p


def _fake_home(tmp_path: Path) -> Path:
    """假 CosyVoice checkout：cosyvoice/cli/cosyvoice.py + Matcha-TTS 子模块位。"""
    home = tmp_path / "CosyVoice"
    (home / "cosyvoice" / "cli").mkdir(parents=True, exist_ok=True)
    (home / "cosyvoice" / "cli" / "cosyvoice.py").write_text("# fake\n", encoding="utf-8")
    (home / "third_party" / "Matcha-TTS").mkdir(parents=True, exist_ok=True)
    return home


_MODEL_FILES = (
    "cosyvoice3.yaml",
    "llm.pt",
    "flow.pt",
    "hift.pt",
    "campplus.onnx",
    "speech_tokenizer_v3.onnx",
    "CosyVoice-BlankEN/config.json",
)


def _make_model_dir(root: Path) -> Path:
    dest = root / _DEST
    dest.mkdir(parents=True)
    (dest / "CosyVoice-BlankEN").mkdir(exist_ok=True)
    for rel in _MODEL_FILES:
        (dest / rel).write_bytes(f"fake {rel}".encode())
    return dest


def _patch_manifest(monkeypatch: pytest.MonkeyPatch, root: Path, *, bad_sha: bool = False) -> None:
    """写一份 sha256 与假文件一致的 MANIFEST 并替换 DEFAULT_MANIFEST。"""
    import momo_models.manifest as manifest_mod

    def sha(p: Path) -> str:
        return hashlib.sha256(p.read_bytes()).hexdigest()

    dest = root / _DEST
    files = []
    for i, rel in enumerate(_MODEL_FILES):
        s = sha(dest / rel)
        if bad_sha and i == 0:
            s = hashlib.sha256(b"other").hexdigest()
        files.append(f'  {{ path = "{rel}", sha256 = "{s}" }},')
    manifest = root / "MANIFEST.toml"
    manifest.write_text(
        f"""
[[model]]
name = "{cosyvoice3._ENTRY}"
repo_id = "FunAudioLLM/Fun-CosyVoice3-0.5B-2512"
revision = "main"
dest = "{_DEST.as_posix()}"
files = [
{chr(10).join(files)}
]
""",
        encoding="utf-8",
    )
    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", manifest)


def _fake_soundfile(
    sr: int = 24000, channels: int = 1, frames: int = 48000, fail_read: bool = False
) -> types.ModuleType:
    mod = types.ModuleType("soundfile")
    writes: list[dict] = []

    def read(path, dtype="float32", always_2d=True):
        if fail_read:
            raise RuntimeError("not an audio file")
        return np.zeros((frames, channels), dtype=np.float32), sr

    def write(path, data, samplerate, subtype=None):
        writes.append({"path": path, "sr": samplerate, "n": len(data), "subtype": subtype})

    mod.read = read
    mod.write = write
    mod.writes = writes
    return mod


def _settings(tmp_path: Path, fake_home: Path | None = None, **kw) -> Settings:
    kw.setdefault("models_dir", tmp_path)
    kw.setdefault("tts_device", "cpu")
    if fake_home is None:
        fake_home = _fake_home(tmp_path)
    kw.setdefault("cosyvoice3_home", str(fake_home))
    kw.setdefault("cosyvoice3_python", sys.executable)
    return Settings(**kw)


def _make_provider(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    env: dict[str, str] | None = None,
    **kw,
) -> CosyVoice3Provider:
    """假权重 + 匹配 manifest + 假栈 + 假 worker；返回未 warmup 的 provider。

    asyncio 子进程绑定创建它的 loop：warmup 与 synth 必须在同一个
    asyncio.run 内完成，故 warmup 不进 fixture。
    """
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    monkeypatch.setattr(cosyvoice3, "_WORKER", _fake_worker(tmp_path))
    monkeypatch.setitem(sys.modules, "soundfile", _fake_soundfile())
    monkeypatch.setenv("FAKE_WORKER_LOG", str(tmp_path / "worker.log"))
    for k, v in (env or {}).items():
        monkeypatch.setenv(k, v)
    return CosyVoice3Provider(_settings(tmp_path, **kw))


def _ref_file(tmp_path: Path) -> Path:
    p = tmp_path / _REF_NAME
    p.write_bytes(b"fake wav")
    return p


def _collect(
    p: CosyVoice3Provider, text: str = "你好", voice: VoiceRef | None = None, emotion=None
) -> list[bytes]:
    """同一 loop 内 warmup + 收集全部 chunk（子进程 loop 亲和性）。"""

    async def run() -> list[bytes]:
        await p.warmup()
        try:
            return [
                c async for c in p.synth_stream(text, voice=voice or VoiceRef(), emotion=emotion)
            ]
        finally:
            p._terminate_sync()

    return asyncio.run(run())


def _worker_requests(tmp_path: Path) -> list[dict]:
    logp = tmp_path / "worker.log"
    if not logp.is_file():
        return []
    return [json.loads(line) for line in logp.read_text().splitlines() if line.strip()]


# ---------- capabilities / 部署定位 ----------


def test_capabilities() -> None:
    caps = CosyVoice3Provider(Settings()).capabilities()
    assert caps.streaming is True
    assert caps.languages == ("zh", "en", "ja", "ko", "yue")
    assert caps.clone is True
    assert caps.emotion is True


def test_warmup_stack_env_missing(tmp_path: Path) -> None:
    """未设 MOMO_COSYVOICE3_HOME/PYTHON → 明确部署 hint（readyz 拒就绪）。"""
    p = CosyVoice3Provider(Settings(models_dir=tmp_path))
    with pytest.raises(CosyVoice3Error, match="MOMO_COSYVOICE3_HOME"):
        asyncio.run(p.warmup())


def test_warmup_home_layout_bad(tmp_path: Path) -> None:
    """checkout 布局不对（缺 cosyvoice/cli / Matcha-TTS）→ 可执行 hint。"""
    p = CosyVoice3Provider(
        _settings(tmp_path, fake_home=tmp_path / "empty", cosyvoice3_python=sys.executable)
    )
    with pytest.raises(CosyVoice3Error, match="not a CosyVoice source checkout"):
        asyncio.run(p.warmup())
    home = _fake_home(tmp_path / "no_submodule_root")
    (home / "third_party" / "Matcha-TTS").rmdir()
    p = CosyVoice3Provider(_settings(tmp_path, fake_home=home))
    with pytest.raises(CosyVoice3Error, match="Matcha-TTS missing"):
        asyncio.run(p.warmup())


def test_warmup_python_missing(tmp_path: Path) -> None:
    p = CosyVoice3Provider(_settings(tmp_path, cosyvoice3_python=str(tmp_path / "no-such-python")))
    with pytest.raises(CosyVoice3Error, match="interpreter not found"):
        asyncio.run(p.warmup())


def test_warmup_model_files_missing(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """基座权重缺失 → 报错列出缺失文件并提示 pull。"""
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    (tmp_path / _DEST / "llm.pt").unlink()
    p = CosyVoice3Provider(_settings(tmp_path))
    with pytest.raises(CosyVoice3Error, match="missing.*llm.pt|pull --only"):
        asyncio.run(p.warmup())


def test_warmup_sha_mismatch(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path, bad_sha=True)
    p = CosyVoice3Provider(_settings(tmp_path))
    with pytest.raises(CosyVoice3Error, match="sha256 mismatch"):
        asyncio.run(p.warmup())


def test_warmup_manifest_absent(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import momo_models.manifest as manifest_mod

    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", tmp_path / "absent.toml")
    p = CosyVoice3Provider(_settings(tmp_path))
    with pytest.raises(CosyVoice3Error, match="MANIFEST.toml not found"):
        asyncio.run(p.warmup())


def test_warmup_happy_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """校验过 → 拉起 worker 并读到 ready 帧（含模型实际采样率）。"""
    p = _make_provider(tmp_path, monkeypatch)
    asyncio.run(p.warmup())
    try:
        assert p._proc is not None
        assert p._sample_rate == 48000  # fake worker 上报
    finally:
        p._terminate_sync()


def test_warmup_worker_load_error(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """worker 报 load error 帧 → warmup 抛错并杀进程。"""
    p = _make_provider(tmp_path, monkeypatch, env={"FAKE_WORKER_FAIL": "1"})
    with pytest.raises(CosyVoice3Error, match="load failed: fake load failure"):
        asyncio.run(p.warmup())


def test_warmup_worker_exit(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    p = _make_provider(tmp_path, monkeypatch, env={"FAKE_WORKER_EXIT": "1"})
    with pytest.raises(CosyVoice3Error, match="worker exited"):
        asyncio.run(p.warmup())


def test_warmup_sft_override(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """MOMO_TTS_MODEL_DIR → 跳过 MANIFEST，按必需文件存在性校验。"""
    ft = tmp_path / "tts" / "ft-cosyvoice3-20261101"
    ft.mkdir(parents=True)
    for rel in cosyvoice3._REQUIRED_MODEL_FILES:
        (ft / rel).write_bytes(b"ft")
    (ft / "CosyVoice-BlankEN").mkdir()
    import momo_models.manifest as manifest_mod

    monkeypatch.setattr(manifest_mod, "DEFAULT_MANIFEST", tmp_path / "absent.toml")
    monkeypatch.setattr(cosyvoice3, "_WORKER", _fake_worker(tmp_path))
    p = CosyVoice3Provider(_settings(tmp_path, tts_model_dir=str(ft)))
    asyncio.run(p.warmup())
    assert p._proc is not None
    p._terminate_sync()


def test_warmup_sft_missing_files(tmp_path: Path) -> None:
    ft = tmp_path / "ft-empty"
    ft.mkdir()
    p = CosyVoice3Provider(_settings(tmp_path, tts_model_dir=str(ft)))
    with pytest.raises(CosyVoice3Error, match="required model files missing"):
        asyncio.run(p.warmup())


def test_warmup_sft_lock_verify(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """SFT 目录有 models.lock 条目 → 按其 sha256 校验；不符报错。"""
    ft_rel = Path("tts") / "ft-cosyvoice3-20261101"
    ft = tmp_path / ft_rel
    ft.mkdir(parents=True)
    for rel in cosyvoice3._REQUIRED_MODEL_FILES:
        (ft / rel).write_bytes(b"ft " + rel.encode())
    (ft / "CosyVoice-BlankEN").mkdir()

    def sha(p: Path) -> str:
        return hashlib.sha256(p.read_bytes()).hexdigest()

    files = "".join(
        f'[[entry.files]]\npath = "{rel}"\nsize = {(ft / rel).stat().st_size}\n'
        f'sha256 = "{sha(ft / rel)}"\n'
        for rel in cosyvoice3._REQUIRED_MODEL_FILES
    )
    lock = tmp_path / "models.lock"
    entry = (
        f'[[entry]]\nrepo = "{ft_rel.as_posix()}@20261101"\n'
        f'name = "ft-cosyvoice3-20261101"\n{files}'
    )
    lock.write_text(entry, encoding="utf-8")
    monkeypatch.setattr(cosyvoice3, "_WORKER", _fake_worker(tmp_path))
    p = CosyVoice3Provider(_settings(tmp_path, tts_model_dir=str(ft)))
    asyncio.run(p.warmup())
    assert p._proc is not None
    p._terminate_sync()

    (ft / "llm.pt").write_bytes(b"tampered")
    p = CosyVoice3Provider(_settings(tmp_path, tts_model_dir=str(ft)))
    with pytest.raises(CosyVoice3Error, match="models.lock entry .* verify failed"):
        asyncio.run(p.warmup())


# ---------- synth_stream ----------


def test_synth_requires_warmup(tmp_path: Path) -> None:
    p = CosyVoice3Provider(_settings(tmp_path))

    async def run() -> list[bytes]:
        return [c async for c in p.synth_stream("x", voice=VoiceRef(), emotion=None)]

    with pytest.raises(CosyVoice3Error, match="not warmed up"):
        asyncio.run(run())


def test_synth_requires_ref_audio(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    p = _make_provider(tmp_path, monkeypatch)
    with pytest.raises(CosyVoice3Error, match="MOMO_TTS_VOICE_REF"):
        _collect(p)
    with pytest.raises(CosyVoice3Error, match="not found"):
        _collect(p, voice=VoiceRef(ref_audio=tmp_path / "nope.wav"))


def test_synth_zero_shot(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """zero_shot：prompt_text = 官方前缀 + ref_text；48k→24k 重采样成 2400B 帧。"""
    p = _make_provider(tmp_path, monkeypatch)
    ref = _ref_file(tmp_path)
    chunks = _collect(p, text="你好，世界", voice=VoiceRef(ref_audio=ref, ref_text="参考文本"))
    # 3 帧 × 2400 采样 @48k → 1200 采样 @24k → 2400B/帧
    assert chunks and all(len(c) == 2400 for c in chunks)
    req = _worker_requests(tmp_path)[0]
    assert req["mode"] == "zero_shot"
    assert req["text"] == "你好，世界"
    assert req["prompt_text"].endswith("参考文本")
    assert req["prompt_wav"].endswith("ref.wav")
    assert req["id"] == 1


def test_synth_instruct_skips_ref_text(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """emotion 非 neutral → instruct2：指令文本注入，不要求 ref_text。"""
    p = _make_provider(tmp_path, monkeypatch)
    ref = _ref_file(tmp_path)  # 无 sidecar、无 ref_text、ASR 也不可用
    chunks = _collect(p, voice=VoiceRef(ref_audio=ref), emotion="happy")
    assert chunks
    req = _worker_requests(tmp_path)[0]
    assert req["mode"] == "instruct2"
    assert "很开心地说" in req["instruct"]
    assert "prompt_text" not in req


def test_synth_ref_text_sidecar(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """缺 ref_text → 参考音同名 .txt sidecar 兜底。"""
    p = _make_provider(tmp_path, monkeypatch)
    ref = _ref_file(tmp_path)
    ref.with_suffix(".txt").write_text(" sidecar transcript \n", encoding="utf-8")
    _collect(p, voice=VoiceRef(ref_audio=ref))
    assert _worker_requests(tmp_path)[0]["prompt_text"].endswith("sidecar transcript")


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
            assert len(pcm) % 2 == 0
            return Transcript(text="auto transcript", lang="zh", duration_ms=100)

    monkeypatch.setattr("momo_models.providers.asr.sensevoice.SenseVoiceASRProvider", _FakeASR)
    p = _make_provider(tmp_path, monkeypatch)
    ref = _ref_file(tmp_path)
    _collect(p, voice=VoiceRef(ref_audio=ref))
    _collect(p, text="第二句", voice=VoiceRef(ref_audio=ref))
    reqs = _worker_requests(tmp_path)
    assert all(r["prompt_text"].endswith("auto transcript") for r in reqs)
    assert _FakeASR.calls == 1  # 缓存命中，不重复 ASR


def test_synth_ref_sample_rate_too_low(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """素材规范：参考音 <24kHz → 拒绝（voice-clone.md）。"""
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    monkeypatch.setattr(cosyvoice3, "_WORKER", _fake_worker(tmp_path))
    monkeypatch.setitem(sys.modules, "soundfile", _fake_soundfile(sr=16000))
    monkeypatch.setenv("FAKE_WORKER_LOG", str(tmp_path / "worker.log"))
    p = CosyVoice3Provider(_settings(tmp_path))
    with pytest.raises(CosyVoice3Error, match="sample rate 16000Hz"):
        _collect(p, voice=VoiceRef(ref_audio=_ref_file(tmp_path), ref_text="x"))


def test_cancel_stops_output(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """cancel 后在下一 chunk 边界停产出；worker 收到 cancel 消息。"""
    p = _make_provider(
        tmp_path,
        monkeypatch,
        env={
            "FAKE_WORKER_CHUNKS": "60",
            "FAKE_WORKER_FRAMES": "2400",
            "FAKE_WORKER_DELAY": "0.01",
        },
    )

    async def run() -> tuple[bytes, list[bytes]]:
        await p.warmup()
        try:
            it = p.synth_stream(
                "你好",
                voice=VoiceRef(ref_audio=_ref_file(tmp_path), ref_text="x"),
                emotion=None,
            )
            first = await it.__anext__()
            await p.cancel("r_1")
            rest = [c async for c in it]
            return first, rest
        finally:
            p._terminate_sync()

    first, rest = asyncio.run(run())
    assert first
    assert rest == []


def test_synth_after_cancel_uses_new_req_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """cancel 后再合成：req id 自增，worker 迟发旧帧按 req 丢弃不串流。"""
    p = _make_provider(
        tmp_path,
        monkeypatch,
        env={
            "FAKE_WORKER_CHUNKS": "50",
            "FAKE_WORKER_FRAMES": "2400",
            "FAKE_WORKER_DELAY": "0.01",
        },
    )

    async def run() -> int:
        await p.warmup()
        try:
            it = p.synth_stream(
                "一",
                voice=VoiceRef(ref_audio=_ref_file(tmp_path), ref_text="x"),
                emotion=None,
            )
            await it.__anext__()
            await p.cancel("r_1")
            async for _ in it:
                pass
            chunks = [
                c
                async for c in p.synth_stream(
                    "二",
                    voice=VoiceRef(ref_audio=_ref_file(tmp_path), ref_text="x"),
                    emotion=None,
                )
            ]
            return len(chunks)
        finally:
            p._terminate_sync()

    n = asyncio.run(run())
    assert n > 0
    reqs = _worker_requests(tmp_path)
    assert [r["id"] for r in reqs] == [1, 2]


def test_synth_worker_error_mid_stream(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """worker 中途报 error 帧 → CosyVoice3Error 冒泡（服务层上报 TTS_FAILED）。"""
    _make_model_dir(tmp_path)
    _patch_manifest(monkeypatch, tmp_path)
    worker = tmp_path / "err_worker.py"
    worker.write_text(
        "import json,sys\n"
        'def emit(o): sys.stdout.write(json.dumps(o)+"\\n"); sys.stdout.flush()\n'
        'emit({"type":"ready","sample_rate":24000})\n'
        "for line in sys.stdin:\n"
        "    req=json.loads(line)\n"
        '    emit({"type":"error","phase":"synth","message":"boom","req":req.get("id")})\n',
        encoding="utf-8",
    )
    monkeypatch.setattr(cosyvoice3, "_WORKER", worker)
    monkeypatch.setitem(sys.modules, "soundfile", _fake_soundfile())
    p = CosyVoice3Provider(_settings(tmp_path))
    with pytest.raises(CosyVoice3Error, match="boom"):
        _collect(p, voice=VoiceRef(ref_audio=_ref_file(tmp_path), ref_text="x"))
