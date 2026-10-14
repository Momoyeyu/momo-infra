"""asr /ws 集成测试：握手、uplink 帧→VAD→segment、收尾与帧校验；provider 用假实现注入。"""

import contextlib

import pytest
from fastapi.testclient import TestClient

import momo_models.services.asr_service as asr_service
from momo_models.proto import (
    DIR_DOWNLINK,
    DIR_UPLINK,
    FLAG_EOS,
    AudioHeader,
    encode_audio_frame,
    ulid_to_bytes,
)
from momo_models.providers.base import (
    SPEECH_END,
    SPEECH_START,
    ASRProvider,
    Transcript,
    VadEvent,
    VADProvider,
)

_HELLO = {
    "type": "hello",
    "protocol": "momo-ws",
    "version": "v2",
    "client": "orchestrator",
    "auth_token": "t",
}

_STREAM_ID = "st_01J5M0N8T1RZ8K2W4V7H6X9P3E"
_CAPTURE_ID = "cap_01J5M0N8T1RZ8K2W4V7H6X9P3D"


class _FakeVad(VADProvider):
    """脚本化断句器：每次 feed 弹出一组事件；flush 返回 flush_events。"""

    name = "fake-vad"

    def __init__(
        self,
        script: list[list[VadEvent]] | None = None,
        flush_events: list[VadEvent] | None = None,
    ) -> None:
        self.script = list(script or [])
        self.flush_events = list(flush_events or [])
        self.fed: list[bytes] = []
        self.reset_count = 0
        self.loaded = False

    async def load(self) -> None:
        self.loaded = True

    def feed(self, pcm: bytes):
        self.fed.append(pcm)
        return iter(self.script.pop(0) if self.script else [])

    def flush(self):
        return iter(self.flush_events)

    def reset(self) -> None:
        self.reset_count += 1
        self.fed.clear()


