"""Fun-CosyVoice3-0.5B 零样本克隆（首选目标）：instruct 控情感，双向流式。

官方推理栈不发 PyPI 且推荐 py3.10（pynini 在 py3.12/WSL 有坑，见
voice-clone.md 风险节），部署形态 = 源码 checkout（含 third_party/Matcha-TTS
子模块）+ 隔离 venv/conda 解释器。本 provider 因此走**子进程**接入：
`cosyvoice3_worker.py`（stdlib-only）由 `MOMO_COSYVOICE3_PYTHON` 指向的
解释器拉起，`MOMO_COSYVOICE3_HOME` 指向源码 checkout；父子间走
stdin/stdout 的「JSON 头行 + f32le 音频负载」协议。隔离收益：依赖不混入
本 venv（torch 红线 ADR-018 与官方栈各自锁版本），cancel/崩溃只影响
worker 进程。

warmup：`MOMO_TTS_MODEL_DIR` 非空时直接吃 SFT 产物目录（灰度切换/回滚；
存在对应 models.lock 条目则按其 sha256 校验，未登记只做必需文件存在性
检查并告警）；否则按 MANIFEST 条目 `cosyvoice3-0.5b` 对
`$MOMO_MODELS_DIR/FunAudioLLM/Fun-CosyVoice3-0.5B-2512/` 全量 sha256 校验，
不过 → readyz 拒就绪。校验通过后拉起 worker 等 `ready` 帧。

零样本：`VoiceRef.ref_audio`（服务层注入 MOMO_TTS_VOICE_REF）按官方
CosyVoice3 口径组装——zero_shot 的 prompt_text = 官方 prompt 前缀 +
参考音转写；emotion 非 neutral 时走 inference_instruct2，instruct 文本
来自 #33 映射层（providers/tts/emotion.py）。ref_text 解析顺序与 f5 一致：
请求携带 → 参考音同名 .txt sidecar → SenseVoice 兜底转写（asr extra +
sense-voice 权重就绪时）；instruct2 不需要转写（指令占据 prompt 槽位，
参考音仅供说话人 embedding/flow prompt）。素材规范见
docs/04-tracks/voice-clone.md（干净单人声、无 BGM、WAV ≥24kHz）。
"""

import asyncio
import atexit
import json
import tempfile
import time
import tomllib
from collections.abc import AsyncIterator
from pathlib import Path

import numpy as np

from momo_models.audio import (
    DOWNLINK_SAMPLE_RATE,
    UPLINK_SAMPLE_RATE,
    numpy_to_s16le_bytes,
    resample,
)
from momo_models.config import Settings
from momo_models.logging import get_logger
from momo_models.manifest import ManifestEntry, sha256_file, verify_entry
from momo_models.providers.base import TTSCaps, TTSProvider, VoiceRef
from momo_models.providers.tts.emotion import map_emotion

log = get_logger(__name__)

# MANIFEST 条目名（基座权重；SFT 产物走 MOMO_TTS_MODEL_DIR + models.lock）。
_ENTRY = "cosyvoice3-0.5b"
# SFT 产物目录运行必需件（布局 = 官方 model_dir；CosyVoice-BlankEN 为
# qwen_pretrain_path 覆写来源，AutoModel 必需）。
_REQUIRED_MODEL_FILES = (
    "cosyvoice3.yaml",
    "llm.pt",
    "flow.pt",
    "hift.pt",
    "campplus.onnx",
    "speech_tokenizer_v3.onnx",
)
_REQUIRED_MODEL_DIRS = ("CosyVoice-BlankEN",)

# 协议 §6：下行 50ms/帧（24kHz mono → 1200 samples → 2400B s16le）。
_CHUNK_BYTES = DOWNLINK_SAMPLE_RATE * 2 * 50 // 1000
# 参考音素材规范下限（voice-clone.md）：WAV/FLAC ≥24kHz。
_MIN_REF_SAMPLE_RATE = 24000
# worker 拉起 + AutoModel 加载上限（权重 ~4.3GB，冷启动分钟级）。
_READY_TIMEOUT_S = 600.0

