"""silero-vad via sherpa-onnx VadModel（纯 CPU）：断句，不做识别。

权重 = MANIFEST `silero-vad` 条目（不走 silero-vad pip 包，避免拖入 torch）；
本类只做窗口对齐与翻转点回推，产出 speech_start/speech_end 与段 PCM。
"""

import asyncio
from collections.abc import Iterator
from pathlib import Path

import numpy as np

from momo_models.audio import UPLINK_SAMPLE_RATE, s16le_bytes_to_numpy
from momo_models.config import Settings
from momo_models.providers.base import SPEECH_END, SPEECH_START, VadEvent, VADProvider

# 与 models/MANIFEST.toml 中 silero-vad 条目的 dest/files 保持一致。
_MODEL_SUBDIR = Path("snakers4") / "silero-vad"
_MODEL_FILE = "silero_vad.onnx"
_MANIFEST_ENTRY = "silero-vad"

# >30s 段强制 speech_end（protocol §7 断句上限口径）；判停静音走 MOMO_VAD_SILENCE_MS。
_MAX_SPEECH_DURATION_S = 30.0
# 低于该时长的毛刺不成段（silero 官方默认 0.25s，此处取更灵敏的 0.1s；
# 误触发由 ASR 空转写兜底）。
_MIN_SPEECH_DURATION_S = 0.1


class SileroVadError(RuntimeError):
    """模型文件缺失 / checksum 不符 / 未 load 即调用 / 依赖缺失。"""


def _import_sherpa():
    """懒加载 sherpa_onnx（可选依赖，`uv sync --extra asr` 安装）。"""
    try:
        import sherpa_onnx
    except ImportError as e:  # pragma: no cover —— 无重依赖环境兜底
        raise SileroVadError(
            "missing sherpa-onnx dependency: run `uv sync --extra asr` under models/"
        ) from e
    return sherpa_onnx


