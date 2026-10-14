#!/usr/bin/env python3
"""ASR RTF 复测：SenseVoice-Small 对一段 wav 解码计时（基线 CPU×4 RTF 0.040，research.md）。

用法：`uv sync --extra asr && uv run python eval/asr_rtf.py <16kHz mono wav>
[--runs N]`；权重走 MANIFEST `sense-voice-small-int8`（`momoctl pull`）。
"""

import argparse
import asyncio
import sys
import time
from pathlib import Path

from momo_models.audio import wav_unwrap
from momo_models.config import Settings
from momo_models.providers.asr.sensevoice import SenseVoiceASRProvider


def main() -> None:
    ap = argparse.ArgumentParser(description="SenseVoice ASR RTF re-measurement")
    ap.add_argument("wav", type=Path, help="16kHz mono s16le wav")
    ap.add_argument("--runs", type=int, default=3, help="decode repetitions, best-of taken")
    ap.add_argument("--lang", default=None, help="auto|zh|en|ja|ko|yue, default auto")
    args = ap.parse_args()

    pcm, rate, channels = wav_unwrap(args.wav.read_bytes())
    if rate != 16000 or channels != 1:
        raise SystemExit(f"expected 16kHz mono wav, got {rate}Hz/{channels}ch")

    provider = SenseVoiceASRProvider(Settings())
    asyncio.run(provider.load())
    # 预热：lang 非 auto 时首个 transcribe 含懒建 recognizer，不计入计时
    asyncio.run(provider.transcribe(pcm, lang=args.lang))

    best = None
    text = ""
    for i in range(max(1, args.runs)):
        t0 = time.perf_counter()
        t = asyncio.run(provider.transcribe(pcm, lang=args.lang))
        dt = time.perf_counter() - t0
        rtf = dt / (t.duration_ms / 1000)
        best = rtf if best is None else min(best, rtf)
        text = t.text
        print(
            f"run{i + 1}: audio={t.duration_ms}ms decode={dt * 1000:.0f}ms "
            f"RTF={rtf:.3f} lang={t.lang} emotion={t.speech_emotion} "
            f"events={t.events}"
        )
    print(f"best RTF={best:.3f} (baseline 0.040, laptop CPU x4)")
    print(f"text: {text}")
    sys.exit(0)


if __name__ == "__main__":
    main()