# 官方 CosyVoice3 prompt 约定（example.py / HF model card）：
# zero_shot 的 prompt_text = 助手前缀 + 分隔符 + 参考音转写；
# instruct2 的 instruct_text = 助手前缀 + 指令 + 分隔符。
_EOP = "<" + "|endofprompt|>"  # 官方 prompt 分隔符
_PROMPT_PREFIX = "You are a helpful assistant." + _EOP
_INSTRUCT_PREFIX = "You are a helpful assistant. "

_STACK_HINT = (
    "cosyvoice3 runs the official source stack in an isolated venv/conda: "
    "set MOMO_COSYVOICE3_HOME to the FunAudioLLM/CosyVoice checkout (with "
    "third_party/Matcha-TTS submodule) and MOMO_COSYVOICE3_PYTHON to that "
    "env's python; see docs/04-tracks/voice-clone.md risk section"
)

_WORKER = Path(__file__).with_name("cosyvoice3_worker.py")


class CosyVoice3Error(RuntimeError):
    """模型文件缺失 / checksum 不符 / 推理栈未部署 / 参考音不合规 / 未 warmup 即合成。"""


def _decode_checked_ref(path: Path) -> tuple[np.ndarray, int]:
    """解码参考音并按素材规范预检，返回 (float32 mono, 原始采样率)。

    规范（voice-clone.md）：可读 WAV/FLAC、采样率 ≥24kHz、非空；
    单人声/无 BGM 属素材制作约束，只能人工保证，这里无法机器判定。
    """
    try:
        import soundfile as sf
    except ImportError as e:
        raise CosyVoice3Error(
            "soundfile missing: run `uv sync --extra tts` for the ref-audio precheck"
        ) from e
    try:
        data, sr = sf.read(str(path), dtype="float32", always_2d=True)
    except Exception as e:
        raise CosyVoice3Error(f"reference audio unreadable: {path}: {e}") from e
    if int(sr) < _MIN_REF_SAMPLE_RATE:
        raise CosyVoice3Error(
            f"reference audio sample rate {sr}Hz < {_MIN_REF_SAMPLE_RATE}Hz; "
            "provide a clean single-speaker WAV >=24kHz (docs/04-tracks/voice-clone.md)"
        )
    mono = data.mean(axis=1)
    if mono.size == 0:
        raise CosyVoice3Error(f"reference audio is empty: {path}")
    return mono.astype(np.float32), int(sr)


