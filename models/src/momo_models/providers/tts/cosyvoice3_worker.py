#!/usr/bin/env python3
"""CosyVoice3 推理 worker——在隔离 venv/conda 解释器下由 CosyVoice3Provider 拉起。

官方推理栈不发 PyPI 且推荐 py3.10（pynini 在 py3.12 有坑，见
docs/04-tracks/voice-clone.md 风险节），故部署形态为源码 checkout +
独立解释器；本文件只用 stdlib + CosyVoice 仓自身依赖，**不得 import
momo_models**（worker 环境里没装本包）。

启动参数：

    python cosyvoice3_worker.py --home <CosyVoice checkout> --model-dir <dir>

stdio 协议（父进程 ⇄ worker，版本随本文件走）：

- 请求：stdin 一行一个 JSON——
    {"type": "synth", "mode": "zero_shot",
     "text": ..., "prompt_text": ..., "prompt_wav": "/abs/ref.wav", "speed": 1.0}
    {"type": "synth", "mode": "instruct2",
     "text": ..., "instruct": ..., "prompt_wav": "/abs/ref.wav", "speed": 1.0}
    {"type": "cancel"}    —— 在途合成于下一 chunk 边界停（尽力取消）
    {"type": "shutdown"}  —— 或 stdin EOF，进程退出
- 响应：stdout 交替「JSON 头行 + 二进制负载」——
    {"type": "ready", "sample_rate": 24000}          （load 完成后首发）
    {"type": "audio", "n": <bytes>, "req": N} + n 字节 f32le mono
    {"type": "eos", "cancelled": bool, "req": N}     （一次 synth 收尾）
    {"type": "error", "phase": "load|synth|dispatch", "message": ..., "req": N}
  synth 期间的 audio/eos/error 帧回显请求的 "id" 字段为 "req"，父进程据此
  丢弃已被 cancel 的旧请求迟发帧。

诊断日志一律走 stderr（由服务进程继承），stdout 只过协议帧。
"""

import argparse
import json
import queue
import sys
import threading
import traceback
from pathlib import Path


def _emit(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def _emit_audio(payload: bytes, req: int | None) -> None:
    """音频帧：JSON 头行声明字节数与请求 id，随后 raw f32le mono 负载。"""
    _emit({"type": "audio", "n": len(payload), "req": req})
    sys.stdout.buffer.write(payload)
    sys.stdout.buffer.flush()


def _synth(model, req: dict, cancel: threading.Event) -> None:
    """跑一次官方推理并把每个 model_output 转 f32le 帧回吐。"""
    mode = req.get("mode")
    fn = {
        "zero_shot": model.inference_zero_shot,
        "instruct2": model.inference_instruct2,
    }.get(mode)
    if fn is None:
        _emit({"type": "error", "phase": "dispatch", "message": f"unknown mode {mode!r}"})
        return
    speed = float(req.get("speed", 1.0))
    req_id = req.get("id")
    try:
        if mode == "zero_shot":
            gen = fn(req["text"], req["prompt_text"], req["prompt_wav"], stream=True, speed=speed)
        else:
            gen = fn(req["text"], req["instruct"], req["prompt_wav"], stream=True, speed=speed)
        cancelled = False
        for out in gen:
            if cancel.is_set():
                cancelled = True
                break
            # model_output["tts_speech"]: torch tensor [1, T] float
            wav = out["tts_speech"].detach().float().cpu().numpy().reshape(-1)
            _emit_audio(wav.astype("<f4").tobytes(), req_id)
        _emit({"type": "eos", "cancelled": cancelled, "req": req_id})
    except Exception as e:
        _emit(
            {
                "type": "error",
                "phase": "synth",
                "message": f"{type(e).__name__}: {e}",
                "req": req_id,
            }
        )
        traceback.print_exc(file=sys.stderr)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", required=True, help="CosyVoice source checkout root")
    parser.add_argument("--model-dir", required=True, help="verified model directory")
    args = parser.parse_args()

    home = Path(args.home).resolve()
    # 官方用法（仓 README/example.py）：Matcha-TTS 子模块与仓根都上 sys.path。
    sys.path.insert(0, str(home / "third_party" / "Matcha-TTS"))
    sys.path.insert(0, str(home))

    try:
        from cosyvoice.cli.cosyvoice import AutoModel

        model = AutoModel(model_dir=str(Path(args.model_dir).resolve()))
    except Exception as e:
        _emit({"type": "error", "phase": "load", "message": f"{type(e).__name__}: {e}"})
        traceback.print_exc(file=sys.stderr)
        return 3
    _emit({"type": "ready", "sample_rate": int(model.sample_rate)})

    requests: queue.Queue[dict | None] = queue.Queue()
    cancel = threading.Event()

    def _stdin_reader() -> None:
        """独立线程消费 stdin：cancel 即时生效，其余进请求队列。"""
        for raw in sys.stdin:
            line = raw.strip()
            if not line:
                continue
            try:
                req = json.loads(line)
            except json.JSONDecodeError:
                _emit(
                    {
                        "type": "error",
                        "phase": "dispatch",
                        "message": "malformed request line",
                    }
                )
                continue
            rtype = req.get("type")
            if rtype == "cancel":
                cancel.set()
            elif rtype == "shutdown":
                requests.put(None)
                return
            else:
                requests.put(req)
        requests.put(None)  # stdin EOF → 父进程已走，收工

    threading.Thread(target=_stdin_reader, daemon=True).start()

    while True:
        req = requests.get()
        if req is None or req.get("type") == "shutdown":
            return 0
        if req.get("type") != "synth":
            _emit(
                {
                    "type": "error",
                    "phase": "dispatch",
                    "message": f"unknown request type {req.get('type')!r}",
                }
            )
            continue
        cancel.clear()
        _synth(model, req, cancel)


if __name__ == "__main__":
    raise SystemExit(main())
