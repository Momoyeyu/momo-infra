"""momo-svc-* 骨架：WS /ws + /healthz + /readyz + 调试 POST（协议细节见 protocol.md）。

业务消息由 `business_factory` 注入的 WsBusiness 处理；未实装消息回 not implemented。
"""

import asyncio
import json
import time
from collections.abc import Awaitable, Callable
from contextlib import asynccontextmanager
from dataclasses import dataclass, field
from typing import Any, Protocol

import uvicorn
from fastapi import FastAPI, Request, WebSocket
from fastapi.responses import JSONResponse
from starlette.websockets import WebSocketDisconnect, WebSocketState

from momo_models.config import Settings
from momo_models.logging import get_logger
from momo_models.proto import ulid_new

PROTOCOL_VERSION = "v2"

# protocol.md 必需字段表的消息目录；模型服务链路只会上行其中一部分，
# 未实装的一律回 error{INTERNAL, "not implemented"}。
KNOWN_TYPES = frozenset(
    {
        "hello",
        "hello.ok",
        "session.snapshot",
        "input.text",
        "input.accepted",
        "input.rejected",
        "input.audio.start",
        "input.audio.end",
        "input.audio.cancel",
        "input.vad",
        "input.audio.segment",
        "audio.open",
        "audio.end",
        "output.sentence",
        "output.complete",
        "playback.started",
        "playback.progress",
        "control.barge_in",
        "control.cancel",
        "turn.finished",
        "state.turn",
        "error",
    }
)

log = get_logger(__name__)


# error 帧可选归因字段（protocol §2；#132 异步失败定位到 stream/capture/turn）。
_ERROR_ATTR_FIELDS = frozenset({"ref_id", "turn_id", "capture_id", "stream_id", "sentence_seq"})


def _error_frame(code: str, message: str, **attribution: Any) -> dict:
    """error 帧体（无 envelope，握手期用）。

    `**attribution` 为可选归因字段（ref_id/turn_id/capture_id/stream_id/
    sentence_seq），None 或表外键不上线。
    """
    return {
        "type": "error",
        "code": code,
        "message": message,
        **{k: v for k, v in attribution.items() if k in _ERROR_ATTR_FIELDS and v is not None},
    }


def msg_attribution(msg: dict) -> dict[str, Any]:
    """从入站消息提取 error 归因字段（#132）：str 型 id 与 int 型
    `sentence_seq`/`seq` 原样携带，其余形态视为未知不带。"""
    seq = msg.get("sentence_seq", msg.get("seq"))

    def _str(key: str) -> str | None:
        v = msg.get(key)
        return v if isinstance(v, str) else None

    return {
        "ref_id": _str("ref_id"),
        "turn_id": _str("turn_id"),
        "capture_id": _str("capture_id"),
        "stream_id": _str("stream_id"),
        "sentence_seq": seq if isinstance(seq, int) and not isinstance(seq, bool) else None,
    }


@dataclass
class WsContext:
    """已握手连接上下文；envelope/send_error 自动携带单调 event_seq。

    所有发送走 `send`/`send_error`/`send_binary` 并经 `_send_lock`
    串行：取号与发送原子，避免并发任务（接收循环 vs 业务 worker）
    造成 event_seq 乱序或 Starlette ws 字节级交错。
    """

    ws: WebSocket
    app: FastAPI
    session_id: str
    event_seq: int = 0
    _send_lock: asyncio.Lock = field(default_factory=asyncio.Lock)

    def envelope(self) -> dict[str, Any]:
        """服务端业务消息公共 envelope（event_seq 每次 +1）。"""
        self.event_seq += 1
        return {
            "session_id": self.session_id,
            "event_id": ulid_new(),
            "at_ms": int(time.time() * 1000),
            "event_seq": self.event_seq,
        }

    async def send(self, payload: dict[str, Any]) -> None:
        """锁内注入 envelope 发送业务消息（payload 不含 envelope 字段）。"""
        payload = dict(payload)
        msg_type = payload.pop("type", None)
        async with self._send_lock:
            await self.ws.send_json({"type": msg_type, **self.envelope(), **payload})

    async def send_binary(self, payload: bytes) -> None:
        """锁内发送二进制帧（与文本发送同锁，防跨任务字节交错）。"""
        async with self._send_lock:
            await self.ws.send_bytes(payload)

    async def send_error(self, code: str, message: str, **attribution: Any) -> None:
        """error 帧（envelope + code/message + 可选归因字段，#132）。"""
        frame = _error_frame(code, message, **attribution)
        frame.pop("type")
        async with self._send_lock:
            await self.ws.send_json(
                {
                    "type": "error",
                    **self.envelope(),
                    **frame,
                }
            )


class WsBusiness(Protocol):
    """每连接业务处理器：handle() 返回 True 表示已消费该消息。"""

    async def handle(self, item: dict | bytes) -> bool:
        """dict=已解析文本消息；bytes=二进制帧。"""
        ...

    async def aclose(self) -> None:
        """连接断开时清理（取消在途任务等）。"""
        ...