class CosyVoice3Provider(TTSProvider):
    """Fun-CosyVoice3-0.5B → 官方栈子进程 → PCM s16le 24kHz mono 50ms 帧。

    - warmup()：模型目录校验（MANIFEST sha256 或 SFT 目录存在性/lock 校验）
      → 拉起隔离 venv 中的 worker → 等 ready 帧（含模型实际采样率）。
    - synth_stream()：emotion 经 map_emotion → instruct；非空走
      inference_instruct2，否则走 inference_zero_shot（需 ref_text）。
      参考音统一重采样写 24k PCM16 临时 WAV 后传路径给 worker。
    - cancel()：置位在途流 + 发 cancel 消息；worker 在下一 chunk 边界停产出。
    """

    name = "cosyvoice3"

    def __init__(self, settings: Settings) -> None:
        self._settings = settings
        self._proc: asyncio.subprocess.Process | None = None
        self._sample_rate = DOWNLINK_SAMPLE_RATE
        self._asr = None  # SenseVoice 兜底转写（缺 ref_text 时懒加载）
        self._cancel_events: set[asyncio.Event] = set()
        # 参考音转写缓存：(path, mtime_ns) → ref_text，避免逐句重复 ASR
        self._ref_text_cache: dict[tuple[str, int], str] = {}
        # 单 worker 串行消费 synth 请求——并发请求会让音频帧归属错乱。
        self._synth_lock = asyncio.Lock()
        self._write_lock = asyncio.Lock()
        # synth 请求自增 id；worker 回显为帧 "req"，cancel 后迟发帧按此丢弃。
        self._req_seq = 0

    def capabilities(self) -> TTSCaps:
        return TTSCaps(
            streaming=True,
            languages=("zh", "en", "ja", "ko", "yue"),
            clone=True,
            emotion=True,
        )

    # ---------- 模型定位与校验 ----------

    def _manifest_entry(self, name: str) -> ManifestEntry:
        """按条目名解析 MANIFEST；清单缺失/条目未知抛 CosyVoice3Error。"""
        from momo_models.manifest import DEFAULT_MANIFEST, load_manifest

        if not DEFAULT_MANIFEST.is_file():
            raise CosyVoice3Error(f"MANIFEST.toml not found at {DEFAULT_MANIFEST}")
        entries = load_manifest(DEFAULT_MANIFEST)
        entry = next((e for e in entries if e.name == name), None)
        if entry is None:
            names = ", ".join(e.name for e in entries)
            raise CosyVoice3Error(f"MANIFEST has no entry {name!r}; entries: {names}")
        return entry

    def _required_files_missing(self, model_dir: Path) -> list[str]:
        missing = [f for f in _REQUIRED_MODEL_FILES if not (model_dir / f).is_file()]
        missing += [d + "/" for d in _REQUIRED_MODEL_DIRS if not (model_dir / d).is_dir()]
        return missing

    def _verify_lock_entry(self, model_dir: Path, models_dir: Path) -> bool:
        """models.lock 中解析到本目录的条目 → 按其 sha256 校验；返回是否找到条目。"""
        lock_path = models_dir / "models.lock"
        if not lock_path.is_file():
            return False
        try:
            raw = tomllib.loads(lock_path.read_text(encoding="utf-8"))
        except (OSError, tomllib.TOMLDecodeError) as e:
            log.warning("cosyvoice3.lock_unreadable", path=str(lock_path), error=str(e))
            return False
        dest_by_name: dict[str, str] = {}
        try:
            for e in self._iter_manifest_entries():
                dest_by_name[e.name] = e.dest
        except CosyVoice3Error:
            pass  # 清单不可读时退化为 repo_id 布局
        for item in raw.get("entry", []):
            name = str(item.get("name", ""))
            repo_id = str(item.get("repo", "")).split("@", 1)[0]
            dest = dest_by_name.get(name, repo_id)
            if not dest or (models_dir / dest).resolve() != model_dir.resolve():
                continue
            missing, mismatched = [], []
            for f in item.get("files", []):
                fp = model_dir / str(f.get("path", ""))
                if not fp.is_file():
                    missing.append(f.get("path"))
                    continue
                sha = f.get("sha256")
                if sha and sha256_file(fp) != str(sha).lower():
                    mismatched.append(f.get("path"))
            if missing or mismatched:
                raise CosyVoice3Error(
                    f"models.lock entry {name!r} verify failed: "
                    f"missing={missing} mismatched={mismatched}; re-register the artifact"
                )
            log.info("cosyvoice3.lock_verified", entry=name, dir=str(model_dir))
            return True
        return False

    def _iter_manifest_entries(self) -> list[ManifestEntry]:
        from momo_models.manifest import DEFAULT_MANIFEST, load_manifest

        if not DEFAULT_MANIFEST.is_file():
            raise CosyVoice3Error(f"MANIFEST.toml not found at {DEFAULT_MANIFEST}")
        return load_manifest(DEFAULT_MANIFEST)

    def _resolve_model_dir(self) -> Path:
        """模型目录：MOMO_TTS_MODEL_DIR 覆盖（SFT 产物）> MANIFEST 基座。"""
        models_dir = self._settings.models_dir.expanduser()
        override = self._settings.tts_model_dir.strip()
        if override:
            model_dir = Path(override).expanduser()
            missing = self._required_files_missing(model_dir)
            if missing:
                raise CosyVoice3Error(
                    f"MOMO_TTS_MODEL_DIR={model_dir}: required model files missing "
                    f"{missing}; point it at a complete cosyvoice3 model dir"
                )
            if not self._verify_lock_entry(model_dir, models_dir):
                log.warning(
                    "cosyvoice3.sft_unverified",
                    dir=str(model_dir),
                    hint="no models.lock entry resolves here; existence check only",
                )
            return model_dir
        entry = self._manifest_entry(_ENTRY)
        result = verify_entry(entry, models_dir)
        if result.missing:
            raise CosyVoice3Error(
                f"{entry.name}: model files missing {list(result.missing)}; "
                f"run `momoctl pull --only {entry.name}`"
            )
        if result.mismatched:
            raise CosyVoice3Error(
                f"{entry.name}: sha256 mismatch {list(result.mismatched)}; "
                "delete the files and re-pull"
            )
        return models_dir / entry.dest

    def _resolve_stack(self) -> tuple[str, Path]:
        """(worker 解释器, 源码 checkout)；未部署/布局不对 → 可执行 hint。"""
        home_raw = self._settings.cosyvoice3_home.strip()
        py_raw = self._settings.cosyvoice3_python.strip()
        if not home_raw or not py_raw:
            raise CosyVoice3Error(_STACK_HINT)
        home = Path(home_raw).expanduser()
        if not (home / "cosyvoice" / "cli" / "cosyvoice.py").is_file():
            raise CosyVoice3Error(
                f"MOMO_COSYVOICE3_HOME={home}: not a CosyVoice source checkout "
                "(cosyvoice/cli/cosyvoice.py missing); clone --recursive "
                "https://github.com/FunAudioLLM/CosyVoice"
            )
        if not (home / "third_party" / "Matcha-TTS").is_dir():
            raise CosyVoice3Error(
                f"{home}: third_party/Matcha-TTS missing; run "
                "`git submodule update --init --recursive` in the checkout"
            )
        python = Path(py_raw).expanduser()
        if not python.is_file():
            raise CosyVoice3Error(
                f"MOMO_COSYVOICE3_PYTHON={python}: interpreter not found; "
                "point it at the isolated cosyvoice venv/conda python"
            )
        return str(python), home

    # ---------- worker 进程与协议 ----------

    async def _send(self, msg: dict) -> None:
        proc = self._proc
        if proc is None or proc.stdin is None:
            raise CosyVoice3Error("cosyvoice3 worker not running")
        async with self._write_lock:
            proc.stdin.write(json.dumps(msg, ensure_ascii=False).encode() + b"\n")
            await proc.stdin.drain()

    async def _read_message(self) -> dict:
        """读一帧：JSON 头行；audio 帧附带读 n 字节负载并入 msg['payload']。"""
        proc = self._proc
        if proc is None or proc.stdout is None:
            raise CosyVoice3Error("cosyvoice3 worker not running")
        line = await proc.stdout.readline()
        if not line:
            rc = await proc.wait()
            raise CosyVoice3Error(f"cosyvoice3 worker exited (code {rc})")
        try:
            msg = json.loads(line)
        except json.JSONDecodeError as e:
            raise CosyVoice3Error(f"cosyvoice3 worker bad frame: {line[:120]!r}") from e
        if msg.get("type") == "audio":
            n = int(msg.get("n", 0))
            msg["payload"] = await proc.stdout.readexactly(n)
        return msg

    def _terminate_sync(self) -> None:
        """atexit / 同步上下文兜底：尽力停 worker。"""
        proc = self._proc
        if proc is not None and proc.returncode is None:
            proc.terminate()
        self._proc = None

    async def _shutdown_worker(self) -> None:
        proc = self._proc
        if proc is None:
            return
        try:
            await self._send({"type": "shutdown"})
            await asyncio.wait_for(proc.wait(), timeout=5.0)
        except Exception:
            pass
        if proc.returncode is None:
            proc.terminate()
        self._proc = None

    # ---------- TTSProvider 契约 ----------

    async def warmup(self) -> None:
        """模型目录校验 + worker 拉起就绪；readyz 以此为据。"""
        python, home = self._resolve_stack()
        model_dir = self._resolve_model_dir()
        env = None
        if self._settings.tts_device == "cpu":
            import os

            env = {**os.environ, "CUDA_VISIBLE_DEVICES": ""}
        t0 = time.perf_counter()
        try:
            self._proc = await asyncio.create_subprocess_exec(
                python,
                str(_WORKER),
                "--home",
                str(home),
                "--model-dir",
                str(model_dir),
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=None,  # 继承服务 stderr：官方栈的诊断日志直通服务日志
                env=env,
            )
            msg = await asyncio.wait_for(self._read_message(), timeout=_READY_TIMEOUT_S)
        except Exception:
            self._terminate_sync()
            raise
        if msg.get("type") == "error":
            self._terminate_sync()
            raise CosyVoice3Error(f"cosyvoice3 worker load failed: {msg.get('message')}")
        if msg.get("type") != "ready":
            self._terminate_sync()
            raise CosyVoice3Error(f"cosyvoice3 worker unexpected first frame: {msg}")
        self._sample_rate = int(msg.get("sample_rate", DOWNLINK_SAMPLE_RATE))
        atexit.register(self._terminate_sync)
        log.info(
            "cosyvoice3.warmup_done",
            model_dir=str(model_dir),
            home=str(home),
            sample_rate=self._sample_rate,
            ms=round((time.perf_counter() - t0) * 1000),
        )

    async def synth_stream(
        self, text: str, *, voice: VoiceRef, emotion: str | None
    ) -> AsyncIterator[bytes]:
        """单句零样本/instruct 合成 → PCM s16le 24kHz mono 50ms 帧回吐。"""
        text = text.strip()
        if not text:
            return
        if self._proc is None:
            raise CosyVoice3Error("cosyvoice3 provider not warmed up; wait for readyz")
        ref_audio = self._resolve_ref_audio(voice)
        params = map_emotion(emotion, self.name, self.capabilities())
        instruct = str(params.get("instruct") or "").strip()
        ref_mono, ref_sr = await asyncio.to_thread(_decode_checked_ref, ref_audio)

        if instruct:
            req = {
                "type": "synth",
                "mode": "instruct2",
                "text": text,
                "instruct": _INSTRUCT_PREFIX + instruct + _EOP,
            }
        else:
            ref_text = await self._resolve_ref_text(voice, ref_audio, ref_mono, ref_sr)
            req = {
                "type": "synth",
                "mode": "zero_shot",
                "text": text,
                "prompt_text": _PROMPT_PREFIX + ref_text,
            }

        cancel = asyncio.Event()
        self._cancel_events.add(cancel)
        self._req_seq += 1
        req_id = self._req_seq
        emitted = 0
        t0 = time.perf_counter()
        try:
            async with self._synth_lock:
                try:
                    with tempfile.TemporaryDirectory(prefix="momo-cv3-") as td:
                        ref_wav = Path(td) / "ref.wav"
                        await asyncio.to_thread(self._write_ref_wav, ref_wav, ref_mono, ref_sr)
                        if cancel.is_set():
                            return
                        await self._send({**req, "id": req_id, "prompt_wav": str(ref_wav)})
                        carry = bytearray()
                        while True:
                            msg = await self._read_message()
                            # 上一请求被 cancel 后 worker 迟发的帧按 req 丢弃。
                            if msg.get("req") is not None and msg["req"] != req_id:
                                continue
                            mtype = msg.get("type")
                            if mtype == "audio":
                                wav = np.frombuffer(msg["payload"], dtype="<f4")
                                if self._sample_rate != DOWNLINK_SAMPLE_RATE:
                                    wav = resample(wav, self._sample_rate, DOWNLINK_SAMPLE_RATE)
                                carry += numpy_to_s16le_bytes(wav)
                                while len(carry) >= _CHUNK_BYTES:
                                    if cancel.is_set():
                                        carry.clear()
                                        break
                                    chunk = bytes(carry[:_CHUNK_BYTES])
                                    del carry[:_CHUNK_BYTES]
                                    emitted += len(chunk) // 2
                                    yield chunk
                                if cancel.is_set():
                                    break
                                continue
                            if mtype == "eos":
                                break
                            if mtype == "error":
                                raise CosyVoice3Error(
                                    f"cosyvoice3 synth failed: {msg.get('message')}"
                                )
                        if not cancel.is_set() and carry:
                            emitted += len(carry) // 2
                            yield bytes(carry)
                finally:
                    if cancel.is_set():
                        # synth_lock 内发 cancel：保证先于下一 synth 请求到达
                        # worker（否则 cancel 会误杀下一请求）。
                        try:
                            await self._send({"type": "cancel"})
                        except CosyVoice3Error:
                            pass
            log.info(
                "cosyvoice3.synth_done",
                cancelled=cancel.is_set(),
                samples=emitted,
                ms=round((time.perf_counter() - t0) * 1000),
            )
        finally:
            self._cancel_events.discard(cancel)

    async def cancel(self, req_id: str) -> None:
        """尽力取消：置位全部在途流 + 通知 worker（P0 单回合在飞）。"""
        for ev in self._cancel_events:
            ev.set()

    # ---------- 参考音与转写 ----------

    def _resolve_ref_audio(self, voice: VoiceRef) -> Path:
        """参考音路径：请求 VoiceRef > MOMO_TTS_VOICE_REF；克隆必需，缺即拒。"""
        ref = voice.ref_audio
        if ref is None and self._settings.tts_voice_ref:
            ref = Path(self._settings.tts_voice_ref)
        if ref is None:
            raise CosyVoice3Error(
                "cosyvoice3 zero-shot clone requires a reference audio: "
                "set MOMO_TTS_VOICE_REF to a clean single-speaker WAV >=24kHz"
            )
        ref = Path(ref).expanduser()
        if not ref.is_file():
            raise CosyVoice3Error(f"reference audio not found: {ref}")
        return ref

    @staticmethod
    def _write_ref_wav(path: Path, mono: np.ndarray, sr: int) -> None:
        """参考音统一转写 24kHz mono PCM16 WAV（输入格式归一，采样率确定）。"""
        import soundfile as sf

        sf.write(
            str(path),
            resample(mono, sr, DOWNLINK_SAMPLE_RATE),
            DOWNLINK_SAMPLE_RATE,
            subtype="PCM_16",
        )

    async def _resolve_ref_text(
        self, voice: VoiceRef, ref_audio: Path, mono: np.ndarray, sr: int
    ) -> str:
        """ref_text 解析顺序：请求携带 → 同名 .txt sidecar → SenseVoice 兜底。"""
        if voice.ref_text.strip():
            return voice.ref_text.strip()
        sidecar = ref_audio.with_suffix(".txt")
        if sidecar.is_file():
            text = sidecar.read_text(encoding="utf-8").strip()
            if text:
                return text
        text = await self._transcribe_ref(ref_audio, mono, sr)
        if not text:
            raise CosyVoice3Error(
                "ref_text unavailable: provide a non-empty voice.ref_text or a "
                f"{sidecar.name} transcript alongside the reference audio, "
                "or make the SenseVoice fallback usable"
            )
        return text

    async def _transcribe_ref(self, ref_audio: Path, mono: np.ndarray, sr: int) -> str:
        """SenseVoice 兜底转写参考音（asr extra + sensevoice 权重就绪才可用）。

        参考音转 16kHz 上行口径送 ASR；结果按 (path, mtime) 缓存，避免逐句重转。
        """
        key = (str(ref_audio), ref_audio.stat().st_mtime_ns)
        if key in self._ref_text_cache:
            return self._ref_text_cache[key]
        asr = await self._sensevoice()
        pcm = numpy_to_s16le_bytes(resample(mono, sr, UPLINK_SAMPLE_RATE))
        t = await asr.transcribe(pcm, lang=None)
        text = t.text.strip()
        if text:
            self._ref_text_cache[key] = text
            log.info("cosyvoice3.ref_text_transcribed", ref=str(ref_audio), lang=t.lang)
        return text

    async def _sensevoice(self):
        """懒加载 SenseVoiceASRProvider；不可用抛 CosyVoice3Error 并给可执行 hint。"""
        if self._asr is None:
            from momo_models.providers.asr.sensevoice import SenseVoiceASRProvider

            provider = SenseVoiceASRProvider(self._settings)
            try:
                await provider.load()
            except Exception as e:
                raise CosyVoice3Error(
                    f"ref_text missing and SenseVoice fallback unavailable: {e}; "
                    "supply a <ref_audio>.txt transcript or run `uv sync --extra asr` "
                    "and pull the sensevoice model"
                ) from e
            self._asr = provider
        return self._asr
