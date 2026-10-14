"""momo-svc-tts：POST /v1/audio/speech（对外 OpenAI 对齐）+ /ws（内部 v2）+ health/ready。

对外形态（provider-protocol.md §2）：OpenAI TTS 请求 → chunked PCM/wav 返回；
客户端断连即取消。内部 /ws 业务面：output.sentence → audio.open + PCM 帧 +
EOS + audio.end；control.cancel 取消在途合成（消息形状见 protocol.md）。
"""

import asyncio
from pathlib import Path

from fastapi import FastAPI, Request
from fastapi.responses import Response, StreamingResponse

from momo_models.audio import DOWNLINK_SAMPLE_RATE, wav_wrap
from momo_models.config import get_settings
from momo_models.logging import configure_logging, get_logger
from momo_models.proto import (
    DIR_DOWNLINK,
    FLAG_EOS,
    AudioHeader,
    encode_audio_frame,
    new_id,
    ulid_to_bytes,
)
from momo_models.providers.base import EMOTION_TAGS, TTSProvider, VoiceRef
from momo_models.providers.registry import create_tts_provider
from momo_models.services.common import (
    WsContext,
    attach_warmup,
    bearer_ok,
    make_app,
    msg_attribution,
    openai_error,
    serve,
)

log = get_logger(__name__)