def make_app(
    *,
    role: str,
    settings: Settings,
    debug_path: str | None = None,
    on_startup: Callable[[FastAPI], Awaitable[None]] | None = None,
    business_factory: Callable[[WsContext], WsBusiness] | None = None,
) -> FastAPI:
    """构建某个角色（asr/tts/embed）的服务 app。"""

    @asynccontextmanager
    async def lifespan(app: FastAPI):
        app.state.ready = False
        app.state.provider = None
        if on_startup is not None:
            await on_startup(app)
        yield

    app = FastAPI(title=f"momo-svc-{role}", lifespan=lifespan)
    app.state.settings = settings

    # ---------- 访问日志 ----------
    @app.middleware("http")
    async def access_log(request: Request, call_next):
        started = time.perf_counter()
        resp = await call_next(request)
        log.info(
            "http.access",
            method=request.method,
            path=request.url.path,
            status=resp.status_code,
            ms=round((time.perf_counter() - started) * 1000, 1),
        )
        return resp

    # ---------- 健康端点 ----------
    @app.get("/healthz")
    async def healthz() -> dict:
        return {"status": "ok", "role": role}

    @app.get("/readyz")
    async def readyz(request: Request) -> JSONResponse:
        if getattr(request.app.state, "ready", False):
            return JSONResponse({"status": "ready", "role": role})
        return JSONResponse(
            {"status": "not_ready", "role": role},
            status_code=503,
        )

    # ---------- 调试 POST 口（不在运行时主路径） ----------
    if debug_path is not None:

        @app.post(debug_path)
        async def debug_endpoint() -> JSONResponse:
            return JSONResponse(
                {
                    "error": {
                        "code": "NOT_IMPLEMENTED",
                        "message": f"{debug_path} not implemented (P0)",
                    }
                },
                status_code=501,
            )

    # ---------- 异常 → error 信封 ----------
    @app.exception_handler(Exception)
    async def unhandled(request: Request, exc: Exception) -> JSONResponse:
        log.error("http.error", path=request.url.path, error=str(exc))
        return JSONResponse(
            {"error": {"code": "INTERNAL", "message": str(exc)}},
            status_code=500,
        )

    # ---------- momo-ws v2 /ws ----------
    @app.websocket("/ws")
    async def ws_endpoint(ws: WebSocket) -> None:
        await ws.accept()
        conn = log.bind(role=role, remote=str(ws.client))

        # 首帧必须文本 hello（protocol §4）
        first = await ws.receive()
        if first.get("text") is None:
            await ws.close(code=4400)
            return
        try:
            hello = json.loads(first["text"])
        except (json.JSONDecodeError, TypeError):
            await ws.send_json(_error_frame("BAD_MESSAGE", "first frame must be hello json"))
            await ws.close(code=4400)
            return

        if not isinstance(hello, dict) or hello.get("type") != "hello":
            await ws.send_json(_error_frame("BAD_MESSAGE", "first message must be hello"))
            await ws.close(code=4400)
            return

        if hello.get("protocol") != "momo-ws" or hello.get("version") != PROTOCOL_VERSION:
            await ws.send_json(
                _error_frame(
                    "VERSION_MISMATCH",
                    f"server speaks momo-ws {PROTOCOL_VERSION}",
                )
            )
            await ws.close(code=4001)
            return

        if hello.get("client") != "orchestrator":
            await ws.send_json(
                _error_frame("BAD_MESSAGE", "model link expects client=orchestrator")
            )
            await ws.close(code=4400)
            return

        session_id = f"sess_{ulid_new()}"
        await ws.send_json(
            {
                "type": "hello.ok",
                "version": PROTOCOL_VERSION,
                "session_id": session_id,
                "role": role,
                "capabilities": [],
            }
        )
        conn.info("ws.hello", session_id=session_id)
        ctx = WsContext(ws=ws, app=app, session_id=session_id)
        business = business_factory(ctx) if business_factory is not None else None

        # 未消费消息回 error 帧（known→INTERNAL not implemented，未知→BAD_MESSAGE）。
        try:
            while ws.client_state == WebSocketState.CONNECTED:
                frame = await ws.receive()
                data = frame.get("bytes")
                if data is not None:
                    if business is not None and await business.handle(data):
                        continue
                    await ctx.send_error("INTERNAL", "unexpected binary frame")
                    continue
                text = frame.get("text")
                if text is None:
                    break
                try:
                    msg = json.loads(text)
                except json.JSONDecodeError:
                    await ctx.send_error("BAD_MESSAGE", "invalid json")
                    continue
                msg_type = msg.get("type") if isinstance(msg, dict) else None
                if msg_type not in KNOWN_TYPES:
                    await ctx.send_error("BAD_MESSAGE", f"unknown type {msg_type!r}")
                    continue
                if msg_type == "hello":
                    await ctx.send_error("BAD_MESSAGE", "already negotiated")
                    continue
                if business is not None and await business.handle(msg):
                    continue
                await ctx.send_error("INTERNAL", f"{msg_type} not implemented (P0)")
        except WebSocketDisconnect:
            conn.info("ws.disconnect", session_id=session_id)
        finally:
            if business is not None:
                await business.aclose()

    return app


def attach_warmup(
    provider_factory: Callable[[Settings], Any],
    settings: Settings,
    warmup_method: str,
) -> Callable[[FastAPI], Awaitable[None]]:
    """生成 on_startup 钩子：provider → warmup → ready；失败不就绪但不致命。"""

    async def _startup(app: FastAPI) -> None:
        try:
            provider = provider_factory(settings)
        except Exception as e:  # 含 MOMO_TTS_PROVIDER 非法值
            log.error("provider.init_failed", error=str(e))
            return
        app.state.provider = provider
        try:
            await getattr(provider, warmup_method)()
        except NotImplementedError as e:
            log.warning("provider.stub", provider=provider.name, todo=str(e))
        except Exception as e:
            log.error("provider.warmup_failed", provider=provider.name, error=str(e))
        else:
            app.state.ready = True
            log.info("provider.ready", provider=provider.name)

    return _startup


def serve(app: FastAPI, listen: str) -> None:
    """console script 共用入口：`host:port` → uvicorn。"""
    from momo_models.config import parse_listen

    host, port = parse_listen(listen)
    uvicorn.run(app, host=host, port=port, log_level="warning")
