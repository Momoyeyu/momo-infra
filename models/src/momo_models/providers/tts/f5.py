"""F5-TTS v1 零样本克隆（已实测）：RTF 0.535、VRAM 0.8GB @ 3070 Laptop。

接入：`f5-tts` pip 包（`tts` extra；torch 走 torch-mac / torch-cu124 extra）。
权重走 MANIFEST 双条目 sha256 校验：`f5-tts-v1`（$MOMO_MODELS_DIR/SWivid/F5-TTS/
F5TTS_v1_Base/ 下 model_1250000.safetensors + vocab.txt）+ `vocos-mel-24khz`
（声码器，charactr/vocos-mel-24khz 仓的 config.yaml + pytorch_model.bin），
校验不过 → warmup 抛错，readyz 拒绝就绪；运行时不在线拉权重（model-fetch.md）。

零样本参考音：VoiceRef.ref_audio（服务层注入 MOMO_TTS_VOICE_REF）+ ref_text
转写文本（f5 要求成对给出）。ref_text 解析顺序：请求携带 → 参考音同名 .txt
sidecar → SenseVoice 兜底转写（asr extra + sense-voice 权重就绪时）；
三途皆空则报错，绝不回退到 f5 内置 whisper（那会在线拉 1.6GB 权重）。
素材规范见 docs/04-tracks/voice-clone.md（干净单人声、无 BGM、WAV ≥24kHz）。

caps.streaming=False：f5 infer 为单次整段调用（内部按句分块并发），完成后
按协议 §6 口径切片回吐 PCM s16le 24kHz mono 50ms/帧；emotion 忽略。
"""

import asyncio
import tempfile
import time
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
from momo_models.manifest import ManifestEntry, verify_entry
from momo_models.providers.base import TTSCaps, TTSProvider, VoiceRef

log = get_logger(__name__)

# MANIFEST 条目名：f5 本体与 vocos 声码器分属不同 HF 仓，
# pull 按条目 repo_id 取文件，故必须两条目分别登记校验。
_F5_ENTRY = "f5-tts-v1"
_VOCOS_ENTRY = "vocos-mel-24khz"
_F5_SUBDIR = Path("F5TTS_v1_Base")
_MODEL_FILE = _F5_SUBDIR / "model_1250000.safetensors"
_VOCAB_FILE = _F5_SUBDIR / "vocab.txt"

# 协议 §6：下行 50ms/帧（24kHz mono → 1200 samples → 2400B s16le）。
_CHUNK_BYTES = DOWNLINK_SAMPLE_RATE * 2 * 50 // 1000
# 参考音素材规范下限（voice-clone.md）：WAV/FLAC ≥24kHz。
_MIN_REF_SAMPLE_RATE = 24000

_DEPS_HINT = (
    "f5 provider deps missing: run `uv sync --extra torch-mac --extra tts` "
    "(macOS) or `uv sync --extra torch-cu124 --extra tts` (linux+CUDA)"
)


class F5Error(RuntimeError):
    """模型文件缺失 / checksum 不符 / 依赖缺失 / 参考音不合规 / 未 warmup 即合成。"""


def _import_f5tts():
    """懒加载 f5_tts + torch（重依赖，`tts` extra 提供）。"""
    try:
        import torch
        from f5_tts.api import F5TTS
    except ImportError as e:
        raise F5Error(_DEPS_HINT) from e
    return F5TTS, torch


def _decode_checked_ref(path: Path) -> tuple[np.ndarray, int]:
    """解码参考音并按素材规范预检，返回 (float32 mono, 原始采样率)。

    规范（voice-clone.md）：可读 WAV/FLAC、采样率 ≥24kHz、非空；
    单人声/无 BGM 属素材制作约束，只能人工保证，这里无法机器判定。
    """
    try:
        import soundfile as sf
    except ImportError as e:
        raise F5Error(_DEPS_HINT) from e
    try:
        data, sr = sf.read(str(path), dtype="float32", always_2d=True)
    except Exception as e:
        raise F5Error(f"reference audio unreadable: {path}: {e}") from e
    if int(sr) < _MIN_REF_SAMPLE_RATE:
        raise F5Error(
            f"reference audio sample rate {sr}Hz < {_MIN_REF_SAMPLE_RATE}Hz; "
            "provide a clean single-speaker WAV >=24kHz (docs/04-tracks/voice-clone.md)"
        )
    mono = data.mean(axis=1)
    if mono.size == 0:
        raise F5Error(f"reference audio is empty: {path}")
    return mono.astype(np.float32), int(sr)


def _show_info(*args) -> None:
    """f5 infer 的 show_info 回调 → debug 日志（默认 print 会污染服务日志）。"""
    log.debug("f5.infer", msg=" ".join(str(a) for a in args))