class _TtsLink:
    """单条 /ws 连接的 TTS 业务面；provider 经 app.state 动态读取（可热切换）。"""

    def __init__(self, ctx: WsContext) -> None:
        self._ctx = ctx
        # stream_id → synth task；turn_id → 其下 stream_id 集合（control.cancel 定位）
        self._tasks: dict[str, asyncio.Task] = {}
        self._turns: dict[str, set[str]] = {}
        # 同连接内已消费的 (turn_id, seq)（幂等去重）
        self._seen: set[tuple[str, int]] = set()

    def _provider(self) -> TTSProvider | None:
        return getattr(self._ctx.app.state, "provider", None)

    # ---------- 消息分发 ----------

    async def handle(self, item: dict | bytes) -> bool:
        if isinstance(item, bytes | bytearray):
            # TTS 链路只产下行帧；任何上行二进制都是 wrong direction。
            await self._ctx.send_error("BAD_MESSAGE", "unexpected binary frame on tts link")
            return True
        msg_type = item.get("type")
        if msg_type == "output.sentence":
            await self._on_sentence(item)
            return True
        if msg_type == "control.cancel":
            await self._on_cancel(item)
            return True
        return False

    async def aclose(self) -> None:
        tasks = list(self._tasks.values())
        for t in tasks:
            t.cancel()
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)
        self._tasks.clear()
        self._turns.clear()
        self._seen.clear()

    # ---------- output.sentence → 下行音频流 ----------

    async def _on_sentence(self, msg: dict) -> None:
        turn_id = msg.get("turn_id")
        seq = msg.get("seq")
        text = msg.get("text")
        attr = msg_attribution(msg)
        if not isinstance(turn_id, str) or not turn_id:
            await self._ctx.send_error("BAD_MESSAGE", "output.sentence requires turn_id", **attr)
            return
        if not isinstance(seq, int) or isinstance(seq, bool) or not (0 <= seq <= 0xFFFFFFFF):
            await self._ctx.send_error("BAD_MESSAGE", "output.sentence requires u32 seq", **attr)
            return
        if not isinstance(text, str) or not text.strip():
            await self._ctx.send_error(
                "BAD_MESSAGE", "output.sentence requires non-empty text", **attr
            )
            return
        provider = self._provider()
        if provider is None:
            await self._ctx.send_error("TTS_FAILED", "tts provider not ready", **attr)
            return
        key = (turn_id, seq)
        if key in self._seen:
            await self._ctx.send_error("BAD_MESSAGE", f"duplicate sentence {turn_id}#{seq}", **attr)
            return
        emotion = msg.get("emotion")
        if emotion not in EMOTION_TAGS:
            emotion = "neutral"
        lang = msg.get("lang")
        lang = lang if isinstance(lang, str) and lang else "auto"

        self._seen.add(key)
        stream_id = new_id("st")
        task = asyncio.create_task(
            self._run_synth(
                provider=provider,
                stream_id=stream_id,
                turn_id=turn_id,
                seq=seq,
                text=text.strip(),
                emotion=emotion,
                lang=lang,
            ),
            name=f"tts-synth-{stream_id}",
        )
        self._tasks[stream_id] = task
        self._turns.setdefault(turn_id, set()).add(stream_id)
        task.add_done_callback(self._drop)

    def _drop(self, task: asyncio.Task) -> None:
        self._tasks.pop(task.get_name().removeprefix("tts-synth-"), None)

    async def _run_synth(
        self,
        *,
        provider: TTSProvider,
        stream_id: str,
        turn_id: str,
        seq: int,
        text: str,
        emotion: str,
        lang: str,
    ) -> None:
        """audio.open → PCM 帧流 → EOS → audio.end（protocol.md 二进制帧一节）。"""
        ctx = self._ctx
        stream_raw = ulid_to_bytes(stream_id.removeprefix("st_"))
        settings = ctx.app.state.settings
        voice = VoiceRef(
            ref_audio=Path(settings.tts_voice_ref).expanduser() if settings.tts_voice_ref else None,
            ref_text="",
            lang=lang,
        )
        stream = provider.synth_stream(text, voice=voice, emotion=emotion)
        attr = {"turn_id": turn_id, "stream_id": stream_id, "sentence_seq": seq}
        try:
            first = await anext(stream, None)
        except Exception as e:
            await ctx.send_error("TTS_FAILED", f"synth failed: {e}", **attr)
            await stream.aclose()
            return
        if first is None:
            # 空产出不开空 stream（下行可播放句不允许零长度音频）。
            await ctx.send_error("TTS_FAILED", f"provider produced no audio for seq {seq}", **attr)
            await stream.aclose()
            return

        await ctx.send(
            {
                "type": "audio.open",
                "stream_id": stream_id,
                "direction": "downlink",
                "turn_id": turn_id,
                "sentence_seq": seq,
                "codec": "pcm_s16le",
                "sample_rate": DOWNLINK_SAMPLE_RATE,
                "channels": 1,
            }
        )
        chunk_seq = 0
        sample_offset = 0
        try:
            chunk: bytes | None = first
            while chunk is not None:
                if chunk:
                    await ctx.send_binary(
                        encode_audio_frame(
                            AudioHeader(
                                direction=DIR_DOWNLINK,
                                flags=0,
                                stream_id=stream_raw,
                                chunk_seq=chunk_seq,
                                sentence_seq=seq,
                                sample_offset=sample_offset,
                            ),
                            chunk,
                        )
                    )
                    sample_offset += len(chunk) // 2
                    chunk_seq += 1
                chunk = await anext(stream, None)
        except asyncio.CancelledError:
            # control.cancel/断连：orchestrator 已丢弃该 stream，不再补帧。
            raise
        except Exception as e:
            await ctx.send_error("TTS_FAILED", f"synth failed mid-stream: {e}", **attr)
        finally:
            await stream.aclose()
        # EOS：独立空载荷收尾帧；audio.end.last_chunk_seq 与最后一帧一致。
        await ctx.send_binary(
            encode_audio_frame(
                AudioHeader(
                    direction=DIR_DOWNLINK,
                    flags=FLAG_EOS,
                    stream_id=stream_raw,
                    chunk_seq=chunk_seq,
                    sentence_seq=seq,
                    sample_offset=sample_offset,
                ),
                b"",
            )
        )
        await ctx.send(
            {
                "type": "audio.end",
                "stream_id": stream_id,
                "last_chunk_seq": chunk_seq,
            }
        )
        log.info(
            "tts.synth_done",
            stream_id=stream_id,
            turn_id=turn_id,
            seq=seq,
            chunks=chunk_seq,
            samples=sample_offset,
        )

    # ---------- control.cancel ----------

    async def _on_cancel(self, msg: dict) -> None:
        target = msg.get("target")
        obj_id = msg.get("id")
        if not isinstance(obj_id, str) or not obj_id:
            await self._ctx.send_error("BAD_MESSAGE", "control.cancel requires id")
            return
        provider = self._provider()
        if target == "turn":
            for sid in list(self._turns.get(obj_id, ())):
                task = self._tasks.get(sid)
                if task is not None:
                    task.cancel()
            self._turns.pop(obj_id, None)
            if provider is not None:
                await provider.cancel(obj_id)
        elif target == "stream":
            task = self._tasks.get(obj_id)
            if task is not None:
                task.cancel()
            if provider is not None:
                await provider.cancel(obj_id)
        elif target == "capture":
            log.info("tts.cancel_ignored", target=target, id=obj_id)  # 非 TTS 职责
        else:
            await self._ctx.send_error(
                "BAD_MESSAGE", f"unknown cancel target {target!r}", ref_id=obj_id
            )


