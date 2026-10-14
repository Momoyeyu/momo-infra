
input.audio.segment（消息形状见 protocol.md）；cancel/end 丢弃缓存并关 stream。
"""

import asyncio
from collections.abc import Awaitable, Callable, Iterator
from dataclasses import dataclass, field


from momo_models.audio import UPLINK_SAMPLE_RATE
from momo_models.config import Settings, get_settings
from momo_models.logging import configure_logging, get_logger
from momo_models.proto import (
    DIR_UPLINK,
    AudioHeader,
    FrameError,
    ulid_from_bytes,
    ulid_to_bytes,
)
from momo_models.providers.registry import create_asr_provider, create_vad_provider

log = get_logger(__name__)


@dataclass
class _Capture:
    """一条 uplink stream 的运行态：seq/offset 连续性校验 + 段转写队列。"""

    stream_id: str  # 文本 id（st_<ulid>）
    raw_stream: bytes  # 16B ULID 原始值（帧头匹配用）
    capture_id: str
    next_seq: int = 0  # 期望的下一帧 chunk_seq
    next_offset: int = 0  # 期望的下一帧 sample_offset（样本数）
    last_chunk_seq: int = -1
    eos: bool = False
    flushed: bool = False
    queue: asyncio.Queue = field(default_factory=asyncio.Queue)
    worker: asyncio.Task | None = None  # 段转写 worker（保序）


class _AsrLink:
    """单条 /ws 连接的 ASR 业务面。

    单连接同时只服务一个 uplink capture（单麦克风语义）：VAD 流内
    状态随 capture 走，audio.open 前 reset。provider 经 app.state
    动态读取（可热切换），未就绪回 ASR_FAILED。
    """

    def __init__(self, ctx: WsContext) -> None:
        self._ctx = ctx
        self._cap: _Capture | None = None

    def _vad(self) -> VADProvider | None:
        return getattr(self._ctx.app.state, "vad", None)

    def _asr(self) -> ASRProvider | None:
        return getattr(self._ctx.app.state, "asr", None)

    # ---------- 消息分发 ----------

    async def handle(self, item: dict | bytes) -> bool:
        if isinstance(item, bytes | bytearray):
            await self._on_frame(bytes(item))
            return True
        msg_type = item.get("type")
        if msg_type == "audio.open":
            await self._on_open(item)
            return True
        if msg_type == "audio.end":
            await self._on_end(item)
            return True
        if msg_type == "input.audio.end":
            await self._on_capture_close(item, discard=False)
            return True
        if msg_type == "input.audio.cancel":
            await self._on_capture_close(item, discard=True)
            return True
        if msg_type == "control.cancel":
            await self._on_cancel(item)
            return True
        return False

    async def aclose(self) -> None:
        if self._cap is not None:
            await self._close(self._cap, discard=True)

    # ---------- audio.open → capture 绑定 ----------

    async def _on_open(self, msg: dict) -> None:
        ctx = self._ctx
        attr = msg_attribution(msg)
        if msg.get("direction") != "uplink":
            await ctx.send_error(
                "BAD_MESSAGE", "audio.open on asr link requires direction=uplink", **attr
            )
            return
        if msg.get("sentence_seq") is not None or msg.get("turn_id") is not None:
            await ctx.send_error(
                "BAD_MESSAGE",
                "uplink audio.open must not carry sentence_seq/turn_id",
                **attr,
            )
            return
        capture_id = msg.get("capture_id")
        if not isinstance(capture_id, str) or not capture_id:
            await ctx.send_error("BAD_MESSAGE", "audio.open requires capture_id", **attr)
            return
        if (
            msg.get("codec") != "pcm_s16le"
            or msg.get("sample_rate") != UPLINK_SAMPLE_RATE
            or msg.get("channels") != 1
        ):
            await ctx.send_error(
                "BAD_MESSAGE", "audio.open requires pcm_s16le 16000Hz mono", **attr
            )
            return
        stream_id = msg.get("stream_id")
        if not isinstance(stream_id, str) or not stream_id:
            await ctx.send_error("BAD_MESSAGE", "audio.open requires stream_id", **attr)
            return
        try:
            raw_stream = ulid_to_bytes(stream_id.removeprefix("st_"))
        except ValueError:
            await ctx.send_error("BAD_MESSAGE", f"invalid stream_id {stream_id!r}", **attr)
            return
        if self._cap is not None:
            await ctx.send_error(
                "BAD_MESSAGE", f"uplink stream {self._cap.stream_id} already open", **attr
            )
            return
        vad = self._vad()
        if not getattr(ctx.app.state, "ready", False) or vad is None or self._asr() is None:
            await ctx.send_error("ASR_FAILED", "asr/vad provider not ready", **attr)
            return
        try:
            vad.reset()
        except Exception as e:
            await ctx.send_error("ASR_FAILED", f"vad reset failed: {e}", **attr)
            return
        cap = _Capture(stream_id=stream_id, raw_stream=raw_stream, capture_id=capture_id)
        cap.worker = asyncio.create_task(self._segment_worker(cap), name=f"asr-seg-{stream_id}")
        self._cap = cap
        log.info("asr.capture_open", stream_id=stream_id, capture_id=capture_id)

    # ---------- 二进制帧 → VAD 断句 ----------

    async def _on_frame(self, frame: bytes) -> None:
        ctx = self._ctx
        try:
            header, payload = AudioHeader.decode(frame)
        except FrameError as e:
            await ctx.send_error("BAD_MESSAGE", f"audio frame: {e}")
            return
        if header.direction != DIR_UPLINK:
            await ctx.send_error(
                "BAD_MESSAGE",
                "audio frame: wrong direction for asr link",
                stream_id=f"st_{ulid_from_bytes(header.stream_id)}",
            )
            return
        cap = self._cap
        if cap is None or header.stream_id != cap.raw_stream:
            await ctx.send_error(
                "BAD_MESSAGE",
                "audio frame: unknown stream",
                stream_id=f"st_{ulid_from_bytes(header.stream_id)}",
            )
            return
        if cap.eos:
            # EOS 后不再接受数据帧；stream 等 audio.end/cancel 收尾。
            await ctx.send_error(
                "BAD_MESSAGE",
                "audio frame after EOS",
                stream_id=cap.stream_id,
                capture_id=cap.capture_id,
            )
            return
        if header.chunk_seq != cap.next_seq or header.sample_offset != cap.next_offset:
            # seq/offset 不连续按 §6 中止 stream（对端需重开）。
            await ctx.send_error(
                "BAD_MESSAGE",
                f"audio frame gap: expected seq={cap.next_seq} offset={cap.next_offset}, "
                f"got seq={header.chunk_seq} offset={header.sample_offset}",
                stream_id=cap.stream_id,
                capture_id=cap.capture_id,
            )
            await self._close(cap, discard=True)
            return
        cap.last_chunk_seq = header.chunk_seq
        cap.next_seq += 1
        cap.next_offset += len(payload) // 2
        if header.eos():
            cap.eos = True
            await self._flush(cap, payload)
            return
        await self._feed(cap, payload)

    async def _feed(self, cap: _Capture, pcm: bytes) -> None:
        vad = self._vad()
        assert vad is not None  # audio.open 已校验 provider 就绪
        try:
            await self._emit(cap, vad.feed(pcm))
        except Exception as e:
            await self._ctx.send_error(
                "ASR_FAILED",
                f"vad feed failed: {e}",
                stream_id=cap.stream_id,
                capture_id=cap.capture_id,
            )

    async def _emit(self, cap: _Capture, events: Iterator[VadEvent]) -> None:
        """VadEvent → input.vad；speech_end 的段 PCM 入转写队列。"""
        for e in events:
            await self._ctx.send(
                {
                    "type": "input.vad",
                    "capture_id": cap.capture_id,
                    "event": e.event,
                }
            )
            if e.event == SPEECH_END and e.pcm:
                cap.queue.put_nowait(e.pcm)

    async def _flush(self, cap: _Capture, tail: bytes = b"") -> None:
        """EOS/audio.end：冲刷 VAD 残留段，随后等转写队列排空。"""
        if cap.flushed:
            return
        cap.flushed = True
        vad = self._vad()
        assert vad is not None
        try:
            events = list(vad.feed(tail)) if tail else []
            events.extend(vad.flush())
            await self._emit(cap, iter(events))
        except Exception as e:
            await self._ctx.send_error(
                "ASR_FAILED",
                f"vad flush failed: {e}",
                stream_id=cap.stream_id,
                capture_id=cap.capture_id,
            )
        cap.queue.put_nowait(None)
        if cap.worker is not None:
            # 末段转写完再算 stream 收尾（decode 有界，约段长×RTF）。
            await asyncio.gather(cap.worker, return_exceptions=True)

    async def _segment_worker(self, cap: _Capture) -> None:
        """按序转写 speech_end 段；空文本段不产出 segment（防空 reply）。"""
        ctx = self._ctx
        while True:
            pcm = await cap.queue.get()
            if pcm is None:
                return
            asr = self._asr()
            if asr is None:
                await ctx.send_error(
                    "ASR_FAILED",
                    "asr provider not ready",
                    stream_id=cap.stream_id,
                    capture_id=cap.capture_id,
                )
                continue
            try:
                t = await asr.transcribe(pcm, lang=None)
            except Exception as e:
                await ctx.send_error(
                    "ASR_FAILED",
                    f"transcribe failed: {e}",
                    stream_id=cap.stream_id,
                    capture_id=cap.capture_id,
                )
                continue
            if not t.text.strip():
                log.info("asr.segment_empty", capture_id=cap.capture_id)
                continue
            await ctx.send(
                {
                    "type": "input.audio.segment",
                    "capture_id": cap.capture_id,
                    "text": t.text,
                    "final": True,
                }
            )
            log.info(
                "asr.segment",
                capture_id=cap.capture_id,
                lang=t.lang,
                duration_ms=t.duration_ms,
            )

    # ---------- 收尾：audio.end / cancel ----------

    async def _on_end(self, msg: dict) -> None:
        ctx = self._ctx
        cap = self._cap
        if cap is None or cap.stream_id != msg.get("stream_id"):
            await ctx.send_error(
                "BAD_MESSAGE", "audio.end for unknown stream", **msg_attribution(msg)
            )
            return
        last = msg.get("last_chunk_seq")
        if not isinstance(last, int) or isinstance(last, bool) or last != cap.next_seq - 1:
            await ctx.send_error(
                "BAD_MESSAGE",
                f"audio.end last_chunk_seq={last!r} != last received frame seq={cap.next_seq - 1}",
                stream_id=cap.stream_id,
                capture_id=cap.capture_id,
            )
            await self._close(cap, discard=True)
            return
        await self._flush(cap)
        await self._close(cap)

    async def _on_capture_close(self, msg: dict, *, discard: bool) -> None:
        """input.audio.end/cancel（orchestrator 按 capture 维度转发）。"""
        ctx = self._ctx
        cap = self._cap
        if cap is None or cap.capture_id != msg.get("capture_id"):
            await ctx.send_error(
                "BAD_MESSAGE",
                f"unknown capture {msg.get('capture_id')!r}",
                **msg_attribution(msg),
            )
            return
        if not discard:
            await self._flush(cap)
        await self._close(cap, discard=discard)

    async def _on_cancel(self, msg: dict) -> None:
        ctx = self._ctx
        target = msg.get("target")
        obj_id = msg.get("id")
        if not isinstance(obj_id, str) or not obj_id:
            await ctx.send_error("BAD_MESSAGE", "control.cancel requires id")
            return
        cap = self._cap
        if target == "capture":
            if cap is not None and cap.capture_id == obj_id:
                await self._close(cap, discard=True)
            else:
                await ctx.send_error(
                    "BAD_MESSAGE", f"unknown capture {obj_id!r}", capture_id=obj_id
                )
        elif target == "stream":
            if cap is not None and cap.stream_id == obj_id:
                await self._close(cap, discard=True)
            else:
                await ctx.send_error("BAD_MESSAGE", f"unknown stream {obj_id!r}", stream_id=obj_id)
        elif target == "turn":
            log.info("asr.cancel_ignored", target=target, id=obj_id)  # 非 ASR 职责
        else:
            await ctx.send_error("BAD_MESSAGE", f"unknown cancel target {target!r}", ref_id=obj_id)

    async def _close(self, cap: _Capture, *, discard: bool = False) -> None:
        """关 stream；discard=True 丢弃未转写段并取消 worker。"""
        if self._cap is cap:
            self._cap = None
        if cap.worker is not None:
            if discard:
                cap.worker.cancel()
            else:
                cap.queue.put_nowait(None)
            await asyncio.gather(cap.worker, return_exceptions=True)
        log.info("asr.capture_closed", stream_id=cap.stream_id, discard=discard)


def _startup(settings: Settings) -> Callable[[FastAPI], Awaitable[None]]:
    """生成 on_startup：VAD + ASR 均 load 后才就绪（运行时不下载权重）。"""

    async def _run(app: FastAPI) -> None:
        try:
            vad = create_vad_provider(settings)
            asr = create_asr_provider(settings)
        except Exception as e:  # 含 MOMO_*_PROVIDER 非法值
            log.error("provider.init_failed", error=str(e))
            return
        app.state.vad = vad
        app.state.asr = asr
        results = await asyncio.gather(vad.load(), asr.load(), return_exceptions=True)
        ok = True
        for provider, result in ((vad, results[0]), (asr, results[1])):
            if isinstance(result, NotImplementedError):
                log.warning("provider.stub", provider=provider.name, todo=str(result))
                ok = False
            elif isinstance(result, BaseException):
                log.error("provider.load_failed", provider=provider.name, error=str(result))
                ok = False
        if ok:
            app.state.ready = True
            log.info("provider.ready", providers=f"{vad.name}+{asr.name}")

    return _run


def create_app() -> FastAPI:
    settings = get_settings()
    configure_logging(settings.log_level, settings.log_format)
        role="asr",
        settings=settings,
        debug_path="/v1/asr/transcribe",
        on_startup=_startup(settings),
        business_factory=_AsrLink,
    )


def main() -> None:
    settings = get_settings()
    app = create_app()
    serve(app, settings.asr_listen)


if __name__ == "__main__":
    main()
