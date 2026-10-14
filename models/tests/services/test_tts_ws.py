"""tts /ws 集成测试：握手、sentence→音频帧流、cancel；provider 用假 PCM 源注入。"""

import asyncio
import contextlib

from fastapi import FastAPI
from fastapi.testclient import TestClient

from momo_models.audio import DOWNLINK_SAMPLE_RATE
from momo_models.config import Settings
from momo_models.proto import DIR_DOWNLINK, AudioHeader, ulid_to_bytes
from momo_models.providers.base import TTSCaps, TTSProvider, VoiceRef
from momo_models.services.common import WsContext
from momo_models.services.tts_service import _TtsLink, create_app

_HELLO = {
    "type": "hello",
    "protocol": "momo-ws",
    "version": "v2",
    "client": "orchestrator",
    "auth_token": "t",
}


def _sentence(turn: str = "t_1", seq: int = 1, text: str = "你好。") -> dict:
    return {
        "type": "output.sentence",
        "session_id": "sess_x",
        "event_id": "01J5M0N8T1RZ8K2W4V7H6X9P3B",
        "at_ms": 0,
        "turn_id": turn,
        "seq": seq,
        "text": text,
        "emotion": "happy",
        "lang": "zh",
    }


class _FakeTTS(TTSProvider):
    """脚本化 PCM 源：逐块产出 chunks；block=True 时第二块后挂起（测 cancel）。"""

    name = "fake"

    def __init__(self, chunks: list[bytes], *, block: bool = False) -> None:
        self.chunks = chunks
        self.block = block
        self.cancels: list[str] = []
        self.calls: list[dict] = []

    def capabilities(self) -> TTSCaps:
        return TTSCaps(streaming=True, languages=("zh",), clone=False, emotion=False)

    async def warmup(self) -> None: ...

    def synth_stream(self, text: str, *, voice: VoiceRef, emotion: str | None):
        self.calls.append({"text": text, "lang": voice.lang, "emotion": emotion})

        async def gen():
            for i, c in enumerate(self.chunks):
                yield c
                if self.block and i == 0:
                    await asyncio.Event().wait()  # 挂起直到 task 被取消

        return gen()

    async def cancel(self, req_id: str) -> None:
        self.cancels.append(req_id)


@contextlib.contextmanager
def _app_with(provider: TTSProvider | None):
    """TestClient + 注入假 provider（lifespan 的 attach_warmup 会覆盖
    app.state.provider，故在 client 启动后再注入）。"""
    app = create_app()
    with TestClient(app) as client:
        app.state.provider = provider
        app.state.settings = Settings()
        yield client


def _hello(ws) -> dict:
    ws.send_json(_HELLO)
    return ws.receive_json()


def test_hello_ok_role_tts() -> None:
    with _app_with(_FakeTTS([])) as client, client.websocket_connect("/ws") as ws:
        ok = _hello(ws)
        assert ok["type"] == "hello.ok"
        assert ok["version"] == "v2"
        assert ok["role"] == "tts"
        assert ok["session_id"].startswith("sess_")


def test_hello_version_mismatch_closes_4001() -> None:
    with _app_with(None) as client, client.websocket_connect("/ws") as ws:
        ws.send_json({**_HELLO, "version": "v1"})
        err = ws.receive_json()
        assert err["code"] == "VERSION_MISMATCH"
        close = ws.receive()
        assert close["type"] == "websocket.close"
        assert close["code"] == 4001


