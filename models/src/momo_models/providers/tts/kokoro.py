"""Kokoro-82M（已实测兜底）：首包 ~209ms、RTF 0.023、VRAM 0.76GB @ 3070 Laptop。

接入：`kokoro` pip 包 + torch（版本须与 torchaudio 对齐，
见 docs/00-context/decisions.md「ADR-018」）。
g2p 依赖 espeak-ng/misaki，按语言懒加载（en=espeak 兜底，zh=misaki[zh]，ja=misaki[ja]）。

模型与音色走 MANIFEST `kokoro-82m` 条目：$MOMO_MODELS_DIR/hexgrad/Kokoro-82M/
下 `kokoro-v1_0.pth`/`config.json`/`voices/*.pt`，sha256 不过 → readyz 拒就绪
（对齐 sensevoice.py 模式；运行时只读本地产物，不在合成时在线下载）。
KModel 直接吃本地 config/权重路径绕开 hf_hub_download；voice 传本地 .pt 路径
（KPipeline 对 .pt 结尾走 torch.load）。产出即 24kHz float32 → 契约口径
PCM s16le 24kHz mono。情感经 providers/tts/emotion.py 映射为 speed。
"""

import asyncio
import time
from collections.abc import AsyncIterator
from pathlib import Path

from momo_models.audio import DOWNLINK_SAMPLE_RATE, numpy_to_s16le_bytes, resample
from momo_models.config import Settings
from momo_models.logging import get_logger
from momo_models.manifest import ManifestEntry
from momo_models.providers.base import TTSCaps, TTSProvider, VoiceRef
from momo_models.providers.tts.emotion import map_emotion

log = get_logger(__name__)

_ENTRY_NAME = "kokoro-82m"
_REPO_ID = "hexgrad/Kokoro-82M"

# 合成输出固定 24kHz（kokoro 口径），与契约下行采样率一致。
_KOKORO_SAMPLE_RATE = 24000

# VoiceRef.lang/MOMO_TTS_LANG → 默认音色；MOMO_TTS_VOICE 可覆盖为 voices/ 下任意音色。
_DEFAULT_VOICES = {
    "zh": "zf_xiaoxiao",
    "en": "af_heart",
    "ja": "jf_alpha",
}
_FALLBACK_VOICE = _DEFAULT_VOICES["zh"]

# kokoro pipeline lang_code；音色名首字母即语言字母（af_→a、zf_→z、jf_→j）。
_KOKORO_LANG_CODES = frozenset("abefhijpz")
_LANG_TO_CODE = {"zh": "z", "en": "a", "ja": "j"}
_FALLBACK_CODE = "z"

_DEPS_HINT = (
    "kokoro provider 依赖未安装：`uv sync --extra torch-mac --extra tts`（CUDA 机用 torch-cu124）"
)


class KokoroError(RuntimeError):
    """模型文件缺失 / checksum 不符 / 依赖缺失 / 未 warmup 即合成。"""


def _import_kokoro():
    """懒加载 kokoro + torch（`tts` + torch 平台档 extra 安装）。"""
    try:
        import torch
        from kokoro import KModel, KPipeline
    except ImportError as e:  # pragma: no cover —— 无重依赖环境兜底
        raise KokoroError(_DEPS_HINT) from e
    return torch, KModel, KPipeline


def _next_result(it):
    """同步生成器 next() 的线程安全封装：StopIteration → None。"""
    try:
        return next(it)
    except StopIteration:
        return None


def _result_audio(result):
    """KPipeline.Result（.audio）或旧版 (gs, ps, audio) 三元组 → float tensor。"""
    audio = getattr(result, "audio", None)
    if audio is None and isinstance(result, tuple) and len(result) >= 3:
        audio = result[2]
    return audio


def _resolve_voice(settings: Settings, voice: VoiceRef) -> str:
    if settings.tts_voice:
        return settings.tts_voice
    lang = voice.lang if voice.lang != "auto" else settings.tts_lang
    return _DEFAULT_VOICES.get(lang, _FALLBACK_VOICE)