# ---------- /v1/audio/speech（provider-protocol.md §2） ----------

_SPEECH_FORMATS = {"pcm", "wav"}
_SPEECH_MAX_INPUT = 4096  # OpenAI 上限口径（字符）


def _speech_fail(message: str, *, param: str | None = None) -> Response:
    return openai_error(400, message, param=param)


async def _speech(request: Request) -> Response:
    """OpenAI TTS 形态：JSON 请求 → PCM 流式 / WAV 整段；断连即取消。"""
    app = request.app
    settings = app.state.settings
    if settings.tts_api_key and not bearer_ok(request, settings.tts_api_key):
        return openai_error(401, "invalid or missing bearer token", type_="authentication_error")
    try:
        body = await request.json()
    except Exception:
        return _speech_fail("request body must be json")
    if not isinstance(body, dict):
        return _speech_fail("request body must be a json object")

    text = body.get("input")
    if not isinstance(text, str) or not text.strip():
        return _speech_fail("input must be a non-empty string", param="input")
    if len(text) > _SPEECH_MAX_INPUT:
        return _speech_fail(f"input exceeds {_SPEECH_MAX_INPUT} characters", param="input")
    fmt = body.get("response_format", "pcm")
    if fmt not in _SPEECH_FORMATS:
        return _speech_fail(
            f"response_format must be one of {sorted(_SPEECH_FORMATS)}",
            param="response_format",
        )
    speed = body.get("speed", 1.0)
    if not isinstance(speed, int | float) or isinstance(speed, bool) or not (0.25 <= speed <= 4.0):
        return _speech_fail("speed must be a number in [0.25, 4.0]", param="speed")
    lang = body.get("lang")
    lang = lang if isinstance(lang, str) and lang else settings.tts_lang
    emotion = body.get("emotion")
    if emotion not in EMOTION_TAGS:
        emotion = "neutral"
    voice_name = body.get("voice")
    voice_name = voice_name if isinstance(voice_name, str) else ""

    provider = getattr(app.state, "provider", None)
    if provider is None or not getattr(app.state, "ready", False):
        return openai_error(503, "tts provider not ready", type_="server_error")

    voice = VoiceRef(
        ref_audio=Path(settings.tts_voice_ref).expanduser() if settings.tts_voice_ref else None,
        ref_text="",
        lang=lang,
        name=voice_name,
    )
    try:
        stream = provider.synth_stream(text.strip(), voice=voice, emotion=emotion)
        # 首块在响应头发出前拉取：早期失败仍可回 500（同 _TtsLink 口径）。
        first = await anext(stream, None)
    except Exception as e:
        return openai_error(500, f"synth failed: {e}", type_="server_error")
    if first is None:
        await stream.aclose()
        return openai_error(500, "provider produced no audio", type_="server_error")

    if fmt == "wav":
        # wav 需 RIFF 头带总长：收集完毕单次返回（句长秒级，缓冲代价可接受）。
        try:
            pcm = first + b"".join([c async for c in stream])
        except Exception as e:
            await stream.aclose()
            return openai_error(500, f"synth failed: {e}", type_="server_error")
        await stream.aclose()
        return Response(
            content=wav_wrap(pcm, DOWNLINK_SAMPLE_RATE),
            media_type="audio/wav",
        )

    async def pcm_chunks():
        try:
            yield first
            async for chunk in stream:
                if chunk:
                    yield chunk
        except Exception as e:
            # 流中失败无法改状态码；记录后由客户端按截断流处理。
            log.error("tts.speech_stream_failed", error=str(e))
        finally:
            await stream.aclose()

    return StreamingResponse(
        pcm_chunks(),
        media_type="audio/pcm",
        headers={"X-Accel-Buffering": "no"},
    )


def create_app() -> FastAPI:
    settings = get_settings()
    configure_logging(settings.log_level, settings.log_format)
    app = make_app(
        role="tts",
        settings=settings,
        debug_path="/v1/tts/synthesize",
        on_startup=attach_warmup(create_tts_provider, settings, "warmup"),
        business_factory=_TtsLink,
    )
    app.post("/v1/audio/speech")(_speech)
    return app


def main() -> None:
    settings = get_settings()
    app = create_app()
    serve(app, settings.tts_listen)


if __name__ == "__main__":
    main()