def test_synth_streams_frames_eos_and_end() -> None:
    chunks = [b"\x01\x00" * 1200, b"\x02\x00" * 800]  # 1200 + 800 samples
    fake = _FakeTTS(chunks)
    with _app_with(fake) as client, client.websocket_connect("/ws") as ws:
        _hello(ws)
        ws.send_json(_sentence(seq=2))

        opened = ws.receive_json()
        assert opened["type"] == "audio.open"
        assert opened["direction"] == "downlink"
        assert opened["turn_id"] == "t_1"
        assert opened["sentence_seq"] == 2
        assert opened["codec"] == "pcm_s16le"
        assert opened["sample_rate"] == DOWNLINK_SAMPLE_RATE == 24000
        assert opened["channels"] == 1
        stream_id = opened["stream_id"]
        assert stream_id.startswith("st_")
        raw = None
        # 数据帧
        payload = b""
        for i in range(2):
            frame = ws.receive_bytes()
            header, data = AudioHeader.decode(frame)
            raw = header.stream_id
            assert header.direction == DIR_DOWNLINK
            assert header.chunk_seq == i
            assert header.sentence_seq == 2
            assert header.sample_offset == (0 if i == 0 else 1200)
            assert not header.eos()
            payload += data
        assert payload == b"".join(chunks)
        assert raw == ulid_to_bytes(stream_id.removeprefix("st_"))

        # EOS 帧
        header, eos_payload = AudioHeader.decode(ws.receive_bytes())
        assert header.eos()
        assert header.chunk_seq == 2
        assert header.sample_offset == 2000
        assert eos_payload == b""

        # audio.end
        end = ws.receive_json()
        assert end["type"] == "audio.end"
        assert end["stream_id"] == stream_id
        assert end["last_chunk_seq"] == 2
        # 服务端 envelope 单调递增
        assert end["event_seq"] > opened["event_seq"]

        assert fake.calls == [{"text": "你好。", "lang": "zh", "emotion": "happy"}]


def test_synth_empty_text_rejected() -> None:
    with _app_with(_FakeTTS([b"\x00\x00"])) as client, client.websocket_connect("/ws") as ws:
        _hello(ws)
        ws.send_json(_sentence(text="   "))
        err = ws.receive_json()
        assert err["type"] == "error"
        assert err["code"] == "BAD_MESSAGE"


def test_synth_no_audio_reports_tts_failed() -> None:
    with _app_with(_FakeTTS([])) as client, client.websocket_connect("/ws") as ws:
        _hello(ws)
        ws.send_json(_sentence())
        err = ws.receive_json()
        assert err["code"] == "TTS_FAILED"


def test_synth_without_provider_reports_not_ready() -> None:
    with _app_with(None) as client, client.websocket_connect("/ws") as ws:
        _hello(ws)
        ws.send_json(_sentence())
        err = ws.receive_json()
        assert err["code"] == "TTS_FAILED"


def test_duplicate_sentence_rejected() -> None:
    fake = _FakeTTS([b"\x01\x00" * 100])
    with _app_with(fake) as client, client.websocket_connect("/ws") as ws:
        _hello(ws)
        ws.send_json(_sentence(seq=1))
        # 收完第一条流
        while True:
            msg = ws.receive()
            if msg.get("text") and '"audio.end"' in msg["text"]:
                break
        ws.send_json(_sentence(seq=1))
        err = ws.receive_json()
        assert err["code"] == "BAD_MESSAGE"


def test_cancel_turn_stops_stream() -> None:
    fake = _FakeTTS([b"\x01\x00" * 100, b"\x02\x00" * 100], block=True)
    with _app_with(fake) as client, client.websocket_connect("/ws") as ws:
        _hello(ws)
        ws.send_json(_sentence(turn="t_9", seq=1))
        opened = ws.receive_json()
        assert opened["type"] == "audio.open"
        AudioHeader.decode(ws.receive_bytes())  # 首块

        ws.send_json({"type": "control.cancel", "target": "turn", "id": "t_9"})
        # 用一个未知消息触发回包；消息按序处理 ⇒ 此时 cancel 已生效
        ws.send_json({"type": "no.such.type"})
        err = ws.receive_json()
        assert err["code"] == "BAD_MESSAGE"
        assert fake.cancels == ["t_9"]