class F5TTSProvider(TTSProvider):
    """F5-TTS v1 零样本克隆 → 整段合成后按 50ms 帧回吐 PCM s16le 24kHz mono。

    - warmup()：MANIFEST `f5-tts-v1` + `vocos-mel-24khz` 双条目 sha256 校验后
      懒加载 F5TTS（vocos 经 vocoder_local_path 指向本地已校验产物）。
    - synth_stream()：ref_audio + ref_text 成对传入做零样本克隆；合成在
      worker 线程整段完成（streaming=False），产出按协议帧切片回吐。
    - cancel()：尽力取消——置位在途流，infer 完成与逐 chunk 两个边界停产出
      （torch 推理本身不可强杀，服务层 task.cancel 即时断开产出流）。
    """

    name = "f5"

    def __init__(self, settings: Settings) -> None:
        self._settings = settings
        self._f5 = None
        self._asr = None  # SenseVoice 兜底转写（缺 ref_text 时懒加载）
        self._cancel_events: set[asyncio.Event] = set()
        # 参考音转写缓存：(path, mtime_ns) → ref_text，避免逐句重复 ASR
        self._ref_text_cache: dict[tuple[str, int], str] = {}

    def capabilities(self) -> TTSCaps:
        return TTSCaps(
            streaming=False,
            languages=("zh", "en"),
            clone=True,
            emotion=False,
        )

    # ---------- 模型定位与校验 ----------

    def _manifest_entry(self, name: str) -> ManifestEntry:
        """按条目名解析 MANIFEST；清单缺失/条目未知抛 F5Error。"""
        from momo_models.manifest import DEFAULT_MANIFEST, load_manifest

        if not DEFAULT_MANIFEST.is_file():
            raise F5Error(f"MANIFEST.toml not found at {DEFAULT_MANIFEST}")
        entries = load_manifest(DEFAULT_MANIFEST)
        entry = next((e for e in entries if e.name == name), None)
        if entry is None:
            names = ", ".join(e.name for e in entries)
            raise F5Error(f"MANIFEST has no entry {name!r}; entries: {names}")
        return entry

    def _verify_files(self, models_dir: Path) -> tuple[Path, Path, Path]:
        """双条目 sha256 校验；返回 (ckpt, vocab, vocos_dir)，不过抛 F5Error。"""
        f5_entry = self._manifest_entry(_F5_ENTRY)
        vocos_entry = self._manifest_entry(_VOCOS_ENTRY)
        for entry in (f5_entry, vocos_entry):
            result = verify_entry(entry, models_dir)
            if result.missing:
                raise F5Error(
                    f"{entry.name}: model files missing {list(result.missing)}; "
                    f"run `momoctl pull --only {entry.name}`"
                )
            if result.mismatched:
                raise F5Error(
                    f"{entry.name}: sha256 mismatch {list(result.mismatched)}; "
                    "delete the files and re-pull"
                )
        f5_root = models_dir / f5_entry.dest
        return (
            f5_root / _MODEL_FILE,
            f5_root / _VOCAB_FILE,
            models_dir / vocos_entry.dest,
        )

    def _resolve_device(self, torch) -> str:
        """MOMO_TTS_DEVICE → f5 device 字符串；请求不可用设备直接拒就绪。"""
        dev = self._settings.tts_device
        if dev == "cuda" and not torch.cuda.is_available():
            raise F5Error(
                "MOMO_TTS_DEVICE=cuda but CUDA is unavailable; "
                "set MOMO_TTS_DEVICE=cpu|mps or install a CUDA-enabled torch"
            )
        if dev == "mps" and not torch.backends.mps.is_available():
            raise F5Error("MOMO_TTS_DEVICE=mps but MPS is unavailable")
        return dev

    # ---------- TTSProvider 契约 ----------

    async def warmup(self) -> None:
        """双条目 sha256 校验 + F5TTS 加载（含 vocos）；readyz 以此为据。"""
        models_dir = self._settings.models_dir.expanduser()
        ckpt, vocab, vocos_dir = self._verify_files(models_dir)
        F5TTS, torch = _import_f5tts()
        device = self._resolve_device(torch)
        t0 = time.perf_counter()
        self._f5 = await asyncio.to_thread(
            F5TTS,
            model="F5TTS_v1_Base",
            ckpt_file=str(ckpt),
            vocab_file=str(vocab),
            vocoder_local_path=str(vocos_dir),
            device=device,
        )
        log.info(
            "f5.warmup_done",
            ckpt=str(ckpt),
            vocos=str(vocos_dir),
            device=device,
            ms=round((time.perf_counter() - t0) * 1000),
        )

    async def synth_stream(
        self, text: str, *, voice: VoiceRef, emotion: str | None
    ) -> AsyncIterator[bytes]:
        """整段零样本合成 → PCM s16le 24kHz mono 50ms 帧回吐。

        f5 infer 无中途产出钩子；cancel 在 infer 返回后与逐 chunk 两个边界生效。
        emotion 忽略（caps.emotion=False）。
        """
        text = text.strip()
        if not text:
            return
        if self._f5 is None:
            raise F5Error("f5 provider not warmed up; wait for readyz")
        ref_audio = self._resolve_ref_audio(voice)
        # 解码 + 素材规范预检在 worker 线程执行（解码属 IO/CPU 重活）
        ref_mono, ref_sr = await asyncio.to_thread(_decode_checked_ref, ref_audio)
        ref_text = await self._resolve_ref_text(voice, ref_audio, ref_mono, ref_sr)

        cancel = asyncio.Event()
        self._cancel_events.add(cancel)
        emitted = 0
        t0 = time.perf_counter()
        try:
            wav, sr = await asyncio.to_thread(
                self._synthesize, text, ref_mono, ref_sr, ref_text, cancel
            )
            if cancel.is_set() or wav is None:
                return
            wav = np.asarray(wav, dtype=np.float32).reshape(-1)
            if sr != DOWNLINK_SAMPLE_RATE:
                wav = resample(wav, sr, DOWNLINK_SAMPLE_RATE)
            pcm = numpy_to_s16le_bytes(wav)
            for off in range(0, len(pcm), _CHUNK_BYTES):
                if cancel.is_set():
                    break
                chunk = pcm[off : off + _CHUNK_BYTES]
                emitted += len(chunk) // 2
                yield chunk
            log.info(
                "f5.synth_done",
                cancelled=cancel.is_set(),
                samples=emitted,
                ms=round((time.perf_counter() - t0) * 1000),
            )
        finally:
            self._cancel_events.discard(cancel)

    async def cancel(self, req_id: str) -> None:
        """尽力取消：置位全部在途流（P0 单回合在飞）。下一 chunk 边界停产出。"""
        for ev in self._cancel_events:
            ev.set()

    # ---------- 参考音与转写 ----------

    def _resolve_ref_audio(self, voice: VoiceRef) -> Path:
        """参考音路径：请求 VoiceRef > MOMO_TTS_VOICE_REF；克隆必需，缺即拒。"""
        ref = voice.ref_audio
        if ref is None and self._settings.tts_voice_ref:
            ref = Path(self._settings.tts_voice_ref)
        if ref is None:
            raise F5Error(
                "f5 zero-shot clone requires a reference audio: "
                "set MOMO_TTS_VOICE_REF to a clean single-speaker WAV >=24kHz"
            )
        ref = Path(ref).expanduser()
        if not ref.is_file():
            raise F5Error(f"reference audio not found: {ref}")
        return ref

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
            raise F5Error(
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
            log.info("f5.ref_text_transcribed", ref=str(ref_audio), lang=t.lang)
        return text

    async def _sensevoice(self):
        """懒加载 SenseVoiceASRProvider；不可用抛 F5Error 并给可执行 hint。"""
        if self._asr is None:
            from momo_models.providers.asr.sensevoice import SenseVoiceASRProvider

            provider = SenseVoiceASRProvider(self._settings)
            try:
                await provider.load()
            except Exception as e:
                raise F5Error(
                    f"ref_text missing and SenseVoice fallback unavailable: {e}; "
                    "supply a <ref_audio>.txt transcript or run `uv sync --extra asr` "
                    "and pull the sensevoice model"
                ) from e
            self._asr = provider
        return self._asr

    def _synthesize(
        self,
        text: str,
        mono: np.ndarray,
        sr: int,
        ref_text: str,
        cancel: asyncio.Event,
    ) -> tuple[np.ndarray | None, int]:
        """worker 线程内整段合成；返回 (float32 波形, 采样率)，wav 可为 None。

        参考音统一转写 24kHz mono PCM16 临时 WAV 再喂 f5：使 WAV/FLAC/MP3
        输入行为一致（f5 内部 pydub 对非 WAV 依赖 ffmpeg），且输入采样率确定。
        """
        import soundfile as sf

        with tempfile.TemporaryDirectory(prefix="momo-f5-") as td:
            ref_wav = Path(td) / "ref.wav"
            sf.write(
                str(ref_wav),
                resample(mono, sr, DOWNLINK_SAMPLE_RATE),
                DOWNLINK_SAMPLE_RATE,
                subtype="PCM_16",
            )
            if cancel.is_set():
                return None, DOWNLINK_SAMPLE_RATE
            wav, out_sr, _ = self._f5.infer(
                ref_file=str(ref_wav),
                ref_text=ref_text,
                gen_text=text,
                show_info=_show_info,
                progress=None,
            )
        return wav, out_sr