class _FakeAsr(ASRProvider):
    """脚本化识别器：transcribe 记录入参并回固定文本。"""

    name = "fake-asr"

    def __init__(self, text: str = "你好，世界。") -> None:
        self.text = text
        self.calls: list[dict] = []
        self.loaded = False

    async def load(self) -> None:
        self.loaded = True

    async def transcribe(self, pcm: bytes, *, lang: str | None) -> Transcript:
        self.calls.append({"pcm": pcm, "lang": lang})
        return Transcript(text=self.text, lang="zh", duration_ms=len(pcm) // 32)


@contextlib.contextmanager
def _app_with(monkeypatch: pytest.MonkeyPatch, asr: ASRProvider, vad: VADProvider):
    """假 provider 注入：startup 工厂替换为返回假实现（load 后即 ready）。"""
    monkeypatch.setattr(asr_service, "create_asr_provider", lambda s: asr)
    monkeypatch.setattr(asr_service, "create_vad_provider", lambda s: vad)
    app = asr_service.create_app()
    with TestClient(app) as client:
        yield client


def _hello(ws) -> dict:
    ws.send_json(_HELLO)
    return ws.receive_json()


def _open_uplink(**over) -> dict:
    return {
        "type": "audio.open",
        "stream_id": _STREAM_ID,
        "direction": "uplink",
        "capture_id": _CAPTURE_ID,
        "codec": "pcm_s16le",
        "sample_rate": 16000,
        "channels": 1,
        **over,
    }


def _frame(
    seq: int,
    offset: int,
    payload: bytes,
    *,
    flags: int = 0,
    direction: int = DIR_UPLINK,
    stream: str = _STREAM_ID,
) -> bytes:
    return encode_audio_frame(
        AudioHeader(
            direction=direction,
            flags=flags,
            stream_id=ulid_to_bytes(stream.removeprefix("st_")),
            chunk_seq=seq,
            sentence_seq=0,
            sample_offset=offset,
        ),
        payload,
    )


def _pcm(samples: int) -> bytes:
    return b"\x01\x00" * samples


def test_hello_ok_role_asr(monkeypatch: pytest.MonkeyPatch) -> None:
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        ok = _hello(ws)
        assert ok["type"] == "hello.ok"
        assert ok["role"] == "asr"
        assert ok["session_id"].startswith("sess_")


def test_readyz_requires_both_providers(monkeypatch: pytest.MonkeyPatch) -> None:
    """VAD + ASR 都 load 成功 readyz 才就绪。"""
    with _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client:
        assert client.get("/readyz").status_code == 200
        assert client.get("/readyz").json()["status"] == "ready"

    class _FailAsr(_FakeAsr):
        async def load(self) -> None:
            raise RuntimeError("boom")

    with _app_with(monkeypatch, _FailAsr(), _FakeVad()) as client:
        assert client.get("/readyz").status_code == 503


def test_uplink_frames_drive_vad_and_segment(monkeypatch: pytest.MonkeyPatch) -> None:
    """open → 2 帧 → speech_start/speech_end → input.audio.segment{final:true}。"""
    seg_pcm = _pcm(1600)
    vad = _FakeVad(
        script=[
            [VadEvent(event=SPEECH_START, at_ms=0, segment_index=1)],
            [VadEvent(event=SPEECH_END, at_ms=100, segment_index=1, pcm=seg_pcm)],
        ]
    )
    asr = _FakeAsr(text="你好")
    with _app_with(monkeypatch, asr, vad) as client, client.websocket_connect("/ws") as ws:
        _hello(ws)
        ws.send_json(_open_uplink())
        ws.send_bytes(_frame(0, 0, _pcm(800)))
        ws.send_bytes(_frame(1, 800, _pcm(800)))

        start = ws.receive_json()
        assert start["type"] == "input.vad"
        assert start["capture_id"] == _CAPTURE_ID
        assert start["event"] == "speech_start"
        assert start["event_seq"] == 1

        end = ws.receive_json()
        assert end["type"] == "input.vad"
        assert end["event"] == "speech_end"

        seg = ws.receive_json()
        assert seg["type"] == "input.audio.segment"
        assert seg["capture_id"] == _CAPTURE_ID
        assert seg["text"] == "你好"
        assert seg["final"] is True
        assert seg["event_seq"] == 3

        # VAD 吃到两帧原始 PCM；转写拿到 speech_end 携带的段 PCM。
        assert vad.fed == [_pcm(800), _pcm(800)]
        assert asr.calls == [{"pcm": seg_pcm, "lang": None}]


def test_frame_before_open_rejected(monkeypatch: pytest.MonkeyPatch) -> None:
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_bytes(_frame(0, 0, _pcm(800)))
        err = ws.receive_json()
        assert err["code"] == "BAD_MESSAGE"
        assert "unknown stream" in err["message"]


def test_frame_gap_aborts_stream(monkeypatch: pytest.MonkeyPatch) -> None:
    """chunk_seq 跳跃 → error + stream 中止（§6 gap 口径），后续帧按 unknown stream 拒绝。"""
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_json(_open_uplink())
        ws.send_bytes(_frame(1, 0, _pcm(800)))  # 期望 seq=0
        err = ws.receive_json()
        assert err["code"] == "BAD_MESSAGE"
        assert "gap" in err["message"]
        ws.send_bytes(_frame(0, 0, _pcm(800)))  # stream 已中止
        err = ws.receive_json()
        assert "unknown stream" in err["message"]


def test_frame_offset_mismatch_aborts_stream(monkeypatch: pytest.MonkeyPatch) -> None:
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_json(_open_uplink())
        ws.send_bytes(_frame(0, 400, _pcm(800)))  # 期望 offset=0
        err = ws.receive_json()
        assert err["code"] == "BAD_MESSAGE"


def test_downlink_frame_rejected(monkeypatch: pytest.MonkeyPatch) -> None:
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_json(_open_uplink())
        ws.send_bytes(_frame(0, 0, _pcm(800), direction=DIR_DOWNLINK))
        err = ws.receive_json()
        assert err["code"] == "BAD_MESSAGE"
        assert "direction" in err["message"]


def test_malformed_frame_rejected(monkeypatch: pytest.MonkeyPatch) -> None:
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_bytes(b"\x02" + b"\x00" * 10)  # <32B
        err = ws.receive_json()
        assert err["code"] == "BAD_MESSAGE"


def test_open_validates_uplink_binding(monkeypatch: pytest.MonkeyPatch) -> None:
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_json(_open_uplink(direction="downlink"))
        assert ws.receive_json()["code"] == "BAD_MESSAGE"
        ws.send_json(_open_uplink(capture_id=None))
        assert ws.receive_json()["code"] == "BAD_MESSAGE"
        ws.send_json(_open_uplink(sentence_seq=1))
        assert ws.receive_json()["code"] == "BAD_MESSAGE"
        ws.send_json(_open_uplink(sample_rate=24000))
        assert ws.receive_json()["code"] == "BAD_MESSAGE"
        ws.send_json(_open_uplink(stream_id="nope"))
        assert ws.receive_json()["code"] == "BAD_MESSAGE"


def test_second_open_rejected(monkeypatch: pytest.MonkeyPatch) -> None:
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_json(_open_uplink())
        ws.send_json(_open_uplink())
        err = ws.receive_json()
        assert err["code"] == "BAD_MESSAGE"
        assert "already open" in err["message"]


def test_audio_end_closes_stream(monkeypatch: pytest.MonkeyPatch) -> None:
    """audio.end.last_chunk_seq 与最后帧一致 → 关 stream（后续帧 unknown）。"""
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_json(_open_uplink())
        ws.send_bytes(_frame(0, 0, _pcm(800)))
        ws.send_json({"type": "audio.end", "stream_id": _STREAM_ID, "last_chunk_seq": 0})
        ws.send_bytes(_frame(1, 800, _pcm(800)))
        err = ws.receive_json()
        assert "unknown stream" in err["message"]


def test_audio_end_seq_mismatch_rejected(monkeypatch: pytest.MonkeyPatch) -> None:
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_json(_open_uplink())
        ws.send_bytes(_frame(0, 0, _pcm(800)))
        ws.send_json({"type": "audio.end", "stream_id": _STREAM_ID, "last_chunk_seq": 7})
        err = ws.receive_json()
        assert err["code"] == "BAD_MESSAGE"
        assert "last_chunk_seq" in err["message"]


def test_audio_end_flushes_tail_segment(monkeypatch: pytest.MonkeyPatch) -> None:
    """capture 结束时仍在 speech → flush 强制 speech_end 并转写末段。"""
    tail_pcm = _pcm(800)
    vad = _FakeVad(
        flush_events=[VadEvent(event=SPEECH_END, at_ms=50, segment_index=1, pcm=tail_pcm)]
    )
    asr = _FakeAsr()
    with _app_with(monkeypatch, asr, vad) as client, client.websocket_connect("/ws") as ws:
        _hello(ws)
        ws.send_json(_open_uplink())
        ws.send_bytes(_frame(0, 0, _pcm(800)))
        ws.send_json({"type": "audio.end", "stream_id": _STREAM_ID, "last_chunk_seq": 0})
        end = ws.receive_json()
        assert end["type"] == "input.vad"
        assert end["event"] == "speech_end"
        seg = ws.receive_json()
        assert seg["type"] == "input.audio.segment"
        assert seg["final"] is True
        assert asr.calls[0]["pcm"] == tail_pcm


def test_eos_zero_length_capture(monkeypatch: pytest.MonkeyPatch) -> None:
    """EOS 帧（空载荷）表零长度 capture 结束；audio.end.last_chunk_seq=0 收尾。"""
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_json(_open_uplink())
        ws.send_bytes(_frame(0, 0, b"", flags=FLAG_EOS))
        # EOS 后再发数据帧 → 拒绝
        ws.send_bytes(_frame(1, 0, _pcm(800)))
        assert ws.receive_json()["code"] == "BAD_MESSAGE"
        ws.send_json({"type": "audio.end", "stream_id": _STREAM_ID, "last_chunk_seq": 0})
        ws.send_json({"type": "no.such.type"})
        assert ws.receive_json()["code"] == "BAD_MESSAGE"  # audio.end 已接受（无回包）


def test_capture_cancel_discards(monkeypatch: pytest.MonkeyPatch) -> None:
    """control.cancel{target:capture} → 关 stream 并可重开。"""
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_json(_open_uplink())
        ws.send_json({"type": "control.cancel", "target": "capture", "id": _CAPTURE_ID})
        ws.send_json({"type": "no.such.type"})
        assert ws.receive_json()["code"] == "BAD_MESSAGE"
        # capture 已关：重开不再报 already open（seq gap 证明 open 生效）
        ws.send_json(_open_uplink())
        ws.send_bytes(_frame(1, 0, _pcm(800)))
        err = ws.receive_json()
        assert "gap" in err["message"]


def test_input_audio_cancel_discards(monkeypatch: pytest.MonkeyPatch) -> None:
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_json(_open_uplink())
        ws.send_json({"type": "input.audio.cancel", "capture_id": _CAPTURE_ID})
        ws.send_bytes(_frame(0, 0, _pcm(800)))
        err = ws.receive_json()
        assert "unknown stream" in err["message"]


def test_empty_text_segment_not_sent(monkeypatch: pytest.MonkeyPatch) -> None:
    """转写空文本不产 segment（避免空 reply 触发）。"""
    vad = _FakeVad(script=[[VadEvent(event=SPEECH_END, at_ms=50, segment_index=1, pcm=_pcm(800))]])
    with (
        _app_with(monkeypatch, _FakeAsr(text="  "), vad) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_json(_open_uplink())
        ws.send_bytes(_frame(0, 0, _pcm(800)))
        end = ws.receive_json()
        assert end["event"] == "speech_end"
        # 下一条应是 error 而非 segment
        ws.send_json({"type": "no.such.type"})
        nxt = ws.receive_json()
        assert nxt["type"] == "error"
        assert nxt["code"] == "BAD_MESSAGE"


def test_providers_not_ready_reports_asr_failed(monkeypatch: pytest.MonkeyPatch) -> None:
    """provider 未 load（ready=False）→ audio.open 回 ASR_FAILED。"""

    class _FailVad(_FakeVad):
        async def load(self) -> None:
            raise RuntimeError("no model")

    with (
        _app_with(monkeypatch, _FakeAsr(), _FailVad()) as client,
        client.websocket_connect("/ws") as ws,
    ):
        _hello(ws)
        ws.send_json(_open_uplink())
        err = ws.receive_json()
        assert err["code"] == "ASR_FAILED"