def test_binary_frame_rejected_on_tts_link() -> None:
    with _app_with(_FakeTTS([])) as client, client.websocket_connect("/ws") as ws:
        _hello(ws)
        ws.send_bytes(b"\x02" + b"\x00" * 40)
        err = ws.receive_json()
        assert err["code"] == "BAD_MESSAGE"


def test_link_contexts_independent() -> None:
    """WsContext.event_seq 从 1 起单调（envelope 校验）。"""
    ctx = WsContext(ws=None, app=None, session_id="s")  # type: ignore[arg-type]
    assert ctx.envelope()["event_seq"] == 1
    assert ctx.envelope()["event_seq"] == 2


class _RecordingWs:
    """模拟底层分段写：每次 send 记 begin/data/end 三元组，begin 后
    让出事件循环；若发送不经 _send_lock，并发任务的三元组会交错。"""

    def __init__(self) -> None:
        self.writes: list[tuple[str, int]] = []
        self.jsons: list[dict] = []
        self.bins: list[bytes] = []
        self._token = 0

    async def send_json(self, payload: dict) -> None:
        token = self._token
        self._token += 1
        self.writes.append(("begin", token))
        await asyncio.sleep(0)
        self.jsons.append(payload)
        self.writes.append(("data", token))
        self.writes.append(("end", token))

    async def send_bytes(self, payload: bytes) -> None:
        token = self._token
        self._token += 1
        self.writes.append(("begin", token))
        await asyncio.sleep(0)
        self.bins.append(payload)
        self.writes.append(("data", token))
        self.writes.append(("end", token))


class _YieldingTTS(_FakeTTS):
    """块间让出事件循环，给并发 send_error 制造交错窗口。"""

    def synth_stream(self, text: str, *, voice: VoiceRef, emotion: str | None):
        async def gen():
            for c in self.chunks:
                yield c
                await asyncio.sleep(0)

        return gen()


def test_synth_worker_and_error_loop_serialized() -> None:
    """_run_synth（独立 task）发 audio.open/PCM 帧/audio.end 的同时，
    接收循环路径并发 send_error：经同一把 _send_lock 串行 ⇒
    event_seq 单调、底层写不交错（#126 回归）。"""
    n_chunks = 8
    n_errors = 12

    async def run() -> _RecordingWs:
        ws = _RecordingWs()
        app = FastAPI()
        app.state.settings = Settings()
        app.state.provider = _YieldingTTS([b"\x01\x00" * 64] * n_chunks)
        ctx = WsContext(ws=ws, app=app, session_id="sess_x")  # type: ignore[arg-type]
        link = _TtsLink(ctx)
        assert await link.handle(_sentence())
        synth_tasks = list(link._tasks.values())
        assert len(synth_tasks) == 1

        async def error_loop() -> None:
            for _ in range(n_errors):
                await ctx.send_error("INTERNAL", "synthetic error")
                await asyncio.sleep(0)

        await asyncio.gather(error_loop(), *synth_tasks)
        return ws

    ws = asyncio.run(run())

    # 文本帧齐：audio.open + audio.end + n_errors 个 error。
    types = [m["type"] for m in ws.jsons]
    assert types.count("audio.open") == 1
    assert types.count("audio.end") == 1
    assert types.count("error") == n_errors
    # 锁内取号+发送原子 ⇒ 落地顺序即 1..N 严格单调。
    assert [m["event_seq"] for m in ws.jsons] == list(range(1, len(ws.jsons) + 1))
    # 数据帧 + EOS 共 n_chunks+1 个二进制帧。
    assert len(ws.bins) == n_chunks + 1
    # 底层写 begin/data/end 三元组连续 ⇒ 无跨任务字节交错。
    assert len(ws.writes) % 3 == 0
    for i in range(0, len(ws.writes), 3):
        kinds, tokens = zip(*ws.writes[i : i + 3], strict=True)
        assert list(kinds) == ["begin", "data", "end"]
        assert len(set(tokens)) == 1