def _resolve_lang_code(settings: Settings, voice: VoiceRef, voice_name: str) -> str:
    """pipeline lang_code：音色名首字母优先（官方要求 lang_code 匹配音色），
    否则按请求/配置语言映射，zh 兜底。"""
    if voice_name and voice_name[0].lower() in _KOKORO_LANG_CODES:
        return voice_name[0].lower()
    lang = voice.lang if voice.lang != "auto" else settings.tts_lang
    return _LANG_TO_CODE.get(lang, _FALLBACK_CODE)


class KokoroTTSProvider(TTSProvider):
    """Kokoro-82M → PCM s16le 24kHz mono。

    - warmup()：MANIFEST `kokoro-82m` 条目定位 + sha256 校验 → 懒 import
      kokoro/torch → 本地路径建 KModel → 按 MOMO_TTS_DEVICE 落 cuda/cpu/mps。
    - synth_stream()：KPipeline 逐块生成，同步生成器放 worker 线程消费，
      chunk 边界查 cancel（≤200ms 停产出）；emotion 经 map_emotion → speed。
    - KPipeline 每语言一实例懒建（g2p 依赖按语言各异），共享同一 KModel。
    """

    name = "kokoro"

    def __init__(self, settings: Settings) -> None:
        self._settings = settings
        self._cancel_events: set[asyncio.Event] = set()
        self._model = None
        self._model_dir: Path | None = None
        self._pipelines: dict[str, object] = {}

    # ---------- 模型定位与校验（对齐 sensevoice.py 模式） ----------

    def _entry(self) -> ManifestEntry:
        """解析 MANIFEST `kokoro-82m` 条目；清单缺失/无条目抛 KokoroError。"""
        from momo_models.manifest import DEFAULT_MANIFEST, load_manifest

        if not DEFAULT_MANIFEST.is_file():
            raise KokoroError(f"MANIFEST.toml not found at {DEFAULT_MANIFEST}")
        entries = load_manifest(DEFAULT_MANIFEST)
        entry = next((e for e in entries if e.name == _ENTRY_NAME), None)
        if entry is None:
            names = ", ".join(e.name for e in entries)
            raise KokoroError(f"MANIFEST 缺少 {_ENTRY_NAME!r} 条目；现有: {names}")
        return entry

    def _resolve_files(self, entry: ManifestEntry) -> tuple[Path, Path]:
        """定位 config.json 与 kokoro-v1_0.pth；缺失抛 KokoroError。

        独立于 manifest files[] 的存在性检查（对齐 sensevoice._resolve_files），
        防止条目漏登文件时带病就绪。
        """
        assert self._model_dir is not None
        config = self._model_dir / "config.json"
        weights = self._model_dir / "kokoro-v1_0.pth"
        missing = [p.name for p in (config, weights) if not p.is_file()]
        if missing:
            raise KokoroError(
                f"kokoro 模型文件缺失 {missing}（期望目录 {self._model_dir}）；"
                f"请先 `momoctl pull --only {entry.name}` 或按 MANIFEST 手动下载"
            )
        return config, weights

    def _verify_entry(self, entry: ManifestEntry) -> None:
        from momo_models.manifest import verify_entry

        result = verify_entry(entry, self._settings.models_dir.expanduser())
        if result.missing:
            root = self._settings.models_dir.expanduser() / entry.dest
            raise KokoroError(
                f"kokoro 模型文件缺失 {list(result.missing)}（期望目录 {root}）；"
                f"请先 `momoctl pull --only {entry.name}` 或按 MANIFEST 手动下载"
            )
        if result.mismatched:
            raise KokoroError(
                f"kokoro 模型 sha256 校验失败 {list(result.mismatched)}；请重新拉取模型文件"
            )

    def _resolve_device(self, torch) -> str:
        """`MOMO_TTS_DEVICE` → torch 设备；请求设备不可用即报错（不就绪）。"""
        dev = self._settings.tts_device
        if dev == "cuda":
            if not torch.cuda.is_available():
                raise KokoroError("MOMO_TTS_DEVICE=cuda 但本机无 CUDA；改 MOMO_TTS_DEVICE=cpu|mps")
            return "cuda"
        if dev == "mps":
            if not torch.backends.mps.is_available():
                raise KokoroError("MOMO_TTS_DEVICE=mps 但本机无 MPS；改 MOMO_TTS_DEVICE=cpu")
            return "mps"
        return "cpu"

    def _build_model(self, torch, KModel, device: str):
        """本地 config+权重路径建 KModel（绕开 hf_hub_download）。"""
        assert self._model_dir is not None
        model = KModel(
            repo_id=_REPO_ID,
            config=str(self._model_dir / "config.json"),
            model=str(self._model_dir / "kokoro-v1_0.pth"),
        )
        return model.to(device).eval()

    async def _pipeline(self, lang_code: str):
        """按语言懒建 KPipeline（g2p 依赖各异：z=misaki[zh]、j=misaki[ja]）。"""
        pipe = self._pipelines.get(lang_code)
        if pipe is None:

            def _build():
                _, _, KPipeline = _import_kokoro()
                return KPipeline(lang_code=lang_code, repo_id=_REPO_ID, model=self._model)

            try:
                pipe = await asyncio.to_thread(_build)
            except ImportError as e:
                raise KokoroError(
                    f"kokoro lang_code={lang_code!r} 的 g2p 依赖缺失：{e} "
                    f"（zh 需 `pip install misaki[zh]`，ja 需 misaki[ja]，en 需 espeak-ng）"
                ) from e
            self._pipelines[lang_code] = pipe
        return pipe

    def _voice_path(self, voice_name: str) -> Path:
        """音色名 → voices/<name>.pt 本地路径；缺失抛 KokoroError（不运行时下载）。"""
        assert self._model_dir is not None
        filename = voice_name if voice_name.endswith(".pt") else f"{voice_name}.pt"
        path = self._model_dir / "voices" / filename
        if not path.is_file():
            raise KokoroError(
                f"kokoro 音色文件缺失 {path}；请在 MANIFEST {_ENTRY_NAME!r} "
                f"条目补登 voices/{filename} 后 `momoctl pull --only {_ENTRY_NAME}`"
            )
        return path

    # ---------- TTSProvider 契约 ----------

    def capabilities(self) -> TTSCaps:
        return TTSCaps(
            streaming=True,
            languages=("zh", "en", "ja"),
            clone=False,
            emotion=True,  # map_emotion → speed（#33 消费侧）
        )

    async def warmup(self) -> None:
        """校验 MANIFEST+sha256 → 建 KModel 落设备；readyz 以此为据。"""
        entry = self._entry()
        self._model_dir = self._settings.models_dir.expanduser() / entry.dest
        self._resolve_files(entry)
        self._verify_entry(entry)
        torch, KModel, _ = _import_kokoro()
        device = self._resolve_device(torch)
        self._model = await asyncio.to_thread(self._build_model, torch, KModel, device)
        log.info("kokoro.warmup", device=device, model_dir=str(self._model_dir))

    async def synth_stream(
        self, text: str, *, voice: VoiceRef, emotion: str | None
    ) -> AsyncIterator[bytes]:
        """KPipeline 逐块 → PCM s16le 24kHz mono；emotion → speed。"""
        text = text.strip()
        if not text:
            return
        if self._model is None or self._model_dir is None:
            raise KokoroError("kokoro 未 warmup：先等待 readyz 就绪")
        params = map_emotion(emotion, self.name, self.capabilities())
        speed = float(params.get("speed", 1.0))
        voice_name = _resolve_voice(self._settings, voice)
        lang_code = _resolve_lang_code(self._settings, voice, voice_name)
        pipeline = await self._pipeline(lang_code)
        voice_path = self._voice_path(voice_name)

        cancel = asyncio.Event()
        self._cancel_events.add(cancel)
        emitted = 0  # 已产出 samples 数
        t0 = time.perf_counter()
        try:
            it = pipeline(text, voice=str(voice_path), speed=speed)
            while not cancel.is_set():
                result = await asyncio.to_thread(_next_result, it)
                if result is None:
                    break
                audio = _result_audio(result)
                if audio is None:
                    continue
                samples = audio.detach().cpu().numpy() if hasattr(audio, "detach") else audio
                pcm = numpy_to_s16le_bytes(
                    resample(samples, _KOKORO_SAMPLE_RATE, DOWNLINK_SAMPLE_RATE)
                )
                if not pcm:
                    continue
                if not emitted:
                    log.info(
                        "kokoro.first_audio",
                        ms=round((time.perf_counter() - t0) * 1000),
                    )
                emitted += len(pcm) // 2
                yield pcm
            log.info(
                "kokoro.synth_done",
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