class SileroVadProvider(VADProvider):
    """VadModel → VadEvent；speech_end 携带段 PCM。"""

    name = "silero"

    def __init__(self, settings: Settings) -> None:
        self._settings = settings
        self._vad = None
        self._window_size = 0
        self._min_speech_samples = 0
        self._min_silence_samples = 0
        # 流内状态（reset 清零）
        self._pending = np.zeros(0, dtype=np.float32)  # 不足一窗的尾样本
        self._fed_samples = 0  # 已喂入总样本数（流内时间基准）
        self._buf = bytearray()  # 未结段的原始 PCM（保留 speech_start 回退段）
        self._buf_start = 0  # _buf[0] 对应的流内样本下标
        self._in_speech = False
        self._segment_index = 0
        self._seg_start = 0  # 当前段估计起点（样本下标）

    # ---------- 模型定位与校验 ----------

    def _model_path(self) -> Path:
        p = self._settings.models_dir.expanduser() / _MODEL_SUBDIR / _MODEL_FILE
        if not p.is_file():
            raise SileroVadError(
                f"silero-vad model file missing: {p}; "
                "run `momoctl pull --only silero-vad` or download manually per MANIFEST"
            )
        return p

    def _verify_manifest(self) -> None:
        """按 MANIFEST.toml 校验 sha256；清单缺失时只做文件存在性检查。"""
        from momo_models.manifest import DEFAULT_MANIFEST, load_manifest, verify_entry

        if not DEFAULT_MANIFEST.is_file():
            return
        entry = next(
            (e for e in load_manifest(DEFAULT_MANIFEST) if e.name == _MANIFEST_ENTRY),
            None,
        )
        if entry is None:
            return
        result = verify_entry(entry, self._settings.models_dir.expanduser())
        if result.mismatched:
            raise SileroVadError(
                f"silero-vad model sha256 mismatch {list(result.mismatched)}; re-fetch model files"
            )

    def _build(self, model: Path):
        sherpa = _import_sherpa()
        cfg = sherpa.SileroVadModelConfig(
            model=str(model),
            threshold=0.5,
            min_silence_duration=self._settings.vad_silence_ms / 1000,
            min_speech_duration=_MIN_SPEECH_DURATION_S,
            max_speech_duration=_MAX_SPEECH_DURATION_S,
        )
        return sherpa.VadModel.create(
            sherpa.VadModelConfig(
                silero_vad=cfg,
                sample_rate=UPLINK_SAMPLE_RATE,
                num_threads=1,
                provider="cpu",
                debug=False,
            )
        )

    # ---------- VADProvider 契约 ----------

    async def load(self) -> None:
        """加载模型；readyz 以此为据。"""
        model = self._model_path()
        self._verify_manifest()
        self._vad = await asyncio.to_thread(self._build, model)
        self._window_size = int(self._vad.window_size())
        self._min_speech_samples = int(self._vad.min_speech_duration_samples())
        self._min_silence_samples = int(self._vad.min_silence_duration_samples())
        self.reset()

    def feed(self, pcm: bytes) -> Iterator[VadEvent]:
        """喂入一块 PCM s16le 16kHz mono，产出 0..n 个边界事件。"""
        if self._vad is None:
            raise SileroVadError("silero-vad not loaded: wait for readyz first")
        # int16 对齐防御：奇数字节尾截断。
        if len(pcm) % 2:
            pcm = pcm[:-1]
        if not pcm:
            return
        self._buf += pcm
        self._pending = np.concatenate([self._pending, s16le_bytes_to_numpy(pcm)])
        self._fed_samples += len(pcm) // 2
        ws = self._window_size
        while len(self._pending) >= ws:
            window = self._pending[:ws]
            self._pending = self._pending[ws:]
            event = self._step(window)
            if event is not None:
                yield event
        if not self._in_speech:
            # 静默期丢弃旧缓冲，只保留 speech_start 回推所需余量
            # （min_speech 迟滞 + 一窗对齐），避免长静音下 _buf 无界增长。
            keep_from = self._fed_samples - len(self._pending) - self._min_speech_samples - ws
            drop = max(0, (keep_from - self._buf_start) * 2)
            if drop:
                del self._buf[:drop]
                self._buf_start += drop // 2

    def flush(self) -> Iterator[VadEvent]:
        """冲刷：补静音凑满尾窗；仍在 speech 则强制 speech_end。"""
        if self._vad is None:
            raise SileroVadError("silero-vad not loaded: wait for readyz first")
        ws = self._window_size
        if len(self._pending):
            pad = np.zeros(ws - len(self._pending), dtype=np.float32)
            tail = np.concatenate([self._pending, pad])
            self._pending = np.zeros(0, dtype=np.float32)
            event = self._step(tail)
            if event is not None:
                yield event
        if self._in_speech:
            yield self._end_segment(self._fed_samples)

    def reset(self) -> None:
        """丢弃流内状态（新 capture 前调用）。"""
        if self._vad is not None:
            self._vad.reset()
        self._pending = np.zeros(0, dtype=np.float32)
        self._fed_samples = 0
        self._buf = bytearray()
        self._buf_start = 0
        self._in_speech = False
        self._segment_index = 0
        self._seg_start = 0

    # ---------- 窗口状态机 ----------

    def _step(self, window: np.ndarray) -> VadEvent | None:
        """处理一窗（window_size 样本）；边界翻转时产出事件。"""
        window_end = self._fed_samples - len(self._pending)
        speech = bool(self._vad.is_speech(window))
        if speech and not self._in_speech:
            self._in_speech = True
            self._segment_index += 1
            # 迟滞判定：真实起点 = 判定满足点回推 min_speech 段长。
            self._seg_start = max(0, window_end - self._min_speech_samples)
            return VadEvent(
                event=SPEECH_START,
                at_ms=self._ms(self._seg_start),
                segment_index=self._segment_index,
            )
        if not speech and self._in_speech:
            # 判停成立前已吞掉 min_silence 静音，剥回尾部。
            end = max(self._seg_start, window_end - self._min_silence_samples)
            return self._end_segment(end)
        return None

    def _end_segment(self, end_sample: int) -> VadEvent:
        """闭合当前段：截取 [seg_start, end) 的原始 PCM 并清空已结段缓冲。"""
        self._in_speech = False
        lo = max(0, (self._seg_start - self._buf_start) * 2)
        hi = max(lo, (end_sample - self._buf_start) * 2)
        segment = bytes(self._buf[lo:hi])
        del self._buf[:hi]
        self._buf_start = end_sample
        return VadEvent(
            event=SPEECH_END,
            at_ms=self._ms(end_sample),
            segment_index=self._segment_index,
            pcm=segment,
        )

    @staticmethod
    def _ms(sample: int) -> int:
        return round(sample / UPLINK_SAMPLE_RATE * 1000)
